// On-device card reading for Scan Cards, off the main thread.
//
// Runs shared/clientScan/pipeline.mjs, the same module the Node replay harness
// (backend/scripts/client-scan-replay.mjs) validates against saved phone
// frames. Models and the title/printing index are fetched once from
// /scan-assets/ (content-hashed names, served immutable) and kept in the Cache
// API, so a returning phone starts reading without touching the network.
//
// CPU wasm EP only (no jsep/webgpu binaries). Threads only when the page is
// cross-origin isolated (backend coep.js: credentialless on Blink/Gecko,
// require-corp on WebKit); otherwise one thread.
import * as ort from 'onnxruntime-web/wasm';
import { createReader, CORN_SIZE } from '../../../shared/clientScan/pipeline.mjs';
import { buildCharset, loadIndex } from '../../../shared/clientScan/text.mjs';
import { createBundles } from './scanBundles.js';

ort.env.wasm.wasmPaths = '/ort/';
// Threads need cross-origin isolation (COOP + COEP, backend coep.js). Replay
// on saved frames: 2 threads cut matched-read p50 ~30% on a 4-core host and 4
// was no better, so isolated devices with >= 4 cores default to 2. If a
// threaded start fails, clientScan.js retries once in a fresh worker at 1
// thread (scanLoad.js). localStorage 'scan.threads' overrides for A/B tests.
function pickThreads(override) {
  if (!self.crossOriginIsolated || typeof SharedArrayBuffer === 'undefined') return 1;
  const n = Number(override);
  if (n >= 1 && n <= 8) return Math.floor(n);
  const cores = self.navigator?.hardwareConcurrency || 1;
  // 2026-10-09 live A/B on the 16-core Windows PC (same Smokestack, CDP-driven,
  // 3 runs each): 2 threads 1.95-2.16 s to added, 4 threads 1.35-1.52 s,
  // 8 threads 1.32-1.52 s. Deep footer stage 1.23 s -> 0.76 s.
  // Same bench: with 4 threads, reading the copyright line on the FIRST look
  // (retroFirst, the default at >= 4 threads) took Smokestack to 0.63-0.83 s.
  // At 2 threads it made first looks 1-2 s (scanspeed4), so it stays off there.
  if (cores >= 8) return 4;
  return cores >= 4 ? 2 : 1;
}
ort.env.wasm.numThreads = 1;

// Set from the load message: '' on the web (same origin), the user's server URL
// in the native app, where relative paths would resolve inside the app bundle.
let ORIGIN = '';
const BASE = '/scan-assets/';
const CACHE = 'scrybox-scan-assets';
let readerPromise = null;     // the first load (shared by every 'load' call)
let pendingReset = false;
let FLAGS = {};
let THREAD_OVERRIDE = null;

async function cachedBytes(cache, url, signal) {
  let res = cache ? await cache.match(url) : null;
  if (!res) {
    res = await fetch(ORIGIN + url, { signal });
    const type = res.headers.get('content-type') || '';
    if (!res.ok || type.includes('text/html')) throw new Error(`${url} not served (${res.status})`);
    if (cache) await cache.put(url, res.clone()).catch(() => {});
  }
  return new Uint8Array(await res.arrayBuffer());
}

async function gunzip(bytes) {
  // Served as a .gz file (not Content-Encoding), so decompress here.
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
  return new Response(stream).text();
}

async function fetchManifest(signal) {
  // Tiny and no-cache: the content hashes in it are the whole version check.
  const res = await fetch(`${ORIGIN}${BASE}manifest.json`, { cache: 'no-cache', signal });
  if (!res.ok) throw new Error(`manifest not served (${res.status})`);
  return res.json();
}

// cornelius has a fixed URL (shared with the server route), so it is kept in
// the Cache API like the hashed assets; each load serves the cached copy and
// stores a revalidated one for the next load (no identity guessing from
// headers; review R1-S2).
const CORN_URL = '/models/cornelius.onnx';
async function cornBytes(cache, signal) {
  const hit = cache ? await cache.match(CORN_URL) : null;
  if (!hit) return cachedBytes(cache, CORN_URL, signal);
  fetch(ORIGIN + CORN_URL, { cache: 'no-cache' }).then(async (res) => {
    const type = res.headers.get('content-type') || '';
    if (res.ok && !type.includes('text/html')) await cache.put(CORN_URL, res);
  }).catch(() => {});
  return new Uint8Array(await hit.arrayBuffer());
}

async function warmUp(rec, cornelius) {
  const zeros = (dims) => new ort.Tensor('float32', new Float32Array(dims.reduce((a, b) => a * b, 1)), dims);
  await cornelius.run({ image: zeros([1, 3, CORN_SIZE, CORN_SIZE]) });
  await rec.run({ [rec.inputNames[0]]: zeros([1, 3, 48, 320]) });
}

// Drop cached assets no current bundle names. Only after a bundle is ACTIVE,
// so a failed refresh never costs the last good copy (review R1-S1).
const releaseSessions = (...ss) => { for (const x of ss) x?.release?.()?.catch?.(() => {}); };
async function pruneCache() {
  const cache = typeof caches !== 'undefined' ? await caches.open(CACHE).catch(() => null) : null;
  if (!cache) return;
  const keep = new Set([bundles.active, bundles.pending].filter(Boolean)
    .flatMap(b => [b.manifest.index, b.manifest.rec, b.manifest.dict].map(n => BASE + n)));
  keep.add(CORN_URL);
  for (const req of await cache.keys()) {
    if (!keep.has(new URL(req.url).pathname)) await cache.delete(req).catch(() => {});
  }
}

// A candidate that failed to load must not leave its bytes in the Cache API,
// or every retry would read the same bad copy (review R2-S2). Files that the
// active/pending bundle also names (shared rec/dict, last-known-good A) stay.
async function dropCandidate(cache, urls) {
  if (!cache) return;
  const keep = new Set([bundles.active, bundles.pending].filter(Boolean)
    .flatMap(b => [b.manifest.index, b.manifest.rec, b.manifest.dict].map(n => BASE + n)));
  for (const u of urls) if (!keep.has(u)) await cache.delete(u).catch(() => {});
}

async function load(manifest = null, signal = undefined) {
  manifest ||= await fetchManifest(signal);
  const urls = [manifest.index, manifest.rec, manifest.dict].map(n => BASE + n);
  const cache = typeof caches !== 'undefined' ? await caches.open(CACHE).catch(() => null) : null;
  try { return await loadFrom(manifest, urls, cache, signal); } catch (err) {
    await dropCandidate(cache, urls);
    throw err;
  }
}

// Sessions this load created, owned until the bundle is returned: on abort,
// every completed one is released now and a late one when it completes, so a
// hung sibling cannot pin them (review R2-B2). A session still running its
// warm-up is not released under it; the bundle is retired when it returns.
function sessionOwner(signal) {
  const made = [];
  let disowned = false, warming = false;
  const drop = () => { disowned = true; releaseSessions(...made.splice(0)); };
  signal?.addEventListener?.('abort', () => { if (!warming) drop(); }, { once: true });
  return {
    create: (bytes, opts) => ort.InferenceSession.create(bytes, opts).then((x) => {
      if (disowned) { releaseSessions(x); throw new Error('refresh aborted'); }
      made.push(x);
      return x;
    }),
    warming: (v) => { warming = v; if (!v && signal?.aborted) drop(); },
    drop,
  };
}

async function loadFrom(manifest, urls, cache, signal) {
  const t0 = performance.now();
  const tFetch = performance.now();
  const [indexGz, recBytes, dictBytes, corn] = await Promise.all([
    cachedBytes(cache, urls[0], signal), cachedBytes(cache, urls[1], signal), cachedBytes(cache, urls[2], signal), cornBytes(cache, signal),
  ]);
  const fetchMs = Math.round(performance.now() - tFetch);
  const opts = { executionProviders: ['wasm'], graphOptimizationLevel: 'all' };
  // Index decode runs while ORT compiles the two sessions; a failure frees
  // whatever was created (review R1-S1).
  const own = sessionOwner(signal);
  const abortWait = new Promise((_, rej) => signal?.addEventListener?.('abort', () => rej(new Error('refresh aborted')), { once: true }));
  abortWait.catch(() => {});
  const parts = await Promise.race([Promise.allSettled([
    own.create(recBytes, opts),
    own.create(corn, opts),
    gunzip(indexGz).then(text => loadIndex(JSON.parse(text))),
  ]), abortWait]).catch((err) => { own.drop(); throw err; });
  const failed = parts.find(p => p.status === 'rejected');
  const [rec, cornelius, index] = parts.map(p => p.value);
  if (failed || signal?.aborted) { own.drop(); throw failed?.reason || new Error('refresh aborted'); }
  const chars = buildCharset(new TextDecoder().decode(dictBytes));
  // Warm-up: the first run of each session allocates and plans its kernels
  // (hundreds of ms on a phone); pay that here, not on the first card.
  const tWarm = performance.now();
  own.warming(true);
  await warmUp(rec, cornelius).catch(() => {});
  own.warming(false);
  if (signal?.aborted) throw new Error('refresh aborted');
  const warmMs = Math.round(performance.now() - tWarm);
  const reader = createReader({ ort, cornelius, rec, chars, index, fastSettle: !!FLAGS.fastSettle, titleGate: !!FLAGS.titleGate, retroFirst: FLAGS.retroFirst ?? ort.env.wasm.numThreads >= 4 });
  // Which assets this worker actually runs, for scan telemetry: rules out a
  // stale cached index/model when live and replay disagree.
  const info = { index: manifest.index, rec: manifest.rec, cornBytes: corn.length, threads: ort.env.wasm.numThreads, threadsOverride: THREAD_OVERRIDE, flags: FLAGS, isolated: !!self.crossOriginIsolated, simd: ort.env.wasm.simd !== false, assetMs: fetchMs, warmMs };
  return { reader, info, manifest, rec, cornelius, busy: 0, retired: false, loadMs: Math.round(performance.now() - t0) };
}

// Reader bundles: first load, camera-start freshness check, swap (scanBundles.js).
const bundles = createBundles({
  loadBundle: (manifest, signal) => load(manifest, signal),
  fetchManifest: (signal) => fetchManifest(signal),
  release: (b) => releaseSessions(b.rec, b.cornelius),
  prune: () => { pruneCache().catch(() => {}); },
});

self.onmessage = async (e) => {
  const { type, id } = e.data;
  if (type === 'load') {
    ORIGIN = e.data.base || '';
    if (!readerPromise) { ort.env.wasm.numThreads = pickThreads(e.data.threads); THREAD_OVERRIDE = e.data.threads || null; FLAGS = e.data.flags || {}; }
    readerPromise ||= load().then(b => bundles.install(b));
    try {
      const { loadMs, info } = await readerPromise;
      self.postMessage({ id, ready: true, loadMs, info });
    } catch (err) {
      readerPromise = null;
      self.postMessage({ id, ready: false, error: err?.message || String(err), threads: ort.env.wasm.numThreads });
    }
    return;
  }
  if (type === 'check') {
    try { if (readerPromise) await readerPromise; self.postMessage({ id, ...(await bundles.check()) }); }
    catch (err) { self.postMessage({ id, error: err?.message || String(err) }); }
    return;
  }
  // Applied at the start of the next read rather than immediately, so a read
  // still in flight when auto restarts cannot repopulate state afterwards.
  if (type === 'reset') { pendingReset = true; return; }
  // Phase 1: corners from the 384px copy. When there is no card this is the
  // whole answer, and the main thread never reads back the full frame.
  if (type === 'probe') {
    const { small, w, h } = e.data;
    try {
      if (!readerPromise) throw new Error('reader not loaded');
      await readerPromise;
      const bundle = bundles.begin();
      // A reset is for whatever bundle reads next; a fresh one starts clean.
      if (pendingReset) { pendingReset = false; bundle.reader.reset(); bundles.noteReset(bundle); }
      const quad = await bundles.use(bundle, r => r.probe(new Uint8ClampedArray(small), 4, w, h));
      bundles.noteProbe(bundle, quad);
      // Bundle identity on the empty result too (review R2-S1).
      const out = quad ? null : { ok: true, engine: 'client', index: bundle.info.index, rec: bundle.info.rec, frame: { width: w, height: h }, candidates: [], results: [], timings: {} };
      self.postMessage({ id, quad, out, small }, [small]);
    } catch (err) {
      self.postMessage({ id, error: err?.message || String(err), small }, [small]);
    }
    return;
  }
  // Phase 2: the full read, reusing the corners phase 1 found for this frame.
  if (type === 'read') {
    const { frame, w, h, quad, requireStill, frameId = null, guess = null } = e.data;
    try {
      if (!readerPromise) throw new Error('reader not loaded');
      await readerPromise;
      const bundle = bundles.current();
      const out = await bundles.use(bundle, r => r.read(
        { data: new Uint8ClampedArray(frame), width: w, height: h },
        null, { requireStill, quad, frameId, guess }));
      // The index this answer came from, per read (telemetry, review R1-S3).
      if (out) { out.index = bundle.info.index; out.rec = bundle.info.rec; }
      // A first-look guess holds the swap until it is verified (R2-S3).
      bundles.noteRead(bundle, out);
      self.postMessage({ id, out, frame }, [frame]);
    } catch (err) {
      self.postMessage({ id, error: err?.message || String(err), frame }, [frame]);
    }
  }
};
