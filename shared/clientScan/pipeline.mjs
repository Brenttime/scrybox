// The in-browser card reader, end to end, for one camera frame:
//
//   cornelius corners -> portrait quad -> edge / blur / stillness gates
//   -> title strips -> recognizer -> fuzzy title -> unique printing?
//   -> staged footer strips (modern 0.90/0.92, retro copyright line,
//      then the lower and upper modern sweeps) -> exactly one printing, or fail.
//
// Environment-free: the caller injects onnxruntime (web in the worker, node in
// the replay harness), both sessions, the charset and the index, so the code
// that is validated offline is byte-for-byte the code that runs on the phone.
//
// Strip geometry, confidence floors and stage order are the cardscan sidecar's
// (_batch_scene_titles, _batch_scene_printings, _retro_footer_pass). One card
// per frame: cornelius predicts a single card. Multi-card scenes are the
// server's job; the caller falls back to it for anything this returns as
// unresolved.
import {
  REC_H, CARD_W, CARD_H, portraitQuad, padQuad, SERVER_PAD, cardToFrame, sampleStrip, artSignature, cosine, titleSharpness, resizeLanczos3,
} from './imaging.mjs';
import {
  ctcDecode, findCardByOcr, normName, uniqueTitlePrinting, uniqueOcrPrinting,
  footerNumbers, footerCodes, resolveFooter, retroNumber, strongNumbers, looksLikeCopyright, voteFooter, normalizeCollector,
} from './text.mjs';

export const CORN_SIZE = 384;
const TITLE_PROVEN = new Set(['unique physical printing', 'unique printed title']);
const MEAN = [0.485, 0.456, 0.406];
const STD = [0.229, 0.224, 0.225];
const CORNER_GATE = 0.02;           // cornelius "sharpness" head: below = no card
const EDGE_FRAC = 0.01;             // server: touches frame edge within 1%
// Near-edge admission. A card whose top sits 6-8 px below a 1080-high frame
// edge is fully visible, but the 1% band (10.8 px) vetoed it indefinitely:
// 161 of 788 desktop passes, and the 8.0 s / 5.6 s / 5.2 s worst adds of the
// 63-card session. Inside the band a card is admitted only when the quad is a
// plausible, near-front-facing card (side-length aspect, opposing sides
// alike), it clears a tight margin (0.2%, >= 2 px), and every OCR strip it is
// about to read projects inside the frame (padQuad reaches past the outline,
// and sampleStrip clamps off-frame pixels into repeated edge rows). Such a
// frame gets a fresh read only: no tracked identity or pooled evidence.
const EDGE_TIGHT_FRAC = 0.002;
const NEAR_EDGE_ASPECT = [0.60, 0.85];   // short/long side, a card is 0.716
const NEAR_EDGE_SIDE_RATIO = 0.85;       // min/max of opposing sides
const NEAR_EDGE_MAX_COS = Math.cos(70 * Math.PI / 180);   // corners 70-110 deg
// Near the edge, every strip a stage would read must fit, or the frame is
// unresolved: dropping a strip can drop exactly the competing identity (or
// title) that would have made the answer ambiguous, so a partial stage is
// never allowed to prove anything. A fresh frame further in will be read.
// Laplacian variance of the sampled title band. Replay of 506 saved phone
// frames: every proven card scored >= 2161; below 500 no title was ever read,
// so reading those only burned ~370 ms before failing. Auto waits for a
// sharper frame instead.
const TITLE_SHARP_FLOOR = 500;
const STILL_DRIFT = 0.012;          // mean corner move / frame diagonal
// Auto admits a card for OCR only after a short stable WINDOW, not one pair of
// frames: Windows telemetry (23 cards) showed the first frame that passed a
// single-pair check was usually still smeared (title OCR garbage, ~0.5 s
// local + ~0.35 s server wasted per card). STILL_OBS consecutive
// observations, each within STILL_DRIFT of the previous and no more than
// STILL_GAP_MS apart, so a sample taken before a long read cannot vouch for
// the frame after it.
const STILL_OBS = 3;
const STILL_GAP_MS = 400;
// A settle window spans real time, not loop iterations: an observation less
// than STILL_MIN_SEP_MS after the last counted one (a fast desktop loop, or
// the same decoded video frame twice) does not advance it. At the old 60 ms
// loop every pass was >= ~100 ms apart, so this keeps the window measured on
// the 63-card session unchanged when the loop runs faster.
const STILL_MIN_SEP_MS = 90;
// Admission is an ELAPSED window: STILL_OBS observations spanning at least
// STILL_WINDOW_MS of real time, instead of counting only observations
// >= 90 ms apart. The counting rule aliased with the loop cadence: at 65 ms
// passes, every other observation fell short and admission took 260 ms, vs
// 210 ms at 105 ms passes. This removes that observation-spacing aliasing
// (65 ms now admits at 195 ms); admission is still quantised to the loop
// cadence, so it is not monotonic across every cadence, and it is never
// sooner than 2 x 90 ms after the first still observation.
const STILL_WINDOW_MS = (STILL_OBS - 1) * STILL_MIN_SEP_MS;
// Adaptive settle (R2-#1), EXPERIMENTAL, off unless env.fastSettle: a window
// of FAST_SETTLE_MS instead of STILL_WINDOW_MS, but only when every counted
// observation of it was a distinct decoded frame (frameId), drifted at most
// half the normal tolerance, and was well above the blur floor. Anything
// else keeps the conservative window; it can only admit sooner, never later.
// Off by default: still images cannot validate earlier admission (Astra R2:
// needs recorded video with decoded-frame timestamps).
const FAST_SETTLE_MS = 120;
const FAST_SETTLE_DRIFT = STILL_DRIFT / 2;
const FAST_SETTLE_SHARP = 4 * 500;
// Crop-level text contrast gate for the title rescue (R2-#3), EXPERIMENTAL,
// off unless env.titleGate: skip the second title batch when EVERY first
// title crop is nearly flat before autocontrast (max channel range below
// this). Always measured (timings.title_range) for shadow calibration.
const TITLE_MIN_RANGE = 24;
// Pure: no confident title from batch one AND every batch-one crop is flat.
export function flatTitleCrops(cands, ranges) {
  return !cands.length && ranges.length > 0 && ranges.every(r => Number.isFinite(r) && r < TITLE_MIN_RANGE);
}
const REC_BATCH = 6;                // RapidOCR rec_batch_num
const TITLE_CONF = 0.60, FOOTER_CONF = 0.45, RETRO_CONF = 0.60;
const TITLE_EXACT_CONF = 0.90;
// A same-frame correction, never more than the stillness tolerance: a
// refined quad further than a still card may drift is not the same region.
const REFINE_MAX_MOVE = STILL_DRIFT;
// Near the edge, the server-style padding (3.6% above, 7.9% below the true
// outline) can push a stage's strips past the frame while the card itself is
// fully visible (Roiling Canopy: title strip at y=-1.6; Solarium Sentry:
// footer rows at y=1079-1100 in a 1080 frame), and the whole stage then
// abstained on every frame. Such a stage is re-projected with that side's
// padding shrunk toward the true outline, whole: every strip of the stage is
// read from the same homography, none is dropped. Only near-edge frames that
// would otherwise abstain use it; everywhere else the padding is unchanged
// (removing it globally lost Winter, Team Player's footer).
// Fitting in the frame is not enough: shrinking a side's padding moves every
// strip of the stage in TRUE-card coordinates, and could slide the footer
// rows up off the collector line onto the text box (Astra review R1-B1: a
// synthetic card read unrelated text box digits and proved the wrong
// printing). A re-projection is used only if the stage still covers,
// on the real card (outline = [0,1]), everything the padded stage covered,
// within COVER_TOL (0.6% of the card): at most that sliver of on-card rows
// is lost, otherwise only reads of the mat beyond the outline. Otherwise the stage abstains, as before.
const PAD_SHRINK = [0.75, 0.5, 0.25, 0];
const COVER_TOL = 0.006;
// Padded-card fraction -> true-card fraction along one axis.
const toTrue = (f, lo, hi) => f * (1 + lo + hi) - lo;
function coversStage(rs, pad) {
  const P = SERVER_PAD;
  for (const [a, b, lo, hi] of [[2, 3, 't', 'b'], [0, 1, 'l', 'r']]) {
    // Union of the new strips, on the true card.
    const got = rs.map(r => [toTrue(r[a], pad[lo], pad[hi]), toTrue(r[b], pad[lo], pad[hi])]).sort((x, y) => x[0] - y[0]);
    const union = [];
    for (const [x0, x1] of got) {
      const last = union[union.length - 1];
      if (last && x0 <= last[1] + COVER_TOL) last[1] = Math.max(last[1], x1); else union.push([x0, x1]);
    }
    // Every on-card part of every original strip must lie inside it.
    for (const r of rs) {
      const oa = Math.max(0, toTrue(r[a], P[lo], P[hi])), ob = Math.min(1, toTrue(r[b], P[lo], P[hi]));
      if (ob <= oa) continue;
      if (!union.some(([u0, u1]) => u0 <= oa + COVER_TOL && u1 >= ob - COVER_TOL)) return false;
    }
  }
  return true;
}

const TITLE_FIRST = [[0.030, 0.82, 0.025, 0.100], [0.040, 0.80, 0.055, 0.120]];
const TITLE_TIGHT = [[0.045, 0.80, 0.045, 0.140], [0.050, 0.80, 0.090, 0.170], [0.010, 0.95, 0.000, 0.090]];
// One modern batch first: 6 strips cost about the same as 2 on the recognizer
// (one ONNX run either way), and 0.86-0.94 is where modern footers resolve.
// Replay: same matches, p50 matched read -29%, 2.3 vs 3.3 recognizer calls.
// The upper modern sweep (0.76-0.84) proved 2 of 353 cards on-device while
// costing ~200 ms on every miss; an unproven card goes to the server, whose
// own sweeps cover it.
// 'wide': the same rows read out to x=0.30 for a card the narrow sweep did not
// prove. FRA-era 4-digit numbers ("U 0298") put the last digit past 0.22, so
// the narrow strip reads "U 029". Only unresolved cards pay for it.
// 0.84 rides in the first batch: on FRA-era frames with a loose (padded)
// outline the collector line sits there ("U 0138"), and neither the narrow
// nor the wide sweep read it, so Konstrari Charm-style cards burned two
// failed server fallbacks each. Replay of 71 saved fallback frames: client
// matches 3 -> 16, 0 printing or title conflicts, 0 baseline hits lost.
// Still one recognizer call (6 strips = REC_BATCH).
// 'rescue' (R2-#2): one batch of the rows the first stage clipped. The wide
// 0.84 row (WIDE_ROWS has no 0.84, so "R.006" never became "R 0064", Oath of
// Eorl) and the modern rows 0.035 tall instead of 0.025 (Helm "R.0209" ->
// "R 0200", H.E.R.B.I.E. "2196" -> "R 0106": the digits' lower edge was
// cut). Its reads are ADDED to the earlier ones, never substituted, so any
// conflicting number still makes the footer ambiguous; nothing is ever
// snapped to the nearest indexed number. One recognizer call (6 strips), only
// for a card the first batch did not prove. Replay: 71 old fallback frames
// 49 -> 55, 5 new 0 -> 5, every baseline hit kept, 0 conflicts.
const RESCUE_RECTS = [[0, 0.30, 0.84, 0.865], ...[0.86, 0.88, 0.90, 0.92, 0.94].map(y => [0, 0.22, y, y + 0.035])];
// Rescue runs LAST, after every other stage (review R1-S2, R2-S2); it only
// sees cards no earlier stage proved. A card the first batch proves costs the
// same calls as before; 'tall' (below) runs before wide/retro, so later-stage
// successes may now resolve at tall or pay one extra call.
// 'tall' right after the first batch: replay of every 4th saved frame (322)
// +12 printings, 0 lost, 0 changed, no new wrong (vs 0.035 everywhere: +16
// but 6 lost). Taller-only-everywhere was rejected for those losses.
const FOOTER_STAGES = [[0.88, 0.90, 0.92, 0.94, 0.86, 0.84], 'tall', 'wide', 'retro', 'rescue'];
// Pure: once rescue reads are in the evidence, a printing is proved only if
// NO other printing of the title is named by it: not by a strong number (N/T
// or read in 2 strips; set totals ignored), and not by an exact set code +
// number pair (review R1-B3: "lea 161 117" then a retro "117").
export function strongConflict(ix, title, pi, raws) {
  const pool = ix.byTitle[title] || [];
  const mine = normalizeCollector(ix.printings[pi][2]);
  const strong = [...strongNumbers(raws).keys()];
  const codes = new Set(footerCodes(ix, raws)), nums = new Set(footerNumbers(raws));
  for (const o of pool) {
    if (o === pi) continue;
    const n = normalizeCollector(ix.printings[o][2]);
    if (n === mine && ix.printings[o][1] !== ix.printings[pi][1]) continue;   // same number elsewhere: set evidence decided
    if (strong.includes(n)) return true;
    if (codes.has(ix.printings[o][1]) && nums.has(String(ix.printings[o][2]).toLowerCase())) return true;
  }
  return false;
}
const WIDE_ROWS = [0.88, 0.90, 0.86, 0.92];
// 'tall': the first batch's rows again at 0.030 instead of 0.025. iPhone
// telemetry (MSH/MSC "U 0086", "8088"/"0888" for 0088) and replay: the
// 0.025 strip clips the digits' lower edge on many frames. Reads are added
// to the first batch's, so a conflicting number still means ambiguity.
const TALL_ROWS = [0.88, 0.90, 0.92, 0.94, 0.86, 0.84];
const TALL_H = 0.030;
const WIDE_X1 = 0.30;
const RETRO_ROWS = [0.855, 0.845];

// Cornelius input: the frame squashed to 384x384 (fit: fill, like the server's
// cvScan), ImageNet-normalised, NCHW.
// One buffer and per-channel lookup tables, reused every frame: detection runs
// several times a second and a fresh 1.7 MB Float32Array plus two divides per
// channel per pixel is pure garbage-collector and ALU churn. Safe to reuse
// because the reader awaits each run before building the next tensor.
const PLANE = CORN_SIZE * CORN_SIZE;
let tensorBuf = null;
const LUT = [0, 1, 2].map(ch => Float32Array.from({ length: 256 }, (_, v) => (v / 255 - MEAN[ch]) / STD[ch]));
export function corneliusTensor(ort, rgb, channels = 3) {
  const t = tensorBuf || (tensorBuf = new Float32Array(3 * PLANE));
  const [l0, l1, l2] = LUT;
  for (let i = 0, j = 0; i < PLANE; i++, j += channels) {
    t[i] = l0[rgb[j]];
    t[PLANE + i] = l1[rgb[j + 1]];
    t[2 * PLANE + i] = l2[rgb[j + 2]];
  }
  return new ort.Tensor('float32', t, [1, 3, CORN_SIZE, CORN_SIZE]);
}

// A near-front-facing card outline: side-length aspect near 0.716 and
// opposing sides alike. Anything else keeps the conservative 1% edge band.
function plausibleCard(q) {
  if (!q.every(p => Number.isFinite(p.x) && Number.isFinite(p.y))) return false;
  const d = (a, b) => Math.hypot(a.x - b.x, a.y - b.y);
  // Convex, consistently wound, every interior angle 70-110 degrees: rules out
  // collapsed, crossed and strongly skewed outlines (whose homography would
  // stretch the OCR strips), not just mismatched side lengths.
  let sign = 0;
  for (let i = 0; i < 4; i++) {
    const a = q[(i + 3) % 4], b = q[i], c = q[(i + 1) % 4];
    const ux = a.x - b.x, uy = a.y - b.y, vx = c.x - b.x, vy = c.y - b.y;
    const cross = ux * vy - uy * vx, nu = Math.hypot(ux, uy), nv = Math.hypot(vx, vy);
    if (!(nu > 0 && nv > 0) || cross === 0) return false;
    const s = Math.sign(cross);
    if (sign && s !== sign) return false;
    sign = s;
    const cos = (ux * vx + uy * vy) / (nu * nv);
    if (Math.abs(cos) > NEAR_EDGE_MAX_COS) return false;
  }
  const top = d(q[0], q[1]), bot = d(q[3], q[2]), lef = d(q[0], q[3]), rig = d(q[1], q[2]);
  if (!(top > 0 && bot > 0 && lef > 0 && rig > 0)) return false;
  if (Math.min(top, bot) / Math.max(top, bot) < NEAR_EDGE_SIDE_RATIO) return false;
  if (Math.min(lef, rig) / Math.max(lef, rig) < NEAR_EDGE_SIDE_RATIO) return false;
  const a = Math.min(top + bot, lef + rig) / Math.max(top + bot, lef + rig);
  return a >= NEAR_EDGE_ASPECT[0] && a <= NEAR_EDGE_ASPECT[1];
}

// Every corner of card-space strip r ([x0,x1,y0,y1] fractions) projects at
// least 1 px inside the frame, so no sample is a clamped edge pixel.
function stripInFrame(m, r, w, h) {
  const y1 = Math.min(0.99, r[3]);
  for (const [fx, fy] of [[r[0], r[2]], [r[1], r[2]], [r[1], y1], [r[0], y1]]) {
    const x = fx * CARD_W, y = fy * CARD_H, den = m[6] * x + m[7] * y + 1;
    const px = (m[0] * x + m[1] * y + m[2]) / den, py = (m[3] * x + m[4] * y + m[5]) / den;
    if (!(px >= 1 && px <= w - 2 && py >= 1 && py <= h - 2)) return false;
  }
  return true;
}

// Recognizer input: a per-length reusable Float32Array (the batch shapes repeat
// frame after frame: 2x3x48x401, 6x3x48x320, ...), zeroed before reuse so the
// right padding stays exactly 0, and a 256-entry normalisation table. Safe to
// reuse: each run is awaited before the next batch is packed. Bounded.
const REC_LUT = Float32Array.from({ length: 256 }, (_, v) => v / 127.5 - 1);
const recBufs = new Map();
function recBuffer(len) {
  let b = recBufs.get(len);
  if (b) { b.fill(0); return b; }
  if (recBufs.size >= 8) recBufs.delete(recBufs.keys().next().value);
  b = new Float32Array(len);
  recBufs.set(len, b);
  return b;
}

// Pack strips idx (RGB, height REC_H) into one NCHW BGR batch of width W.
export function packRecBatch(strips, idx, W) {
  const plane = REC_H * W;
  const data = recBuffer(idx.length * 3 * plane);
  idx.forEach((si, n) => {
    const s = strips[si];
    const rw = Math.min(W, s.w);
    // RapidOCR feeds BGR (the sidecar converts RGB->BGR before text_rec).
    // REC_LUT[v] is the same float32 as v / 127.5 - 1 (R2-#5).
    const src = s.data, b0 = n * 3 * plane;
    for (let y = 0; y < REC_H; y++) {
      let p = y * s.w * 3, o = b0 + y * W;
      for (let x = 0; x < rw; x++, p += 3, o++) {
        data[o] = REC_LUT[src[p + 2]];
        data[o + plane] = REC_LUT[src[p + 1]];
        data[o + 2 * plane] = REC_LUT[src[p]];
      }
    }
  });
  return data;
}

// RapidOCR TextRecognizer: sort by aspect, batches of 6, each padded (zeros,
// i.e. mid-grey after normalisation) to the batch's widest ratio, min 320/48.
async function recognize(env, strips) {
  const out = new Array(strips.length);
  if (!strips.length) return out;
  const order = strips.map((s, i) => i).sort((a, b) => strips[a].w / strips[a].h - strips[b].w / strips[b].h);
  for (let b = 0; b < order.length; b += REC_BATCH) {
    const idx = order.slice(b, b + REC_BATCH);
    let maxRatio = 320 / REC_H;
    for (const i of idx) maxRatio = Math.max(maxRatio, strips[i].w / strips[i].h);
    const W = Math.trunc(REC_H * maxRatio);
    const data = packRecBatch(strips, idx, W);
    const res = await env.rec.run({ [env.rec.inputNames[0]]: new env.ort.Tensor('float32', data, [idx.length, 3, REC_H, W]) });
    const pred = res[env.rec.outputNames[0]];
    const [, steps, classes] = pred.dims;
    idx.forEach((si, n) => { out[si] = ctcDecode(pred.data, steps, classes, env.chars, n); });
    env.stats.recCalls++; env.stats.recStrips += idx.length;
  }
  return out;
}

export function createReader(env) {
  // env: { ort, cornelius, rec, chars, index }
  env.stats = { recCalls: 0, recStrips: 0 };
  let lastQuad = null;
  let stillRun = 0, lastAt = 0;   // consecutive low-drift observations
  let stillSince = 0;             // real time the current still window began
  let readSince = false;          // OCR ran since the last observation
  let lastDeferred = false;       // previous read deferred its deep footer stages
  let lastFrameId = null;         // decoded-frame id of the last counted observation
  let fastRun = true;             // every observation of this window qualifies for fast settle
  // Identity is carried only for the card being CONTINUOUSLY tracked. Artwork
  // alone does not prove a printing — reprints share art and differ only in
  // the footer — so a signature match is trusted only while the same physical
  // card has stayed in view since it was proven. Any frame without a card
  // drops it, and the next card is re-proven from its own title + footer.
  let tracked = null;   // {sig, result}
  // Cross-frame footer evidence. A still card is read on consecutive frames;
  // each frame's footer OCR is noisy in different places, so an unresolved
  // card's footer reads are kept and pooled with the next frame's IF it is the
  // same card (same title, same art). Pooled reads then count as independent
  // strips for the "two strips agree" rule. Exactness is unchanged: an answer
  // is still one printing for title + number (+ set), or nothing.
  let evidence = null;  // {sig, name, frames: [raws per frame], age}
  const EVIDENCE_SIM = 0.92, EVIDENCE_FRAMES = 8, EVIDENCE_KEEP = 6;

  async function detect(small, channels, frameW, frameH) {
    const out = await env.cornelius.run({ image: corneliusTensor(env.ort, small, channels) });
    const c = out.corners.data;
    const conf = out.sharpness ? out.sharpness.data[0] : 1;
    if (!(conf > CORNER_GATE)) return null;
    const pts = [0, 1, 2, 3].map(i => ({ x: c[2 * i] * frameW, y: c[2 * i + 1] * frameH }));
    return portraitQuad(pts);
  }

  // One frame. `frame` = {data: RGBA, width, height}; `small` = 384x384 pixels
  // of the same frame (RGBA or RGB, `smallChannels`). Options:
  //   requireStill: auto mode — only read once the card has stopped moving.
  //   quad: corners already found by probe() for this same frame, so the
  //         caller can skip reading back the full frame when there is no card.
  async function read(frame, small, opts = {}) {
    const { smallChannels = 4, requireStill = false, quad: known, frameId = null } = opts;
    const t0 = now();
    const { data: rgba, width: w, height: h } = frame;
    const timings = {};
    const quad = known !== undefined ? known : await detect(small, smallChannels, w, h);
    timings.detect_ms = Math.round(now() - t0);
    const base = { ok: true, engine: 'client', frame: { width: w, height: h }, candidates: [], results: [], timings };
    if (!quad) { lastQuad = null; stillRun = 0; lastDeferred = false; lastFrameId = null; tracked = null; evidence = null; return base; }
    const xs = quad.map(p => p.x), ys = quad.map(p => p.y);
    const box = [Math.round(Math.min(...xs)), Math.round(Math.min(...ys)),
      Math.round(Math.max(...xs) - Math.min(...xs)), Math.round(Math.max(...ys) - Math.min(...ys))];
    const cand = { number: 1, box, quad: quad.map(p => [p.x, p.y]), eligible: false, status: 'ready' };
    base.candidates.push(cand);

    const ex = Math.max(2, EDGE_FRAC * w), ey = Math.max(2, EDGE_FRAC * h);
    const tx = Math.max(2, EDGE_TIGHT_FRAC * w), ty = Math.max(2, EDGE_TIGHT_FRAC * h);
    const inside = (q, mx, my) => q.every(p => p.x > mx && p.x < w - mx && p.y > my && p.y < h - my);
    const nearEdge = !inside(quad, ex, ey) && inside(quad, tx, ty) && plausibleCard(quad);
    const clipped = !inside(quad, ex, ey) && !nearEdge;
    if (nearEdge) cand.near_edge = true;
    // Which frame sides the outline is at (R2-#9), for a directional hint.
    // Guidance only: admission is unchanged.
    if (clipped || nearEdge) {
      const sides = [];
      if (ys.some(y => y <= ey)) sides.push('top');
      if (ys.some(y => y >= h - ey)) sides.push('bottom');
      if (xs.some(x => x <= ex)) sides.push('left');
      if (xs.some(x => x >= w - ex)) sides.push('right');
      if (sides.length) cand.edge_sides = sides;
    }
    const diag = Math.hypot(w, h);
    // The same decoded video frame seen again (R2-#6): it is not new evidence
    // of stillness, and re-reading its pixels cannot change the answer. In
    // auto it never advances the settle window and is never read.
    if (requireStill && frameId != null && frameId === lastFrameId && lastQuad) {
      cand.duplicate = true; cand.status = 'settling'; cand.still = stillRun;
      return base;
    }
    const drift = lastQuad ? quad.reduce((s, p, i) => s + Math.hypot(p.x - lastQuad[i].x, p.y - lastQuad[i].y), 0) / 4 / diag : Infinity;
    lastQuad = quad;
    // A change of frame-identity source (decoded-frame number <-> pixel
    // fingerprint, when rVFC stops or resumes) is not comparable evidence:
    // the window restarts (review R2-B2).
    const sourceSwitch = lastFrameId != null && frameId != null && typeof lastFrameId !== typeof frameId;
    lastFrameId = frameId;
    const tNow = env.clock ? env.clock() : now();
    // A long gap right after an OCR read may not vouch for the frame after it
    // (the card could have moved and come back), so it counts at most as the
    // start of a new window; a gap with no read in it (slow camera or device)
    // is plain slow sampling and counts normally, so a slow cadence can never
    // wedge Auto.
    // Only a read that actually ran the recognizer counts as a gap "after an
    // OCR read": a geometric abstention (zero rec calls) is as quick as a gate
    // pass and must not force a fresh settle cycle.
    if (drift > STILL_DRIFT || stillRun === 0 || sourceSwitch) { stillRun = 1; stillSince = tNow; fastRun = true; }
    else if (tNow - lastAt > STILL_GAP_MS && readSince) {
      // The window continues but needs one more observation, and at least
      // STILL_MIN_SEP_MS more of it, after this gap.
      stillRun = Math.min(stillRun + 1, STILL_OBS - 1);
      stillSince = Math.max(stillSince, tNow - STILL_WINDOW_MS + STILL_MIN_SEP_MS);
      fastRun = false;
    } else { stillRun++; if (drift > FAST_SETTLE_DRIFT) fastRun = false; }
    lastAt = tNow;
    readSince = false;
    let m = cardToFrame(padQuad(quad));
    const sharp = titleSharpness(rgba, w, h, m);
    cand.sharpness = Math.round(sharp * 10) / 10;
    // Fast settle needs real decoded-frame numbers (rVFC), not fingerprints.
    if (sharp < FAST_SETTLE_SHARP || typeof frameId !== 'number') fastRun = false;
    const window = env.fastSettle && fastRun ? FAST_SETTLE_MS : STILL_WINDOW_MS;
    const settled = stillRun >= STILL_OBS && tNow - stillSince >= window;
    if (settled && window < STILL_WINDOW_MS && tNow - stillSince < STILL_WINDOW_MS) cand.fast_settle = true;
    cand.still = stillRun;
    // Blurred or clipped observations are not part of a settled window.
    if (clipped || sharp < TITLE_SHARP_FLOOR) stillRun = 0;
    cand.still = stillRun;
    if (clipped) cand.status = 'touches frame edge';
    else if (sharp < TITLE_SHARP_FLOOR) cand.status = 'too blurry';
    // No settling by default (Brent, 2026-09-28): a sharp, unclipped card is
    // read on the first frame it is seen. Exact-printing proof (name + number)
    // and the sharpness floor, not a stillness window, guard correctness.
    // env.settle === true restores the old multi-observation window.
    else if (env.settle === true && requireStill && drift > STILL_DRIFT) cand.status = 'moving';
    else if (env.settle === true && requireStill && (stillRun === 0 || !settled)) cand.status = 'settling';
    cand.eligible = cand.status === 'ready';
    // Tracking ends the moment the card is not plainly in view: a blurred,
    // clipped or moving frame is exactly when one card gets swapped for another
    // with the same art, so nothing proven before it may carry across.
    // Pooled footer reads go with it: votes may only combine across frames of
    // one uninterrupted presentation.
    // 'settling' is a still card waiting out its window: nothing about it
    // suggests a swap, so tracking and pooled evidence survive it.
    if ((!cand.eligible && cand.status !== 'settling') || drift > STILL_DRIFT * 4 || nearEdge) { tracked = null; evidence = null; }
    if (!cand.eligible) return base;

    // Refine the corners on a Lanczos-resized copy of THIS frame (the corner
    // model's validated input): the caller's 384px copy comes from a cheap
    // canvas downscale, whose corners are a few px off. Saved Windows frames
    // through a canvas-equivalent downscale: 4 of 11 printings proven without
    // this, 11 of 11 with it (identical to the Node/sharp replay).
    // The refined quad is only a small correction of the gated one: it must
    // stay close (REFINE_MAX_MOVE), off the frame edge and sharp, or the
    // gated quad is used unchanged. The art signature is taken AFTER, so
    // tracking and pooled evidence always refer to the region OCR reads.
    // ~140-170 ms in the browser; only frames about to be OCR'd pay for it.
    // A tracked, title-proven card still in view skips refinement and OCR:
    // it is matched on the signature of the gated quad, as it was stored.
    const coarseSig = artSignature(rgba, w, h, m);
    if (tracked && cosine(tracked.coarse, coarseSig) < 0.97) tracked = null;
    if (tracked) {
      base.results.push({ ...tracked.result, number: 1, cached: true });
      timings.total_ms = Math.round(now() - t0);
      return base;
    }
    if (env.refineCorners !== false) {
      const tr = now();
      const fine = await detect(resizeLanczos3(rgba, w, h, 4, CORN_SIZE, CORN_SIZE), 3, w, h);
      const move = fine ? fine.reduce((s, p, i) => s + Math.hypot(p.x - quad[i].x, p.y - quad[i].y), 0) / 4 / diag : Infinity;
      // A near-edge card's refinement must pass the same geometry check as
      // its coarse outline wherever it lands, inward included.
      const fineClipped = fine && (nearEdge ? !(inside(fine, tx, ty) && plausibleCard(fine)) : !inside(fine, ex, ey));
      if (fine && move <= REFINE_MAX_MOVE && !fineClipped) {
        const mf = cardToFrame(padQuad(fine));
        if (titleSharpness(rgba, w, h, mf) >= TITLE_SHARP_FLOOR) { m = mf; cand.quad = fine.map(p => [p.x, p.y]); timings.refined = 1; }
      }
      timings.refine_ms = Math.round(now() - tr);
    }
    const sig = timings.refined ? artSignature(rgba, w, h, m) : coarseSig;
    if (evidence && ++evidence.age > EVIDENCE_FRAMES) evidence = null;
    const prior = evidence && cosine(evidence.sig, sig) >= EVIDENCE_SIM ? evidence : null;
    const recBefore = env.stats.recCalls;
    const readQuad = timings.refined ? cand.quad.map(([x, y]) => ({ x, y })) : quad;
    const result = await readCard(rgba, w, h, m, timings, prior, requireStill && !lastDeferred, nearEdge, readQuad);
    lastDeferred = !!result.deferred;
    timings.total_ms = Math.round(now() - t0);
    timings.rec_calls = env.stats.recCalls - recBefore;
    readSince = timings.rec_calls > 0;
    if (prior) timings.pooled_frames = prior.frames.length;
    base.results.push(result);
    // Only a title-proven answer is carried: a card swapped for a same-art
    // reprint between two frames at the same spot is invisible to tracking,
    // so a printing that needed its footer is re-read every time.
    if (nearEdge) { evidence = null; tracked = null; }
    else if (result.ok) { evidence = null; tracked = TITLE_PROVEN.has(result.via) ? { sig, coarse: coarseSig, result } : null; }
    else if (result.title && (result.footer_ocr?.length || result.deferred)) {
      const keep = prior && prior.name === result.title ? prior.frames : [];
      evidence = { sig, name: result.title, frames: [...keep, result.footer_ocr].slice(-EVIDENCE_KEEP), age: 0, deferred: !!result.deferred || !!(prior && prior.name === result.title && prior.deferred),
        rescued: !!result.rescued || !!(prior && prior.name === result.title && prior.rescued) };
    }
    return base;
  }

  async function readCard(rgba, w, h, m, timings, prior = null, auto = false, nearEdge = false, quad = null) {
    const tA = now();
    // Near the frame edge, a strip that reaches off-frame would be read from
    // clamped, repeated edge pixels. The stage is re-projected with the
    // offending side's padding shrunk (PAD_SHRINK) until every strip fits;
    // if none does, the whole stage is skipped (never a partial stage) and
    // the card fails this frame. The FIRST skipped stage is reported.
    let truncated = false;
    const fitted = [];
    const stripsIn = (mm, rs) => rs.every(r => stripInFrame(mm, r, w, h));
    const strips = (rs, stage) => {
      let mm = m;
      if (nearEdge && !stripsIn(mm, rs)) {
        const off = rs.map((r, i) => (stripInFrame(m, r, w, h) ? -1 : i)).filter(i => i >= 0);
        mm = null;
        if (quad) {
          // One side first (the one at the edge), then both; mildest first.
          const P = SERVER_PAD;
          outer: for (const k of env.padShrink || PAD_SHRINK) {
            for (const [pad, side] of [[{ ...P, t: P.t * k }, 't'], [{ ...P, b: P.b * k }, 'b'], [{ ...P, t: P.t * k, b: P.b * k }, 'tb'],
              [{ ...P, l: P.l * k, r: P.r * k }, 'lr'], [{ l: P.l * k, r: P.r * k, t: P.t * k, b: P.b * k }, 'all']]) {
              if (!coversStage(rs, pad)) continue;
              const c = cardToFrame(padQuad(quad, pad));
              if (stripsIn(c, rs)) { mm = c; fitted.push([stage, side, k]); timings.pad_fit = fitted; break outer; }
            }
          }
        }
        if (!mm) {
          if (!truncated) truncated = { stage, off };
          else (truncated.also ||= []).push(stage);
          return [];
        }
      }
      return rs.map(r => sampleStrip(rgba, w, h, mm, r[0], r[1], r[2], r[3]));
    };
    const cands = [];
    // Every title read, accepted or not, for scan telemetry: a failed title
    // otherwise leaves no trace of what the recognizer actually saw.
    const titleRaw = [];
    const consider = (reads, batch) => {
      for (const r of reads) {
        if (!r) continue;
        if (r?.text && titleRaw.length < 8) titleRaw.push([r.text.slice(0, 60), Math.round(r.conf * 100) / 100, batch]);
        if (!r.text || r.conf < TITLE_CONF) continue;
        const found = findCardByOcr(env.index, r.text);
        if (found.name) cands.push({ score: found.score, conf: r.conf, name: found.name, raw: r.text });
      }
    };
    const t1 = strips(TITLE_FIRST, 'title1');
    const titleRange = t1.map(s => s.range);
    if (titleRange.length) timings.title_range = titleRange;
    consider(await recognize(env, t1), 1);
    // The tighter crops rescue weak or partial reads. An exact, high-confidence
    // match of a full index name needs no rescue: skip the second recognizer
    // call. This only selects the TITLE; printings still need their proof.
    const exact = cands.length > 0 && cands.every(c => c.score === 1 && c.conf >= TITLE_EXACT_CONF && c.name === cands[0].name);
    const flat = flatTitleCrops(cands, titleRange);
    if (flat) timings.title_flat = 1;
    if (flat && env.titleGate) timings.title_gated = 1;
    else if (!exact && (!cands.length || Math.max(...cands.map(c => c.name.length)) < 12)) {
      consider(await recognize(env, strips(TITLE_TIGHT, 'title2')), 2);
    }
    timings.title_ms = Math.round(now() - tA);
    const titleReads = cands.map(c => c.raw);
    const partial = (extra = {}) => ({ number: 1, ok: false, retry: true, error: 'card too close to the frame edge', near_edge_partial: truncated, title: null, ocr: titleReads, title_raw: titleRaw, ...extra });
    if (truncated) return partial();
    if (!cands.length) {
      return { number: 1, ok: false, retry: true, error: 'no confident card title', title: null, ocr: titleReads, title_raw: titleRaw };
    }
    cands.sort((a, b) => b.score - a.score || b.conf - a.conf || (a.name < b.name ? 1 : -1));
    const { name, raw, score } = cands[0];
    const ix = env.index;
    const done = (pi, via, footer, stage = -1) => {
      const p = ix.printings[pi];
      timings.footer_ms = Math.round(now() - tA) - timings.title_ms;
      return { number: 1, ok: true, scryfallId: p[0], set: p[1], num: p[2], title: name, title_score: score, via, footer_stage: stage, footer_ocr: footer, title_raw: titleRaw };
    };
    let pi = uniqueTitlePrinting(ix, name);
    if (pi != null) return done(pi, 'unique physical printing', []);
    pi = uniqueOcrPrinting(ix, raw, name);
    if (pi != null) return done(pi, 'unique printed title', []);

    const raws = [];
    let rescued = false;
    const pooled = prior && prior.name === name ? prior.frames : null;
    // Evidence pooled from a deferred frame only saw the first footer batch.
    // Pooling it must not settle the printing before this frame has run the
    // stages that frame skipped (wide can disambiguate what narrow misread).
    // v1's last stage (retro): a deferred frame's pooled evidence may settle
    // the printing there exactly as in v1, before the added rescue stage
    // (review R3-S2). Rescue is appended after it and never moves this line.
    const stageList = env.footerStages || FOOTER_STAGES;
    const lastStage = stageList.includes('rescue') ? stageList.indexOf('rescue') - 1 : stageList.length - 1;
    const tryPooled = (si) => {
      if (!pooled || !raws.length) return null;
      if (prior.deferred && si < lastStage) return null;
      const frames = [...pooled, raws];
      const all = frames.flat();
      let p = resolveFooter(ix, name, footerCodes(ix, all), footerNumbers(all), strongNumbers(all));
      if (p == null) p = voteFooter(ix, name, frames);
      // Rescue reads (this frame or a pooled one) never prove a printing that
      // another strong number on these frames contradicts.
      if (p != null && (rescued || prior.rescued) && strongConflict(ix, name, p, all)) { timings.rescue_conflict = 1; p = null; }
      return p == null ? null : done(p, 'title+collector (multi-frame)', all, si);
    };
    for (const [si, stage] of (env.footerStages || FOOTER_STAGES).entries()) {
      if (stage === 'retro') {
        const reads = await recognize(env, strips(RETRO_ROWS.map(y => [0.35, 0.95, y, y + 0.025]), 'retro'));
        if (truncated) return partial({ title: name, footer_ocr: raws });
        const nums = [];
        for (const r of reads) {
          if (!r.text || r.conf < RETRO_CONF) continue;
          const n = looksLikeCopyright(r.text) ? retroNumber(r.text) : null;
          if (n && !nums.includes(n)) nums.push(n);
          raws.push(r.text);
        }
        if (nums.length) {
          pi = resolveFooter(ix, name, [], nums);
          if (pi != null && rescued && strongConflict(ix, name, pi, raws)) { timings.rescue_conflict = 1; pi = null; }
          if (pi != null) return done(pi, 'title+collector (retro frame)', raws, si);
        }
        const pooledHit = tryPooled(si);
        if (pooledHit) return pooledHit;
        continue;
      }
      const rows = stage === 'wide' ? WIDE_ROWS : stage === 'tall' ? TALL_ROWS : stage;
      const x1 = stage === 'wide' ? WIDE_X1 : 0.22;
      const hgt = stage === 'tall' ? TALL_H : 0.025;
      const rects = stage === 'rescue' ? RESCUE_RECTS : rows.map(y => [0, x1, y, y + hgt]);
      const reads = await recognize(env, strips(rects, `footer${si}`));
      if (truncated) return partial({ title: name, footer_ocr: raws });
      for (const r of reads) if (r.text && r.conf >= FOOTER_CONF) raws.push(r.text);
      pi = resolveFooter(ix, name, footerCodes(ix, raws), footerNumbers(raws), strongNumbers(raws));
      // A rescue-stage answer must not contradict ANY strong number read on
      // this frame (R2-#2 replay: rescue read "089/59%" -> DRC 89 while the
      // first batch read "080/505", the real MH2 80, dropped only by its set
      // total check). Conflicting evidence means ambiguity, not a pick.
      // Holds for every later stage too: the rescue reads stay in raws.
      if (stage === 'rescue' || stage === 'tall') rescued = true;
      if (pi != null && rescued && strongConflict(ix, name, pi, raws)) { timings.rescue_conflict = 1; pi = null; }
      if (pi != null) return done(pi, 'title+set+collector', raws, si);
      const pooledHit = tryPooled(si);
      if (pooledHit) return pooledHit;
      // Auto, first look at this card: the deep stages (wide, retro) cost
      // ~0.9 s and on a frame this fresh usually fail too (Windows telemetry:
      // Refute Destiny, Marwyn). Stop here; this frame's footer reads are
      // pooled, and the next still frame of the same card continues with them
      // and runs every stage.
      if (auto && !pooled && si === 0) {
        timings.footer_ms = Math.round(now() - tA) - timings.title_ms;
        return { number: 1, ok: false, retry: true, error: 'exact printing not resolved', deferred: true, title: name, title_score: score, footer_ocr: raws, title_raw: titleRaw };
      }
    }
    timings.footer_ms = Math.round(now() - tA) - timings.title_ms;
    return { number: 1, ok: false, retry: true, error: 'exact printing not resolved', title: name, title_score: score, footer_ocr: raws, title_raw: titleRaw, rescued: rescued || undefined };
  }

  // Corners only, from the 384px copy. A null here is a definite "no card" and
  // ends tracking exactly as a full read would.
  async function probe(small, smallChannels, w, h) {
    const quad = await detect(small, smallChannels, w, h);
    if (!quad) { lastQuad = null; stillRun = 0; lastDeferred = false; lastFrameId = null; tracked = null; evidence = null; }
    return quad;
  }

  return { read, probe, stats: env.stats, // A new presentation / run: nothing about the last card's footer
    // deferral may make this card skip its cheap first look (review R2-#8).
    reset() { lastQuad = null; stillRun = 0; lastDeferred = false; lastFrameId = null; tracked = null; evidence = null; }, resetCache() { tracked = null; } };
}

export { normName };

function now() {
  return typeof performance !== 'undefined' ? performance.now() : Date.now();
}
