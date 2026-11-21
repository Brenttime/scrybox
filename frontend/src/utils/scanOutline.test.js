// Outer-paper outline (shared/clientScan/outline.mjs) and its use by the
// reader when the corner model boxes only the art, or finds nothing, on a
// borderless cream card (2026-11-20 Zeta round). Fails on the code before it.
import test from 'node:test';
import assert from 'node:assert/strict';
import { paperOutline } from '../../../shared/clientScan/outline.mjs';
import { createReader, CORN_SIZE } from '../../../shared/clientScan/pipeline.mjs';
import { loadIndex } from '../../../shared/clientScan/text.mjs';

const FW = 1920, FH = 1080;
// A 384x384 squashed copy of a FW x FH frame holding one card (paper value
// `card`, background `bg`) rotated by `deg` around (cx, cy), card height ch px.
function scene({ bg = 40, card = 225, deg = 0, cx = 960, cy = 540, ch = 600, none = false } = {}) {
  const out = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  const cw = ch * 0.716, a = (deg * Math.PI) / 180, c = Math.cos(a), s = Math.sin(a);
  for (let y = 0; y < CORN_SIZE; y++) for (let x = 0; x < CORN_SIZE; x++) {
    const fx = ((x + 0.5) * FW) / CORN_SIZE - cx, fy = ((y + 0.5) * FH) / CORN_SIZE - cy;
    const u = fx * c + fy * s, v = -fx * s + fy * c;
    const inside = !none && Math.abs(u) < cw / 2 && Math.abs(v) < ch / 2;
    // Art window inside the paper (darker, textured), like a Zeta card.
    const art = inside && Math.abs(u) < cw * 0.4 && v > -ch * 0.3 && v < ch * 0.25;
    const val = !inside ? bg + ((x * 7 + y * 3) % 9) : art ? 90 + ((x * 13 + y * 5) % 60) : card;
    const i = (y * CORN_SIZE + x) * 4; out[i] = out[i + 1] = out[i + 2] = val; out[i + 3] = 255;
  }
  return out;
}
const box = (q) => { const xs = q.map(p => p.x), ys = q.map(p => p.y); return [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)]; };
const near = (a, b, tol) => a.every((v, i) => Math.abs(v - b[i]) <= tol);

test('paperOutline: finds the whole paper on dark and light backgrounds, upright and rotated', () => {
  const want = [960 - 215, 240, 960 + 215, 840];
  for (const [bg, card] of [[40, 225], [235, 120]]) {
    const o = paperOutline(scene({ bg, card }), CORN_SIZE, CORN_SIZE, 4, FW, FH);
    assert.ok(o, `bg ${bg}`);
    assert.ok(near(box(o.quad), want, 20), `bg ${bg}: ${box(o.quad)}`);
  }
  const r = paperOutline(scene({ deg: 12 }), CORN_SIZE, CORN_SIZE, 4, FW, FH);
  assert.ok(r, 'rotated');
  const [x0, y0, x1, y1] = box(r.quad);
  assert.ok(Math.abs((x0 + x1) / 2 - 960) < 20 && Math.abs((y0 + y1) / 2 - 540) < 20);
});

test('paperOutline: nothing on an empty table', () => {
  assert.equal(paperOutline(scene({ none: true }), CORN_SIZE, CORN_SIZE, 4, FW, FH), null);
});

// Reader with a fake corner model and recognizer.
const ix = () => loadIndex({ names: ['oboro palace in the clouds'], canon: {}, excluded: [], sets: ['slz', 'sok'], printings: [['ob-sok164', 'sok', '164'], ['ob-slz119', 'slz', '119']], byTitle: { 'oboro palace in the clouds': [0, 1] }, uniqueAlias: {} });
function reader(corners, textFor) {
  const ort = { Tensor: class { constructor(t, d, s) { this.data = d; this.dims = s; } } };
  const cornelius = { run: async () => (corners ? { corners: { data: corners }, sharpness: { data: [0.9] } } : { corners: { data: [0, 0, 0, 0, 0, 0, 0, 0] }, sharpness: { data: [0] } }) };
  const chars = ['', ...'abcdefghijklmnopqrstuvwxyz0123456789/ ©'];
  let calls = 0;
  const rec = {
    inputNames: ['x'], outputNames: ['y'],
    run: async ({ x }) => {
      calls++;
      const n = x.dims[0], texts = Array.from({ length: n }, (_, b) => textFor(calls, n, b));
      const steps = Math.max(1, ...texts.map(t => t.length)) * 2 + 1, classes = chars.length;
      const data = new Float32Array(n * steps * classes);
      for (let b = 0; b < n; b++) for (let s = 0; s < steps; s++) data[(b * steps + s) * classes + (s % 2 ? Math.max(0, chars.indexOf(texts[b][(s - 1) / 2] ?? '')) : 0)] = 1;
      return { y: { data, dims: [n, steps, classes] } };
    },
  };
  return { r: createReader({ ort, cornelius, rec, chars, index: ix(), refineCorners: false, clock: () => 0, listStamp: false }), calls: () => calls };
}
// Full frame: a sharp checker everywhere (titleSharpness needs detail).
function frame() {
  const data = new Uint8ClampedArray(FW * FH * 4);
  for (let y = 0; y < FH; y++) for (let x = 0; x < FW; x++) { const i = (y * FW + x) * 4, v = ((x >> 1) + (y >> 1)) % 2 ? 255 : 0; data[i] = data[i + 1] = data[i + 2] = v; data[i + 3] = 255; }
  return { data, width: FW, height: FH };
}
const F = frame();
const ART = [902, 347, 1173, 347, 1173, 840, 902, 840].map((v, i) => v / (i % 2 ? FH : FW));   // Oboro art-only box

test('reader: an art-only corner quad with no title is re-read on the outer paper outline', async () => {
  // Title strips of the art box read noise; any strip of the outline reads the name.
  let outlineRead = false;
  const { r } = reader(ART, (c, n, b) => {
    if (c <= 3) return 'k';
    if (c <= 5) { outlineRead = true; return n === 2 ? ['oboro palace in', 'the clouds'][b] : 'oboro palace in the clouds'; }
    return 'slz en r 0119';
  });
  const out = await r.read(F, scene());
  const res = out.results[0];
  assert.ok(outlineRead);
  assert.equal(res.ok, true, JSON.stringify(res));
  assert.equal(res.scryfallId, 'ob-slz119');
  assert.equal(out.candidates[0].outline, 1);
});

test('reader: no corners but a paper outline is read; with no title it stays "no card"', async () => {
  const hit = reader(null, (c) => (c === 1 ? 'oboro palace in the clouds' : 'slz en r 0119'));
  const a = await hit.r.read(F, scene());
  assert.equal(a.results[0]?.scryfallId, 'ob-slz119', JSON.stringify(a.results[0]));
  const miss = reader(null, () => 'k');
  const b = await miss.r.read(F, scene());
  assert.equal(b.candidates.length, 0);
  assert.equal(b.results.length, 0);
  // An empty table never reaches the recognizer.
  const empty = reader(null, () => 'oboro palace in the clouds');
  const e = await empty.r.read(F, scene({ none: true }));
  assert.equal(e.candidates.length, 0);
  assert.equal(empty.calls(), 0);
});

test('reader: a clipped card (outline at the frame edge) is never read from its outline', async () => {
  const { r, calls } = reader(null, () => 'oboro palace in the clouds');
  const out = await r.read(F, scene({ cx: 1920 - 150 }));
  assert.equal(out.results.length, 0);
  assert.equal(calls(), 0);
});

test('reader: env.paperOutline === false keeps the old behaviour', async () => {
  const out = await createReader({ ort: { Tensor: class {} }, cornelius: { run: async () => ({ corners: { data: [0, 0, 0, 0, 0, 0, 0, 0] }, sharpness: { data: [0] } }) }, rec: null, chars: [], index: ix(), paperOutline: false, refineCorners: false, listStamp: false }).read(F, scene());
  assert.equal(out.candidates.length, 0);
});
