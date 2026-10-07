import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Zap, ZapOff, ScanLine, Check, X, SwitchCamera, Camera, Sparkles, Trash2, Send, Undo2, Minus, Plus } from 'lucide-react';
import { resolveCardPrice, usdPrice } from '../utils/resolveCardPrice';
import { priceText } from '../utils/formatPrice';
import { displayName } from '../utils/languages';
import { useT } from '../utils/i18n';
import { FRAME_MAX, fitContain, quadPath, zoomPlan, nextFailStreak, serverAllowed, nextEdgeRun, isEdgePartial, nextPresentation, presentationInput, dedupeFresh, notePresence, edgeDirection, choiceHits, choiceLabel } from '../utils/fastScan';
import { loadClientScan, readOnDevice, resetOnDevice, lastFrameJpeg, hydrateResults, needsServer, takeHydrateMs, isHydrated, frameClock, newFrameAfter, lastCapturedFrame, fetchChoices } from '../utils/clientScan';
import { scanTelemetry } from '../utils/scanTelemetry';

// Fast scan. One request per scan: the frame goes up once, the cardscan
// sidecar detects, warps and OCRs every card from the same decoded pixels, and
// cards already identified on the table come back from its identity cache.
// Identity is text-proven (title + collector footer) against Scryfall's full
// printing index, so an answer is an exact printing or nothing.
//
// On-device first: when the phone can run it (utils/clientScan.js), the same
// title + collector-number proof runs locally on a still, in-frame card and
// only the Scryfall id goes to the backend for prices. Anything it cannot
// prove, or a device that cannot load it, uses the server path unchanged.

const AUTO_GAP_MS = 60;
// After a pass the gate rejected (moving, settling, near the edge, blurry)
// no OCR ran, so the CPU is mostly idle: on a desktop-class device look again
// sooner. The settle window is time-guarded in the pipeline, so this cannot
// shorten it; it only removes idle time between observations.
const AUTO_GATE_MS = (typeof navigator !== 'undefined' && (navigator.hardwareConcurrency || 0) >= 8) ? 25 : AUTO_GAP_MS;
const AUTO_IDLE_MS = 350;
// Directional edge hints (R2-#9), keyed by edgeDirection().
const EDGE_HINT = {
  down: 'fastscan.hintMoveDown', up: 'fastscan.hintMoveUp', left: 'fastscan.hintMoveLeft', right: 'fastscan.hintMoveRight',
  'down-left': 'fastscan.hintMoveDownLeft', 'down-right': 'fastscan.hintMoveDownRight', 'up-left': 'fastscan.hintMoveUpLeft', 'up-right': 'fastscan.hintMoveUpRight',
  back: 'fastscan.hintMoveBack',
};
// With rVFC, the shortest pause before waiting for the next decoded frame:
// yields the main thread (tray, overlay) between passes.
const AUTO_FRAME_MIN_MS = 8;
const HYDRATE_RETRY_MS = [1000, 3000, 10000, 30000];
const AUTO_FRAME_MAX_EXTRA_MS = 40;
const FAST_DEVICE = (typeof navigator !== 'undefined' && (navigator.hardwareConcurrency || 0) >= 8);
const AUTO_BUSY_MS = 1000;   // sidecar said 429: back off instead of re-asking in 60 ms
// The tray is capped for render cost, but unsent scans are never dropped to
// make room: at the cap, auto pauses and asks for a Send instead.
const TRAY_MAX = 10000;
// Server bulk endpoints accept 1-250 entries; larger trays are sent in chunks.
const SEND_CHUNK = 250;
const chunks = (a, n = SEND_CHUNK) => Array.from({ length: Math.ceil(a.length / n) }, (_, i) => a.slice(i * n, i * n + n));

// JSON body or a readable error, never a SyntaxError from a proxy's HTML page.
async function readJson(r) {
  try { return await r.json(); } catch { return { ok: false, error: `HTTP ${r.status}` }; }
}

async function grabJpeg(source, sw, sh, canvasRef) {
  const k = Math.min(1, FRAME_MAX / Math.max(sw, sh));
  const w = Math.round(sw * k), h = Math.round(sh * k);
  if (typeof OffscreenCanvas !== 'undefined') {
    const oc = new OffscreenCanvas(w, h);
    oc.getContext('2d').drawImage(source, 0, 0, w, h);
    return oc.convertToBlob({ type: 'image/jpeg', quality: 0.88 });
  }
  const c = canvasRef.current || (canvasRef.current = document.createElement('canvas'));
  c.width = w; c.height = h;
  c.getContext('2d').drawImage(source, 0, 0, w, h);
  return new Promise((res, rej) => c.toBlob(b => (b ? res(b) : rej(new Error('encode'))), 'image/jpeg', 0.88));
}

// Per-finish helpers shared by the tray and Send.
const qtyOf = (row) => Math.max(1, row.qty || 1);
const printingOf = (row) => (row.foil ? 'Holofoil' : 'Normal');
const canFoil = (card) => card?.price_holofoil > 0 || (Array.isArray(card?.finishes) && card.finishes.includes('foil'));
const rowPriceOf = (row) => { const p = usdPrice(row.card, printingOf(row)); return p == null ? null : p * qtyOf(row); };

// One tray card, memoized (R2-#12): an auto pass that adds or updates one
// row no longer re-renders every row of a long tray. Props are the row object
// (replaced only when that row changes) and stable callbacks.
const TrayCard = memo(function TrayCard({ row, dest, t, onEdit, onDismiss, onPatch, onRetry }) {
  const price = rowPriceOf(row);
  return (
<li className={`fs-card${row.sent ? ' is-added' : ''}${row.pending ? ' is-pending' : ''}${row.hydrateFailed ? ' is-failed' : ''}${row.needsChoice ? ' is-choice' : ''}`} aria-busy={row.pending || undefined}>
        <div className="fs-card-art">
          {row.card.image_url ? <img src={row.card.image_url} alt="" loading="lazy" crossOrigin="anonymous" /> : null}
          {!row.sent && !row.pending && (
            <button type="button" className="fs-card-edit" onClick={() => onEdit(row)} aria-label={row.needsChoice ? t('fastscan.choosePrinting') : t('fastscan.changePrinting')} />
          )}
          {row.needsChoice && <span className="fs-card-choose" aria-hidden="true">{t('fastscan.choosePrinting')}</span>}
          {row.pending && row.hydrateFailed && (
            <button type="button" className="fs-card-edit fs-card-retry" onClick={() => onRetry(row)} aria-label={t('fastscan.hydrateRetry')} title={t('fastscan.hydrateRetry')} />
          )}
          <button type="button" className="fs-card-x" onClick={() => onDismiss(row.key)} aria-label={t('fastscan.dismiss')}><X size={12} /></button>
          {price != null && <span className="fs-card-price">{priceText(price, 'USD')}</span>}
          {row.sent && <span className="fs-card-badge"><Check size={14} /></span>}
        </div>
        <div className="fs-card-name">{displayName(row.card)}</div>
        <div className="fs-card-meta">{row.needsChoice ? choiceLabel(row.choose) : `${String(row.card.set_id || '').toUpperCase()} · #${row.card.number}`}</div>
        {!row.sent && (
          <div className="fs-card-quick">
            <div className="fs-qty" role="group" aria-label={t('fastscan.quantity')}>
              <button type="button" onClick={() => onPatch(row.key, x => ({ qty: Math.max(1, qtyOf(x) - 1) }))} disabled={qtyOf(row) <= 1} aria-label={t('fastscan.qtyLess')}><Minus size={12} /></button>
              <span aria-live="polite">{qtyOf(row)}</span>
              <button type="button" onClick={() => onPatch(row.key, x => ({ qty: Math.min(99, qtyOf(x) + 1) }))} aria-label={t('fastscan.qtyMore')}><Plus size={12} /></button>
            </div>
            {dest === 'collection' && (
              <button type="button" className={`fs-foil${row.foil ? ' is-on' : ''}`} aria-pressed={!!row.foil}
                disabled={!row.foil && !canFoil(row.card)}
                onClick={() => onPatch(row.key, x => ({ foil: !x.foil }))}>
                <Sparkles size={11} />{t('fastscan.foil')}
              </button>
            )}
          </div>
        )}
      </li>
  );
});

export default function FastScanner({ onAddSuccess, showToast }) {
  const { t } = useT();
  const videoRef = useRef(null);
  const overlayRef = useRef(null);
  const canvasRef = useRef(null);
  // When the card now in view was first seen, so the latency pill shows the
  // user's real wait across every auto pass, not just the final read.
  const firstSeenRef = useRef(null);
  const streamRef = useRef(null);
  const busyRef = useRef(false);
  const autoRef = useRef(false);
  const timerRef = useRef(null);
  const runRef = useRef(0);          // auto-loop generation; a stale loop sees a new number and exits
  const aliveRef = useRef(true);     // false after unmount: late results must not touch state
  const scanAbortRef = useRef(null); // in-flight server read, aborted on stop/unmount
  const resultsRef = useRef([]);
  const failStreakRef = useRef(null); // same unresolved card, repeated: back off the server
  // One id per camera session: the sidecar scopes its identity cache to it.
  const sessionRef = useRef(Math.random().toString(36).slice(2, 12));
  const seenIdsRef = useRef(new Map()); // card.id -> {at, place, broken} (auto de-dupe)
  const presRef = useRef(null);         // current presentation (one card in view), for telemetry
  const epochRef = useRef(0);           // last presentation epoch handed out this run (monotonic)
  const hydrateAbortRef = useRef(null); // background hydrations, aborted on unmount only
  const hydrateLifeRef = useRef(new Map()); // row key -> {busy, timer}: one retry chain per row
  const onDeviceRef = useRef(false);
  const edgeRunRef = useRef(null);   // consecutive near-edge partials, same place
  const noTitleRunRef = useRef(0);   // consecutive auto passes with unreadable title OCR

  const [service, setService] = useState(null);
  const [devices, setDevices] = useState([]);
  const [deviceId, setDeviceId] = useState(() => localStorage.getItem('fastscan.device') || '');
  const [cameraOn, setCameraOn] = useState(false);
  const [torch, setTorch] = useState(false);
  const [torchOk, setTorchOk] = useState(false);
  // Mode switch (like a camera app's photo/video): the side button picks
  // Single or Auto; the shutter acts in that mode. In Auto the shutter starts
  // and stops the continuous loop.
  const [mode, setMode] = useState('auto');   // always opens in Auto
  const [auto, setAuto] = useState(false);   // auto loop running
  const [lists, setLists] = useState([]);
  const [dest, setDest] = useState(() => localStorage.getItem('fastscan.dest') || 'collection');
  const [sending, setSending] = useState(false);
  const [busy, setBusy] = useState(false);
  const [flash, setFlash] = useState(0);
  const [latency, setLatency] = useState(null);
  const [hint, setHint] = useState('');
  const [error, setError] = useState('');
  const [results, setResults] = useState([]);
  const [onDevice, setOnDevice] = useState(false);
  // Change printing: tap a scanned card -> every physical printing of that
  // name (same search the manual add uses) -> tap one to swap it in place.
  const [editKey, setEditKey] = useState(null);
  // Last send, kept briefly so a wrong destination can be reversed.
  const [undo, setUndo] = useState(null);   // {rows, dest, where, entryIds?}
  const undoTimer = useRef(null);
  useEffect(() => () => clearTimeout(undoTimer.current), []);
  const [prints, setPrints] = useState(null);   // null = loading, [] = none

  useEffect(() => {
    fetch('/api/lists').then(r => (r.ok ? r.json() : [])).then(d => setLists(Array.isArray(d) ? d : (d.lists || []))).catch(() => {});
  }, []);
  // One-time on-device download, started once the camera is on so opening the
  // tab alone costs nothing. Failure just leaves the server path in charge.
  // A failed load (flaky network, server restarting) is retried while the
  // camera stays on; loadClientScan itself spaces the attempts 30 s apart.
  useEffect(() => {
    if (!cameraOn) return;
    let live = true, timer;
    const attempt = () => loadClientScan().then(r => {
      if (!live) return;
      onDeviceRef.current = r.ok; setOnDevice(r.ok);
      scanTelemetry.device({ ok: r.ok, loadMs: r.loadMs, error: r.error, ...(r.info || {}) });
      if (!r.ok) {
        console.info('[fastscan] on-device reader unavailable:', r.error);
        if (r.error !== 'unsupported browser') timer = setTimeout(attempt, 31000);
      }
    });
    attempt();
    return () => { live = false; clearTimeout(timer); };
  }, [cameraOn]);
  useEffect(() => {
    fetch('/api/cardscan/status').then(r => setService(r.ok)).catch(() => setService(false));
  }, []);

  const stopCamera = useCallback(() => {
    streamRef.current?.getTracks().forEach(tr => tr.stop());
    streamRef.current = null;
    setCameraOn(false); setTorch(false);
  }, []);
  useEffect(() => {
    aliveRef.current = true;
    return () => {
      aliveRef.current = false; autoRef.current = false;
      // A generation counter, not a DOM ref: bumping the live value is the point.
      // eslint-disable-next-line react-hooks/exhaustive-deps
      runRef.current++;
      clearTimeout(timerRef.current); scanAbortRef.current?.abort(); hydrateAbortRef.current?.abort();
      // eslint-disable-next-line react-hooks/exhaustive-deps
      for (const l of hydrateLifeRef.current.values()) clearTimeout(l.timer);
      stopCamera();
    };
  }, [stopCamera]);

  const startCamera = useCallback(async (id = deviceId) => {
    setError('');
    if (!window.isSecureContext || !navigator.mediaDevices?.getUserMedia) {
      setError(t('fastscan.insecure', { origin: window.location.origin })); return;
    }
    stopCamera();
    const video = { width: { ideal: 2560 }, height: { ideal: 1440 }, frameRate: { ideal: 30 } };
    if (id) video.deviceId = { exact: id }; else video.facingMode = { ideal: 'environment' };
    try {
      let stream;
      try { stream = await navigator.mediaDevices.getUserMedia({ video, audio: false }); }
      catch (e) { if (!id) throw e; stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: 'environment' } }, audio: false }); }
      streamRef.current = stream;
      const v = videoRef.current;
      v.srcObject = stream;
      await v.play().catch(() => {});
      const track = stream.getVideoTracks()[0];
      const caps = track.getCapabilities?.() || {};
      setTorchOk(!!caps.torch);
      if (caps.focusMode?.includes('continuous')) track.applyConstraints({ advanced: [{ focusMode: 'continuous' }] }).catch(() => {});
      setCameraOn(true);
      const all = await navigator.mediaDevices.enumerateDevices();
      setDevices(all.filter(d => d.kind === 'videoinput'));
    } catch (e) {
      console.error('camera', e);
      setError(t('fastscan.cameraDenied'));
    }
  }, [deviceId, stopCamera, t]);

  const cycleCamera = () => {
    if (devices.length < 2) return;
    const i = devices.findIndex(d => d.deviceId === deviceId);
    const next = devices[(i + 1) % devices.length].deviceId;
    setDeviceId(next); localStorage.setItem('fastscan.device', next); startCamera(next);
  };

  const toggleTorch = async () => {
    const track = streamRef.current?.getVideoTracks()[0];
    if (!track) return;
    try { await track.applyConstraints({ advanced: [{ torch: !torch }] }); setTorch(!torch); } catch { setTorchOk(false); }
  };

  const drawOverlay = useCallback((frame, candidates, byNumber) => {
    const c = overlayRef.current;
    if (!c) return;
    const dpr = window.devicePixelRatio || 1;
    const W = c.clientWidth, H = c.clientHeight;
    // Assigning width/height reallocates the backing store even when unchanged.
    const bw = Math.round(W * dpr), bh = Math.round(H * dpr);
    if (c.width !== bw || c.height !== bh) { c.width = bw; c.height = bh; }
    const ctx = c.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);
    if (!frame || !candidates?.length) return;
    const fit = fitContain(frame, W, H, 'cover');
    // Safe capture region (R2-#9): when the card is at a frame edge, outline
    // the area it must be fully inside (1.5% in from every edge; the reader's
    // band is 1%), so the user can see where to move it.
    if (candidates.some(c => c.edge_sides?.length)) {
      const m = 0.015;
      const [[x0, y0], , [x1, y1]] = quadPath({ box: [frame.width * m, frame.height * m, frame.width * (1 - 2 * m), frame.height * (1 - 2 * m)] }, fit);
      ctx.save(); ctx.setLineDash([8, 6]); ctx.lineWidth = 2; ctx.strokeStyle = 'rgba(251,191,36,0.85)';
      ctx.strokeRect(Math.max(2, x0), Math.max(2, y0), Math.min(W - 4, x1 - x0), Math.min(H - 4, y1 - y0));
      ctx.restore();
    }
    for (const cand of candidates) {
      const r = byNumber.get(cand.number);
      const good = r?.ok;
      const color = good ? '#34d399' : cand.eligible ? '#fbbf24' : 'rgba(255,255,255,0.45)';
      const pts = quadPath(cand, fit);
      ctx.lineJoin = 'round';
      ctx.lineWidth = 3;
      ctx.strokeStyle = color;
      ctx.shadowColor = color; ctx.shadowBlur = good ? 18 : 8;
      ctx.beginPath();
      pts.forEach(([x, y], i) => (i ? ctx.lineTo(x, y) : ctx.moveTo(x, y)));
      ctx.closePath(); ctx.stroke();
      ctx.shadowBlur = 0;
      if (good) {
        ctx.fillStyle = 'rgba(52,211,153,0.10)'; ctx.fill();
        const label = displayName(r.card);
        ctx.font = '600 13px -apple-system, system-ui, sans-serif';
        const tw = ctx.measureText(label).width + 16;
        const [lx, ly] = pts[0];
        const y = Math.max(4, ly - 28);
        ctx.fillStyle = 'rgba(10,14,24,0.78)';
        ctx.beginPath(); ctx.roundRect(lx, y, tw, 22, 11); ctx.fill();
        ctx.fillStyle = '#ecfdf5'; ctx.fillText(label, lx + 8, y + 15);
      }
    }
  }, []);

  // Background hydration of a pending tray row (R2-#13). Shared with any
  // other pass that proves the same id (hydrateResults dedupes in flight).
  // A failure never drops the proven card (review R1-B4): the row stays in
  // the tray, unsendable, marked failed, and is retried with backoff (and on
  // tap) until it hydrates or the user dismisses it.
  const hydrateRow = useCallback((row, attempt = 0) => {
    // One lifecycle per row (review R2-S3): a manual retry replaces any
    // scheduled one, and a row already hydrating is not started twice.
    const life = hydrateLifeRef.current;
    const cur = life.get(row.key);
    if (cur?.busy) return;
    clearTimeout(cur?.timer);
    life.set(row.key, { busy: true });
    const ctl = hydrateAbortRef.current || (hydrateAbortRef.current = new AbortController());
    const h = row.hit;
    const th = performance.now();
    setResults(prev => prev.map(x => (x.key === row.key ? { ...x, hydrateFailed: false } : x)));
    hydrateResults([{ number: h.number ?? 1, ok: true, scryfallId: h.scryfallId, title: h.title, via: h.via }], ctl.signal)
      .then((res) => {
        const card = res?.[0]?.card;
        if (!card) throw new Error('hydrate incomplete');
        if (!aliveRef.current) return;
        life.delete(row.key);
        setResults(prev => prev.map(x => (x.key === row.key ? { ...x, card, pending: false, hydrateFailed: false, hit: undefined } : x)));
        scanTelemetry.begin({ mode: 'hydrate', sw: 0, sh: 0 }).end({ outcome: 'hydrated', row: row.key, pres: row.pres, attempt, hydrate_ms: Math.round(performance.now() - th), usable_enqueue_ms: row.t0 != null ? Math.round(performance.now() - row.t0) : undefined });
      })
      .catch((e) => {
        if (!aliveRef.current || ctl.signal.aborted) { life.delete(row.key); return; }
        scanTelemetry.begin({ mode: 'hydrate', sw: 0, sh: 0 }).end({ outcome: 'hydrate-failed', row: row.key, pres: row.pres, attempt, error: e?.message || String(e) });
        let still = false;
        setResults(prev => prev.map(x => { if (x.key !== row.key) return x; still = true; return { ...x, hydrateFailed: true }; }));
        const delay = HYDRATE_RETRY_MS[Math.min(attempt, HYDRATE_RETRY_MS.length - 1)];
        const timer = setTimeout(() => {
          life.delete(row.key);
          if (aliveRef.current && still && resultsRef.current.some(x => x.key === row.key && x.pending)) hydrateRow(row, attempt + 1);
        }, delay);
        life.set(row.key, { busy: false, timer });
      });
  }, []);

  const scan = useCallback(async (source, { autoPass = false, gen = runRef.current } = {}) => {
    if (busyRef.current) return { busy: true };
    // Results commit only for the run that asked: an auto pass that outlives a
    // stop (or a stop+restart) must not add rows to the new run's tray.
    const stale = () => !aliveRef.current || (autoPass && (!autoRef.current || gen !== runRef.current));
    const sw = source.videoWidth || source.naturalWidth || source.width;
    const sh = source.videoHeight || source.naturalHeight || source.height;
    if (!sw || !sh) return { busy: true };
    // Auto passes do not flip the busy state: in Auto the shutter shows the
    // loop, and a re-render per 25-60 ms gate pass is pure main-thread work.
    busyRef.current = true; if (!autoPass) setBusy(true);
    if (!autoPass) setError('');
    const t0 = performance.now();
    // One telemetry record per pass (see utils/scanTelemetry.js). The id also
    // rides in X-Scan-Client so the sidecar's [scan] line can be joined to it.
    const tel = scanTelemetry.begin({ mode: autoPass ? 'auto' : 'shutter', sw, sh });
    try {
      let out = null;
      // Shutter press: hedge. The server read starts now, alongside the
      // on-device one, instead of only after the phone gives up — the old
      // sequential fallback is where the 2 s+ scans came from (a ~1.2 s local
      // miss, then a full server read). Auto passes stay sequential so the
      // 60 ms loop does not flood the 2-core sidecar.
      const abort = new AbortController();
      scanAbortRef.current = abort;
      // Why this frame goes to the server, logged by the sidecar next to its
      // answer so a slow or failing session can be diagnosed from the logs.
      let why = onDeviceRef.current ? (autoPass ? 'unproven' : 'hedge') : 'no-device';
      const serverRead = async (blobP) => {
        const headers = { 'Content-Type': 'image/jpeg', 'X-Scan-Session': sessionRef.current, 'X-Scan-Client': `od=${onDeviceRef.current ? 1 : 0} why=${why} mode=${autoPass ? 'auto' : 'shutter'} id=${tel.id}` };
        const ts = performance.now();
        const body = await blobP;
        tel.mark('jpeg_ms', performance.now() - ts);
        tel.set({ upload_bytes: body?.size, why });
        const tr = performance.now();
        const r = await fetch('/api/cardscan/frame', { method: 'POST', headers, body, signal: abort.signal });
        if (r.status === 429) { tel.set({ server_status: 429 }); return { busy: true, backoff: true }; }
        const j = await readJson(r);
        serverDoneAt = performance.now();
        tel.mark('server_rtt_ms', serverDoneAt - tr);
        tel.server(j);
        if (!r.ok || !j.ok) throw new Error(j.error || t('fastscan.serviceError'));
        return j;
      };
      const hedged = onDeviceRef.current && !autoPass ? serverRead(grabJpeg(source, sw, sh, canvasRef)) : null;
      hedged?.catch(() => {});
      let local = null, localStart = null, serverDoneAt = null;
      if (onDeviceRef.current) {
        const tl = performance.now();
        localStart = tl;
        local = await readOnDevice(source, sw, sh, { requireStill: autoPass });
        tel.mark('local_ms', performance.now() - tl);
        tel.local(local);
        if (local?.error) { console.warn('[fastscan] on-device read failed:', local.error); why = 'device-error'; }
        else if (!local?.candidates?.length) { why = 'no-card'; failStreakRef.current = null; }
        else if (!local.candidates[0].eligible) why = String(local.candidates[0].status || 'ineligible').replace(/\s+/g, '-');
        // Proven on the phone (or an auto pass the stillness gate held back):
        // done. Anything else — unproven card, no card on a shutter press,
        // failure — goes to the server with the same frame.
        const r0 = local?.results?.[0];
        if (autoPass && r0 && !r0.ok && r0.error === 'no confident card title') noTitleRunRef.current++;
        else if (!local?.error && local?.candidates?.[0]?.status !== 'settling' && local?.candidates?.[0]?.status !== 'moving') noTitleRunRef.current = 0;
        // A settling pass says nothing new about the edge: keep the run. Any
        // other outcome (moving included) goes through nextEdgeRun and resets it.
        if (local?.candidates?.[0]?.status !== 'settling') edgeRunRef.current = nextEdgeRun(edgeRunRef.current, local);
        if (!needsServer(local, { autoPass, noTitleRun: noTitleRunRef.current, edgeRun: edgeRunRef.current?.count || 0 })) {
          // Auto: a proven card goes into the tray at once as a PENDING row
          // (not sendable) while /cards hydrates it in the background, so the
          // loop keeps capturing (R2-#13). Already-hydrated ids and shutter
          // presses keep the inline path.
          const proven = (local.results || []).filter(x => x.ok && x.scryfallId);
          const background = autoPass && proven.length > 0 && proven.every(x => !isHydrated(x.scryfallId));
          if (background) {
            out = { ...local, results: local.results.map(x => (x.ok && x.scryfallId
              ? { ...x, card: { id: `mtg-${x.scryfallId}`, name: x.title, set_id: x.set, number: x.num }, pendingHydrate: true } : x)) };
            tel.set({ answered_by: 'client', hydrate: 'background' });
          } else {
            out = { ...local, results: await hydrateResults(local.results, abort.signal).catch((e) => { tel.set({ hydrate_error: e?.message || String(e) }); return null; }) };
            tel.mark('hydrate_ms', takeHydrateMs());
            if (!out.results) { out = null; why = 'hydrate-failed'; }
            else tel.set({ answered_by: 'client' });
          }
          // Footer twins proven on-device: fetch their card rows for the picker.
          if (out && out.results.some(x => !x.ok && x.choices?.length > 1)) {
            try {
              out = { ...out, results: await Promise.all(out.results.map(async x => (!x.ok && x.choices?.length > 1 ? { ...x, choices: await fetchChoices(x.choices, abort.signal) } : x))) };
              tel.set({ choice: true });
            } catch (e) { tel.set({ choice_error: e?.message || String(e) }); out = null; why = 'choices-failed'; }
          }
        }
      }
      if (out) abort.abort();
      else if (hedged) out = await hedged;
      else if (autoPass && !serverAllowed(failStreakRef.current, Date.now(), local)) {
        // Same card, same unresolved footer as the last few passes: don't pay
        // another ~0.9 s of sidecar OCR for the same answer. Keep telling the
        // user what would help instead.
        if (!stale()) setHint(t('fastscan.hintFooter'));
        tel.end({ outcome: 'held' });
        return { held: true };
      } else { noTitleRunRef.current = 0; edgeRunRef.current = null; out = await serverRead((async () => (onDeviceRef.current && await Promise.resolve(lastFrameJpeg()).catch(() => null)) || grabJpeg(source, sw, sh, canvasRef))()); }
      // Stopped or navigated away while this was in flight: drop it on the floor.
      if (stale()) { tel.end({ outcome: 'stale' }); return { busy: true }; }
      if (out.busy) { tel.end({ outcome: 'busy' }); return out; }
      if (!tel.get('answered_by')) tel.set({ answered_by: 'server' });
      const fromServer = tel.get('answered_by') === 'server';
      const streak = nextFailStreak(failStreakRef.current, out, Date.now(), { fromServer, sameFrameLocal: fromServer && !hedged ? local : null });
      failStreakRef.current = streak;
      const plan = zoomPlan({ candidates: out.candidates, results: out.results, frame: out.frame, sw: out.frame?.width || sw, sh: out.frame?.height || sh });
      const ms = Math.round(performance.now() - t0);
      const byNumber = new Map(out.results.map(x => [x.number ?? x.scene_number, x]));
      drawOverlay(out.frame, out.candidates, byNumber);
      // Proven printings, plus proven footers that several printings share
      // (the user picks those in the tray; never a guess).
      const hits = [...out.results.filter(x => x.ok && x.card), ...choiceHits(out.results)];
      const eligible = out.candidates.filter(c => c.eligible).length;
      if (!out.candidates.length) firstSeenRef.current = null;
      else if (firstSeenRef.current == null) firstSeenRef.current = t0;
      const waited = autoPass && firstSeenRef.current != null ? Math.round(performance.now() - firstSeenRef.current) : ms;
      // Presentation (R2 measurement fix): the card now in view, from the
      // client's own same-frame read when there is one.
      // Presentation input: geometry from the on-device read when it saw a
      // card, title + title time from whichever source actually read the
      // title (review R4-S1): the local title stage's end (capture + draw/
      // probe/readback + detect/refine/title), else the server response's
      // own completion time (hedged or sequential).
      const localHasCard = !!(local && !local.error && local.candidates?.length);
      const localTitled = localHasCard && (local.results || []).some(x => x.title);
      const lt = local?.timings || {};
      const localTitleAt = localStart != null && lt.title_ms != null ? localStart + (local.span?.wait_ms || 0) + (local.span?.draw_ms || 0) + (local.span?.probe_ms || 0) + (local.span?.readback_ms || 0) + (lt.total_ms || 0) - (lt.footer_ms || 0) : null;
      const presOut = presentationInput(localHasCard ? local : null, localTitled, out);
      const titleAt = localTitled ? localTitleAt : (serverDoneAt ?? performance.now());
      const pres = nextPresentation(presRef.current, presOut, t0, epochRef.current, titleAt ?? performance.now());
      presRef.current = pres;
      if (pres) epochRef.current = Math.max(epochRef.current, pres.epoch);
      const edgeDir = edgeDirection(out.candidates[0]);
      if (!out.candidates.length) setHint(t('fastscan.hintNoCard'));
      else if (!eligible && edgeDir) setHint(t(EDGE_HINT[edgeDir]));
      else if (!eligible) setHint(t('fastscan.hintAdjust', { reason: out.candidates[0].status }));
      else if (!hits.length && isEdgePartial(out)) setHint(edgeDir ? t(EDGE_HINT[edgeDir]) : t('fastscan.hintEdge'));
      else if (plan.tooSmall.length && hits.length < eligible) setHint(t('fastscan.hintCloser', { count: plan.tooSmall.length }));
      else if (!hits.length && streak && streak.count >= 2) setHint(t('fastscan.hintFooter'));
      else if (!hits.length) setHint(t('fastscan.hintHold'));
      else setHint('');

      // Auto: a card that stays on the table must not be re-added every pass,
      // nor after 4 s of failed reads while it never left (dedupeFresh).
      const now = Date.now();
      const fresh = hits.filter(h => !autoPass || dedupeFresh(seenIdsRef.current, h.card.id, now));
      notePresence(seenIdsRef.current, out, hits, now, new Set(fresh.map(h => h.card.id)));
      if (hits.length) firstSeenRef.current = null;
      const tEnd = performance.now();
      tel.end({
        outcome: hits.length ? (fresh.length ? 'added' : 'repeat') : (out.candidates.length ? 'miss' : 'no-card'),
        hits: hits.map(h => (h.choose ? `${h.card.name}[choose ${h.choose.map(c => `${c.set_id} ${c.number}`).join(' | ')}]` : `${h.card.name}[${h.card.set_id} ${h.card.number}]`)).slice(0, 8),
        total_ms: ms, waited_ms: waited,
        // Presentation timeline: epoch, and ms from its first candidate to
        // its first confident title and to this pass's end (proof/commit).
        pres: pres ? { epoch: pres.epoch, age_ms: Math.round(tEnd - pres.since), title_ms: pres.titleAt != null ? Math.round(pres.titleAt - pres.since) : undefined } : undefined,
        // Approximations, named as such: pass start -> tray insertion
        // ENQUEUED (not painted). Rows still awaiting /cards are counted in
        // pending_rows; their 'hydrated' record carries hydrate_ms and
        // usable_enqueue_ms (pass start -> sendable state enqueued).
        insert_enqueue_ms: fresh.length ? Math.round(tEnd - t0) : undefined,
        pending_rows: fresh.filter(h => h.pendingHydrate).length || undefined,
      });
      if (fresh.length) {
        setLatency({ total: waited, read: ms });
        setFlash(f => f + 1);
        navigator.vibrate?.(18);
        const rows = fresh.map(h => ({ key: `${h.card.id}-${now}-${Math.random().toString(36).slice(2, 7)}`, card: h.card, added: false, pending: !!h.pendingHydrate, hit: h, pres: pres?.epoch, t0,
          ...(h.choose ? { needsChoice: true, choose: h.choose } : {}) }));
        setResults(prev => [...rows, ...prev]);
        for (const row of rows) if (row.pending) hydrateRow(row);
      } else if (!autoPass) {
        setLatency({ total: ms, read: ms });
      }
      return { matched: fresh.length, none: !out.candidates.length, gated: out.candidates.length > 0 && !eligible && tel.get('answered_by') === 'client' };
    } catch (e) {
      tel.end({ outcome: e?.name === 'AbortError' ? 'aborted' : 'error', error: e?.message || String(e) });
      if (e?.name === 'AbortError' || stale()) return { busy: true };
      setError(e.message || String(e));
      return { error: true };
    } finally {
      busyRef.current = false;
      if (aliveRef.current) setBusy(false);
    }
  }, [drawOverlay, hydrateRow, t]);

  // Latest scan, read through a ref so a running loop never calls a stale one
  // from the render it started in.
  const scanRef = useRef(scan);
  scanRef.current = scan;
  resultsRef.current = results;

  // One loop per generation. Stopping bumps runRef, so a pass still awaiting
  // its scan when the user stops (and maybe restarts) sees a different number
  // and exits instead of scheduling a second loop beside the new one.
  const autoLoop = useCallback(async (gen) => {
    if (!autoRef.current || gen !== runRef.current) return;
    const unsent = resultsRef.current.filter(x => !x.sent).length;
    if (unsent >= TRAY_MAX) {
      setHint(t('fastscan.trayFull', { count: TRAY_MAX }));
      timerRef.current = setTimeout(() => autoLoop(gen), AUTO_IDLE_MS);
      return;
    }
    const v = videoRef.current;
    const r = v && v.readyState >= 2 ? await scanRef.current(v, { autoPass: true, gen }) : { busy: true };
    if (!autoRef.current || gen !== runRef.current) return;
    const wait = r.backoff ? AUTO_BUSY_MS : r.none || r.error ? AUTO_IDLE_MS : r.gated ? AUTO_GATE_MS : AUTO_GAP_MS;
    // Decoded-frame scheduling (R2-#6), where requestVideoFrameCallback
    // exists: after a gate pass or a failed read, look again as soon as the
    // camera has presented a NEW frame (at most the usual delay), instead of
    // sleeping a fixed time or re-sampling the same pixels. The pipeline
    // ignores a repeated frame id, so a duplicate is never settle evidence.
    // Backoff, idle (no card) and errors keep their timers. No rVFC (older
    // iOS/Android WebViews): timers exactly as before.
    const shortWait = !r.backoff && !r.none && !r.error && !r.busy;
    // Devices reporting >= 8 logical cores (the ones that already used the
    // 25 ms gate gap; some phones qualify) look again after AUTO_FRAME_MIN_MS;
    // the rest keep their cadence and only skip a repeated frame (bounded
    // extra wait).
    const since = lastCapturedFrame();
    if (shortWait && v && since != null && frameClock(v)) {
      const first = FAST_DEVICE ? Math.min(wait, AUTO_FRAME_MIN_MS) : wait;
      timerRef.current = setTimeout(() => {
        newFrameAfter(v, since, FAST_DEVICE ? Math.max(0, wait - first) : AUTO_FRAME_MAX_EXTRA_MS)
          .then(() => { if (autoRef.current && gen === runRef.current) autoLoop(gen); });
      }, first);
      return;
    }
    timerRef.current = setTimeout(() => autoLoop(gen), wait);
  }, [t]);

  const setAutoRunning = (next) => {
    setAuto(next); autoRef.current = next;
    clearTimeout(timerRef.current);
    const gen = ++runRef.current;
    if (!next) { scanAbortRef.current?.abort(); return; }
    // A new run proves every card afresh; nothing tracked in the last one carries over.
    seenIdsRef.current.clear(); failStreakRef.current = null; edgeRunRef.current = null; noTitleRunRef.current = 0; presRef.current = null; resetOnDevice();
    sessionRef.current = Math.random().toString(36).slice(2, 12);   // fresh sidecar cache too
    autoLoop(gen);
  };
  const toggleMode = () => {
    const next = mode === 'auto' ? 'single' : 'auto';
    if (auto) setAutoRunning(false);
    setMode(next);
  };
  const onShutter = () => {
    if (mode === 'auto') setAutoRunning(!auto);
    else scan(videoRef.current);
  };



  const chooseDest = async (value) => {
    if (value !== '__new') { setDest(value); localStorage.setItem('fastscan.dest', value); return; }
    const name = window.prompt(t('fastscan.newListPrompt'));
    if (!name || !name.trim()) return;
    const r = await fetch('/api/lists', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ name: name.trim() }) });
    const j = await r.json().catch(() => ({}));
    const newId = j.id ?? j.list?.id;
    if (!r.ok || newId == null) { showToast?.(t('fastscan.sendFailed')); return; }
    setLists(prev => [...prev, { id: newId, name: name.trim() }]);
    setDest(String(newId)); localStorage.setItem('fastscan.dest', String(newId));
  };

  // Per-row finish and count, set from the tray before sending.
  const patchRow = useCallback((key, fn) => setResults(prev => prev.map(x => (x.key === key ? { ...x, ...fn(x) } : x))), []);
  const onDismissRow = useCallback((key) => setResults(prev => prev.filter(r => r.key !== key)), []);

  // Send every unsent scan to the chosen destination in one request.
  const sendAll = async () => {
    // Pending rows (still hydrating, R2-#13) are never sent: they have no card row yet.
    // Rows waiting for a printing choice are never sent until the user picks.
    const rows = results.filter(r => !r.sent && !r.pending && !r.needsChoice);
    if (!rows.length || sending) return;
    setSending(true);
    try {
      // Chunks go one at a time; a failed chunk stays in the tray for retry.
      const sentRows = [], entries = [];
      let chunkError = null;
      for (const part of chunks(rows)) {
        let r;
        if (dest === 'collection') {
          r = await fetch('/api/collection/bulk-add', {
            method: 'POST', headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              card_ids: part.map(row => ({ card_id: row.card.id, quantity: qtyOf(row), printing: printingOf(row), purchase_price: resolveCardPrice(row.card, printingOf(row)) })),
              quantity: 1, condition: 'Near Mint', printing: 'Normal', language: 'English',
            }),
          });
        } else {
          r = await fetch(`/api/lists/${encodeURIComponent(dest)}/cards/bulk`, {
            method: 'POST', headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ cards: part.map(row => ({ card_id: row.card.id, quantity: qtyOf(row) })) }),
          });
        }
        const j = await r.json().catch(() => ({}));
        if (!r.ok) { chunkError = new Error(j.error || 'send failed'); break; }
        const failed = new Set((j.failed || []).map(f => (typeof f === 'object' ? f.card_id : f)));
        sentRows.push(...part.filter(row => !failed.has(row.card.id)));
        entries.push(...(j.entries || []));
      }
      if (chunkError && !sentRows.length) throw chunkError;
      const keys = new Set(sentRows.map(row => row.key));
      // Sent cards leave the tray; failures stay so they can be retried.
      setResults(prev => prev.filter(x => !keys.has(x.key)));
      const where = dest === 'collection' ? t('fastscan.destCollection') : (lists.find(l => String(l.id) === dest)?.name || t('fastscan.destList'));
      clearTimeout(undoTimer.current);
      if (sentRows.length) {
        setUndo({ rows: sentRows, dest, where, entryIds: dest === 'collection' ? entries.flatMap(e => (e.ids?.length ? e.ids : [e.id])).filter(Boolean) : null });
        undoTimer.current = setTimeout(() => setUndo(null), 15000);
      }
      showToast?.(chunkError ? t('fastscan.sendFailed') : t('fastscan.sent', { count: keys.size, where }));
      if (dest === 'collection') onAddSuccess?.();
    } catch (e) {
      showToast?.(t('fastscan.sendFailed'));
      console.error(e);
    } finally { setSending(false); }
  };

  const openPrintings = async (row) => {
    if (row.sent) return;
    setEditKey(row.key); setPrints(null);
    if (row.needsChoice && row.choose?.length) { setPrints(row.choose); return; }
    try {
      const qs = new URLSearchParams({ name: row.card.name, prints: '1', scope: 'internet', limit: '250' });
      const r = await fetch(`/api/search?${qs}`);
      const list = r.ok ? await r.json() : [];
      const same = (Array.isArray(list) ? list : []).filter(c => c.name === row.card.name);
      setPrints(same.length ? same : [row.card]);
    } catch { setPrints([row.card]); }
  };
  const choosePrinting = (card) => {
    setResults(prev => prev.map(x => (x.key === editKey ? { ...x, card, needsChoice: false, choose: undefined } : x)));
    setEditKey(null); setPrints(null);
  };
  const editing = results.find(r => r.key === editKey);
  const openPrintingsRef = useRef(openPrintings);
  openPrintingsRef.current = openPrintings;
  const onEditRow = useCallback((row) => openPrintingsRef.current(row), []);

  const undoSend = async () => {
    const u = undo;
    if (!u || sending) return;
    clearTimeout(undoTimer.current); setUndo(null); setSending(true);
    try {
      if (u.dest === 'collection') {
        let bad = false;
        for (const part of chunks(u.entryIds, 20)) {
          const res = await Promise.all(part.map(id => fetch(`/api/collection/${id}`, { method: 'DELETE' }).then(r => r.ok)));
          if (res.some(ok => !ok)) bad = true;
        }
        if (bad) throw new Error('undo partial');
        onAddSuccess?.();
      } else {
        for (const part of chunks(u.rows)) {
          const r = await fetch(`/api/lists/${encodeURIComponent(u.dest)}/cards/bulk-remove`, {
            method: 'POST', headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ cards: part.map(row => ({ card_id: row.card.id, quantity: qtyOf(row) })) }),
          });
          if (!r.ok) throw new Error('undo failed');
        }
      }
      setResults(prev => [...u.rows.map(row => ({ ...row, sent: false })), ...prev]);
      showToast?.(t('fastscan.undone', { where: u.where }));
    } catch (e) {
      console.error(e); showToast?.(t('fastscan.undoFailed'));
    } finally { setSending(false); }
  };

  const { total, anyPriced } = useMemo(() => {
    let sum = 0, any = false;
    for (const r of results) { const p = rowPriceOf(r); if (p != null) { sum += p; any = true; } }
    return { total: sum, anyPriced: any };
  }, [results]);

  if (service === false) {
    return <div className="fs-offline glass-panel">{t('fastscan.unavailable')}</div>;
  }

  const pending = results.filter(r => !r.sent && !r.pending && !r.needsChoice).length;
  // Scanner shows USD only. A printing with no USD quote (Scryfall fell back
  // to EUR) shows no price at all rather than a euro figure or a mixed total.
  const priceOf = (card, printing = 'Normal') => usdPrice(card, printing);
  const destValid = dest === 'collection' || lists.some(l => String(l.id) === dest);

  return (
    <div className="fs">
      <div className={`fs-viewfinder${cameraOn ? ' is-live' : ''}`}>
        <video ref={videoRef} playsInline muted className="fs-video" />
        <canvas ref={overlayRef} className="fs-overlay" />
        <div key={flash} className={flash ? 'fs-flash' : ''} />

        <div className="fs-topbar">
          {results.length > 0 && <span className="fs-pill fs-pill-value">{anyPriced ? `${priceText(total, 'USD')} · ` : ''}{results.length}</span>}
          {latency != null && <span className="fs-pill" title={t('fastscan.latencyHint', { read: latency.read })}><Sparkles size={12} /> {latency.total >= 1000 ? `${(latency.total / 1000).toFixed(1)} s` : `${latency.total} ms`}</span>}
          {auto && <span className="fs-pill fs-pill-live"><span className="fs-dot" /> {t('fastscan.autoOn')}</span>}
          {onDevice && <span className="fs-pill" title={t('fastscan.onDeviceHint')}>{t('fastscan.onDevice')}</span>}
          <span className="fs-spacer" />
          {torchOk && (
            <button type="button" className="fs-icon" onClick={toggleTorch} aria-label={t('fastscan.torch')}>
              {torch ? <ZapOff size={18} /> : <Zap size={18} />}
            </button>
          )}
          {devices.length > 1 && (
            <button type="button" className="fs-icon" onClick={cycleCamera} aria-label={t('fastscan.camera')}>
              <SwitchCamera size={18} />
            </button>
          )}
        </div>

        {!cameraOn && (
          <div className="fs-empty">
            <div className="fs-empty-icon"><Camera size={28} /></div>
            <div className="fs-empty-title">{t('fastscan.emptyTitle')}</div>
            <div className="fs-empty-sub">{t('fastscan.emptySub')}</div>
            <button type="button" className="fs-cta" onClick={() => startCamera()}>{t('fastscan.startCamera')}</button>
          </div>
        )}

        {cameraOn && (hint || error) && <div className={`fs-hint${error ? ' is-error' : ''}`}>{error || hint}</div>}

        <div className="fs-dock">
          <span className="fs-dock-side" aria-hidden="true" />
          <button
            type="button"
            className={`fs-shutter${busy ? ' is-busy' : ''}${mode === 'auto' ? ' is-auto-mode' : ''}${auto ? ' is-auto' : ''}`}
            disabled={!cameraOn}
            onClick={onShutter}
            aria-label={mode === 'auto' ? t(auto ? 'fastscan.autoStop' : 'fastscan.autoStart') : t('fastscan.scan')}
          ><span /></button>
          <button type="button" className={`fs-icon fs-dock-side${mode === 'auto' ? ' is-on' : ''}`} onClick={toggleMode} aria-pressed={mode === 'auto'} aria-label={t('fastscan.modeToggle')}>
            {mode === 'auto' ? <ScanLine size={20} /> : <Camera size={20} />}
            <span className="fs-dock-label">{mode === 'auto' ? t('fastscan.autoShort') : t('fastscan.single')}</span>
          </button>
        </div>
      </div>

      <div className="fs-tray">
        <div className="fs-tray-head">
          <div>
            <div className="fs-tray-title">{t('fastscan.results', { count: results.length })}</div>
            {anyPriced && <div className="fs-tray-total">{priceText(total, 'USD')}</div>}
          </div>
          {results.length > 0 && (
            <button type="button" className="fs-ghost" onClick={() => setResults([])} aria-label={t('fastscan.clear')}><Trash2 size={14} /></button>
          )}
        </div>
        {results.length === 0 ? (
          <div className="fs-tray-empty">{t('fastscan.trayEmpty')}</div>
        ) : (
          <ul className="fs-cards">
            {results.map(row => (
              <TrayCard key={row.key} row={row} dest={dest} t={t} onEdit={onEditRow} onDismiss={onDismissRow} onPatch={patchRow} onRetry={hydrateRow} />
            ))}
          </ul>
        )}
        {editing && (
          <div className="fs-sheet-backdrop" onClick={() => setEditKey(null)}>
            <div className="fs-sheet glass-panel" role="dialog" aria-modal="true" aria-label={t('fastscan.changePrinting')} onClick={e => e.stopPropagation()}>
              <div className="fs-sheet-head">
                <div>
                  <div className="fs-tray-title">{displayName(editing.card)}</div>
                  <div className="fs-card-meta">{editing.needsChoice ? choiceLabel(editing.choose) : `${t('fastscan.changePrinting')}${prints ? ` · ${prints.length}` : ''}`}</div>
                </div>
                <button type="button" className="fs-ghost" onClick={() => setEditKey(null)} aria-label={t('fastscan.dismiss')}><X size={16} /></button>
              </div>
              {prints == null ? (
                <div className="fs-tray-empty">{t('fastscan.loadingPrintings')}</div>
              ) : (
                <ul className="fs-cards fs-sheet-grid">
                  {prints.map(c => (
                    <li key={c.id} className={`fs-card${c.id === editing.card.id ? ' is-current' : ''}`}>
                      <button type="button" className="fs-print" onClick={() => choosePrinting(c)} aria-pressed={c.id === editing.card.id}>
                        <div className="fs-card-art">
                          {c.image_url ? <img src={c.image_url} alt="" loading="lazy" crossOrigin="anonymous" /> : null}
                          {priceOf(c) != null && <span className="fs-card-price">{priceText(priceOf(c), 'USD')}</span>}
                          {c.id === editing.card.id && <span className="fs-card-badge"><Check size={14} /></span>}
                        </div>
                        <div className="fs-card-name">{c.set_name || String(c.set_id || '').toUpperCase()}</div>
                        <div className="fs-card-meta">{String(c.set_id || '').toUpperCase()} · #{c.number}</div>
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </div>
        )}
        <div className="fs-send">
          <select className="fs-select" value={destValid ? dest : 'collection'} onChange={(e) => chooseDest(e.target.value)} aria-label={t('fastscan.sendTo')}>
            <option value="collection">{t('fastscan.destCollection')}</option>
            {lists.length > 0 && (
              <optgroup label={t('fastscan.destLists')}>
                {lists.map(l => <option key={l.id} value={String(l.id)}>{l.name}</option>)}
              </optgroup>
            )}
            <option value="__new">{t('fastscan.newList')}</option>
          </select>
          {undo && (
            <button type="button" className="fs-ghost fs-undo" onClick={undoSend} disabled={sending}>
              <Undo2 size={14} /> {t('fastscan.undo')}
            </button>
          )}
          <button type="button" className="fs-cta fs-cta-sm" disabled={!pending || sending} onClick={sendAll}>
            <Send size={14} /> {pending ? t('fastscan.sendCount', { count: pending }) : t('fastscan.allSent')}
          </button>
        </div>
      </div>
    </div>
  );
}
