import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { parseHTML } from 'linkedom';
import { buildBookmarklet, scrapeTcgOrderPage } from './tcgBookmarklet.js';

// A real TCGplayer Order History page (store.tcgplayer.com/myaccount/orderhistory,
// captured 2026-10-15, addresses redacted). Item links there are name slugs;
// the product id only appears in the thumbnail URL.
const html = readFileSync(new URL('./fixtures/tcg-order-history.html', import.meta.url), 'utf8');
const { document } = parseHTML(html);

const got = scrapeTcgOrderPage(document);
assert.deepStrictEqual(got.orders, ['840C56DE-131D59-3D8EF', '8E81A76B-036B69-DD871', '0EF27E7A-563CB6-129E9']);
assert.strictEqual(got.lines.length, 11, 'every item row on the page');
assert.ok(got.lines.every((l) => /^\d+$/.test(l.tcgplayer_product_id) && l.tcgplayer_product_id !== '0'));

const first = got.lines[0];
assert.deepStrictEqual(
  [first.tcgplayer_product_id, first.name, first.set, first.quantity, first.condition, first.is_foil, first.price_cents, first.order],
  ['4517', 'Koskun Falls', 'Homelands', 1, 'Moderately Played', false, 1447, '840C56DE-131D59-3D8EF'],
);
// Lines are attributed to their own order, not the first one on the page.
assert.strictEqual(got.lines.filter((l) => l.order === '840C56DE-131D59-3D8EF').length, 3);
assert.strictEqual(new Set(got.lines.map((l) => l.order)).size, 3);
// The page's one foil line ("Near Mint Foil") is read as foil, condition kept.
const foils = got.lines.filter((l) => l.is_foil);
assert.strictEqual(foils.length, 1);
assert.strictEqual(foils[0].condition, 'Near Mint');
// Quantity comes from the QUANTITY column, price from the PRICE column.
assert.ok(got.lines.every((l) => l.quantity >= 1 && Number.isInteger(l.price_cents) && l.price_cents > 0));

// The serialised bookmarklet runs the same reader end to end on the real page.
const body = decodeURIComponent(buildBookmarklet('https://scrybox.test').slice('javascript:'.length));
let opened = null;
const win = { open: (u) => { opened = u; return {}; } };
new Function('document', 'window', 'location', 'alert', body)(document, win, { href: 'https://store.tcgplayer.com/myaccount/orderhistory' }, () => { throw new Error('alerted'); });
assert.ok(opened && opened.startsWith('https://scrybox.test/#tcgimport='));
const payload = JSON.parse(decodeURIComponent(opened.split('#tcgimport=')[1]));
assert.strictEqual(payload.lines.length, 11);
assert.strictEqual(payload.orders.length, 3);

// An empty page alerts instead of opening Scrybox with nothing.
const empty = parseHTML('<html><body><p>Order History</p></body></html>').document;
let alerted = '';
opened = null;
new Function('document', 'window', 'location', 'alert', body)(empty, win, { href: 'x' }, (m) => { alerted = m; });
assert.ok(alerted.includes('0 cards') && opened === null);
console.log('tcgHistoryPage ok');
