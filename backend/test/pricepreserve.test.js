// Price-less card responses (the local scryfall sidecar serves bulk data with no
// `prices` object) must never zero a cached printing's market price.
// Run: `node test/pricepreserve.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-pricepreserve-${process.pid}.db`);
const db = require('../src/db');
const { normalizeCard } = require('../src/scryfallApi');
const { cacheNormalizedCards } = require('../src/utils/cardCache');

const base = {
  id: '4cbc6901-6a4a-4d0a-83ea-7eefa3b35021', name: 'Sol Ring', set: 'c21',
  set_name: 'Commander 2021', collector_number: '263', rarity: 'uncommon', lang: 'en',
  type_line: 'Artifact', image_uris: { normal: 'x' },
};

(async () => {
  await db.initDb();
  const priced = normalizeCard({ ...base, prices: { usd: '1.71', usd_foil: '4.00' } });
  assert.strictEqual(priced.price_source, 'scryfall');
  await cacheNormalizedCards([priced]);

  const bare = normalizeCard({ ...base });
  assert.strictEqual(bare.price_source, null, 'price-less response has no price source');
  assert.strictEqual(bare.price_trend, null);
  await cacheNormalizedCards([bare]);

  let row = await db.get(`SELECT * FROM card_cache WHERE id = ?`, ['mtg-' + base.id]);
  assert.strictEqual(row.price_normal, 1.71, 'sidecar refresh kept the market price');
  assert.strictEqual(row.price_holofoil, 4);
  assert.strictEqual(row.price_trend, 1.71);
  assert.strictEqual(row.price_source, 'scryfall');

  // A real quote still replaces the old one, including a genuine drop to null.
  await cacheNormalizedCards([normalizeCard({ ...base, prices: { usd: '2.05', usd_foil: null } })]);
  row = await db.get(`SELECT * FROM card_cache WHERE id = ?`, ['mtg-' + base.id]);
  assert.strictEqual(row.price_normal, 2.05);
  assert.strictEqual(row.price_holofoil, null);

  console.log('pricepreserve: ok');
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });
