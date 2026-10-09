// Pure helpers for FastScanner (unit-tested in fastScan.test.js).

// Frame upload ceiling. Measured on the cardscan fixtures: 1920px matches as
// well as 2560px (the sidecar warps each card from the full frame it
// receives) with smaller uploads and faster detection.
export const FRAME_MAX = 1920;

// Map frame coordinates onto an element showing it with object-fit.
export function fitContain(frame, W, H, mode = 'contain') {
  const s = mode === 'cover'
    ? Math.max(W / frame.width, H / frame.height)
    : Math.min(W / frame.width, H / frame.height);
  return { s, ox: (W - frame.width * s) / 2, oy: (H - frame.height * s) / 2 };
}

// Candidate outline in element coordinates (quad when known, else box).
export function quadPath(cand, { s, ox, oy }) {
  const pts = cand.quad?.length === 4
    ? cand.quad
    : (() => { const [x, y, w, h] = cand.box; return [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]; })();
  return pts.map(([x, y]) => [ox + x * s, oy + y * s]);
}

// Pure: should this on-device outcome go to the server instead? Unit-tested.
//   - pipeline error / worker failure: yes.
//   - a still, in-frame card the client read but could not prove: yes, the
//     server's wider sweeps and multi-card detection may.
//   - shutter press with no usable card: yes (maybe several cards, which the
//     single-card detector does not handle).
//   - auto pass with no card, or a moving/clipped/blurred one: no — that is
//     the stillness gate doing its job; the next pass retries.
//   - auto pass whose title OCR was unreadable: no. Windows telemetry: 12 of
//     12 such fallbacks failed on the server too (same smeared pixels), each
//     costing ~0.35 s before the next frame could be read. After
//     NO_TITLE_ESCAPE of them in a row the server gets one try anyway, for
//     what the on-device reader cannot handle (multi-card spreads, layouts
//     its strips miss).
//   - auto pass whose footer was deferred to the next frame: no, the next
//     still frame continues the read on-device with the pooled evidence.
//   - auto pass that stopped because an OCR stage would read past the frame
//     edge (near_edge_partial): no. Telemetry: Roiling Canopy / Heartstring
//     Puller sent 7-10 of these, the server read nothing on them (same
//     clipped pixels), ~0.4 s each. The user is told to move the card in
//     instead; after EDGE_ESCAPE such passes at about the same place the
//     server still gets one try (bounded rescue), and a moved card or a
//     different outcome starts over (nextEdgeRun).
export const NO_TITLE_ESCAPE = 4;
export const EDGE_ESCAPE = 8;
// Auto pass on a reprint-sheet / Mystery Booster title (res.sheet_like) whose
// printing the phone could not prove yet: no, for SHEET_ESCAPE passes in a
// row. The proof for these cards is the List stamp or the copyright-line
// number, both read on-device; 2026-10-08 telemetry: every server fallback
// on them (Hatchet Bully, Smokestack, Chain of Smog, Hullbreacher) failed and
// cost ~2.2 s, while the next local frame proved the card. The server still
// gets one try after SHEET_ESCAPE misses.
export const SHEET_ESCAPE = 4;
export function isEdgePartial(out) {
  const res = out?.results?.[0];
  return !!(res && !res.ok && res.near_edge_partial);
}
export function needsServer(out, { autoPass, noTitleRun = 0, edgeRun = 0, sheetRun = 0 }) {
  if (!out || out.error) return true;
  const cand = out.candidates?.[0];
  const res = out.results?.[0];
  // A proven footer shared by several printings: the cascade is finished and
  // the user picks. The server cannot split it either.
  if (res && !res.ok && res.choices?.length > 1) return false;
  if (res && !res.ok && autoPass) {
    if (res.deferred) return false;
    if (res.error === 'no confident card title' && noTitleRun < NO_TITLE_ESCAPE) return false;
    if (res.near_edge_partial && edgeRun < EDGE_ESCAPE) return false;
    if (res.sheet_like && res.title && sheetRun < SHEET_ESCAPE) return false;
  }
  if (res) return !res.ok;
  if (!cand || !cand.eligible) return !autoPass;
  return true;
}

// Repeat-failure budget for auto mode. A card whose title reads but whose
// printing never resolves (worn/tiny footer, glare) used to be re-sent every
// pass — ~870 ms of sidecar OCR each, forever, while the user waited. Now the
// same unresolved card (same title, about the same place) backs off
// 1.5 s, 3 s, 6 s, capped at 8 s; any different outcome resets it. Never a
// guess: it only spaces retries, it does not change what counts as proven.
// A delay, never a block: a same-title reprint swapped in at the same spot
// looks identical until its footer is read, so the cap stays short (3 s), and
// lifting the card (a no-card frame) ends the streak at once.
export const FAIL_BACKOFF_MS = [0, 1000, 2000, 3000];
const SAME_PLACE = 0.08;   // centre move, fraction of frame diagonal
const MOTION_BREAK = 3;    // moving/blurred passes in a row that end a presentation

function unresolvedSignature(out) {
  const res = (out?.results || []).find(r => !r.ok && r.title);
  if (!res || (out.results || []).some(r => r.ok)) return null;
  const n = res.number ?? res.scene_number;
  const cand = (out.candidates || []).find(c => c.number === n) || out.candidates?.[0];
  const box = cand?.box || res.box;
  if (!box) return { title: res.title, cx: 0, cy: 0 };
  const diag = Math.hypot(out.frame?.width || 1, out.frame?.height || 1);
  return { title: res.title, cx: (box[0] + box[2] / 2) / diag, cy: (box[1] + box[3] / 2) / diag };
}

// Pure: fold one pass's outcome into the failure streak. Only a real server
// attempt (fromServer) can start or extend it; an on-device outcome never
// renews the deadline (review R1-B2: local deferred reads kept pushing it
// out, so the server was never asked again). A hit or a lifted card (no
// candidate) ends it; any other pass (settling, moving, blurred, a local
// miss, near-edge with no title) keeps it unchanged, so the next still frame
// of the same card does not re-send at once (Konstrari Charm: a settling
// pass between two failed fallbacks cleared the backoff), while the fixed
// deadline still expires on time. serverAllowed only ever applies it to the
// same title in the same place.
// sameFrameLocal (R2-#10): the on-device read of the very frame that was
// just sent. The sidecar often fails WITHOUT a title ("no confident card
// title", Helm of the Host) while the client read one; that failed server
// attempt is then signed with the client's title and position, so the next
// local failure of the same card backs off instead of re-sending at once.
// Only for a single-card server answer, and only on a real server attempt.
export function nextFailStreak(prev, out, now, { fromServer = true, sameFrameLocal = null } = {}) {
  const resolved = (out?.results || []).some(r => r.ok);
  if (resolved || !out?.candidates?.length) return null;
  let sig = fromServer ? unresolvedSignature(out) : null;
  if (!sig && fromServer && sameFrameLocal && !sameFrameLocal.error && out.candidates.length === 1
    && !(out.results || []).some(r => r.title)) sig = unresolvedSignature(sameFrameLocal);
  if (!sig) return prev || null;
  const same = prev && normTitle(prev.title) === normTitle(sig.title)
    && Math.hypot(prev.cx - sig.cx, prev.cy - sig.cy) <= SAME_PLACE;
  const count = same ? prev.count + 1 : 1;
  return { ...sig, count, until: now + FAIL_BACKOFF_MS[Math.min(count, FAIL_BACKOFF_MS.length - 1)] };
}

// Pure: may an auto pass send to the server now? Backoff applies only when
// the CURRENT observation shows the same card: the on-device read of this very
// frame found a card with the same title, at the same place. With no local
// evidence (no on-device reader, no title, a different card, moved) the server
// is always allowed — backoff must never hold back a new card.
export function serverAllowed(streak, now, local = null) {
  if (!streak || now >= streak.until) return true;
  if (local && !local.error && !local.candidates?.length) return true;
  const res = (local?.results || []).find(r => r.title);
  const cand = local?.candidates?.[0];
  if (!res || !cand?.box || !local.frame) return true;
  if (normTitle(res.title) !== normTitle(streak.title)) return true;
  const diag = Math.hypot(local.frame.width || 1, local.frame.height || 1);
  const [x, y, w, h] = cand.box;
  const cx = (x + w / 2) / diag, cy = (y + h / 2) / diag;
  return Math.hypot(cx - streak.cx, cy - streak.cy) > SAME_PLACE;
}
// Pure: consecutive near-edge partials of about the same outline. Anything
// else (a read, a hit, no card, a moved card) resets the run, so a card that
// is pulled in from the edge is read on its very next frame.
export function nextEdgeRun(prev, out) {
  if (!isEdgePartial(out)) return null;
  const box = out.candidates?.[0]?.box;
  const diag = Math.hypot(out.frame?.width || 1, out.frame?.height || 1);
  const cx = box ? (box[0] + box[2] / 2) / diag : 0, cy = box ? (box[1] + box[3] / 2) / diag : 0;
  const same = prev && Math.hypot(prev.cx - cx, prev.cy - cy) <= SAME_PLACE;
  return { cx, cy, count: same ? prev.count + 1 : 1 };
}
// --- presentations (R2 measurement fix) -----------------------------------
// One presentation = one physical card continuously in view. waited_ms used to
// run from the first candidate after a hit or no-card frame, so a card that
// replaced an unresolved one inherited its wait (Galactus 8.4 s included
// 3.5 s of failed Helm of the Host reads). A new presentation starts on a
// no-card pass, a different confident title, or a card at a different place.
// Pure; returns the same object when the presentation continues.
function centreOf(out) {
  const box = out?.candidates?.[0]?.box;
  if (!box) return null;
  const diag = Math.hypot(out.frame?.width || 1, out.frame?.height || 1);
  return { cx: (box[0] + box[2] / 2) / diag, cy: (box[1] + box[3] / 2) / diag };
}
function titleOf(out) {
  const r = (out?.results || []).find(x => x.title || x.card?.name);
  return r ? normTitle(r.title || r.card?.name) : null;
}
// Pure (review R4-S1): which pass output describes the presentation.
// Geometry comes from the on-device read when it saw a card; the title from
// that read if it has one, else from the final answer (server fallback).
export function presentationInput(local, localTitled, out) {
  if (!local) return out;
  if (localTitled) return local;
  const r = (out?.results || []).find(x => x.title || x.card?.name);
  if (!r) return local;
  return { ...local, results: [{ ...(local.results?.[0] || {}), number: 1, title: r.title || r.card?.name }] };
}

// epochBase: the last epoch handed out in this run, so epochs stay monotonic
// across lifts (a no-card pass returns null, the next card gets base + 1).
// Sustained hand motion (MOTION_BREAK moving/blurred passes) also starts a new
// presentation, the same rule the de-dupe uses. `at` is when this pass's
// capture began (first candidate); `titleAt` when its confident title was
// read: the end of the on-device title stage, or the completion of the
// server response that carried the title.
export function nextPresentation(prev, out, at, epochBase = prev?.epoch || 0, titleAt = at) {
  if (!out || out.error) return prev;
  if (!out.candidates?.length) return null;
  const c = centreOf(out), title = titleOf(out);
  const status = out.candidates[0].status;
  const motion = status === 'moving' || status === 'too blurry' ? (prev?.motion || 0) + 1 : 0;
  const moved = prev && c && prev.cx != null && Math.hypot(prev.cx - c.cx, prev.cy - c.cy) > SAME_PLACE;
  const retitled = prev && title && prev.title && title !== prev.title;
  const shaken = prev && prev.motion >= MOTION_BREAK && !motion;
  if (!prev || moved || retitled || shaken) {
    return { epoch: Math.max(epochBase, prev?.epoch || 0) + 1, since: at, cx: c?.cx ?? null, cy: c?.cy ?? null, title, titleAt: title ? titleAt : null, motion };
  }
  const next = { ...prev, motion };
  if (title && !prev.title) { next.title = title; next.titleAt = titleAt; }
  if (c && prev.cx == null) { next.cx = c.cx; next.cy = c.cy; }
  return next;
}

// Auto de-dupe (R2 measurement fix). A card is re-added only if, since its
// last COMMITTED add, its presentation was broken (lifted: a no-card pass;
// moved elsewhere; another card read at its place; or sustained hand motion,
// MOTION_BREAK moving/blurred passes in a row) and DEDUPE_MS have passed.
// The Masamune was added, repeated, then added again after 4 s of failed
// reads at the same box: no evidence of a second physical copy. A suppressed
// repeat never clears a break or renews the clock (review R1-B1), so a second
// copy put down after a lift is added once DEDUPE_MS after the first add.
export const DEDUPE_MS = 4000;
export function dedupeFresh(seen, id, at) {
  const e = seen.get(id);
  if (!e) return true;
  return at - e.at > DEDUPE_MS && !!e.broken;
}
// Fold one pass into the remembered cards. committed = ids added this pass.
export function notePresence(seen, out, hits, at, committed = new Set(hits.map(h => h.card.id))) {
  const noCard = !out?.error && !out?.candidates?.length;
  const status = out?.candidates?.[0]?.status;
  const motion = status === 'moving' || status === 'too blurry';
  const c = centreOf(out), title = titleOf(out);
  const hitIds = new Set(hits.map(h => h.card.id));
  for (const [id, e] of seen) {
    if (committed.has(id)) continue;
    if (hitIds.has(id)) { e.motion = 0; continue; }     // suppressed repeat: seen, nothing else changes
    e.motion = motion ? (e.motion || 0) + 1 : 0;
    if (noCard || e.motion >= MOTION_BREAK
      || (c && e.cx != null && Math.hypot(e.cx - c.cx, e.cy - c.cy) > SAME_PLACE)
      || (title && e.title && title !== e.title)) e.broken = true;
  }
  for (const h of hits) {
    if (!committed.has(h.card.id) && seen.has(h.card.id)) continue;
    seen.set(h.card.id, { at, cx: c?.cx ?? null, cy: c?.cy ?? null, title: normTitle(h.card.name || h.title), broken: false, motion: 0 });
  }
}

// Directional edge hint (R2-#9): which way to move the card, from the frame
// sides its outline touches. Guidance only.
export function edgeDirection(cand) {
  const s = cand?.edge_sides;
  if (!s?.length) return null;
  const v = s.includes('top') && !s.includes('bottom') ? 'down' : s.includes('bottom') && !s.includes('top') ? 'up' : null;
  const h = s.includes('left') && !s.includes('right') ? 'right' : s.includes('right') && !s.includes('left') ? 'left' : null;
  if (v && h) return `${v}-${h}`;
  if (v || h) return v || h;
  return 'back';   // touches opposite sides: too close to the camera
}

// FNV-1a over every 5th byte (RGB and alpha interleave, so all channels are
// sampled across rows): ~118k steps on the 384x384 copy (host Node p50
// 0.19 ms; not measured on phones). A collision only skips evidence.
export function pixelPrint(buf) {
  const a = new Uint8Array(buf);
  let h = 0x811c9dc5;
  for (let i = 0; i < a.length; i += 5) { h ^= a[i]; h = Math.imul(h, 0x01000193); }
  return (h >>> 0).toString(36);
}

function normTitle(s) { return String(s || '').toLowerCase().replace(/[^a-z0-9]+/g, ' ').trim(); }

// Small-card rescue. Measured on saved scans: a card whose short side is
// under ~350 px in the uploaded frame almost never reads (title or collector
// line), and upscaling that frame rescues nothing — the detail is not there.
// The camera's NATIVE frame usually has 1.3-2x more pixels than the 1920
// upload, so a crop around the small card, taken from the native video,
// carries real extra detail. zoomPlan picks those crops; cards still too small
// even natively get a "move closer" hint instead of a silent failure.
export const SMALL_CARD_PX = 350;     // below this (upload px) reads fail
export const ZOOM_TARGET_PX = 620;    // crop is scaled so the card is ~this
export const ZOOM_MIN_GAIN = 1.25;    // native must add at least this much
export const ZOOM_MARGIN = 0.3;

export function zoomPlan({ candidates = [], results = [], frame, sw, sh, max = 3 }) {
  const byNum = new Map(results.map(r => [r.number ?? r.scene_number, r]));
  const k = frame?.width ? sw / frame.width : 1;      // upload px -> native px
  const crops = [];
  const tooSmall = [];
  for (const c of candidates) {
    if (!c.eligible || !c.box) continue;
    const r = byNum.get(c.number);
    if (r?.ok) continue;
    const [x, y, w, h] = c.box;
    const short = Math.min(w, h);
    if (short >= SMALL_CARD_PX) continue;
    const nativeShort = short * k;
    if (k < ZOOM_MIN_GAIN || nativeShort < SMALL_CARD_PX * 0.8) { tooSmall.push(c.number); continue; }
    if (crops.length >= max) continue;
    const mx = w * ZOOM_MARGIN, my = h * ZOOM_MARGIN;
    const x0 = Math.max(0, (x - mx) * k), y0 = Math.max(0, (y - my) * k);
    const x1 = Math.min(sw, (x + w + mx) * k), y1 = Math.min(sh, (y + h + my) * k);
    const scale = Math.min(1, ZOOM_TARGET_PX / nativeShort);   // never upscale
    crops.push({ number: c.number, sx: Math.round(x0), sy: Math.round(y0), sw: Math.round(x1 - x0), sh: Math.round(y1 - y0), scale });
  }
  return { crops, tooSmall };
}

// Pure: results whose printing the user must choose (footer twins), as tray
// hits. The placeholder id is stable for the same twin set, so Auto de-dupe
// treats a card that stays in view as one row, never one per pass.
export function choiceKey(ids) { return `choose:${[...ids].sort().join(',')}`; }
export function choiceHits(results) {
  return (results || []).filter(r => !r.ok && Array.isArray(r.choices) && r.choices.length > 1 && r.choices.every(c => c && c.id))
    .map(r => ({ ...r, ok: false, card: { ...r.choices[0], id: choiceKey(r.choices.map(c => c.id)) }, choose: r.choices }));
}
// "Odyssey or The List?" from the candidates' own set names (set-agnostic).
export function choiceLabel(cards) {
  const names = [...new Set((cards || []).map(c => c.set_name || String(c.set_id || '').toUpperCase()).filter(Boolean))];
  return names.length ? `${names.join(' or ')}?` : '';
}
