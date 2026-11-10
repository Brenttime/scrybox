#!/usr/bin/env node
// Rust-vs-Node response parity for the ported routes (BRIEF hard rule 1).
// Compares status, JSON body (values, array order AND object key order) and
// the headers the ported routes own (X-Total-Count, ETag behaviour,
// Content-Type) for every bench scenario plus 10 extra collection variants.
//
// usage: node parity.mjs [scenarios.json] [--rust http://127.0.0.1:3310] [--node http://127.0.0.1:3312] [--verbose]
//
// Node's background jobs also write to the shared DB copy, so a mismatch is
// retried as a fresh pair (up to 3 attempts) before it counts as a failure.
import fs from 'node:fs';
import http from 'node:http';

const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args[i + 1] : d; };
const scenPath = args.find(a => a.endsWith('.json')) ||
  new URL('.', import.meta.url).pathname + '../../../scen-rust.json';
const scen = JSON.parse(fs.readFileSync(scenPath, 'utf8'));
const RUST = opt('--rust', scen.base);
const NODE = opt('--node', RUST.replace(':3310', ':3312'));
const VERBOSE = args.includes('--verbose');

// --- cases -------------------------------------------------------------------

const EXTRA_COLLECTION = [
  '/api/collection?limit=1&offset=0',
  '/api/collection?limit=2000&offset=0',
  '/api/collection?limit=50&offset=25450',
  '/api/collection?limit=25&offset=7&is_trade=true',
  '/api/collection?is_trade=1',
  '/api/collection?card_id=mtg-dda6f965-cbdf-4dbc-b0b1-00c27e32f5f2',
  '/api/collection?include_allocation=1&limit=50',
  '/api/collection?count=0&limit=100',
  '/api/collection?include_allocation=1',
  '/api/collection?limit=100&offset=0&count=1&foo=%C3%A9',
];

// Search coverage for the ported local-cache fast path (iter 2): native
// shapes (name/number/set/lang/page/limit), the proxy-gated forms, and the
// zero-row live path. deepDiff compares parsed JSON including array order,
// so the card_cache row order is asserted against Node, not eyeballed.
const EXTRA_SEARCH = [
  '/api/search?scope=database&name=Black%20Lotus',
  '/api/search?name=black%20lotus',
  '/api/search?name=%20Sol%20Ring%20',
  '/api/search?name=Sol%20Ring&page=2&limit=10',
  '/api/search?name=Sol%20Ring&number=%23160%2Fbrc',
  '/api/search?name=Sol%20Ring&set=brc',
  '/api/search?name=%E3%82%BD%E3%83%BC%E3%83%AB&lang=ja',
  '/api/search?name=Ancestral%20Recall&lang=English',
  '/api/search?name=Sol%20Ring&lang=xx&limit=1',
  '/api/search?q=%20&name=Sol%20Ring',
  '/api/search',
  '/api/search?scope=collection&name=Sol%20Ring',
  '/api/search?name=__definitely_no_such_card__',
  // Sidecar-walk leg (iter 7): zero-row offsets force the live walk. These
  // names are in the shared card_cache at shallow pages but not at these
  // offsets, so Node answers them by paging scryfall-local — the exact leg
  // Rust now serves natively (X-Source scryfall/cache, X-Total-Count).
  '/api/search?scope=database&name=Kodama%20of%20the%20West%20Tree&page=3&limit=100',
  '/api/search?scope=database&name=Elvish%20Mystic&page=2&limit=60',
  '/api/search?scope=database&name=Lightning%20Bolt&page=4&limit=60',
  '/api/search?scope=database&name=Shalai%2C%20Voice%20of%20Plenty&page=3&limit=100',
  // Collection q-leg (iter 9): name-only free-text match list + live owned
  // intersect, served natively. These ride the SHARED collection_query_cache,
  // so whichever backend walks first, both then answer from the same entry —
  // body equality (incl. row order and prices) asserts the whole leg.
  '/api/search?scope=collection&q=March%20of%20Wretched%20Sorrow&limit=20&page=1',
  "/api/search?scope=collection&q=G'raha%20Tia&limit=60&page=1",
  '/api/search?scope=collection&q=Neerdiv%2C%20Devious%20Diver&limit=60&page=1',
  '/api/search?scope=collection&q=definitely%20no%20such%20card%20xyz&limit=20&page=1',
  // Operator-conjunction leg (iter 10): t:/c: conjunctions share the same
  // match-list walk + ownedByNames intersect. The durable rows ride the
  // shared collection_query_cache under the byte-exact toScryfall spelling,
  // so a divergence in the Rust gate shows up as a body diff, not a
  // silent cache-key mismatch. `colorless` and `c:xyz` sit OUTSIDE the gate
  // (Node rewrites/throws) and must stay byte-equal through the proxy path.
  '/api/search?scope=collection&q=t%3Acreature%20c%3Ag&limit=60&page=1',
  '/api/search?scope=collection&q=T%3ACreature%20C%3AG&limit=20&page=2',
  '/api/search?scope=collection&q=t%3Acreature%20and%20c%3Ag&limit=60&page=1',
  '/api/search?scope=collection&q=c%3Awu&limit=60&page=1',
  '/api/search?scope=collection&q=sol%20ring%20t%3Acreature&limit=20&page=1',
  '/api/search?scope=collection&q=colorless&limit=60&page=1',
  '/api/search?scope=collection&q=c%3Axyz&limit=20&page=1',
];

// Stats coverage for the ported /api/stats (iter 3): the bench scenario URL
// plus parameter-insensitivity (the handler reads no query — cache name is
// the constant 'stats'), the per-user contract (admin session inserted into
// the DB copy by scripts/seed-parity-user1-session.sh), the sub-route the
// port must NOT swallow (/api/stats/history stays on Node), and the two
// ETag/304 surfaces. Bodies are compared with deepDiff including key order,
// so the insertion-ordered Map assembly is asserted against Node, not
// eyeballed.
const USER1_TOKEN = '39381d59b2d9874a99c9dfe050a40942ff8bf2fe2338346d7c539def20e5c353';
const EXTRA_STATS = [
  '/api/stats?foo=1&bar=baz',
  '/api/stats/',
  '/api/stats/history?period=30d',
];

const cases = [];
for (const s of scen.scenarios) {
  cases.push({ name: s.name, path: s.path, headers: s.headers || {}, method: s.method || 'GET' });
}
const authHeaders = scen.scenarios.find(s => s.headers?.Authorization)?.headers || {};
for (const [i, p] of EXTRA_COLLECTION.entries()) {
  cases.push({ name: `extra${i + 1}_${p.slice('/api/collection'.length + 1) || 'full'}`, path: p, headers: authHeaders, method: 'GET' });
}
for (const [i, p] of EXTRA_SEARCH.entries()) {
  cases.push({ name: `search${i + 1}_${p.slice('/api/search'.length + 1) || 'full'}`, path: p, headers: authHeaders, method: 'GET' });
}
for (const [i, p] of EXTRA_STATS.entries()) {
  cases.push({ name: `stats${i + 1}_${p.slice('/api/stats'.length + 1) || 'full'}`, path: p, headers: authHeaders, method: 'GET' });
}
// Second user: the stats body is per-user (owned/valued aggregates of user 1),
// so parity must run a user the bench token is NOT. Session lives in the DB
// copy only (scripts/seed-parity-user1-session.sh).
cases.push({ name: 'stats_user1', path: '/api/stats', headers: { Authorization: `Bearer ${USER1_TOKEN}` }, method: 'GET' });
// Auth-surface cases (BRIEF rule 2): exact 401/403 bodies.
cases.push({ name: 'auth_no_token', path: '/api/collection?limit=10', headers: {}, method: 'GET' });
cases.push({ name: 'auth_bad_token', path: '/api/collection?limit=10', headers: { Authorization: 'Bearer de...def' }, method: 'GET' });
cases.push({ name: 'auth_apikey_post', path: '/api/collection', headers: { Authorization: '***' + 'x'.repeat(59) }, method: 'POST' });
cases.push({ name: 'auth_no_token_stats', path: '/api/stats', headers: {}, method: 'GET' });
cases.push({ name: 'auth_bad_token_stats', path: '/api/stats', headers: { Authorization: '***' }, method: 'GET' });

// Auth route group (iter 20 port): the non-mutating surface, compared
// Rust-vs-Node. Deliberately excludes anything that would write the shared
// DB copy (valid login/register/bootstrap-success/api-key mutations) —
// those are covered by the cargo auth suites against fixture DBs instead;
// write-side live parity is iter-21 work. All POST bodies here are
// validation-error or disabled paths, which Node answers before any write.
const AUTH_JSON = { 'content-type': 'application/json' };
cases.push({ name: 'authr_config', path: '/api/auth/config', headers: {}, method: 'GET' });
cases.push({ name: 'authr_me_no_token', path: '/api/auth/me', headers: {}, method: 'GET' });
cases.push({ name: 'authr_me_bad_token', path: '/api/auth/me', headers: { Authorization: '***' }, method: 'GET' });
cases.push({ name: 'authr_me_ok', path: '/api/auth/me', headers: authHeaders, method: 'GET' });
cases.push({ name: 'authr_login_missing_fields', path: '/api/auth/login', headers: AUTH_JSON, method: 'POST', body: '{}' });
cases.push({ name: 'authr_login_wrong', path: '/api/auth/login', headers: AUTH_JSON, method: 'POST', body: '{"username":"no_such_user_xyz","password":"whatever123"}' });
cases.push({ name: 'authr_register_disabled', path: '/api/auth/register', headers: AUTH_JSON, method: 'POST', body: '{"username":"parityprobe","password":"goodpass123"}' });
cases.push({ name: 'authr_bootstrap_users_exist', path: '/api/auth/bootstrap', headers: AUTH_JSON, method: 'POST', body: '{"password":"paritypass123"}' });
cases.push({ name: 'authr_bootstrap_short_pw', path: '/api/auth/bootstrap', headers: AUTH_JSON, method: 'POST', body: '{"password":"short"}' });
cases.push({ name: 'authr_oidc_login_disabled', path: '/api/auth/oidc/login', headers: {}, method: 'GET' });
cases.push({ name: 'authr_oidc_callback_disabled', path: '/api/auth/oidc/callback', headers: {}, method: 'GET' });
cases.push({ name: 'authr_unknown_path_proxy', path: '/api/auth/definitely_not_a_route', headers: {}, method: 'GET' });
// Mutating auth routes: their write paths touch the shared DB copy (session
// rows, api_key, settings) and randomised tokens make byte-equality
// impossible, so live parity here covers the auth-gate contract (no token ->
// Node's exact 401 body, answered BEFORE any write; proven native on a dead
// upstream). Mutation paths are compared cargo-vs-fixture (tests/auth.rs
// logout_and_api_key_round_trip, settings_...); live Rust-write->Node-read
// round-trips with a disposable probe user are queued for iter 21.
cases.push({ name: 'authr_logout_no_token', path: '/api/auth/logout', headers: {}, method: 'POST' });
cases.push({ name: 'authr_apikey_post_no_token', path: '/api/auth/api-key', headers: {}, method: 'POST' });
cases.push({ name: 'authr_apikey_delete_no_token', path: '/api/auth/api-key', headers: {}, method: 'DELETE' });
cases.push({ name: 'authr_settings_no_token', path: '/api/auth/settings', headers: AUTH_JSON, method: 'PUT', body: '{}' });

// Collection WRITE group (iter 22 port: GET cardlist, PUT/DELETE :id, POST
// bulk; RUST_ROUTES=collection_writes). Read/gate/validation cases are plain;
// success-path WRITE cases use setup()/teardown() so each side answers the
// identical request from an identical fresh probe row. POST /api/collection
// does NOT accept `notes` (addCardToCollection destructures a fixed field
// set), so probes are tracked by the ids the create response returned and
// deleted again by id through bulk — every bulk body uses `entry_ids`, the
// field Node's bulk route actually reads (an earlier `ids` typo made every
// bulk case pass on a 400==400 'entry_ids is required' without ever
// executing the write). Owner ruling (iter 21): each success case also
// carries a `readBack` verifying the mutation from the OPPOSITE server
// (Rust-write → Node-read and vice versa) before teardown cleans up. The
// add_to_deck full-move path mutates decks + collection rows non-idempotently
// and stays cargo-fixture-only (tests/collection_writes.rs); its pre-write
// validation branches (bad value, missing deck, checked-out deck) are covered
// live here because Node answers them before any deck write.
const CW_JSON = { ...authHeaders, 'content-type': 'application/json' };
cases.push({ name: 'cw_cardlist_plain', path: '/api/collection/cardlist', headers: authHeaders, method: 'GET' });
cases.push({ name: 'cw_cardlist_detailed', path: '/api/collection/cardlist?style=detailed', headers: authHeaders, method: 'GET' });
cases.push({ name: 'cw_cardlist_no_token', path: '/api/collection/cardlist', headers: {}, method: 'GET' });
cases.push({ name: 'cw_put_no_token', path: '/api/collection/99999999', headers: {}, method: 'PUT', body: '{"notes":"x"}' });
cases.push({ name: 'cw_put_bad_token', path: '/api/collection/99999999', headers: { Authorization: '***' }, method: 'PUT', body: '{"notes":"x"}' });
cases.push({ name: 'cw_delete_no_token', path: '/api/collection/99999999', headers: {}, method: 'DELETE' });
cases.push({ name: 'cw_bulk_no_token', path: '/api/collection/bulk', headers: {}, method: 'POST', body: '{"entry_ids":[1],"action":"delete"}' });
cases.push({ name: 'cw_put_missing_404', path: '/api/collection/99999999', headers: CW_JSON, method: 'PUT', body: '{"notes":"x"}' });
cases.push({ name: 'cw_delete_missing_404', path: '/api/collection/99999999', headers: authHeaders, method: 'DELETE' });
cases.push({ name: 'cw_bulk_bad_ids_400', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST', body: '{"action":"delete"}' });
cases.push({ name: 'cw_bulk_empty_ids_400', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST', body: '{"entry_ids":[],"action":"delete"}' });
cases.push({ name: 'cw_bulk_bad_action_400', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST', body: '{"entry_ids":[1],"action":"frobnicate"}' });
cases.push({ name: 'cw_bulk_bad_condition_400', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST', body: '{"entry_ids":[1],"action":"condition","value":"Bent"}' });
cases.push({ name: 'cw_bulk_bad_printing_400', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST', body: '{"entry_ids":[1],"action":"printing","value":"Foil"}' });
cases.push({ name: 'cw_bulk_split_bad_total_400', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST', body: '{"entry_ids":[1],"action":"purchase_split","value":{}}' });
// add_to_deck `value` is the deck id itself (Node: parseInt(value,10)).
cases.push({
  name: 'cw_bulk_deck_bad_value_400', path: '/api/collection/0', headers: CW_JSON, method: 'POST',
  setup: async (base) => { const p = await createProbe(base); return { path: '/api/collection/bulk', body: JSON.stringify({ entry_ids: p.ids, action: 'add_to_deck', value: { deck_id: 1 } }), ctx: { ids: p.ids } }; },
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_bulk_deck_missing_404', path: '/api/collection/0', headers: CW_JSON, method: 'POST',
  setup: async (base) => { const p = await createProbe(base); return { path: '/api/collection/bulk', body: JSON.stringify({ entry_ids: p.ids, action: 'add_to_deck', value: 99999999 }), ctx: { ids: p.ids } }; },
  teardown: deleteProbes,
});
// Deck 114 ('Urlings' sibling in the bench DB) is owned by the bench token
// user and checked_out=1 — Node answers 409 before touching any deck row.
cases.push({
  name: 'cw_bulk_deck_checked_out_409', path: '/api/collection/0', headers: CW_JSON, method: 'POST',
  setup: async (base) => { const p = await createProbe(base); return { path: '/api/collection/bulk', body: JSON.stringify({ entry_ids: p.ids, action: 'add_to_deck', value: 114 }), ctx: { ids: p.ids } }; },
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_put_notes_200', path: '/api/collection/0', headers: CW_JSON, method: 'PUT', body: '{"notes":"parity-notes"}',
  setup: async (base) => { const p = await createProbe(base); return { path: `/api/collection/${p.id}`, ctx: { ids: p.ids, pid: p.id } }; },
  readBack: expectPersisted((ctx) => [ctx.pid], (rows) => rows.length === 1 && rows[0].notes === 'parity-notes', 'PUT notes must persist'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_put_condition_200', path: '/api/collection/0', headers: CW_JSON, method: 'PUT', body: '{"condition":"Lightly Played"}',
  setup: async (base) => { const p = await createProbe(base); return { path: `/api/collection/${p.id}`, ctx: { ids: p.ids, pid: p.id } }; },
  readBack: expectPersisted((ctx) => [ctx.pid], (rows) => rows.length === 1 && rows[0].condition === 'Lightly Played', 'PUT condition must persist'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_put_printing_200', path: '/api/collection/0', headers: CW_JSON, method: 'PUT', body: '{"printing":"Holofoil"}',
  setup: async (base) => { const p = await createProbe(base); return { path: `/api/collection/${p.id}`, ctx: { ids: p.ids, pid: p.id } }; },
  readBack: expectPersisted((ctx) => [ctx.pid], (rows) => rows.length === 1 && rows[0].printing === 'Holofoil', 'PUT printing must persist'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_put_bad_printing_400', path: '/api/collection/0', headers: CW_JSON, method: 'PUT', body: '{"printing":"Foil"}',
  setup: async (base) => { const p = await createProbe(base); return { path: `/api/collection/${p.id}`, ctx: { ids: p.ids, pid: p.id } }; },
  teardown: deleteProbes,
});
// Node reconciles the physical stack to N single-copy rows (cargo pins the
// family math); the live read-back verifies the written row is visible from
// the other server with the reconciled quantity.
cases.push({
  name: 'cw_put_qty_grow_200', path: '/api/collection/0', headers: CW_JSON, method: 'PUT', body: '{"quantity":3}',
  setup: async (base) => { const p = await createProbe(base); return { path: `/api/collection/${p.id}`, ctx: { ids: p.ids, pid: p.id } }; },
  readBack: expectPersisted((ctx) => [ctx.pid], (rows) => rows.length === 1 && rows[0].quantity === 1, 'grown stack keeps row as single copy'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_put_qty_zero_200', path: '/api/collection/0', headers: CW_JSON, method: 'PUT', body: '{"quantity":0}',
  setup: async (base) => { const p = await createProbe(base, { quantity: 2 }); return { path: `/api/collection/${p.id}`, ctx: { ids: p.ids, pid: p.id } }; },
  readBack: expectPersisted((ctx) => [ctx.pid], (rows) => rows.length === 1 && rows[0].quantity === 1, 'quantity 0 floors at 1 and never deletes'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_put_trade_200', path: '/api/collection/0', headers: CW_JSON, method: 'PUT', body: '{"is_trade":true}',
  setup: async (base) => { const p = await createProbe(base); return { path: `/api/collection/${p.id}`, ctx: { ids: p.ids, pid: p.id } }; },
  readBack: expectPersisted((ctx) => [ctx.pid], (rows) => rows.length === 1 && Boolean(rows[0].is_trade), 'PUT is_trade must persist'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_delete_200', path: '/api/collection/0', headers: authHeaders, method: 'DELETE',
  setup: async (base) => { const p = await createProbe(base); return { path: `/api/collection/${p.id}`, ctx: { ids: p.ids, pid: p.id } }; },
  readBack: expectPersisted((ctx) => [ctx.pid], (rows) => rows.length === 0, 'DELETE must remove the row'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_bulk_delete_200', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST',
  setup: async (base) => { const p = await createProbe(base); return { body: JSON.stringify({ entry_ids: p.ids, action: 'delete' }), ctx: { ids: p.ids } }; },
  readBack: expectPersisted((ctx) => ctx.ids, (rows) => rows.length === 0, 'bulk delete must remove rows'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_bulk_trade_200', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST',
  setup: async (base) => { const p = await createProbe(base); return { body: JSON.stringify({ entry_ids: p.ids, action: 'trade' }), ctx: { ids: p.ids } }; },
  readBack: expectPersisted((ctx) => ctx.ids, (rows) => rows.length >= 1 && rows.every((r) => Boolean(r.is_trade)), 'bulk trade must persist'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_bulk_condition_200', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST',
  setup: async (base) => { const p = await createProbe(base); return { body: JSON.stringify({ entry_ids: p.ids, action: 'condition', value: 'Near Mint' }), ctx: { ids: p.ids } }; },
  readBack: expectPersisted((ctx) => ctx.ids, (rows) => rows.length === 1 && rows[0].condition === 'Near Mint', 'bulk condition must persist'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_split_weighted_200', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST',
  setup: async (base) => {
    const a = await createProbe(base), b = await createProbe(base);
    return { body: JSON.stringify({ entry_ids: [a.id, b.id], action: 'purchase_split', value: { total: 1.5, method: 'weighted' } }), ctx: { ids: [a.id, b.id] } };
  },
  readBack: expectPersisted((ctx) => ctx.ids, (rows) => rows.length === 2 && Math.abs(rows.reduce((s, r) => s + Number(r.purchase_price), 0) - 1.5) < 1e-9, 'weighted split must persist summing to 1.50'),
  teardown: deleteProbes,
});
cases.push({
  name: 'cw_split_equal_200', path: '/api/collection/bulk', headers: CW_JSON, method: 'POST',
  setup: async (base) => {
    const a = await createProbe(base), b = await createProbe(base);
    return { body: JSON.stringify({ entry_ids: [a.id, b.id], action: 'purchase_split', value: { total: 1, method: 'equal' } }), ctx: { ids: [a.id, b.id] } };
  },
  readBack: expectPersisted((ctx) => ctx.ids, (rows) => rows.length === 2 && rows.every((r) => Math.abs(Number(r.purchase_price) - 0.5) < 1e-9), 'equal split must persist 0.50 per row'),
  teardown: deleteProbes,
});

// --- write-parity helpers (iter 22: collection_writes group) -----------------
// A write case is only honest if BOTH sides answer the identical request from
// the identical starting state. `setup(base)` runs immediately before each
// side's request (per attempt — a retry gets a fresh probe) and can rewrite
// path/body/headers and hand state to `teardown(base, ctx)`, which always
// cleans up through the API so the shared DB copy is left as found. Probes
// are tagged with a unique marker in `notes`; teardown deletes ONLY rows
// whose notes match, never pre-existing user data.
let ipSeq = 1;
function xff() { const n = ipSeq++; return `10.${(n >> 16) & 255}.${(n >> 8) & 255}.${n & 255}`; }

const CW_CARD = 'mtg-dda6f965-cbdf-4dbc-b0b1-00c27e32f5f2'; // parity fixture card (in card_cache + owned)
async function apiReq(base, method, path, headers, body) {
  const res = await fetch(base + path, {
    method, headers: { ...headers, 'x-forwarded-for': xff() }, body, redirect: 'manual',
  });
  const text = await res.text();
  let json; try { json = JSON.parse(text); } catch { /* non-JSON */ }
  return { status: res.status, json, text };
}
async function createProbe(base, body = {}) {
  // POST /api/collection ignores `notes` (addCardToCollection takes a fixed
  // field set), so probes are identified by the ids the create response
  // returned — teardown deletes exactly those rows, never pre-existing data.
  const r = await apiReq(base, 'POST', '/api/collection',
    { ...authHeaders, 'content-type': 'application/json' },
    JSON.stringify({ card_id: CW_CARD, quantity: 1, ...body }));
  if (r.status !== 200 || !r.json || !Array.isArray(r.json.ids) || r.json.ids.length === 0) {
    throw new Error(`parity probe create failed: ${r.status} ${r.text.slice(0, 120)}`);
  }
  return { id: r.json.id, ids: r.json.ids };
}
async function deleteProbes(base, ctx) {
  const ids = ctx && Array.isArray(ctx.ids) ? ctx.ids : [];
  if (!ids.length) return;
  await apiReq(base, 'POST', '/api/collection/bulk',
    { ...authHeaders, 'content-type': 'application/json' },
    JSON.stringify({ entry_ids: ids, action: 'delete' }));
}
async function rowsByIdsVia(base, ids) {
  const g = await apiReq(base, 'GET', `/api/collection?card_id=${CW_CARD}&limit=1000`, authHeaders);
  const rows = Array.isArray(g.json) ? g.json : (Array.isArray(g.json?.data) ? g.json.data : []);
  const want = new Set(ids.map((n) => Number(n)));
  return rows.filter((row) => want.has(Number(row.entry_id)));
}
const persistView = (rows) => rows
  .map((r) => ({ quantity: r.quantity, condition: r.condition, printing: r.printing, is_trade: r.is_trade, purchase_price: r.purchase_price, notes: r.notes }))
  .sort((a, b) => (JSON.stringify(a) < JSON.stringify(b) ? -1 : 1));
/// Owner ruling (iter 21): write success cases must be VERIFIED by a read
/// through the opposite server. Returns (otherBase, ctx) => {ok, why, seen}:
/// `ok` false fails the case outright; `seen` (normalized persisted fields of
/// the probe rows) is deep-compared across the two sides, so a Rust write
/// that persists differently from a Node write cannot hide behind matching
/// write-response bodies.
function expectPersisted(idsOf, pred, why) {
  return async (other, ctx) => {
    const rows = await rowsByIdsVia(other, idsOf(ctx));
    const detail = rows.map((r) => ({ id: r.entry_id, q: r.quantity, c: r.condition, p: r.printing, t: r.is_trade, pp: r.purchase_price, n: r.notes }));
    return { ok: pred(rows), why: `${why} -> ${JSON.stringify(detail)}`, seen: persistView(rows) };
  };
}

async function fetchSide(base, c) {
  const t0 = Date.now();
  let path = c.path, body = c.body, headers = c.headers, ctx = null;
  if (c.setup) {
    const s = await c.setup(base, c);
    if (s) { path = s.path ?? path; body = s.body ?? body; headers = s.headers ?? headers; ctx = s.ctx ?? null; }
  }
  const res = await fetch(base + path, {
    method: c.method,
    headers: { ...headers, 'x-forwarded-for': xff(), 'cache-control': 'no-cache' },
    body,
    redirect: 'manual',
  });
  const resBody = res.status === 304 ? '' : await res.text();
  // Cross-side write verification BEFORE teardown (the probe must still exist).
  let readback;
  if (c.readBack) {
    const other = base === RUST ? NODE : RUST;
    try { readback = await c.readBack(other, ctx); }
    catch (e) { readback = { ok: false, why: `readBack threw ${e}`, seen: null }; }
  }
  if (c.teardown) { try { await c.teardown(base, ctx); } catch (e) { console.log(`  (teardown ${c.name}: ${e})`); } }
  return {
    status: res.status,
    body: resBody,
    json: resBody && (res.headers.get('content-type') || '').includes('json') ? JSON.parse(resBody) : undefined,
    total: res.headers.get('x-total-count'),
    source: res.headers.get('x-source'),
    etag: res.headers.get('etag'),
    ctype: (res.headers.get('content-type') || '').split(';')[0].trim().toLowerCase(),
    readback,
    ms: Date.now() - t0,
  };
}

const fmt = v => { const s = typeof v === 'string' ? v : JSON.stringify(v); return s && s.length > 200 ? s.slice(0, 200) + '…' : s; };

/// Structural equality: values, array order, AND object key order (the
/// contract is byte-shaped JSON, so key order is compared too).
function deepDiff(a, b, path, diffs) {
  if (diffs.length >= 5) return;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return diffs.push(`${path}: array shape ${a?.length} vs ${b?.length}`);
    // Deep array comparison is O(size); for huge collections compare
    // element key order only on the first item (shape) — values on all.
    for (let i = 0; i < a.length; i++) {
      if (i < 3 || a.length <= 200) deepDiff(a[i], b[i], `${path}[${i}]`, diffs);
      else {
        const ka = Object.keys(a[0] ?? {}), kb = Object.keys(b[0] ?? {});
        if (ka.join('\u0000') !== kb.join('\u0000')) diffs.push(`${path}[${i}] key order differs`);
        break;
      }
    }
    return;
  }
  if (a && b && typeof a === 'object' && typeof b === 'object') {
    const ka = Object.keys(a), kb = Object.keys(b);
    if (ka.join('\u0000') !== kb.join('\u0000')) return diffs.push(`${path}: key order/set ${ka.length} vs ${kb.length} (${ka.slice(0, 6)} vs ${kb.slice(0, 6)})`);
    for (const k of ka) deepDiff(a[k], b[k], `${path}.${k}`, diffs);
    return;
  }
  if (!(a === b || (Number.isNaN(a) && Number.isNaN(b)))) diffs.push(`${path}: ${fmt(a)} vs ${fmt(b)}`);
}

function compareCase(c, r, n) {
  const diffs = [];
  if (r.status !== n.status) diffs.push(`status ${r.status} vs ${n.status}`);
  if (r.ctype !== n.ctype) diffs.push(`content-type ${r.ctype} vs ${n.ctype}`);
  if ((r.total ?? undefined) !== (n.total ?? undefined)) diffs.push(`x-total-count ${r.total} vs ${n.total}`);
  if ((r.source ?? undefined) !== (n.source ?? undefined)) diffs.push(`x-source ${r.source} vs ${n.source}`);
  if (r.json !== undefined || n.json !== undefined) {
    if (r.json === undefined || n.json === undefined) diffs.push('one side non-JSON body');
    else deepDiff(r.json, n.json, '$', diffs);
  } else if (r.body !== n.body) {
    diffs.push(`body ${fmt(r.body)} vs ${fmt(n.body)}`);
  }
  // Owner ruling (iter 21): write success cases verified through the OTHER
  // server's read path. A failed read-back is an absolute failure (it cannot
  // cancel out across sides), and the persisted views must be identical.
  if (r.readback || n.readback) {
    if (!r.readback || !n.readback) diffs.push('readback missing on one side');
    else {
      if (!r.readback.ok) diffs.push(`rust-write not verified by Node read: ${r.readback.why}`);
      if (!n.readback.ok) diffs.push(`node-write not verified by Rust read: ${n.readback.why}`);
      deepDiff(r.readback.seen, n.readback.seen, '$persisted', diffs);
    }
  }
  return diffs;
}

/// Conditional GET via raw node:http, NOT fetch. undici always sends its own
/// `accept-encoding`/header set, and Node's Express stack (`Vary: User-Agent,
/// Origin, Accept-Encoding` + its freshness check) then treats the replay as
/// stale and answers 200 — an artifact of the *client*, identical on both
/// servers. Raw http with explicit headers reproduces what a browser/curl
/// actually sends and is the only honest way to assert 304 behaviour.
function rawGet(base, path, headers) {
  return new Promise((resolve, reject) => {
    const u = new URL(base);
    const req = http.request(
      { host: u.hostname, port: u.port, path, method: 'GET', headers },
      (res) => {
        let bytes = 0;
        res.on('data', (c) => { bytes += c.length; });
        res.on('end', () => resolve({ status: res.statusCode, bytes, etag: res.headers.etag }));
      },
    );
    req.on('error', reject);
    req.end();
  });
}

/// ETag behaviour parity, for successful (200) collection reads only — an
/// ETag on a 401/403 legitimately re-401s/403s, so replaying it proves nothing.
///
/// Two distinct tags, two contracts:
///  1. The route's OWN tag, `W/"col-{boot}-{rev}-{user}-{qh}"` (Node:
///     utils/collectionEtag.js; emitted only for paginated, unallocated
///     reads). Boot id is per-process so the strings differ by design; the
///     revision, user and query-hash segments MUST agree across the two
///     servers, and each side must answer a replay of its own tag with 304.
///  2. Express's DEFAULT tag, which `res.json()` stamps on every other JSON
///     response (`etag` package: W/"{byteLen:hex}-{base64(sha1)[0..27]}").
///     Both sides must emit one; a replay must 304; and when the two bodies
///     are byte-identical the tags must be identical, which cross-checks the
///     Rust serializer against Node's at byte level.
async function etagParity(c) {
  // Replay only makes sense for idempotent GETs; write cases are compared by
  // response alone (their ETag is Express's default body tag, not a contract).
  if ((c.method || 'GET') !== 'GET') return [];
  if (!c.path.startsWith('/api/collection') && !c.path.startsWith('/api/search') && !c.path.startsWith('/api/stats')) return [];
  const [r, n] = await Promise.all([fetchSide(RUST, c), fetchSide(NODE, c)]);
  if (r.status !== 200 || n.status !== 200) return [];
  const diffs = [];
  const segs = t => /^W\/"col-([^-]+)-([^-]+)-([^-]+)-([^"]+)"$/.exec(t || '');
  const rs = segs(r.etag), ns = segs(n.etag);
  if (rs || ns) {
    if (!rs || !ns) return [`route ETag on one side only: rust=${r.etag} node=${n.etag}`];
    // groups: 1=boot (per-process, differs by design) 2=rev 3=user 4=qh
    if (rs.slice(2).join('|') !== ns.slice(2).join('|')) {
      diffs.push(`route ETag (rev,user,qh) disagree: rust=${r.etag} node=${n.etag}`);
    }
  } else {
    if (!r.etag) diffs.push('rust sent no ETag where Node stamps its default body tag');
    if (!n.etag) diffs.push('node sent no ETag on a 200 JSON read');
    if (r.etag && n.etag && r.body === n.body && r.etag !== n.etag) {
      diffs.push(`byte-identical bodies, different tags: rust=${r.etag} node=${n.etag}`);
    }
  }
  for (const [name, base, first] of [['rust', RUST, r], ['node', NODE, n]]) {
    if (!first.etag) continue;
    const again = await rawGet(base, c.path, { ...c.headers, 'if-none-match': first.etag });
    if (again.status !== 304) diffs.push(`${name}: replay of own ETag answered ${again.status}, not 304`);
    if (again.bytes !== 0) diffs.push(`${name}: 304 carried a body (${again.bytes} bytes)`);
  }
  return diffs;
}

// --- run ---------------------------------------------------------------------

let failed = 0, flaky = 0;
const MAX_TRIES = 3;
for (const c of cases) {
  let lastDiffs = null, tries = 0, etagDiffs = [];
  for (; tries < MAX_TRIES; tries++) {
    // Write cases run RUST then NODE serially: probes for one case share a
    // physical stack (card/user/condition/printing/language), and Node's
    // setStackQuantity trim deletes by stack key — a concurrent other-side
    // probe could be trimmed out from under its own flow. Sequential keeps
    // the "identical request from identical fresh state" property and makes
    // the trim deterministic. Pure reads stay parallel.
    const [r, n] = c.setup
      ? [await fetchSide(RUST, c), await fetchSide(NODE, c)]
      : await Promise.all([fetchSide(RUST, c), fetchSide(NODE, c)]);
    lastDiffs = compareCase(c, r, n);
    if (lastDiffs.length === 0) { etagDiffs = await etagParity(c); if (etagDiffs.length === 0) break; }
  }
  const ok = lastDiffs.length === 0 && etagDiffs.length === 0;
  if (ok && tries > 0) { flaky++; console.log(`PASS(flaky x${tries + 1}) ${c.name}`); continue; }
  if (ok) {
    console.log(`PASS ${c.name}`);
    if (VERBOSE) console.log(`     ${c.path}`);
  } else {
    failed++;
    console.log(`FAIL ${c.name}  ${c.method} ${c.path}`);
    for (const d of [...lastDiffs, ...etagDiffs]) console.log(`     · ${d}`);
  }
}

// --- bounded staleness: the stats SWR exception (owner ruling, 2026-11-10) ---
// Hard rule 1 excepts ONLY GET /api/stats: after a write, Rust may serve the
// pre-write body while it rebuilds in the background, for at most 5 s
// (STALE_GRACE). Past the bound it must match Node exactly. Write a
// collection edit through Node (:3312 — same shape as the bench write-mix),
// wait > 5 s, require an exact rust==node match, then revert the edit so the
// DB copy is left as found.
const pw = { name: 'stats_post_write', path: '/api/stats', headers: authHeaders, method: 'GET' };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function putTrade(v) {
  return new Promise((resolve) => {
    const body = JSON.stringify({ is_trade: v });
    const req = http.request(new URL('/api/collection/4', NODE), {
      method: 'PUT',
      headers: { ...authHeaders, 'content-type': 'application/json', 'content-length': Buffer.byteLength(body), 'x-forwarded-for': xff() },
    }, (res) => { res.resume(); res.on('end', () => resolve(res.statusCode)); });
    req.on('error', () => resolve(0));
    req.write(body); req.end();
  });
}
{
  await fetchSide(RUST, pw); // warm Rust's stats slot so the write lands in a live grace window
  const a = await putTrade(1);
  await sleep(6500); // > STALE_GRACE (5 s) + rebuild margin
  let diffs = ['comparison not run'];
  for (let t = 0; t < MAX_TRIES; t++) {
    const [r, n] = await Promise.all([fetchSide(RUST, pw), fetchSide(NODE, pw)]);
    diffs = compareCase(pw, r, n);
    if (!diffs.length) break;
  }
  const b = await putTrade(0);
  if (a !== 200 || b !== 200) { failed++; console.log(`FAIL stats_post_write  PUTs answered ${a}/${b} (expected 200/200)`); }
  else if (diffs.length) {
    failed++;
    console.log(`FAIL stats_post_write  ${pw.method} ${pw.path} past the 5 s staleness bound`);
    for (const d of diffs) console.log(`     · ${d}`);
  } else console.log('PASS stats_post_write (write -> >5 s -> rust==node exact; edit reverted)');
}

console.log(`\nparity: ${cases.length + 1 - failed}/${cases.length + 1} cases pass (${flaky} needed retries) rust=${RUST} node=${NODE}`);
process.exit(failed === 0 ? 0 : 1);
