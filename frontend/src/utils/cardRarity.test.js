import assert from 'node:assert';
import {
  getRarityRank,
  getRarityTier,
  getRarityBadgeLabel,
  isPremiumRarity,
} from './cardRarity.js';

const scryfallRarities = ['common', 'uncommon', 'rare', 'mythic', 'special', 'bonus'];
assert.deepStrictEqual(
  scryfallRarities.map(getRarityRank),
  [1, 2, 3, 4, 5, 6],
  'all and only Scryfall rarities have the canonical sort order'
);
assert.deepStrictEqual(
  scryfallRarities.map(getRarityTier),
  ['common', 'uncommon', 'rare', 'top', 'top', 'top']
);
assert.deepStrictEqual(
  scryfallRarities.map(getRarityBadgeLabel),
  ['COM', 'UNC', 'RARE', 'MYTHIC', 'SPECIAL', 'BONUS']
);
assert.deepStrictEqual(
  scryfallRarities.filter(isPremiumRarity),
  ['mythic', 'special', 'bonus']
);

// Exact matching is deliberate: provider-era composite labels must never be
// interpreted as current MTG rarity values.
for (const unsupported of ['rare holo', 'ultra rare', 'promo', 'illustration rare', 'secret rare']) {
  assert.strictEqual(getRarityRank(unsupported), 0, `${unsupported} is not a Scryfall rarity`);
}
assert.strictEqual(getRarityRank(' MYTHIC '), 4, 'normalization trims and lowercases');
assert.strictEqual(getRarityBadgeLabel(''), '—');

console.log('PASS: cardRarity.test.js');

// Proxy copies display as their own rarity and restore on unmark.
{
  const { displayRarity, getRarityBadgeStyle } = await import('./cardRarity.js');
  const { resolveCardPrice } = await import('./resolveCardPrice.js');
  const { stackKey } = await import('./collectionStack.js');
  assert.strictEqual(displayRarity({ rarity: 'rare', is_proxy: 1 }), 'Proxy');
  assert.strictEqual(displayRarity({ rarity: 'Proxy', base_rarity: 'rare', is_proxy: 0 }), 'rare');
  assert.strictEqual(displayRarity({ rarity: 'mythic' }), 'mythic');
  assert.strictEqual(getRarityBadgeLabel('Proxy'), 'PROXY');
  assert.ok(getRarityBadgeStyle('Proxy').background);
  const c = { printing: 'Normal', price_normal: 5, price_trend: 4 };
  assert.strictEqual(resolveCardPrice({ ...c, is_proxy: 1 }, 'Normal'), 0);
  assert.strictEqual(resolveCardPrice({ ...c, is_proxy: 0 }, 'Normal'), 5);
  assert.notStrictEqual(stackKey({ card_id: 'x', is_proxy: 1 }), stackKey({ card_id: 'x', is_proxy: 0 }), 'proxy never stacks with a real copy');
}
