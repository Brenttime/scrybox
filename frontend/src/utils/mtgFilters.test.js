import test from 'node:test';
import assert from 'node:assert/strict';
import { cardColors, cardIdentity, cardTypes, cardSupertypes, cardSubtypes, matchColors, matchMtgFilters, mvBucket, sortRarities } from './mtgFilters.js';

const elf = { types: ['Green'], color_identity: ['Green'], subtypes: ['Legendary', 'Creature', 'Elf', 'Druid'], cmc: 2, rarity: 'Rare' };
const golgari = { types: '["Black","Green"]', color_identity: '["Black","Green"]', subtypes: '["Instant"]', cmc: 3, rarity: 'Uncommon' };
const rock = { types: [], color_identity: [], subtypes: ['Artifact'], cmc: 8, rarity: 'Common' };

test('parses facets from stored row shapes', () => {
  assert.deepEqual(cardColors(elf), ['G']);
  assert.deepEqual(cardColors(golgari), ['B', 'G']);
  assert.deepEqual(cardColors(rock), ['C']);
  assert.deepEqual(cardIdentity(golgari), ['B', 'G']);
  assert.deepEqual(cardTypes(elf), ['Creature']);
  assert.deepEqual(cardSupertypes(elf), ['Legendary']);
  assert.deepEqual(cardSubtypes(elf), ['Elf', 'Druid']);
});

test('colour modes', () => {
  assert.ok(matchColors(['B', 'G'], ['G'], 'any'));
  assert.ok(!matchColors(['B', 'G'], ['G'], 'exact'));
  assert.ok(matchColors(['B', 'G'], ['G', 'B'], 'exact'));
  assert.ok(!matchColors(['B', 'G'], ['G'], 'within'));
  assert.ok(matchColors(['C'], ['G', 'C'], 'within'));
});

test('combined filter', () => {
  assert.ok(matchMtgFilters(elf, { colors: ['G'], cardTypes: ['Creature'], subtypes: ['Elf'], manaValues: ['2'] }));
  assert.ok(!matchMtgFilters(golgari, { identity: ['G'] }), 'BG does not fit a mono-G identity');
  assert.ok(matchMtgFilters(rock, { identity: ['G'] }), 'colourless fits any identity');
  assert.ok(matchMtgFilters(rock, { manaValues: ['7+'] }));
  assert.ok(!matchMtgFilters(elf, { supertypes: ['Snow'] }));
  assert.equal(mvBucket(null), null);
});

test('rarities sort by rank', () => {
  assert.deepEqual(sortRarities(['Rare', 'Common', 'Mythic', 'Uncommon']), ['Common', 'Uncommon', 'Rare', 'Mythic']);
});
