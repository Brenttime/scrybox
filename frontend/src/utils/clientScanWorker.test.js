// clientScanWorker refresh failure paths (review R2-B2, R2-S2): the real
// worker source in a vm with Cache API / fetch / ORT mocks.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import zlib from 'node:zlib';
import { createBundles } from './scanBundles.js';

const source = fs.readFileSync(new URL('./clientScanWorker.js', import.meta.url), 'utf8').replace(/^import .*;\n/gm, '');
const tick = () => new Promise(r => setImmediate(r));
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
async function until(fn) { for (let i = 0; i < 1000; i++) { if (fn()) return; await tick(); } throw new Error('condition not reached'); }
const manifest = (k) => ({ index: `scan-index.${k}.json.gz`, rec: 'rec.same.onnx', dict: 'dict.same.txt' });

function rig() {
  const s = { clock: 0, manifest: manifest('A'), sessions: [], cache: new Map(), fetches: [], messages: [], timers: [], puts: [], badIndex: false, createGate: null };
  const cache = {
    match: async (k) => { if (s.matchGate && k.endsWith('rec.same.onnx')) await s.matchGate.promise; return s.cache.get(k)?.clone(); },
    keys: async () => [...s.cache.keys()].map(k => ({ url: `https://scrybox.test${k}` })),
    delete: async (q) => s.cache.delete(typeof q === 'string' ? q : new URL(q.url).pathname),
    put: async (k, v) => { const c = v.clone(); s.puts.push(k); if (s.putGate && k.includes('.B.')) await s.putGate.promise; s.cache.set(k, c); },
  };
  const ort = { env: { wasm: {} }, Tensor: class { constructor(t, d, dims) { this.dims = dims; } }, InferenceSession: { create: async () => {
    const id = s.sessions.length;
    const x = { id, released: 0, inputNames: ['x'], release: async () => { x.released++; }, run: async () => { x.runs = (x.runs || 0) + 1; if (s.warmGate) await s.warmGate.promise; return {}; } };
    s.sessions.push(x);
    if (s.createGate && id % 2 === 1 && id >= 2) await s.createGate.promise;
    return x;
  } } };
  const context = vm.createContext({
    console, performance, URL, Response, Blob, TextDecoder, Uint8Array, Uint8ClampedArray, Float32Array, DecompressionStream, SharedArrayBuffer, AbortController, Promise, ort, CORN_SIZE: 384,
    createBundles: (opts) => createBundles({ ...opts, now: () => s.clock, setTimer: (fn) => { s.timers.push(fn); return fn; }, clearTimer: () => {} }),
    caches: { open: async () => cache }, buildCharset: (t) => t, loadIndex: (r) => r,
    fetch: async (url, opts = {}) => {
      s.fetches.push({ url, signal: opts.signal });
      if (s.failDict && url.endsWith('dict.bad.txt')) { await tick(); throw new Error('dictionary network failure'); }
      if (url.endsWith('manifest.json')) { const m = s.manifest; if (s.manifestGate) await s.manifestGate.promise; return Response.json(m); }
      if (s.fetchGate && url.includes('.B.')) await s.fetchGate.promise;
      if (s.bytes?.[url.slice(url.lastIndexOf('/') + 1)]) return new Response(s.bytes[url.slice(url.lastIndexOf('/') + 1)]);
      if (url.endsWith('.json.gz')) { const k = url.match(/scan-index\.(.+)\.json/)[1]; return new Response(s.badIndex ? 'invalid gzip' : zlib.gzipSync(JSON.stringify({ version: k }))); }
      return new Response('model');
    },
    createReader: () => ({ reset() {}, async probe() { return null; }, async read() { return { results: [] }; } }),
    self: { crypto: globalThis.crypto, navigator: { hardwareConcurrency: 4 }, crossOriginIsolated: false, postMessage: (m) => s.messages.push(m) },
  });
  vm.runInContext(`${source}\nglobalThis.testAPI={bundles,load};`, context);
  let id = 0;
  const send = async (type, data = {}) => { const n = ++id; await context.self.onmessage({ data: { type, id: n, ...data } }); return s.messages.find(m => m.id === n); };
  return { s, send, api: context.testAPI };
}

test('worker: a failed candidate does not poison the Cache API; a retry refetches and succeeds, A kept (R2-S2)', async () => {
  const r = rig();
  await r.send('load');
  r.s.clock = 16000; r.s.manifest = manifest('B'); r.s.badIndex = true;
  const bad = await r.send('check');
  assert.ok(bad.error);
  await tick();
  assert.equal(r.api.bundles.active.info.index, manifest('A').index);
  assert.ok(r.s.cache.has('/scan-assets/' + manifest('A').index), 'last-known-good A kept');
  assert.ok(r.s.cache.has('/scan-assets/rec.same.onnx'), 'assets shared with A kept');
  const count = () => r.s.fetches.filter(x => x.url.includes('.B.')).length;
  const before = count();
  r.s.badIndex = false;
  const retry = await r.send('check');
  assert.equal(retry.updated, true, JSON.stringify(retry));
  assert.equal(count(), before + 1, 'B refetched from the network');
});

test('worker: the manifest request carries the refresh signal (R2-B2)', async () => {
  const r = rig();
  await r.send('load');
  r.s.clock = 16000;
  await r.send('check');
  assert.ok(r.s.fetches.filter(x => x.url.endsWith('manifest.json')).at(-1).signal, 'manifest fetch is abortable');
});

test('worker: an abandoned build frees a completed session even while a sibling hangs (R2-B2)', async () => {
  const r = rig();
  await r.send('load');
  r.s.clock = 16000; r.s.manifest = manifest('B'); r.s.createGate = deferred();
  const p = r.send('check');
  await until(() => r.s.sessions.length === 4);
  for (const t of r.s.timers.splice(0)) t();
  const res = await p;
  assert.ok(res.error, 'check settles while the sibling is still hung');
  await tick();
  assert.equal(r.s.sessions[2].released, 1, 'completed candidate session released');
  assert.deepEqual([r.s.sessions[0].released, r.s.sessions[1].released], [0, 0], 'A untouched');
  r.s.createGate.resolve();
  await until(() => r.s.sessions[3].released === 1);
});

test('worker: a load aborted before its sessions exist never creates or pins them (R3-S1)', async () => {
  const r = rig();
  await r.send('load'); await tick();
  r.s.clock = 16000; r.s.manifest = manifest('B'); r.s.matchGate = deferred(); r.s.createGate = deferred();
  const p = r.send('check');
  await until(() => r.s.fetches.some(x => x.url.includes('.B.')));
  for (const t of r.s.timers.splice(0)) t();          // deadline while the Cache API wait is pending
  assert.ok((await p).error);
  r.s.matchGate.resolve();                            // the non-abortable wait returns late
  for (let i = 0; i < 20; i++) await tick();
  assert.equal(r.s.sessions.length, 2, 'no candidate sessions created after the abort');
  assert.deepEqual(r.s.sessions.map(x => x.released), [0, 0], 'A kept');
  r.s.createGate.resolve();
});

test('worker: a cache write still pending when a candidate fails cannot resurrect its bytes (R3-S2)', async () => {
  const r = rig();
  await r.send('load'); await tick();
  r.s.clock = 16000; r.s.manifest = { ...manifest('B'), dict: 'dict.bad.txt' }; r.s.badIndex = true; r.s.failDict = true; r.s.putGate = deferred();
  const failed = await r.send('check');
  assert.ok(failed.error);
  const key = '/scan-assets/' + manifest('B').index;
  r.s.putGate.resolve();                              // the late put lands after the failure
  for (let i = 0; i < 20; i++) await tick();
  assert.equal(r.s.cache.has(key), false, 'late put does not re-poison the cache');
  const count = () => r.s.fetches.filter(x => x.url.includes('.B.')).length;
  const before = count();
  r.s.failDict = false; r.s.badIndex = false;
  const retry = await r.send('check');
  assert.equal(retry.updated, true, JSON.stringify(retry));
  assert.equal(count(), before + 1, 'first repaired retry refetches B');
  assert.ok(r.s.cache.has('/scan-assets/' + manifest('A').index), 'A kept');
});

const settle = async () => { for (let i = 0; i < 30; i++) await tick(); };

test('worker: overlapping failed candidates write nothing; the first repaired retry refetches (R4-S1 A)', async () => {
  const r = rig();
  await r.send('load'); await settle();
  r.s.clock = 16000; r.s.manifest = { ...manifest('B'), rec: 'rec.B.onnx', dict: 'dict.bad.txt' }; r.s.badIndex = true; r.s.failDict = true;
  const assetPuts = () => r.s.puts.filter(k => k.startsWith('/scan-assets/')).length;   // cornelius revalidates on its own
  const putsBefore = assetPuts();
  assert.ok((await r.send('check')).error);
  assert.ok((await r.send('check')).error);         // second failure overlaps the first
  await settle();
  assert.equal(assetPuts(), putsBefore, 'failed loads never write the cache');
  assert.equal([...r.s.cache.keys()].some(k => k.includes('.B.') || k.includes('bad')), false);
  const count = () => r.s.fetches.filter(x => x.url.includes('.B.')).length;
  const before = count();
  r.s.badIndex = false; r.s.failDict = false;
  const fixed = await r.send('check');
  assert.equal(fixed.updated, true, JSON.stringify(fixed));
  assert.ok(count() > before, 'repaired retry hit the network');
});

test('worker: an expired load whose bytes arrive late cannot overwrite a successful retry (R4-S1 B)', async () => {
  const r = rig();
  await r.send('load'); await settle();
  r.s.clock = 16000; r.s.manifest = manifest('B'); r.s.badIndex = true; r.s.fetchGate = deferred();
  const first = r.send('check');
  await until(() => r.s.fetches.some(x => x.url.includes('.B.')));
  for (const t of r.s.timers.splice(0)) t();         // expire while B's fetch hangs
  assert.equal((await first).error, 'refresh timed out');
  const oldGate = r.s.fetchGate; r.s.fetchGate = null; r.s.badIndex = false;
  const retry = await r.send('check');
  assert.equal(retry.updated, true, JSON.stringify(retry));
  await settle();
  const key = '/scan-assets/' + manifest('B').index;
  const version = async () => JSON.parse(zlib.gunzipSync(Buffer.from(await r.s.cache.get(key).clone().arrayBuffer()))).version;
  assert.equal(await version(), 'B');
  r.s.badIndex = true; oldGate.resolve();            // the abandoned load's bytes land now
  await settle();
  assert.equal(await version(), 'B', 'cache still holds the validated bytes');
  const fresh = await r.api.load(manifest('B')).then(() => 'ok', e => String(e));
  assert.equal(fresh, 'ok', 'a later load from that cache succeeds');
});

test('worker: a hashed asset whose cached bytes do not match its name is refetched, never trusted (R4-S1)', async () => {
  const good = zlib.gzipSync(JSON.stringify({ version: 'H' }));
  const hex = Buffer.from(await crypto.subtle.digest('SHA-256', good)).toString('hex').slice(0, 12);
  const r = rig();
  await r.send('load'); await settle();
  const name = `scan-index.${hex}.json.gz`;
  r.s.bytes = { [name]: good };
  r.s.cache.set('/scan-assets/' + name, new Response('corrupt'));
  r.s.clock = 16000; r.s.manifest = { ...manifest('A'), index: name };
  const res = await r.send('check');
  assert.equal(res.updated, true, JSON.stringify(res));
  await settle();
  assert.deepEqual(new Uint8Array(await r.s.cache.get('/scan-assets/' + name).clone().arrayBuffer()), new Uint8Array(good));
  r.s.bytes = { [name]: new TextEncoder().encode('tampered') };
  r.s.cache.delete('/scan-assets/' + name);
  const bad = await r.api.load({ ...manifest('A'), index: name }).then(() => 'ok', e => String(e));
  assert.match(bad, /content hash mismatch/);
  await settle();
  assert.equal(r.s.cache.has('/scan-assets/' + name), false, 'mismatched bytes never cached');
});

for (const next of ['A', 'B', 'C', 'error', 'timeout']) {
  test(`worker: a completed held build releases each session exactly once when the newer check picks ${next} (R4-S2, R4-N1)`, async () => {
    const r = rig();
    await r.send('load'); await settle();
    r.s.clock = 16000; r.s.manifest = manifest('B');
    const warm = deferred(); r.s.warmGate = warm;
    const first = r.send('check');
    await until(() => r.s.sessions.length === 4 && r.s.sessions[3].runs);
    const man = deferred(); r.s.manifestGate = man; r.s.manifest = manifest(next === 'error' || next === 'timeout' ? 'A' : next);
    const second = r.send('check');
    await until(() => r.s.fetches.filter(x => x.url.endsWith('manifest.json')).length === 3);
    r.s.warmGate = null; warm.resolve(); await settle();
    assert.equal(r.api.bundles.pending, null, 'held, not installed');
    r.s.manifestGate = null;
    if (next === 'error') man.reject(new Error('manifest network failure'));
    else if (next === 'timeout') r.s.timers.at(-1)();
    else man.resolve();
    const newer = await second, older = await first; await settle();
    const rel = r.s.sessions.slice(0, 4).map(x => x.released);
    if (next === 'B') { assert.deepEqual(rel, [0, 0, 0, 0]); assert.equal(older.updated, true); return; }
    assert.deepEqual(rel, [0, 0, 1, 1], 'held B released exactly once, A untouched');
    if (next === 'error') { assert.equal(newer.error, 'manifest network failure'); assert.equal(older.error, 'refresh superseded'); }
    if (next === 'timeout') assert.equal(older.error, 'refresh timed out');
  });
}
