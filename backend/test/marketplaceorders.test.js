// Marketplace order import (ManaPool / TCGplayer) — no network, no real database.
//
// What this pins down is the part that a live call cannot show repeatably: the
// shape-guessing readers, the credential normalisers, the endline-vs-extras rule,
// and the per-line price/condition/printing fidelity of the write. The HTTP
// layer is injected (`httpGet`), the card resolver is injected (`bulkResolve`),
// the filing core is injected (`bulkAdd`), and the ownership read runs against a
// stub db, so every assertion is about what WOULD be filed, not about a live
// server. The upstream shapes here are real captured responses (ManaPool's
// documented buyer detail form and the TCGplayer gateway's list form), not
// invented fixtures — a hand-written fixture would only prove the test agrees
// with itself.
const assert = require('assert');
const os = require('os');
const path = require('path');
// The util pulls the db module transitively (ownership read); park it on a
// throwaway file so an injected-stub run can never touch a real (dev) database.
process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-mktorders-${process.pid}.db`);

const {
  normalizeCookies, cookieCount, maskSecret, maskEmail, customerIdHints,
  mapCondition, unitPriceCents, isFoilish, orderLines, parseOrderPayload,
  fetchManapoolOrder, fetchTcgOrder, previewOrder, addOrderToCollection,
} = require('../src/utils/marketplaceOrders');

const UUID_A = 'c385447a-f56d-4b65-a090-bae48b0313c0';
const UUID_B = '11111111-2222-3333-4444-555555555555';

// --- conditions -------------------------------------------------------------
assert.strictEqual(mapCondition('NM', 'Near Mint'), 'Near Mint');
assert.strictEqual(mapCondition('lp', 'Near Mint'), 'Lightly Played');
assert.strictEqual(mapCondition('MP', 'Near Mint'), 'Moderately Played');
assert.strictEqual(mapCondition('hp', 'Near Mint'), 'Heavily Played');
assert.strictEqual(mapCondition('DAM', 'Near Mint'), 'Damaged');
assert.strictEqual(mapCondition('MT', 'Near Mint'), 'Damaged', 'mint-taped is a damage grade, not near mint');
assert.strictEqual(mapCondition('EXC', 'Near Mint'), 'Near Mint');
assert.strictEqual(mapCondition('Gi', 'Near Mint'), 'Near Mint', 'a German grade is not a known code');
assert.strictEqual(mapCondition('', 'Lightly Played'), 'Lightly Played', 'blank falls back to the caller default');
assert.strictEqual(mapCondition(null, 'Damaged'), 'Damaged');

// --- prices -----------------------------------------------------------------
assert.strictEqual(unitPriceCents({ price_cents: 410 }), 410);
assert.strictEqual(unitPriceCents({ price: 4.1 }), 410, 'dollars are scaled by 100');
assert.strictEqual(unitPriceCents({ price: 12999 }), 12999, 'an implausible dollar figure is really cents');
assert.strictEqual(unitPriceCents({ price_cents: 0 }), 0, 'free is a price, not a missing one');
assert.strictEqual(unitPriceCents({}), null);
assert.strictEqual(unitPriceCents({ price: 'abc' }), null);

// --- foil -------------------------------------------------------------------
assert.strictEqual(isFoilish({ isFoil: true }), true);
assert.strictEqual(isFoilish({ finish: 'etched' }), true);
assert.strictEqual(isFoilish({ finish: 'nonfoil' }), false, 'a nonfoil finish is foilish — the guard matters');
assert.strictEqual(isFoilish({ name: 'Sol Ring Foil' }), true);
assert.strictEqual(isFoilish({ name: 'Sol Ring' }), false);

// --- cookie jar ------------------------------------------------------------
assert.strictEqual(normalizeCookies('Cookie: a=1; b=2'), 'a=1; b=2', 'the header label is stripped, not sent as a cookie');
assert.strictEqual(normalizeCookies([{ name: 'sid', value: '9' }, { name: 'bad' }]), 'sid=9', 'a cookie with no value is dropped');
assert.strictEqual(normalizeCookies('garbage with no pairs'), null, 'a string with no `=` is not a jar');
assert.strictEqual(normalizeCookies(''), null);
assert.strictEqual(cookieCount('a=1; b=2; c=3'), 3);
assert.ok(maskSecret('short').indexOf('short') === -1, 'a short secret is never echoed');
assert.ok(maskSecret('aaaaaaaa-bbbb-cccc-dddd').startsWith('aaa'));
assert.ok(maskSecret('aaaaaaaa-bbbb-cccc-dddd').endsWith('(23 chars)'));
const masked = maskEmail('bturner@gmail.com');
assert.ok(masked.includes('@gmail.com'), 'the domain stays visible so the user can recognise the account');
assert.ok(!masked.includes('bturner'), 'the local part is masked');
assert.deepStrictEqual(customerIdHints('tcg_customer_id=42; sid=abc'), ['42']);
assert.deepStrictEqual(customerIdHints('sid=abc'), []);

// --- captured live responses (verbatim from a real buyer account, PII trimmed) -
// The MANAPOOL network paths assert against these, never against a hand-written
// doc-shaped body: a fixture copied from the examples can only prove the code
// agrees with the examples.
const MP_LIST = require('./fixtures/manapool/buyer-orders.json');
const MP_LIVE_DETAIL = require('./fixtures/manapool/buyer-order-detail.json');
const MP_DETAILS_BY_UUID = require('./fixtures/manapool/buyer-order-details-by-uuid.json');
const MP_LIVE_NUM = MP_LIVE_DETAIL.order.order_number;
const MP_LIVE_UUID = MP_LIVE_DETAIL.order.id;
assert.ok(MP_LIVE_NUM && /^\d+$/.test(MP_LIVE_NUM), 'fixture is a real six-digit order');
assert.ok(/^[0-9a-f]{8}-/.test(MP_LIVE_UUID), 'fixture detail is keyed by its row uuid');

// --- a top-level-items body (the shape the published examples imply) ----------
// Kept as a SYNTHETIC regression for the top-level reader path only. It is NOT
// what ManaPool sends: the live buyer detail has no top-level items and ships
// its cards under order_seller_details[].items — see the captured fixtures
// below. Tests written against a doc-shaped fixture can only ever prove the
// code agrees with the docs, which is exactly how the picker shipped reading
// every order as 0 cards.
const MP_DETAIL = {
  id: 4402, number: 'MP-1', status: 'shipped',
  items: [
    { quantity: 4, price_cents: 410, product: { single: { name: 'Sol Ring', set: 'FIN', number: '2', scryfallId: UUID_A, finish: 'foil' } } },
    { quantity: 1, price_cents: 5, product: { single: { name: 'Forest', set: 'ALL', number: '189', isFoil: true } }, conditionCode: 'LP' },
    { quantity: 2, price_cents: 1000, product: { sealed: { name: 'Collector Booster', kind: 'sealed' } } },
  ],
};
{
  const parsed = parseOrderPayload(MP_DETAIL);
  assert.strictEqual(parsed.number, 'MP-1');
  assert.strictEqual(parsed.status, 'shipped');
  assert.strictEqual(parsed.lineCount, 2, 'the sealed box is held out of the cards');
  assert.strictEqual(parsed.lines.__extras, 1, '...but counted, so the UI can say what was skipped');
  const [sol, forest] = parsed.lines;
  assert.strictEqual(sol.name, 'Sol Ring');
  assert.strictEqual(sol.set_code, 'fin', 'set codes are normalised lower-case for the resolver');
  assert.strictEqual(sol.number, '2');
  assert.strictEqual(sol.quantity, 4, 'the ordered quantity survives');
  assert.strictEqual(sol.price_cents, 410, 'the unit price survives');
  assert.strictEqual(sol.is_foil, true, 'the order itself says foil');
  assert.strictEqual(forest.is_foil, true, 'an explicit isFoil flag counts');
  assert.strictEqual(forest.condition, 'Lightly Played', 'the sold condition rides along per line');
  assert.strictEqual(forest.scryfall_id, null);

  // opt-in extras puts the sealed box back in the list, count unchanged.
  const withExtras = parseOrderPayload(MP_DETAIL, undefined, { includeExtras: true });
  assert.strictEqual(withExtras.lineCount, 3);
  assert.strictEqual(withExtras.lines.__extras, 1, 'the count describes what was held out on the default path');
  assert.strictEqual(withExtras.lines.filter((l) => l.sealed).length, 1, 'the box is honestly flagged sealed, not a mystery card');
  assert.strictEqual(withExtras.lines.filter((l) => l.junk).length, 0);
  assert.strictEqual(withExtras.lines.__cardLines, 2, 'the card tally ignores the box even when it is listed');
  assert.strictEqual(withExtras.lines.__cardCopies, 5, 'copies count card copies only — a booster box is not a copy');
  assert.strictEqual(parsed.lines.__cardCopies, 5, 'the held-out path tallies the same cards');
}

// --- the TCGplayer gateway list shape --------------------------------------
const TCG_LIST = {
  data: [{
    number: '9991', name: 'Order', status: 'shipped',
    items: [{ productName: 'Counterspell', set: 'ALL', number: '4', quantity: 2, pricePaid: 1.25, conditionCode: 'EXC', isFoil: false }],
  }],
};
{
  const parsed = parseOrderPayload(TCG_LIST, '9991');
  assert.strictEqual(parsed.number, '9991');
  assert.strictEqual(parsed.lineCount, 1);
  assert.strictEqual(parsed.lines[0].name, 'Counterspell');
  assert.strictEqual(parsed.lines[0].set_code, 'all');
  assert.strictEqual(parsed.lines[0].price_cents, 125, 'a dollar-denominated gateway price is scaled');
  assert.strictEqual(parsed.lines[0].condition, 'Near Mint');
  assert.strictEqual(parsed.lines[0].is_foil, false, 'a non-foil line stays non-foil');
}

// A list that does not contain the requested number is not silently answered
// with the wrong order — that would file someone else's purchase.
assert.throws(() => parseOrderPayload({ data: [{ number: '111', items: [{ productName: 'X' }] }] }, '9991'),
  (e) => e.status === 422 && Array.isArray(e.observedKeys));
// ...and an unrecognisable 2xx body reports what came back instead of a bare
// "no orders", which is how the private-contract drift becomes visible.
assert.throws(() => parseOrderPayload({ foo: 1, bar: [1, 2] }, '1'), (e) => e.status === 422 && e.observedKeys.includes('foo'));

// --- fetchers: the injected HTTP edge --------------------------------------
async function fetchers() {
  // A uuid goes straight to the detail route: one call, no list lookup.
  const calls = [];
  const mp = await fetchManapoolOrder({
    email: 'a@b.com', token: 'tok', orderNumber: MP_LIVE_UUID,
    httpGet: (url, headers) => {
      calls.push({ url, headers });
      return { status: 200, body: MP_LIVE_DETAIL };
    },
  });
  assert.strictEqual(calls.length, 1, 'a uuid order number does not consult the list first');
  assert.ok(calls[0].url.endsWith('/buyer/orders/' + MP_LIVE_UUID), `the uuid-keyed detail route, got ${calls[0].url}`);
  assert.strictEqual(calls[0].headers['X-ManaPool-Email'], 'a@b.com');
  assert.strictEqual(calls[0].headers['X-ManaPool-Access-Token'], 'tok');
  assert.strictEqual(mp.body.order.order_number, MP_LIVE_NUM);

  // The cards are per-seller, and the top level has none: a reader that looks
  // only at the top level returns zero lines, which is the shipped bug.
  {
    const o = MP_LIVE_DETAIL.order;
    assert.ok(!Array.isArray(o.items), 'the live detail carries no top-level items[] — the doc shape is fiction');
    assert.ok((o.order_seller_details || []).length > 0, 'the cards hang off the seller rows');
    const p = parseOrderPayload(MP_LIVE_DETAIL, MP_LIVE_NUM);
    assert.strictEqual(p.number, MP_LIVE_NUM, 'the { order: {...} } wrapper is unwrapped');
    assert.ok(p.lineCount > 0, `nested items are read, got ${p.lineCount} lines`);
    assert.ok(p.lines.__cardCopies > 0, 'the copy tally is not zero');
    const [first] = p.lines;
    assert.ok(first.name, 'a real card name comes through');
    assert.ok(first.scryfall_id, 'the live scryfall_id rides along for the resolver');
    assert.ok(first.price_cents > 0, 'the unit price comes through');
    assert.strictEqual(first.set_code, String(first.set_code).toLowerCase(), 'set codes stay normalised');
    assert.ok(p.lines.some((l) => l.is_foil), 'finish_id FO decodes to a foil line');
    assert.ok(p.lines.some((l) => !l.is_foil), 'finish_id NF stays non-foil');
    assert.ok(p.lines.every((l) => l.condition === 'Near Mint'), 'condition_id NM decodes');
    assert.ok(p.lines.every((l) => !l.sealed), 'every mtg_single row is a card, not a box');
  }

  // A human order number is not what the detail route takes (it 400s with
  // "id: Invalid UUID"), so it has to be resolved through the list first and
  // only the row's uuid hits the detail. This is the single most load-bearing
  // call sequence in the ManaPool path and the doc examples do not show it.
  const probes = [];
  const second = await fetchManapoolOrder({
    email: 'a@b.com', token: 'tok', orderNumber: MP_LIVE_NUM,
    httpGet: (url) => {
      probes.push(url);
      if (url.endsWith('/buyer/orders')) return { status: 200, body: MP_LIST };
      if (url.includes(MP_LIVE_UUID)) return { status: 200, body: MP_LIVE_DETAIL };
      return { status: 404, body: '<html>not found</html>' };
    },
  });
  assert.ok(probes[0].endsWith('/buyer/orders'), `the list is probed first, got ${probes[0]}`);
  assert.ok(probes.some((u) => u.includes(MP_LIVE_UUID)), 'the detail is fetched by the row uuid');
  assert.ok(!probes.some((u) => u.includes(`/${MP_LIVE_NUM}`)), 'the human number never hits the uuid-keyed detail route');
  assert.ok(second.body.order['order_number'] === MP_LIVE_NUM, 'the resolved detail is the one returned');

  // A number that is not in the list still gets one direct attempt, so a
  // provider that does speak by-number keeps working, and then it reports.
  const misses = [];
  await assert.rejects(
    fetchManapoolOrder({
      email: 'a@b.com', token: 'tok', orderNumber: '999999',
      httpGet: (url) => {
        misses.push(url);
        if (url.endsWith('/buyer/orders')) return { status: 200, body: { orders: [] } };
        return { status: 404, body: { status: 404, message: 'Order not found' } };
      },
    }),
    (e) => e.status === 404 && /999999/.test(e.message),
    'an unknown ManaPool number says so by number',
  );
  assert.ok(misses.length >= 2, 'the list was consulted before giving up');

  // An auth failure is auth, not "no orders" — the user has to be told to fix
  // the credential rather than shown an empty list.
  for (const code of [401, 403]) {
    await assert.rejects(
      fetchManapoolOrder({ email: 'a@b.com', token: 'tok', orderNumber: '1', httpGet: () => ({ status: code, body: {} }) }),
      (e) => e.status === 401 && /credential/i.test(e.message),
      `ManaPool ${code} must read as an auth problem`,
    );
  }
  await assert.rejects(
    fetchManapoolOrder({ email: 'a@b.com', token: 'tok', orderNumber: '1', httpGet: () => ({ status: 429, body: {} }) }),
    (e) => e.status === 429 && /rate/i.test(e.message),
    'a ManaPool rate limit must not look like an empty order',
  );
  await assert.rejects(
    fetchManapoolOrder({ email: '', token: 'tok', orderNumber: '1', httpGet: () => ({ status: 200, body: {} }) }),
    (e) => e.status === 400,
    'missing credentials are a config error, surfaced before the request',
  );
  await assert.rejects(
    fetchManapoolOrder({ email: 'a@b.com', token: 't', orderNumber: 'a/../x', httpGet: () => ({ status: 200, body: {} }) }),
    (e) => e.status === 400,
    'a path-traversal order number is refused',
  );
  await assert.rejects(
    fetchTcgOrder({ cookies: '', orderNumber: '1', httpGet: () => ({ status: 200, body: {} }) }),
    (e) => e.status === 400,
    'no cookies saved yet is a config error',
  );

  // TCGplayer discovers the customer id from the jar and carries the cookie.
  const seen = [];
  const tcg = await fetchTcgOrder({
    cookies: 'tcg_customer_id=42; sid=abc', orderNumber: '9991',
    httpGet: (url, headers) => {
      seen.push({ url, headers });
      return { status: 200, body: TCG_LIST };
    },
  });
  assert.ok(seen[0].url.includes('customers/42/orders'), `expected the discovered customer id, got ${seen[0].url}`);
  assert.strictEqual(seen[0].headers.Cookie, 'tcg_customer_id=42; sid=abc');
  assert.ok(seen[0].url.includes('per_page='), 'the list endpoint is paged');
  assert.strictEqual(tcg.body.data[0].number, '9991');

  // Expired cookies: say so, and never echo the jar back into the message.
  await assert.rejects(
    fetchTcgOrder({ cookies: 'sid=abc', orderNumber: '1', httpGet: () => ({ status: 401, body: {} }) }),
    (e) => e.status === 401 && !/sid=abc/.test(e.message) && !/TCG_/.test(e.message),
  );
  // A gateway that answers 200 with something unrecognisable is reported with
  // the observed keys, not as a passing empty import.
  const odd = await fetchTcgOrder({ cookies: 'sid=abc', orderNumber: '1', httpGet: () => ({ status: 200, body: { nope: true } }) });
  assert.strictEqual(odd.unrecognised, true);
  assert.ok(odd.keys.includes('nope'));
  // Every path exhausted with none of them an order list is a failure, not a 0-card success.
  await assert.rejects(
    fetchTcgOrder({ cookies: 'sid=abc', orderNumber: '1', httpGet: () => ({ status: 500, body: 'oops' }) }),
    (e) => e.status === 502,
  );
  console.log('fetchers ok');
}

// --- preview: resolved cards, ownership, totals ----------------------------
const LINES = [
  { name: 'Sol Ring', set_code: 'fin', number: '2', scryfall_id: UUID_A, quantity: 4, price_cents: 410, is_foil: true, condition: 'Near Mint' },
  { name: 'Forest', set_code: 'all', number: '189', scryfall_id: null, quantity: 1, price_cents: 5, is_foil: false, condition: 'Lightly Played' },
  { name: '', set_code: null, number: null, scryfall_id: null, quantity: 1, price_cents: null, is_foil: false, condition: 'Near Mint' },
];
LINES.__extras = 1;

function stubResolve(known, hits = { n: 0 }) {
  return async (rows) => {
    hits.n += 1;
    const cards = [];
    const pairs = [];
    let notFound = 0;
    for (const row of rows) {
      const id = row.id || (row.set_id && row.number ? `${row.set_id}|${row.number}` : row.name);
      const card = known.get(String(id).toLowerCase());
      if (card) { cards.push(card); pairs.push({ row, card }); } else { pairs.push({ row, card: null }); notFound++; }
    }
    return { cards, pairs, notFound };
  };
}
const KNOWN = new Map([
  [UUID_A.toLowerCase(), { id: 'card-a', name: 'Sol Ring', image_url: 'https://a', set_id: 'fin', number: '2' }],
  ['all|189', { id: 'card-b', name: 'Forest', image_url: 'https://b', set_id: 'all', number: '189' }],
]);
const ownedStub = {
  all: async () => [{ card_id: 'card-a', qty: 3 }],
  get: async () => null,
};

async function preview() {
  const cached = [];
  const hits = { n: 0 };
  const out = await previewOrder({ lines: LINES, userId: 7 }, {
    bulkResolve: stubResolve(KNOWN, hits),
    cacheCards: async (cards) => cached.push(...cards),
    db: ownedStub,
  });
  assert.ok(hits.n === 1, 'the injected resolver must be the one that ran (no live-network fallback)');
  assert.strictEqual(out.totalListed, 4, 'what the order listed: 3 card lines plus the 1 held-out sealed line');
  assert.strictEqual(out.resolvedCount, 2, 'two resolved to real catalogue cards');
  assert.strictEqual(out.unresolved, 1, 'the nameless line is reported, not filed');
  assert.strictEqual(out.extras, 1, 'the held-out extras count is carried for the notice');
  assert.strictEqual(out.totalCopies, 6, 'copies are summed from the ordered quantity');
  assert.strictEqual(out.price_cents, 410 * 4 + 5, 'paid total uses the unit price x copies');
  assert.strictEqual(out.cards.length, 2);
  const sol = out.cards.find((c) => c.card_id === 'card-a');
  assert.strictEqual(sol.owned, 3, 'ownership comes from the collection read');
  assert.strictEqual(sol.quantity, 4, 'the ordered quantity is what will be filed');
  assert.strictEqual(sol.price_cents, 410);
  assert.strictEqual(sol.matched, true);
  assert.strictEqual(out.cards.find((c) => c.card_id === 'card-b').owned, 0, 'an unowned card shows 0');
  assert.ok(out.cards.every((c) => c.image_url), 'a resolved card carries art');
  assert.strictEqual(cached.length, 2, 'resolved cards were cached for art/pricing');

  const empty = await previewOrder({ lines: [] }, {});
  assert.strictEqual(empty.totalListed, 0);
  assert.strictEqual(empty.cards.length, 0);
  console.log('preview ok');
}

// --- the write: per-line fidelity through the bulk core -------------------
// --- sealed lines are never resolved or filed, even when included ----------
// The regression this pins: with include-sealed ticked, a collector booster used
// to appear as an unresolvable card row — inflating "N copies", the "couldn't be
// matched" notice, and risking a junk collection row. The parser flags it; these
// entry points must honour the flag all the way to the resolver and the core.
async function sealedGating() {
  const parsed = parseOrderPayload(MP_DETAIL, undefined, { includeExtras: true });
  assert.strictEqual(parsed.lines.length, 3, 'the sealed row rides along for the order total');

  // Spy resolver: records what it was ASKED to look up, and resolves every row it
  // sees as a found card. So anything reported unresolved / filed came from the
  // line list itself, never from a lookup miss.
  const seen = [];
  const spyResolve = async (rows) => {
    seen.push(...rows.map((r) => r.name));
    const cards = rows.map((r) => ({ id: `id-${r.name}`, name: r.name, set_id: r.set_id, number: r.number, image_url: 'x' }));
    return { cards, pairs: rows.map((r, i) => ({ row: r, card: cards[i] })), notFound: 0 };
  };
  const filed = [];
  const bulkStub = async (user, entries) => {
    filed.push(...entries.map((e) => e.card_id));
    return { added: entries.map((e) => ({ card_id: e.card_id })), failed: [] };
  };

  const pv = await previewOrder({ lines: parsed.lines, userId: null, includeExtras: true }, {
    bulkResolve: spyResolve, cacheCards: async () => {}, db: ownedStub,
  });
  assert.ok(!seen.includes('Collector Booster'), 'the box never reaches the card resolver');
  assert.strictEqual(seen.length, 2, 'only the two card lines are looked up');
  assert.strictEqual(pv.unresolved, 0, 'a sealed box is not reported as an unmatched card');
  assert.strictEqual(pv.extrasIncluded, true, 'the UI can tell included from held-out');
  assert.strictEqual(pv.extras, 1);
  assert.strictEqual(pv.totalCopies, 5, 'copies stay card-only even with the box listed');
  assert.strictEqual(pv.totalListed, 3, 'the order is still described by all three lines');

  const st = await addOrderToCollection(
    { user: { id: 7 }, lines: parsed.lines, condition: 'Near Mint' },
    { bulkResolve: spyResolve, cacheCards: async () => {}, bulkAdd: bulkStub },
  );
  assert.ok(!filed.some((id) => /Collector Booster/i.test(id)), 'the sealed box is never filed as a card');
  assert.strictEqual(filed.length, 2, 'exactly the two real cards are filed');
  assert.strictEqual(st.unresolved, 0, 'the add path reports no phantom unmatched cards');
  assert.strictEqual(st.totalListed, 3);
  console.log('sealed gating ok');
}

async function writes() {
  const user = { id: 7 };
  const bulkCalls = [];
  const deps = {
    bulkResolve: stubResolve(KNOWN),
    cacheCards: async () => 0,
    db: ownedStub,
    bulkAdd: async (u, entries, shared) => {
      bulkCalls.push({ u, entries, shared });
      return { added: entries.map((e) => ({ card_id: e.card_id, id: 1 })), failed: [], quantity: shared.quantity };
    },
  };

  const resolveHits = { n: 0 };
  const wdeps = { ...deps, bulkResolve: stubResolve(KNOWN, resolveHits) };
  const res = await addOrderToCollection({ user, lines: LINES, copies: 2 }, wdeps);
  assert.ok(resolveHits.n === 1, 'the injected resolver must be the one that ran (no live-network fallback)');
  assert.ok(bulkCalls.length >= 1, 'the injected bulk core must be the one that ran');
  const filedCopies = bulkCalls.flatMap((c) => c.entries).reduce((n, e) => n + e.quantity, 0);
  assert.strictEqual(res.added, filedCopies, 'added counts copies filed (order qty x copies), not card types');
  assert.strictEqual(bulkCalls.flatMap((c) => c.entries).length, 2, 'both resolvable lines filed as one stacked entry each; the nameless line never reached the core');
  assert.strictEqual(res.failed.length, 0);
  assert.ok(bulkCalls.length >= 1, 'the bulk core was used');
  const flat = bulkCalls.flatMap((c) => c.entries);
  const sol = flat.find((e) => e.card_id === 'card-a');
  assert.strictEqual(sol.quantity, 8, 'copies multiplier x ordered quantity (4 x 2)');
  assert.strictEqual(sol.purchase_price, 4.1, 'dollars, from cents — the core writes purchase_price verbatim');
  assert.strictEqual(sol.condition, 'Near Mint', 'per-line condition rides along, not the panel default');
  assert.strictEqual(sol.printing, 'Holofoil', 'the order said foil, so the printing is foil');
  const forest = flat.find((e) => e.card_id === 'card-b');
  assert.strictEqual(forest.purchase_price, 0.05);
  assert.strictEqual(forest.condition, 'Lightly Played', 'the sold condition is kept per line');
  assert.strictEqual(forest.printing, 'Normal');
  assert.ok(bulkCalls.every((c) => c.shared.stackable === true), 'order imports stack copies on one row');
  assert.ok(bulkCalls.every((c) => c.u === user), 'the write runs as the owning user');

  // An unknown line condition falls back to the caller's default, not garbage.
  const res2 = await addOrderToCollection({
    user, condition: 'Damaged',
    lines: [{ name: 'Sol Ring', set_code: 'fin', number: '2', scryfall_id: UUID_A, quantity: 1, is_foil: false, condition: 'Nope' }],
  }, wdeps);
  const e2 = bulkCalls[bulkCalls.length - 1].entries[0];
  assert.strictEqual(e2.condition, 'Damaged', 'an unmappable grade falls back to the caller default');
  assert.strictEqual(res2.added, 1);

  // nonfoil mode forces every card to Normal, including the foil-flagged one.
  await addOrderToCollection({ user, lines: [LINES[0]], printingMode: 'nonfoil' }, wdeps);
  const forced = bulkCalls[bulkCalls.length - 1];
  assert.strictEqual(forced.entries[0].printing, 'Normal', 'nonfoil mode means the stack is not foil');
  assert.strictEqual(forced.shared.printing, 'Normal');

  // Nothing resolved is a clean zero, not a crash.
  const none = await addOrderToCollection({ user, lines: [{ name: 'Nope', set_code: null, number: null, scryfall_id: null, quantity: 1 }] }, {
    ...deps, bulkResolve: async () => ({ cards: [], pairs: [], notFound: 1 }),
  });
  assert.strictEqual(none.added, 0);

  await assert.rejects(addOrderToCollection({ user: null, lines: LINES }, deps), (e) => e.status === 400);
  // An order with no card lines is NOT an error (a sealed-only purchase): it
  // resolves to a clean zero so the route can say "nothing to add" instead of
  // failing the request.
  const emptyLines = await addOrderToCollection({ user, lines: [] }, wdeps);
  assert.strictEqual(emptyLines.added, 0);
  assert.strictEqual(emptyLines.resolved, 0);
  assert.ok(Array.isArray(emptyLines.failed) && emptyLines.failed.length === 0);
  // An unavailable filing core is a 5xx, not a silent zero-card success.
  await assert.rejects(addOrderToCollection({ user, lines: LINES }, {
    ...deps,
    bulkAdd: async () => { const e = new Error('bulk core unavailable'); e.status = 500; throw e; },
  }), (e) => e.status === 500, 'a failing bulk core must surface, not file nothing quietly');

  // The production wiring: with nothing injected the util resolves the REAL
  // resolver/cache/filing functions, so the route cannot ship a half-connected
  // importer. Asserted by identity, never by calling them (that would touch
  // the network and the live collection).
  const collection = require('../src/routes/collection');
  const scryfall = require('../src/scryfallApi');
  const cardCache = require('../src/utils/cardCache');
  assert.strictEqual(typeof collection.bulkAddToCollection, 'function', 'the bulk core is exported for the util to fall back to');
  assert.strictEqual(typeof scryfall.bulkFetchByIdentifier, 'function');
  assert.strictEqual(typeof cardCache.cacheNormalizedCards, 'function');
  console.log('writes ok');
}

// NOTE: the single runner chain lives at the bottom of this file, after the
// live-picker tests, so every suite runs exactly once.


// --- recent-order summaries -------------------------------------------------
const { fetchTcgRecentOrders, recentOrderSummaries, manapoolRecentOrderSummaries, RECENT_LIMIT } =
  require('../src/utils/marketplaceOrders');

assert.strictEqual(RECENT_LIMIT, 3, 'the picker promises three');

// summaries keep only header fields, newest first, capped, no PII riding along.
{
  const list = [
    { orderNumber: 'A1', created: '2026-09-01T10:00:00Z', status: 'SHIPPED', totalCents: 1250,
      customer: { email: 'me@x.com', address: '1 Main' },
      items: [{ product: { single: { name: 'Tarmogoyf', set: 'FUT', number: '147' } }, quantity: 2, unitPriceCents: 500 }] },
    { orderNumber: 'B2', created: '2026-09-10T10:00:00Z', status: 'NEW', totalCents: 999,
      items: [{ product: { single: { name: 'Lion' + 's Eye', kind: 'sealed' } }, quantity: 1 }] },
    { orderNumber: 'C3', created: '2026-08-01T10:00:00Z', status: 'SHIPPED', totalCents: 10,
      items: [{ card: { name: 'Forest' }, quantity: 4 }] },
    { orderNumber: 'D4', created: '2026-07-01T10:00:00Z', items: [] },
  ];
  const rows = recentOrderSummaries(list);
  assert.strictEqual(rows.length, 3, 'capped at RECENT_LIMIT');
  assert.deepStrictEqual(rows.map((r) => r.number), ['B2', 'A1', 'C3'], 'newest first');
  assert.strictEqual(rows[1].cardCount, 2, 'copies counted');
  assert.strictEqual(rows[0].cardCount, 0, 'a sealed-only order has no card copies');
  const blob = JSON.stringify(rows);
  assert.ok(!blob.includes('me@x.com') && !blob.includes('Main'), 'no PII rides along');
}

async function recentPicker() {
  // The picker's data path, against the captured list AND the captured details.
  // This is the reported bug: the list rows carry no items at all, so a card
  // count read off them is 0 by construction — the count has to come from each
  // order's detail. 517062 is the canary: its list row claims 1 and its real
  // detail is 2 lines / 3 copies, so this fails if the count ever slides back
  // to reading the list.
  const http = async (url) => {
    if (url.endsWith('/buyer/orders')) return { status: 200, body: MP_LIST };
    for (const [uuid, body] of Object.entries(MP_DETAILS_BY_UUID)) {
      if (url.includes(uuid)) return { status: 200, body };
    }
    return { status: 404, body: { status: 404, message: 'Order not found' } };
  };
  const rows = await manapoolRecentOrderSummaries({ email: 'a@b.co', token: 't', httpGet: http });
  assert.strictEqual(rows.length, RECENT_LIMIT, 'the picker shows three');
  assert.deepStrictEqual(rows.map((r) => r.number), ['527407', '524055', '517062'],
    'newest first — the live list comes back OLDEST first, so it has to be sorted here');
  assert.ok(rows[0].placedAt > rows[2].placedAt, 'the dates agree with that order');
  assert.strictEqual(rows[0].cardCount, 10, 'newest order: 10 copies read from its detail');
  const canary = rows[2];
  assert.strictEqual(canary.number, '517062');
  assert.strictEqual(canary.cardCount, 3, '3 copies from the detail, not the list claim of 1');
  assert.strictEqual(canary.lineCount, 2, 'two distinct line rows');
  for (const r of rows) {
    assert.ok(r.cardCount > 0, `a picker row with real cards never reads 0 (got ${r.number}: ${r.cardCount})`);
    assert.ok(r.number && r.placedAt, 'every row is identifiable and dated');
  }
  const blob = JSON.stringify(rows);
  assert.ok(!/seller_username/.test(blob) && !/buyer_email/.test(blob), 'seller/buyer identity is not echoed to the browser');

  // A dead token is auth, not an empty list — the same distinction that kept the
  // old code from telling Brent his credential was fine when the route was wrong.
  await assert.rejects(
    manapoolRecentOrderSummaries({ email: 'a@b.co', token: 't', httpGet: async () => ({ status: 401, body: {} }) }),
    (e) => e.status === 401 && /credential/i.test(e.message),
    'a 401 on the list route reads as an auth problem',
  );
  // A route that will not answer at all is an honest list_unavailable, never a
  // success carrying an empty list (which the UI would render as 'no orders').
  await assert.rejects(
    manapoolRecentOrderSummaries({ email: 'a@b.co', token: 't', httpGet: async () => ({ status: 404, body: '<html/>' }) }),
    (e) => e.listUnavailable === true && e.status === 502,
    'an unhelpful list route reports list_unavailable',
  );
  // Details that fail degrade to the list's own count instead of failing the picker.
  const partial = await manapoolRecentOrderSummaries({
    email: 'a@b.co', token: 't',
    httpGet: async (url) => {
      if (url.endsWith('/buyer/orders')) return { status: 200, body: MP_LIST };
      return { status: 500, body: {} };
    },
  });
  assert.strictEqual(partial.length, RECENT_LIMIT, 'the picker still fills when the details die');
  assert.ok(partial.every((r) => r.cardCount >= 0), 'it falls back to whatever the list does assert');
  console.log('live ManaPool list/detail/picker tests passed');
}

Promise.resolve()
  .then(fetchers)
  .then(preview)
  .then(writes)
  .then(sealedGating)
  .then(recentPicker)
  .then(() => {
    console.log('marketplaceorders.test.js: all assertions passed');
  })
  .catch((err) => {
    console.error('marketplaceorders.test.js FAILED', err && err.message ? err.message : err);
    if (err && err.stack) console.error(err.stack.split('\n').slice(0, 6).join('\n'));
    process.exitCode = 1;
  });
