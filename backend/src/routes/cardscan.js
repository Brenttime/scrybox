// Fast OCR card scanner (the `cardscan` sidecar, github.com/Brenttime/cardscan).
//
// The browser owns the camera and freezes one frame; the sidecar finds every
// card in it (/detect) and reads title + collector footer for the native crops
// (/scan). Identity is proven by text against Scryfall's full printing index,
// so a result is an exact printing or nothing — no nearest-neighbour guesses.
// This router only proxies and then maps Scryfall ids onto card_cache rows so
// the client can file them through the normal POST /api/collection.
//
//   GET  /api/cardscan/status   sidecar health (the UI hides itself if down)
//   POST /api/cardscan/detect   image/jpeg overview -> candidate quads
//   POST /api/cardscan/scan     { cards:[{number,image,box,quad}] } -> results
//   POST /api/cardscan/cards    { results:[{number,scryfallId,title,via}] } -> hydrated
//                               (cards the phone read on-device)
//   POST /api/cardscan/choices  { ids:[scryfallId x2-8] } -> { cards } for the
//                               printing picker (footer twins)
const express = require('express');
const axios = require('axios');
const scryfallApi = require('../scryfallApi');

const router = express.Router();
const BASE = (process.env.CARDSCAN_URL || 'http://cardscan:8321').replace(/\/$/, '');
// The sidecar serialises OCR behind one lock; a scene batch is ~0.5-2 s.
const TIMEOUT_MS = 20000;
const http = axios.create({ baseURL: BASE, timeout: TIMEOUT_MS, validateStatus: () => true, maxBodyLength: 40e6, maxContentLength: 40e6 });

function upstreamError(res, e) {
  const down = e && (e.code === 'ECONNREFUSED' || e.code === 'ENOTFOUND' || e.code === 'EAI_AGAIN');
  res.status(down ? 503 : 502).json({ ok: false, unavailable: !!down, error: down ? 'Scanner service is not running' : 'Scanner service error' });
}

router.get('/status', async (req, res) => {
  try {
    const r = await http.get('/api/health', { timeout: 2500 });
    res.status(r.status === 200 ? 200 : 503).json({ ok: r.status === 200, ...(typeof r.data === 'object' ? r.data : {}) });
  } catch (e) { upstreamError(res, e); }
});

router.post('/detect', express.raw({ type: ['image/jpeg', 'application/octet-stream'], limit: '15mb' }), async (req, res) => {
  if (!Buffer.isBuffer(req.body) || req.body.length < 100) return res.status(400).json({ ok: false, error: 'Missing image' });
  try {
    const r = await http.post('/api/detect-cards', req.body, { headers: { 'Content-Type': 'image/jpeg', 'X-Scan-Source': 'browser-camera' } });
    res.status(r.status).json(r.data);
  } catch (e) { upstreamError(res, e); }
});

// Only the fields the client renders; the sidecar result carries OCR debug too.
async function hydrate(result) {
  const out = {
    number: result.scene_number, ok: !!result.ok, error: result.ok ? undefined : result.error,
    title: result.title || result.identified?.name || null,
    via: result.footer_ocr?.resolved_by || result.identified?.via || null,
  };
  const sid = result.ok && result.card?.id;
  if (sid) {
    const card = await scryfallApi.getCardById(`mtg-${sid}`).catch(() => null);
    if (card) out.card = card;
    else { out.ok = false; out.error = 'printing not in the card database yet'; }
  }
  // The scan cascade proved the footer but several printings carry it (e.g.
  // an original and its The List / Mystery Booster reprint): the user picks.
  const choices = !result.ok && result.footer_ocr?.choices;
  if (Array.isArray(choices) && choices.length > 1) {
    const cards = await hydrateChoices(choices.map(c => c.id || c.scryfallId));
    if (cards) out.choices = cards;
  }
  return out;
}

const SCRY_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
// Every id must hydrate, or there is no choice to offer (null).
async function hydrateChoices(ids) {
  if (!Array.isArray(ids) || ids.length < 2 || ids.length > 8 || ids.some(id => !SCRY_ID.test(String(id || '')))) return null;
  const cards = await Promise.all(ids.map(id => scryfallApi.getCardById(`mtg-${id}`).catch(() => null)));
  return cards.every(Boolean) ? cards : null;
}

// One round trip: the full frame goes up, the sidecar detects, warps and reads
// from the same decoded pixels. Cards it has already identified on the table
// come back from its identity cache without OCR.
router.post('/frame', express.raw({ type: ['image/jpeg', 'application/octet-stream'], limit: '15mb' }), async (req, res) => {
  if (!Buffer.isBuffer(req.body) || req.body.length < 100) return res.status(400).json({ ok: false, error: 'Missing image' });
  try {
    const t0 = Date.now();
    // A phone that gives up (auto stopped, tab closed) cancels the sidecar read
    // too, instead of leaving the 2-core OCR box busy on a frame nobody wants.
    // res 'close' before the response is written = the client went away.
    // (req 'close' also fires when the upload body simply finishes.)
    const ctl = new AbortController();
    const cancel = () => { if (!res.writableEnded) ctl.abort(); };
    res.on('close', cancel);
    // Why the phone sent this frame (on-device state + fallback reason), passed
    // through so the sidecar's scan log shows it next to the result.
    const via = String(req.get('x-scan-client') || '').slice(0, 120).replace(/[^\w .:=,/-]/g, '');
    // Camera session id: scopes the sidecar's identity cache to this scanner.
    const session = String(req.get('x-scan-session') || '').slice(0, 40).replace(/[^\w-]/g, '');
    const r = await http.post('/api/scan-frame', req.body, {
      headers: {
        'Content-Type': 'image/jpeg',
        ...(via ? { 'X-Scan-Client': via } : {}),
        ...(session ? { 'X-Scan-Session': `${req.user?.id ?? 'u'}-${session}` } : {}),
      },
      signal: ctl.signal,
    }).finally(() => res.off('close', cancel));
    if (r.status !== 200 || !r.data?.ok) return res.status(r.status === 200 ? 422 : r.status).json(r.data);
    const results = await Promise.all((r.data.results || []).map(async (x) => ({ ...(await hydrate(x)), cached: !!x.cached })));
    res.json({
      ok: true, frame: r.data.frame,
      candidates: (r.data.candidates || []).map(c => ({ number: c.number, box: c.box, quad: c.quad, eligible: c.eligible, status: c.status })),
      results, timings: { ...r.data.timings, proxy_ms: Date.now() - t0 },
    });
  } catch (e) {
    if (e?.code === 'ERR_CANCELED' || e?.name === 'CanceledError') { if (!res.headersSent) res.status(499).end(); return; }
    upstreamError(res, e);
  }
});

// Results the phone read itself (on-device pipeline): the client proves the
// printing, this only turns Scryfall ids into card_cache rows so prices and the
// Send flow behave exactly as for server scans.
router.post('/cards', async (req, res) => {
  const items = req.body?.results;
  if (!Array.isArray(items) || items.length < 1 || items.length > 8) return res.status(400).json({ ok: false, error: 'Expected 1-8 results' });
  const ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
  if (items.some(x => !x || !ID.test(String(x.scryfallId || '')))) return res.status(400).json({ ok: false, error: 'Invalid card id' });
  const results = await Promise.all(items.map(x => hydrate({
    ok: true, scene_number: x.number, title: x.title, card: { id: x.scryfallId },
    footer_ocr: { resolved_by: x.via },
  })));
  res.json({ ok: true, results });
});

// Footer twins the phone found on-device: card rows for the printing picker.
router.post('/choices', async (req, res) => {
  const cards = await hydrateChoices(req.body?.ids);
  if (!cards) return res.status(400).json({ ok: false, error: 'Expected 2-8 card ids that exist' });
  res.json({ ok: true, cards });
});

// Client scan telemetry (frontend/src/utils/scanTelemetry.js): per-pass
// timings and OCR text, no images. Appended as JSON lines to
// <SCAN_TELEMETRY_DIR>/YYYY-MM-DD.jsonl, capped per day so a runaway client
// cannot fill the disk. Off with SCAN_TELEMETRY=0.
const fs = require('fs');
const path = require('path');
const TELEMETRY_DIR = process.env.SCAN_TELEMETRY_DIR || path.join(__dirname, '..', '..', '..', 'database', 'scan-telemetry');
const TELEMETRY_DAY_MAX = 50 * 1024 * 1024;
router.post('/telemetry', express.json({ limit: '512kb', type: ['application/json', 'text/plain'] }), (req, res) => {
  if (process.env.SCAN_TELEMETRY === '0') return res.status(204).end();
  const b = req.body || {};
  const records = Array.isArray(b.records) ? b.records.slice(0, 200) : [];
  if (!records.length) return res.status(400).json({ ok: false, error: 'No records' });
  const day = new Date().toISOString().slice(0, 10);
  const file = path.join(TELEMETRY_DIR, `${day}.jsonl`);
  const meta = { user: req.user?.id ?? null, session: String(b.session || '').slice(0, 20), platform: b.platform, device: b.device, ua: String(req.get('user-agent') || '').slice(0, 200) };
  const lines = records.map(r => JSON.stringify({ ...meta, ...r, recv: new Date().toISOString() })).join('\n') + '\n';
  fs.promises.mkdir(TELEMETRY_DIR, { recursive: true })
    .then(() => fs.promises.stat(file).then(s => s.size).catch(() => 0))
    .then(size => (size + lines.length > TELEMETRY_DAY_MAX ? null : fs.promises.appendFile(file, lines)))
    .then(() => res.status(204).end())
    .catch(() => res.status(500).json({ ok: false }));
});

router.post('/scan', async (req, res) => {
  const cards = req.body?.cards;
  if (!Array.isArray(cards) || cards.length < 1 || cards.length > 8) return res.status(400).json({ ok: false, error: 'Expected 1-8 cards' });
  try {
    const r = await http.post('/api/scan-scene', { cards }, { headers: { 'X-Scene-Protocol': '3' } });
    if (r.status !== 200 || !r.data?.ok) return res.status(r.status === 200 ? 422 : r.status).json(r.data);
    const results = await Promise.all((r.data.results || []).map(hydrate));
    res.json({ ok: true, results, timings: r.data.timings });
    http.post('/api/scene-complete', {}).catch(() => {});
  } catch (e) { upstreamError(res, e); }
});

module.exports = router;
