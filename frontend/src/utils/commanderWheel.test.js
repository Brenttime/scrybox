import test from 'node:test';
import assert from 'node:assert/strict';
import { familyCodes, masterSets, filterMasterSets, commanderQuery, dedupeCommanders, pickWinner, buildSlices, landingRotation, deckNameFor } from './commanderWheel.js';

const tree = [
  { set_code: 'fin', name: 'Final Fantasy', release_date: '2025-06-13', children: [
    { code: 'fic', name: 'Final Fantasy Commander', type: 'commander' },
    { code: 'tfin', name: 'Final Fantasy Tokens', type: 'token' },
    { set_code: 'fca', name: 'Final Fantasy: Through the Ages', children: [{ code: 'pfca', name: 'deep' }] },
  ] },
  { set_code: 'mbc', name: 'Mystery Booster Commander Edition', release_date: '2025-07-01', children: [] },
  { set_code: 'tdm', name: 'Some Tokens', release_date: '2025-01-01', children: [] },
];

test('familyCodes walks every descendant', () => {
  assert.deepEqual(familyCodes(tree[0]), ['fin', 'fic', 'fca', 'pfca']);
});

test('masterSets: newest first, token-only families dropped', () => {
  const m = masterSets(tree);
  assert.deepEqual(m.map(f => f.code), ['mbc', 'fin']);
  assert.equal(filterMasterSets(m, 'through the ages')[0].code, 'fin');
  assert.equal(filterMasterSets(m, 'fic')[0].code, 'fin');
});

test('commanderQuery ORs every set and keeps reprint printings', () => {
  assert.equal(commanderQuery(['fin', 'fic']), 'is:commander -is:funny (e:fin or e:fic) unique:prints');
  assert.equal(commanderQuery(['mbc']), 'is:commander -is:funny e:mbc unique:prints');
  assert.equal(commanderQuery(['bad code;']), '');
});

test('dedupeCommanders prefers the in-set printing', () => {
  const rows = [
    { name: 'Atraxa', set_id: 'one', image_url: 'x', released_at: '2023' },
    { name: 'Atraxa', set_id: 'fca', image_url: 'y', released_at: '2025' },
    { name: 'Cloud', set_id: 'fin', image_url: 'z' },
  ];
  const out = dedupeCommanders(rows, ['fca', 'fin']);
  assert.equal(out.length, 2);
  assert.equal(out[0].set_id, 'fca');
});

test('winner is always on the wheel and slices cap at 24', () => {
  const pool = Array.from({ length: 200 }, (_, i) => ({ name: `c${i}` }));
  const w = pickWinner(pool, () => 0.999);
  assert.equal(w, 199);
  const { slices, winnerSlice } = buildSlices(pool, w);
  assert.equal(slices.length, 24);
  assert.equal(slices[winnerSlice].name, 'c199');
  assert.equal(pickWinner([], Math.random), -1);
});

test('landingRotation puts the slice centre under the top pointer', () => {
  for (const [idx, n] of [[0, 24], [5, 24], [2, 3]]) {
    const r = landingRotation(100, idx, n, 6, 0.5);
    assert.ok(r >= 100 + 5 * 360);
    const centre = (idx + 0.5) * (360 / n);
    assert.ok(Math.abs(((r + centre) % 360)) < 1e-9 || Math.abs(((r + centre) % 360) - 360) < 1e-9);
  }
});

test('deckNameFor uses the front face', () => {
  assert.equal(deckNameFor({ name: 'Cloud // Strife' }), 'Cloud (Wheel)');
});
