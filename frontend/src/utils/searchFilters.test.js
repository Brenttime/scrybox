import { test } from 'node:test';
import assert from 'node:assert/strict';
import { compileSearchFilters as c, countSearchFilters as n, withFilters } from './searchFilters.js';

test('empty filters compile to nothing', () => {
  assert.equal(c({}), '');
  assert.equal(n({}), 0);
});
test('colour any / exact / colourless', () => {
  assert.equal(c({ colors: ['R'] }), 'c>=r');
  assert.equal(c({ colors: ['U', 'R'] }), '(c>=u or c>=r)');
  assert.equal(c({ colors: ['R', 'U'], colorMode: 'exact' }), 'c=ur');
  assert.equal(c({ colors: ['C'] }), 'c=c');
  assert.equal(c({ colors: ['C'], colorMode: 'exact' }), 'c=c');
});
test('identity is within (WUBRG order)', () => {
  assert.equal(c({ identity: ['B', 'W', 'U'] }), 'id<=wub');
});
test('types, subtypes, mana value, rarity, set', () => {
  assert.equal(c({ types: ['Creature'] }), 't:creature');
  assert.equal(c({ subtypes: ['Elf', 'Dragon'] }), '(t:elf or t:dragon)');
  assert.equal(c({ mv: ['3'] }), 'mv=3');
  assert.equal(c({ mv: ['0', '7+'] }), '(mv=0 or mv>=7)');
  assert.equal(c({ rarities: ['mythic'] }), 'r:mythic');
  assert.equal(c({ sets: ['ltr', 'mh3'] }), '(s:ltr or s:mh3)');
});
test('finish, language, price', () => {
  assert.equal(c({ finishes: ['foil'] }), 'is:foil');
  assert.equal(c({ finishes: ['bogus'] }), '');
  assert.equal(c({ languages: ['ja'] }), 'lang:ja');
  assert.equal(c({ minUsd: '5', maxUsd: '20' }), 'usd>=5 usd<=20');
  assert.equal(c({ minUsd: 'x', maxUsd: '' }), '');
});
test('combination and count', () => {
  const f = { colors: ['R'], types: ['Creature'], subtypes: ['Dragon'], rarities: ['mythic'], minUsd: '1' };
  assert.equal(c(f), 'c>=r t:creature t:dragon r:mythic usd>=1');
  assert.equal(n(f), 5);
  assert.equal(withFilters('', c(f)), c(f));
  assert.equal(withFilters(' shivan ', 'r:mythic'), 'shivan r:mythic');
});
test('injection-safe values', () => {
  assert.equal(c({ sets: ['ltr) or (t:x'] }), 's:ltrortx');
  assert.ok(!c({ sets: ['ltr) or (t:x'] }).includes(')'));
});
