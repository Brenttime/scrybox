// TCGplayer bookmarklet import: product-id lines resolve by identity only.
const assert = require('assert');
const os = require('os');
const path = require('path');
process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-tcgpage-${process.pid}.db`);
const { cleanTcgPageLines, resolveTcgPageLines, importedTcgOrders, recordTcgOrders } = require('../src/utils/marketplaceOrders');

(async () => {
  const clean = cleanTcgPageLines([
    { tcgplayer_product_id: '111', name: 'Sol Ring', quantity: 3, condition: 'Lightly Played', is_foil: true, price_cents: 150 },
    { tcgplayer_product_id: 'abc', name: 'bad id' },
    { tcgplayer_product_id: 222, quantity: -4, price_cents: 'x' },
    null,
  ]);
  assert.strictEqual(clean.length, 2);
  assert.deepStrictEqual(clean[0], { tcgplayer_product_id: '111', name: 'Sol Ring', quantity: 3, price_cents: 150, is_foil: true, condition: 'Lightly Played', order: null });
  assert.strictEqual(clean[1].quantity, 1, 'nonsense quantity falls back to 1');
  assert.strictEqual(clean[1].price_cents, null);
  assert.strictEqual(clean[1].condition, null, 'no condition is not invented');

  const asked = [];
  const db = { all: async (sql, params) => { assert.deepStrictEqual(params, [111, 222, 333]); return [{ id: 'mtg-aaaa', pid: 111, language: 'English' }]; } };
  const scryGet = async (url) => {
    asked.push(url);
    if (url.endsWith('/222')) return { data: { id: 'bbbb' } };
    const e = new Error('nf'); e.response = { status: 404 }; throw e;
  };
  const lines = await resolveTcgPageLines([
    { tcgplayer_product_id: '111', name: 'Sol Ring', quantity: 2 },
    { tcgplayer_product_id: '222', name: 'Signet' },
    { tcgplayer_product_id: '333', name: 'Booster Box' },
    { tcgplayer_product_id: '111', name: 'Sol Ring', quantity: 1 },
  ], { db, scryGet });
  assert.deepStrictEqual(asked, ['/cards/tcgplayer/222', '/cards/tcgplayer/333'], 'cached ids never hit Scryfall');
  assert.deepStrictEqual(lines.map((l) => [l.scryfall_id, l.junk]), [['aaaa', false], ['bbbb', false], [null, true], ['aaaa', false]]);
  assert.strictEqual(lines.__cardCopies, 4);
  assert.deepStrictEqual(lines.__unmatched, ['Booster Box']);
  // Lines keep their order number through resolution (needed for dedupe).
  const withOrder = await resolveTcgPageLines([{ tcgplayer_product_id: '111', order: '840c56de-131d59-3d8ef' }], { db: { all: async () => [{ id: 'mtg-aaaa', pid: 111 }] }, scryGet });
  assert.strictEqual(withOrder[0].order, '840c56de-131d59-3d8ef');

  // Imported-order ledger: per user, case-insensitive, junk ignored, idempotent.
  const rows = [];
  const fake = {
    run: async (sql, [u, o]) => { if (!rows.some((r) => r.u === u && r.o === o)) rows.push({ u, o }); },
    all: async (sql, [u, ...os]) => rows.filter((r) => r.u === u && os.includes(r.o)).map((r) => ({ order_number: r.o })),
  };
  assert.deepStrictEqual(await recordTcgOrders(2, ['840c56de-131d59-3d8ef', 'garbage', null, '840C56DE-131D59-3D8EF'], fake), ['840C56DE-131D59-3D8EF']);
  await recordTcgOrders(2, ['840C56DE-131D59-3D8EF'], fake);
  assert.strictEqual(rows.length, 1, 'recording twice is a no-op');
  assert.deepStrictEqual([...await importedTcgOrders(2, ['840c56de-131d59-3d8ef', '0EF27E7A-563CB6-129E9'], fake)], ['840C56DE-131D59-3D8EF']);
  assert.strictEqual((await importedTcgOrders(3, ['840C56DE-131D59-3D8EF'], fake)).size, 0, 'another user is unaffected');
  assert.strictEqual((await importedTcgOrders(2, [], fake)).size, 0);
  console.log('tcgpageimport ok');
})().catch((e) => { console.error(e); process.exit(1); });
