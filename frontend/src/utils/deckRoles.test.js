import assert from 'node:assert/strict';
import { groupByRole, roleOf, ROLE_SLUGS } from './deckRoles.js';

const isLand = c => /Land/.test(c.type_line || '');
const cards = [
  { id: 'a', name: 'Sol Ring', quantity: 1, type_line: 'Artifact' },
  { id: 'b', name: 'Wrath', quantity: 1, type_line: 'Sorcery' },
  { id: 'c', name: 'Forest', quantity: 30, type_line: 'Basic Land' },
  { id: 'd', name: 'Grizzly Bears', quantity: 2, type_line: 'Creature' },
  { id: 'e', name: 'Harmonize', quantity: 1, type_line: 'Sorcery', card_id: 'e2' },
];
const tags = { a: ['ramp'], b: ['removal', 'sweeper'], c: ['ramp'], e2: ['draw'] };
assert.equal(roleOf(cards[1], tags, isLand), 'wipe', 'first matching role wins (wipe before removal)');
assert.equal(roleOf(cards[2], tags, isLand), 'Land', 'lands stay together even when tagged');
assert.equal(roleOf(cards[4], tags, isLand), 'draw', 'collection-style rows use card_id');
const groups = groupByRole(cards, tags, isLand);
assert.deepEqual(groups.map(g => g.name), ['ramp', 'draw', 'wipe', 'Other', 'Land']);
assert.equal(groups.reduce((n, g) => n + g.count, 0), 35, 'counts add up to the deck size');
assert.ok(ROLE_SLUGS.includes('sweeper'));
console.log('# deckRoles.test.js: all assertions passed');
