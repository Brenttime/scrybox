// Proxy copies: valued at $0 and shown with rarity 'Proxy', without touching
// card_cache prices. Run: `node test/proxy.test.js`.
const assert = require('assert');
const { resolveCardPrice, parseCardRow, applyProxyRarity } = require('../src/utils/priceHelpers');

const row = { printing: 'Normal', price_normal: 12.5, price_holofoil: 30, price_trend: 11, rarity: 'rare', quantity: 2 };

assert.strictEqual(resolveCardPrice({ ...row, is_proxy: 0 }), 12.5, 'real copy keeps its price');
assert.strictEqual(resolveCardPrice({ ...row, is_proxy: 1 }), 0, 'proxy copy is worth $0');
assert.strictEqual(resolveCardPrice({ ...row, printing: 'Holofoil', is_proxy: 1 }), 0, 'proxy foil is worth $0');

const rows = [{ ...row, is_proxy: 0 }, { ...row, is_proxy: 1 }, { ...row, printing: 'Holofoil', quantity: 1, is_proxy: 0 }];
const total = rows.reduce((s, r) => s + r.quantity * resolveCardPrice(r), 0);
assert.strictEqual(total, 2 * 12.5 + 30, 'collection total excludes proxy copies');

const proxied = parseCardRow({ ...row, is_proxy: 1, types: '[]', subtypes: '[]' });
assert.strictEqual(proxied.rarity, 'Proxy');
assert.strictEqual(proxied.base_rarity, 'rare');
assert.strictEqual(proxied.price_normal, 12.5, 'stored price untouched');
const real = parseCardRow({ ...row, is_proxy: 0, types: '[]', subtypes: '[]' });
assert.strictEqual(real.rarity, 'rare');
assert.strictEqual(applyProxyRarity({ ...proxied, is_proxy: 0 }).rarity, 'rare', 'unmarking restores rarity');
assert.strictEqual(parseCardRow({ rarity: 'rare', types: '[]', subtypes: '[]' }).rarity, 'rare', 'card_cache rows unchanged');
assert.strictEqual(parseCardRow({ rarity: 'rare', types: '[]', subtypes: '[]' }).base_rarity, undefined);

console.log('proxy tests passed');
