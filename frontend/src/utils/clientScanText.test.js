import test from 'node:test';
import assert from 'node:assert/strict';
import { needsServer } from './fastScan.js';
import {
  normName, normalizeCollector, ratio, extractTop2, footerNumbers, retroNumber,
  ctcDecode, buildCharset, loadIndex, findCardByOcr, resolveFooter, footerCodes,
  strongNumbers, looksLikeCopyright, voteFooter,
} from '../../../shared/clientScan/text.mjs';

// Expected values come from the cardscan sidecar (server.py / rapidfuzz 3.14)
// run on the same inputs, so the port cannot drift silently.
test('normName matches server.norm_name', () => {
  assert.equal(normName('Nazgûl'), 'nazgul');
  assert.equal(normName('Enchantress\u2019s  Presence'), 'enchantress s presence');
  assert.equal(normName('  Jewel-Eyed Cobra! '), 'jewel eyed cobra');
});

test('normalizeCollector strips zero padding, keeps suffix', () => {
  assert.equal(normalizeCollector('0408'), '408');
  assert.equal(normalizeCollector('096z'), '96z');
  assert.equal(normalizeCollector('000'), '0');
  assert.equal(normalizeCollector('CNS-6'), 'cns-6');
});

test('ratio equals rapidfuzz fuzz.ratio / 100', () => {
  // rapidfuzz.fuzz.ratio values
  assert.equal(Math.round(ratio('mine colapse', 'mine collapse') * 1e4), Math.round(96.0 * 100));
  assert.equal(Math.round(ratio('syr gwyn hero of ashvale', 'syr gwvn her of ashvale') * 1e4) / 1e4, 0.9362); // 93.617
  assert.equal(ratio('abc', 'abc'), 1);
  assert.equal(ratio('abc', 'xyz'), 0);
  // multi-word (>30 chars) LCS path
  const a = 'the ever changing dane and the long name test';
  assert.equal(ratio(a, a), 1);
  assert.equal(Math.round(ratio(a, a.replace('dane', 'dune')) * 1e4) / 1e4, 0.9778);
});

test('extractTop2 returns best two above cutoff', () => {
  const r = extractTop2('grief', ['grief', 'brief', 'griefs', 'xxxx'], 0.6);
  assert.deepEqual(r.map(x => x[0]), [0, 2]);
});

test('footerNumbers keeps the slash rule and rarity suffixes', () => {
  // server._footer_numbers outputs
  assert.deepEqual(footerNumbers(['219/281 U']), ['219']);
  assert.deepEqual(footerNumbers(['0330M']), ['330m', '330']);
  assert.deepEqual(footerNumbers(['80177']), ['177', '8017']);
});

test('retro copyright line: trailing number only, never the year', () => {
  assert.equal(retroNumber('TM & © 2021 Wizards of the Coast 408'), '408');
  assert.equal(retroNumber('TM & © 2021'), null);
  assert.equal(retroNumber('began. Nice ol rock....'), null);
});

test('ctcDecode drops blanks and repeats', () => {
  const chars = buildCharset('a\nb\n');           // ['', 'a', 'b', ' ']
  const steps = [1, 1, 0, 1, 2, 2, 3, 0];          // a a _ a b b ' ' _
  const data = new Float32Array(steps.length * 4);
  steps.forEach((k, t) => { data[t * 4 + k] = 0.9; });
  const { text, conf } = ctcDecode(data, steps.length, 4, chars);
  assert.equal(text, 'aab ');
  assert.ok(Math.abs(conf - 0.9) < 1e-6);
});

// A miniature index in the generator's format.
const ix = loadIndex({
  names: ['mine collapse', 'grief', 'lightning bolt', 'panther idol'],
  canon: { 3: 'mind s eye' },
  excluded: ['treasure'],
  sets: ['mh2', '2x2', 'lea', 'msc'],
  printings: [
    ['id-mine-mh2', 'mh2', '124'], ['id-mine-mh2r', 'mh2', '408'],
    ['id-grief', 'mh2', '87'],
    ['id-bolt-lea', 'lea', '161'], ['id-bolt-2x2', '2x2', '117'], ['id-bolt-2x2b', '2x2', '161'],
  ],
  byTitle: { 'mine collapse': [0, 1], grief: [2], 'lightning bolt': [3, 4, 5] },
  uniqueAlias: {},
});

test('findCardByOcr: exact, fuzzy, excluded, alias to canonical', () => {
  assert.deepEqual(findCardByOcr(ix, 'Mine Collapse'), { name: 'mine collapse', score: 1 });
  assert.equal(findCardByOcr(ix, 'Mine Colapse').name, 'mine collapse');
  assert.equal(findCardByOcr(ix, 'Treasure').name, null);
  assert.equal(findCardByOcr(ix, 'Panther Idol').name, 'mind s eye');
  assert.equal(findCardByOcr(ix, 'Lightning').name, null);
});

test('resolveFooter never guesses between printings', () => {
  // number alone that exists in two sets for this title -> no answer
  assert.equal(resolveFooter(ix, 'lightning bolt', [], ['161']), null);
  // set code disambiguates
  assert.equal(resolveFooter(ix, 'lightning bolt', ['2x2'], ['161']), 5);
  // a unique number is enough
  assert.equal(resolveFooter(ix, 'mine collapse', [], ['408']), 1);
  // two plausible numbers that both match -> no answer
  assert.equal(resolveFooter(ix, 'mine collapse', [], ['408', '124']), null);
  assert.equal(resolveFooter(ix, 'mine collapse', [], ['999']), null);
});

test('footerCodes reads a set code with up to two noise chars', () => {
  assert.deepEqual(footerCodes(ix, ['• 2X2 • EN']), ['2x2']);
  assert.deepEqual(footerCodes(ix, ['0408 R MH2']), []);
});

test('needsServer: fallback rules', () => {
  const card = (ok) => ({ candidates: [{ eligible: true }], results: [{ ok }] });
  assert.equal(needsServer(null, { autoPass: true }), true);
  assert.equal(needsServer({ error: 'x' }, { autoPass: true }), true);
  assert.equal(needsServer(card(true), { autoPass: true }), false);
  assert.equal(needsServer(card(false), { autoPass: true }), true);
  // gate held the card back (moving / edge): auto waits, shutter asks server
  const held = { candidates: [{ eligible: false, status: 'moving' }], results: [] };
  assert.equal(needsServer(held, { autoPass: true }), false);
  assert.equal(needsServer(held, { autoPass: false }), true);
  const none = { candidates: [], results: [] };
  assert.equal(needsServer(none, { autoPass: true }), false);
  assert.equal(needsServer(none, { autoPass: false }), true);
});

// The two wrong printings the first replay produced, as regressions.
test('set-less numbers need a modern footer shape or two agreeing strips', () => {
  const keys = (raws) => [...strongNumbers(raws).keys()];
  assert.deepEqual(keys(['MANENN', '00895093']), []);
  assert.deepEqual(keys(['M2-EM', '32', '312', '@77-300', '7/5']), []);
  assert.deepEqual([...strongNumbers(['080/303 R'])], [['80', 303]]);
  assert.deepEqual([...strongNumbers(['x 408', 'y 408'])], [['408', 0]]);
  assert.deepEqual(keys(['2021 Wizards', '© 2021']), []);
  assert.equal(looksLikeCopyright('TM & © 2021 Wizards of the Coast 408'), true);
  assert.equal(looksLikeCopyright('shaking began. Nice ol rock 3'), false);
});

test('"N/T" only resolves in a set that runs to #T', () => {
  const dm = loadIndex({
    names: ['damn'], canon: {}, excluded: [], sets: ['mh2', 'drc'],
    printings: [['id-mh2', 'mh2', '80'], ['id-drc', 'drc', '89'], ['x', 'drc', '184'], ['y', 'mh2', '492']],
    byTitle: { damn: [0, 1] }, uniqueAlias: {},
  });
  // PP-OCRv5 misread of "080/303" on a real MH2 Damn frame
  const raws = ['089/303'];
  assert.equal(resolveFooter(dm, 'damn', [], footerNumbers(raws), strongNumbers(raws)), null);
  const good = ['080/303'];
  assert.equal(resolveFooter(dm, 'damn', [], footerNumbers(good), strongNumbers(good)), 0);
});

test('voteFooter: multi-frame vote needs 2 frames and one candidate', () => {
  const ix = {
    byTitle: { damn: [0, 1, 2] },
    printings: [['a', 'mh2', '80'], ['b', 'mh2', '403'], ['c', 'sld', '1011']],
    setLookup: new Set(['mh2', 'sld']), setLengths: [3], setRank: new Map([['mh2', 0], ['sld', 1]]),
    setMax: new Map([['mh2', 303], ['sld', 2000]]),
  };
  // Garbled 080/303 on three frames; "303" (the set total) must not vote.
  assert.equal(voteFooter(ix, 'damn', [['0807303', 'M2～N'], ['080305'], ['080/505']]), 0);
  assert.equal(voteFooter(ix, 'damn', [['0807303']]), null);             // one frame
  assert.equal(voteFooter(ix, 'damn', [['080'], ['403 080']]), null);    // two candidates
  assert.equal(voteFooter(ix, 'damn', [['x'], ['y']]), null);
});

test('findCardByOcr: memoized, bounded, and identical to the uncached lookup (R2-11)', async () => {
  const { findCardByOcrUncached } = await import('../../../shared/clientScan/text.mjs');
  const names = ['lightning bolt', 'deception', 'reception', 'grief', 'the masamune', 'x'];
  const ix = loadIndex({ names, canon: {}, excluded: [], sets: [], printings: [], byTitle: {}, uniqueAlias: {} });
  let reads = 0;
  const counted = new Proxy(ix.names, { get(tg, k, r) { if (k === 'length') reads++; return Reflect.get(tg, k, r); } });
  ix.names = counted;
  for (const q of ['Lightnlng Bolt', 'ley Reception', 'The Masamunc', 'X', 'zzzz']) {
    const a = findCardByOcr(ix, q);
    const before = reads;
    const b = findCardByOcr(ix, q);
    assert.equal(reads, before, `second identical lookup must not rescan names (${q})`);
    assert.deepEqual(a, b); assert.deepEqual(a, findCardByOcrUncached(ix, q));
    b.name = 'mutated';
    assert.deepEqual(findCardByOcr(ix, q), a, 'callers cannot poison the memo');
  }
  for (let i = 0; i < 2000; i++) findCardByOcr(ix, 'noise ' + i);
  assert.ok(ix.__findMemo.size <= 512, 'bounded');
});
