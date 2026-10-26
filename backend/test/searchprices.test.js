// Add Cards search: sidecar results arrive price-less; prices must come from
// card_cache first, then the real API (upstream:true) for never-priced cards.
// Run: `node test/searchprices.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-searchprices-${process.pid}.db`);
const db = require('../src/db');
const { normalizeCard } = require('../src/scryfallApi');
const { cacheNormalizedCards } = require('../src/utils/cardCache');
const { mergeSearchPrices } = require('../src/utils/searchPrices');

const mk = (id, extra = {}) => ({
  id, name: 'Sol Ring', set: 'c21', set_name: 'C21', collector_number: id.slice(0, 3), rarity: 'uncommon',
  lang: 'en', type_line: 'Artifact', image_uris: { normal: 'x' }, ...extra,
});
const A = '4cbc6901-6a4a-4d0a-83ea-7eefa3b35021';
const B = '5cbc6901-6a4a-4d0a-83ea-7eefa3b35022';

(async () => {
  await db.initDb();
  await cacheNormalizedCards([normalizeCard(mk(A, { prices: { usd: '1.71', usd_foil: '4.00' } }))]);
  const page = [normalizeCard(mk(A)), normalizeCard(mk(B, { finishes: ['foil'], released_at: '2024-01-01' }))];
  assert.strictEqual(page[0].price_trend, null, 'sidecar row is price-less');
  assert.deepStrictEqual(page[1].finishes, ['foil']);
  assert.strictEqual(page[1].released_at, '2024-01-01');

  const calls = [];
  const res = await mergeSearchPrices(page, {
    db,
    bulkFetchByIdentifier: async (rows, opts) => {
      calls.push({ rows, opts });
      return { cards: [normalizeCard(mk(B, { prices: { usd: null, usd_foil: '9.50' } }))] };
    },
    cacheCards: cacheNormalizedCards,
  });
  assert.deepStrictEqual(res, { cached: 1, fetched: 1 });
  assert.strictEqual(page[0].price_trend, 1.71, 'cached quote merged');
  assert.strictEqual(page[1].price_holofoil, 9.5, 'upstream quote merged');
  assert.strictEqual(calls.length, 1);
  assert.deepStrictEqual(calls[0].rows, [{ id: 'mtg-' + B }], 'only the never-priced card goes upstream');
  assert.deepStrictEqual(calls[0].opts, { upstream: true }, 'pinned to the real API');
  const row = await db.get('SELECT price_holofoil FROM card_cache WHERE id = ?', ['mtg-' + B]);
  assert.strictEqual(row.price_holofoil, 9.5, 'fetched quote cached for next time');

  // Second run: everything is cached, so upstream is never asked again.
  const again = [normalizeCard(mk(A)), normalizeCard(mk(B))];
  const res2 = await mergeSearchPrices(again, { db, bulkFetchByIdentifier: async () => { throw new Error('no upstream'); } });
  assert.deepStrictEqual(res2, { cached: 2, fetched: 0 });

  // Upstream failure leaves prices null (UI shows a dash), never throws.
  const C = '6cbc6901-6a4a-4d0a-83ea-7eefa3b35023';
  const fail = [normalizeCard(mk(C))];
  await mergeSearchPrices(fail, { db, bulkFetchByIdentifier: async () => { throw new Error('down'); } });
  assert.strictEqual(fail[0].price_trend, null);
  console.log('searchprices: ok');
  process.exit(0);
})().catch(e => { console.error(e); process.exit(1); });
