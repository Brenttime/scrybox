// Secret Lair "The Zeta Set" (slz) on the on-device reader (2026-11-20
// telemetry: Oboro, Palace in the Clouds read "Oboro, Palace in" on every
// pass and never matched a title; Thought Scour's "C 0031" footer has no
// printed total and resolved nothing). Port of the server fix (PR #156):
// two-line titles and zero-padded rarity footers. Every positive test here
// fails on the code before this change.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadIndex, paddedNumbers, resolveFooter, footerNumbersPadded, footerCodes, strongNumbers } from '../../../shared/clientScan/text.mjs';
import { createReader, CORN_SIZE, joinTitleLines, strongConflict } from '../../../shared/clientScan/pipeline.mjs';

// Real printings (Scryfall 2026-11): Thought Scour has sch 51 and slz 31 among
// others; Oboro has sok 164 and slz 119.
const ix = () => loadIndex({
  names: ['thought scour', 'oboro palace in the clouds', 'cathartic reunion'], canon: {}, excluded: [], sets: ['slz', 'sch', 'ima', 'sok', 'kld'],
  printings: [
    ['ts-slz31', 'slz', '31'], ['ts-sch51', 'sch', '51'], ['ts-ima76', 'ima', '76'], ['ts-slz273', 'slz', '273'],
    ['ob-sok164', 'sok', '164'], ['ob-slz119', 'slz', '119'],
    ['cr-kld', 'kld', '109'], ['cr-slz55', 'slz', '55'], ['cr-slz297', 'slz', '297'],
  ],
  byTitle: { 'thought scour': [0, 1, 2, 3], 'oboro palace in the clouds': [4, 5], 'cathartic reunion': [6, 7, 8] },
  uniqueAlias: {},
});

const resolve = (x, title, raws) => resolveFooter(x, title, footerCodes(x, raws), footerNumbersPadded(raws), strongNumbers(raws));

test('paddedNumbers: "C 0031" footers survive letter misreads (server footer_numbers_padded)', () => {
  assert.deepEqual(paddedNumbers(['C 0031']), ['31']);
  assert.deepEqual(paddedNumbers(['c oosi']), ['51', '31']);   // s = 5 or 3: both readings
  assert.deepEqual(paddedNumbers(['c00ss']), ['55', '53', '35', '33']);
  assert.deepEqual(paddedNumbers(['R 0ii9']), ['119']);
  assert.deepEqual(paddedNumbers(['c.0031']), ['31']);
  // Plain words, short tokens, totals and non-padded numbers add nothing.
  assert.deepEqual(paddedNumbers(['SLZ · EN MSCHF']), []);
  assert.deepEqual(paddedNumbers(['c ooi']), []);
  assert.deepEqual(paddedNumbers(['c 0000']), []);
  assert.deepEqual(paddedNumbers(['R 1234']), []);
  assert.deepEqual(paddedNumbers(['TM & © 2026 Wizards of the Coast']), []);
  assert.deepEqual(paddedNumbers(['scour 0031x']), []);
});

test('zeta footer: a padded number proves the one printing of the read title (with its set code, or read twice)', () => {
  const x = ix();
  // Real client OCR of synthesized slz frames: the set code read once.
  assert.equal(resolve(x, 'oboro palace in the clouds', ['SLZ·EN', 'R 0119']), 5);
  assert.equal(resolve(x, 'oboro palace in the clouds', ['SLZ·EN', 'R 0ii9']), 5);
  assert.equal(resolve(x, 'thought scour', ['SLZ.EN', 'c oosi']), 0);   // slz has 31, not 51
  // Set code garbled ('SLOENMS'): the number read in two strips / pooled frames.
  assert.equal(resolve(x, 'thought scour', ['c 0031', '2+EN', 'SLOENMS', 'c oo3i']), 0);
  assert.equal(resolve(x, 'cathartic reunion', ['C O055', 'c00s5']), 7);
});

test('zeta footer negatives: a lone set-less read, ambiguity and non-printings never resolve', () => {
  const x = ix();
  // One set-less read is not evidence, padded or not (scanAudit: lone 'U 0221').
  assert.equal(resolve(x, 'thought scour', ['c 0031', '2+EN', 'SLOENMS', 'a 2026 Windk of de Cot']), null);
  assert.equal(strongNumbers(['c oosi']).size, 0);
  // "s" reads as 5 or 3: 0051 (sch 51) and 0031 (slz 31) are both Thought Scour -> abstain.
  assert.equal(resolve(x, 'thought scour', ['c 00s1', 'c 00s1']), null);
  assert.equal(resolve(x, 'thought scour', ['c oosi', 'c oosi']), null);
  // A padded number that is no printing of the title proves nothing.
  assert.equal(resolve(x, 'thought scour', ['slz 0099', 'c 0099']), null);
  // A padded reading naming another printing is strong evidence against it.
  assert.equal(strongConflict(x, 'thought scour', 1, ['sch 51', 'c 0031', 'c 0031']), true);
  assert.equal(strongConflict(x, 'thought scour', 1, ['sch 51', 'slz c oo3i']), true);
  assert.equal(strongConflict(x, 'thought scour', 0, ['SLZ EN', 'c 0031']), false);
});

test('joinTitleLines: both lines must be read with title confidence', () => {
  assert.deepEqual(joinTitleLines([{ text: 'Oboro, Palace in ', conf: 0.9 }, { text: 'the Clouds', conf: 0.8 }]), { text: 'Oboro, Palace in the Clouds', conf: 0.8 });
  assert.equal(joinTitleLines([{ text: 'Oboro, Palace in', conf: 0.9 }, { text: '', conf: 0.9 }]), null);
  assert.equal(joinTitleLines([{ text: 'Oboro, Palace in', conf: 0.9 }, { text: 'the Clouds', conf: 0.4 }]), null);
  assert.equal(joinTitleLines([{ text: 'Oboro', conf: 0.9 }]), null);
});

// Pipeline with a fake recognizer (same harness as scanCascade.test.js).
function reader(textFor) {
  const ort = { Tensor: class { constructor(t, d, s) { this.data = d; this.dims = s; } } };
  const cornelius = { run: async () => ({ corners: { data: [0.2, 0.1, 0.8, 0.1, 0.8, 0.9, 0.2, 0.9] }, sharpness: { data: [0.9] } }) };
  const chars = ['', ...'abcdefghijklmnopqrstuvwxyz0123456789/ ©'];
  let calls = 0;
  const rec = {
    inputNames: ['x'], outputNames: ['y'],
    run: async ({ x }) => {
      calls++;
      const n = x.dims[0], texts = Array.from({ length: n }, (_, b) => textFor(calls, n, b));
      const steps = Math.max(1, ...texts.map(t => t.length)) * 2 + 1, classes = chars.length;
      const data = new Float32Array(n * steps * classes);
      for (let b = 0; b < n; b++) for (let s = 0; s < steps; s++) data[(b * steps + s) * classes + (s % 2 ? chars.indexOf(texts[b][(s - 1) / 2] ?? '') : 0)] = 1;
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

// Calls: 1 title (2 strips), 2 tight title (3 strips) + the 3 line bands when
// batch 1 read a confident fragment, else 3 = line bands; then footer.
test('pipeline: a wrapped two-line title (Oboro, Palace in / the Clouds) is matched and its padded footer proves slz 119', async () => {
  // A confident fragment in batch 1 puts the line bands into batch 2 (strips 3-5).
  const { r } = reader((c, n, b) => (c === 1 ? 'oboro palace in' : c === 2 ? (b < 3 ? 'oboro palace in' : ['oboro palace in', 'the clouds', 'legendary land'][b - 3]) : 'slz en r 0119'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.ok, true, JSON.stringify(res));
  assert.equal(res.title, 'oboro palace in the clouds');
  assert.equal(res.scryfallId, 'ob-slz119');
});

test('pipeline: a title fragment alone is never guessed into the longer name', async () => {
  const { r } = reader((c, n, b) => (c <= 2 ? 'oboro palace in' : c === 3 ? ['oboro palace in', ''][b] : 'r 0119'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.ok, false);
  assert.equal(res.error, 'no confident card title');
});

test('pipeline: a joined two-line read that matches no name stays a title miss', async () => {
  const { r } = reader((c, n, b) => (c <= 2 ? 'oboro palace in' : c === 3 ? ['oboro palace in', 'legendary land'][b] : 'r 0119'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.error, 'no confident card title');
});

test('pipeline: a one-line title that matches never pays for the two-line pass', async () => {
  const { r, calls } = reader((c) => (c === 1 ? 'thought scour' : 'slz en c oosi'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.ok, true, JSON.stringify(res));
  assert.equal(res.scryfallId, 'ts-slz31');
  // title + first footer batch only (6 rows + 2 Zeta bands: 2 recognizer runs)
  assert.equal(calls(), 3);
});
