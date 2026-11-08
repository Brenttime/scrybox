// EDHREC synergy parsing for the deck Synergy map: slug rules and the 0.35
// threshold (string or number synergy, lists without cardviews skipped,
// duplicates keep the highest score). Run: `node test/edhrecsynergy.test.js`
const assert = require('assert');
const { commanderSlug, synergyCards, SYNERGY_THRESHOLD } = require('../src/utils/edhrecSynergy');

assert.strictEqual(SYNERGY_THRESHOLD, 0.35);
assert.strictEqual(commanderSlug("Atraxa, Praetors' Voice"), 'atraxa-praetors-voice');
assert.strictEqual(commanderSlug('Noctis, Prince of Lucis'), 'noctis-prince-of-lucis');
assert.strictEqual(commanderSlug('Lim-Dûl the Necromancer'), 'lim-dul-the-necromancer');
assert.strictEqual(commanderSlug('Esika, God of the Tree // The Prismatic Bridge'), 'esika-god-of-the-tree');
assert.strictEqual(commanderSlug(''), '');

const data = { container: { json_dict: { cardlists: [
  { cardviews: [{ name: 'A', synergy: 0.5 }, { name: 'B', synergy: '0.35' }, { name: 'C', synergy: 0.34 }] },
  { header: 'no views' },
  null,
  { cardviews: [{ name: 'A', synergy: 0.6 }, { name: 'D' }, { synergy: 0.9 }, { name: 'E', synergy: 'x' }] },
] } } };
assert.deepStrictEqual(synergyCards(data), [{ name: 'A', synergy: 0.6 }, { name: 'B', synergy: 0.35 }]);
assert.deepStrictEqual(synergyCards({}), []);
assert.deepStrictEqual(synergyCards(null), []);
assert.deepStrictEqual(synergyCards(data, 0.55).map(c => c.name), ['A']);
console.log('edhrecsynergy ok');
