import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { deriveDeckRenderData } from './deckRenderData.js';

const srcDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const deckBuilder = readFileSync(join(srcDir, 'components', 'DeckBuilder.jsx'), 'utf8');
const deckRenderData = readFileSync(join(srcDir, 'utils', 'deckRenderData.js'), 'utf8');

const section = (start, end) => {
  const startIndex = deckBuilder.indexOf(start);
  const endIndex = deckBuilder.indexOf(end, startIndex + start.length);
  assert.notEqual(startIndex, -1, `missing section marker: ${start}`);
  assert.notEqual(endIndex, -1, `missing section marker: ${end}`);
  return deckBuilder.slice(startIndex, endIndex);
};

// The list view is one text line per card (image on hover only), so it has
// no thumbnails to defer.
assert.doesNotMatch(section('{/* 1. COMPACT LIST VIEW */}', '{/* 2. VISUAL CARD GRID VIEW */}'), /<CardImage/,
  'list rows are text-only; the image preview is the hover/sheet');
for (const thumbnailSection of [
  section('{/* Search Results list */}', '{/* Deck Cards Header & Display Mode Toggle */}'),
  section('{/* 2. VISUAL CARD GRID VIEW */}', '{/* Right Column: Statistics, Mana Curve & Deck Health */}'),
]) {
  assert.match(thumbnailSection, /<CardImage[\s\S]*?loading="lazy"[\s\S]*?decoding="async"[\s\S]*?\/>/,
    'search, list, and grid thumbnails must defer loading and decode work');
}

// Card details use the same shared inspector as the Collection.
assert.match(deckBuilder, /<CardInspectorModal[\s\S]*?card=\{inspectEntry\}/,
  'deck card details must open the shared CardInspectorModal');

assert.match(deckBuilder, /import \{[^}]*\buseMemo\b[^}]*\} from 'react';/,
  'deck-wide derived data must use React useMemo');
assert.match(deckBuilder, /const deckDerived = useMemo\(\(\) => deriveDeckRenderData\(deckCards\), \[deckCards\]\);/,
  'deck derivations must be memoized only by the stable deck-card array');

const deriveStart = deckRenderData.indexOf('function deriveDeckRenderData(deckCards)');
assert.notEqual(deriveStart, -1, 'missing consolidated deck derivation helper');
const deriveBody = deckRenderData.slice(deriveStart);
assert.equal((deriveBody.match(/for \(const card of deckCards\)/g) || []).length, 1,
  'all deck summaries and groups must be collected in one card pass');
assert.doesNotMatch(deriveBody, /deckCards\.(?:filter|reduce|forEach|map)\(/,
  'deck derivation helper must not add hidden deck-wide array passes');

const cards = [
  { id: 'island', name: 'Island', quantity: 3, supertype: 'Land', subtypes: ['Basic', 'Land', 'Island'], colors: [], cmc: 0 },
  { id: 'bolt-a', name: 'Lightning Bolt', quantity: 2, subtypes: ['Instant'], colors: ['R'], cmc: 1 },
  { id: 'ring', name: 'Sol Ring', quantity: 1, subtypes: ['Artifact'], colors: [], cmc: 1 },
  { id: 'elves', name: 'Llanowar Elves', quantity: 2, subtypes: ['Creature', 'Elf'], colors: ['G'], cmc: 1 },
  { id: 'bolt-b', name: ' Lightning Bolt ', quantity: 1, subtypes: ['Instant'], colors: ['R'], cmc: 1 },
  { id: 'ugin', name: 'Ugin, the Spirit Dragon', quantity: 1, subtypes: ['Planeswalker'], colors: [], cmc: 7 },
];
const derived = deriveDeckRenderData(cards);

assert.deepEqual(derived.supertypeData, [
  { name: 'Land', value: 3 },
  { name: 'Instant', value: 3 },
  { name: 'Artifact', value: 1 },
  { name: 'Creature', value: 2 },
  { name: 'Planeswalker', value: 1 },
], 'pie slices and legend must retain baseline first-seen card-group order');
assert.deepEqual(derived.deckGroups.map(group => ({
  name: group.name,
  count: group.count,
  cardIds: group.cards.map(card => card.id),
})), [
  { name: 'Creature', count: 2, cardIds: ['elves'] },
  { name: 'Planeswalker', count: 1, cardIds: ['ugin'] },
  { name: 'Instant', count: 3, cardIds: ['bolt-a', 'bolt-b'] },
  { name: 'Artifact', count: 1, cardIds: ['ring'] },
  { name: 'Land', count: 3, cardIds: ['island'] },
], 'deck rendering must retain fixed group order, quantities, and card order');
assert.equal(derived.totalDeckCardsCount, 10);
assert.equal(derived.basicLandCount, 3);
assert.deepEqual(derived.manaCurveData, [
  { cost: '0', count: 3 },
  { cost: '1', count: 6 },
  { cost: '2', count: 0 },
  { cost: '3', count: 0 },
  { cost: '4', count: 0 },
  { cost: '5', count: 0 },
  { cost: '6', count: 0 },
  { cost: '7+', count: 1 },
]);
assert.deepEqual(derived.colorLandData, [
  { name: 'Land (Island)', value: 3 },
  { name: 'Red', value: 3 },
  { name: 'Colorless', value: 2 },
  { name: 'Green', value: 2 },
]);
assert.deepEqual(Object.fromEntries(derived.countsByName), {
  island: 3,
  'lightning bolt': 3,
  'sol ring': 1,
  'llanowar elves': 2,
  'ugin, the spirit dragon': 1,
});

const deckCardsSection = section('{/* Deck Cards Header & Display Mode Toggle */}', '{/* Right Column: Statistics, Mana Curve & Deck Health */}');
assert.doesNotMatch(deckCardsSection, /activeDeck\.cards\.filter\(/,
  'GROUP_ORDER rendering must consume pre-bucketed groups instead of filtering once per group');

console.log('PASS: deckRenderPerformance.test.js');
