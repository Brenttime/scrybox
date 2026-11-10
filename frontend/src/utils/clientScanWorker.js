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
let readerPromise = null;
let pendingReset = false;
let FLAGS = {};
let THREAD_OVERRIDE = null;

async function cachedBytes(cache, url) {
  let res = cache ? await cache.match(url) : null;
  if (!res) {
    res = await fetch(ORIGIN + url);
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

async function fetchManifest() {
  // Tiny and no-cache: the content hashes in it are the whole version check.
  const res = await fetch(`${ORIGIN}${BASE}manifest.json`, { cache: 'no-cache' });
  if (!res.ok) throw new Error(`manifest not served (${res.status})`);
  return res.json();
}

// Same assets (content-hashed names) = same reader.
const sameAssets = (a, b) => !!a && !!b && a.index === b.index && a.rec === b.rec && a.dict === b.dict;

// cornelius has a fixed URL (shared with the server route), so it is kept in
// the Cache API like the hashed assets and revalidated in the background
// (one conditional request); a changed model is picked up by the next load.
const CORN_URL = '/models/cornelius.onnx';
async function cornBytes(cache) {
  const hit = cache ? await cache.match(CORN_URL) : null;
  if (!hit) return cachedBytes(cache, CORN_URL);
  fetch(ORIGIN + CORN_URL, { cache: 'no-cache' }).then(res => {
    const type = res.headers.get('content-type') || '';
    if (res.ok && !type.includes('text/html')
      && res.headers.get('content-length') !== hit.headers.get('content-length')) cache.put(CORN_URL, res);
  }).catch(() => {});
  return new Uint8Array(await hit.arrayBuffer());
}

async function warmUp(rec, cornelius) {
  const zeros = (dims) => new ort.Tensor('float32', new Float32Array(dims.reduce((a, b) => a * b, 1)), dims);
  await cornelius.run({ image: zeros([1, 3, CORN_SIZE, CORN_SIZE]) });
  await rec.run({ [rec.inputNames[0]]: zeros([1, 3, 48, 320]) });
}

async function load(manifest = null) {
  const t0 = performance.now();
  // Ask the browser not to evict the ~25 MB of models and index under storage
  // pressure (granted silently to installed PWAs; harmless when refused).
  self.navigator?.storage?.persist?.().catch(() => {});
  manifest ||= await fetchManifest();
  const urls = [manifest.index, manifest.rec, manifest.dict].map(n => BASE + n);
  const cache = typeof caches !== 'undefined' ? await caches.open(CACHE).catch(() => null) : null;
  if (cache) {
    // Drop assets a newer manifest no longer names.
    for (const req of await cache.keys()) {
      const path = new URL(req.url).pathname;
      if (path !== CORN_URL && !urls.includes(path)) cache.delete(req);
    }
  }
  const tFetch = performance.now();
  const [indexGz, recBytes, dictBytes, corn] = await Promise.all([
    cachedBytes(cache, urls[0]), cachedBytes(cache, urls[1]), cachedBytes(cache, urls[2]), cornBytes(cache),
  ]);
  const fetchMs = Math.round(performance.now() - tFetch);
  const opts = { executionProviders: ['wasm'], graphOptimizationLevel: 'all' };
  // Index decode runs while ORT compiles the two sessions.
  const [rec, cornelius, index] = await Promise.all([
    ort.InferenceSession.create(recBytes, opts),
    ort.InferenceSession.create(corn, opts),
    gunzip(indexGz).then(text => loadIndex(JSON.parse(text))),
  ]);
  const chars = buildCharset(new TextDecoder().decode(dictBytes));
  // Warm-up: the first run of each session allocates and plans its kernels
  // (hundreds of ms on a phone); pay that here, not on the first card.
  const tWarm = performance.now();
  await warmUp(rec, cornelius).catch(() => {});
  const warmMs = Math.round(performance.now() - tWarm);
  const reader = createReader({ ort, cornelius, rec, chars, index, fastSettle: !!FLAGS.fastSettle, titleGate: !!FLAGS.titleGate, retroFirst: FLAGS.retroFirst ?? ort.env.wasm.numThreads >= 4 });
  // Which assets this worker actually runs, for scan telemetry: rules out a
  // stale cached index/model when live and replay disagree.
  const info = { index: manifest.index, rec: manifest.rec, cornBytes: corn.length, threads: ort.env.wasm.numThreads, threadsOverride: THREAD_OVERRIDE, flags: FLAGS, isolated: !!self.crossOriginIsolated, simd: ort.env.wasm.simd !== false, fetchMs, warmMs };
  return { reader, info, manifest, loadMs: Math.round(performance.now() - t0) };
}

// Camera start: compare the device's loaded card index with the server's by
// content hash. Same -> nothing to do (one tiny request). Different -> build
// the new reader in the background from the Cache API / network while the
// current one keeps reading, then swap it in between reads. A failed update
// keeps the current reader.
let updating = null;
async function checkAssets() {
  if (!readerPromise) return { loaded: false };
  const current = await readerPromise;
  const manifest = await fetchManifest();
  if (sameAssets(current.manifest, manifest)) return { fresh: true, info: current.info };
  updating ||= load(manifest).then(next => {
    readerPromise = Promise.resolve(next);
    return next;
  }).finally(() => { updating = null; });
  const next = await updating;
  return { updated: true, from: current.info.index, info: next.info, loadMs: next.loadMs };
}

self.onmessage = async (e) => {
  const { type, id } = e.data;
  if (type === 'load') {
    ORIGIN = e.data.base || '';
    if (!readerPromise) { ort.env.wasm.numThreads = pickThreads(e.data.threads); THREAD_OVERRIDE = e.data.threads || null; FLAGS = e.data.flags || {}; }
    readerPromise ||= load();
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
    try { self.postMessage({ id, ...(await checkAssets()) }); }
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
      const { reader } = await readerPromise;
      if (pendingReset) { pendingReset = false; reader.reset(); }
      const quad = await reader.probe(new Uint8ClampedArray(small), 4, w, h);
      const out = quad ? null : { ok: true, engine: 'client', frame: { width: w, height: h }, candidates: [], results: [], timings: {} };
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
      const { reader } = await readerPromise;
      const out = await reader.read(
        { data: new Uint8ClampedArray(frame), width: w, height: h },
        null, { requireStill, quad, frameId, guess });
      self.postMessage({ id, out, frame }, [frame]);
    } catch (err) {
      self.postMessage({ id, error: err?.message || String(err), frame }, [frame]);
    }
  }
};
