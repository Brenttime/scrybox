import assert from 'node:assert';
import { buildCardListText, cardListLine } from './cardList.js';
// The shared builder the server ports byte-for-byte — the import guard below is the real
// drift detector: a change to one that is not made to the other fails here.
import { buildCardListText as sharedBuild, cardListLine as sharedLine } from '../../../shared/cardListText.js';

const cards = [
  { quantity: 4, name: 'Lightning Bolt', set_id: 'jud', number: '124' },
  { quantity: 1, name: 'The Legend of Yangchen // Avatar Yangchen', set_id: 'tla', number: '27' },
  { quantity: 2, name: 'Basic Snow Island' }, // no set/number — imported without cache
  { quantity: 1, name: 'Daily Bugle Newspaper', set_code: 'MSC', collector_number: '749' }, // deck-card spelling
];

assert.strictEqual(cardListLine(cards[0], 'plain'), '4 Lightning Bolt');
assert.strictEqual(cardListLine(cards[0], 'detailed'), '4 Lightning Bolt (JUD) 124');
assert.strictEqual(cardListLine(cards[1], 'detailed'), '1 The Legend of Yangchen // Avatar Yangchen (TLA) 27');
assert.strictEqual(cardListLine(cards[2], 'detailed'), '2 Basic Snow Island', 'missing set/number degrades to name');
assert.strictEqual(cardListLine(cards[3], 'detailed'), '1 Daily Bugle Newspaper (MSC) 749');
assert.strictEqual(cardListLine({ quantity: 0, name: 'X' }, 'plain'), '', 'nonpositive rows are omitted');
assert.strictEqual(
  buildCardListText([{ quantity: 0, name: 'X' }, { quantity: 2, name: 'Y' }], 'plain'),
  '2 Y',
  'nonpositive rows do not create blank export lines'
);

// ManaBox's own documented "most standard format" (manabox.app/guides/decks/
// import-export/ and .../collection/import-export/ — the MTGA format). This is
// the pattern the detailed export must stay byte-identical to.
//   4 Tarmogoyf
//   3 Verdant Catacombs (MH2) 260
//   2 Surgical Extraction
const manaboxDoc = [
  { quantity: 4, name: 'Tarmogoyf' },
  { quantity: 3, name: 'Verdant Catacombs', set_id: 'mh2', number: '260' },
  { quantity: 2, name: 'Surgical Extraction' },
];
assert.strictEqual(
  buildCardListText(manaboxDoc, 'detailed'),
  '4 Tarmogoyf\n3 Verdant Catacombs (MH2) 260\n2 Surgical Extraction',
  'detailed export must match the ManaBox documented MTGA format'
);

const plain = buildCardListText(cards, 'plain');
assert.strictEqual(plain.split('\n').length, 4);
assert.ok(plain.startsWith('4 Lightning Bolt\n'), 'plain line one');
assert.ok(!plain.includes('('), 'plain has no set codes');

const detailed = buildCardListText(cards, 'detailed');
assert.ok(detailed.includes('1 Daily Bugle Newspaper (MSC) 749'));
assert.strictEqual(buildCardListText([], 'plain'), '', 'empty list exports nothing');

// The two implementations must stay byte-identical for every style.
for (const style of ['plain', 'detailed']) {
  assert.strictEqual(buildCardListText(cards, style), sharedBuild(cards, style), `shared copy drift (${style})`);
  cards.forEach(c => assert.strictEqual(cardListLine(c, style), sharedLine(c, style)));
}

console.log('cardList self-check passed');
