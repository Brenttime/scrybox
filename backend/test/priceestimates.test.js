// Unpriced printings borrow the cheapest real quote of another printing of the
// same card and are flagged price_estimated; a real quote always replaces it.
// Run: `node test/priceestimates.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-priceestimates-${process.pid}.db`);
const db = require('../src/db');
const { normalizeCard } = require('../src/scryfallApi');
const { cacheNormalizedCards } = require('../src/utils/cardCache');
const { refreshAllEstimates } = require('../src/utils/priceEstimates');
const { getCheapestPrintings, currentPrintingPrice, deckCurrentPrintValues } = require('../src/utils/deckPricing');

const mk = (id, name, prices) => ({
  id, name, set: 'plst', set_name: 'The List', collector_number: id.slice(0, 3), rarity: 'rare',
  lang: 'en', type_line: 'Sorcery', image_uris: { normal: 'x' }, prices,
});
const LIST = '1cbc6901-6a4a-4d0a-83ea-7eefa3b35021';
const CHEAP = '2cbc6901-6a4a-4d0a-83ea-7eefa3b35022';
const DEAR = '3cbc6901-6a4a-4d0a-83ea-7eefa3b35023';
const LONE = '4cbc6901-6a4a-4d0a-83ea-7eefa3b35024';
const row = (id) => db.get('SELECT * FROM card_cache WHERE id = ?', [`mtg-${id}`]);

(async () => {
  await db.initDb();
  await cacheNormalizedCards([
    normalizeCard(mk(LIST, 'Harrow', { usd: null, usd_foil: null })),
    normalizeCard(mk(CHEAP, 'Harrow', { usd: '0.40', usd_foil: '3.00' })),
    normalizeCard(mk(DEAR, 'Harrow', { usd: '2.10' })),
    normalizeCard(mk(LONE, 'Lonely Card', { usd: null })),
  ]);

  let r = await row(LIST);
  assert.strictEqual(r.price_estimated, 1, 'unpriced printing is estimated on write');
  assert.strictEqual(r.price_trend, 0.4, 'estimate = cheapest other printing');
  assert.strictEqual((await row(CHEAP)).price_estimated, 0, 'real quotes are never flagged');
  assert.strictEqual((await row(LONE)).price_estimated, 0, 'no priced sibling, no estimate');
  assert.ok(!((await row(LONE)).price_trend > 0));

  // Totals see the estimate and count it.
  assert.strictEqual(currentPrintingPrice(r), 0.4);
  const totals = deckCurrentPrintValues([{ quantity: 2, current_price: 0.4, price_estimated: 1 }, { quantity: 1, current_price: 2.1 }]);
  assert.deepStrictEqual(totals, { current_printing_value: 2.9, current_unpriced_cards: 0, current_estimated_cards: 2 });

  // The cheapest-printing floor never picks an estimated row.
  const floor = await getCheapestPrintings(db, ['harrow']);
  assert.strictEqual(floor.get('harrow').card_id, `mtg-${CHEAP}`);

  // A cheaper sibling arriving moves the estimate.
  await cacheNormalizedCards([normalizeCard(mk(CHEAP, 'Harrow', { usd: '0.25' }))]);
  assert.strictEqual((await row(LIST)).price_trend, 0.25, 'estimate follows the floor');

  // The printing's own quote replaces the estimate and clears the flag.
  await cacheNormalizedCards([normalizeCard(mk(LIST, 'Harrow', { usd: '1.50' }))]);
  r = await row(LIST);
  assert.strictEqual(r.price_estimated, 0);
  assert.strictEqual(r.price_trend, 1.5);

  // Losing that quote again restores the estimate.
  await cacheNormalizedCards([normalizeCard(mk(LIST, 'Harrow', { usd: null }))]);
  r = await row(LIST);
  assert.strictEqual(r.price_estimated, 1);
  assert.strictEqual(r.price_trend, 0.25);

  // Floor disappears: the estimate is withdrawn by the full sweep.
  await db.run(`UPDATE card_cache SET price_trend = 0, price_normal = NULL, price_holofoil = NULL WHERE id IN (?, ?)`, [`mtg-${CHEAP}`, `mtg-${DEAR}`]);
  await refreshAllEstimates(db);
  r = await row(LIST);
  assert.strictEqual(r.price_estimated, 0, 'no floor left, no estimate');
  assert.strictEqual(r.price_trend, 0);

  console.log('priceestimates: ok');
  process.exit(0);
})().catch(e => { console.error(e); process.exit(1); });
