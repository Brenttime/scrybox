// Printing guess (Brent, 2026-10-24): a proven title whose printing the
// footer cannot split is guessed from the scan context (sets of the last
// added cards), else the regular original; never a blocking picker. Every
// test here fails on the code before this change.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadIndex } from '../../../shared/clientScan/text.mjs';
import { guessPrinting, guessCodes, guessNumbers } from '../../../shared/clientScan/guess.mjs';
import { createReader, CORN_SIZE } from '../../../shared/clientScan/pipeline.mjs';
import { recentSets, guessFromChoices, guessChoiceHit, presenceDecision, notePresenceCommit, canChangePrinting, sentEntryIds } from './fastScan.js';

// Real printings (Scryfall, 2026-10) with the index's appended columns
// [released yyyymmdd, flags: 1 variant, 2 promo/reprint product].
const SETS = ['sos', 'soa', 'plst', 'psos', 'woe', 'mar', 'pw25', 'ody', 'som', 'a25', 'mh2'];
const ix = () => loadIndex({
  names: ['artistic process', 'wildgrowth archaic', 'monstrous rage', 'diabolic tutor', 'twisted image', 'mine collapse'], canon: {}, excluded: [], sets: SETS,
  printings: [
    ['ap-plst', 'plst', 'SOS-108', 20261109, 2], ['ap-sos', 'sos', '108', 20260424, 0],
    ['wa-sos', 'sos', '168', 20260424, 0], ['wa-sos-sc', 'sos', '297', 20260424, 1], ['wa-psos', 'psos', '168p', 20260424, 2],
    ['mr-pw25', 'pw25', '9', 20250801, 2], ['mr-soa45', 'soa', '45', 20260424, 1], ['mr-mar', 'mar', '72', 20260626, 1],
    ['mr-soa110', 'soa', '110', 20260424, 1], ['mr-soa175', 'soa', '175', 20260424, 1], ['mr-woe', 'woe', '142', 20230908, 0],
    ['dt-ody', 'ody', '129', 20011001, 0], ['dt-plst', 'plst', 'ODY-129', 20200901, 2],
    ['ti-som', 'som', '50', 20101001, 0], ['ti-plst', 'plst', 'SOM-50', 20200901, 2], ['ti-a25', 'a25', '75', 20180316, 0],
    ['f-ody', 'ody', '350'], ['f-som', 'som', '249'], ['f-sos', 'sos', '300'], ['f-soa', 'soa', '180'],
    ['mc-135', 'mh2', '135', 20210618, 0], ['mc-408', 'mh2', '408', 20210618, 5],
  ],
  byTitle: { 'artistic process': [0, 1], 'wildgrowth archaic': [2, 3, 4], 'monstrous rage': [5, 6, 7, 8, 9, 10],
    'diabolic tutor': [11, 12], 'twisted image': [13, 14, 15], 'mine collapse': [20, 21] },
  uniqueAlias: {},
});
const id = (x, g) => x.printings[g.pi][0];

test('guess: the scan context picks the original over its List twin (sos 108 over plst SOS-108)', () => {
  const x = ix();
  const g = guessPrinting(x, x.byTitle['artistic process'], { sets: ['sos', 'sos', 'sos'] });
  assert.equal(id(x, g), 'ap-sos'); assert.equal(g.guess, 'context'); assert.equal(g.sure, true);
  // A List context picks the List printing.
  assert.equal(id(x, guessPrinting(x, x.byTitle['artistic process'], { sets: ['plst'] })), 'ap-plst');
  // No context: the non-reprint-sheet original, and it is the only regular one.
  const f = guessPrinting(x, x.byTitle['artistic process'], {});
  assert.equal(id(x, f), 'ap-sos'); assert.equal(f.guess, 'fallback'); assert.equal(f.sure, true);
});

test('guess: inside the context set the regular printing wins, then the lowest number', () => {
  const x = ix();
  assert.equal(id(x, guessPrinting(x, x.byTitle['wildgrowth archaic'], { sets: ['sos'] })), 'wa-sos');
  assert.equal(id(x, guessPrinting(x, x.byTitle['monstrous rage'], { sets: ['soa', 'sos'] })), 'mr-soa45');
});

test('guess: footer evidence beats context (a clean set code, a padded number)', () => {
  const x = ix();
  const g = guessPrinting(x, x.byTitle['monstrous rage'], { footer: ['SOA', 'Its pa'], sets: ['woe', 'woe', 'woe'] });
  assert.equal(id(x, g), 'mr-soa45'); assert.equal(g.guess, 'footer');
  assert.equal(id(x, guessPrinting(x, x.byTitle['monstrous rage'], { footer: ['C 0110'], sets: ['soa'] })), 'mr-soa110');
  // Rules text is not a set code, a year is not a number.
  assert.deepEqual([...guessCodes(x, ['more than', 'SOS·EN', 'soa'])].sort(), ['soa', 'sos']);
  assert.deepEqual([...guessNumbers(['© 2026 Wizards', 'U 0058']).keys()], ['58']);
});

test('guess: only the last 3 adds are context; footer numbers keep the set-total check', () => {
  const x = ix();
  // An older soa add does not count once the last 3 are other sets.
  const g = guessPrinting(x, x.byTitle['monstrous rage'], { sets: ['sos', 'sos', 'sos', 'soa'] });
  assert.equal(g.guess, 'fallback'); assert.equal(id(x, g), 'mr-woe');
  // '110/999': no set runs to 999, so the number is not evidence.
  assert.equal(id(x, guessPrinting(x, x.byTitle['monstrous rage'], { footer: ['110/999'], sets: ['soa'] })), 'mr-soa45');
  assert.equal(id(x, guessPrinting(x, x.byTitle['monstrous rage'], { footer: ['110/180'], sets: ['soa'] })), 'mr-soa110');
  // A copyright year is never a number; denominators are never numbers.
  assert.deepEqual([...guessNumbers(['© 2003/2010', '050/249']).keys()], ['50']);
  assert.deepEqual([...guessNumbers(['050/0249']).keys()], ['50'], 'a padded total is not a number');
});

test('guess: a same-set old-frame twin (mh2 135 / retro 408) is not a first-look guess', () => {
  const x = ix();
  const g = guessPrinting(x, x.byTitle['mine collapse'], { sets: ['mh2', 'mh2', 'mh2'] });
  assert.equal(id(x, g), 'mc-135'); assert.equal(g.sure, false);
  // A same-set variant with the same frame era does not block (sos 168 / 297).
  assert.equal(guessPrinting(x, x.byTitle['wildgrowth archaic'], { sets: ['sos'] }).sure, true);
  // Variant-only sets (soa) are never a first-look guess.
  assert.equal(guessPrinting(x, x.byTitle['monstrous rage'], { sets: ['soa'] }).sure, false);
});

test('guess: a fallback over several regular printings is not sure (first look keeps the cascade)', () => {
  const x = ix();
  const g = guessPrinting(x, x.byTitle['twisted image'], {});
  assert.equal(g.guess, 'fallback'); assert.equal(g.sure, false);
  assert.equal(id(x, g), 'ti-a25', 'newest regular printing first');
});

// Pipeline with a fake recognizer (same harness as scanCascade.test.js).
function reader(textFor, extra = {}) {
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
      for (let b = 0; b < n; b++) for (let s = 0; s < steps; s++) data[(b * steps + s) * classes + (s % 2 ? chars.indexOf(texts[b][(s - 1) / 2] ?? '') : 0)] = 1;
      return { y: { data, dims: [n, steps, classes] } };
    },
  };
  return { r: createReader({ ort, cornelius, rec, chars, index: ix(), refineCorners: false, clock: () => 0, listStamp: false, ...extra }), calls: () => calls };
}
function sharpFrame(w = 200, h = 280) {
  const data = new Uint8ClampedArray(w * h * 4);
  for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) { const i = (y * w + x) * 4, v = ((x >> 1) + (y >> 1)) % 2 ? 255 : 0; data[i] = data[i + 1] = data[i + 2] = v; data[i + 3] = 255; }
  return { data, width: w, height: h };
}
const small = new Uint8ClampedArray(CORN_SIZE * CORN_SIZE * 4);

test('pipeline: footer twins are guessed from context instead of a picker', async () => {
  const { r } = reader((c) => (c === 1 ? 'diabolic tutor' : '129/350 c'));
  const res = (await r.read(sharpFrame(), small, { guess: { sets: ['plst', 'plst'] } })).results[0];
  assert.equal(res.ok, true, JSON.stringify(res));
  assert.equal(res.scryfallId, 'dt-plst'); assert.equal(res.guess, 'context'); assert.equal(res.alts, 2);
  const { r: r2 } = reader((c) => (c === 1 ? 'diabolic tutor' : '129/350 c'));
  const f = (await r2.read(sharpFrame(), small, { guess: { sets: [] } })).results[0];
  assert.equal(f.scryfallId, 'dt-ody'); assert.equal(f.guess, 'fallback');
});

test('pipeline: auto first look guesses after the first footer batch instead of looping', async () => {
  // Title, then an unreadable footer: the old code deferred ('exact printing
  // not resolved') and re-read every stage on the next frames.
  const { r, calls } = reader((c) => (c === 1 ? 'twisted image' : 'zz'));
  const res = (await r.read(sharpFrame(), small, { requireStill: true, frameId: 1, guess: { sets: ['som', 'som', 'a25'] } })).results[0];
  assert.equal(res.ok, true, JSON.stringify(res));
  assert.equal(res.scryfallId, 'ti-som'); assert.equal(res.guess, 'context');
  assert.equal(calls(), 2, 'title + first footer batch only');
});

test('pipeline: a weak fallback on the first look still defers, then the full cascade guesses', async () => {
  const { r } = reader((c) => (c === 1 || c === 3 ? 'twisted image' : 'zz'));
  const r1 = (await r.read(sharpFrame(), small, { requireStill: true, frameId: 1, guess: { sets: [] } })).results[0];
  assert.equal(r1.ok, false, JSON.stringify(r1)); assert.equal(r1.deferred, true, JSON.stringify(r1));
  const r2 = (await r.read(sharpFrame(), small, { requireStill: true, frameId: 2, guess: { sets: [] } })).results[0];
  assert.equal(r2.ok, true, JSON.stringify(r2)); assert.equal(r2.guess, 'fallback'); assert.equal(r2.scryfallId, 'ti-a25');
});

test('pipeline: after a first-look guess the next frame runs every stage once, so a deeper footer proof still wins (review R1-B2)', async () => {
  // Calls: frame 1 title + first batch (guess SOM 50); frame 2 title, first
  // batch, tall (proves A25 75); frame 3 is carried (no OCR).
  const { r, calls } = reader((c) => (c === 1 || c === 3 ? 'twisted image' : c === 5 ? 'a25 075/249' : 'zz'));
  const opts = (frameId) => ({ requireStill: true, frameId, guess: { sets: ['som', 'som', 'som'] } });
  const r1 = (await r.read(sharpFrame(), small, opts(1))).results[0];
  assert.equal(r1.scryfallId, 'ti-som'); assert.equal(r1.guess, 'context');
  const r2 = (await r.read(sharpFrame(), small, opts(2))).results[0];
  assert.equal(r2.scryfallId, 'ti-a25', JSON.stringify(r2)); assert.equal(r2.guess, undefined);
  const { r: q, calls: qc } = reader((c) => (c === 1 || c === 3 ? 'twisted image' : 'zz'));
  await q.read(sharpFrame(), small, opts(1));
  const v = (await q.read(sharpFrame(), small, opts(2))).results[0];
  assert.equal(v.guess, 'context', 'nothing deeper: the guess stands');
  const before = qc();
  const c3 = (await q.read(sharpFrame(), small, opts(3))).results[0];
  assert.equal(c3.cached, true); assert.equal(qc(), before, 'verified once, then carried without OCR');
  assert.ok(calls() >= 5);
});

test('pipeline: a footer-proven printing is never overridden by context', async () => {
  const { r } = reader((c) => (c === 1 ? 'twisted image' : 'a25 075/249'));
  const res = (await r.read(sharpFrame(), small, { guess: { sets: ['som', 'som', 'som'] } })).results[0];
  assert.equal(res.scryfallId, 'ti-a25', JSON.stringify(res)); assert.equal(res.guess, undefined);
});

test('pipeline: a title that starts a longer title gets no title-only guess (replay: Flicker vs Flickering Hound)', async () => {
  // 'monstrous rage' is a prefix of 'monstrous rage tribute' in this index.
  const x = ix();
  x.prefixOfLonger.add('monstrous rage');
  const { r } = reader(() => 'monstrous rage', { index: x });
  for (const frameId of [1, 2, 3]) {
    const res = (await r.read(sharpFrame(), small, { requireStill: true, frameId, guess: { sets: ['soa', 'soa', 'soa'] } })).results[0];
    assert.notEqual(res.ok, true, JSON.stringify(res));
  }
});

test('pipeline: no guess option keeps the old picker', async () => {
  const { r } = reader((c) => (c === 1 ? 'diabolic tutor' : '129/350 c'));
  const res = (await r.read(sharpFrame(), small)).results[0];
  assert.equal(res.error, 'printing needs a choice');
});

test('tray helpers: recent sets, server-choice guess, same-card correction, change-printing button', () => {
  const rows = [{ card: { set_id: 'SOS' } }, { needsChoice: true, card: { set_id: 'x' } }, { card: { set_id: 'soa' } }, { card: { set_id: 'sos' } }, { card: { set_id: 'woe' } }];
  assert.deepEqual(recentSets(rows), ['sos', 'soa', 'sos']);
  const cards = [{ id: 'mtg-p', set_id: 'plst', set_name: 'The List' }, { id: 'mtg-s', set_id: 'sos', set_name: 'Secrets of Strixhaven' }];
  assert.deepEqual(guessFromChoices(cards, ['sos']), { card: cards[1], guess: 'context' });
  assert.deepEqual(guessFromChoices(cards, []), { card: cards[1], guess: 'fallback' });
  const h = guessChoiceHit({ card: { id: 'choose:x' }, choose: cards }, ['plst']);
  assert.equal(h.card.id, 'mtg-p'); assert.equal(h.ok, true); assert.equal(h.alts, 2); assert.equal(h.choose, undefined);
  // Same-presentation bookkeeping, independent of the tray (review R1-B5).
  const committed = new Map();
  notePresenceCommit(committed, 7, { name: "Archaic's Agony" }, { rowKey: 'a', guess: true });
  assert.equal(presenceDecision(committed, { card: { name: 'archaic s agony', id: 'x' } }, 7).action, 'fix', 'a proof after a guess corrects it');
  assert.equal(presenceDecision(committed, { guess: 'context', card: { name: "Archaic's Agony", id: 'y' } }, 7).action, 'repeat');
  assert.equal(presenceDecision(committed, { card: { name: "Archaic's Agony" } }, 8).action, 'add', 'another presentation is another copy');
  notePresenceCommit(committed, 9, { name: 'Twisted Image' }, { rowKey: 'p', guess: false });
  assert.equal(presenceDecision(committed, { guess: 'fallback', card: { name: 'Twisted Image', id: 'mtg-a25' } }, 9).action, 'repeat', 'a guess after a proof is a repeat');
  assert.equal(presenceDecision(committed, { card: { name: 'Twisted Image', id: 'mtg-a25' } }, 9).action, 'add', 'two proofs keep the old id de-dupe');
  assert.equal(presenceDecision(committed, { card: { name: 'Bala Ged Recovery // Bala Ged Sanctuary' } }, 3).action, 'add');
  // The server-choice guess uses the same ranking: newest regular printing.
  const tw = [{ id: 'mtg-o', set_id: 'ody', number: '129', released_at: '2001-10-01' }, { id: 'mtg-l', set_id: 'plst', number: 'ODY-129', set_name: 'The List', released_at: '2020-09-01' }, { id: 'mtg-m', set_id: 'm12', number: '92', released_at: '2011-07-15' }];
  assert.equal(guessFromChoices(tw, []).card.id, 'mtg-m');
  assert.equal(guessFromChoices([...tw].reverse(), []).card.id, 'mtg-m', 'order-independent');
  assert.equal(guessFromChoices(tw, ['woe', 'woe', 'woe', 'ody']).guess, 'fallback', 'only the last 3 adds are context');
  assert.equal(canChangePrinting({ alts: 2 }), true);
  assert.equal(canChangePrinting({ alts: 1 }), false);
  assert.equal(canChangePrinting({}), true, 'server answers: printing count unknown');
  assert.equal(canChangePrinting({ alts: 3, sent: true }), false, 'sent without entry ids');
  assert.equal(canChangePrinting({ alts: 3, sent: true, sentDest: '12' }), false, 'lists hold cards by name');
  assert.equal(canChangePrinting({ alts: 3, sent: true, entryIds: [5] }), true);
  assert.equal(canChangePrinting({ alts: 3, pending: true }), false);
});

test('tray: bulk-add entry ids go to the row whose request entry landed (review R1-S5)', () => {
  const rows = [{ key: 'a', card: { id: 'mtg-x' } }, { key: 'b', card: { id: 'mtg-x' } }];
  // The first copy failed; the second landed with three ids.
  const m = sentEntryIds(rows, [{ card_id: 'mtg-x', id: 7, ids: [7, 8, 9], index: 1 }]);
  assert.deepEqual([...m], [['b', [7, 8, 9]]]);
  // The server-choice guess puts a promo set after the regular printing.
  assert.equal(guessFromChoices([{ id: 'p', set_id: 'pm21', number: '1' }, { id: 'r', set_id: 'm21', number: '200' }], []).card.id, 'r');
});

test('pipeline: a verify read that misses the title verifies nothing (review R2-B2)', async () => {
  const { r, calls } = reader((c) => (c === 1 ? 'twisted image' : 'zz'));
  const opts = (frameId) => ({ requireStill: true, frameId, guess: { sets: ['som', 'som', 'som'] } });
  assert.equal((await r.read(sharpFrame(), small, opts(1))).results[0].guess, 'context');
  await r.read(sharpFrame(), small, opts(2));            // title miss
  const n = calls();
  const third = (await r.read(sharpFrame(), small, opts(3))).results[0];
  assert.ok(calls() > n, 'the next frame reads again'); assert.ok(!third?.cached);
  r.resetCache?.();
});
