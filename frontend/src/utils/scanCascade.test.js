// Set-agnostic identification cascade (2026-10-07): physical footer keys,
// collision groups (an original and its reprint-sheet twin, e.g. The List /
// Mystery Booster 2 'plst' numbers like 'ODY-129'), the old-frame 'N/T'
// copyright line, and the user picker when the footer cannot decide.
// Every test here fails on the code before this change.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  loadIndex, physicalKey, physicalTwins, resolveFooter, resolveFooterGroup, retroNumber, strongNumbers, voteFooter, voteFooterGroup,
} from '../../../shared/clientScan/text.mjs';
import { createReader, CORN_SIZE, strongConflict, retroLineNumber } from '../../../shared/clientScan/pipeline.mjs';
import { needsServer, choiceHits, choiceKey, choiceLabel } from './fastScan.js';

// Real printings (Scryfall, 2026-10): Diabolic Tutor has ody 129 and The List
// ODY-129 (identical physical footer '129/350'); Twisted Image som 50 / plst
// SOM-50 / a25 75.
const SETS = ['ody', 'plst', 'som', 'a25', 'm12', 'sld', 'pmei', 'mh2', 'h2r', 'drc', 'arb', 'nph'];
const ix = () => loadIndex({
  names: ['diabolic tutor', 'twisted image', 'promo thing', 'grief', 'damn', 'grizzled leotau', 'leotau grizalho', 'surgical extraction'], canon: { 6: 'grizzled leotau' }, excluded: [], sets: SETS,
  printings: [
    ['id-ody', 'ody', '129'], ['id-plst-ody', 'plst', 'ODY-129'], ['id-m12', 'm12', '92'],
    ['id-som', 'som', '50'], ['id-plst-som', 'plst', 'SOM-50'], ['id-a25', 'a25', '75'],
    ['id-pmei', 'pmei', '2020-1'], ['id-sld', 'sld', 'IFIYW-3'],
    // Set sizes as printed (the N/T total check): ODY 350, SOM 249, M12 249.
    ['f-ody', 'ody', '350'], ['f-som', 'som', '249'], ['f-m12', 'm12', '249'],
    // Review R1 probes (real numbers): Grief mh2 87 / h2r 7, Damn mh2 80 /
    // drc 89 (DRC stops at 184), Grizzled Leotau arb 69 + plst ARB-69 with a
    // unique printed alias, Surgical Extraction sld 2003.
    ['id-grief-mh2', 'mh2', '87'], ['id-grief-h2r', 'h2r', '7'],
    ['id-damn-mh2', 'mh2', '80'], ['id-damn-drc', 'drc', '89'], ['f-drc', 'drc', '184'], ['f-mh2', 'mh2', '303'],
    ['id-leo-arb', 'arb', '69'], ['id-leo-plst', 'plst', 'ARB-69'],
    ['id-se-sld', 'sld', '2003'], ['id-se-nph', 'nph', '74'],
  ],
  byTitle: { 'diabolic tutor': [0, 1, 2], 'twisted image': [3, 4, 5], 'promo thing': [6, 7],
    grief: [11, 12], damn: [13, 14], 'grizzled leotau': [17, 18], 'surgical extraction': [19, 20] },
  uniqueAlias: { 'grizzled leotau': [['leotau grizalho', 18]] },
});

test('physicalKey: a <SET>-<N> number of another known set prints that set\'s footer; nothing else changes', () => {
  const look = new Set(SETS);
  assert.deepEqual([physicalKey('plst', 'ODY-129', look).set, physicalKey('plst', 'ODY-129', look).num], ['ody', '129']);
  assert.deepEqual([physicalKey('plst', 'SOM-050', look).set, physicalKey('plst', 'SOM-050', look).num], ['som', '50']);
  // Year-numbered promos and unknown prefixes stay their own set.
  assert.equal(physicalKey('pmei', '2020-1', look).set, 'pmei');
  assert.equal(physicalKey('sld', 'IFIYW-3', look).set, 'sld');
  assert.equal(physicalKey('ody', '129', look).raw, '129');
});

test('a footer shared by an original and its reprint-sheet twin is a collision group, never one printing', () => {
  const x = ix();
  // Old code: resolveFooter returned 0 (ody 129) -> the Diabolic Tutor bug.
  assert.equal(resolveFooter(x, 'diabolic tutor', [], ['129'], strongNumbers(['129/350'])), null);
  assert.equal(resolveFooter(x, 'diabolic tutor', ['ody'], ['129']), null);
  assert.deepEqual(resolveFooterGroup(x, 'diabolic tutor', [], ['129'], strongNumbers(['129/350'])), [0, 1]);
  assert.deepEqual(resolveFooterGroup(x, 'diabolic tutor', ['ody'], ['129']), [0, 1]);
  assert.deepEqual(physicalTwins(x, 'diabolic tutor', 1), [0, 1]);
  // A different footer of the same title still proves exactly one printing.
  assert.equal(resolveFooter(x, 'diabolic tutor', ['m12'], ['92']), 2);
  // Two physical footers named at once: ambiguity, as before.
  assert.equal(resolveFooterGroup(x, 'diabolic tutor', [], ['129', '92']), null);
  // The multi-frame vote counts twins as ONE candidate and returns the group.
  assert.deepEqual(voteFooterGroup(x, 'diabolic tutor', [['129 x'], ['129/350']]), [0, 1]);
  assert.equal(voteFooter(x, 'diabolic tutor', [['129 x'], ['129/350']]), null);
});

test('strongConflict: a footer twin is the same evidence, not a conflict', () => {
  const x = ix();
  assert.equal(strongConflict(x, 'diabolic tutor', 0, ['129/350', '129/350']), false);
  assert.equal(strongConflict(x, 'diabolic tutor', 0, ['129/350', '092/249', '92']), true);
});

test('old-frame copyright line: the collector number is N of a trailing N/T, never T or a year', () => {
  // Old code returned '249' (the set total) for this line.
  assert.equal(retroNumber('s of the Coast LLC 50/249'), '50');
  assert.equal(retroLineNumber('s of the Coast LLC 50/249'), '50');
  assert.equal(retroLineNumber('LC50/249'), '50');
  assert.equal(retroLineNumber('50/249'), '50');
  assert.equal(retroLineNumber('TM & (c) 1993-2010'), null);
  assert.equal(retroLineNumber('4/3'), null, 'a P/T box is not a collector line');
  assert.equal(retroLineNumber('Wizards of the Coast 408'), '408');
});

// Pipeline with a fake recognizer: text per recognizer call.
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
      const steps = Math.max(...texts.map(t => t.length)) * 2 + 1, classes = chars.length;
      const data = new Float32Array(n * steps * classes);
      // Odd steps carry characters, even steps blanks (so repeats survive CTC).
      for (let b = 0; b < n; b++) for (let s = 0; s < steps; s++) data[(b * steps + s) * classes + (s % 2 ? chars.indexOf(texts[b][(s - 1) / 2] ?? '') : 0)] = 1;
      return { y: { data, dims: [n, steps, classes] } };
    },
  };
  return { r: createReader({ ort, cornelius, rec, chars, index: ix(), refineCorners: false, clock: () => 0 }), calls: () => calls };
}
function sharpFrame(w = 200, h = 280) {
  const data = new Uint8ClampedArray(w * h * 4);
  for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) { const i = (y * w + x) * 4, v = ((x >> 1) + (y >> 1)) % 2 ? 255 : 0; data[i] = data[i + 1] = data[i + 2] = v; data[i + 3] = 255; }
  return { data, width: w, height: h };
}
const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);

test('pipeline: Diabolic Tutor footer 129/350 asks Original-or-List instead of adding ODY 129', async () => {
  const { r, calls } = reader((c) => (c === 1 ? 'diabolic tutor' : '129/350 c'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.ok, false, JSON.stringify(res));
  assert.equal(res.error, 'printing needs a choice');
  assert.deepEqual(res.choices.map(c => c.scryfallId), ['id-ody', 'id-plst-ody']);
  assert.equal(calls(), 2, 'the cascade stops at the first stage that proves the footer');
});

test('pipeline: an old-frame card proves its footer from the copyright line N/T (Twisted Image)', async () => {
  // Calls: 1 title, 2 first batch, 3 tall, 4 wide, 5 retro (4 rows).
  const { r } = reader((c, n) => (c === 1 ? 'twisted image' : c === 5 && n === 4 ? 'of the coast llc 50/249' : c < 5 ? 'izzy' : 'zz'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.error, 'printing needs a choice', JSON.stringify(res));
  assert.equal(res.via, 'title+collector (retro frame)');
  assert.deepEqual(res.choices.map(c => c.scryfallId), ['id-som', 'id-plst-som']);
});

test('pipeline: a footer with no twin still adds directly', async () => {
  const { r } = reader((c) => (c === 1 ? 'diabolic tutor' : 'm12 092/249'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.ok, true); assert.equal(res.scryfallId, 'id-m12');
});

test('a choice result never goes to the server and becomes one stable tray row', () => {
  const out = { candidates: [{ eligible: true }], results: [{ ok: false, error: 'printing needs a choice', choices: [{ scryfallId: 'a' }, { scryfallId: 'b' }] }] };
  assert.equal(needsServer(out, { autoPass: true }), false);
  assert.equal(needsServer(out, { autoPass: false }), false);
  const cards = [{ id: 'mtg-b', set_name: 'The List' }, { id: 'mtg-a', set_name: 'Odyssey' }];
  const hits = choiceHits([{ ok: false, choices: cards }]);
  assert.equal(hits.length, 1);
  assert.equal(hits[0].card.id, choiceKey(['mtg-a', 'mtg-b']));
  assert.equal(choiceHits([{ ok: false, choices: [...cards].reverse() }])[0].card.id, hits[0].card.id);
  assert.equal(choiceLabel(cards), 'The List or Odyssey?');
  assert.equal(choiceHits([{ ok: false, choices: [cards[0]] }]).length, 0, 'one candidate is not a choice');
});

// Astra review R1 (each reproduced a wrong printing on the first candidate).
test('R1-B2: a unique printed alias with a footer twin is not a proof', async () => {
  const { r } = reader(() => 'leotau grizalho');
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.notEqual(res.ok, true, JSON.stringify(res));
});
for (const [title, line, why] of [['damn', '89/303', 'B5: set total'], ['surgical extraction', '© 2003/2010', 'B4: year range'], ['grief', '7/5', 'B3: P/T box'], ['grief', 'draw 7', 'B3: rules digits']]) {
  test(`R1-${why}: '${line}' on the added old-frame rows never proves ${title}`, async () => {
    const { r } = reader((c, n, b) => (c === 1 ? title : c === 5 && n === 4 && b >= 2 ? line : 'zz'));
    const res = (await r.read(sharpFrame(), small)).results[0];
    assert.notEqual(res.ok, true, JSON.stringify(res));
    assert.equal(res.choices, undefined);
  });
}
test('R1-B5: an N/T copyright line still resolves in a set that runs to #T', async () => {
  const { r } = reader((c, n) => (c === 1 ? 'damn' : c === 5 && n === 4 ? 'of the coast 80/303' : 'zz'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.scryfallId, 'id-damn-mh2', JSON.stringify(res));
});

// Astra review R2: validated old-frame numbers stay constraints for later
// stages; padded N/T normalizes; a total belongs to its own number.
test('R2-NEW-B1: a validated retro number vetoes a later different footer (80/303 + 89/184, then drc 089)', async () => {
  const rows = ['of the coast 80/303', 'of the coast 89/184'];
  const { r } = reader((c, n, b) => (c === 1 ? 'damn' : c === 5 && n === 4 && b >= 2 ? rows[b - 2] : c === 6 ? 'drc 089' : 'zz'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.notEqual(res.ok, true, JSON.stringify(res));
});
test('R2-NEW-B1: a total-filtered retro number still conflicts inside the retro stage (80/505 + 89/184)', async () => {
  const rows = ['of the coast 80/505', 'of the coast 89/184'];
  const { r } = reader((c, n, b) => (c === 1 ? 'damn' : c === 5 && n === 4 && b >= 2 ? rows[b - 2] : 'zz'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.notEqual(res.ok, true, JSON.stringify(res));
});
test('R2-NEW-S1: a zero-padded copyright N/T (050/249) proves the footer', async () => {
  const { r } = reader((c, n, b) => (c === 1 ? 'twisted image' : c === 5 && n === 4 && b >= 2 ? 'of the coast 050/249' : 'zz'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.error, 'printing needs a choice', JSON.stringify(res));
});

// Astra review R3: held numbers are stored per frame (also with no other
// footer text) and expire with their frame.
// Script per recognizer call: title (1 call: exact, high confidence), then
// footer stages first(6) tall(6) wide(4) retro(4) rescue(6).
function scripted() {
  const script = [];
  const drv = reader((c, n, b) => (script[c - 1] ? script[c - 1](n, b) : ''));
  return { drv, push: (...fns) => script.push(...fns) };
}
test('R3-B1: retro-only constraints are kept (no other footer text) and veto later frames of the same card', async () => {
  const rows = ['of the coast 80/303', 'of the coast 89/184'];
  const { drv, push } = scripted();
  push(() => 'damn', () => '', () => '', () => '', (n, b) => (b >= 2 ? rows[b - 2] : ''), () => '');
  const f = sharpFrame();
  const first = (await drv.r.read(f, small)).results[0];
  assert.deepEqual(first.held, ['80', '89']);
  for (let k = 0; k < 2; k++) {
    push(() => 'damn', ...Array.from({ length: 5 }, () => (n, b) => (b === 0 ? '089' : '')));
    const res = (await drv.r.read(f, small)).results[0];
    assert.notEqual(res.ok, true, `frame ${k + 2}: ${JSON.stringify(res)}`);
  }
});
test('R3-S1: held numbers expire with their frame (evidence window)', async () => {
  const rows = ['of the coast 80/303', 'of the coast 89/184'];
  const { drv, push } = scripted();
  push(() => 'damn', () => 'x', () => '', () => '', (n, b) => (b >= 2 ? rows[b - 2] : ''), () => '');
  const f = sharpFrame();
  await drv.r.read(f, small);
  // Six more unresolved frames push the conflicting one out of the window.
  for (let k = 0; k < 6; k++) { push(() => 'damn', () => 'x', () => 'x', () => 'x', () => 'x', () => 'x'); await drv.r.read(f, small); }
  push(() => 'damn', ...Array.from({ length: 5 }, () => (n, b) => (b === 0 ? 'drc 089' : '')));
  const res = (await drv.r.read(f, small)).results[0];
  assert.equal(res.scryfallId, 'id-damn-drc', JSON.stringify(res));
});
