// Buy-from-marketplace order import: turn a ManaPool or TCGplayer order number
// into collection entries, the same way the Secret Lair importer turns a product
// card list into entries — resolve each line through the proven
// bulkFetchByIdentifier -> card_cache path, then file through the shared
// bulk-add core. The user picks the order; Scrybox does the rest.
//
// Why the two providers are built differently (verified against their live
// endpoints, see the probes in the commit that introduced this file):
//
//   ManaPool — has a real buyer-facing REST API. base https://manapool.com/api/v1,
//     auth is `X-ManaPool-Email` + `X-ManaPool-Access-Token` (a token generated
//     in the user's dashboard Integration settings). THE PUBLISHED EXAMPLES ARE
//     WRONG ABOUT THE BUYER ROUTES, so the shapes below come from probes against
//     a live buyer account (2026-09-17), not from the docs:
//       - the buyer list is `GET /buyer/orders`, which IGNORES page/per_page
//         and returns the whole set OLDEST first.
//       - the detail is `GET /buyer/orders/{uuid}` keyed by the row's `id`, NOT
//         the six-digit `order_number` a human reads; ask it for a number and
//         it 400s with "Invalid path parameters: id: Invalid UUID". The doc's
//         `GET /orders/{number}` answers 400 the same way, so a human number
//         must be resolved through the list first (see fetchManapoolOrder).
//       - the detail body is `{ order: {...} }`, and it has NO top-level
//         `items`: the cards hang off `order_seller_details[].items`. Reading a
//         top-level `items[]` — which is what the doc shape implied — finds
//         nothing and every order looks like 0 cards.
//       - per line, identity is `product.single` {name, set, number,
//         scryfall_id} and the print/grade are IDS: `finish_id` FO|NF and
//         `condition_id` NM|LP|MP|HP|DAM, with `price_cents` + `quantity`.
//         `product.product_type` is 'mtg_single' for cards and something else
//         for sealed. Σ(price_cents × quantity) equals the list's
//         `subtotal_cents` for every order probed, which is what makes the
//         detail trustworthy; the list's own `item_count` does NOT agree with
//         the real line rows (27 vs 35 on one order), so counts come from here.
//     Unknown routes under that base 404 with an HTML body; a bad token 401s
//     with a JSON {"status":401,...} — so the two failure classes are tellable
//     apart, which is what makes a server-side "Test" button honest here.
//
//   TCGplayer — its public API is closed to new keys and every documented
//     order endpoint is `Stores_*` (seller-side, needs the store's own
//     bearer). There is NO buyer order-history API. What the logged-in site
//     itself calls is the private SPA gateway: `GET {base}/customers/{id}/orders`
//     (cookie-authenticated, `withCredentials`), discovered by mining the
//     site's shipped JS bundle — the shape is `{ data: [...orders] }` style
//     list. Because that is an UNVERIFIED private contract (it can change
//     without notice, and we could not exercise it without a logged-in
//     account), the TCGplayer path is deliberately forgiving: it probes the
//     id, the me-alias, and the list endpoint, tolerates either response
//     shape, and — when the shape is not recognizable — returns the observed
//     keys so the Settings panel can say exactly what came back instead of a
//     silent "no cards". It also accepts a paste-JSON escape hatch: the
//     browser's own network response for that call can be pasted and parsed
//     locally, so the feature keeps working even if the private contract
//     shifts. Cookies are the user's own session, entered once in Settings.
const crypto = require('crypto');

// --- endpoints (pinned; never user-supplied, so the saved credential can only
// ever be sent where the user themselves chose to send it) ---
const MANAPOOL_BASE = process.env.MANAPOOL_API_BASE || 'https://manapool.com/api/v1';
// Live-verified 2026-09-17 against a real buyer account (responses captured to
// test fixtures). Two things the published examples do not tell you: the buyer
// list is GET /buyer/orders (page/per_page are IGNORED — it answers the whole
// set, oldest first), and the detail is GET /buyer/orders/{uuid}, where the id
// is the row's `id`, NOT the six-digit order_number a human reads. Asking the
// doc route for a number 400s with "Invalid path parameters: id: Invalid UUID",
// so a human number has to be resolved through the list first.
const MANAPOOL_LIST_PATH = '/buyer/orders';
const RECENT_LIMIT = 3;
const MANAPOOL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const TCG_GATEWAY_BASES = [
  process.env.TCG_ORDER_BASE, // single-base override for testing
].filter(Boolean).length ? [process.env.TCG_ORDER_BASE] : [
  'https://mpapi.tcgplayer.com/',
  'https://mpgateway.tcgplayer.com/',
];
const TCG_REFERER = 'https://www.tcgplayer.com/';
const HTTP_TIMEOUT_MS = Number(process.env.MARKETPLACE_TIMEOUT_MS || 16000);

// The legal collection.printing values (mirrors the CHECK constraint in db.js).
// Order lines call foil variants "Foil"/"Etched"; both map to Holofoil, the
// only foil treatment Magic cards actually have and the one the DB accepts.
const PRINTING_VALUES = ['Normal', 'Holofoil'];
const VALID_CONDITIONS = ['Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged'];

// Line condition codes the two markets use, mapped onto our CHECK-constrained
// vocabulary. Anything unmapped (or absent) falls back to the caller's default
// rather than being invented.
const CONDITION_MAP = {
  NM: 'Near Mint', LP: 'Lightly Played', MP: 'Moderately Played', HP: 'Heavily Played', DAM: 'Damaged',
  MT: 'Damaged', // ManaPool "mint taped" is a damage grade, not a near-mint one
  GP: 'Lightly Played',
  EXC: 'Near Mint', VG: 'Moderately Played', G: 'Heavily Played', P: 'Damaged', // TCGplayer grading
  'Near Mint': 'Near Mint', 'Lightly Played': 'Lightly Played', 'Moderately Played': 'Moderately Played',
  'Heavily Played': 'Heavily Played', Damaged: 'Damaged',
};
function mapCondition(raw, fallback = 'Near Mint') {
  if (raw == null || raw === '') return fallback;
  const s = String(raw).trim();
  return CONDITION_MAP[s] || CONDITION_MAP[s.toUpperCase()] || fallback;
}
function isFoilish(item) {
  const flag = item.isFoil ?? item.foil ?? item.is_foil;
  if (flag === true || flag === 1 || flag === '1') return true;
  // ManaPool's live rows carry no foil word at all: the print is a two-letter
  // id on the product's `single` object, FO for foil and NF for normal. Matched
  // exactly rather than as a substring, so no other code can read as foil.
  const fid = String(item.finish_id ?? item.finishId ?? '').trim().toUpperCase();
  if (fid === 'FO' || fid === 'FOIL') return true;
  const finish = String(item.finish || item.printing || item.edge || '').toLowerCase();
  const name = String(item.name || item.productName || '').toLowerCase();
  if (/holofoil|etched|showcase|textless|overlay|borderless/.test(`${finish} ${name}`)) return true;
  if (/\bfoil\b/.test(name) && !/non-?foil/.test(name)) return true; // standalone word only: "foil" alone is too weak
  if (finish.includes('foil') && !finish.includes('non')) return true;
  return false;
}
// A sold card's unit price. ManaPool quotes cents (price_cents); TCGplayer's
// gateway uses dollars. Deciding on the VALUE's magnitude rather than trusting
// the field name is deliberate: a caller that mixes the two (or a gateway that
// changes dialect) then mis-prices by 100x in a way that reads as a bug —
// anything over $500 a card almost certainly is not, so that field is cents.
function unitPriceCents(item) {
  const cents = item.price_cents ?? item.priceCents ?? item.unit_price_cents;
  const dollars = item.price ?? item.unitPrice ?? item.pricePaid ?? item.unit_price;
  if (cents != null && Number.isFinite(Number(cents)) && Number(cents) >= 0) return Math.round(Number(cents));
  if (dollars == null || !Number.isFinite(Number(dollars)) || Number(dollars) < 0) return null;
  const d = Number(dollars);
  return d > 500 ? Math.round(d) : Math.round(d * 100);
}

// --- credentials normalisation -------------------------------------------------
// Cookies arrive either as a raw `Cookie:` header string or as a JSON array of
// {name,value,domain?} objects (what a browser export/extension hands over).
// Both normalise to a clean header string; junk that could not be read
// through is rejected rather than half-stored, because a broken cookie jar is
// indistinguishable from an expired one at fetch time.
function normalizeCookies(input) {
  if (typeof input !== 'string' || !input.trim()) {
    if (Array.isArray(input)) {
      const pairs = input
        .filter((c) => c && typeof c.name === 'string' && c.name && typeof c.value === 'string')
        .map((c) => `${c.name.trim()}=${c.value.trim()}`);
      return pairs.length ? pairs.join('; ') : null;
    }
    return null;
  }
  const raw = input.replace(/[\r\n]+/g, ' ').trim();
  if (raw.startsWith('cookie:') || raw.toLowerCase().startsWith('cookie:')) {
    // A pasted `Cookie:` header line is the whole jar, not a cookie pair; the
    // prefix itself would poison the very header we're assembling.
    return normalizeCookies(raw.replace(/^cookie:\s*/i, ''));
  }
  const pairs = raw.split(';').map((p) => p.trim()).filter((p) => p && p.includes('=') && !p.includes('\0'));
  const clean = pairs.map((p) => {
    const i = p.indexOf('=');
    return `${p.slice(0, i).trim()}=${p.slice(i + 1).trim()}`;
  });
  // De-dupe by name, last write wins (a re-export repeats names).
  const byName = new Map();
  for (const c of clean) byName.set(c.slice(0, c.indexOf('=')), c);
  const joined = [...byName.values()].join('; ');
  return joined.length && joined.length <= 64 * 1024 ? joined : null;
}
function cookieCount(cookies) {
  if (!cookies) return 0;
  return cookies.split(';').filter((p) => p.trim() && p.includes('=')).length;
}
// Never echo a token back over GET (the UI shows "configured?" state instead),
// and mask the account email so a shared screen/screenshot cannot leak it.
function maskSecret(s) {
  const str = String(s || '');
  if (!str) return '';
  if (str.length <= 8) return '••••';
  return `${str.slice(0, 3)}…${str.slice(-4)} (${str.length} chars)`;
}
function maskEmail(email) {
  const str = String(email || '');
  const at = str.indexOf('@');
  if (at < 1) return str ? `${str.slice(0, 1)}…` : '';
  const [user, domain] = [str.slice(0, at), str.slice(at + 1)];
  return `${user.slice(0, 2)}${'•'.repeat(Math.max(2, Math.min(8, user.length - 2)))}@${domain}`;
}
// The gateway wants a customer id; the SPA keeps it in a same-origin cookie,
// so we can often discover it without the user. Try the obvious cookie names
// before any network call (a cheap win when they exist).
function customerIdHints(cookies) {
  const out = [];
  for (const pair of String(cookies || '').split(';')) {
    const i = pair.indexOf('=');
    if (i < 0) continue;
    const name = pair.slice(0, i).trim().toLowerCase();
    const value = pair.slice(i + 1).trim();
    if (value && /^(tcg_)?(customer|customerid|cust_id|customer_id|person|member|buyer|party|account)([_-]?id)?$/i.test(name)) {
      out.push(value);
    }
  }
  return [...new Set(out)].slice(0, 5);
}

// --- order fetch ----------------------------------------------------------------
// Both fetchers share the shape: return the parsed JSON body plus enough
// context for the route's error copy. `validateStatus: () => false` so we can
// read the body of a 401/404 and tell a bad credential from a bad order number
// — an honest error is worth more than a status code.
async function httpGet(url, headers = {}) {
  const axios = require('axios');
  try {
    const res = await axios.get(url, {
      headers: { 'User-Agent': 'Scrybox', Accept: 'application/json', ...headers },
      timeout: HTTP_TIMEOUT_MS,
      maxRedirects: 3,
      validateStatus: () => true,
    });
    let body = res.data;
    if (typeof body === 'string') {
      try { body = JSON.parse(body); } catch { body = { raw: body }; }
    }
    return { status: res.status, body };
  } catch (err) {
    throw Object.assign(new Error(`Marketplace request failed: ${err.message}`), { status: 502, cause: err });
  }
}

// ManaPool hangs its line items one level deeper than the docs imply: the buyer
// detail nests them per seller under order_seller_details[].items, and the list
// rows carry no items at all. Reading only the top level (what LINE_KEYS
// covers) silently yields zero lines, which is how the recent-orders picker
// came to list every order as 0 cards. These readers flatten that nesting, and
// they are additive: a body that already exposes top-level items (TCGplayer)
// never reaches them.
function unwrapManapoolOrder(body) {
  if (!body || typeof body !== 'object') return null;
  if (body.order && typeof body.order === 'object') return body.order;
  return body;
}
function manapoolNestedItems(order) {
  const sellers = order && order.order_seller_details;
  if (!Array.isArray(sellers)) return [];
  const out = [];
  for (const s of sellers) {
    if (s && Array.isArray(s.items) && s.items.length) out.push(...s.items);
  }
  return out;
}
// The live rows encode print and grade as ids, not words: finish_id FO/NF and
// condition_id NM/LP/MP/HP/DAM, on the product's `single` object.
function isManapoolSealed(product) {
  if (!product || typeof product !== 'object') return false;
  const type = String(product.product_type || '').toLowerCase();
  if (type && !/^mtg_single$/.test(type)) return true;
  return Boolean(product.sealed) && !product.single;
}

function mpHeaders(email, token) {
  return { 'X-ManaPool-Email': email, 'X-ManaPool-Access-Token': token };
}
// One auth/rate guard for every ManaPool call. An auth rejection is an auth
// problem, not an empty order: saying "we could not find order X" when the
// token is dead sends the user hunting for a typo in the wrong field.
function mpGuard(r, what) {
  if (r.status === 401 || r.status === 403) {
    throw Object.assign(new Error('ManaPool rejected the saved credentials. Check the account email and the access token under ManaPool → account → Integration settings, then save them again.'), { status: 401 });
  }
  if (r.status === 429) {
    throw Object.assign(new Error('ManaPool is rate-limiting this account right now (too many requests or too much traffic). Wait a minute and try again.'), { status: 429 });
  }
  return r.status >= 200 && r.status < 300;
}
function hasOrderShape(body) {
  if (!body || typeof body !== 'object') return false;
  if (body.order && typeof body.order === 'object') return true;
  if (Array.isArray(body.orders)) return true;
  return LINE_KEYS.some((k) => Array.isArray(body[k]) && body[k].length);
}
function sumItemCounts(order) {
  const sellers = order && order.order_seller_details;
  if (!Array.isArray(sellers)) return 0;
  return sellers.reduce((n, s) => n + (Number(s && s.item_count) || 0), 0);
}

// The buyer list. `page`/`per_page` are accepted by the route but IGNORED by
// the live API (it answers the whole set) and the rows come back oldest first,
// so anything that shows "most recent" has to sort them itself.
async function fetchManapoolOrderList({ email, token, httpGet: http = httpGet } = {}) {
  if (!email || !token) throw Object.assign(new Error('ManaPool credentials are not configured'), { status: 400 });
  const r = await http(`${MANAPOOL_BASE}${MANAPOOL_LIST_PATH}`, mpHeaders(email, token));
  if (!mpGuard(r, 'list')) {
    throw Object.assign(new Error(`ManaPool did not return an order list (status ${r.status}).`), { status: 502, listUnavailable: true });
  }
  return orderArray(r.body);
}

// One order's detail. The live detail route is keyed by the row's UUID `id`,
// NOT the six-digit order_number a human reads off the site, so a human number
// is resolved through the list first. Asking the detail route for a number
// 400s with "Invalid path parameters: id: Invalid UUID", which is why this
// indirection is not optional. A number absent from the list still gets one
// direct attempt, so a provider that does speak by-number keeps working.
async function fetchManapoolOrder({ email, token, orderNumber, httpGet: http = httpGet }) {
  if (!email || !token) throw Object.assign(new Error('ManaPool credentials are not configured'), { status: 400 });
  const num = String(orderNumber || '').trim();
  if (!/^[A-Za-z0-9_-]{1,64}$/.test(num)) throw Object.assign(new Error('Order number is not valid'), { status: 400 });
  const headers = mpHeaders(email, token);

  let id = num;
  if (!MANAPOOL_UUID.test(num)) {
    let rows = [];
    try { rows = await fetchManapoolOrderList({ email, token, httpGet: http }); } catch { rows = []; }
    const hit = rows.find((o) => orderNumberMatches(o, num));
    const cand = hit ? pick(hit, ['id', 'order_id', 'orderId', 'uuid']) : null;
    if (cand != null) id = String(cand);
  }

  const r = await http(`${MANAPOOL_BASE}/buyer/orders/${encodeURIComponent(id)}`, headers);
  const is2xx = mpGuard(r, 'detail');
  if (!is2xx) {
    const err = new Error(`ManaPool has no order ${num} for this account. Check the number on ManaPool → account → Orders.`);
    // The route answering "not found", and the uuid-keyed route refusing a
    // human number as malformed (which is what happens when the number is not
    // in the list either), are both "that order is not yours". Only a genuinely
    // unhappy upstream counts as 502.
    err.status = (r.status === 404 || r.status === 400) ? 404 : 502;
    err.observedKeys = bodyKeys(r && r.body);
    throw err;
  }
  // A 2xx is handed to parseOrderPayload even when it is not order-shaped: that
  // is the reader that reports the observed keys of an unrecognisable body
  // (422), which is how private-contract drift stays visible instead of
  // collapsing into a bare "not found".
  return r;
}

// The picker's data. The card count CANNOT come from the list rows: they carry
// no items at all (reading a top-level `items` there is exactly the bug that
// showed every order as 0 cards), and the list's own `item_count` disagrees
// with the real line rows on orders with refunds/replacements. The
// authoritative count is the detail's nested per-seller items, so the handful
// of shown rows are fetched. One detail failing degrades to the list number
// rather than failing the picker.
async function manapoolRecentOrderSummaries({ email, token, limit = RECENT_LIMIT, offset = 0, httpGet: http = httpGet, meta = null } = {}) {
  const rows = await fetchManapoolOrderList({ email, token, httpGet: http });
  const stampOf = (o) => String(pick(o, ['created_at', 'createdAt', 'created', 'date', 'placedAt', 'orderDate']) || '');
  const numbered = [...rows]
    .filter((r) => pick(r, ['order_number', 'orderNumber', 'number', 'id']) != null)
    .sort((a, b) => stampOf(b).localeCompare(stampOf(a)));
  const start = Math.max(0, Math.floor(Number(offset) || 0));
  if (meta) meta.total = numbered.length;
  const dated = numbered.slice(start);
  const out = [];
  for (const row of dated) {
    if (out.length >= limit) break;
    const number = pick(row, ['order_number', 'orderNumber', 'number', 'id']);
    if (number == null) continue;
    const summary = {
      number: String(number),
      placedAt: stampOf(row) || null,
      status: pick(row, ['status', 'orderStatus', 'fulfillmentStatus', 'fulfillment_status']) || null,
      cardCount: sumItemCounts(row),
      lineCount: 0,
      total_cents: pick(row, ['total_cents', 'totalCents', 'total', 'orderTotal']) ?? null,
    };
    const id = pick(row, ['id', 'order_id', 'orderId', 'uuid']);
    if (id != null && MANAPOOL_UUID.test(String(id))) {
      try {
        const detail = await fetchManapoolOrder({ email, token, orderNumber: String(id), httpGet: http });
        const order = unwrapManapoolOrder(detail.body);
        if (order) {
          const lines = orderLines(order, { includeExtras: false });
          summary.cardCount = lines.__cardCopies;
          summary.lineCount = lines.__cardLines;
        }
      } catch { /* keep the list's own count */ }
    }
    out.push(summary);
  }
  return out;
}

async function fetchTcgOrder({ cookies, customerId, orderNumber, httpGet: http = httpGet, requireMatch = true }) {
  if (!cookies) throw Object.assign(new Error('TCGplayer session cookie is not configured'), { status: 400 });
  const num = String(orderNumber || '').trim();
  if (!/^[A-Za-z0-9_-]{1,64}$/.test(num)) throw Object.assign(new Error('Order number is not valid'), { status: 400 });
  // requireMatch=false is the recent-list path: the caller wants the LIST,
  // not a specific order, so a 404 on every probe path means "no list",
  // not "unknown number" - keep the status honest for that caller.
  const headers = { Cookie: cookies, Referer: TCG_REFERER };
  const ids = [customerId, ...customerIdHints(cookies)].filter(Boolean);
  const seen = new Set();
  let last = null;
  for (const base of TCG_GATEWAY_BASES) {
    const paths = [];
    for (const id of ids) {
      if (!seen.has(String(id))) { seen.add(String(id)); paths.push(`customers/${encodeURIComponent(id)}/orders`); }
    }
    paths.push('customers/me/orders');
    for (const p of paths) {
      const url = `${base}${p}${p.includes('?') ? '&' : '?'}per_page=200&page=1`;
      const r = await http(url, headers);
      last = r;
      if (r.status >= 200 && r.status < 300 && looksLikeOrderList(r.body)) return { ...r, path: p };
      if (r.status === 401 || r.status === 403) {
        throw Object.assign(new Error('TCGplayer rejected the saved cookies (session expired or password-protected). Re-export them from a logged-in browser and save them again in Settings.'), { status: 401 });
      }
      // A gateway that 404s this path just does not speak it; keep looking.
    }
  }
  if (last && last.status >= 200 && last.status < 300) {
    // 2xx but not a recognisable list — report what DID come back rather than
    // a bare "no orders", so the private-contract drift is visible.
    return { ...last, unrecognised: true, keys: bodyKeys(last.body) };
  }
  throw Object.assign(new Error(`TCGplayer did not return an order list (last status ${last ? last.status : 'none'}).`), { status: 502 });
}
function bodyKeys(body) {
  if (!body || typeof body !== 'object') return [];
  const keys = Object.keys(body);
  return keys.length <= 25 ? keys : [...keys.slice(0, 25), `(+${keys.length - 25} more)`];
}
// One row proves the shape: an array of order-ish objects, under any of the
// keys the gateway/SPA dialects put their list under.
function looksLikeOrderList(body) {
  if (Array.isArray(body)) return body.length > 0 && body.every((o) => o && typeof o === 'object');
  if (!body || typeof body !== 'object') return false;
  for (const k of ['data', 'orders', 'results', 'items', 'list', 'records', 'orderResults']) {
    const v = body[k];
    if (Array.isArray(v) && v.length && v.every((o) => o && typeof o === 'object')) return true;
    if (v && typeof v === 'object' && !Array.isArray(v)) {
      const inner = v.data || v.orders || v.results;
      if (Array.isArray(inner) && inner.length && inner.every((o) => o && typeof o === 'object')) return true;
    }
  }
  return false;
}
function orderArray(body) {
  if (Array.isArray(body)) return body;
  if (!body || typeof body !== 'object') return [];
  for (const k of ['data', 'orders', 'results', 'items', 'list', 'records', 'orderResults']) {
    const v = body[k];
    if (Array.isArray(v) && v.length && v.every((o) => o && typeof o === 'object')) return v;
    if (v && typeof v === 'object') {
      const inner = v.data || v.orders || v.results;
      if (Array.isArray(inner) && inner.length) return inner;
    }
  }
  return [];
}

// --- the recent-order list ("most recent 3") ---------------------------------
// The picker's data lives in the source's own order list; see
// manapoolRecentOrderSummaries (whose card counts need the detail) and
// fetchTcgRecentOrders below.

async function fetchTcgRecentOrders({ cookies, customerId, httpGet: http = httpGet } = {}) {
  // The recent-list IS what fetchTcgOrder's path-probing already retrieves
  // (per_page=200, page 1); reuse it without requiring an order number by
  // passing a sentinel the matcher ignores.
  return fetchTcgOrder({ cookies, customerId, orderNumber: '0', httpGet: http, requireMatch: false });
}

// Normalise a provider list response into the small header-only shape the
// picker UI needs. Deliberately NO addresses/emails/payment data.
function recentOrderSummaries(body, limit = RECENT_LIMIT) {
  const list = orderArray(body);
  const rows = list.map((o) => {
    const lines = orderLines(o, { includeExtras: false });
    const number = pick(o, ['number', 'orderNumber', 'order_number', 'friendly_id', 'friendlyId', 'id', 'orderId', 'order_id']);
    const placed = pick(o, ['created', 'createdAt', 'created_at', 'date', 'placedAt', 'orderDate']);
    return {
      number: number != null ? String(number) : null,
      placedAt: placed || null,
      status: pick(o, ['status', 'orderStatus', 'fulfillmentStatus', 'fulfillment_status']) || null,
      cardCount: lines.__cardCopies || 0,
      lineCount: lines.length,
      total_cents: pick(o, ['total_cents', 'totalCents', 'total', 'orderTotal']) ?? null,
    };
  }).filter((r) => r.number);
  // Newest first on whatever timestamp the provider gave; providers that sort
  // already are left untouched.
  const withDates = rows.filter((r) => r.placedAt);
  if (withDates.length === rows.length && rows.length > 1) {
    rows.sort((a, b) => String(b.placedAt).localeCompare(String(a.placedAt)));
  }
  return rows.slice(0, limit);
}

// --- normalising one order's lines ---------------------------------------------
// Both providers arrive as: a header (number/date/status/prices) + line items.
// We keep only what the filing path needs and drop the rest (addresses, payment
// methods, tracking — none of that belongs in a collection tool, and holding
// it in memory/server logs would be pure PII sprawl).
function pick(obj, names) {
  for (const n of names) {
    if (obj && obj[n] != null && obj[n] !== '') return obj[n];
  }
  return null;
}
const LINE_KEYS = ['items', 'lineItems', 'line_items', 'products', 'orderItems', 'cards', 'contents', 'entries'];

// Flatten the nested product forms the two markets use: ManaPool puts the card
// under `product.single` (or `.sealed`), TCGplayer rows carry `card`/`sku` or
// the `single` form directly. Returns the product object to read identity off
// and the wider row that carries quantity/price/condition.
function eachProductLines(item) {
  const p = item.product;
  if (p && typeof p === 'object') {
    const s = p.single || p.sealed || p.product || p;
    if (s && typeof s === 'object') return [s, p];
  }
  for (const k of ['card', 'single', 'sku', 'printing']) {
    if (item[k] && typeof item[k] === 'object') return [item[k], item];
  }
  return [item, item];
}

// Is this a sealed product (or accessory) rather than an individual card? The
// user asked for the ENDLINES of an order — the cards themselves — not booster
// boxes and storage gizmos, which come back as "products" with no card
// identity. Sealed detections are counted and reported, never filed as cards,
// unless the caller explicitly asked for extras.
const SEALED_KINDS = ['sealed', 'bundle', 'box', 'case', 'display', 'accessory', 'accessories', 'supplies'];
function isSealedLine(prod, item) {
  const kind = String(pick(prod, ['kind', 'product_type', 'productType', 'type', 'category'])
    ?? pick(item, ['kind', 'product_type', 'productType', 'type', 'category']) ?? '').toLowerCase();
  const sealedFlag = pick(prod, ['is_sealed', 'isSealed', 'sealed']) ?? pick(item, ['is_sealed', 'isSealed', 'sealed']);
  if (sealedFlag === true || sealedFlag === 1 || sealedFlag === '1') return true;
  if (SEALED_KINDS.includes(kind)) return true;
  const name = String(pick(prod, ['name', 'title']) ?? pick(item, ['name', 'productName']) ?? '').toLowerCase();
  if (/\b(sealed|booster|bundle|display|case|brick)\b/.test(name)) return true;
  if (/\bbox(es)?\b|binder|sleeves|protector|playmat|deck box|storage/i.test(name)
      && !pick(prod, ['scryfallId', 'scryfall_id', 'tcgplayerProductId', 'product_id'])
      && !pick(prod, ['set', 'setCode', 'set_code'])) return true;   // accessory with no card identity
  return false;
}

function orderLines(raw, opts = {}) {
  const includeExtras = opts.includeExtras === true;
  const src = raw && typeof raw === 'object' ? raw : {};
  let lines = [];
  for (const k of LINE_KEYS) {
    if (Array.isArray(src[k]) && src[k].length && src[k].every((x) => x && typeof x === 'object')) { lines = src[k]; break; }
  }
  if (!lines.length && Array.isArray(src.orderItems)) lines = src.orderItems;
  if (!lines.length) lines = manapoolNestedItems(src);
  const out = [];
  let extras = 0;
  for (const item of lines) {
    const [prod, ctx] = eachProductLines(item);
    // A single sold card can be spread across forms; check both the row and the
    // product for sealed markers before believing it is a card.
    const sealedLine = isSealedLine(prod, item) || isManapoolSealed(item.product);
    if (sealedLine) { extras += 1; if (!includeExtras) continue; }
    const name = String(pick(prod, ['name', 'cardName', 'title']) || pick(item, ['name', 'productName', 'description']) || '').trim();
    const set = pick(prod, ['set', 'setCode', 'set_code', 'expansion', 'expansionName']) || pick(item, ['set', 'setCode']);
    const number = pick(prod, ['number', 'collectorNumber', 'cardNumber', 'collector_number']) ?? pick(item, ['number', 'collectorNumber']);
    const qty = Number(pick(item, ['quantity', 'qty', 'count', 'quantityFulfilled']) ?? pick(prod, ['quantity']) ?? 1);
    const cents = unitPriceCents(item) ?? unitPriceCents(prod);
    const scryfall = pick(prod, ['scryfallId', 'scryfall_id']) || pick(item, ['scryfallId']) || null;
    const tcgIdRaw = pick(prod, ['tcgplayerProductId', 'tcgplayer_product_id', 'product_id', 'id'])
      ?? pick(item, ['tcgplayerProductId', 'product_id']);
    // With no name and no identity at all, there is nothing to resolve: report
    // it unresolved rather than file a blank row.
    let junkLine = false;
    if (!name && !scryfall && !(set && number) && !(tcgIdRaw != null && /^\d+$/.test(String(tcgIdRaw)))) {
      junkLine = true; extras += 1; if (!includeExtras) continue;
    }
    out.push({
      name,
      set_code: set ? String(set).trim().toLowerCase() : null,
      number: number != null && number !== '' ? String(number).trim() : null,
      scryfall_id: scryfall,
      tcgplayer_product_id: tcgIdRaw != null && /^\d+$/.test(String(tcgIdRaw)) ? String(tcgIdRaw) : null,
      quantity: Number.isFinite(qty) && qty > 0 ? Math.floor(qty) : 1,
      price_cents: cents,
      is_foil: isFoilish(item) || isFoilish(prod),
      condition: mapCondition(pick(item, ['condition', 'conditionCode', 'grade', 'conditionText', 'condition_id']) ?? pick(prod, ['condition', 'conditionCode', 'condition_id'])),
      year: pick(prod, ['year', 'printed_year', 'printedYear']) || null,
      // Whether this line is a card at all. A sealed box is a real line the user
      // may choose to file, but it is not a card: it must not be counted as one,
      // and it must not be described as a card that failed to match.
      sealed: sealedLine,
      junk: junkLine,
    });
  }
  out.__extras = extras;
  // Copies on card lines only. `out.length` counts everything the caller asked
  // to keep, which with sealed included would let a booster box inflate a
  // "N copies" card tally.
  out.__cardCopies = out.reduce((n, l) => n + (l.sealed || l.junk ? 0 : (l.quantity || 1)), 0);
  out.__cardLines = out.filter((l) => !l.sealed && !l.junk).length;
  return out;
}
// One order-detail object normalised (both providers, list or detail form).
function parseOrderPayload(body, wantNumber, opts = {}) {
  const list = orderArray(body);
  const direct = body && typeof body === 'object' && !Array.isArray(body)
    && LINE_KEYS.some((k) => Array.isArray(body[k]) && body[k].length);
  let order = null;
  if (direct) order = body;
  else if (body && typeof body === 'object' && body.order && typeof body.order === 'object') order = body.order;
  else if (list.length) {
    const num = String(wantNumber || '').trim();
    order = list.find((o) => orderNumberMatches(o, num)) || (num ? null : list[0]);
  }
  if (!order) {
    const err = new Error('The order response did not include any usable line items.');
    err.status = 422;
    err.observedKeys = bodyKeys(body);
    throw err;
  }
  const lines = orderLines(order, opts);
  return {
    number: String(pick(order, ['number', 'orderNumber', 'order_number', 'id', 'orderId']) ?? wantNumber ?? '').trim() || null,
    placedAt: pick(order, ['created', 'createdAt', 'created_at', 'date', 'placedAt', 'orderDate']) || null,
    status: pick(order, ['status', 'orderStatus', 'fulfillmentStatus', 'fulfillment_status']) || null,
    currency: pick(order, ['currency', 'currencySymbol']) || null,
    total_cents: pick(order, ['total_cents', 'totalCents', 'total', 'orderTotal']) ?? pick(order.payment, ['total_cents', 'subtotal_cents']) ?? null,
    lines,
    lineCount: lines.length,
  };
}
function orderNumberMatches(order, want) {
  if (!want) return true;
  const cands = [];
  for (const k of ['number', 'orderNumber', 'order_number', 'friendly_id', 'friendlyId', 'name', 'id', 'orderId', 'order_id', 'key', 'reference']) {
    if (order[k] != null) cands.push(String(order[k]));
  }
  const norm = want.replace(/^#/, '').trim().toLowerCase();
  return cands.some((c) => c.toLowerCase() === norm || c.toLowerCase() === `#${norm}` || c.toLowerCase() === `order-${norm}`);
}

// --- the collection write -------------------------------------------------------
// The filing path is the SAME bulk-add core the tray and Secret Lair importer
// use; entries carry per-card quantity/price overrides so a multi-copy order
// line files as one stack at the right unit price instead of N inserts. The
// core merges shared condition/printing/language under the per-entry fields
// (collection.js:546), which is exactly what order lines need.
async function addOrderToCollection({ user, lines, condition, printingMode = 'auto', language = 'English', copies = 1, deps: depsOption = {} } = {}, depsArg = {}) {
  // `previewOrder` takes its injected collaborators as a SECOND argument; this
  // function historically took them inside the options object. Accept both, with
  // the positional form winning, because an asymmetric signature is a footgun: a
  // caller who passes `{...} , deps` here would have their stubs silently
  // ignored and the util would reach for the real network/db instead of failing
  // — which reads as "the test passed" while nothing was actually injected.
  const deps = { ...depsOption, ...depsArg };
  if (!user || !user.id) throw Object.assign(new Error('user required'), { status: 400 });
  if (!Array.isArray(lines) || !lines.length) return { added: 0, failed: [], resolved: 0, totalListed: 0, unresolved: 0, message: 'No cards to add' };
  const resolve = deps.bulkResolve || require('../scryfallApi').bulkFetchByIdentifier;
  const cache = deps.cacheCards || require('./cardCache').cacheNormalizedCards;
  const bulk = deps.bulkAdd || require('../routes/collection').bulkAddToCollection;
  if (typeof bulk !== 'function') throw Object.assign(new Error('bulk add service unavailable'), { status: 500 });

  const cond = VALID_CONDITIONS.includes(condition) ? condition : 'Near Mint';
  const mult = Math.max(1, Math.min(Number(copies) || 1, 500));

  // Collapse duplicate printings of the same line (a deck list can name the
  // same card twice) before resolving, then file with each line's own count.
  const uniq = new Map();
  for (const l of lines) {
    // A sealed box or an identity-less row has no card to file. It stays in the
    // order tally (so the order never looks short) but never reaches the
    // collection as a card that "failed to match" — those lines are not cards.
    if (l.sealed === true || l.junk === true) continue;
    const key = l.scryfall_id
      ? `i:${l.scryfall_id.toLowerCase()}`
      : `n:${String(l.name).toLowerCase()}|${String(l.set_code || '').toLowerCase()}|${String(l.number || '').toLowerCase()}|${l.is_foil ? 'f' : 'n'}`;
    if (uniq.has(key)) { uniq.get(key).quantity += l.quantity; continue; }
    uniq.set(key, { ...l });
  }
  const rows = [...uniq.values()].map((l) => ({
    id: l.scryfall_id || undefined,
    set_id: l.set_code || undefined,
    number: l.number || undefined,
    name: l.name || undefined,
    _line: l,
  }));

  // When the caller did not ask for extras, the sealed/accessory lines were
  // already dropped by orderLines; carry their count so the UI can say "3
  // non-card line(s) ignored" instead of the user wondering if the order was
  // read short. The card count shown is the cards, never the padded total.
  const extrasHeld = Number(lines.__extras || 0);
  const { cards, pairs, notFound } = await resolve(rows);
  if (cards && cards.length) await cache(cards);

  const plan = [];
  const seen = new Set();
  for (const p of pairs || []) {
    const card = p && p.card;
    if (!card || !card.id || seen.has(card.id)) continue;
    seen.add(card.id);
    const line = (p.row && p.row._line) || {};
    const printing = printingMode === 'foil' ? 'Holofoil'
      : printingMode === 'nonfoil' ? 'Normal'
        : (line.is_foil ? 'Holofoil' : 'Normal');
    plan.push({
      card_id: card.id,
      quantity: Math.max(1, Math.round((line.quantity || 1) * mult)),
      printing,
      purchase_price: line.price_cents != null ? Number((Number(line.price_cents) / 100).toFixed(2)) : undefined,
      condition: line.condition && VALID_CONDITIONS.includes(line.condition) ? line.condition : cond,
    });
  }
  let added = 0;
  const failed = [];
  for (const printing of ['Normal', 'Holofoil']) {
    const group = plan.filter((e) => e.printing === printing);
    if (!group.length) continue;
    const res = await bulk(user, group, { condition: cond, printing, language, stackable: true });
    // Count COPIES, not card types: an order line of 4x one card is 4 cards.
    const qtyById = new Map(group.map((e) => [e.card_id, e.quantity || 1]));
    for (const a of (res && res.added) || []) added += qtyById.get(a.card_id) || 1;
    if (res && res.failed) failed.push(...res.failed);
  }
  return {
    added,
    failed,
    resolved: plan.length,
    // "What the order contained", including the non-card lines that were held
    // out of the file-in. Reporting only the card rows would make a sealed-heavy
    // order look like it came up short, which is exactly the doubt this number
    // exists to answer; `resolved` is the card-only count next to it.
    totalListed: uniq.size + extrasHeld,
    // Only genuine card lines that the resolver could not identify count as
    // unresolved; sealed/accessory rows are reported by `extras`, not here.
    unresolved: notFound || 0,
  };
}


// Resolve an order's lines to real card_cache rows WITHOUT filing anything, so
// the UI can show art, names, matched/not-matched and totals before the user
// commits. Shares the resolver and the dedup key with addOrderToCollection on
// purpose: what the preview shows is exactly what the add will file, so a card
// that resolves here cannot fail to resolve there, and the count the user reads
// is the count that lands.
//
// Resolution rides bulkFetchByIdentifier (scryfallId -> set+number -> name), the
// same path the Secret Lair and precon importers use, so an order behaves like
// every other bulk add in the app: unmatched lines are reported, never faked.
async function previewOrder({ lines, userId, includeExtras = false }, deps = {}) {
  if (!Array.isArray(lines) || !lines.length) {
    return { cards: [], totalListed: 0, resolvedCount: 0, unresolved: 0, extras: 0, sealedCount: 0 };
  }
  const resolve = deps.bulkResolve || require('../scryfallApi').bulkFetchByIdentifier;
  const cache = deps.cacheCards || require('./cardCache').cacheNormalizedCards;
  const database = deps.db || require('../db');

  // Collapse the same printing named twice (a deck-list order can repeat a
  // card across sections) before resolving; carry the summed count for the row.
  const uniq = new Map();
  for (const l of lines) {
    // Not a card: kept for the order tally and the price total, but never sent
    // to the resolver, so a booster box cannot show up as a card you "own" or as
    // a line that failed to match.
    if (l.sealed === true || l.junk === true) continue;
    const key = l.scryfall_id
      ? `i:${String(l.scryfall_id).toLowerCase()}`
      : `n:${String(l.name || '').toLowerCase()}|${String(l.set_code || '').toLowerCase()}|${String(l.number || '').toLowerCase()}|${l.is_foil ? 'f' : 'n'}`;
    if (uniq.has(key)) { uniq.get(key).quantity += l.quantity; continue; }
    uniq.set(key, { ...l });
  }
  // The row doubles as the identity handle: the resolver echoes the exact object
  // it was handed back on pairs[].row, so hanging the line off it survives the
  // round trip and reads back by object identity — no fragile re-keying of a
  // resolver that may normalise case or pick a different identifier.
  const rows = [...uniq.values()].map((l) => ({
    id: l.scryfall_id || undefined,
    set_id: l.set_code || undefined,
    number: l.number || undefined,
    name: l.name || undefined,
    _line: l,
  }));

  // When the caller did not ask for extras, the sealed/accessory lines were
  // already dropped by orderLines; carry their count so the UI can say "3
  // non-card line(s) ignored" instead of the user wondering if the order was
  // read short. The card count shown is the cards, never the padded total.
  const extrasHeld = Number(lines.__extras || 0);
  const { cards, pairs, notFound } = await resolve(rows);
  if (cards && cards.length) await cache(cards);

  const resolved = [];
  const seen = new Set();
  for (const p of pairs || []) {
    const card = p && p.card;
    if (!card || !card.id || seen.has(card.id)) continue;
    seen.add(card.id);
    const line = (p.row && p.row._line) || {};
    resolved.push({
      card_id: card.id,
      name: card.name || line.name || '',
      image_url: card.image_url || null,
      set_code: card.set_id || line.set_code || null,
      number: card.number != null ? String(card.number) : (line.number || null),
      quantity: line.quantity || 1,
      is_foil: Boolean(line.is_foil),
      condition: line.condition || null,
      price_cents: line.price_cents != null ? line.price_cents : null,
      matched: true,
    });
  }

  // Ownership, the way the Secret Lair preview counts it: SUM(quantity) grouped
  // per card_id for THIS user only (never COUNT(rows) — a stack is one row), so
  // the UI can badge "you already have 3 of these" before an add.
  let owned = new Map();
  if (userId && resolved.length) {
    const ids = resolved.map((r) => r.card_id).filter(Boolean);
    if (ids.length) {
      const placeholders = ids.map(() => '?').join(',');
      const rowsOwned = await database.all(
        `SELECT card_id, SUM(quantity) AS qty FROM collection
          WHERE user_id = ? AND card_id IN (${placeholders})
          GROUP BY card_id`,
        [userId, ...ids]
      );
      for (const r of rowsOwned) owned.set(String(r.card_id).toLowerCase(), Number(r.qty) || 0);
    }
  }

  const totalCopies = [...uniq.values()].reduce((n, l) => n + (l.quantity || 1), 0);
  const cents = lines.reduce((n, l) => n + (l.price_cents != null ? l.price_cents * (l.quantity || 1) : 0), 0);
  return {
    cards: resolved.map((r) => ({ ...r, owned: owned.get(String(r.card_id).toLowerCase()) || 0 })),
    // Counted the same way the commit reports it: what the order listed, sealed
    // and accessories included, so `totalListed` = cards filed + held-out extras
    // and the summary line adds up in front of the user.
    totalListed: uniq.size + extrasHeld,
    totalCopies,
    resolvedCount: resolved.length,
    unresolved: (notFound || 0),
    extras: extrasHeld,
    sealedCount: extrasHeld,
    // Whether the user asked to keep the non-card lines. They are never FILED
    // either way (a booster box is not a card), but the wording the UI picks
    // depends on it: "left out" is wrong when the user deliberately included
    // them for the order total.
    extrasIncluded: !!includeExtras,
    price_cents: cents || null,
  };
}

// --- TCGplayer page import (bookmarklet) -------------------------------------
// The bookmarklet reads an order off the user's own logged-in TCGplayer page
// and hands over lines keyed by TCGplayer product id. That id is exactly what
// Scryfall stores as tcgplayer_id, so the printing is matched by identity, not
// by name: first against card_cache (already-known printings, English row
// preferred), then Scryfall's /cards/tcgplayer/:id for anything not cached.
// A product id that resolves nowhere (sealed, tokens TCGplayer sells that
// Scryfall lacks) stays unresolved and is reported, never guessed.
const TCG_PAGE_MAX_LINES = 1000;
function cleanTcgPageLines(raw) {
  if (!Array.isArray(raw)) return [];
  const out = [];
  for (const l of raw.slice(0, TCG_PAGE_MAX_LINES)) {
    if (!l || typeof l !== 'object') continue;
    const pid = String(l.tcgplayer_product_id ?? '').trim();
    if (!/^\d{1,10}$/.test(pid)) continue;
    const qty = Math.floor(Number(l.quantity));
    const cents = l.price_cents == null ? null : Math.round(Number(l.price_cents));
    out.push({
      tcgplayer_product_id: pid,
      name: String(l.name || '').slice(0, 200).trim(),
      quantity: Number.isFinite(qty) && qty > 0 && qty < 1000 ? qty : 1,
      price_cents: Number.isFinite(cents) && cents >= 0 && cents < 10_000_000 ? cents : null,
      is_foil: l.is_foil === true,
      condition: mapCondition(l.condition, null),
      order: l.order ? String(l.order).slice(0, 64) : null,
    });
  }
  return out;
}

async function resolveTcgPageLines(rawLines, deps = {}) {
  const lines = cleanTcgPageLines(rawLines);
  const database = deps.db || require('../db');
  const scryGet = deps.scryGet || require('../scryfallApi').scryGetRetried;
  const ids = [...new Set(lines.map((l) => l.tcgplayer_product_id))];
  const byPid = new Map();
  if (ids.length) {
    const ph = ids.map(() => '?').join(',');
    const rows = await database.all(
      `SELECT id, tcgplayer_product_id AS pid, language FROM card_cache
        WHERE id LIKE 'mtg-%' AND tcgplayer_product_id IN (${ph})
        ORDER BY CASE WHEN language IS NULL OR language = 'English' THEN 0 ELSE 1 END`,
      ids.map(Number)
    );
    for (const r of rows) {
      const k = String(r.pid);
      if (!byPid.has(k)) byPid.set(k, String(r.id).slice(4));
    }
  }
  for (const pid of ids) {
    if (byPid.has(pid)) continue;
    try {
      const resp = await scryGet(`/cards/tcgplayer/${pid}`);
      if (resp && resp.data && resp.data.id) byPid.set(pid, String(resp.data.id));
    } catch { /* 404: not a card Scryfall knows; stays unresolved */ }
  }
  const out = lines.map((l) => {
    const sid = byPid.get(l.tcgplayer_product_id) || null;
    return {
      name: l.name,
      set_code: null,
      number: null,
      scryfall_id: sid,
      tcgplayer_product_id: l.tcgplayer_product_id,
      quantity: l.quantity,
      price_cents: l.price_cents,
      is_foil: l.is_foil,
      condition: l.condition,
      year: null,
      sealed: false,
      // Unresolvable ids are not sent to the name resolver: TCGplayer product
      // names carry set/variant suffixes that would match the wrong printing.
      junk: !sid,
    };
  });
  out.__extras = out.filter((l) => l.junk).length;
  out.__cardCopies = out.reduce((n, l) => n + (l.junk ? 0 : l.quantity), 0);
  out.__cardLines = out.filter((l) => !l.junk).length;
  out.__unmatched = out.filter((l) => l.junk).map((l) => l.name || `#${l.tcgplayer_product_id}`);
  return out;
}

module.exports = {
  MANAPOOL_BASE,
  cleanTcgPageLines,
  resolveTcgPageLines,
  RECENT_LIMIT,
  normalizeCookies,
  cookieCount,
  maskSecret,
  maskEmail,
  customerIdHints,
  fetchManapoolOrder,
  fetchManapoolOrderList,
  manapoolRecentOrderSummaries,
  fetchTcgRecentOrders,
  recentOrderSummaries,
  fetchTcgOrder,
  parseOrderPayload,
  orderLines,
  mapCondition,
  isFoilish,
  unitPriceCents,
  addOrderToCollection,
  previewOrder,
};
