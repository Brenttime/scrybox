// Title proof before any fast exit (2026-11-20 Zeta round, session sc3zv9p4).
// Fixtures are the exact title_raw arrays logged on the passes that added a
// wrong card, matched against a subset of the PRODUCTION client index
// (fixtures/zeta-title-index.json; see its `note`):
//   429/473/481/645: 'Oboro, Palace in' (1.0) + 'the Cloudse in' -> alias
//     'The Cloudsea Djinn' -> Nyxbloom Ancient fca 16 via the unique printed
//     title shortcut, footer never read.
//   1190: 'ulldriter' -> Hulldrifter (.900), guessed dft 47; the card was
//     Mulldrifter (next pass read 'Mulldrifter ATJ').
// Every negative here fails on the code before this change.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { loadIndex, findCardByOcr, titleCompatible, clippedAmbiguous } from '../../../shared/clientScan/text.mjs';
import { createReader, CORN_SIZE } from '../../../shared/clientScan/pipeline.mjs';

const raw = JSON.parse(fs.readFileSync(new URL('./fixtures/zeta-title-index.json', import.meta.url), 'utf8'));
const ix = () => loadIndex(raw);

test('fixture reproduces the production false matches (sanity)', () => {
  const x = ix();
  const a = findCardByOcr(x, 'the Cloudse in');
  assert.equal(a.name, 'nyxbloom ancient');
  assert.ok(a.score > 0.87 && a.score < 0.88);
  assert.equal(findCardByOcr(x, 'ulldriter').name, 'hulldrifter');
});

test('titleCompatible: a confident line of another card is incompatible; noisy lines of the right card are not', () => {
  const x = ix();
  assert.equal(titleCompatible(x, 'nyxbloom ancient', 'Oboro, Palace in'), false);
  assert.equal(titleCompatible(x, 'nyxbloom ancient', 'LObore Palace in'), false);
  assert.equal(titleCompatible(x, 'oboro palace in the clouds', 'Oboro, Palace in'), true);
  assert.equal(titleCompatible(x, 'oboro palace in the clouds', 'the Cloudse in'), true);
  assert.equal(titleCompatible(x, 'nature s claim', 'Instantre s Claim'), true);
  assert.equal(titleCompatible(x, 'obeka brute chronologist', 'OBEKA, BRUTE 1UE'), true);
  assert.equal(titleCompatible(x, 'obeka brute chronologist', 'CHRONOLOGIST 10'), true);
  assert.equal(titleCompatible(x, 'mulldrifter', 'Mulldrifter ATJ'), true);
  assert.equal(titleCompatible(x, 'mother of runes', 'Mother of Bunes'), true);
  // An alias counts as the title's own name.
  assert.equal(titleCompatible(x, 'nyxbloom ancient', 'the Cloudsea Djinn'), true);
});

test('clippedAmbiguous: a read missing leading letters never picks between same-tail names', () => {
  const x = ix();
  assert.equal(clippedAmbiguous(x, 'ulldriter', 'hulldrifter'), true);
  assert.equal(clippedAmbiguous(x, 'ulldrifter', 'hulldrifter'), true);
  // Unclipped reads, and clipped reads with one possible tail, stay usable.
  assert.equal(clippedAmbiguous(x, 'Hulldrifter', 'hulldrifter'), false);
  assert.equal(clippedAmbiguous(x, 'Mulldrifter4T', 'mulldrifter'), false);
  assert.equal(clippedAmbiguous(x, 'ightning Bolt', 'lightning bolt'), false);
  assert.equal(clippedAmbiguous(x, 'ornet Queen', 'hornet queen'), false);
});

// Pipeline with a fake recognizer (scanCascade.test.js harness). Batches:
// 1 = title (2 strips), 2 = tight title (3), 3 = two-line bands (2), then footer.
function reader(textFor) {
  const ort = { Tensor: class { constructor(t, d, s) { this.data = d; this.dims = s; } } };
  const cornelius = { run: async () => ({ corners: { data: [0.2, 0.1, 0.8, 0.1, 0.8, 0.9, 0.2, 0.9] }, sharpness: { data: [0.9] } }) };
  const chars = ['', ...'abcdefghijklmnopqrstuvwxyz0123456789/ ©'];
  let calls = 0;
  const rec = {
    inputNames: ['x'], outputNames: ['y'],
    run: async ({ x }) => {
      calls++;
      const n = x.dims[0], texts = Array.from({ length: n }, (_, b) => String(textFor(calls, n, b) ?? '').toLowerCase());
      const steps = Math.max(1, ...texts.map(t => t.length)) * 2 + 1, classes = chars.length;
      const data = new Float32Array(n * steps * classes);
      for (let b = 0; b < n; b++) for (let s = 0; s < steps; s++) data[(b * steps + s) * classes + (s % 2 ? Math.max(0, chars.indexOf(texts[b][(s - 1) / 2] ?? '')) : 0)] = 1;
      return { y: { data, dims: [n, steps, classes] } };
    },
  };
  return { r: createReader({ ort, cornelius, rec, chars, index: ix(), refineCorners: false, clock: () => 0, listStamp: false }), calls: () => calls };
}
function sharpFrame(w = 200, h = 280) {
  const data = new Uint8ClampedArray(w * h * 4);
  for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) { const i = (y * w + x) * 4, v = ((x >> 1) + (y >> 1)) % 2 ? 255 : 0; data[i] = data[i + 1] = data[i + 2] = v; data[i + 3] = 255; }
  return { data, width: w, height: h };
}
const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
const pick = (arr, b) => arr[b] ?? '';
const one = async (textFor, opts) => (await reader(textFor).r.read(sharpFrame(), small, opts)).results[0];

test('pass 429: the second line of Oboro never adds Nyxbloom Ancient via its printed alias', async () => {
  // Logged: b1 ['Lobore Palaca in', 'Oboro, Palace in'], b2 ['Oboro, Palace in', 'the Cloudse in'].
  // The two-line bands read the same fragments: no proof of any title.
  const res = await one((c, n, b) => (c === 1 ? pick(['Lobore Palaca in', 'Oboro, Palace in'], b)
    : c === 2 ? pick(['Oboro, Palace in', 'the Cloudse in', ''], b)
      : c === 3 ? pick(['Oboro, Palace in', 'the Cloudse in'], b) : ''), { guess: { sets: ['slz', 'slz', 'slz'] } });
  assert.notEqual(res.title, 'nyxbloom ancient', JSON.stringify(res));
  assert.notEqual(res.scryfallId, raw.printings[raw.byTitle['nyxbloom ancient'][0]][0]);
});

test('pass 429 with a legible second line: the ordered two-line read names Oboro and its footer proves slz 119', async () => {
  const res = await one((c, n, b) => (c === 1 ? pick(['Lobore Palaca in', 'Oboro, Palace in'], b)
    : c === 2 ? pick(['Oboro, Palace in', 'the Cloudse in', '', 'Oboro, Palace in', 'the Clouds', 'Legendary Land'], b)
      : 'slz en r 0119'));
  assert.equal(res.ok, true, JSON.stringify(res));
  assert.equal(res.title, 'oboro palace in the clouds');
  assert.equal(res.set, 'slz');
  assert.equal(res.num, '119');
});

test('pass 1190: a clipped "ulldriter" is not guessed into Hulldrifter', async () => {
  const res = await one((c, n, b) => (c === 1 ? pick(['ulldriter', ''], b) : c === 2 ? pick(['drifter', 'canner', '2'], b) : c === 3 ? '' : ''), { guess: { sets: ['slz', 'slz', 'slz'] } });
  assert.equal(res.ok, false, JSON.stringify(res));
  assert.notEqual(res.title, 'hulldrifter');
});

test('legit reads keep working: Hulldrifter vs Mulldrifter, trailing glyphs, a clean printed alias', async () => {
  const h = await one((c, n, b) => (c <= 2 ? pick(['Hulldrifter', 'Hulldrifter', ''], b) : 'dft en c 0047'));
  assert.equal(h.title, 'hulldrifter', JSON.stringify(h));
  assert.equal(h.num, '47');
  const m = await one((c, n, b) => (c <= 2 ? pick(['Mulldrifter4T', 'Mulldrifter'], b) : 'slz en c 0025'));
  assert.equal(m.title, 'mulldrifter', JSON.stringify(m));
  assert.equal(m.num, '25');
  // The alias read cleanly on its own is still the printed title it names.
  const a = await one((c, n, b) => (c <= 2 ? pick(['The Cloudsea Djinn', 'The Cloudsea Djinn', ''], b) : ''));
  assert.equal(a.ok, true, JSON.stringify(a));
  assert.equal(a.title, 'nyxbloom ancient');
  assert.equal(a.via, 'unique printed title');
});

test('two confident lines naming different cards abstain, whatever the footer says', async () => {
  const res = await one((c, n, b) => (c <= 2 ? pick(['Hornet Queen', 'Mother of Runes', ''], b) : c === 3 ? '' : 'slz en u 0078'));
  assert.equal(res.ok, false, JSON.stringify(res));
  assert.equal(res.error, 'no confident card title');
});

// Item 3: the whole two-line title on the first useful read.
test('two-line titles resolve with ONE tight+bands batch (title calls 2, not 3), trailing pips stripped', async () => {
  const { r, calls } = reader((c, n, b) => (c === 1 ? pick(['OBEKA, BRUTE 1UE', 'CHRONOLOGIST 10'], b)
    : c === 2 ? pick(['OBEKA, BRUTE 1U', 'CHRONOLOGIST', '', 'OBEKA, BRUTE 1UE', 'CHRONOLOGIST 10', 'Legendary Creature'], b)
      : 'slz en r 0092'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.ok, true, JSON.stringify(res));
  assert.equal(res.title, 'obeka brute chronologist');
  assert.equal(res.num, '92');
  assert.equal(calls(), 3, 'title, tight+bands, one footer batch');
});

test('an ordinary one-line card never pays for the line bands', async () => {
  const { r, calls } = reader((c, n, b) => (c === 1 ? pick(['Mother of Runes', 'Mother of Runes'], b) : 'slz en u 0006'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.ok, true, JSON.stringify(res));
  assert.equal(calls(), 2);
});
