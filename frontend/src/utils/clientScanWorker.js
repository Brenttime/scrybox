// On-device card reading for Scan Cards, off the main thread.
//
// Runs shared/clientScan/pipeline.mjs, the same module the Node replay harness
// (backend/scripts/client-scan-replay.mjs) validates against saved phone
// frames. Models and the title/printing index are fetched once from
// /scan-assets/ (content-hashed names, served immutable) and kept in the Cache
// API, so a returning phone starts reading without touching the network.
//
// Same ORT entry point and settings as detectWorker, for the reasons spelled
// out there: CPU wasm EP only (no jsep/webgpu binaries), one thread (no
// COOP/COEP on an arbitrary self-hosted proxy).
import * as ort from 'onnxruntime-web/wasm';
import { createReader } from '../../../shared/clientScan/pipeline.mjs';
import { buildCharset, loadIndex } from '../../../shared/clientScan/text.mjs';

ort.env.wasm.wasmPaths = '/ort/';
// Threads need cross-origin isolation (COEP credentialless + COOP, set by the
// backend). Replay on saved frames: 2 threads cut matched-read p50 ~30% on a
// 4-core host and 4 was no better, so every device defaults to 2 (a phone in
// desktop-site mode cannot be told from a desktop by its UA). localStorage
// 'scan.threads' (passed in the load message) overrides for A/B tests.
function pickThreads(override) {
  if (!self.crossOriginIsolated || typeof SharedArrayBuffer === 'undefined') return 1;
  const n = Number(override);
  if (n >= 1 && n <= 8) return Math.floor(n);
  const cores = self.navigator?.hardwareConcurrency || 1;
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

async function load() {
  const t0 = performance.now();
  // Revalidated, not refetched: a returning phone gets a 304 and then reads
  // every asset from the Cache API.
  const manifest = await (await fetch(`${ORIGIN}${BASE}manifest.json`, { cache: 'no-cache' })).json();
  const urls = [manifest.index, manifest.rec, manifest.dict].map(n => BASE + n);
  const cache = typeof caches !== 'undefined' ? await caches.open(CACHE).catch(() => null) : null;
  if (cache) {
    // Drop assets a newer manifest no longer names.
    for (const req of await cache.keys()) {
      if (!urls.includes(new URL(req.url).pathname)) cache.delete(req);
    }
  }
  const [indexGz, recBytes, dictBytes, cornBytes] = await Promise.all([
    cachedBytes(cache, urls[0]), cachedBytes(cache, urls[1]), cachedBytes(cache, urls[2]),
    cachedBytes(null, '/models/cornelius.onnx'),      // HTTP-cached by detectWorker's route
  ]);
  const opts = { executionProviders: ['wasm'], graphOptimizationLevel: 'all' };
  const [rec, cornelius] = await Promise.all([
    ort.InferenceSession.create(recBytes, opts),
    ort.InferenceSession.create(cornBytes, opts),
  ]);
  const index = loadIndex(JSON.parse(await gunzip(indexGz)));
  const chars = buildCharset(new TextDecoder().decode(dictBytes));
  const reader = createReader({ ort, cornelius, rec, chars, index, fastSettle: !!FLAGS.fastSettle, titleGate: !!FLAGS.titleGate });
  // Which assets this worker actually runs, for scan telemetry: rules out a
  // stale cached index/model when live and replay disagree.
  const info = { index: manifest.index, rec: manifest.rec, cornBytes: cornBytes.length, threads: ort.env.wasm.numThreads, threadsOverride: THREAD_OVERRIDE, flags: FLAGS, isolated: !!self.crossOriginIsolated, simd: ort.env.wasm.simd !== false };
  return { reader, info, loadMs: Math.round(performance.now() - t0) };
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
    const { frame, w, h, quad, requireStill, frameId = null } = e.data;
    try {
      if (!readerPromise) throw new Error('reader not loaded');
      const { reader } = await readerPromise;
      const out = await reader.read(
        { data: new Uint8ClampedArray(frame), width: w, height: h },
        null, { requireStill, quad, frameId });
      self.postMessage({ id, out, frame }, [frame]);
    } catch (err) {
      self.postMessage({ id, error: err?.message || String(err), frame }, [frame]);
    }
  }
};
