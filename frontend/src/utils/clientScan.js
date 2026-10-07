// Main-thread side of on-device Scan Cards (see clientScanWorker.js).
//
// One worker, one frame in flight. The frame is grabbed at the SAME ceiling the
// server path uploads (FRAME_MAX), because that is the resolution the pipeline
// was validated at against saved phone frames; the 384x384 copy for cornelius
// is scaled by the canvas (GPU) rather than in JS.
//
// Two phases per frame: corners first from the 384px copy, and only when there
// is a card does the full frame get read back (~8 MB at 1920x1080). Empty desk
// and hand-in-motion frames, which are most auto passes, never pay for it.
//
// Every worker call has a deadline. A worker that crashes or wedges is torn
// down and the next call gets a fresh one, so a single bad frame can never
// leave the scanner "busy" forever.
import { FRAME_MAX, pixelPrint } from './fastScan';
export { needsServer } from './fastScan';
import { CORN_SIZE } from '../../../shared/clientScan/pipeline.mjs';
import { isNative, getServerUrl } from '../apiBase';
import { startLoad } from './scanLoad.js';

const LOAD_TIMEOUT_MS = 120000;   // first download of ~40 MB on a slow phone
const READ_TIMEOUT_MS = 8000;     // a normal read is well under 2 s
const LOAD_RETRY_MS = 30000;      // after a failed load, try again this much later

let worker = null;
let loading = false;        // a load (incl. its 1-thread fallback) is in flight
let ready = null;          // Promise<{ok, loadMs, error}>
let readyFailedAt = 0;
// One-thread fallback state for the session (see scanLoad.js).
const loadState = { forceOneThread: false, threadFallback: null };
let nextId = 1;
const waiting = new Map(); // id -> {resolve, timer}

// Ends the worker and fails its pending calls, without touching `ready`.
function retireWorker(reason) {
  if (worker) { worker.terminate(); worker = null; }
  for (const { resolve, timer } of waiting.values()) { clearTimeout(timer); resolve({ error: reason }); }
  waiting.clear();
}

function killWorker(reason) {
  retireWorker(reason);
  // The models lived in that worker; the next load must start over.
  ready = null;
}

function ensureWorker() {
  if (worker) return worker;
  worker = new Worker(new URL('./clientScanWorker.js', import.meta.url), { type: 'module' });
  worker.onmessage = (e) => {
    const w = waiting.get(e.data.id);
    if (!w) return;
    waiting.delete(e.data.id); clearTimeout(w.timer);
    w.resolve(e.data);
  };
  // During a load, only retire the worker: its pending load call resolves
  // with the error and startLoad decides (fallback / cooldown). `ready` stays
  // shared. After startup a crash invalidates the models as before.
  worker.onerror = (e) => (loading ? retireWorker : killWorker)(e?.message || 'scan worker failed');
  return worker;
}

function call(msg, transfer = [], timeoutMs = READ_TIMEOUT_MS, onTimeout = killWorker) {
  const id = nextId++;
  return new Promise((resolve) => {
    const timer = setTimeout(() => onTimeout('scan worker timed out'), timeoutMs);
    waiting.set(id, { resolve, timer });
    ensureWorker().postMessage({ ...msg, id }, transfer);
  });
}

// Where the worker fetches models and the index from. On the web that is this
// origin; in the native app it is the user's own server — the window.fetch shim
// in apiBase does not reach into workers, so it has to be passed explicitly.
function threadOverride() {
  try { return localStorage.getItem('scan.threads') || null; } catch { return null; }
}
// Experimental reader switches (review R2), all OFF unless set to '1' in
// localStorage, so the default path is exactly the validated one:
//   scan.fastSettle  shorter settle window on clean, distinct decoded frames
//   scan.titleGate   skip the title rescue batch when every first crop is flat
export function scanFlags() {
  const on = (k) => { try { return localStorage.getItem(k) === '1'; } catch { return false; } };
  return { fastSettle: on('scan.fastSettle'), titleGate: on('scan.titleGate') };
}
function assetBase() {
  return isNative ? getServerUrl() : '';
}

// Start the one-time download. Resolves {ok:false} rather than throwing: a
// phone that cannot run it simply keeps the server scanner. A failure is
// retried after LOAD_RETRY_MS instead of being remembered until reload.
function loadOnce(threads) {
  // A load timeout retires the worker but leaves `ready` to the load itself.
  return call({ type: 'load', base: assetBase(), threads, flags: scanFlags() }, [], LOAD_TIMEOUT_MS, retireWorker);
}

export function loadClientScan() {
  if (ready && readyFailedAt && Date.now() - readyFailedAt > LOAD_RETRY_MS) ready = null;
  if (!ready) {
    readyFailedAt = 0;
    const supported = typeof Worker !== 'undefined' && typeof WebAssembly !== 'undefined'
      && typeof DecompressionStream !== 'undefined' && (!isNative || !!getServerUrl());
    // retireWorker, not killWorker: `ready` is this very promise and must
    // stay shared by every caller until it settles.
    ready = !supported
      ? Promise.resolve({ ok: false, error: 'unsupported browser' })
      : (loading = true, startLoad({ loadOnce, retire: retireWorker, override: threadOverride(), state: loadState })
        .finally(() => { loading = false; }));
    const mine = ready;
    ready.then(r => { if (!r.ok && ready === mine) readyFailedAt = Date.now(); });
  }
  return ready;
}


let frameCanvas = null, smallCanvas = null;
function ctx2d(c) { return c.getContext('2d', { willReadFrequently: true }); }
function canvas(w, h) {
  if (typeof OffscreenCanvas !== 'undefined') return new OffscreenCanvas(w, h);
  const c = document.createElement('canvas'); c.width = w; c.height = h; return c;
}
// getImageData allocates every call; its buffer is transferred, never copied.
function pixels(ctx, w, h) { return ctx.getImageData(0, 0, w, h).data.buffer; }

// One capture -> probe -> read transaction at a time, module-wide. The canvases
// are shared, so a second scanner instance (remount) or an overlapping call
// must wait its turn rather than redraw the frame between the two phases.
let chain = Promise.resolve();
function serialized(fn) {
  const run = chain.then(fn, fn);
  chain = run.catch(() => {});
  return run;
}

// Forget the tracked card and pooled footer evidence (auto stop/start).
export function resetOnDevice() {
  if (worker) worker.postMessage({ type: 'reset' });
}

// Decoded-frame clock (R2-#6). Where the browser has
// requestVideoFrameCallback (Chrome/Edge, Safari 15.4+, Android Chrome) every
// presented camera frame bumps a counter, so a capture knows WHICH decoded
// frame it drew and the auto loop can wait for a new one instead of sampling
// the same pixels twice. Without it (older WebViews) frameId stays null and
// everything runs on timers exactly as before.
const clocks = new WeakMap();
const FRAME_CLOCK_STALE_MS = 500;   // video -> {frames, mediaTime, waiters}
export function frameClock(video) {
  if (!video || typeof video.requestVideoFrameCallback !== 'function') return null;
  let c = clocks.get(video);
  if (c) return c;
  c = { frames: 0, mediaTime: null, waiters: [], at: 0 };
  const tick = (_now, meta) => {
    c.at = performance.now();
    c.frames = meta?.presentedFrames ?? c.frames + 1;
    c.mediaTime = meta?.mediaTime ?? null;
    const w = c.waiters; c.waiters = [];
    for (const fn of w) fn();
    video.requestVideoFrameCallback(tick);
  };
  video.requestVideoFrameCallback(tick);
  clocks.set(video, c);
  return c;
}
// Resolves once a frame newer than `since` has been presented, or after
// maxMs (never waits forever: a paused stream must not stall auto).
export function newFrameAfter(video, since, maxMs) {
  const c = frameClock(video);
  if (!c || since == null || c.frames !== since) return Promise.resolve();
  return new Promise((resolve) => {
    const timer = setTimeout(() => { const i = c.waiters.indexOf(done); if (i >= 0) c.waiters.splice(i, 1); resolve(); }, maxMs);
    function done() { clearTimeout(timer); resolve(); }
    c.waiters.push(done);
  });
}

// The decoded frame the last capture drew (null without a live rVFC clock).
let lastFrameId = null;
export function lastCapturedFrame() { return lastFrameId; }

// Read one frame on-device. Returns the pipeline's server-shaped output
// ({frame, candidates, results:[{ok, scryfallId, ...}]}), or {error}.
export function readOnDevice(source, sw, sh, opts = {}) {
  return serialized(() => readOnce(source, sw, sh, opts));
}

async function readOnce(source, sw, sh, { requireStill = false } = {}) {
  const clock = frameClock(source);
  // Re-loads transparently if a crash or deadline tore the last worker down.
  const tq = performance.now();
  const st = await loadClientScan();
  if (!st.ok) return { error: st.error || 'on-device reader unavailable' };
  const span = { wait_ms: Math.round(performance.now() - tq) };
  const k = Math.min(1, FRAME_MAX / Math.max(sw, sh));
  const w = Math.round(sw * k), h = Math.round(sh * k);
  if (!frameCanvas || frameCanvas.width !== w || frameCanvas.height !== h) frameCanvas = canvas(w, h);
  if (!smallCanvas) smallCanvas = canvas(CORN_SIZE, CORN_SIZE);
  // Both canvases are drawn from the same video frame now, so the corners and
  // the pixels they are applied to can never come from different moments.
  let t = performance.now();
  const lap = (k) => { const n = performance.now(); span[k] = Math.round(n - t); t = n; };
  // The frame the canvases are about to draw (the latest presented one).
  // Frame identity (review R1-B2). With a live rVFC clock: the presented
  // frame number. Otherwise (no rVFC, or callbacks stopped for this stream)
  // a fingerprint of the pixels actually drawn: a frozen stream repeats it
  // exactly and is never counted twice, while live camera noise makes real
  // frames differ. Either way a duplicate is never settle evidence.
  const live = clock && clock.at && performance.now() - clock.at < FRAME_CLOCK_STALE_MS;
  const fc = ctx2d(frameCanvas); fc.drawImage(source, 0, 0, w, h);
  const sc = ctx2d(smallCanvas); sc.drawImage(source, 0, 0, CORN_SIZE, CORN_SIZE);
  const small = pixels(sc, CORN_SIZE, CORN_SIZE);
  const frameId = live ? clock.frames : `p${pixelPrint(small)}`;
  lastFrameId = live ? frameId : null;
  span.frame_id = frameId;
  if (live && clock.mediaTime != null) span.media_ms = Math.round(clock.mediaTime * 1000);
  lap('draw_ms');
  const p = await call({ type: 'probe', small, w, h }, [small]);
  lap('probe_ms');
  if (p.error) return { error: p.error, span };
  if (!p.quad) return { ...p.out, span };          // no card: skip the full-frame readback
  const frame = pixels(fc, w, h);
  lap('readback_ms');
  const r = await call({ type: 'read', frame, w, h, quad: p.quad, requireStill, frameId }, [frame]);
  lap('worker_read_ms');
  return r.error ? { error: r.error, span } : { ...r.out, span };
}

// Server fallback wants a JPEG of the same frame the client just looked at.
export function lastFrameJpeg() {
  if (!frameCanvas) return null;
  if (frameCanvas.convertToBlob) return frameCanvas.convertToBlob({ type: 'image/jpeg', quality: 0.88 });
  return new Promise((res, rej) => frameCanvas.toBlob(b => (b ? res(b) : rej(new Error('encode'))), 'image/jpeg', 0.88));
}

// Turn on-device answers into card_cache rows (prices, image, set) via the
// backend, so the tray and Send flow see exactly what a server scan returns.
const hydrated = new Map();
// Duration of the last /cards round trip (0 when every hit was cached), read by
// scan telemetry right after hydrateResults resolves.
let lastHydrateMs = 0;
export function takeHydrateMs() { const v = lastHydrateMs; lastHydrateMs = 0; return v; }   // scryfallId -> hydrated result (auto passes re-see cards)
const HYDRATE_TIMEOUT_MS = 8000;
export function isHydrated(scryfallId) { return hydrated.has(scryfallId); }
// In-flight /cards requests (R2-#13): an auto loop that keeps capturing while
// a hydration is still pending re-sees the same card; it joins the request
// already running for those ids instead of sending another.
const inflight = new Map();
export function hydrateResults(results, signal) {
  const key = results.filter(r => r.ok && r.scryfallId && !hydrated.has(r.scryfallId)).map(r => `${r.number}:${r.scryfallId}`).sort().join(',');
  if (!key) return hydrateOnce(results, signal);
  let p = inflight.get(key);
  if (!p) {
    p = hydrateOnce(results, signal);
    inflight.set(key, p);
    const clear = () => { if (inflight.get(key) === p) inflight.delete(key); };
    p.then(clear, clear);
  }
  return p;
}
async function hydrateOnce(results, signal) {
  results = results.map(x => (x.ok && hydrated.has(x.scryfallId) ? { ...x, ...hydrated.get(x.scryfallId), number: x.number } : x));
  const hits = results.filter(r => r.ok && r.scryfallId && !r.card);
  if (!hits.length) return results;
  // Aborted with the scan, and never allowed to hold the scanner busy forever.
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), HYDRATE_TIMEOUT_MS);
  const onAbort = () => ctl.abort();
  if (signal?.aborted) ctl.abort();
  signal?.addEventListener('abort', onAbort, { once: true });
  let r, j;
  const th = performance.now();
  try {
    r = await fetch('/api/cardscan/cards', {
      signal: ctl.signal,
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ results: hits.map(h => ({ number: h.number, scryfallId: h.scryfallId, title: h.title, via: h.via })) }),
    });
    // The body is part of the request: a stalled stream is still covered by
    // the deadline and the scan's abort.
    j = await r.json().catch((e) => { if (e?.name === 'AbortError') throw e; return {}; });
  } finally { clearTimeout(timer); signal?.removeEventListener('abort', onAbort); }
  lastHydrateMs = Math.round(performance.now() - th);
  if (!r.ok || !j.ok) throw new Error(j.error || 'hydrate failed');
  const byNumber = new Map(j.results.map(x => [x.number, x]));
  for (const h of hits) { const x = byNumber.get(h.number); if (x?.ok && x.card) hydrated.set(h.scryfallId, x); }
  const out = results.map(x => (x.ok && byNumber.has(x.number) ? { ...x, ...byNumber.get(x.number) } : x));
  // A card the phone proved but the backend could not turn into a row is not
  // a result the tray can use. Throw so the caller takes the server path
  // instead of accepting an answer with nothing in it.
  if (out.some(x => x.scryfallId && !(x.ok && x.card))) throw new Error('hydrate incomplete');
  return out;
}

// Card rows for footer twins the reader could not split (printing picker).
// Same deadline as hydration (covers the body too), and cached per twin set:
// Auto re-sees the card every pass.
const choiceCache = new Map();   // twin-set key -> Promise<cards> (in flight or done)
export function fetchChoices(choices, signal) {
  const ids = (choices || []).map(c => c.scryfallId || c.id);
  const key = [...ids].sort().join(',');
  if (choiceCache.has(key)) return choiceCache.get(key);
  const p = fetchChoicesOnce(ids, signal);
  if (choiceCache.size >= 64) choiceCache.delete(choiceCache.keys().next().value);
  choiceCache.set(key, p);
  // Failures are not cached; overlapping callers share the one request.
  p.catch(() => { if (choiceCache.get(key) === p) choiceCache.delete(key); });
  return p;
}
async function fetchChoicesOnce(ids, signal) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), HYDRATE_TIMEOUT_MS);
  const onAbort = () => ctl.abort();
  if (signal?.aborted) ctl.abort();
  signal?.addEventListener('abort', onAbort, { once: true });
  try {
    const r = await fetch('/api/cardscan/choices', { signal: ctl.signal, method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ ids }) });
    const j = await r.json().catch((e) => { if (e?.name === 'AbortError') throw e; return {}; });
    if (!r.ok || !j.ok || !Array.isArray(j.cards) || j.cards.length !== ids.length) throw new Error(j.error || 'choices failed');
    return j.cards;
  } finally { clearTimeout(timer); signal?.removeEventListener('abort', onAbort); }
}
