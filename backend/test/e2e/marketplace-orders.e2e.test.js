const path = require('path');
const os = require('os');
const fs = require('fs');
const http = require('http');
const assert = require('assert');
const { spawn } = require('child_process');

// "Import from order" driven through the real HTTP stack: the real Express app
// on a throwaway DB, the real credential routes, the real fetchers, the real
// bulk-add core. Two local fake markets stand in for ManaPool and the TCGplayer
// gateway (their bases are env-overridable for exactly this reason), and
// scryfall-mock.js is preloaded so card resolution never leaves the box.
//
// What this proves that the unit test cannot: the routes are actually mounted,
// auth is actually enforced, credentials really persist and are really withheld
// from every GET body, and the numbers a user sees in preview are the numbers
// that land in the collection.
const tmpDb = path.join(os.tmpdir(), `scrybox-mkt-e2e-${process.pid}.db`);
const projectRoot = path.join(__dirname, '../../..');

// --- the fake markets ------------------------------------------------------
// MP_ORDER is the documented shape and stays a deliberate synthetic: it proves
// the top-level-items reader still works. The BUYER_LIST / BUYER_DETAILS below
// are the captured LIVE shapes, because that is what the real ManaPool sends
// for the buyer routes and the picker test has to face reality, not the docs.
const MP_ORDER = {
  id: 4402,
  number: '90001',
  status: 'shipped',
  items: [
    { quantity: 2, price_cents: 410, product: { single: { name: 'Black Lotus', set: 'lea', number: '232' } } },
    { quantity: 1, price_cents: 25, conditionCode: 'LP', product: { single: { name: 'Lightning Bolt', set: 'm10', number: '146' } } },
    { quantity: 1, price_cents: 1000, product: { sealed: { name: 'Collector Booster', kind: 'sealed' } } },
  ],
};
const TCG_ORDER = {
  data: [{
    number: '77001',
    name: 'Order',
    status: 'shipped',
    items: [{ productName: 'Lightning Bolt', set: 'm10', number: '146', quantity: 3, pricePaid: 1.25 }],
  }],
};

// The buyer routes answer in the captured live contract (same files the unit
// test asserts against), so the picker is exercised against what ManaPool
// actually sends: a list whose rows carry NO items, plus per-order details that
// hang their cards off order_seller_details[].items. Serving the doc shape here
// would let the 0-card bug sail straight through again.
const BUYER_LIST = require('../fixtures/manapool/buyer-orders.json');
const BUYER_DETAILS = require('../fixtures/manapool/buyer-order-details-by-uuid.json');

function fakeMarkets() {
  const hits = [];
  const server = http.createServer((req, res) => {
    const url = req.url || '';
    hits.push(url);
    const send = (code, obj) => {
      res.writeHead(code, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(obj));
    };
    const mpToken = String(req.headers['x-manapool-access-token'] || '');
    const cookie = String(req.headers.cookie || '');
    // A dead token / expired jar must read as an auth problem, not an empty order.
    if (mpToken === '401' || cookie.includes('tcgexpired')) return send(401, { message: 'Unauthorized' });
    if (mpToken === '429') return send(429, { message: 'too many requests' });

    if (url.startsWith('/mp/')) {
      if (url.includes('/orders/not-a-list')) return send(200, { nothing: 'matched' });
      if (url.includes('/orders/wrong-number')) return send(200, { data: [{ number: '111', items: [] }] });
      if (url.includes('/orders/90001')) return send(200, MP_ORDER);
      // The live buyer routes, ahead of the doc-shape cases so those keep their
      // own coverage: the list (rows with no items) and the uuid-keyed details.
      if (url.split('?')[0].endsWith('/buyer/orders')) return send(200, BUYER_LIST);
      const buyer = url.split('?')[0].match(/\/buyer\/orders\/([0-9a-f-]{36})/i);
      if (buyer && BUYER_DETAILS[buyer[1]]) return send(200, BUYER_DETAILS[buyer[1]]);
      return send(404, { message: 'not found' });
    }
    if (url.includes('/customers/') && url.includes('/orders')) return send(200, TCG_ORDER);
    return send(404, { error: 'unhandled' });
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port, hits })));
}

async function waitForServer(url) {
  for (let i = 0; i < 150; i++) {
    try { const r = await fetch(url); if (r.ok) return; } catch { /* not up */ }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error('Server did not start in time');
}

const MP_CRED = { email: 'collector@example.test', token: 'mp-token-supercalifragilistic-5150' };
const TCG_JAR = 'tcg_customer_id=42; tcgplayer_session=abcdef123456; junk_no_value';

async function runTests() {
  const markets = await fakeMarkets();
  const baseMkt = `http://127.0.0.1:${markets.port}`;
  const server = spawn('node', [
    '-r', path.join(__dirname, 'scryfall-mock.js'),
    path.join(projectRoot, 'backend/src/server.js'),
  ], {
    env: {
      ...process.env,
      PORT: '3011',
      DB_PATH: tmpDb,
      HTTPS_PORT: '',
      DEFAULT_ADMIN_PASSWORD: 'test-admin-password',
      MANAPOOL_API_BASE: `${baseMkt}/mp`,
      TCG_ORDER_BASE: `${baseMkt}/tcg/`,
      MARKETPLACE_TIMEOUT_MS: '4000',
    },
  });
  const base = 'http://localhost:3011';

  try {
    await waitForServer(`${base}/api/health`);
    const login = await fetch(`${base}/api/auth/login`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username: 'admin', password: 'test-admin-password' }),
    });
    assert.strictEqual(login.status, 200, `login returned ${login.status}`);
    const token = (await login.json()).token;
    const H = { 'Content-Type': 'application/json', 'Authorization': `Bearer ${token}` };
    const getAccounts = async () => (await (await fetch(`${base}/api/marketplace/accounts`, { headers: H }))).json();
    const collection = async () => (await (await fetch(`${base}/api/collection?limit=2000`, { headers: H }))).json();
    const put = async (body) => fetch(`${base}/api/marketplace/accounts`, { method: 'PUT', headers: H, body: JSON.stringify(body) });
    const post = async (route, body) => {
      const r = await fetch(`${base}/api/marketplace/${route}`, { method: 'POST', headers: H, body: JSON.stringify(body) });
      return { status: r.status, body: await r.json().catch(() => ({})) };
    };

    // F9-TC1: the marketplace routes are mounted and demand authentication.
    const anon = await fetch(`${base}/api/marketplace/accounts`, { headers: H });
    assert.strictEqual(anon.status, 200, 'sanity: authenticated accounts read works');
    const noAuth = await fetch(`${base}/api/marketplace/accounts`);
    assert.ok(noAuth.status === 401 || noAuth.status === 403, `unauthenticated accounts read must be rejected, got ${noAuth.status}`);
    const anonAdd = await fetch(`${base}/api/marketplace/add`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ source: 'manapool', order_number: '90001' }),
    });
    assert.ok(anonAdd.status === 401 || anonAdd.status === 403, `unauthenticated add must be rejected, got ${anonAdd.status}`);
    console.log('PASS: F9-TC1');

    // F9-TC2: nothing configured yet -> both providers report unconfigured, and
    // the response carries no secret material.
    const boot = await getAccounts();
    assert.strictEqual(boot.manapool.configured, false);
    assert.strictEqual(boot.tcgplayer.configured, false);
    assert.ok(!JSON.stringify(boot).includes('abcdef123456'), 'GET must not echo a cookie value');
    console.log('PASS: F9-TC2');

    // F9-TC3: saving ManaPool persists it (masked) and never returns the token.
    const savedMP = await put({ manapool: MP_CRED });
    assert.strictEqual(savedMP.status, 200, `save manapool returned ${savedMP.status}`);
    const savedMPBody = await savedMP.text();
    assert.ok(!savedMPBody.includes(MP_CRED.token), 'the access token must never appear in a response body');
    assert.ok(savedMPBody.includes('"configured":true'), 'save responds with the masked status');
    const accMP = await getAccounts();
    assert.strictEqual(accMP.manapool.configured, true);
    assert.strictEqual(accMP.manapool.enabled, true);
    assert.ok(accMP.manapool.savedAt, 'savedAt is reported');
    assert.ok(!JSON.stringify(accMP).includes(MP_CRED.token), 'the token must never appear in the status body');
    // The masked form keeps a short prefix and the domain so the user can tell
    // which account is saved, without handing back the full local part.
    assert.ok(String(accMP.manapool.email).includes('@example.test'), `domain stays visible, got ${accMP.manapool.email}`);
    assert.ok(!String(accMP.manapool.email).includes('collector@'), `local part is masked, got ${accMP.manapool.email}`);
    assert.ok(String(accMP.manapool.email).includes('\u2022') || /\*/.test(String(accMP.manapool.email)), 'masking characters are shown');
    console.log('PASS: F9-TC3');

    // F9-TC4: TCGplayer cookies normalize on save; a jar with no usable pair is
    // refused rather than stored.
    const badJar = await put({ tcgplayer: { cookies: 'garbage-with-no-pair' } });
    assert.strictEqual(badJar.status, 400, 'a jar with no usable pair must be refused');
    const savedTCG = await put({ tcgplayer: { cookies: `Cookie: ${TCG_JAR}` } });
    assert.strictEqual(savedTCG.status, 200, `save tcgplayer returned ${savedTCG.status}`);
    assert.ok(!(await savedTCG.text()).includes('abcdef123456'), 'the cookie jar must not be echoed back');
    const accts = await getAccounts();
    assert.strictEqual(accts.tcgplayer.configured, true);
    assert.strictEqual(accts.tcgplayer.cookies, '2 cookies saved', 'the jar is counted, never reproduced');
    assert.ok(!JSON.stringify(accts).includes('tcgplayer_session'), 'no cookie NAME may leak either');
    assert.ok(!JSON.stringify(accts).includes('junk_no_value'), 'the unusable fragment was dropped, not stored');
    console.log('PASS: F9-TC4');

    // F9-TC4b: the master switch moves on its own. With a token already stored,
    // a PUT carrying only {enabled:true} keeps the credential instead of
    // demanding a fresh one (the field is write-only; blank means keep).
    const onOnly = await put({ manapool: { enabled: true } });
    assert.strictEqual(onOnly.status, 200, `enabled-only toggle returned ${onOnly.status}`);
    const onOnlyBody = (await onOnly.json()).accounts;
    assert.strictEqual(onOnlyBody.manapool?.configured, true, 'the stored token survives a toggle-only PUT');
    assert.strictEqual(onOnlyBody.manapool?.enabled, true, 'the switch moved');
    console.log('PASS: F9-TC4b');

    // F9-TC4c: enabled-only with nothing stored is still nonsense -> 400.
    await put({ manapool: { clear: true } });
    const toggleReject = await put({ manapool: { enabled: true } });
    assert.strictEqual(toggleReject.status, 400, 'enabled-only on an empty row must be refused');
    await put({ manapool: MP_CRED });   // restore for the order-flow tests
    console.log('PASS: F9-TC4c');

    // F9-TC5: preview resolves the real order through the fake market, holds the
    // sealed box out of the card list but counts it, and totals from the cards.
    const prev = await post('preview', { source: 'manapool', order_number: '90001' });
    assert.strictEqual(prev.status, 200, `preview returned ${prev.status}: ${JSON.stringify(prev.body)}`);
    const pv = prev.body;
    assert.strictEqual(pv.lineCount, 2, 'two card lines (the sealed box is not a card)');
    assert.strictEqual(pv.totalListed, 3, 'every order line was still considered');
    assert.strictEqual(pv.extras, 1, 'the sealed box is counted, not hidden');
    assert.strictEqual(pv.resolvedCount, 2, 'both named cards resolve through the mock');
    assert.strictEqual(pv.totalCopies, 3, 'copies are summed from the order quantities');
    assert.strictEqual(pv.price_cents, 410 * 2 + 25, 'paid total comes from the order prices');
    assert.ok(pv.cards.every((c) => c.matched), 'previewed cards all resolved');
    assert.ok(pv.cards.some((c) => c.image_url), 'resolved cards carry art for the preview grid');
    assert.ok(pv.cards.every((c) => 'owned' in c), 'preview reports owned counts so duplicates are visible before commit');
    assert.ok(pv.cards.every((c) => c.condition), 'each preview line carries the condition that will be filed');
    console.log('PASS: F9-TC5');

    // F9-TC6: the commit files exactly what preview showed, with per-line
    // condition/printing/price preserved, and reports the held-out extras.
    const add = await post('add', { source: 'manapool', order_number: '90001' });
    assert.strictEqual(add.status, 200, `add returned ${add.status}: ${JSON.stringify(add.body)}`);
    assert.strictEqual(add.body.added, 3, 'added counts copies (2x + 1x), not card types');
    assert.strictEqual(add.body.resolved, 2, 'both card types filed');
    assert.deepStrictEqual(add.body.failed, [], 'nothing should fail on a clean import');
    assert.strictEqual(add.body.totalListed, 3);
    assert.strictEqual(add.body.unresolved, 0, 'the sealed box is not counted as an unresolved card');
    assert.ok(/order 90001/i.test(add.body.message), `message should name the order, got ${add.body.message}`);
    const rows = await collection();
    const lotus = rows.find((r) => r.name === 'Black Lotus');
    const bolt = rows.find((r) => r.name === 'Lightning Bolt');
    assert.ok(lotus && bolt, 'both cards are in the collection now');
    assert.strictEqual(Number(lotus.quantity), 2, 'ordered quantity preserved');
    assert.strictEqual(Number(lotus.purchase_price), 4.1, 'per-line unit price survived the bulk path');
    assert.strictEqual(Number(bolt.quantity), 1);
    assert.strictEqual(Number(bolt.purchase_price), 0.25);
    assert.strictEqual(bolt.condition, 'Lightly Played', 'per-line sold condition survived the bulk path');
    assert.ok(['Normal', 'Holofoil'].includes(lotus.printing), 'a printing was assigned');
    console.log('PASS: F9-TC6');

    // F9-TC7: re-importing the same order is safe and additive. The core files
    // one row per add event (`stackable` collapses COPIES within an add, it is
    // not an upsert), which is what an order import wants: two purchases of the
    // same card keep their own price and date rather than one averaging away.
    // What must not happen is the import failing, or copies going missing.
    const before7 = await collection();
    const lotusBefore = before7.filter((r) => r.name === 'Black Lotus');
    const reAdd = await post('add', { source: 'manapool', order_number: '90001' });
    assert.strictEqual(reAdd.status, 200);
    assert.strictEqual(reAdd.body.added, 3, 'the re-import still reports its three copies');
    assert.deepStrictEqual(reAdd.body.failed, [], 're-import must not fail against existing rows');
    const after7 = await collection();
    const lotusAfter = after7.filter((r) => r.name === 'Black Lotus');
    assert.strictEqual(lotusAfter.length, lotusBefore.length + 1, 'the second order files its own purchase row');
    assert.ok(lotusAfter.every((r) => Number(r.purchase_price) === 4.1), 'each purchase row keeps its own unit price');
    const lotusCopies = lotusAfter.reduce((n, r) => n + Number(r.quantity), 0);
    assert.strictEqual(lotusCopies, 4, 'total copies across purchase events is 2 + 2, none lost');
    const boltRows = after7.filter((r) => r.name === 'Lightning Bolt');
    assert.strictEqual(boltRows.reduce((n, r) => n + Number(r.quantity), 0), 2, 'the LP line stacked its copies too');
    assert.ok(boltRows.every((r) => r.condition === 'Lightly Played'), 'per-line condition held on both events');
    assert.strictEqual(after7.length, before7.length + 2, 'the second order added exactly its two card rows');

    // The preview's owned-count badge is what stops a user re-buying a card they
    // already filed; it must reflect the rows that are now in the collection.
    const prevOwned = await post('preview', { source: 'manapool', order_number: '90001' });
    assert.strictEqual(prevOwned.status, 200);
    const ownedLotus = prevOwned.body.cards.find((c) => c.name === 'Black Lotus');
    assert.ok(ownedLotus && ownedLotus.owned >= 4, `preview should report the copies already held, got ${ownedLotus && ownedLotus.owned}`);
    console.log('PASS: F9-TC7');

    // F9-TC8: TCGplayer flows through the gateway list shape, with the customer
    // id discovered from the saved cookie jar.
    const tPrev = await post('preview', { source: 'tcgplayer', order_number: '77001' });
    assert.strictEqual(tPrev.status, 200, `tcg preview returned ${tPrev.status}: ${JSON.stringify(tPrev.body)}`);
    assert.strictEqual(tPrev.body.lineCount, 1);
    assert.strictEqual(tPrev.body.resolvedCount, 1, 'the gateway card resolved through the mock');
    assert.strictEqual(tPrev.body.totalCopies, 3);
    // The gateway quotes dollars where ManaPool quotes cents; the line's UNIT
    // price is the thing that must normalise (125c), and the order total is
    // unit x copies like every other preview.
    assert.strictEqual(tPrev.body.price_cents, 375, 'preview total is the unit price times copies');
    assert.strictEqual(tPrev.body.cards[0].price_cents, 125, 'a dollar-denominated gateway price is normalised to cents');
    assert.ok(markets.hits.some((h) => h.startsWith('/tcg/customers/42/orders')), 'the discovered customer id was used');
    assert.ok(markets.hits.some((h) => h.includes('per_page=')), 'the list endpoint is paged');
    console.log('PASS: F9-TC8');

    // F9-TC9: an auth rejection is surfaced as auth (never "no orders"), and an
    // unrecognisable 2xx reports what came back instead of a silent zero.
    const dead = await put({ manapool: { email: MP_CRED.email, token: '401' } });
    assert.strictEqual(dead.status, 200, `swapping in the failing token: ${dead.status}`);
    const denied = await post('preview', { source: 'manapool', order_number: '90001' });
    assert.strictEqual(denied.status, 401, `a dead token must read as auth, got ${denied.status}`);
    assert.ok(/credential|not available|expired|check the account/i.test(denied.body.error),
      `auth message should name the credential problem, got: ${denied.body.error}`);
    assert.ok(!/no orders|not found|empty/i.test(denied.body.error), 'an auth failure must not be reported as an empty order');
    await put({ manapool: MP_CRED });

    const odd = await post('preview', { source: 'manapool', order_number: 'not-a-list' });
    assert.strictEqual(odd.status, 422, `an unrecognisable shape must report, got ${odd.status}`);
    assert.ok(Array.isArray(odd.body.observedKeys) && odd.body.observedKeys.includes('nothing'),
      'the response reports what came back');
    console.log('PASS: F9-TC9');

    // F9-TC10: input validation is real.
    const badSource = await post('preview', { source: 'ebay', order_number: '90001' });
    assert.strictEqual(badSource.status, 400, 'an unknown source is rejected');
    const badNum = await post('preview', { source: 'manapool', order_number: '../../etc/passwd' });
    assert.strictEqual(badNum.status, 400, 'a path-traversal order number must be refused');
    const emptyNum = await post('preview', { source: 'manapool', order_number: '' });
    assert.strictEqual(emptyNum.status, 400, 'a blank order number must be refused');
    const noProvider = await put({});
    assert.strictEqual(noProvider.status, 400, 'an empty credential save must be refused');
    console.log('PASS: F9-TC10');

    // F9-TC11b: the recent-orders picker route. This is the endpoint that
    // shipped listing every ManaPool order as 0 cards, and it had NO coverage
    // until the fix - which is precisely how the bug got out. The fake answers
    // in the live contract shape ({ order: {...} }, cards under
    // order_seller_details[].items, list rows with no items at all), so this
    // fails if the reader ever slides back to the doc shape.
    const getRecent = async (source, headers = H) => {
      const r = await fetch(`${base}/api/marketplace/recent/${source}`, { headers });
      return { status: r.status, body: await r.json().catch(() => ({})) };
    };
    const recMp = await getRecent('manapool');
    assert.strictEqual(recMp.status, 200, `the ManaPool picker must answer, got ${recMp.status}: ${JSON.stringify(recMp.body)}`);
    assert.ok(Array.isArray(recMp.body.orders) && recMp.body.orders.length === 3, 'the picker promises three');
    assert.ok(recMp.body.orders.every((o) => o.cardCount > 0),
      `no picker row may read 0 cards when the order has cards, got ${JSON.stringify(recMp.body.orders.map((o) => [o.number, o.cardCount]))}`);
    // 517062 claims one item on the list row and really is two lines / three copies.
    const canary = recMp.body.orders.find((o) => o.number === '517062');
    assert.ok(canary, 'the newest three lead the list (rows arrive oldest-first upstream)');
    assert.strictEqual(canary.cardCount, 3, 'the count comes from the detail, not the list item_count of 1');
    assert.strictEqual(canary.lineCount, 2);
    assert.deepStrictEqual(recMp.body.orders.map((o) => o.number), ['527407', '524055', '517062'],
      'newest first, capped at three');
    assert.ok(!JSON.stringify(recMp.body).match(/seller_username|buyer_email/), 'no seller or buyer identity crosses to the browser');
    const recBad = await getRecent('ebay');
    assert.strictEqual(recBad.status, 400, 'an unknown picker source is rejected');
    const recNoAuth = await fetch(`${base}/api/marketplace/recent/manapool`);
    assert.ok(recNoAuth.status === 401 || recNoAuth.status === 403, 'the picker demands authentication');
    // A provider that is turned off stays silent here too, rather than leaking a
    // half-list or an upstream call.
    await put({ manapool: { ...MP_CRED, enabled: false } });
    const recOff = await getRecent('manapool');
    assert.strictEqual(recOff.status, 400, 'a disabled source must refuse the picker call');
    assert.ok(/not configured|turned off/i.test(recOff.body.error), `it should say so, got ${recOff.body.error}`);
    await put({ manapool: { ...MP_CRED, enabled: true } });
    // A dead token on the list call is auth, not an empty list.
    await put({ manapool: { email: MP_CRED.email, token: '401' } });
    const recDead = await getRecent('manapool');
    assert.strictEqual(recDead.status, 401, `a dead token must read as auth, got ${recDead.status}`);
    assert.ok(recDead.body.list_unavailable !== true, 'an auth failure is not reported as an unavailable list');
    await put({ manapool: MP_CRED });
    console.log('PASS: F9-TC11b (recent-orders picker)');

    // F9-TC12: clearing a provider's credentials works and empties the status.
    const cleared = await put({ tcgplayer: { clear: true } });
    assert.strictEqual(cleared.status, 200);
    const afterClear = await getAccounts();
    assert.strictEqual(afterClear.tcgplayer.configured, false);
    const blocked = await post('preview', { source: 'tcgplayer', order_number: '77001' });
    assert.strictEqual(blocked.status, 400, 'a cleared provider must refuse, not fetch with an empty jar');
    assert.ok(/not configured/i.test(blocked.body.error), `cleared provider should say so, got ${blocked.body.error}`);
    console.log('PASS: F9-TC11');
  } finally {
    server.kill('SIGKILL');
    for (const suffix of ['', '-wal', '-shm']) {
      try { fs.unlinkSync(tmpDb + suffix); } catch { /* already gone */ }
    }
    try { markets.server.close(); } catch { /* already closed */ }
  }
}

runTests()
  .then(() => { console.log('marketplace-orders.e2e.test.js: all assertions passed'); process.exit(0); })
  .catch((err) => {
    console.error('marketplace-orders.e2e.test.js FAILED', err && err.message ? err.message : err);
    if (err && err.stack) console.error(err.stack.split('\n').slice(0, 5).join('\n'));
    process.exit(1);
  });
