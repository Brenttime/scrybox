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
  const s = { clock: 0, manifest: manifest('A'), sessions: [], cache: new Map(), fetches: [], messages: [], timers: [], badIndex: false, createGate: null };
  const cache = {
    match: async (k) => { if (s.matchGate && k.endsWith('rec.same.onnx')) await s.matchGate.promise; return s.cache.get(k)?.clone(); },
    keys: async () => [...s.cache.keys()].map(k => ({ url: `https://scrybox.test${k}` })),
    delete: async (q) => s.cache.delete(typeof q === 'string' ? q : new URL(q.url).pathname),
    put: async (k, v) => { const c = v.clone(); if (s.putGate && k.includes('.B.')) await s.putGate.promise; s.cache.set(k, c); },
  };
  const ort = { env: { wasm: {} }, Tensor: class { constructor(t, d, dims) { this.dims = dims; } }, InferenceSession: { create: async () => {
    const id = s.sessions.length;
    const x = { id, released: 0, inputNames: ['x'], release: async () => { x.released++; }, run: async () => ({}) };
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
      if (url.endsWith('manifest.json')) return Response.json(s.manifest);
      if (url.endsWith('.json.gz')) { const k = url.match(/scan-index\.(.+)\.json/)[1]; return new Response(s.badIndex ? 'invalid gzip' : zlib.gzipSync(JSON.stringify({ version: k }))); }
      return new Response('model');
    },
    createReader: () => ({ reset() {}, async probe() { return null; }, async read() { return { results: [] }; } }),
    self: { navigator: { hardwareConcurrency: 4 }, crossOriginIsolated: false, postMessage: (m) => s.messages.push(m) },
  });
  vm.runInContext(`${source}\nglobalThis.testAPI={bundles};`, context);
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
