import { useCallback, useEffect, useRef, useState } from 'react';
import { Zap, ZapOff, ScanLine, Check, X, SwitchCamera, Camera, Sparkles, Trash2, Send, Undo2, Minus, Plus } from 'lucide-react';
import { resolveCardPrice, usdPrice } from '../utils/resolveCardPrice';
import { priceText } from '../utils/formatPrice';
import { displayName } from '../utils/languages';
import { useT } from '../utils/i18n';
import { FRAME_MAX, fitContain, quadPath, zoomPlan, nextFailStreak, serverAllowed } from '../utils/fastScan';
import { loadClientScan, readOnDevice, resetOnDevice, lastFrameJpeg, hydrateResults, needsServer, takeHydrateMs } from '../utils/clientScan';
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
const AUTO_BUSY_MS = 1000;   // sidecar said 429: back off instead of re-asking in 60 ms
// The tray is capped for render cost, but unsent scans are never dropped to
// make room: at the cap, auto pauses and asks for a Send instead.
const TRAY_MAX = 80;

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
  const seenIdsRef = useRef(new Map()); // card.id -> last seen ms (auto de-dupe)
  const onDeviceRef = useRef(false);
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
      clearTimeout(timerRef.current); scanAbortRef.current?.abort(); stopCamera();
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

  const scan = useCallback(async (source, { autoPass = false, gen = runRef.current } = {}) => {
    if (busyRef.current) return { busy: true };
    // Results commit only for the run that asked: an auto pass that outlives a
    // stop (or a stop+restart) must not add rows to the new run's tray.
    const stale = () => !aliveRef.current || (autoPass && (!autoRef.current || gen !== runRef.current));
    const sw = source.videoWidth || source.naturalWidth || source.width;
    const sh = source.videoHeight || source.naturalHeight || source.height;
    if (!sw || !sh) return { busy: true };
    busyRef.current = true; setBusy(true);
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
        tel.mark('server_rtt_ms', performance.now() - tr);
        tel.server(j);
        if (!r.ok || !j.ok) throw new Error(j.error || t('fastscan.serviceError'));
        return j;
      };
      const hedged = onDeviceRef.current && !autoPass ? serverRead(grabJpeg(source, sw, sh, canvasRef)) : null;
      hedged?.catch(() => {});
      let local = null;
      if (onDeviceRef.current) {
        const tl = performance.now();
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
        if (!needsServer(local, { autoPass, noTitleRun: noTitleRunRef.current })) {
          out = { ...local, results: await hydrateResults(local.results, abort.signal).catch((e) => { tel.set({ hydrate_error: e?.message || String(e) }); return null; }) };
          tel.mark('hydrate_ms', takeHydrateMs());
          if (!out.results) { out = null; why = 'hydrate-failed'; }
          else tel.set({ answered_by: 'client' });
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
      } else { noTitleRunRef.current = 0; out = await serverRead((async () => (onDeviceRef.current && await Promise.resolve(lastFrameJpeg()).catch(() => null)) || grabJpeg(source, sw, sh, canvasRef))()); }
      // Stopped or navigated away while this was in flight: drop it on the floor.
      if (stale()) { tel.end({ outcome: 'stale' }); return { busy: true }; }
      if (out.busy) { tel.end({ outcome: 'busy' }); return out; }
      if (!tel.get('answered_by')) tel.set({ answered_by: 'server' });
      const streak = nextFailStreak(failStreakRef.current, out, Date.now());
      failStreakRef.current = streak;
      const plan = zoomPlan({ candidates: out.candidates, results: out.results, frame: out.frame, sw: out.frame?.width || sw, sh: out.frame?.height || sh });
      const ms = Math.round(performance.now() - t0);
      const byNumber = new Map(out.results.map(x => [x.number ?? x.scene_number, x]));
      drawOverlay(out.frame, out.candidates, byNumber);
      const hits = out.results.filter(x => x.ok && x.card);
      const eligible = out.candidates.filter(c => c.eligible).length;
      if (!out.candidates.length) firstSeenRef.current = null;
      else if (firstSeenRef.current == null) firstSeenRef.current = t0;
      const waited = autoPass && firstSeenRef.current != null ? Math.round(performance.now() - firstSeenRef.current) : ms;
      if (!out.candidates.length) setHint(t('fastscan.hintNoCard'));
      else if (!eligible) setHint(t('fastscan.hintAdjust', { reason: out.candidates[0].status }));
      else if (plan.tooSmall.length && hits.length < eligible) setHint(t('fastscan.hintCloser', { count: plan.tooSmall.length }));
      else if (!hits.length && streak && streak.count >= 2) setHint(t('fastscan.hintFooter'));
      else if (!hits.length) setHint(t('fastscan.hintHold'));
      else setHint('');

      // Auto: a card that stays on the table must not be re-added every pass.
      const now = Date.now();
      const fresh = hits.filter(h => {
        if (!autoPass) return true;
        const last = seenIdsRef.current.get(h.card.id);
        return !last || now - last > 4000;
      });
      for (const h of hits) seenIdsRef.current.set(h.card.id, now);
      if (hits.length) firstSeenRef.current = null;
      tel.end({
        outcome: hits.length ? (fresh.length ? 'added' : 'repeat') : (out.candidates.length ? 'miss' : 'no-card'),
        hits: hits.map(h => `${h.card.name}[${h.card.set_id} ${h.card.number}]`).slice(0, 8),
        total_ms: ms, waited_ms: waited,
      });
      if (fresh.length) {
        setLatency({ total: waited, read: ms });
        setFlash(f => f + 1);
        navigator.vibrate?.(18);
        const rows = fresh.map(h => ({ key: `${h.card.id}-${now}-${Math.random().toString(36).slice(2, 7)}`, card: h.card, added: false }));
        setResults(prev => [...rows, ...prev]);
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
  }, [drawOverlay, t]);

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
    timerRef.current = setTimeout(() => autoLoop(gen), wait);
  }, [t]);

  const setAutoRunning = (next) => {
    setAuto(next); autoRef.current = next;
    clearTimeout(timerRef.current);
    const gen = ++runRef.current;
    if (!next) { scanAbortRef.current?.abort(); return; }
    // A new run proves every card afresh; nothing tracked in the last one carries over.
    seenIdsRef.current.clear(); failStreakRef.current = null; resetOnDevice();
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
  const qtyOf = (row) => Math.max(1, row.qty || 1);
  const printingOf = (row) => (row.foil ? 'Holofoil' : 'Normal');
  const patchRow = (key, fn) => setResults(prev => prev.map(x => (x.key === key ? { ...x, ...fn(x) } : x)));
  const canFoil = (card) => card?.price_holofoil > 0 || (Array.isArray(card?.finishes) && card.finishes.includes('foil'));

  // Send every unsent scan to the chosen destination in one request.
  const sendAll = async () => {
    const rows = results.filter(r => !r.sent);
    if (!rows.length || sending) return;
    setSending(true);
    try {
      let r;
      if (dest === 'collection') {
        r = await fetch('/api/collection/bulk-add', {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            card_ids: rows.map(row => ({ card_id: row.card.id, quantity: qtyOf(row), printing: printingOf(row), purchase_price: resolveCardPrice(row.card, printingOf(row)) })),
            quantity: 1, condition: 'Near Mint', printing: 'Normal', language: 'English',
          }),
        });
      } else {
        r = await fetch(`/api/lists/${encodeURIComponent(dest)}/cards/bulk`, {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ cards: rows.map(row => ({ card_id: row.card.id, quantity: qtyOf(row) })) }),
        });
      }
      const j = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(j.error || 'send failed');
      const failed = new Set((j.failed || []).map(f => (typeof f === 'object' ? f.card_id : f)));
      const sentRows = rows.filter(row => !failed.has(row.card.id));
      const keys = new Set(sentRows.map(row => row.key));
      // Sent cards leave the tray; failures stay so they can be retried.
      setResults(prev => prev.filter(x => !keys.has(x.key)));
      const where = dest === 'collection' ? t('fastscan.destCollection') : (lists.find(l => String(l.id) === dest)?.name || t('fastscan.destList'));
      clearTimeout(undoTimer.current);
      if (sentRows.length) {
        setUndo({ rows: sentRows, dest, where, entryIds: dest === 'collection' ? (j.entries || []).flatMap(e => (e.ids?.length ? e.ids : [e.id])).filter(Boolean) : null });
        undoTimer.current = setTimeout(() => setUndo(null), 15000);
      }
      showToast?.(t('fastscan.sent', { count: keys.size, where }));
      if (dest === 'collection') onAddSuccess?.();
    } catch (e) {
      showToast?.(t('fastscan.sendFailed'));
      console.error(e);
    } finally { setSending(false); }
  };

  const openPrintings = async (row) => {
    if (row.sent) return;
    setEditKey(row.key); setPrints(null);
    try {
      const qs = new URLSearchParams({ name: row.card.name, prints: '1', scope: 'internet', limit: '250' });
      const r = await fetch(`/api/search?${qs}`);
      const list = r.ok ? await r.json() : [];
      const same = (Array.isArray(list) ? list : []).filter(c => c.name === row.card.name);
      setPrints(same.length ? same : [row.card]);
    } catch { setPrints([row.card]); }
  };
  const choosePrinting = (card) => {
    setResults(prev => prev.map(x => (x.key === editKey ? { ...x, card } : x)));
    setEditKey(null); setPrints(null);
  };
  const editing = results.find(r => r.key === editKey);

  const undoSend = async () => {
    const u = undo;
    if (!u || sending) return;
    clearTimeout(undoTimer.current); setUndo(null); setSending(true);
    try {
      if (u.dest === 'collection') {
        const res = await Promise.all(u.entryIds.map(id => fetch(`/api/collection/${id}`, { method: 'DELETE' }).then(r => r.ok)));
        if (res.some(ok => !ok)) throw new Error('undo partial');
        onAddSuccess?.();
      } else {
        const r = await fetch(`/api/lists/${encodeURIComponent(u.dest)}/cards/bulk-remove`, {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ cards: u.rows.map(row => ({ card_id: row.card.id, quantity: qtyOf(row) })) }),
        });
        if (!r.ok) throw new Error('undo failed');
      }
      setResults(prev => [...u.rows.map(row => ({ ...row, sent: false })), ...prev]);
      showToast?.(t('fastscan.undone', { where: u.where }));
    } catch (e) {
      console.error(e); showToast?.(t('fastscan.undoFailed'));
    } finally { setSending(false); }
  };

  if (service === false) {
    return <div className="fs-offline glass-panel">{t('fastscan.unavailable')}</div>;
  }

  const pending = results.filter(r => !r.sent).length;
  // Scanner shows USD only. A printing with no USD quote (Scryfall fell back
  // to EUR) shows no price at all rather than a euro figure or a mixed total.
  const priceOf = (card, printing = 'Normal') => usdPrice(card, printing);
  const rowPrice = (row) => { const p = priceOf(row.card, printingOf(row)); return p == null ? null : p * qtyOf(row); };
  const total = results.reduce((sum, r) => sum + (rowPrice(r) || 0), 0);
  const anyPriced = results.some(r => rowPrice(r) != null);
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
              <li key={row.key} className={`fs-card${row.sent ? ' is-added' : ''}`}>
                <div className="fs-card-art">
                  {row.card.image_url ? <img src={row.card.image_url} alt="" loading="lazy" /> : null}
                  {!row.sent && (
                    <button type="button" className="fs-card-edit" onClick={() => openPrintings(row)} aria-label={t('fastscan.changePrinting')} />
                  )}
                  <button type="button" className="fs-card-x" onClick={() => setResults(prev => prev.filter(r => r.key !== row.key))} aria-label={t('fastscan.dismiss')}><X size={12} /></button>
                  {rowPrice(row) != null && <span className="fs-card-price">{priceText(rowPrice(row), 'USD')}</span>}
                  {row.sent && <span className="fs-card-badge"><Check size={14} /></span>}
                </div>
                <div className="fs-card-name">{displayName(row.card)}</div>
                <div className="fs-card-meta">{String(row.card.set_id || '').toUpperCase()} · #{row.card.number}</div>
                {!row.sent && (
                  <div className="fs-card-quick">
                    <div className="fs-qty" role="group" aria-label={t('fastscan.quantity')}>
                      <button type="button" onClick={() => patchRow(row.key, x => ({ qty: Math.max(1, qtyOf(x) - 1) }))} disabled={qtyOf(row) <= 1} aria-label={t('fastscan.qtyLess')}><Minus size={12} /></button>
                      <span aria-live="polite">{qtyOf(row)}</span>
                      <button type="button" onClick={() => patchRow(row.key, x => ({ qty: Math.min(99, qtyOf(x) + 1) }))} aria-label={t('fastscan.qtyMore')}><Plus size={12} /></button>
                    </div>
                    {dest === 'collection' && (
                      <button type="button" className={`fs-foil${row.foil ? ' is-on' : ''}`} aria-pressed={!!row.foil}
                        disabled={!row.foil && !canFoil(row.card)}
                        onClick={() => patchRow(row.key, x => ({ foil: !x.foil }))}>
                        <Sparkles size={11} />{t('fastscan.foil')}
                      </button>
                    )}
                  </div>
                )}
              </li>
            ))}
          </ul>
        )}
        {editing && (
          <div className="fs-sheet-backdrop" onClick={() => setEditKey(null)}>
            <div className="fs-sheet glass-panel" role="dialog" aria-modal="true" aria-label={t('fastscan.changePrinting')} onClick={e => e.stopPropagation()}>
              <div className="fs-sheet-head">
                <div>
                  <div className="fs-tray-title">{displayName(editing.card)}</div>
                  <div className="fs-card-meta">{t('fastscan.changePrinting')}{prints ? ` · ${prints.length}` : ''}</div>
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
                          {c.image_url ? <img src={c.image_url} alt="" loading="lazy" /> : null}
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
