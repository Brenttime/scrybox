import test from 'node:test';
import assert from 'node:assert/strict';
import { loadIndex, voteFooter, resolveFooter } from '../../../shared/clientScan/text.mjs';
import { createReader, corneliusTensor, CORN_SIZE } from '../../../shared/clientScan/pipeline.mjs';

// Regressions from the 2026-09 scanner audit.

test('voteFooter: the printed set total never votes, even when the set runs past it', () => {
  // MH2 runs to #492, so the old "k === setMax" guard did not protect #303.
  const ix = {
    byTitle: { lab: [0, 1] },
    printings: [['a', 'mh2', '80'], ['b', 'mh2', '303']],
    setLookup: new Set(['mh2']), setLengths: [3], setRank: new Map([['mh2', 0]]),
    setMax: new Map([['mh2', 492]]),
  };
  assert.equal(voteFooter(ix, 'lab', [['080/303'], ['080/303']]), 0);
  assert.notEqual(voteFooter(ix, 'lab', [['080/303'], ['080/303']]), 1);
  // Suffixed numerators strip their total too.
  assert.notEqual(voteFooter(ix, 'lab', [['080a/303'], ['080a/303']]), 1);
});

test('voteFooter: separate reads are never concatenated into a new number', () => {
  const ix = {
    byTitle: { lab: [0, 1] },
    printings: [['a', 'mh2', '30'], ['b', 'mh2', '300']],
    setLookup: new Set(['mh2']), setLengths: [3], setRank: new Map([['mh2', 0]]),
    setMax: new Map([['mh2', 492]]),
  };
  // "030" + "030" must not read as "030030" and vote for #300.
  assert.equal(voteFooter(ix, 'lab', [['030/303', '030/303'], ['030/303', '030/303']]), 0);
});

test('resolveFooter: contradictory exact set+number evidence is ambiguity', () => {
  const ix = loadIndex({
    names: ['bolt'], canon: {}, excluded: [], sets: ['lea', '2x2'],
    printings: [['x', 'lea', '161'], ['y', '2x2', '117']],
    byTitle: { bolt: [0, 1] }, uniqueAlias: {},
  });
  // Both codes and both numbers read: two exact identities -> no answer.
  assert.equal(resolveFooter(ix, 'bolt', ['lea', '2x2'], ['161', '117']), null);
  assert.equal(resolveFooter(ix, 'bolt', ['lea'], ['161']), 0);
});

test('corneliusTensor reuses its buffer and matches the reference normalisation', () => {
  const ort = { Tensor: class { constructor(t, d, s) { this.data = d; this.dims = s; } } };
  const px = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4).fill(128);
  const a = corneliusTensor(ort, px, 4), b = corneliusTensor(ort, px, 4);
  assert.equal(a.data, b.data);
  assert.ok(Math.abs(a.data[0] - (128 / 255 - 0.485) / 0.229) < 1e-5);
});

// A reader whose detector always finds the same full-frame card and whose
// recognizer returns scripted text, so identity-cache behaviour is observable.
function fakeReader(titleFor) {
  const ort = { Tensor: class { constructor(t, d, s) { this.data = d; this.dims = s; } } };
  let present = true;
  const cornelius = {
    run: async () => ({
      corners: { data: present ? [0.2, 0.1, 0.8, 0.1, 0.8, 0.9, 0.2, 0.9] : [0, 0, 0, 0, 0, 0, 0, 0] },
      sharpness: { data: [present ? 0.9 : 0] },
    }),
  };
  let calls = 0;
  const chars = ['', ...'abcdefghijklmnopqrstuvwxyz0123456789/ '];
  const rec = {
    inputNames: ['x'], outputNames: ['y'],
    run: async ({ x }) => {
      calls++;
      const n = x.dims[0], text = titleFor(calls), steps = text.length, classes = chars.length;
      const data = new Float32Array(n * steps * classes);
      for (let b = 0; b < n; b++) for (let s = 0; s < steps; s++) data[(b * steps + s) * classes + chars.indexOf(text[s])] = 1;
      return { y: { data, dims: [n, steps, classes] } };
    },
  };
  const index = loadIndex({ names: ['grief'], canon: {}, excluded: [], sets: ['mh2'], printings: [['id-grief', 'mh2', '87']], byTitle: { grief: [0] }, uniqueAlias: {} });
  let detects = 0;
  const run = cornelius.run;
  cornelius.run = async (x) => { detects++; return run(x); };
  const reader = createReader({ ort, cornelius, rec, chars, index });
  reader.__env = { ort, cornelius, rec, chars };
  return { reader, setPresent: (v) => { present = v; }, calls: () => calls, detects: () => detects };
}

// Like fakeReader, but the title has two printings, so identity needs the footer.
function fakeReaderIx(textFor) {
  const f = fakeReader(textFor);
  const index = loadIndex({ names: ['bolt'], canon: {}, excluded: [], sets: ['lea', '2x2'],
    printings: [['id-lea', 'lea', '161'], ['id-2x2', '2x2', '117']], byTitle: { bolt: [0, 1] }, uniqueAlias: {} });
  const env = f.reader.__env;
  return { ...f, reader: createReader({ ...env, index }) };
}

function sharpFrame(w = 200, h = 280, blur = false) {
  const d = new Uint8ClampedArray(w * h * 4);
  for (let i = 0; i < w * h; i++) { const v = blur ? 128 : ((i % w) + Math.floor(i / w)) % 2 ? 255 : 0; d[i * 4] = d[i * 4 + 1] = d[i * 4 + 2] = v; d[i * 4 + 3] = 255; }
  return { data: d, width: w, height: h };
}

test('identity cache does not survive the card leaving the frame', async () => {
  const { reader, setPresent, calls } = fakeReader(() => 'grief');
  const f = sharpFrame(), small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  const a = await reader.read(f, small);
  assert.equal(a.results[0]?.ok, true);
  const before = calls();
  const b = await reader.read(f, small);       // same card, still in view
  assert.equal(b.results[0]?.cached, true);
  assert.equal(calls(), before);
  setPresent(false);
  await reader.read(f, small);                   // card lifted
  setPresent(true);
  const c = await reader.read(f, small);         // same art put down again
  assert.notEqual(c.results[0]?.cached, true, 'a new card must be re-proven, not inherited from art');
  assert.ok(calls() > before);
});

test('identity cache does not survive a blurred (obscured) frame', async () => {
  const { reader, calls } = fakeReader(() => 'grief');
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  assert.equal((await reader.read(sharpFrame(), small)).results[0]?.ok, true);
  const before = calls();
  const blurred = await reader.read(sharpFrame(200, 280, true), small);
  assert.equal(blurred.candidates[0].status, 'too blurry');
  const again = await reader.read(sharpFrame(), small);
  assert.notEqual(again.results[0]?.cached, true);
  assert.ok(calls() > before);
});

test('reset() drops the tracked card', async () => {
  const { reader } = fakeReader(() => 'grief');
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  await reader.read(sharpFrame(), small);
  reader.reset();
  assert.notEqual((await reader.read(sharpFrame(), small)).results[0]?.cached, true);
});

test('a footer-proven printing is never carried by art alone', async () => {
  const { reader, calls } = fakeReaderIx((n) => (n % 3 === 1 ? 'bolt' : 'lea 161'));
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  const a = await reader.read(sharpFrame(), small);
  assert.equal(a.results[0]?.ok, true);
  assert.notEqual(a.results[0]?.via, 'unique physical printing');
  const before = calls();
  const b = await reader.read(sharpFrame(), small);   // same art, same spot
  assert.notEqual(b.results[0]?.cached, true, 'footer-proven: re-read every frame');
  assert.ok(calls() > before);
});

test('read() accepts corners from probe() and skips a second detection', async () => {
  const { reader, detects } = fakeReader(() => 'grief');
  const f = sharpFrame();
  const quad = await reader.probe(new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4), 4, f.width, f.height);
  assert.ok(quad);
  assert.equal(detects(), 1);
  const out = await reader.read(f, null, { quad });
  assert.equal(out.candidates.length, 1);
  assert.equal(detects(), 1, 'read with known corners must not run cornelius again');
});
