// Scan telemetry: one small JSON record per scan pass, batched to
// POST /api/cardscan/telemetry (appended server-side to a JSONL file).
//
// Why: the sidecar only sees frames the client gave up on, and saved frames
// replayed under Node do not reproduce browser timings. This records, per
// pass, where the time went on the real device (canvas draw, corner probe,
// full-frame readback, worker read, JPEG encode, server round trip, hydration)
// plus what the on-device reader saw (gate status, sharpness, raw title OCR,
// footer reads, outcome), so a slow or failing session can be diagnosed
// after the fact. No images are sent; text and numbers only.
//
// Off with localStorage 'scan.telemetry' = '0'.

const FLUSH_MS = 4000;
const FLUSH_AT = 25;
const MAX_QUEUE = 200;

let queue = [];
let timer = null;
let deviceInfo = null;
let seq = 0;
const session = Math.random().toString(36).slice(2, 10);

function enabled() {
  try { return localStorage.getItem('scan.telemetry') !== '0'; } catch { return true; }
}

function platform() {
  const ua = typeof navigator !== 'undefined' ? navigator.userAgent : '';
  const os = /iPhone|iPad|iPod/.test(ua) ? 'ios' : /Android/.test(ua) ? 'android' : /Windows/.test(ua) ? 'windows' : /Mac OS/.test(ua) ? 'mac' : /Linux/.test(ua) ? 'linux' : 'other';
  const br = /Edg\//.test(ua) ? 'edge' : /CriOS|Chrome\//.test(ua) ? 'chrome' : /FxiOS|Firefox\//.test(ua) ? 'firefox' : /Safari\//.test(ua) ? 'safari' : 'other';
  return { os, browser: br, cores: navigator?.hardwareConcurrency || null, mem: navigator?.deviceMemory || null };
}

function flush(useBeacon = false) {
  clearTimeout(timer); timer = null;
  if (!queue.length) return;
  const batch = queue; queue = [];
  const body = JSON.stringify({ session, platform: platform(), device: deviceInfo, records: batch });
  try {
    if (useBeacon && navigator.sendBeacon) {
      navigator.sendBeacon('/api/cardscan/telemetry', new Blob([body], { type: 'application/json' }));
      return;
    }
    fetch('/api/cardscan/telemetry', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body, keepalive: true }).catch(() => {});
  } catch { /* telemetry never breaks scanning */ }
}

function push(rec) {
  if (!enabled()) return;
  queue.push(rec);
  if (queue.length > MAX_QUEUE) queue = queue.slice(-MAX_QUEUE);
  if (queue.length >= FLUSH_AT) flush();
  else if (!timer) timer = setTimeout(flush, FLUSH_MS);
}

if (typeof window !== 'undefined') {
  window.addEventListener('pagehide', () => flush(true));
  document.addEventListener?.('visibilitychange', () => { if (document.visibilityState === 'hidden') flush(true); });
}

const round = (v) => (typeof v === 'number' ? Math.round(v) : v);
const clip = (arr, n = 12, len = 60) => (Array.isArray(arr) ? arr.slice(0, n).map(s => (typeof s === 'string' ? s.slice(0, len) : s)) : undefined);

// A pass that has no card at all and never left the device is common (empty
// desk between cards) and uninformative; keep one in every 10.
let emptyCount = 0;

export const scanTelemetry = {
  device(info) { deviceInfo = info; },
  begin({ mode, sw, sh }) {
    const rec = { id: `${session}-${++seq}`, t: new Date().toISOString(), mode, video: [sw, sh], ms: {} };
    let done = false;
    return {
      id: rec.id,
      mark(k, v) { rec.ms[k] = round(v); },
      set(obj) { Object.assign(rec, obj); },
      get(k) { return rec[k]; },
      local(out) {
        if (!out) return;
        if (out.error) { rec.local_error = String(out.error).slice(0, 120); }
        if (out.span) Object.assign(rec.ms, out.span);
        const c = out.candidates?.[0];
        const r = out.results?.[0];
        rec.local = {
          frame: out.frame ? [out.frame.width, out.frame.height] : undefined,
          status: c ? c.status : 'no card',
          sharp: c?.sharpness,
          still: c?.still,
          near_edge: c?.near_edge || undefined,
          partial: r?.near_edge_partial || undefined,
          quad: c?.near_edge ? c.quad?.map(p => p.map(Math.round)) : undefined,
          deferred: r?.deferred || undefined,
          box: c?.box,
          timings: out.timings,
          ok: r ? !!r.ok : undefined,
          error: r?.error,
          title: r?.title || undefined,
          title_score: r?.title_score != null ? Math.round(r.title_score * 1000) / 1000 : undefined,
          title_raw: r?.title_raw,
          via: r?.via,
          stage: r?.footer_stage,
          footer: clip(r?.footer_ocr),
          printing: r?.ok ? `${r.set} ${r.num}` : undefined,
          cached: r?.cached || undefined,
        };
      },
      server(j) {
        if (!j) return;
        rec.server = {
          ok: !!j.ok, error: j.ok ? undefined : String(j.error || '').slice(0, 120),
          cards: j.candidates?.length,
          statuses: j.candidates?.map(c => c.status),
          results: j.results?.map(r => (r.ok && r.card ? `${r.card.name}[${r.card.set_id} ${r.card.number}]` : `x:${r.error || '?'}${r.title ? `(${r.title})` : ''}`)),
          timings: j.timings,
        };
      },
      end(extra) {
        if (done) return; done = true;
        Object.assign(rec, extra);
        const trivial = rec.outcome === 'no-card' && !rec.server && !rec.local_error;
        if (trivial && (emptyCount++ % 10)) return;
        push(rec);
      },
    };
  },
  flush,
};
