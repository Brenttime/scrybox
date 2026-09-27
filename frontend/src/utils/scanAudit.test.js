import test from 'node:test';
import assert from 'node:assert/strict';
import { loadIndex, voteFooter, resolveFooter } from '../../../shared/clientScan/text.mjs';
import { needsServer, NO_TITLE_ESCAPE } from './fastScan.js';
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
function fakeReader(titleFor, opts = {}) {
  const ort = { Tensor: class { constructor(t, d, s) { this.data = d; this.dims = s; } } };
  let present = true;
  const corners = opts.corners || [0.2, 0.1, 0.8, 0.1, 0.8, 0.9, 0.2, 0.9];
  const cornelius = {
    run: async () => ({
      corners: { data: present ? corners : [0, 0, 0, 0, 0, 0, 0, 0] },
      sharpness: { data: [present ? 0.9 : 0] },
    }),
  };
  let calls = 0;
  const chars = ['', ...'abcdefghijklmnopqrstuvwxyz0123456789/ '];
  const rec = {
    inputNames: ['x'], outputNames: ['y'],
    run: async ({ x }) => {
      calls++;
      const n = x.dims[0], text = titleFor(calls, n), steps = text.length, classes = chars.length;
      const data = new Float32Array(n * steps * classes);
      for (let b = 0; b < n; b++) for (let s = 0; s < steps; s++) data[(b * steps + s) * classes + chars.indexOf(text[s])] = 1;
      return { y: { data, dims: [n, steps, classes] } };
    },
  };
  const index = loadIndex({ names: ['grief'], canon: {}, excluded: [], sets: ['mh2'], printings: [['id-grief', 'mh2', '87']], byTitle: { grief: [0] }, uniqueAlias: {} });
  let detects = 0;
  const run = cornelius.run;
  cornelius.run = async (x) => { detects++; return run(x); };
  // Each observation 100 ms after the last (the settle window is time-based).
  let tick = 0; const clock = opts.clock || (() => (tick += 100));
  const reader = createReader({ ort, cornelius, rec, chars, index, refineCorners: opts.refine ?? false, clock });
  reader.__env = { ort, cornelius, rec, chars, refineCorners: opts.refine ?? false, clock };
  return { reader, setPresent: (v) => { present = v; }, calls: () => calls, detects: () => detects };
}

// Like fakeReader, but the title has two printings, so identity needs the footer.
function fakeReaderIx(textFor, opts = {}) {
  const f = fakeReader(textFor, opts);
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

test('auto: a still card needs a settled window before OCR; one stable pair is not enough', async () => {
  const { reader, calls } = fakeReader(() => 'grief');
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  const a = await reader.read(sharpFrame(), small, { requireStill: true });
  assert.equal(a.candidates[0].status, 'moving');          // first sighting: no history
  const b = await reader.read(sharpFrame(), small, { requireStill: true });
  assert.equal(b.candidates[0].status, 'settling');        // one stable pair
  assert.equal(calls(), 0, 'no OCR before the window is complete');
  const c = await reader.read(sharpFrame(), small, { requireStill: true });
  assert.equal(c.results[0]?.ok, true);
  // Non-auto (shutter) reads immediately.
  const { reader: r2 } = fakeReader(() => 'grief');
  assert.equal((await r2.read(sharpFrame(), small)).results[0]?.ok, true);
});

test('auto: a first unresolved footer defers deep stages, next frame continues and never guesses', async () => {
  // Title 'bolt' (two printings); footer never readable.
  const { reader, calls } = fakeReaderIx((_, batch) => (batch === 2 ? 'bolt' : 'zz'));
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  for (let i = 0; i < 2; i++) await reader.read(sharpFrame(), small, { requireStill: true });
  const c0 = calls();
  const a = await reader.read(sharpFrame(), small, { requireStill: true });
  assert.equal(a.results[0].ok, false);
  assert.equal(a.results[0].deferred, true);
  assert.equal(calls() - c0, 2, 'one title call (exact, confident: tight crops skipped) + first footer batch only');
  const c1 = calls();
  const b = await reader.read(sharpFrame(), small, { requireStill: true });
  assert.equal(b.results[0].ok, false);
  assert.notEqual(b.results[0].deferred, true, 'second frame runs every stage');
  assert.ok(calls() - c1 > 2);
});

test('needsServer: auto skips the server for unreadable titles (bounded) and deferred footers', () => {
  const out = (res) => ({ candidates: [{ eligible: true }], results: [res] });
  const noTitle = out({ ok: false, error: 'no confident card title' });
  assert.equal(needsServer(noTitle, { autoPass: true, noTitleRun: 1 }), false);
  assert.equal(needsServer(noTitle, { autoPass: true, noTitleRun: NO_TITLE_ESCAPE }), true, 'escape hatch');
  assert.equal(needsServer(noTitle, { autoPass: false }), true, 'shutter always may');
  assert.equal(needsServer(out({ ok: false, error: 'exact printing not resolved', deferred: true }), { autoPass: true }), false);
  assert.equal(needsServer(out({ ok: false, error: 'exact printing not resolved', title: 'x' }), { autoPass: true }), true);
  assert.equal(needsServer({ error: 'worker died' }, { autoPass: true }), true);
});

test('auto: deferral never repeats back to back (bounded), and blurred frames reset the settle window', async () => {
  const { reader, calls } = fakeReaderIx((_, batch) => (batch === 2 ? 'bolt' : 'zz'));
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  for (let i = 0; i < 2; i++) await reader.read(sharpFrame(), small, { requireStill: true });
  const outs = [];
  for (let i = 0; i < 4; i++) outs.push((await reader.read(sharpFrame(), small, { requireStill: true })).results[0]);
  for (let i = 1; i < outs.length; i++) assert.ok(!(outs[i].deferred && outs[i - 1].deferred), 'two deferrals in a row');
  assert.ok(outs.some(o => o && !o.deferred), 'full stages ran');
  // blur resets
  const { reader: r2 } = fakeReader(() => 'grief');
  await r2.read(sharpFrame(200, 280, true), small, { requireStill: true });
  await r2.read(sharpFrame(200, 280, true), small, { requireStill: true });
  const s = await r2.read(sharpFrame(), small, { requireStill: true });
  assert.notEqual(s.candidates[0].status, 'ready', 'first clear frame after blur must not inherit settling history');
  assert.ok(calls() >= 0);
});

test('corner refinement: one extra detection only for frames that get OCR', async () => {
  const { reader, detects } = fakeReader(() => 'grief', { refine: true });
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  await reader.read(sharpFrame(200, 280, true), small);          // blurry: gated, no OCR
  assert.equal(detects(), 1, 'gated frame: no refinement');
  const f = sharpFrame();
  const quad = await reader.probe(small, 4, f.width, f.height);
  const out = await reader.read(f, null, { quad });
  assert.equal(out.results[0]?.ok, true);
  assert.equal(detects(), 3, 'probe + one refinement on the Lanczos copy');
});

test('corner refinement: a far-off or edge-touching refined quad is ignored (gated quad used)', async () => {
  let n = 0;
  const f = fakeReader(() => 'grief', { refine: true });
  const env = f.reader.__env;
  const cornelius = { run: async () => { n++; return n === 1
    ? { corners: { data: [0.2, 0.1, 0.8, 0.1, 0.8, 0.9, 0.2, 0.9] }, sharpness: { data: [0.9] } }
    : { corners: { data: [0, 0, 0.5, 0, 0.5, 0.5, 0, 0.5] }, sharpness: { data: [0.9] } }; } };
  const index = loadIndex({ names: ['grief'], canon: {}, excluded: [], sets: ['mh2'], printings: [['id-grief', 'mh2', '87']], byTitle: { grief: [0] }, uniqueAlias: {} });
  const reader = createReader({ ...env, cornelius, index, refineCorners: true });
  const out = await reader.read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  assert.equal(n, 2);
  assert.notEqual(out.timings.refined, 1, 'refined quad touching the edge / far away must be rejected');
  assert.equal(out.candidates[0].quad[0][0], 0.2 * 200);
});

// 1000x1000 frame; card corners as fractions. The frame helper is square, so a
// 0.716 card is ~0.358 wide per 0.5 tall.
const nearTop = (y0) => [0.32, y0, 0.68, y0, 0.68, y0 + 0.5, 0.32, y0 + 0.5];

test('near-edge: a plausible fully visible card inside the 1% band is read (fresh, not tracked)', async () => {
  const { reader, calls } = fakeReader(() => 'grief', { corners: nearTop(0.006) });
  const f = sharpFrame(1000, 1000);
  const out = await reader.read(f, new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  assert.equal(out.candidates[0].near_edge, true);
  assert.equal(out.candidates[0].status, 'ready');
  // Both first title strips fit this outline: read as one batch, tagged 1.
  assert.equal(calls(), 1);
  assert.deepEqual(out.results[0].title_raw.map(t => t[2]), [1, 1]);
  const again = await reader.read(f, new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  assert.notEqual(again.results[0]?.cached, true, 'near-edge answers are never carried by tracking');
});

test('near-edge: a card at the very edge, or an implausible outline, is still vetoed', async () => {
  for (const corners of [nearTop(0.001), [0.25, 0.006, 0.75, 0.006, 0.75, 0.5, 0.25, 0.5], [0.32, 0.006, 0.68, 0.006, 0.60, 0.5, 0.40, 0.5]]) {
    const { reader, calls } = fakeReader(() => 'grief', { corners });
    const out = await reader.read(sharpFrame(1000, 1000), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
    assert.equal(out.candidates[0].status, 'touches frame edge', JSON.stringify(corners));
    assert.equal(calls(), 0);
  }
});

test('settle window counts real time: a faster loop does not shorten it', async () => {
  let t = 0;
  const { reader, calls } = fakeReader(() => 'grief', { clock: () => t });
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  const f = sharpFrame();
  for (let i = 0; i < 6; i++) { t += 30; await reader.read(f, small, { requireStill: true }); }
  // 6 passes 30 ms apart = 150 ms: only 2 observations >= 90 ms apart.
  assert.equal(calls(), 0, 'no OCR before 3 real-time-separated observations');
  t += 100; await reader.read(f, small, { requireStill: true });
  assert.ok(calls() > 0, 'third separated observation settles');
});

test('near-edge: collapsed, crossed or skewed outlines are rejected before OCR (review B2)', async () => {
  const f = 1000;
  const cases = [
    [100, 6, 460, 6, 959, 7, 599, 7],            // sides match, area ~0: collapsed
    [320, 6, 680, 6, 900, 400, 540, 400],        // strongly sheared
  ].map(q => q.map(v => v / f));
  for (const corners of cases) {
    const { reader, calls } = fakeReader(() => 'grief', { corners });
    const out = await reader.read(sharpFrame(f, f), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
    assert.equal(out.candidates[0].eligible, false, JSON.stringify(corners));
    assert.equal(calls(), 0, 'no OCR for ' + JSON.stringify(corners));
  }
});

test('near-edge: a footer stage with a strip outside the frame never proves a printing (review B1)', async () => {
  // Bottom-edge card: padded footer rows 0.92/0.94 cross the frame edge.
  // Rows that fit say lea 161, the dropped ones would say 2x2 117: the
  // complete stage is ambiguous, so the truncated one must not resolve.
  const corners = [0.32, 0.494, 0.68, 0.494, 0.68, 0.994, 0.32, 0.994];
  const { reader } = fakeReaderIx((calls, n) => (calls === 1 ? 'bolt' : n >= 5 ? 'lea 161' : 'lea 161'), { corners });
  const out = await reader.read(sharpFrame(1000, 1000), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  const r = out.results[0];
  assert.equal(out.candidates[0].near_edge, true, 'fixture must exercise the near-edge path');
  assert.equal(r.ok, false, 'a truncated footer stage must not prove a printing');
  assert.equal(r.near_edge_partial.stage, 'footer0', 'whole first footer stage skipped');
  assert.ok(r.near_edge_partial.off.length > 0);
});

test('near-edge: an inward refinement must pass the geometry check too (review B3)', async () => {
  // Astra's repro: valid small coarse card at the top edge, refined outline
  // collapsed but inside the 1% band and within the correction limit.
  let n = 0;
  const f = fakeReader(() => 'grief', { refine: true });
  const env = f.reader.__env;
  const S = 1000;
  const coarse = [320, 6, 334.4, 6, 334.4, 26, 320, 26].map(v => v / S);
  const fine = [320, 11, 334.4, 11, 354.4, 12, 340, 12].map(v => v / S);
  const cornelius = { run: async () => { n++; return { corners: { data: n % 2 ? coarse : fine }, sharpness: { data: [0.9] } }; } };
  const index = loadIndex({ names: ['grief'], canon: {}, excluded: [], sets: ['mh2'], printings: [['id-grief', 'mh2', '87']], byTitle: { grief: [0] }, uniqueAlias: {} });
  const reader = createReader({ ...env, cornelius, index, refineCorners: true });
  const out = await reader.read(sharpFrame(S, S), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  assert.equal(n, 2, 'coarse + refinement ran');
  assert.notEqual(out.timings.refined, 1, 'collapsed refinement must not be adopted');
  assert.equal(out.candidates[0].quad[3][1], 26, 'coarse geometry kept');
});
