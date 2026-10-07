import test from 'node:test';
import assert from 'node:assert/strict';
import { loadIndex, voteFooter, resolveFooter } from '../../../shared/clientScan/text.mjs';
import { needsServer, NO_TITLE_ESCAPE } from './fastScan.js';
import { createReader, corneliusTensor, CORN_SIZE, packRecBatch, FOOTER_HEIGHTS } from '../../../shared/clientScan/pipeline.mjs';
import { strongNumbers } from '../../../shared/clientScan/text.mjs';

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
  const reader = createReader({ ort, cornelius, rec, chars, index, refineCorners: opts.refine ?? false, clock, settle: opts.settle ?? true });
  // settle: the legacy stillness window (opt-in in production) stays covered.
  reader.__env = { ort, cornelius, rec, chars, index, refineCorners: opts.refine ?? false, clock, settle: opts.settle ?? true };
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

test('near-edge: a footer stage past the frame is re-projected WHOLE, never read partially (review B1)', async () => {
  // Bottom-edge card: padded footer rows cross the frame edge. The stage is
  // re-projected with less bottom padding and read in full (6 strips, one
  // batch). A complete read sees both printings (ambiguous -> null); a
  // partial stage (fewer strips) would see only lea 161 and resolve wrongly.
  const corners = [0.32, 0.494, 0.68, 0.494, 0.68, 0.994, 0.32, 0.994];
  const seen = [];
  // (The fake decoder collapses repeated glyphs, so no doubled digits here.)
  const f = fakeReader((calls, n) => { seen.push(n); return calls === 1 ? 'bolt' : n === 5 ? 'lea 161' : '161 147'; }, { corners });
  const index = loadIndex({ names: ['bolt'], canon: {}, excluded: [], sets: ['lea', '2x2'],
    printings: [['id-lea', 'lea', '161'], ['id-2x2', '2x2', '147']], byTitle: { bolt: [0, 1] }, uniqueAlias: {} });
  const reader = createReader({ ...f.reader.__env, index });
  const out = await reader.read(sharpFrame(1000, 1000), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  const r = out.results[0];
  assert.equal(out.candidates[0].near_edge, true, 'fixture must exercise the near-edge path');
  assert.equal(r.ok, false, 'the complete stage is ambiguous: no printing');
  assert.ok(seen.includes(6), 'whole first footer stage read in one batch');
  assert.ok(!seen.includes(5), 'never a 5-strip (partial) first stage');
  assert.ok(out.timings.pad_fit?.some(([st]) => st === 'footer0'), 'stage re-projected with shrunk padding');
});

test('near-edge: a title stage past the top edge is re-projected and read (Roiling Canopy)', async () => {
  // Card top 8 px below a 1000 px frame: padded title strip reaches y<0,
  // the old code skipped both title stages and never read the card.
  const { reader, calls } = fakeReader(() => 'grief', { corners: nearTop(0.004) });
  const out = await reader.read(sharpFrame(1000, 1000), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  assert.equal(out.candidates[0].near_edge, true);
  assert.equal(out.results[0].ok, true, JSON.stringify(out.results[0]));
  assert.equal(calls(), 1);
  assert.ok(out.timings.pad_fit?.some(([st, side]) => st === 'title1' && side === 't'));
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

test('footer: the first modern batch reads the 0.84 row (FRA collector line, Konstrari Charm)', async () => {
  // Only a 6-strip batch (0.84 included) sees the number; the old 5-row
  // batch, the 4-row wide stage and the 2-row retro stage never do.
  const { reader } = fakeReaderIx((calls, n) => (calls === 1 ? 'bolt' : n === 6 ? 'lea 161' : 'lea'));
  const out = await reader.read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  const r = out.results[0];
  assert.equal(r.ok, true, JSON.stringify(r));
  assert.equal(r.scryfallId, 'id-lea');
  assert.equal(r.footer_stage, 0, 'resolved by the first batch, one recognizer call');
});

test('settle window: 65 ms cadence no longer admits later than 105 ms (aliasing removed)', async () => {
  const admitAt = async (step) => {
    let t = 0;
    const { reader, calls } = fakeReader(() => 'grief', { clock: () => t });
    const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4), f = sharpFrame();
    for (let i = 0; i < 20; i++) { await reader.read(f, small, { requireStill: true }); if (calls()) return t; t += step; }
    return Infinity;
  };
  const fast = await admitAt(65), slow = await admitAt(105);
  assert.ok(fast <= slow, `65 ms cadence admitted at ${fast}, 105 ms at ${slow}`);
  assert.ok(fast >= 180, 'never shorter than the 2 x 90 ms window');
});

test('settle window: a geometric abstention (0 recognizer calls) does not demote the next window', async () => {
  // Near-edge card whose title stage cannot fit even unpadded: zero OCR.
  let t = 0;
  // padShrink [] disables the re-projection, so the title stage abstains.
  const f0 = fakeReader(() => 'grief', { clock: () => t, corners: nearTop(0.004) });
  const reader = createReader({ ...f0.reader.__env, padShrink: [] });
  const calls = f0.calls;
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4), f = sharpFrame(1000, 1000);
  const seen = [];
  for (let i = 0; i < 4; i++) { const o = await reader.read(f, small, { requireStill: true }); seen.push(o.candidates[0].status); t += 100; }
  assert.equal(calls(), 0, 'fixture: no recognizer call');
  t += 500;   // long pause (e.g. a server fallback) after the abstention
  const o = await reader.read(f, small, { requireStill: true });
  assert.equal(o.candidates[0].status, 'ready', `still card stays admitted after a no-OCR gap (${seen})`);
});

test('near-edge re-projection never slides a stage off on-card rows (review R1-B1)', async () => {
  // Card bottom 6 px above a 1000 px frame edge. The only fitting bottom
  // padding (<= 25%) would move the footer rows up onto the text box and off
  // the true collector line: the stage must abstain, not re-project.
  const corners = [0.32, 0.4975, 0.68, 0.4975, 0.68, 0.9975, 0.32, 0.9975];
  const { reader } = fakeReaderIx((calls) => (calls === 1 ? 'bolt' : 'lea 161'), { corners });
  const out = await reader.read(sharpFrame(1000, 1000), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  const r = out.results[0];
  assert.equal(out.candidates[0].near_edge, true);
  assert.equal(r.ok, false, 'no printing from a stage that lost on-card rows');
  assert.equal(r.near_edge_partial?.stage, 'footer0');
});

// Round 2 (#8): a previous card's deferral must not follow a new presentation.
test('reset()/no-card clear lastDeferred: a new presentation gets its cheap first look again (R2-8)', async () => {
  for (const boundary of ['reset', 'probe-null']) {
    const { reader, calls, setPresent } = fakeReaderIx((_, batch) => (batch === 2 ? 'bolt' : 'zz'));
    const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
    for (let i = 0; i < 2; i++) await reader.read(sharpFrame(), small, { requireStill: true });
    const a = await reader.read(sharpFrame(), small, { requireStill: true });
    assert.equal(a.results[0].deferred, true, 'fixture: first read defers');
    if (boundary === 'reset') reader.reset();
    else { setPresent(false); assert.equal(await reader.probe(small, 4, 200, 280), null); setPresent(true); }
    for (let i = 0; i < 2; i++) await reader.read(sharpFrame(), small, { requireStill: true });
    const c0 = calls();
    const b = await reader.read(sharpFrame(), small, { requireStill: true });
    assert.equal(b.results[0].deferred, true, `${boundary}: new presentation defers its deep stages again`);
    assert.equal(calls() - c0, 2, `${boundary}: title + first footer batch only`);
  }
});

test('footer rescue stage: taller rows + wide 0.84 prove what the first batch clipped (R2-2)', async () => {
  // Call 1 title, 2 first footer batch, 3 tall (6), 4 wide (4 strips), 5
  // retro (2), 6 must be the 6-strip rescue batch.
  const { reader } = fakeReaderIx((calls, n) => (calls === 1 ? 'bolt' : calls === 6 && n === 6 ? 'lea 161' : 'zz'));
  const r = (await reader.read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4))).results[0];
  assert.equal(r.ok, true, JSON.stringify(r));
  assert.equal(r.scryfallId, 'id-lea');
  assert.equal(r.footer_stage, 4, 'resolved by the rescue batch, after every other stage');
});

test('footer rescue stage: a misread number is never snapped to the nearest indexed one (R2-2)', async () => {
  // Rescue reads 'lea 169' (index has lea 161 / 2x2 117): no printing.
  const { reader } = fakeReaderIx((calls) => (calls === 1 ? 'bolt' : calls === 6 ? 'lea 169' : 'zz'));
  const r = (await reader.read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4))).results[0];
  assert.equal(r.ok, false, JSON.stringify(r));
});

test('tall stage: 0.025 rows read right after the first batch prove a clipped number; a conflicting read still abstains', async () => {
  // Call 1 title, 2 first batch (0.030), 3 = the tall batch (0.025, 6 strips).
  const { reader } = fakeReaderIx((calls, n) => (calls === 1 ? 'bolt' : calls === 3 && n === 6 ? 'lea 161' : 'zz'));
  const r = (await reader.read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4))).results[0];
  assert.equal(r.ok, true, JSON.stringify(r));
  assert.equal(r.scryfallId, 'id-lea');
  assert.equal(r.footer_stage, 1);
  // First batch reads a strong but unresolved '127/505' (set total fails the
  // set check); tall reads 'lea 161'; 127 is another indexed printing of the
  // title: conflicting evidence abstains, and tall was reached (Astra repro).
  const ix = loadIndex({ names: ['bolt'], canon: {}, excluded: [], sets: ['lea', '2x2'],
    printings: [['id-lea', 'lea', '161'], ['id-2x2', '2x2', '127']], byTitle: { bolt: [0, 1] }, uniqueAlias: {} });
  const seen = [];
  const c0 = fakeReader((calls, n) => { seen.push([calls, n]); return calls === 1 ? 'bolt' : calls === 2 ? '127/505' : calls === 3 && n === 6 ? 'lea 161' : 'zz'; });
  const q = (await createReader({ ...c0.reader.__env, index: ix }).read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4))).results[0];
  assert.ok(seen.some(([c, n]) => c === 3 && n === 6), 'tall batch ran');
  assert.equal(q.ok, false, JSON.stringify(q));
  // And never snaps: tall 'lea 169' (no such printing) proves nothing.
  const s0 = fakeReader((calls, n) => (calls === 1 ? 'bolt' : calls === 3 && n === 6 ? 'lea 169' : 'zz'));
  const q2 = (await createReader({ ...s0.reader.__env, index: ix }).read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4))).results[0];
  assert.equal(q2.ok, false, JSON.stringify(q2));
});

test('recognizer packing: LUT + reused buffers are bit-identical to the reference, padding stays 0 (R2-5)', () => {
  const REC_H = 48;
  const strip = (w, seed) => { const d = new Uint8Array(w * REC_H * 3); for (let i = 0; i < d.length; i++) d[i] = (i * 31 + seed * 17) & 255; return { data: d, w, h: REC_H }; };
  const ref = (strips, idx, W) => {
    const plane = REC_H * W, data = new Float32Array(idx.length * 3 * plane);
    idx.forEach((si, n) => { const s = strips[si], rw = Math.min(W, s.w);
      for (let y = 0; y < REC_H; y++) for (let x = 0; x < rw; x++) { const p = (y * s.w + x) * 3, o = n * 3 * plane + y * W + x;
        data[o] = s.data[p + 2] / 127.5 - 1; data[o + plane] = s.data[p + 1] / 127.5 - 1; data[o + 2 * plane] = s.data[p] / 127.5 - 1; } });
    return data;
  };
  // Same shape twice with different (narrower) content: stale values from the
  // first pack must not survive in the padding.
  const a = [strip(401, 1), strip(380, 2)], b = [strip(200, 3), strip(150, 4)];
  const pa = Float32Array.from(packRecBatch(a, [0, 1], 401));
  assert.deepEqual(pa, ref(a, [0, 1], 401));
  const pb = packRecBatch(b, [0, 1], 401);
  assert.deepEqual(Float32Array.from(pb), ref(b, [0, 1], 401));
  for (let v = 0; v < 256; v++) assert.ok(Object.is(Math.fround(v / 127.5 - 1), Float32Array.of(v / 127.5 - 1)[0]));
});

test('resizeLanczos3 fast path is bit-identical to the reference (R2-4)', async () => {
  const { resizeLanczos3, resizeLanczos3Reference } = await import('../../../shared/clientScan/imaging.mjs');
  for (const [W, H, ch] of [[640, 360, 4], [517, 911, 3], [384, 384, 4]]) {
    const src = new Uint8ClampedArray(W * H * ch);
    for (let i = 0; i < src.length; i++) src[i] = (i * 2654435761 >>> 24) ^ ((i / ch / W) | 0);
    assert.deepEqual(resizeLanczos3(src, W, H, ch, 384, 384), resizeLanczos3Reference(src, W, H, ch, 384, 384), `${W}x${H}x${ch}`);
    assert.deepEqual(resizeLanczos3(src, W, H, ch, 384, 384), resizeLanczos3Reference(src, W, H, ch, 384, 384), 'cached taps: same again');
  }
});

test('duplicate decoded frames never count as settle evidence and are never read (R2-6)', async () => {
  let t = 0;
  const { reader, calls } = fakeReader(() => 'grief', { clock: () => t });
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4), f = sharpFrame();
  // One decoded frame (id 7) observed 5 times over 400 ms: never admitted.
  for (let i = 0; i < 5; i++) { const o = await reader.read(f, small, { requireStill: true, frameId: 7 }); t += 100; if (i) assert.equal(o.candidates[0].duplicate, true); }
  assert.equal(calls(), 0, 'the same frame repeated is not a settled window');
  // Distinct frames: normal 180 ms window.
  for (let id = 8; id < 11; id++) { await reader.read(f, small, { requireStill: true, frameId: id }); t += 100; }
  assert.ok(calls() > 0);
  // Without frame ids (no rVFC: iOS < 15.4, old Android) behaviour is unchanged.
  let t2 = 0; const b = fakeReader(() => 'grief', { clock: () => t2 });
  for (let i = 0; i < 3; i++) { await b.reader.read(f, small, { requireStill: true }); t2 += 100; }
  assert.ok(b.calls() > 0, 'no frameId: timer path admits as before');
});

test('adaptive settle is OFF by default and, when on, only ever admits sooner, on clean distinct frames (R2-1)', async () => {
  const admitAt = async (opts, frames) => {
    let t = 0;
    const f0 = fakeReader(() => 'grief', { clock: () => t });
    const reader = createReader({ ...f0.reader.__env, ...opts });
    const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
    for (let i = 0; i < 20; i++) { await reader.read(frames(i), small, { requireStill: true, frameId: i }); if (f0.calls()) return t; t += 60; }
    return Infinity;
  };
  const sharpF = () => sharpFrame();
  const off = await admitAt({}, sharpF), on = await admitAt({ fastSettle: true }, sharpF);
  assert.ok(off >= 180, `default window unchanged (${off})`);
  assert.ok(on >= 120 && on < off, `flag on: shorter for clean frames (${on} vs ${off})`);
  // Without frame ids the fast path never applies.
  let t = 0; const g = fakeReader(() => 'grief', { clock: () => t });
  const r2 = createReader({ ...g.reader.__env, fastSettle: true });
  for (let i = 0; i < 3; i++) { await r2.read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4), { requireStill: true }); t += 60; }
  assert.equal(g.calls(), 0, 'no frameId -> conservative window (120 ms of 60 ms passes is not enough)');
});

test('edge sides are reported for a directional hint; admission unchanged (R2-9)', async () => {
  const { reader, calls } = fakeReader(() => 'grief', { corners: nearTop(0.001) });
  const out = await reader.read(sharpFrame(1000, 1000), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  assert.equal(out.candidates[0].status, 'touches frame edge');
  assert.deepEqual(out.candidates[0].edge_sides, ['top']);
  assert.equal(calls(), 0);
});

test('title rescue gate: measured always, gated only when every crop is flat and no title, behind env.titleGate (R2-3/7)', async () => {
  const { flatTitleCrops } = await import('../../../shared/clientScan/pipeline.mjs');
  assert.equal(flatTitleCrops([], [10, 12]), true);
  assert.equal(flatTitleCrops([], [10, 80]), false, 'one contrasty crop keeps the rescue');
  assert.equal(flatTitleCrops([{ name: 'x' }], [1, 1]), false, 'a title candidate keeps its rescue');
  assert.equal(flatTitleCrops([], []), false);
  const { reader } = fakeReader(() => 'zz');
  const o = await reader.read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4));
  assert.equal(o.timings.title_range.length, 2, 'crop contrast recorded for shadow calibration');
  assert.notEqual(o.timings.title_gated, 1, 'contrasty crops are never gated');
});

test('footer rescue never resolves against a conflicting strong number read on the same frame (Damn DRC 89 vs MH2 80, R2-2)', async () => {
  // First batch: '080/505' (number 80, total too high for the set check);
  // rescue: '089/59' (strong 89). Old candidate proved drc 89 -> wrong.
  const f = fakeReader((calls, n) => (calls === 1 ? 'damn' : calls === 2 ? '080/505' : calls === 6 && n === 6 ? '089/59' : 'zz'));
  const index = loadIndex({ names: ['damn'], canon: {}, excluded: [], sets: ['mh2', 'drc'],
    printings: [['id-mh2', 'mh2', '80'], ['id-drc', 'drc', '89'], ['id-mh2b', 'mh2', '396']], byTitle: { damn: [0, 1, 2] }, uniqueAlias: {} });
  const reader = createReader({ ...f.reader.__env, index });
  const r = (await reader.read(sharpFrame(), new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4))).results[0];
  assert.notEqual(r.scryfallId, 'id-drc', 'never the misread printing');
  assert.equal(r.ok, false, JSON.stringify(r));
});

test('rescue never pre-empts wide or retro (R1-S2/R2-S2); its evidence blocks conflicting identities (R1-B3)', async () => {
  const ix2 = () => loadIndex({ names: ['bolt'], canon: {}, excluded: [], sets: ['lea', '2x2'],
    printings: [['id-lea', 'lea', '161'], ['id-2x2', '2x2', '147']], byTitle: { bolt: [0, 1] }, uniqueAlias: {} });
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  // Order: 1 title, 2 first batch, 3 tall (6), 4 wide (4), 5 retro (4), 6 rescue (6).
  // Wide says 2x2 147: proved there (stage 2), rescue never runs.
  const f1 = fakeReader((calls, n) => (calls === 1 ? 'bolt' : calls === 4 && n === 4 ? '2x2 147' : calls === 6 && n === 6 ? 'lea 161' : 'zz'));
  const r1 = (await createReader({ ...f1.reader.__env, index: ix2() }).read(sharpFrame(), small)).results[0];
  assert.equal(r1.scryfallId, 'id-2x2'); assert.equal(r1.footer_stage, 2); assert.equal(f1.calls(), 4);
  // Retro says 147 (copyright line): proved there (stage 3), rescue never runs.
  const f2 = fakeReader((calls, n) => (calls === 1 ? 'bolt' : calls === 5 && n === 4 ? 'wizards 147' : n === 6 && calls === 6 ? 'lea 161' : 'zz'));
  const r2 = (await createReader({ ...f2.reader.__env, index: ix2() }).read(sharpFrame(), small)).results[0];
  assert.equal(r2.scryfallId, 'id-2x2'); assert.equal(r2.footer_stage, 3); assert.equal(f2.calls(), 5, 'no rescue call on a retro success');
  // Rescue reads two identities of one set: ambiguous, never a pick.
  const ixLea = loadIndex({ names: ['bolt'], canon: {}, excluded: [], sets: ['lea'],
    printings: [['id-lea', 'lea', '161'], ['id-lea2', 'lea', '147']], byTitle: { bolt: [0, 1] }, uniqueAlias: {} });
  const f3 = fakeReader((calls, n) => (calls === 1 ? 'bolt' : calls === 6 && n === 6 ? 'lea 161 147' : 'zz'));
  const r3 = (await createReader({ ...f3.reader.__env, index: ixLea }).read(sharpFrame(), small)).results[0];
  assert.equal(r3.ok, false, JSON.stringify(r3));
});

test('fingerprint frame ids (no rVFC / stale clock): a frozen stream is never settle evidence (R1-B2)', async () => {
  let t = 0;
  const { reader, calls } = fakeReader(() => 'grief', { clock: () => t });
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4), f = sharpFrame();
  for (let i = 0; i < 6; i++) { await reader.read(f, small, { requireStill: true, frameId: 'pabc' }); t += 100; }
  assert.equal(calls(), 0, 'same pixels 6 times: never admitted');
  for (const id of ['p1', 'p2', 'p3']) { await reader.read(f, small, { requireStill: true, frameId: id }); t += 100; }
  assert.ok(calls() > 0, 'distinct frames admit on the normal window');
  // Fingerprints never enable fast settle.
  let t2 = 0; const g = fakeReader(() => 'grief', { clock: () => t2 });
  const r2 = createReader({ ...g.reader.__env, fastSettle: true });
  for (const id of ['a', 'b', 'c']) { await r2.read(sharpFrame(), small, { requireStill: true, frameId: id }); t2 += 60; }
  assert.equal(g.calls(), 0);
});

test('switching frame-id source (rVFC number <-> pixel fingerprint) restarts the settle window (R2-B2)', async () => {
  let t = 0;
  const { reader, calls } = fakeReader(() => 'grief', { clock: () => t });
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4), f = sharpFrame();
  t = 100; await reader.read(f, small, { requireStill: true, frameId: 7 });
  t = 200; await reader.read(f, small, { requireStill: true, frameId: 8 });
  t = 400; await reader.read(f, small, { requireStill: true, frameId: 8 });
  t = 701; const o = await reader.read(f, small, { requireStill: true, frameId: 'pfrozen' });
  assert.notEqual(o.candidates[0].status, 'ready'); assert.equal(calls(), 0);
  t = 801; await reader.read(f, small, { requireStill: true, frameId: 9 });
  assert.equal(calls(), 0, 'reverse switch restarts too');
});

test('deferred pooled proof settles at v1\'s retro stage, before rescue (R3-S2)', async () => {
  const ix = loadIndex({ names: ['bolt'], canon: {}, excluded: [], sets: ['lea', '2x2'], printings: [['id-lea', 'lea', '161'], ['id-2x2', '2x2', '161']], byTitle: { bolt: [0, 1] }, uniqueAlias: {} });
  let t = 0, frame = 0, call = 0; const batches = [];
  const f = fakeReader((_c, n) => { call++; batches.push(n); return call === 1 ? 'bolt' : frame === 0 ? 'lea' : call === 6 ? '2x2 161' : n === 4 && call === 5 ? 'zz' : '161'; }, { clock: () => t });
  const reader = createReader({ ...f.reader.__env, index: ix });
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4), full = sharpFrame();
  let first; for (t = 0; t <= 200; t += 100) first = await reader.read(full, small, { requireStill: true, frameId: t });
  assert.equal(first.results[0].deferred, true);
  frame = 1; call = 0; batches.length = 0;
  const r = (await reader.read(full, small, { requireStill: true, frameId: t })).results[0];
  assert.equal(r.scryfallId, 'id-lea');
  assert.deepEqual(batches, [2, 6, 6, 4, 4], 'v1 batches + tall; retro now reads 4 rows; rescue never runs');
});

test('no settling by default: a sharp still card is read on its first auto frame; blur/clip still gate', async () => {
  const { reader, calls } = fakeReader(() => 'grief', { settle: false });
  const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);
  const a = await reader.read(sharpFrame(), small, { requireStill: true });
  assert.equal(a.results[0]?.ok, true, JSON.stringify(a.candidates[0]));
  assert.ok(calls() > 0);
  // Production default (no env.settle): same.
  const f = fakeReader(() => 'grief');
  const env = { ...f.reader.__env }; delete env.settle;
  const b = await createReader(env).read(sharpFrame(), small, { requireStill: true });
  assert.equal(b.results[0]?.ok, true);
});

test('first footer batch and tall stage never sample identical crops (no self-corroboration)', () => {
  // strongNumbers treats a number read in 2 strips as corroborated. If the
  // tall stage re-read the first batch's exact crops, one read would count
  // twice (Astra review, perf/collector-first-read).
  assert.notEqual(FOOTER_HEIGHTS.first, FOOTER_HEIGHTS.tall);
  assert.equal(FOOTER_HEIGHTS.first, 0.030);
  // The mechanism the guard protects against: a lone read is not strong,
  // the same text twice is.
  assert.equal(strongNumbers(['U 0221']).has('221'), false);
  assert.equal(strongNumbers(['U 0221', 'U 0221']).has('221'), true);
});
