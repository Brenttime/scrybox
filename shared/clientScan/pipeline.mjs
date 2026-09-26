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
  REC_H, portraitQuad, padQuad, cardToFrame, sampleStrip, artSignature, cosine, titleSharpness,
} from './imaging.mjs';
import {
  ctcDecode, findCardByOcr, normName, uniqueTitlePrinting, uniqueOcrPrinting,
  footerNumbers, footerCodes, resolveFooter, retroNumber, strongNumbers, looksLikeCopyright, voteFooter,
} from './text.mjs';

export const CORN_SIZE = 384;
const MEAN = [0.485, 0.456, 0.406];
const STD = [0.229, 0.224, 0.225];
const CORNER_GATE = 0.02;           // cornelius "sharpness" head: below = no card
const EDGE_FRAC = 0.01;             // server: touches frame edge within 1%
// Laplacian variance of the sampled title band. Replay of 506 saved phone
// frames: every proven card scored >= 2161; below 500 no title was ever read,
// so reading those only burned ~370 ms before failing. Auto waits for a
// sharper frame instead.
const TITLE_SHARP_FLOOR = 500;
const STILL_DRIFT = 0.012;          // mean corner move / frame diagonal
const REC_BATCH = 6;                // RapidOCR rec_batch_num
const TITLE_CONF = 0.60, FOOTER_CONF = 0.45, RETRO_CONF = 0.60;

const TITLE_FIRST = [[0.030, 0.82, 0.025, 0.100], [0.040, 0.80, 0.055, 0.120]];
const TITLE_TIGHT = [[0.045, 0.80, 0.045, 0.140], [0.050, 0.80, 0.090, 0.170], [0.010, 0.95, 0.000, 0.090]];
// One modern batch first: 5 strips cost about the same as 2 on the recognizer
// (one ONNX run either way), and 0.86-0.94 is where modern footers resolve.
// Replay: same matches, p50 matched read -29%, 2.3 vs 3.3 recognizer calls.
// The upper modern sweep (0.76-0.84) proved 2 of 353 cards on-device while
// costing ~200 ms on every miss; an unproven card goes to the server, whose
// own sweeps cover it.
const FOOTER_STAGES = [[0.88, 0.90, 0.92, 0.94, 0.86], 'retro'];
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
    const plane = REC_H * W;
    const data = new Float32Array(idx.length * 3 * plane);
    idx.forEach((si, n) => {
      const s = strips[si];
      const rw = Math.min(W, s.w);
      // RapidOCR feeds BGR (the sidecar converts RGB->BGR before text_rec).
      for (let y = 0; y < REC_H; y++) {
        for (let x = 0; x < rw; x++) {
          const p = (y * s.w + x) * 3, o = n * 3 * plane + y * W + x;
          data[o] = s.data[p + 2] / 127.5 - 1;
          data[o + plane] = s.data[p + 1] / 127.5 - 1;
          data[o + 2 * plane] = s.data[p] / 127.5 - 1;
        }
      }
    });
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
  async function read(frame, small, { smallChannels = 4, requireStill = false, quad: known } = {}) {
    const t0 = now();
    const { data: rgba, width: w, height: h } = frame;
    const timings = {};
    const quad = known !== undefined ? known : await detect(small, smallChannels, w, h);
    timings.detect_ms = Math.round(now() - t0);
    const base = { ok: true, engine: 'client', frame: { width: w, height: h }, candidates: [], results: [], timings };
    if (!quad) { lastQuad = null; tracked = null; evidence = null; return base; }
    const xs = quad.map(p => p.x), ys = quad.map(p => p.y);
    const box = [Math.round(Math.min(...xs)), Math.round(Math.min(...ys)),
      Math.round(Math.max(...xs) - Math.min(...xs)), Math.round(Math.max(...ys) - Math.min(...ys))];
    const cand = { number: 1, box, quad: quad.map(p => [p.x, p.y]), eligible: false, status: 'ready' };
    base.candidates.push(cand);

    const ex = Math.max(2, EDGE_FRAC * w), ey = Math.max(2, EDGE_FRAC * h);
    const clipped = xs.some(x => x <= ex || x >= w - ex) || ys.some(y => y <= ey || y >= h - ey);
    const diag = Math.hypot(w, h);
    const drift = lastQuad ? quad.reduce((s, p, i) => s + Math.hypot(p.x - lastQuad[i].x, p.y - lastQuad[i].y), 0) / 4 / diag : Infinity;
    lastQuad = quad;
    const m = cardToFrame(padQuad(quad));
    const sharp = titleSharpness(rgba, w, h, m);
    cand.sharpness = Math.round(sharp * 10) / 10;
    if (clipped) cand.status = 'touches frame edge';
    else if (sharp < TITLE_SHARP_FLOOR) cand.status = 'too blurry';
    else if (requireStill && drift > STILL_DRIFT) cand.status = 'moving';
    cand.eligible = cand.status === 'ready';
    // Tracking ends the moment the card is not plainly in view: a blurred,
    // clipped or moving frame is exactly when one card gets swapped for another
    // with the same art, so nothing proven before it may carry across.
    // Pooled footer reads go with it: votes may only combine across frames of
    // one uninterrupted presentation.
    if (!cand.eligible || drift > STILL_DRIFT * 4) { tracked = null; evidence = null; }
    if (!cand.eligible) return base;

    const sig = artSignature(rgba, w, h, m);
    if (tracked && cosine(tracked.sig, sig) < 0.97) tracked = null;
    if (tracked) {
      base.results.push({ ...tracked.result, number: 1, cached: true });
      timings.total_ms = Math.round(now() - t0);
      return base;
    }
    if (evidence && ++evidence.age > EVIDENCE_FRAMES) evidence = null;
    const prior = evidence && cosine(evidence.sig, sig) >= EVIDENCE_SIM ? evidence : null;
    const result = await readCard(rgba, w, h, m, timings, prior);
    timings.total_ms = Math.round(now() - t0);
    base.results.push(result);
    if (result.ok) { evidence = null; tracked = { sig, result }; }
    else if (result.title && result.footer_ocr?.length) {
      const keep = prior && prior.name === result.title ? prior.frames : [];
      evidence = { sig, name: result.title, frames: [...keep, result.footer_ocr].slice(-EVIDENCE_KEEP), age: 0 };
    }
    return base;
  }

  async function readCard(rgba, w, h, m, timings, prior = null) {
    const tA = now();
    const strip = (r) => sampleStrip(rgba, w, h, m, r[0], r[1], r[2], r[3]);
    const cands = [];
    const consider = (reads) => {
      for (const r of reads) {
        if (!r.text || r.conf < TITLE_CONF) continue;
        const found = findCardByOcr(env.index, r.text);
        if (found.name) cands.push({ score: found.score, conf: r.conf, name: found.name, raw: r.text });
      }
    };
    consider(await recognize(env, TITLE_FIRST.map(strip)));
    if (!cands.length || Math.max(...cands.map(c => c.name.length)) < 12) {
      consider(await recognize(env, TITLE_TIGHT.map(strip)));
    }
    timings.title_ms = Math.round(now() - tA);
    const titleReads = cands.map(c => c.raw);
    if (!cands.length) {
      return { number: 1, ok: false, retry: true, error: 'no confident card title', title: null, ocr: titleReads };
    }
    cands.sort((a, b) => b.score - a.score || b.conf - a.conf || (a.name < b.name ? 1 : -1));
    const { name, raw, score } = cands[0];
    const ix = env.index;
    const done = (pi, via, footer, stage = -1) => {
      const p = ix.printings[pi];
      timings.footer_ms = Math.round(now() - tA) - timings.title_ms;
      return { number: 1, ok: true, scryfallId: p[0], set: p[1], num: p[2], title: name, title_score: score, via, footer_stage: stage, footer_ocr: footer };
    };
    let pi = uniqueTitlePrinting(ix, name);
    if (pi != null) return done(pi, 'unique physical printing', []);
    pi = uniqueOcrPrinting(ix, raw, name);
    if (pi != null) return done(pi, 'unique printed title', []);

    const raws = [];
    const pooled = prior && prior.name === name ? prior.frames : null;
    const tryPooled = (si) => {
      if (!pooled || !raws.length) return null;
      const frames = [...pooled, raws];
      const all = frames.flat();
      let p = resolveFooter(ix, name, footerCodes(ix, all), footerNumbers(all), strongNumbers(all));
      if (p == null) p = voteFooter(ix, name, frames);
      return p == null ? null : done(p, 'title+collector (multi-frame)', all, si);
    };
    for (const [si, stage] of (env.footerStages || FOOTER_STAGES).entries()) {
      if (stage === 'retro') {
        const reads = await recognize(env, RETRO_ROWS.map(y => strip([0.35, 0.95, y, y + 0.025])));
        const nums = [];
        for (const r of reads) {
          if (!r.text || r.conf < RETRO_CONF) continue;
          const n = looksLikeCopyright(r.text) ? retroNumber(r.text) : null;
          if (n && !nums.includes(n)) nums.push(n);
          raws.push(r.text);
        }
        if (nums.length) {
          pi = resolveFooter(ix, name, [], nums);
          if (pi != null) return done(pi, 'title+collector (retro frame)', raws, si);
        }
        const pooledHit = tryPooled(si);
        if (pooledHit) return pooledHit;
        continue;
      }
      const reads = await recognize(env, stage.map(y => strip([0, 0.22, y, y + 0.025])));
      for (const r of reads) if (r.text && r.conf >= FOOTER_CONF) raws.push(r.text);
      pi = resolveFooter(ix, name, footerCodes(ix, raws), footerNumbers(raws), strongNumbers(raws));
      if (pi != null) return done(pi, 'title+set+collector', raws, si);
      const pooledHit = tryPooled(si);
      if (pooledHit) return pooledHit;
    }
    timings.footer_ms = Math.round(now() - tA) - timings.title_ms;
    return { number: 1, ok: false, retry: true, error: 'exact printing not resolved', title: name, footer_ocr: raws };
  }

  // Corners only, from the 384px copy. A null here is a definite "no card" and
  // ends tracking exactly as a full read would.
  async function probe(small, smallChannels, w, h) {
    const quad = await detect(small, smallChannels, w, h);
    if (!quad) { lastQuad = null; tracked = null; evidence = null; }
    return quad;
  }

  return { read, probe, stats: env.stats, reset() { lastQuad = null; tracked = null; evidence = null; }, resetCache() { tracked = null; } };
}

export { normName };

function now() {
  return typeof performance !== 'undefined' ? performance.now() : Date.now();
}
