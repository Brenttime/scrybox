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
import crypto from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';

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
// --- POST /api/search (iter-47 slice) -----------------------------------------
// Node mounts the GET/POST pair (collection.js:233-234) and POST carries the
// query through { ...req.query, ...req.body } (collection.js:81) after
// authenticateToken and the app-level bigJson parse, searchLimiter in
// between. Native arms are name/number/set/page/limit (JS-truthy gates +
// String() coercion); q/image/exotic shapes ride a whole-request forward.
// deepDiff compares bodies incl. key order, so the merge order and the
// res.json ETag both assert against Node, not eyeballed. POST must NEVER
// replay 304 (conditional revalidation is GET/HEAD) even on an ETag hit.
const PS_JSON = { ...authHeaders, 'content-type': 'application/json' };
const psPost = (name, path, body, headers = PS_JSON, extra = {}) =>
  cases.push({ name, path: path ?? '/api/search', headers, method: 'POST', body, ...extra });
psPost('ps_post_name', undefined, JSON.stringify({ name: 'Sol Ring' }));
psPost('ps_post_merge_body_wins', '/api/search?name=Black%20Lotus&limit=5', JSON.stringify({ name: 'Sol Ring' }));
psPost('ps_post_merge_body_limit', '/api/search?limit=5', JSON.stringify({ name: 'Sol Ring', limit: '10' }));
psPost('ps_post_number_numeric', undefined, JSON.stringify({ name: 'Sol Ring', number: 160 }));
psPost('ps_post_number_hash_str', undefined, JSON.stringify({ name: 'Sol Ring', number: '#160/brc' }));
psPost('ps_post_set_upper', undefined, JSON.stringify({ name: 'Sol Ring', set: 'BRC' }));
psPost('ps_post_falsy_gates', undefined, JSON.stringify({ name: 'Sol Ring', number: '', set: '', page: 0, limit: 0 }));
// 0/-0/false/null are JS-FALSY: runSearch's `(x || '')` gates them to ''
// (probed: {"name":0} answers the no-name browse). Rust must not stringify
// them into LIKE params.
psPost('ps_post_falsy_zero_names', undefined, JSON.stringify({ name: 0, number: -0, set: false, lang: 0 }));
psPost('ps_post_page_numeric', undefined, JSON.stringify({ name: 'Lightning', page: 2, limit: 10 }));
psPost('ps_post_plain_empty', undefined, undefined);
psPost('ps_post_text_ct_skipped', undefined, 'name=Sol Ring', { ...authHeaders, 'content-type': 'text/plain' });
psPost('ps_post_malformed_json', undefined, '{"name":');
psPost('ps_post_exotic_page', undefined, JSON.stringify({ name: 'Sol Ring', page: { x: 1 } }));
psPost('ps_post_array_name', undefined, JSON.stringify({ name: ['Sol Ring'] }));
psPost('ps_post_dup_qs_name', '/api/search?name=aa&name=Sol%20Ring', undefined);
psPost('ps_post_no_token', undefined, JSON.stringify({ name: 'Sol Ring' }), { 'content-type': 'application/json' });
// API key on POST: keys are GET-only, the gate answers 403 (is_get=false)
// BEFORE the limiter consumes a bucket. shx_post_apikey_403 precedent.
cases.push({
  name: 'ps_post_apikey_403', path: '/api/search',
  headers: { Authorization: '***' + 'x'.repeat(59), 'content-type': 'application/json' },
  method: 'POST', body: JSON.stringify({ name: 'Sol Ring' }),
  setup: async () => { dbWrite((d) => d.prepare('UPDATE users SET api_key = ? WHERE username = ?').run('x'.repeat(59), PARITY_USER)); return { ctx: null }; },
  teardown: () => dbWrite((d) => d.prepare('UPDATE users SET api_key = NULL WHERE username = ?').run(PARITY_USER)),
});
// ETag hit on POST: the GET twin's ETag must NOT replay as 304 — both sides
// answer the full 200 body (Express conditional revalidation is GET/HEAD).
cases.push({
  name: 'ps_post_no304', path: '/api/search',
  headers: PS_JSON, method: 'POST', body: JSON.stringify({ name: 'Sol Ring' }),
  setup: async (base) => {
    const r = await fetch(base + '/api/search?name=Sol%20Ring', { headers: { ...authHeaders, 'cache-control': 'no-cache' } });
    const etag = r.headers.get('etag');
    return { headers: { ...PS_JSON, ...(etag ? { 'if-none-match': etag } : {}) } };
  },
});
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

// Auth WRITER success-path parity (iter 23; orchestrator audit ruling
// 2026-11-10 #1: "verified auth routes need success-path parity ... the
// current 15 'verified' entries include login/register/bootstrap/api-key/
// settings, none of which has a verified write round-trip"). A dedicated
// test user `rusttest_parity` lives in the DB COPY only (never prod; the
// same direct-SQL channel scripts/seed-parity-user1-session.sh uses). Every
// case: resetParityUser() puts the row at a deterministic baseline, setup
// seeds what the case needs, readBack verifies the WRITE THROUGH THE OPPOSITE
// SERVER (cross-side per the iter-21 owner ruling), teardown restores the
// baseline — sessions wiped, password/share/api_key back to seed — so the DB
// is left as found even if a case fails. Random values (session tokens,
// issued api keys, rotated share tokens) are stripped from body equality
// (c.strip) and instead asserted by SHAPE plus persistence-visibility, so a
// Rust write that does not land where Node can read it fails outright.
// register/bootstrap success cannot run here (users table non-empty; this
// deployment runs ALLOW_REGISTRATION=false) — those two are covered by the
// throwaway-instance ladder bench/parity-auth-ephemeral.mjs + cargo fixture
// tests, and stay `rust` in the manifest until that ladder passes.
const AUTH_DB = new URL('../../../data/bindarr.db', import.meta.url).pathname;
const PW_OLD = 'OldPass!2345';
const PW_NEW = 'NewPass!2345';
const PARITY_USER = 'rusttest_parity';
const SALT0 = '5273745061726974792d30303030'; // fixed 16-byte salt (hex)
const PBKDF2_ITER = 210000; // db.js PBKDF2_ITERATIONS
const SHARE_T0 = '52737450617269747900000000000054'; // baseline share_token
const FIXED_KEY = 'bnd_' + 'f'.repeat(48);
const SESSION_EXP = '2030-01-01 00:00:00';
const hashPw = (pw, salt) => `${PBKDF2_ITER}:${salt}:${crypto.pbkdf2Sync(pw, salt, PBKDF2_ITER, 64, 'sha512').toString('hex')}`;
const PW_HASH0 = hashPw(PW_OLD, SALT0);
// Busy timeout: the servers write the shared DB copy too (seed-cards'
// BEGIN IMMEDIATE + 208 inserts, bulk adds, collection writes). Without a
// timeout DatabaseSync returns SQLITE_BUSY immediately — the pre-documented
// harness race family (iter-10 printing-identity, iter-12 dr_* crash). 5 s
// of waiting rides out any server-side transaction; a real deadlock still
// surfaces as an error.
function dbWrite(fn) { const d = new DatabaseSync(AUTH_DB, { timeout: 5000 }); try { return fn(d); } finally { d.close(); } }
function resetParityUser() {
  dbWrite((d) => {
    const row = d.prepare('SELECT id FROM users WHERE username = ?').get(PARITY_USER);
    if (row) d.prepare('DELETE FROM sessions WHERE user_id = ?').run(row.id);
    d.prepare('DELETE FROM users WHERE username = ?').run(PARITY_USER);
    d.prepare('INSERT INTO users (username, password_hash, role, share_token, share_enabled, api_key) VALUES (?, ?, ?, ?, ?, NULL)')
      .run(PARITY_USER, PW_HASH0, 'member', SHARE_T0, 0);
  });
}
function seedSession(token) {
  dbWrite((d) => {
    d.prepare('DELETE FROM sessions WHERE token = ?').run(token);
    const row = d.prepare('SELECT id FROM users WHERE username = ?').get(PARITY_USER);
    if (!row) throw new Error('parity user missing');
    d.prepare('INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)').run(token, row.id, SESSION_EXP);
  });
}
const sideTag = (base) => (base === RUST ? 'r' : 'n');
if (fs.existsSync(AUTH_DB)) {
cases.push({
  // Login success: token shape asserted here, and the freshly ISSUED session
  // must authenticate on the opposite server (Rust-written session row read
  // by Node and vice versa). token stripped: random by design.
  name: 'authr_login_ok', path: '/api/auth/login', headers: AUTH_JSON, method: 'POST',
  body: JSON.stringify({ username: PARITY_USER, password: PW_OLD }),
  strip: ['$.token'],
  setup: async () => { resetParityUser(); },
  readBack: async (other, ctx, res) => {
    const tok = res?.json?.token;
    if (typeof tok !== 'string' || !/^[0-9a-f]{64}$/.test(tok)) return { ok: false, why: `login token shape ${fmt(tok)}`, seen: null };
    const me = await apiReq(other, 'GET', '/api/auth/me', { Authorization: `Bearer ${tok}` });
    const u = me.json?.user || {};
    // `id` excluded from the compared view: resetParityUser recreates the
    // row, so AUTOINCREMENT ids legitimately differ between the two runs.
    const view = { username: u.username, role: u.role, share_token: u.share_token, share_enabled: u.share_enabled, api_key: u.api_key ?? '' };
    return me.status === 200 && u.username === PARITY_USER && u.role === 'member'
      ? { ok: true, why: 'fresh session authenticates on the other server', seen: view }
      : { ok: false, why: `fresh session rejected across sides: ${me.status} ${fmt(me.text)}`, seen: null };
  },
  teardown: async () => { resetParityUser(); },
});
cases.push({
  // Logout success: deterministic seeded session; after the write the token
  // must 401 on the opposite server (row actually deleted, not just answered).
  name: 'authr_logout_ok', path: '/api/auth/logout', headers: {}, method: 'POST',
  setup: async (base) => {
    resetParityUser();
    const tok = `rusttest-logout-${sideTag(base)}`;
    seedSession(tok);
    return { headers: { Authorization: `Bearer ${tok}` }, ctx: { tok } };
  },
  readBack: async (other, ctx) => {
    const me = await apiReq(other, 'GET', '/api/auth/me', { Authorization: `Bearer ${ctx.tok}` });
    return me.status === 401
      ? { ok: true, why: 'revoked session 401s on the other server', seen: { revoked: true } }
      : { ok: false, why: `revoked session still answers ${me.status}`, seen: null };
  },
  teardown: async () => { resetParityUser(); },
});
cases.push({
  // API-key issue: bnd_ + 48 hex; the fresh key must authenticate a GET on
  // the opposite server AND match what /api/auth/me persists there (rule 2:
  // API key is GET-only — exercised live here, not just in cargo).
  name: 'authr_apikey_post_ok', path: '/api/auth/api-key', headers: {}, method: 'POST',
  strip: ['$.api_key'],
  setup: async (base) => {
    resetParityUser();
    const tok = `rusttest-keypost-${sideTag(base)}`;
    seedSession(tok);
    return { headers: { Authorization: `Bearer ${tok}` }, ctx: { tok } };
  },
  readBack: async (other, ctx, res) => {
    const key = res?.json?.api_key;
    if (typeof key !== 'string' || !/^bnd_[0-9a-f]{48}$/.test(key)) return { ok: false, why: `api-key shape ${fmt(key)}`, seen: null };
    const col = await apiReq(other, 'GET', '/api/collection?limit=1', { Authorization: `Bearer ${key}` });
    const me = await apiReq(other, 'GET', '/api/auth/me', { Authorization: `Bearer ${key}` });
    const post = await apiReq(other, 'POST', '/api/collection', { Authorization: `Bearer ${key}`, 'content-type': 'application/json' }, '{}');
    return col.status === 200 && post.status === 403 && me.json?.user?.api_key === key
      ? { ok: true, why: 'key usable GET-only and persisted on the other server', seen: { get_ok: true, post_forbidden: true } }
      : { ok: false, why: `get=${col.status} post=${post.status} persisted=${me.json?.user?.api_key === key}`, seen: null };
  },
  teardown: async () => { resetParityUser(); },
});
cases.push({
  // API-key revoke: known key seeded, DELETE must make it 401 on the other
  // server (UPDATE users SET api_key=NULL actually landed).
  name: 'authr_apikey_delete_ok', path: '/api/auth/api-key', headers: {}, method: 'DELETE',
  setup: async (base) => {
    resetParityUser();
    dbWrite((d) => d.prepare('UPDATE users SET api_key = ? WHERE username = ?').run(FIXED_KEY, PARITY_USER));
    const tok = `rusttest-keydel-${sideTag(base)}`;
    seedSession(tok);
    return { headers: { Authorization: `Bearer ${tok}` }, ctx: { tok } };
  },
  readBack: async (other) => {
    const me = await apiReq(other, 'GET', '/api/auth/me', { Authorization: `Bearer ${FIXED_KEY}` });
    return me.status === 401
      ? { ok: true, why: 'revoked key 401s on the other server', seen: { revoked: true } }
      : { ok: false, why: `revoked key still answers ${me.status}`, seen: null };
  },
  teardown: async () => { resetParityUser(); },
});
cases.push({
  // Settings write: share_enabled flag; response body is fully deterministic
  // for the seeded user (no strip) and must be visible from the other server.
  name: 'authr_settings_share_toggle', path: '/api/auth/settings', headers: {}, method: 'PUT',
  body: '{"share_enabled":1}',
  setup: async (base) => {
    resetParityUser();
    const tok = `rusttest-share-${sideTag(base)}`;
    seedSession(tok);
    return { headers: { Authorization: `Bearer ${tok}`, 'content-type': 'application/json' }, ctx: { tok } };
  },
  readBack: async (other, ctx) => {
    const me = await apiReq(other, 'GET', '/api/auth/me', { Authorization: `Bearer ${ctx.tok}` });
    return me.status === 200 && me.json?.user?.share_enabled === 1
      ? { ok: true, why: 'share flag visible on the other server', seen: { share_enabled: 1 } }
      : { ok: false, why: `share flag not visible: ${me.status} ${fmt(me.json)}`, seen: null };
  },
  teardown: async () => { resetParityUser(); },
});
cases.push({
  // Share-token rotation: new token is random (stripped) but must be a fresh
  // hex32, equal to what the opposite server reads back AND to what the
  // response body carried.
  name: 'authr_settings_share_regen', path: '/api/auth/settings', headers: {}, method: 'PUT',
  body: '{"regenerate_share_token":true}',
  strip: ['$.user.share_token'],
  setup: async (base) => {
    resetParityUser();
    const tok = `rusttest-regen-${sideTag(base)}`;
    seedSession(tok);
    return { headers: { Authorization: `Bearer ${tok}`, 'content-type': 'application/json' }, ctx: { tok } };
  },
  readBack: async (other, ctx, res) => {
    const me = await apiReq(other, 'GET', '/api/auth/me', { Authorization: `Bearer ${ctx.tok}` });
    const tok = me.json?.user?.share_token;
    const ok = me.status === 200 && typeof tok === 'string' && /^[0-9a-f]{32}$/.test(tok) && tok !== SHARE_T0 && tok === res?.json?.user?.share_token;
    return ok
      ? { ok: true, why: 'rotated token persisted and matches the response', seen: { rotated_shape_ok: true } }
      : { ok: false, why: `rotation not visible: ${me.status} stored=${fmt(tok)} resp=${fmt(res?.json?.user?.share_token)}`, seen: null };
  },
  teardown: async () => { resetParityUser(); },
});
cases.push({
  // Password change: the CURRENT-password check plus the new hash must be
  // verifiable by the OPPOSITE server's pbkdf2 path (cross-implementation
  // crypto parity on a live write), and the old password must stop working.
  name: 'authr_settings_password_change', path: '/api/auth/settings', headers: {}, method: 'PUT',
  body: JSON.stringify({ current_password: PW_OLD, password: PW_NEW }),
  setup: async (base) => {
    resetParityUser();
    const tok = `rusttest-pwchange-${sideTag(base)}`;
    seedSession(tok);
    return { headers: { Authorization: `Bearer ${tok}`, 'content-type': 'application/json' }, ctx: { tok } };
  },
  readBack: async (other) => {
    const okNew = await apiReq(other, 'POST', '/api/auth/login', { 'content-type': 'application/json' }, JSON.stringify({ username: PARITY_USER, password: PW_NEW }));
    const badOld = await apiReq(other, 'POST', '/api/auth/login', { 'content-type': 'application/json' }, JSON.stringify({ username: PARITY_USER, password: PW_OLD }));
    return okNew.status === 200 && typeof okNew.json?.token === 'string' && badOld.status === 401
      ? { ok: true, why: 'new password verifies on the other server; old rejected', seen: { new_pw_verified: true, old_pw_rejected: true } }
      : { ok: false, why: `new=${okNew.status} old=${badOld.status}`, seen: null };
  },
  teardown: async () => { resetParityUser(); },
});
cases.push({
  // Negative twin of the writer block: current_password wrong must 401 AND
  // leave the old password working on the other server (no partial write).
  name: 'authr_settings_pwchange_wrong_401', path: '/api/auth/settings', headers: {}, method: 'PUT',
  body: JSON.stringify({ current_password: 'WrongPass!9999', password: PW_NEW }),
  setup: async (base) => {
    resetParityUser();
    const tok = `rusttest-pwwrong-${sideTag(base)}`;
    seedSession(tok);
    return { headers: { Authorization: `Bearer ${tok}`, 'content-type': 'application/json' }, ctx: { tok } };
  },
  readBack: async (other) => {
    const ok = await apiReq(other, 'POST', '/api/auth/login', { 'content-type': 'application/json' }, JSON.stringify({ username: PARITY_USER, password: PW_OLD }));
    return ok.status === 200 ? { ok: true, why: 'old password still works — 401 wrote nothing', seen: { unchanged: true } }
      : { ok: false, why: `rejected change still altered state: login ${ok.status}`, seen: null };
  },
  teardown: async () => { resetParityUser(); },
});
} // auth writers (gated on the local DB copy)


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

// Sets route group (Lane B unit 1 port: GET /api/sets; RUST_ROUTES=sets).
// Both sides read the shared `sets` table, so deepDiff (values, row order,
// key order) asserts the whole SQL port; ?tree=1 additionally drives the
// Scryfall child-set fold (cardSets.js getMtgChildSetMap through the
// rate-limited client), the part with real porting risk. ?lang is accepted
// and ignored by Node (void lang) — must stay byte-equal.
cases.push({ name: 'sets_flat', path: '/api/sets', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sets_tree', path: '/api/sets?tree=1', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sets_lang_de', path: '/api/sets?lang=de', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sets_empty_query', path: '/api/sets?', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sets_no_token', path: '/api/sets', headers: {}, method: 'GET' });
cases.push({ name: 'sets_bad_token', path: '/api/sets', headers: { Authorization: '***' }, method: 'GET' });

// Cards tags group (Lane B unit 2a port: oracle views; RUST_ROUTES=cards_tags).
// Fling has oracle_text cached, so the GET hits the card_cache columns on both
// sides with NO Scryfall leg (a legacy row would page live Scryfall and write
// back — cargo tests cover that leg with a mock, not parity). `no-card` is a
// non-mtg id absent from the cache: Node's getOracleText answers 404 from the
// row check alone, deterministic on both sides. The tag views read the active
// generation 71 closure tables in the shared DB; role/tag-stats bodies are
// pure query folds — no writes, no timers — so plain deepDiff is honest.
const TAGGED_CARD = 'mtg-12e4f93c-6daf-458e-aa5a-26844c54bfdd'; // 46 direct tags
const CACHED_RULES_CARD = 'mtg-cb28fe03-8269-41de-b766-42c3421aeaef'; // Fling
const ROLE_TAG_IDS = ['mtg-0001c639-8bd0-426f-89cb-4ca61f3cc054', 'mtg-000499a4-3f39-4d25-a2a2-b2f014e30754', 'mtg-0005c844-787c-4f0c-8d25-85cec151642b', 'mtg-000e2f80-7667-4a8b-91c2-359121d44ebb', 'mtg-00108a1c-e620-4124-9b31-3bb3ff2a0407', 'mtg-00127acd-a284-4bc8-bdfa-895914e3cd49'];
const ROLE_TAG_SLUGS = ['draw', 'card-advantage', 'burst-draw', 'synergy-exile-cast', 'definitely-not-a-real-tag'];
cases.push({ name: 'cards_oracle_ok', path: `/api/cards/${CACHED_RULES_CARD}/oracle`, headers: authHeaders, method: 'GET' });
cases.push({ name: 'cards_oracle_404', path: '/api/cards/no-card/oracle', headers: authHeaders, method: 'GET' });
cases.push({ name: 'cards_oracle_no_token', path: `/api/cards/${CACHED_RULES_CARD}/oracle`, headers: {}, method: 'GET' });
cases.push({ name: 'cards_oracle_tags_ok', path: `/api/cards/${TAGGED_CARD}/oracle-tags`, headers: authHeaders, method: 'GET' });
cases.push({ name: 'cards_oracle_tags_fallback', path: `/api/cards/${TAGGED_CARD}/oracle-tags?oracle_id=mtg-17b429bd-d7da-45f5-988b-7eed0d3efeaa`, headers: authHeaders, method: 'GET' });
cases.push({ name: 'cards_oracle_tags_404_shape', path: '/api/cards/no-card/oracle-tags', headers: authHeaders, method: 'GET' });
cases.push({ name: 'cards_oracle_tags_no_token', path: `/api/cards/${TAGGED_CARD}/oracle-tags`, headers: {}, method: 'GET' });
cases.push({ name: 'cards_role_tags_ok', path: '/api/cards/role-tags', headers: { ...AUTH_JSON, ...authHeaders }, method: 'POST', body: JSON.stringify({ ids: ROLE_TAG_IDS, slugs: ROLE_TAG_SLUGS }) });
cases.push({ name: 'cards_role_tags_missing_slugs', path: '/api/cards/role-tags', headers: { ...AUTH_JSON, ...authHeaders }, method: 'POST', body: JSON.stringify({ ids: ROLE_TAG_IDS }) });
cases.push({ name: 'cards_role_tags_too_many_ids', path: '/api/cards/role-tags', headers: { ...AUTH_JSON, ...authHeaders }, method: 'POST', body: JSON.stringify({ ids: Array.from({ length: 1001 }, (_, i) => `mtg-x${i}`), slugs: ROLE_TAG_SLUGS }) });
cases.push({ name: 'cards_role_tags_no_token', path: '/api/cards/role-tags', headers: { ...AUTH_JSON }, method: 'POST', body: JSON.stringify({ ids: ROLE_TAG_IDS, slugs: ROLE_TAG_SLUGS }) });
cases.push({ name: 'cards_tag_stats_ok', path: '/api/cards/tag-stats', headers: { ...AUTH_JSON, ...authHeaders }, method: 'POST', body: JSON.stringify({ ids: ROLE_TAG_IDS }) });
cases.push({ name: 'cards_tag_stats_empty_ids', path: '/api/cards/tag-stats', headers: { ...AUTH_JSON, ...authHeaders }, method: 'POST', body: '{"ids":[]}' });
cases.push({ name: 'cards_tag_stats_no_token', path: '/api/cards/tag-stats', headers: { ...AUTH_JSON }, method: 'POST', body: JSON.stringify({ ids: ROLE_TAG_IDS }) });

// Catalog + card-art group (Lane B unit 2b port; RUST_ROUTES=catalog_art).
// Reads only — the build engine (ONNX/sharp) and the sharp upload stay on
// Node, so the POST/DELETE card-art shapes below are NOT native proof (the
// proxy answers them); cargo tests carry the native surface proof, these
// assert Node answers the same requests identically. scan-sets and
// catalogs/languages are pure DB+FS folds over the shared snapshot (no
// timers, no writes): deepDiff is honest. Art fixtures live in
// data/card-art (created by this iteration; the Node container was
// restarted after they landed so its process-level ids cache saw them).
const MEMBER_SESSION = '7d1f4a52e3b0c9d8476155026af3e1b9c8d70246fae9135b0c2d8e4f6a7b1c3d';
cases.push({ name: 'card_art_index', path: '/api/card-art/index', headers: {}, method: 'GET' });
cases.push({ name: 'card_art_png_ok', path: '/api/card-art/parity-fixture-a.png', headers: {}, method: 'GET' });
cases.push({ name: 'card_art_png_404', path: '/api/card-art/parity-no-such-card.png', headers: {}, method: 'GET' });
cases.push({ name: 'card_art_png_bad_shape', path: '/api/card-art/a..b.png', headers: {}, method: 'GET' });
cases.push({ name: 'card_art_upload_no_token', path: '/api/card-art/parity-fixture-a', headers: {}, method: 'POST', body: '{}' });
cases.push({ name: 'scan_sets_ok', path: '/api/scan-sets', headers: authHeaders, method: 'GET' });
cases.push({ name: 'scan_sets_lang_ja', path: '/api/scan-sets?lang=ja', headers: authHeaders, method: 'GET' });
cases.push({ name: 'scan_sets_lang_es', path: '/api/scan-sets?lang=Spanish', headers: authHeaders, method: 'GET' });
cases.push({ name: 'scan_sets_no_token', path: '/api/scan-sets', headers: {}, method: 'GET' });
cases.push({ name: 'catalog_langs_ok', path: '/api/admin/catalogs/languages', headers: { Authorization: `Bearer ${USER1_TOKEN}` }, method: 'GET' });
cases.push({ name: 'catalog_langs_game_pokemon', path: '/api/admin/catalogs/languages?game=pokemon', headers: { Authorization: `Bearer ${USER1_TOKEN}` }, method: 'GET' });
cases.push({ name: 'catalog_langs_no_token', path: '/api/admin/catalogs/languages', headers: {}, method: 'GET' });
cases.push({ name: 'catalog_langs_member', path: '/api/admin/catalogs/languages', headers: { Authorization: `Bearer ${MEMBER_SESSION}` }, method: 'GET' });
cases.push({
  name: 'catalog_langs_apikey', path: '/api/admin/catalogs/languages',
  headers: { Authorization: '***' + 'k'.repeat(59) }, method: 'GET',
  setup: async () => {
    dbWrite((d) => d.prepare('UPDATE users SET api_key = ? WHERE username = ?').run('k'.repeat(59), 'sparks'));
    return null;
  },
  teardown: async () => {
    dbWrite((d) => d.prepare('UPDATE users SET api_key = NULL WHERE username = ?').run('sparks'));
  },
});

// --- Moxfield parity (Lane B unit3a) -----------------------------------------
// The status route is the deep fold (authors + their tracked decks + interval
// settings from app_settings) over the shared DB copy; the scenario token
// owns the real moxfield_authors row so the fold is exercised, not empty.
// Scheduler ticks are quiesced on BOTH sides (Rust
// SCRYBOX_DISABLE_MOXFIELD_SCHEDULER=1, Node container flag file), so the
// stamps stay stable between the two reads. Remaining cases cover the auth
// ladder and the validation/404 arms — none mutate the DB.
cases.push({ name: 'mox_status', path: '/api/moxfield', headers: authHeaders, method: 'GET' });
cases.push({ name: 'mox_status_no_token', path: '/api/moxfield', headers: {}, method: 'GET' });
cases.push({ name: 'mox_add_author_missing', path: '/api/moxfield/authors', headers: authHeaders, method: 'POST', body: '{}' });
cases.push({ name: 'mox_add_author_nonstring', path: '/api/moxfield/authors', headers: authHeaders, method: 'POST', body: JSON.stringify({ author: 123 }) });
cases.push({ name: 'mox_delete_missing', path: '/api/moxfield/authors/999999', headers: authHeaders, method: 'DELETE' });
cases.push({ name: 'mox_deck_put_missing', path: '/api/moxfield/decks/no-such-public-id', headers: authHeaders, method: 'PUT', body: '{"enabled":true}' });
cases.push({ name: 'mox_deck_sync_missing', path: '/api/moxfield/decks/no-such-public-id/sync', headers: authHeaders, method: 'POST' });

// --- ManaBox sync + backups parity (Lane B unit3b) ---------------------------
// preview is the deep fold (CSV parse + identity check + planCollection +
// planLists + summary + markdown report) over the shared DB copy with the
// fixture card already cached (no Scryfall leg fires). The report's
// wall-clock header line is scrubbed; every other byte of body must agree.
// Apply is exercised ONLY through the pre-write 409 arm (expectBefore guard
// rejects before any write) — the full write path is non-idempotent across
// the two sides and stays cargo-fixture-only (tests/manabox_sync.rs), per the
// collection_writes precedent. Admin backup GET lists the same host
// directory on both sides (container bind-mounts data/); the env-dependent
// `dir` string is stripped, the listing itself is compared.
const MB_HDR = 'Binder Name,Binder Type,Name,Set code,Collector number,Foil,Quantity,Scryfall ID,Condition,Language,Added';
const MB_UUID = 'dda6f965-cbdf-4dbc-b0b1-00c27e32f5f2'; // parity fixture card (cached, set trk #15)
const MB_CSV = [
  MB_HDR,
  `SyncTest,binder,Emergency Medical Hologram,trk,15,normal,1,${MB_UUID},mint,en,2024-01-02T03:04:05.000Z`,
  `SyncTestList,list,Emergency Medical Hologram,trk,15,normal,1,${MB_UUID},mint,en,2024-01-01T00:00:00.000Z`,
  `SyncTest,binder,Whatever,trk,15,metal,1,${MB_UUID},mint,en,`,
].join('\n') + '\n';
const MB_JSON = { ...authHeaders, 'content-type': 'application/json' };
const MB_TS = ['^# ManaBox sync [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2} UTC', '# ManaBox sync TS UTC'];
cases.push({ name: 'mb_preview_ok', path: '/api/manabox-sync/preview', headers: MB_JSON, method: 'POST',
  body: JSON.stringify({ csv: MB_CSV, fileName: 'parity.csv' }), scrub: [['$.report', MB_TS[0], MB_TS[1]]] });
cases.push({ name: 'mb_preview_empty', path: '/api/manabox-sync/preview', headers: MB_JSON, method: 'POST', body: '{}' });
cases.push({ name: 'mb_preview_not_export', path: '/api/manabox-sync/preview', headers: MB_JSON, method: 'POST', body: JSON.stringify({ csv: 'Name,Quantity\nx,2\n' }) });
cases.push({ name: 'mb_preview_no_token', path: '/api/manabox-sync/preview', headers: { 'content-type': 'application/json' }, method: 'POST', body: JSON.stringify({ csv: MB_CSV }) });
cases.push({ name: 'mb_preview_member', path: '/api/manabox-sync/preview', headers: { ...MB_JSON, Authorization: `Bearer ${MEMBER_SESSION}` }, method: 'POST',
  body: JSON.stringify({ csv: MB_CSV }), scrub: [['$.report', MB_TS[0], MB_TS[1]]] });
cases.push({ name: 'mb_apply_stale_409', path: '/api/manabox-sync/apply', headers: MB_JSON, method: 'POST',
  body: JSON.stringify({ csv: MB_CSV, expectBefore: 999999 }) });
cases.push({ name: 'mb_apply_empty', path: '/api/manabox-sync/apply', headers: MB_JSON, method: 'POST', body: '{}' });
cases.push({ name: 'mb_runs_empty', path: '/api/manabox-sync/runs', headers: authHeaders, method: 'GET' });
cases.push({ name: 'mb_report_missing', path: '/api/manabox-sync/runs/999999/report', headers: authHeaders, method: 'GET' });
cases.push({ name: 'mb_backups_list', path: '/api/admin/backups', headers: authHeaders, method: 'GET', strip: ['$.dir'] });
cases.push({ name: 'mb_backups_list_member', path: '/api/admin/backups', headers: { Authorization: `Bearer ${MEMBER_SESSION}` }, method: 'GET' });
cases.push({ name: 'mb_backups_list_no_token', path: '/api/admin/backups', headers: {}, method: 'GET' });

// --- iter 34: settings group (Lane A port: GET/PUT /api/settings,
// GET /api/settings/version, RUST_ROUTES=settings) ----------------------------
// The effective-settings fold is DB (app_settings row) + env half; restart-
// rust.sh mirrors the container's PUBLIC_BASE_URL / LOCAL_SCRYFALL_ENABLED /
// LOCAL_WEB_PORTAL_ENABLED into the Rust unit, so the merged body must be
// byte-equal including key ORDER. The check=1 GitHub leg is volatile (fetched
// + published_at stamp, external API) and stays cargo-fixture-only; the
// check=0 arm plus the full auth ladder run live. Write cases restore the
// row from a snapshot captured in setup (both sides answer from the same
// start; teardown always runs, retry-safe).
const SET_JSON = { ...authHeaders, 'content-type': 'application/json' };
const SET_COLS = ['public_base_url', 'price_refresh_days', 'scan_exclude_tokens', 'scan_exclude_art_cards',
  'scan_exclude_jumpstart', 'scan_exclude_promos', 'setup_complete', 'moxfield_decklist_interval_min',
  'moxfield_content_interval_min'];
function setSnap() {
  return dbWrite((d) => d.prepare(`SELECT ${SET_COLS.join(', ')} FROM app_settings WHERE id = 1`).get());
}
function setRestore(snap) {
  dbWrite((d) => d.prepare(`UPDATE app_settings SET ${SET_COLS.map((c) => c + ' = ?').join(', ')} WHERE id = 1`)
    .run(...SET_COLS.map((c) => snap[c])));
}
cases.push({ name: 'set_get_admin', path: '/api/settings', headers: authHeaders, method: 'GET' });
cases.push({ name: 'set_get_member', path: '/api/settings', headers: { Authorization: `Bearer ${MEMBER_SESSION}` }, method: 'GET' });
cases.push({ name: 'set_get_no_token', path: '/api/settings', headers: {}, method: 'GET' });
cases.push({ name: 'set_version_admin', path: '/api/settings/version?check=0', headers: authHeaders, method: 'GET' });
cases.push({ name: 'set_version_no_token', path: '/api/settings/version', headers: {}, method: 'GET' });
cases.push({ name: 'set_put_no_token', path: '/api/settings', headers: { 'content-type': 'application/json' }, method: 'PUT', body: '{"price_refresh_days":3}' });
cases.push({ name: 'set_put_member', path: '/api/settings', headers: { Authorization: `Bearer ${MEMBER_SESSION}`, 'content-type': 'application/json' }, method: 'PUT', body: '{"price_refresh_days":3}' });
cases.push({ name: 'set_put_badjson_noauth', path: '/api/settings', headers: { 'content-type': 'application/json' }, method: 'PUT', body: '{oops' });
cases.push({
  name: 'set_put_price_7', path: '/api/settings', headers: SET_JSON, method: 'PUT',
  body: JSON.stringify({ price_refresh_days: 7 }),
  setup: async () => ({ ctx: { snap: setSnap() } }),
  teardown: async (_b, ctx) => { if (ctx?.snap) setRestore(ctx.snap); },
  readBack: async (other) => {
    const r = await fetch(other + '/api/settings', { headers: authHeaders });
    const j = await r.json();
    return { ok: j.price_refresh_days === 7, why: `price read-back ${j.price_refresh_days}`, seen: j.price_refresh_days };
  },
});
cases.push({
  name: 'set_put_price_prefix_400', path: '/api/settings', headers: SET_JSON, method: 'PUT',
  body: JSON.stringify({ price_refresh_days: '7.9abc' }),
  setup: async () => ({ ctx: { snap: setSnap() } }),
  teardown: async (_b, ctx) => { if (ctx?.snap) setRestore(ctx.snap); },
});
cases.push({
  name: 'set_put_baseurl_trim', path: '/api/settings', headers: SET_JSON, method: 'PUT',
  body: JSON.stringify({ public_base_url: '  https://parity.test:9  ' }),
  setup: async () => ({ ctx: { snap: setSnap() } }),
  teardown: async (_b, ctx) => { if (ctx?.snap) setRestore(ctx.snap); },
  readBack: async (other) => {
    const r = await fetch(other + '/api/settings', { headers: authHeaders });
    const j = await r.json();
    return { ok: j.public_base_url === 'https://parity.test:9', why: `base_url read-back ${JSON.stringify(j.public_base_url)}`, seen: j.public_base_url };
  },
});
cases.push({
  name: 'set_put_noop_empty', path: '/api/settings', headers: SET_JSON, method: 'PUT', body: '{}',
  setup: async () => ({ ctx: { snap: setSnap() } }),
  teardown: async (_b, ctx) => { if (ctx?.snap) setRestore(ctx.snap); },
});

// --- iter 35: precons group (Lane A port: GET /api/precons,
// POST /api/precons/import, RUST_ROUTES=precons) ------------------------------
// The search reads ride the SHARED mirror data/precons.json (bind-mounted
// into both containers), so with the mirror < 24 h old both sides answer from
// the same deck list with ZERO upstream hits — the ?nocache= refresh leg is
// cargo-fixture-only (parallel sides would race the mirror write). The rank
// ladder, PRECON_TYPES filter, 50-cap and localeCompare tie-break are
// compared byte-for-byte incl. key order. The q fold exercises the shared
// live-pinned qs twin (repeated-key join, q[]/q[0] array slots, q[k] object,
// raw-% escape kept literal — container qs probes). Import: guards answer
// before any upstream; deck-file fetch failures forward the WHOLE request so
// Node authors both sides' bytes; the happy paths run the real Scryfall bulk
// (deck BRC Urza's Iron Alliance, 100 cards, clean resolutions) and the
// created deck is verified through the OPPOSITE server's GET /api/decks/:id
// (deck id is random by design → stripped; message/cards/notFound must agree,
// persisted shape cross-checked, probes deleted by name in teardown).
const PC_JSON = { ...authHeaders, 'content-type': 'application/json' };
const PC_DECK = 'UrzaSIronAlliance_BRC'; // Commander Deck, 2022 BRC — stable, fully resolvable
const PC_NAME_A = 'PCParity Commander';
const PC_NAME_B = 'PCParity Override';
function pcClean(name) {
  dbWrite((d) => {
    const ids = d.prepare('SELECT id FROM decks WHERE name = ?').all(name).map((r) => r.id);
    const delc = d.prepare('DELETE FROM deck_cards WHERE deck_id = ?');
    for (const id of ids) delc.run(id);
    return d.prepare('DELETE FROM decks WHERE name = ?').run(name).changes;
  });
}
function pcReadDeck(other, id) {
  return fetch(other + `/api/decks/${id}`, {
    headers: { ...authHeaders, 'x-forwarded-for': xff(), 'cache-control': 'no-cache' },
    redirect: 'manual',
  }).then(async (r) => ({ status: r.status, json: await r.json().catch(() => null) }));
}
function pcDeckView(j) {
  if (!j) return null;
  const cards = Array.isArray(j.cards) ? j.cards : [];
  return {
    name: j.name, format: j.format, category: j.category,
    accent: j.accent_color, size: j.target_size, src: j.source,
    n: cards.length, ncmd: cards.filter((c) => c.is_commander).length,
  };
}
cases.push({ name: 'pc_search_no_token', path: '/api/precons?q=urza', headers: {}, method: 'GET' });
cases.push({ name: 'pc_search_missing_q', path: '/api/precons', headers: authHeaders, method: 'GET' });
cases.push({ name: 'pc_search_blank_q', path: '/api/precons?q=%20%20', headers: authHeaders, method: 'GET' });
cases.push({ name: 'pc_search_ok_urza', path: '/api/precons?q=urza', headers: authHeaders, method: 'GET' });
cases.push({ name: 'pc_search_caps_the', path: '/api/precons?q=the', headers: authHeaders, method: 'GET' });
// qs fold: repeated keys join with ',' (matches nothing); q[0] builds an
// array slot (needle 'urza'); q[k] stringifies to '[object Object]'.
cases.push({ name: 'pc_search_join_q', path: '/api/precons?q=zzz&q=alpha', headers: authHeaders, method: 'GET' });
cases.push({ name: 'pc_search_arr0_q', path: '/api/precons?q%5B0%5D=urza', headers: authHeaders, method: 'GET' });
cases.push({ name: 'pc_search_obj_q', path: '/api/precons?q%5Bk%5D=urza', headers: authHeaders, method: 'GET' });
// raw malformed escape: qs keeps '%zz' literal — 200 empty, NOT 400.
cases.push({ name: 'pc_search_rawpct_q', path: '/api/precons?q=%zz', headers: authHeaders, method: 'GET' });
// Node applies no charset gate to q: odd needles simply search empty.
cases.push({ name: 'pc_search_underscore_q', path: '/api/precons?q=Sol_Ring', headers: authHeaders, method: 'GET' });
cases.push({ name: 'pc_search_bang_q', path: '/api/precons?q=my_deck%21', headers: authHeaders, method: 'GET' });
// unknown subpath / other methods never 405 from Rust: the handlers
// self-check and forward; Node's router 404s either way.
cases.push({ name: 'pc_unknown_subpath', path: '/api/precons/nope', headers: authHeaders, method: 'GET' });
cases.push({ name: 'pc_put_method', path: '/api/precons', headers: PC_JSON, method: 'PUT', body: '{}' });
// import guards (answer before any upstream touch)
cases.push({ name: 'pc_import_no_token', path: '/api/precons/import', headers: { 'content-type': 'application/json' }, method: 'POST', body: '{}' });
cases.push({ name: 'pc_import_missing_file', path: '/api/precons/import', headers: PC_JSON, method: 'POST', body: '{}' });
cases.push({ name: 'pc_import_nonstring_file', path: '/api/precons/import', headers: PC_JSON, method: 'POST', body: '{"fileName":42}' });
cases.push({ name: 'pc_import_sanitized_empty', path: '/api/precons/import', headers: PC_JSON, method: 'POST', body: '{"fileName":"../../"}' });
cases.push({ name: 'pc_import_bad_json', path: '/api/precons/import', headers: PC_JSON, method: 'POST', body: '{"fileName":' });
// deck-file fetch errors answer LOCALLY with the byte-faithful axios-shape
// message ('Request failed with status code 404'); only Scryfall-side states
// forward the whole request (see cargo pins import_deck_fetch_errors /
// import_partial_miss_forwards_whole_request).
cases.push({ name: 'pc_import_missing_deck', path: '/api/precons/import', headers: PC_JSON, method: 'POST',
  body: JSON.stringify({ fileName: 'NoSuchDeckXYZ' }) });
// happy path 1: commander deck, default shape (Commander/EDH, #8b5cf6, 100,
// category null), includeCommander default-true keeps the commander section.
cases.push({ name: 'pc_import_ok', path: '/api/precons/import', headers: PC_JSON, method: 'POST',
  strip: ['$.id'],
  setup: async () => { pcClean(PC_NAME_A); pcClean(PC_NAME_B); return { ctx: {} }; },
  body: JSON.stringify({ fileName: PC_DECK, name: PC_NAME_A }),
  readBack: async (other, ctx, res) => {
    const id = res?.json?.id;
    if (typeof id !== 'number') return { ok: false, why: `no deck id in ${JSON.stringify(res?.json)}`, seen: null };
    const { status, json } = await pcReadDeck(other, id);
    const view = pcDeckView(json);
    return { ok: status === 200 && view && view.src === 'precon' && view.format === 'Commander / EDH'
        && view.category === null && view.accent === '#8b5cf6' && view.size === 100 && view.ncmd >= 1,
      why: `deck read-back: ${status} ${JSON.stringify(view)}`, seen: view };
  },
  teardown: async () => { pcClean(PC_NAME_A); } });
// happy path 2: every body override honoured + includeCommander=false drops
// the commander section; targetSize arrives as a STRING ('81' → Number 81,
// stored INTEGER — REAL would diverge through the opposite server's read).
cases.push({ name: 'pc_import_overrides', path: '/api/precons/import', headers: PC_JSON, method: 'POST',
  strip: ['$.id'],
  setup: async () => { pcClean(PC_NAME_A); pcClean(PC_NAME_B); return { ctx: {} }; },
  body: JSON.stringify({ fileName: PC_DECK, name: PC_NAME_B, format: 'Legacy', category: 'Casual',
    accentColor: '#123456', targetSize: '81', includeCommander: false }),
  readBack: async (other, ctx, res) => {
    const id = res?.json?.id;
    if (typeof id !== 'number') return { ok: false, why: `no deck id in ${JSON.stringify(res?.json)}`, seen: null };
    const { status, json } = await pcReadDeck(other, id);
    const view = pcDeckView(json);
    return { ok: status === 200 && view && view.name === PC_NAME_B && view.format === 'Legacy'
        && view.category === 'Casual' && view.accent === '#123456' && view.size === 81
        && view.src === 'precon' && view.ncmd === 0,
      why: `deck read-back: ${status} ${JSON.stringify(view)}`, seen: view };
  },
  teardown: async () => { pcClean(PC_NAME_B); } });

// --- iter 37: secret_lair group (Lane A port: GET /api/secret-lair,
// GET /api/secret-lair/preview, POST /api/secret-lair/add; RUST_ROUTES=secret_lair)
// Search reads ride the SAME shared mirror data/precons.json the precons
// group pinned (<24 h → zero upstream hits, identical DeckList on both
// sides). Needle 'box of rocks' scores exactly the A Box of Rocks base + its
// FoilEdition twin (slot fold by trimmed-lowercased NAME, /foiledition/i on
// the NAME — distinct from the fileName test in resolveFoilFileName); q=the
// exercises the rank ladder + localeCompare ties over a large match set.
// preview/add fetch the MTGJSON deck file through the SHARED data/precons/
// cache; Scryfall bulk is live (same lane as pc_import). The collection
// writes land in the shared DB and are deleted by card id in teardown;
// owned counts compare server-vs-server over the same table, so ordering is
// not a divergence source.
const SL_JSON = { ...authHeaders, 'content-type': 'application/json' };
const SL_BASE = 'ABoxOfRocks_SLD';
const SL_FOIL = 'ABoxOfRocksFoilEdition_SLD';
const SL_NOTWIN = 'AlienAuroras_SLD';
async function slDeckIds(fileName) {
  const r = await fetch(`https://mtgjson.com/api/v5/decks/${encodeURIComponent(fileName)}.json`);
  const j = await r.json();
  const rows = [...(j?.data?.mainBoard || []), ...(j?.data?.commander || [])];
  return rows
    .map((c) => c?.identifiers?.scryfallId)
    .filter(Boolean)
    .map((id) => `mtg-${id}`);
}
function slClean(_base, ids) {
  dbWrite((d) => {
    const del = d.prepare('DELETE FROM collection WHERE card_id = ?');
    for (const id of (ids || [])) del.run(id);
    return (ids || []).length;
  });
}
cases.push({ name: 'sl_search_no_token', path: '/api/secret-lair?q=rocks', headers: {}, method: 'GET' });
cases.push({ name: 'sl_search_empty_q', path: '/api/secret-lair?q=', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_search_rocks', path: '/api/secret-lair?q=box%20of%20rocks', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_search_the_rank', path: '/api/secret-lair?q=the', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_search_limit3', path: '/api/secret-lair?q=the&limit=3', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_search_limit_bad', path: '/api/secret-lair?q=the&limit=abc', headers: authHeaders, method: 'GET' });
// qs twin on the new keys: repeated q joins; q[0] is an array slot;
// foil arriving as an ARRAY never === '1' (false), fileName array → String().
cases.push({ name: 'sl_search_join_q', path: '/api/secret-lair?q=rocks&q=box', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_search_arr0_q', path: '/api/secret-lair?q%5B0%5D=rocks', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_preview_no_token', path: `/api/secret-lair/preview?fileName=${SL_BASE}`, headers: {}, method: 'GET' });
cases.push({ name: 'sl_preview_missing_file', path: '/api/secret-lair/preview', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_preview_blank_file', path: '/api/secret-lair/preview?fileName=%20', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_preview_ok', path: `/api/secret-lair/preview?fileName=${SL_BASE}`, headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_preview_foil_twin', path: `/api/secret-lair/preview?fileName=${SL_BASE}&foil=1`, headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_preview_foil_passthrough', path: `/api/secret-lair/preview?fileName=${SL_FOIL}&foil=true`, headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_preview_foil_missing_422', path: `/api/secret-lair/preview?fileName=${SL_NOTWIN}&foil=1`, headers: authHeaders, method: 'GET' });
// unknown fileName passes the twin lookup untouched; the deck fetch answers
// honestly with the axios-status-carrying message.
cases.push({ name: 'sl_preview_missing_deck', path: '/api/secret-lair/preview?fileName=NoSuchDeckXYZ', headers: authHeaders, method: 'GET' });
// fileName arriving as an array → String(['A','B']) === 'A,B' → cleaned to
// 'AB' → deck 404 (pins the String() coercion, not a 400).
cases.push({ name: 'sl_preview_arr_file', path: '/api/secret-lair/preview?fileName%5B0%5D=A&fileName%5B1%5D=B', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_add_no_token', path: '/api/secret-lair/add', headers: { 'content-type': 'application/json' }, method: 'POST', body: '{}' });
cases.push({ name: 'sl_add_missing_file', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST', body: '{}' });
cases.push({ name: 'sl_add_blank_file', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST', body: '{"fileName":"   "}' });
cases.push({ name: 'sl_add_bad_json', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST', body: '{"fileName":' });
cases.push({ name: 'sl_add_nonstring_file', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST', body: '{"fileName":42}' });
// add guard ORDER: fileName required answers BEFORE the foil twin lookup
// (blank fileName + foil:true still the plain 400).
cases.push({ name: 'sl_add_blank_file_foil', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST', body: '{"fileName":"","foil":true}' });
// happy add: 5 SLD card types through the live Scryfall bulk, collection
// rows verified in the shared DB (same rows read by BOTH sides — the
// response itself carries the counts).
cases.push({ name: 'sl_add_ok', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST',
  setup: async () => ({ ctx: { ids: await slDeckIds(SL_BASE) } }),
  body: JSON.stringify({ fileName: SL_BASE, condition: 'Near Mint' }),
  readBack: async (other, ctx) => {
    const got = dbWrite((d) => d.prepare(
      `SELECT card_id, quantity, printing, language, condition FROM collection
        WHERE card_id IN (${ctx.ids.map(() => '?').join(',')}) ORDER BY card_id`,
    ).all(...ctx.ids));
    return { ok: got.length === ctx.ids.length && got.every((r) => r.printing === 'Normal'
        && r.condition === 'Near Mint' && r.quantity >= 1),
      why: `collection rows: ${JSON.stringify(got)}`, seen: got };
  },
  teardown: async (_b, ctx) => slClean(_b, ctx?.ids || []) });
// foil=true strict form swaps the twin; copies '2x' is the Number()-vs-
// parseInt trap: Number('2x') is NaN → || 1 → mult 1, so every quantity
// stays at the product's own count (1). The response fileName is the twin,
// resolvedFoil true.
cases.push({ name: 'sl_add_copies_foil', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST',
  setup: async () => ({ ctx: { ids: await slDeckIds(SL_FOIL) } }),
  body: JSON.stringify({ fileName: SL_BASE, foil: true, copies: '2x' }),
  readBack: async (other, ctx) => {
    const got = dbWrite((d) => d.prepare(
      `SELECT card_id, quantity, printing FROM collection
        WHERE card_id IN (${ctx.ids.map(() => '?').join(',')}) ORDER BY card_id`,
    ).all(...ctx.ids));
    return { ok: got.length === ctx.ids.length && got.every((r) => r.printing === 'Normal' && r.quantity === 1),
      why: `foil/copy rows: ${JSON.stringify(got)}`, seen: got };
  },
  teardown: async (_b, ctx) => slClean(_b, ctx?.ids || []) });
// copies '2' IS a clean Number → mult 2 → every quantity doubles (2x1).
cases.push({ name: 'sl_add_copies_numeric', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST',
  setup: async () => ({ ctx: { ids: await slDeckIds(SL_BASE) } }),
  body: JSON.stringify({ fileName: SL_BASE, copies: '2' }),
  readBack: async (other, ctx) => {
    const got = dbWrite((d) => d.prepare(
      `SELECT card_id, quantity, printing FROM collection
        WHERE card_id IN (${ctx.ids.map(() => '?').join(',')}) ORDER BY card_id`,
    ).all(...ctx.ids));
    return { ok: got.length === ctx.ids.length && got.every((r) => r.printing === 'Normal' && r.quantity === 2),
      why: `numeric-copy rows: ${JSON.stringify(got)}`, seen: got };
  },
  teardown: async (_b, ctx) => slClean(_b, ctx?.ids || []) });
// printingMode 'Foil' hits the whitelist after toLowerCase → EVERY row is
// forced Holofoil even though the deck rows are isFoil-blind.
cases.push({ name: 'sl_add_printing_mode_foil', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST',
  setup: async () => ({ ctx: { ids: await slDeckIds(SL_BASE) } }),
  body: JSON.stringify({ fileName: SL_BASE, printingMode: 'Foil' }),
  readBack: async (other, ctx) => {
    const got = dbWrite((d) => d.prepare(
      `SELECT card_id, quantity, printing FROM collection
        WHERE card_id IN (${ctx.ids.map(() => '?').join(',')}) ORDER BY card_id`,
    ).all(...ctx.ids));
    return { ok: got.length === ctx.ids.length && got.every((r) => r.printing === 'Holofoil'),
      why: `mode-forced rows: ${JSON.stringify(got)}`, seen: got };
  },
  teardown: async (_b, ctx) => slClean(_b, ctx?.ids || []) });
// Off-list condition is rewritten to 'Near Mint' by the util's own whitelist.
cases.push({ name: 'sl_add_condition_whitelist', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST',
  setup: async () => ({ ctx: { ids: await slDeckIds(SL_BASE) } }),
  body: JSON.stringify({ fileName: SL_BASE, condition: 'Like New' }),
  readBack: async (other, ctx) => {
    const got = dbWrite((d) => d.prepare(
      `SELECT card_id, condition FROM collection
        WHERE card_id IN (${ctx.ids.map(() => '?').join(',')}) ORDER BY card_id`,
    ).all(...ctx.ids));
    return { ok: got.length === ctx.ids.length && got.every((r) => r.condition === 'Near Mint'),
      why: `grade-rewritten rows: ${JSON.stringify(got)}`, seen: got };
  },
  teardown: async (_b, ctx) => slClean(_b, ctx?.ids || []) });
// body.foil === '1' (string) is NOT a strict-=== form → base product.
cases.push({ name: 'sl_add_foil_strict_string', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST',
  setup: async () => ({ ctx: { ids: await slDeckIds(SL_BASE) } }),
  body: JSON.stringify({ fileName: SL_BASE, foil: '1' }),
  teardown: async (_b, ctx) => slClean(_b, ctx?.ids || []) });
// no FoilEdition twin → the route-level 422 {error} before any deck fetch.
cases.push({ name: 'sl_add_foil_missing_422', path: '/api/secret-lair/add', headers: SL_JSON, method: 'POST',
  body: JSON.stringify({ fileName: SL_NOTWIN, foil: true }) });
// unknown subpath + other method never 405 from Rust: handlers self-check
// and forward; Node's router answers either way.
cases.push({ name: 'sl_unknown_subpath', path: '/api/secret-lair/nope', headers: authHeaders, method: 'GET' });
cases.push({ name: 'sl_put_method', path: '/api/secret-lair', headers: SL_JSON, method: 'PUT', body: '{}' });

// --- rules group (iter-36: GET /api/rules, /api/rules/keywords; RUST_ROUTES=rules)
// `mem` is PER PROCESS in the Node twin: once each server has built, its
// fetched stamp is its own (like two Node processes would). '$.fetched' is
// therefore stripped on every case (same class as networth's '$.asOf'), and
// '$.stale' for live-upstream hiccup resilience; everything else — the full
// rules/keywords payloads, key ORDER, crUrl/crVersion/commanderUrl — is
// compared byte-for-byte.
cases.push({ name: 'rl_rules_ok', path: '/api/rules', headers: authHeaders, method: 'GET',
  strip: ['$.fetched', '$.stale'], etagReplay: false });
cases.push({ name: 'rl_rules_no_token', path: '/api/rules', headers: {}, method: 'GET', etagReplay: false });
cases.push({ name: 'rl_rules_bad_token', path: '/api/rules', headers: { Authorization: `Bearer ${'Bad' + 'Token999'}` }, method: 'GET', etagReplay: false });
cases.push({ name: 'rl_keywords_ok', path: '/api/rules/keywords', headers: authHeaders, method: 'GET',
  strip: ['$.fetched'], etagReplay: false });
cases.push({ name: 'rl_keywords_no_token', path: '/api/rules/keywords', headers: {}, method: 'GET', etagReplay: false });
// 'refresh' presence (any value, incl. bare ?refresh) forces the rebuild —
// both sides must answer 200 with the same payload minus build stamps.
cases.push({ name: 'rl_rules_refresh', path: '/api/rules?refresh=1', headers: authHeaders, method: 'GET',
  strip: ['$.fetched', '$.crUrl', '$.crVersion', '$.commanderUrl', '$.stale'], etagReplay: false });
cases.push({ name: 'rl_keywords_refresh', path: '/api/rules/keywords?refresh', headers: authHeaders, method: 'GET',
  strip: ['$.fetched'], etagReplay: false });
// fall-throughs: unknown subpath under the mount + non-GET never 405 locally
// (handlers self-check and forward; Node's router answers).
cases.push({ name: 'rl_unknown_subpath', path: '/api/rules/nope', headers: authHeaders, method: 'GET', etagReplay: false });
cases.push({ name: 'rl_post_method', path: '/api/rules', headers: { ...authHeaders, 'content-type': 'application/json' }, method: 'POST', body: '{}', etagReplay: false });

// --- write-parity helpers (iter 22: collection_writes group) -----------------
// A write case is only honest if BOTH sides answer the identical request from
// the identical starting state. `setup(base)` runs immediately before each
// side's request (per attempt — a retry gets a fresh probe) and can rewrite
// path/body/headers and hand state to `teardown(base, ctx)`, which always
// cleans up through the API so the shared DB copy is left as found. Probes
// are tagged with a unique marker in `notes`; teardown deletes ONLY rows
// whose notes match, never pre-existing user data.
let ipSeq = 1;
// RFC 2544 benchmark space 198.18.0.0/15: NOT private, so proxy-addr / the
// Rust Trust rules treat it as the real client (unlike 10.x — under
// TRUST_PROXY=loopback,uniquelocal a trusted XFF collapses to the socket
// bucket, which is production-correct but made every harness request share
// one authLimiter window and the auth-writer password cases 429 once Rust
// restarted mid-iteration). /15 gives 65k unique keys; key_for() uses the
// full v4 string, so every request gets its own limiter bucket.
function xff() { const n = ipSeq++; return `198.18.${(n >> 8) & 255}.${n & 255}`; }

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

async function fetchSide(base, c, opts = {}) {
  const t0 = Date.now();
  let path = c.path, body = c.body, headers = c.headers, ctx = null;
  if (c.setup) {
    const s = await c.setup(base, c);
    if (s) { path = s.path ?? path; body = s.body ?? body; headers = s.headers ?? headers; ctx = s.ctx ?? null; }
  }
  // Connection-level flakes (undici `TypeError: fetch failed` with an
  // EPIPE/ECONNRESET cause — a stale keep-alive socket or an early server
  // close mid-upload) must not crash the suite: the whole case is compared
  // under MAX_TRIES, but an exception escaping here skipped even that.
  // Retry the fetch itself (string bodies — safe to resend; write cases run
  // serially with a fresh setup, and a duplicate landing surfaces as a
  // cross-side mismatch on the next MAX_TRIES attempt, not a false PASS).
  let res, resBody;
  let lastErr = null;
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      res = await fetch(base + path, {
        method: c.method,
        headers: { ...headers, 'x-forwarded-for': xff(), 'cache-control': 'no-cache' },
        body,
        redirect: 'manual',
      });
      resBody = res.status === 304 ? '' : await res.text();
      lastErr = null;
      break;
    } catch (e) {
      lastErr = e;
      if (!(e instanceof TypeError)) break; // only connection-level errors are retryable
      await new Promise((r) => setTimeout(r, 150 * (attempt + 1)));
    }
  }
  if (lastErr) throw lastErr;
  const parsed = resBody && (res.headers.get('content-type') || '').includes('json') ? JSON.parse(resBody) : undefined;
  // Cross-side write verification BEFORE teardown (the probe must still exist).
  // `parsed` lets auth-writer readBacks assert the SHAPE of freshly issued
  // random values (tokens, api keys) — readBacks that don't need it (cw_*)
  // ignore the extra argument.
  let readback;
  if (c.readBack) {
    const other = base === RUST ? NODE : RUST;
    try { readback = await c.readBack(other, ctx, { json: parsed, status: res.status }); }
    catch (e) { readback = { ok: false, why: `readBack threw ${e}`, seen: null }; }
  }
  // etagParity refetches pass skipTeardown: tearing down here would wipe the
  // seeded rows and the ETag replays below would then read an emptied
  // fixture (the iter-27 shrd_* replay failures: 200-with-empty-body instead
  // of 304). etagParity runs the case's teardown itself once after replays.
  if (c.teardown && !opts.skipTeardown) {
    try { await c.teardown(base, ctx); } catch (e) { console.log(`  (teardown ${c.name}: ${e})`); }
  }
  return {
    status: res.status,
    body: resBody,
    json: parsed,
    total: res.headers.get('x-total-count'),
    source: res.headers.get('x-source'),
    etag: res.headers.get('etag'),
    ctype: (res.headers.get('content-type') || '').split(';')[0].trim().toLowerCase(),
    disposition: res.headers.get('content-disposition'),
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

/// Remove `$.a.b`-style paths from a parsed body before comparison (the
/// auth-writer cases strip values that are random by design — session
/// tokens, issued keys — whose SHAPE and PERSISTENCE the readBack asserts).
function stripPaths(obj, paths) {
  if (!paths || !obj || typeof obj !== 'object') return obj;
  const clone = structuredClone(obj);
  for (const p of paths) {
    const parts = p.replace(/^\$\.?/, '').split('.');
    let cur = clone;
    for (let i = 0; i < parts.length - 1 && cur; i++) cur = cur[parts[i]];
    if (cur && typeof cur === 'object') delete cur[parts[parts.length - 1]];
  }
  return clone;
}

/// Like stripPaths but REWRITES string leaves at `$.a.b` paths
/// ([[path, regexSource, replacement], ...]). Used by manabox preview: the
/// report's `# ManaBox sync <ISO minute> UTC` header is wall-clock by design,
/// everything else in the report is a pure fold and must match byte for byte.
function scrubStrings(obj, scrubs) {
  if (!scrubs || !obj || typeof obj !== 'object') return obj;
  const clone = structuredClone(obj);
  for (const [p, re, rep] of scrubs) {
    const parts = p.replace(/^\$\.?/, '').split('.');
    let cur = clone;
    for (let i = 0; i < parts.length - 1 && cur; i++) cur = cur[parts[i]];
    const leaf = parts[parts.length - 1];
    if (cur && typeof cur[leaf] === 'string') cur[leaf] = cur[leaf].replace(new RegExp(re, 'g'), rep);
  }
  return clone;
}

function compareCase(c, r, n) {
  const diffs = [];
  if (r.status !== n.status) diffs.push(`status ${r.status} vs ${n.status}`);
  if (r.ctype !== n.ctype) diffs.push(`content-type ${r.ctype} vs ${n.ctype}`);
  if (c.cmpDisposition && r.disposition !== n.disposition) diffs.push(`content-disposition ${r.disposition} vs ${n.disposition}`);
  if ((r.total ?? undefined) !== (n.total ?? undefined)) diffs.push(`x-total-count ${r.total} vs ${n.total}`);
  if ((r.source ?? undefined) !== (n.source ?? undefined)) diffs.push(`x-source ${r.source} vs ${n.source}`);
  if (r.json !== undefined || n.json !== undefined) {
    if (r.json === undefined || n.json === undefined) diffs.push('one side non-JSON body');
    else deepDiff(scrubStrings(stripPaths(r.json, c.strip), c.scrub), scrubStrings(stripPaths(n.json, c.strip), c.scrub), '$', diffs);
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
  if (!c.path.startsWith('/api/collection') && !c.path.startsWith('/api/search') && !c.path.startsWith('/api/stats') && !c.path.startsWith('/api/shared') && !c.path.startsWith('/api/decks') && !c.path.startsWith('/api/lists')) return [];
  // Sequential on purpose: setup-bearing cases reseed shared DB rows, and
  // two parallel setups would race each other's DELETE/INSERT mid-request
  // (the iter-27 shrd_* replay failures). Same rationale as the write-case
  // serial rule in the main loop. skipTeardown: the refetch must leave the
  // fixture in place for the self-replay probes below; the case teardown
  // then runs once in the finally.
  let diffs = [];
  try {
  const r = await fetchSide(RUST, c, { skipTeardown: true });
  const n = await fetchSide(NODE, c, { skipTeardown: true });
  if (r.status !== 200 || n.status !== 200) return [];
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
  // Cases whose body is RANDOM BY DESIGN (e.g. /stats/networth: cached()'s
  // onHit restamps asOf = new Date().toISOString() on EVERY response, hit or
  // miss, so the Express body-derived ETag differs per request and a client
  // can never observe a matching If-None-Match — 304 is unreachable by
  // construction on BOTH sides). `etagReplay: false` opts those out of the
  // self-replay probe; the ETag-presence checks above still run, and the
  // response's shape is pinned by a dedicated readBack case instead.
  if (c.etagReplay === false) return diffs;
  for (const [name, base, first] of [['rust', RUST, r], ['node', NODE, n]]) {
    if (!first.etag) continue;
    const again = await rawGet(base, c.path, { ...c.headers, 'if-none-match': first.etag });
    if (again.status !== 304) diffs.push(`${name}: replay of own ETag answered ${again.status}, not 304`);
    if (again.bytes !== 0) diffs.push(`${name}: 304 carried a body (${again.bytes} bytes)`);
  }
  return diffs;
  } finally {
    if (c.teardown) { try { await c.teardown(RUST, null); } catch (e) { console.log(`  (teardown ${c.name}: ${e})`); } }
  }
}

  // --- add-group cases (iter 24: POST /api/collection, POST
  // /api/collection/bulk-add; RUST_ROUTES=collection_adds). Adds CREATE
  // rows, so the request response carries the ids: readBack stashes them
  // for teardown AND reads them back through the OPPOSITE server (owner
  // ruling iter 21 — a write is only verified when the other server sees
  // it), so a Rust write that Node's reads can't confirm fails the case.
  // ids/id/entries are autoincrement-random and stripped from the
  // cross-side diff; the persisted-view JSON carries the real values. ---
  const addCtx = async () => ({ ctx: {} });
  const collectAddIds = (json) => [
    ...(Array.isArray(json?.ids) ? json.ids : []),
    ...(Array.isArray(json?.entries) ? json.entries.flatMap((e) => e?.ids || []) : []),
  ];
  function addReadBack(why, pred) {
    return async (other, ctx, resp) => {
      const ids = collectAddIds(resp?.json);
      if (ctx) ctx.ids = ids;
      if (!ids.length) return { ok: false, why: `${why}: response carried no ids` };
      const rows = await rowsByIdsVia(other, ids);
      if (rows.length !== ids.length) {
        return { ok: false, why: `${why}: ${ids.length} ids -> ${rows.length} rows via ${other}` };
      }
      return { ok: pred(rows, resp), why: `${why} -> ${JSON.stringify(rows)}`, seen: persistView(rows) };
    };
  }

  cases.push(
    { name: 'ca_add_401', path: '/api/collection', headers: {}, method: 'POST', body: JSON.stringify({ card_id: CW_CARD }) },
    { name: 'ca_add_missing_400', path: '/api/collection', headers: CW_JSON, method: 'POST', body: '{}' },
    { name: 'ca_add_null_card_400', path: '/api/collection', headers: CW_JSON, method: 'POST', body: '{"card_id":null}' },
    { name: 'ca_add_bad_printing_400', path: '/api/collection', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_id: CW_CARD, printing: 'Foil' }) },
    { name: 'ca_add_bool_printing_400', path: '/api/collection', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_id: CW_CARD, printing: true }) },
    { name: 'ca_add_success_200', path: '/api/collection', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_id: CW_CARD, quantity: 2, condition: 'Lightly Played', printing: 'Normal',
        language: 'English', purchase_price: '2.5', is_trade: 1 }),
      strip: ['$.id', '$.ids'], setup: addCtx, teardown: deleteProbes,
      readBack: addReadBack('add rows', (rows) =>
        rows.length === 2 && rows.every((r) => r.condition === 'Lightly Played' && r.quantity === 1
          && r.is_trade === 1 && r.purchase_price === 2.5)) },
    { name: 'ca_add_stack_200', path: '/api/collection', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_id: CW_CARD, quantity: 3, stackable: true }),
      strip: ['$.id', '$.ids'], setup: addCtx, teardown: deleteProbes,
      readBack: addReadBack('stacked row', (rows) =>
        rows.length === 1 && rows[0].quantity === 3) },
    { name: 'ca_add_zero_qty_200', path: '/api/collection', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_id: CW_CARD, quantity: 0 }),
      strip: ['$.id', '$.ids'], setup: addCtx, teardown: deleteProbes,
      readBack: addReadBack('floored row', (rows) =>
        rows.length === 1 && rows[0].quantity === 1) },
    // Unknown card: Node fetches Scryfall and 404s on the miss; Rust must
    // forward (both sides end on the identical Node-shaped 404).
    { name: 'ca_add_unknown_404', path: '/api/collection', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_id: 'mtg-00000000-0000-4000-8000-000000000000' }) },
    // condition is not validated up front (only printing is): the collection
    // CHECK rejects it inside the add, surfacing as the generic 500 shape.
    { name: 'ca_add_bad_condition_500', path: '/api/collection', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_id: CW_CARD, condition: 'Played' }) },

    { name: 'ca_bulk_add_missing_400', path: '/api/collection/bulk-add', headers: CW_JSON, method: 'POST', body: '{}' },
    { name: 'ca_bulk_add_empty_400', path: '/api/collection/bulk-add', headers: CW_JSON, method: 'POST',
      body: '{"card_ids":[]}' },
    { name: 'ca_bulk_add_badids_400', path: '/api/collection/bulk-add', headers: CW_JSON, method: 'POST',
      body: '{"card_ids":"x"}' },
    { name: 'ca_bulk_add_over_400', path: '/api/collection/bulk-add', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_ids: Array.from({ length: 251 }, () => CW_CARD) }) },
    { name: 'ca_bulk_add_success_200', path: '/api/collection/bulk-add', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_ids: [CW_CARD, CW_CARD], condition: 'Damaged', quantity: '2' }),
      strip: ['$.entries'], setup: addCtx, teardown: deleteProbes,
      readBack: addReadBack('bulk rows', (rows) =>
        rows.length === 4 && rows.every((r) => r.condition === 'Damaged' && r.quantity === 1)) },
    { name: 'ca_bulk_add_mixed_200', path: '/api/collection/bulk-add', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_ids: [CW_CARD, {}] }),
      strip: ['$.entries'], setup: addCtx, teardown: deleteProbes,
      readBack: addReadBack('bulk partial', (rows, resp) =>
        rows.length === 1 && resp.json.message === 'Added 1 of 2 cards; 1 failed.'
        && resp.json.failed[0].error === 'card_id is required' && resp.json.failed[0].index === 1) },
    { name: 'ca_bulk_add_all_fail_500', path: '/api/collection/bulk-add', headers: CW_JSON, method: 'POST',
      body: JSON.stringify({ card_ids: [{}, {}] }) },
  );

// --- stats-subs group cases (iter 25: GET /api/stats/history,
// /api/stats/networth, /api/stats/top-mover; RUST_ROUTES=stats_subs). Pure
// cached reads — no writes, no readBack needed. asOf is random-by-design on
// EVERY networth response (Node's cached() onHit restamps it per request,
// hit or miss — routes/stats.js onHit), so it is stripped from body equality
// like the auth tokens AND its per-request ETag can never replay to 304 on
// either side: those cases set etagReplay:false (shape pinned by
// ss_networth_asof_shape instead). history/top-mover keep ETag/304 replay
// through etagParity (/api/stats* prefix). ---
{
  const sub = (name, path, extra = {}) =>
    cases.push({ name, path, headers: authHeaders, method: 'GET', ...extra });
  sub('ss_history_30d', '/api/stats/history?period=30d');
  sub('ss_history_7d', '/api/stats/history?period=7d');
  sub('ss_history_1y', '/api/stats/history?period=1y');
  sub('ss_history_5y', '/api/stats/history?period=5y');
  sub('ss_history_default', '/api/stats/history');
  sub('ss_history_empty', '/api/stats/history?period=');
  sub('ss_history_unknown', '/api/stats/history?period=xyz');
  sub('ss_history_dup', '/api/stats/history?period=7d&period=1y');
  sub('ss_history_encoded', '/api/stats/history?period=7%64');
  sub('ss_networth', '/api/stats/networth', { strip: ['$.asOf'], etagReplay: false });
  sub('ss_topmover', '/api/stats/top-mover');
  // Second user: per-user bodies (and the empty-collection {card:null} /
  // zero-totals paths) — session seeded by scripts/seed-parity-user1-session.sh.
  const u1 = { Authorization: `Bearer ${USER1_TOKEN}` };
  sub('ss_history_7d_user1', '/api/stats/history?period=7d', { headers: u1 });
  sub('ss_networth_user1', '/api/stats/networth', { headers: u1, strip: ['$.asOf'], etagReplay: false });
  sub('ss_topmover_user1', '/api/stats/top-mover', { headers: u1 });
  // Error-path parity (orchestrator audit 2026-11-10 20:20, ruling 1):
  // networth/top-mover need no-token 401 + bad-token 401 byte parity —
  // same 401 bodies as the /api/stats surface (middleware/auth), asserted
  // cross-side. etagReplay:false: 401 has no cache semantics to replay.
  const noauth = { headers: {}, etagReplay: false };
  const badtok = { headers: { Authorization: `Bearer ${'Bad' + 'Token999'}` }, etagReplay: false };
  sub('ss_networth_noauth', '/api/stats/networth', noauth);
  sub('ss_networth_badtoken', '/api/stats/networth', badtok);
  sub('ss_topmover_noauth', '/api/stats/top-mover', noauth);
  sub('ss_topmover_badtoken', '/api/stats/top-mover', badtok);
  // asOf shape (fresh-stamp contract): each side's own asOf must be exact
  // ISO-8601 UTC with ms (new Date().toISOString() shape); the value itself
  // is random-by-design (restamped per request) and stripped from equality.
  cases.push({
    name: 'ss_networth_asof_shape', path: '/api/stats/networth', headers: authHeaders, method: 'GET',
    strip: ['$.asOf'], etagReplay: false,
    readBack: async (_other, _ctx, resp) => ({
      ok: /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(String(resp?.json?.asOf || '')),
      why: `asOf not toISOString-shaped: ${resp?.json?.asOf}`,
      seen: { shape: 'iso-ms' },
    }),
  });
  // HEAD on a sub-route: Express router.get answers HEAD; Rust proxies it.
  // Byte-comparison of the two answers is still the contract.
  cases.push({ name: 'ss_networth_head', path: '/api/stats/networth', headers: authHeaders, method: 'HEAD' });
  // POST is not routed in Express — 404 either side through the proxy.
  cases.push({ name: 'ss_history_post', path: '/api/stats/history', headers: authHeaders, method: 'POST', body: '{}' });
}

// --- iter 27: search-history + shared (RUST_ROUTES=search_history,shared) ---
// GET/POST/DELETE /api/search-history (per-user deflate-raw blob, session
// gate) and the PUBLIC GET /api/shared/:share_token. Writes ride the HTTP
// API channel only (rule 2); the per-user rows live and die with the
// rusttest_parity fixture user, and every write case read-back-verifies
// through the OPPOSITE server — which is also the cross-side codec test:
// Rust's deflateRaw(level 9) blob must be readable by Node's inflateRawSync
// and vice versa. Body-parser runs ABOVE the auth gate in server.js, so the
// malformed-JSON POST answers 500 even without a token (shx_post_badjson).
{
  const SH = { Authorization: `Bearer ${'rusttest' + '-sh'}` };
  const SH_JSON = { ...SH, 'content-type': 'application/json' };
  function shClear() {
    dbWrite((d) => {
      const u = d.prepare('SELECT id FROM users WHERE username = ?').get(PARITY_USER);
      if (u) d.prepare('DELETE FROM search_history WHERE user_id = ?').run(u.id);
    });
  }
  async function shSeedItems(base, items) {
    const r = await apiReq(base, 'POST', '/api/search-history', SH_JSON, JSON.stringify({ items }));
    if (r.status !== 200) throw new Error(`parity shx seed failed: ${r.status} ${r.text.slice(0, 120)}`);
  }
  const shPrep = async () => { resetParityUser(); seedSession('rusttest-sh'); shClear(); return { ctx: null }; };

  // --- error paths (no state touched) ---
  const noauth = { headers: {}, etagReplay: false };
  const badtok = { headers: { Authorization: `Bearer ${'Bad' + 'Token999'}` }, etagReplay: false };
  cases.push({ name: 'shx_get_noauth', path: '/api/search-history', ...noauth });
  cases.push({ name: 'shx_get_badtoken', path: '/api/search-history', ...badtok });
  cases.push({ name: 'shx_post_noauth', path: '/api/search-history', headers: { ...AUTH_JSON }, method: 'POST', body: '{"q":"x"}', etagReplay: false });
  // Malformed JSON parses (and fails) BEFORE the gate — 500 with the generic
  // error-handler body, not 401. Proves the parse/auth ORDER is ported too.
  cases.push({ name: 'shx_post_badjson_noauth', path: '/api/search-history', headers: { ...AUTH_JSON }, method: 'POST', body: '{oops', etagReplay: false });
  // express.json({limit:'1mb'}) sits ABOVE the auth gate: a >1mb body is a
  // 413 with the custom too-large message even with no token. Pins the
  // limit + message on both sides.
  cases.push({ name: 'shx_post_too_large_noauth', path: '/api/search-history', headers: { ...AUTH_JSON }, method: 'POST', body: '{"q":"' + 'x'.repeat(1.2 * 1024 * 1024) + '"}', etagReplay: false });

  // --- empty read (Express default ETag + 304 replay via etagParity) ---
  cases.push({ name: 'shx_get_empty', path: '/api/search-history', headers: SH, setup: shPrep });

  // --- deterministic import of several items: dedupe (max-at wins), trim,
  // empty-q filter, Number() coercion (string at), sort desc, key order. ---
  const SH_ITEMS = [
    { q: 'delta force', at: 1760000000000 },
    { q: '  spaced  ', at: 1760000001000 },
    { q: 'delta force', at: 1759999999000 },
    { q: '   ' },
    { q: 'emoji 🧪', at: '1760000002500' },
  ];
  const SH_EXPECT = [
    { q: 'emoji 🧪', at: 1760000002000 },
    { q: 'spaced', at: 1760000001000 },
    { q: 'delta force', at: 1760000000000 },
  ];
  cases.push({
    name: 'shx_post_items_ok', path: '/api/search-history', headers: SH_JSON, method: 'POST',
    body: JSON.stringify({ items: SH_ITEMS }), etagReplay: false,
    setup: shPrep, teardown: () => shClear(),
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/search-history', SH);
      const seen = JSON.stringify(g.json);
      return { ok: g.status === 200 && JSON.stringify(g.json) === JSON.stringify(SH_EXPECT), why: `opposite GET -> ${g.status} ${seen}`, seen };
    },
  });

  // --- single {q} add: at = now (random by design) — stripped; the readBack
  // pins shape + freshness AND confirms the Rust-written blob on Node. ---
  cases.push({
    name: 'shx_post_q_ok', path: '/api/search-history', headers: SH_JSON, method: 'POST',
    body: JSON.stringify({ q: '  parity solo q 🧪  ' }), strip: ['$.0.at'], etagReplay: false,
    setup: shPrep, teardown: () => shClear(),
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/search-history', SH);
      const e = Array.isArray(g.json) ? g.json[0] : null;
      const ok = g.status === 200 && Array.isArray(g.json) && g.json.length === 1
        && e?.q === 'parity solo q 🧪' && typeof e?.at === 'number'
        && Math.abs(e.at - Date.now()) < 5 * 60 * 1000;
      return { ok, why: `opposite GET -> ${g.status} ${JSON.stringify(g.json)}`, seen: { q: e?.q, atShape: typeof e?.at } };
    },
  });

  // --- 400 branches (empty add after cleaning) ---
  cases.push({ name: 'shx_post_empty', path: '/api/search-history', headers: SH, method: 'POST', setup: shPrep, teardown: () => shClear(), etagReplay: false });
  cases.push({ name: 'shx_post_items_all_blank', path: '/api/search-history', headers: SH_JSON, method: 'POST', body: JSON.stringify({ items: [{ q: '  ' }, {}] }), setup: shPrep, teardown: () => shClear(), etagReplay: false });
  cases.push({ name: 'shx_post_array_body', path: '/api/search-history', headers: SH_JSON, method: 'POST', body: '[]', setup: shPrep, teardown: () => shClear(), etagReplay: false });

  // --- deletes: seed 3 through the side's own API, then delete one / all. ---
  const shSeedThree = async (base) => {
    await shPrep();
    await shSeedItems(base, [
      { q: 'a one', at: 1760000000000 },
      { q: 'b two', at: 1760000001000 },
      { q: 'c three', at: 1760000002000 },
    ]);
    return { ctx: null };
  };
  cases.push({
    name: 'shx_delete_one', path: '/api/search-history', headers: SH_JSON, method: 'DELETE',
    body: JSON.stringify({ q: 'b two' }), etagReplay: false,
    setup: shSeedThree, teardown: () => shClear(),
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/search-history', SH);
      const seen = JSON.stringify(g.json);
      const ok = g.status === 200 && seen === JSON.stringify([
        { q: 'c three', at: 1760000002000 }, { q: 'a one', at: 1760000000000 },
      ]);
      return { ok, why: `opposite GET -> ${g.status} ${seen}`, seen };
    },
  });
  cases.push({
    name: 'shx_delete_clear', path: '/api/search-history', headers: SH_JSON, method: 'DELETE',
    body: '{}', etagReplay: false,
    setup: shSeedThree, teardown: () => shClear(),
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/search-history', SH);
      const ok = g.status === 200 && JSON.stringify(g.json) === '[]';
      return { ok, why: `opposite GET -> ${g.status} ${JSON.stringify(g.json)}`, seen: { cleared: ok } };
    },
  });
  // No body at all: express.json skips (no content-type) -> req.body = {} ->
  // q '' -> clear-everything. Ported decision, not assumed.
  cases.push({
    name: 'shx_delete_nobody', path: '/api/search-history', headers: SH, method: 'DELETE',
    etagReplay: false,
    setup: shSeedThree, teardown: () => shClear(),
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/search-history', SH);
      const ok = g.status === 200 && JSON.stringify(g.json) === '[]';
      return { ok, why: `opposite GET -> ${g.status} ${JSON.stringify(g.json)}`, seen: { cleared: ok } };
    },
  });

  // --- API key on a write: 403 from the gate (keys are GET-only). ---
  cases.push({
    name: 'shx_post_apikey_403', path: '/api/search-history',
    headers: { Authorization: '***' + 'x'.repeat(59), 'content-type': 'application/json' },
    method: 'POST', body: '{"q":"nope"}', etagReplay: false,
    setup: async () => { await shPrep(); dbWrite((d) => d.prepare('UPDATE users SET api_key = ? WHERE username = ?').run('x'.repeat(59), PARITY_USER)); return { ctx: null }; },
    teardown: () => dbWrite((d) => d.prepare('UPDATE users SET api_key = NULL WHERE username = ?').run(PARITY_USER)),
  });

  // ===== shared (public, no session) =====
  const SHARED_CARD2 = 'mtg-0001c639-8bd0-426f-89cb-4ca61f3cc054'; // Surge of Brilliance (fixture in card_cache)
  const BAD_CARD = 'rusttest-shared-bad-types';
  function shrdPrep({ trade = false, enable = true, rows = true, badRow = false } = {}) {
    dbWrite((d) => {
      d.prepare('DELETE FROM sessions WHERE user_id IN (SELECT id FROM users WHERE username = ?)').run(PARITY_USER);
      d.prepare('DELETE FROM users WHERE username = ?').run(PARITY_USER);
      const r = d.prepare('INSERT INTO users (username, password_hash, role, share_token, share_enabled, api_key) VALUES (?, ?, ?, ?, ?, NULL)')
        .run(PARITY_USER, 'x', 'member', SHARE_T0, enable ? 1 : 0);
      const uid = Number(r.lastInsertRowid);
      d.prepare('DELETE FROM collection WHERE user_id = ? OR id BETWEEN 930001 AND 930010').run(uid);
      d.prepare('DELETE FROM card_cache WHERE id = ?').run(BAD_CARD);
      if (!rows) return;
      // Explicit ids: etagParity re-runs setup per side; autoincrement ids
      // would shift entry_id between the two runs and make the route's body
      // ETag (and therefore the 304 replay) unreproducible by construction.
      const ins = d.prepare('INSERT INTO collection (id, card_id, quantity, condition, printing, language, added_at, is_trade, user_id, is_proxy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)');
      ins.run(930001, CW_CARD, 2, 'Near Mint', 'Holofoil', 'English', '2026-01-03 10:00:00', 0, uid, 0);
      ins.run(930002, SHARED_CARD2, 1, 'Lightly Played', 'Normal', 'English', '2026-01-02 10:00:00', trade ? 1 : 0, uid, 0);
      ins.run(930003, SHARED_CARD2, 3, 'Near Mint', 'Normal', 'Japanese', '2026-01-01 10:00:00', 0, uid, 1);
      if (badRow) {
        d.prepare('INSERT INTO card_cache (id, name, types, subtypes, rarity, price_trend, price_estimated) VALUES (?, ?, ?, ?, ?, ?, 0)')
          .run(BAD_CARD, 'Parity Bad Types', 'oops', '[]', 'Common', 1.5);
        ins.run(930004, BAD_CARD, 1, 'Near Mint', 'Normal', 'English', '2025-12-31 10:00:00', 0, uid, 0);
      }
    });
  }
  function shrdTeardown() {
    dbWrite((d) => {
      const u = d.prepare('SELECT id FROM users WHERE username = ?').get(PARITY_USER);
      if (u) d.prepare('DELETE FROM collection WHERE user_id = ?').run(u.id);
      d.prepare('DELETE FROM card_cache WHERE id = ?').run(BAD_CARD);
    });
  }
  // entry_id is autoincrement-random across the two side-runs — stripped per
  // index (row count is fixture-deterministic); the persisted join is the
  // same physical DB both sides read.
  cases.push({
    name: 'shrd_ok', path: `/api/shared/${SHARE_T0}`, headers: {},
    // No teardown: etagParity replays AFTER both side fetches, and
    // shrdTeardown's row-deletion would flip the replay GET to an empty
    // collection (200, not 304). Rows are rebuilt deterministically
    // (fixed ids) by every setup, and cleared by later cases.
    setup: () => { shrdPrep({}); },
  });
  cases.push({
    name: 'shrd_trade', path: `/api/shared/${SHARE_T0}?list=trade`, headers: {},
    setup: () => { shrdPrep({ trade: true }); },
  });
  // Repeated list= becomes an ARRAY in qs (not === 'trade') -> collection
  // branch; the trade row must appear again. Pins the qs semantics.
  cases.push({
    name: 'shrd_dup_list_param', path: `/api/shared/${SHARE_T0}?list=trade&list=trade`, headers: {},
    setup: () => { shrdPrep({}); },
  });
  // qs ARRAY semantics: FIRST value 'trade' but a duplicate param -> value
  // is an array !== 'trade' -> collection branch (trade row visible again).
  cases.push({
    name: 'shrd_list_conflict', path: `/api/shared/${SHARE_T0}?list=trade&list=foo`, headers: {},
    setup: () => { shrdPrep({ trade: true }); },
  });
  // Malformed % in a query VALUE: qs 6.15.3 does NOT throw (verified against
  // the container's own qs: qs.parse('x=%ZZ') -> {x:'%ZZ'}), so the raw
  // value simply fails the === 'trade' equality and the collection branch
  // answers normally. Pinned here with a valid token + seeded rows; if
  // either server started throwing (Express 5 qs behaviour) this diverges.
  cases.push({
    name: 'shrd_bad_query_percent', path: `/api/shared/${SHARE_T0}?x=%ZZ`, headers: {}, etagReplay: false,
    strip: ['$.collection.0.entry_id', '$.collection.1.entry_id', '$.collection.2.entry_id'],
    setup: () => { shrdPrep({}); }, teardown: shrdTeardown,
  });
  cases.push({
    name: 'shrd_empty', path: `/api/shared/${SHARE_T0}`, headers: {},
    setup: () => { shrdPrep({ rows: false }); }, teardown: shrdTeardown,
  });
  cases.push({ name: 'shrd_notfound', path: '/api/shared/definitely-not-a-token', headers: {}, etagReplay: false });
  cases.push({
    name: 'shrd_disabled', path: `/api/shared/${SHARE_T0}`, headers: {}, etagReplay: false,
    setup: () => { shrdPrep({ enable: false }); }, teardown: shrdTeardown,
  });
  // Percent-decoded param: %2F stays one raw segment -> 'abc/def' -> 404;
  // invalid escape -> decodeURIComponent throws in routing -> generic 500.
  cases.push({ name: 'shrd_percent_token', path: '/api/shared/abc%2Fdef', headers: {}, etagReplay: false });
  cases.push({ name: 'shrd_bad_percent', path: '/api/shared/a%ZZb', headers: {}, etagReplay: false });
  // Bad array column -> JSON.parse throws inside the try -> 500 body.
  cases.push({
    name: 'shrd_bad_types_500', path: `/api/shared/${SHARE_T0}`, headers: {}, etagReplay: false,
    setup: () => { shrdPrep({ badRow: true }); }, teardown: shrdTeardown,
  });
  // HEAD: Rust proxies (port owns GET only); Express answers HEAD natively.
  cases.push({ name: 'shrd_head', path: `/api/shared/${SHARE_T0}`, headers: {}, method: 'HEAD', setup: () => { shrdPrep({}); }, teardown: shrdTeardown });

  // ===== decks_writes (iter 28): PUT /api/decks/:id, DELETE /api/decks/:id,
  // DELETE /api/decks/:id/cards/:card_id. Fixed ids (like shrdPrep) so both
  // side-runs see identical starting rows; teardown clears them. =====
  const DK_DUP = 'rusttest-dk-dup-emh'; // second printing of Emergency Medical Hologram
  function dkUid(d) {
    const t = String(authHeaders.Authorization || '').replace(/^Bearer\s+/i, '');
    const s = d.prepare('SELECT user_id FROM sessions WHERE token = ?').get(t);
    if (!s) throw new Error('parity: bench session not found for deck cases');
    return Number(s.user_id);
  }
  function dkClear(d) {
    d.prepare('DELETE FROM deck_cards WHERE deck_id BETWEEN 931001 AND 931004').run();
    d.prepare('DELETE FROM deck_maybe_cards WHERE deck_id BETWEEN 931001 AND 931004').run();
    d.prepare('DELETE FROM decks WHERE id BETWEEN 931001 AND 931004').run();
    d.prepare('DELETE FROM card_cache WHERE id = ?').run(DK_DUP);
  }
  function dkPrep() {
    dbWrite((d) => {
      dkClear(d);
      const uid = dkUid(d);
      const other = d.prepare('SELECT id FROM users WHERE id != ? ORDER BY id LIMIT 1').get(uid);
      if (!other) throw new Error('parity: no second user for foreign-deck case');
      const ins = d.prepare('INSERT INTO decks (id,user_id,name,description,checked_out,created_at) VALUES (?,?,?,?,?,?)');
      ins.run(931001, uid, 'DK Seed One', 'seed one', 0, '2026-01-01 00:00:00');
      ins.run(931002, uid, 'DK Checked Out', 'seed co', 1, '2026-01-01 00:00:00');
      ins.run(931003, other.id, 'DK Foreign', 'fd', 0, '2026-01-01 00:00:00');
      ins.run(931004, uid, 'DK Empty', '', 0, '2026-01-01 00:00:00');
      d.prepare('INSERT INTO card_cache (id,name,types,subtypes,rarity,price_trend,price_normal,price_holofoil,price_estimated) VALUES (?,?,?,?,?,?,0,0,0)')
        .run(DK_DUP, 'Emergency Medical Hologram', '["Artifact"]', '[]', 'Rare', 0.2);
      const dc = d.prepare('INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES (?,?,?)');
      dc.run(931001, CW_CARD, 2);
      dc.run(931001, DK_DUP, 1);
      dc.run(931002, CW_CARD, 1);
      d.prepare('INSERT INTO deck_maybe_cards (deck_id,card_id,quantity) VALUES (?,?,?)').run(931001, CW_CARD, 1);
    });
  }
  function dkTeardown() { dbWrite((d) => dkClear(d)); }
  const dkDbCounts = () => dbWrite((d) => ({
    dc: d.prepare('SELECT COUNT(*) n FROM deck_cards WHERE deck_id=931001').get().n,
    dm: d.prepare('SELECT COUNT(*) n FROM deck_maybe_cards WHERE deck_id=931001').get().n,
    co: d.prepare('SELECT COUNT(*) n FROM decks WHERE id=931002').get().n,
  }));
  // NOTE: all dk cases set etagReplay:false — these are write routes; the
  // 304 replay of a *read* ETag is pinned by the shared/shrd cases instead.
  const DK_JSON = { ...authHeaders, 'content-type': 'application/json' };
  cases.push({
    name: 'dk_put_200', path: '/api/decks/931001', headers: DK_JSON, method: 'PUT',
    body: '{"name":"DK Renamed","description":"parity desc"}', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/decks/931001', authHeaders);
      const seen = { name: g.json?.name, description: g.json?.description };
      return { ok: g.status === 200 && seen.name === 'DK Renamed' && seen.description === 'parity desc', why: `PUT persisted -> ${JSON.stringify(seen)}`, seen };
    },
  });
  cases.push({
    name: 'dk_put_missing_404', path: '/api/decks/999999', headers: DK_JSON, method: 'PUT',
    body: '{"name":"DK Renamed"}', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
  });
  cases.push({
    name: 'dk_put_foreign_404', path: '/api/decks/931003', headers: DK_JSON, method: 'PUT',
    body: '{"name":"Hijacked"}', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/decks/931003', { ...authHeaders, Authorization: '***' });
      // The foreign deck belongs to another user — even that user's session
      // is not available here; assert it still carries its original name
      // through the shared DB (hijack did not persist).
      const d0 = dbWrite((d) => d.prepare('SELECT name FROM decks WHERE id=931003').get());
      return { ok: !!d0 && d0.name === 'DK Foreign', why: `foreign deck untouched -> ${JSON.stringify(d0)}`, seen: d0 };
    },
  });
  cases.push({
    name: 'dk_put_no_name_400', path: '/api/decks/931001', headers: DK_JSON, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
  });
  cases.push({
    name: 'dk_put_noauth_401', path: '/api/decks/931001', headers: { 'content-type': 'application/json' }, method: 'PUT',
    body: '{"name":"Nope"}', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async () => {
      const c = dkDbCounts();
      const d0 = dbWrite((d) => d.prepare('SELECT name FROM decks WHERE id=931001').get());
      return { ok: !!d0 && d0.name === 'DK Seed One', why: `noauth left row -> ${JSON.stringify(d0)} ${JSON.stringify(c)}`, seen: d0 };
    },
  });
  cases.push({
    name: 'dk_put_badjson_500', path: '/api/decks/931001', headers: DK_JSON, method: 'PUT',
    body: '{oops', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
  });
  cases.push({
    name: 'dk_put_apikey_403', path: '/api/decks/931001', method: 'PUT', etagReplay: false,
    headers: { Authorization: '***' + 'x'.repeat(59), 'content-type': 'application/json' },
    body: '{"name":"Nope"}',
    setup: async () => { dbWrite((d) => d.prepare('UPDATE users SET api_key = ? WHERE username = ?').run('x'.repeat(59), PARITY_USER)); dkPrep(); },
    teardown: () => { dbWrite((d) => d.prepare('UPDATE users SET api_key = NULL WHERE username = ?').run(PARITY_USER)); dkTeardown(); },
  });
  cases.push({
    name: 'dk_delete_200', path: '/api/decks/931001', headers: authHeaders, method: 'DELETE', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/decks/931001', authHeaders);
      const c = dkDbCounts();
      return { ok: g.status === 404 && c.dc === 0 && c.dm === 0, why: `delete persisted -> HTTP ${g.status}, deck_cards=${c.dc}, maybe=${c.dm}`, seen: { status: g.status, ...c } };
    },
  });
  cases.push({
    name: 'dk_delete_checked_out_409', path: '/api/decks/931002', headers: authHeaders, method: 'DELETE', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async () => {
      const c = dkDbCounts();
      const co = dbWrite((d) => d.prepare('SELECT name FROM decks WHERE id=931002').get());
      return { ok: !!co && co.name === 'DK Checked Out', why: `checked-out deck survived -> ${JSON.stringify(co)} ${JSON.stringify(c)}`, seen: co };
    },
  });
  cases.push({
    name: 'dk_delete_missing_404', path: '/api/decks/999999', headers: authHeaders, method: 'DELETE', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
  });
  cases.push({
    name: 'dk_delete_noauth_401', path: '/api/decks/931001', headers: {}, method: 'DELETE', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async () => {
      const d0 = dbWrite((d) => d.prepare('SELECT name FROM decks WHERE id=931001').get());
      return { ok: !!d0 && d0.name === 'DK Seed One', why: `noauth left deck -> ${JSON.stringify(d0)}`, seen: d0 };
    },
  });
  cases.push({
    name: 'dk_deletecard_200', path: `/api/decks/931001/cards/${CW_CARD}`, headers: authHeaders, method: 'DELETE', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async (other) => {
      const g = await apiReq(other, 'GET', '/api/decks/931001', authHeaders);
      const cards = Array.isArray(g.json?.cards) ? g.json.cards.map((c) => c.name).sort() : null;
      const maybe = Array.isArray(g.json?.maybeboard) ? g.json.maybeboard.length : null;
      // Both printings of the logical card (CW_CARD + DK_DUP share the
      // trimmed-lower name) must be gone; the maybeboard row stays.
      const ok = g.status === 200 && cards && !cards.includes('Emergency Medical Hologram') && maybe === 1;
      return { ok, why: `logical-card delete -> HTTP ${g.status}, cards=${JSON.stringify(cards)}, maybe=${maybe}`, seen: { cards, maybe } };
    },
  });
  cases.push({
    name: 'dk_deletecard_deck_missing_404', path: `/api/decks/999999/cards/${CW_CARD}`, headers: authHeaders, method: 'DELETE', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
  });
  cases.push({
    name: 'dk_deletecard_checked_out_409', path: `/api/decks/931002/cards/${CW_CARD}`, headers: authHeaders, method: 'DELETE', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async () => {
      const n = dbWrite((d) => d.prepare('SELECT COUNT(*) n FROM deck_cards WHERE deck_id=931002').get().n);
      return { ok: n === 1, why: `checked-out deck kept its card (${n})`, seen: { n } };
    },
  });
  cases.push({
    name: 'dk_deletecard_noauth_401', path: `/api/decks/931001/cards/${CW_CARD}`, headers: {}, method: 'DELETE', etagReplay: false,
    setup: () => { dkPrep(); }, teardown: dkTeardown,
    readBack: async () => {
      const n = dbWrite((d) => d.prepare('SELECT COUNT(*) n FROM deck_cards WHERE deck_id=931001').get().n);
      return { ok: n === 2, why: `noauth kept both deck_cards rows (${n})`, seen: { n } };
    },
  });

  // ===== decks_writes iter 30: POST /api/decks, POST /api/decks/:id/cards,
  // POST /api/decks/:id/register-collection, PUT /api/decks/:id/checkout,
  // PUT /api/decks/:id/return. Own id range (931011..931018) and two
  // private card_cache rows (rusttest-dk2-*) so fixture churn cannot leak
  // into the iter-28/29 cases or the stats/networth bodies. 931012 (the
  // checked-out deck) carries CW_CARD, NOT the dk2 cards: the checkout
  // locked/supply CTEs scan the user's checked-out decks, and CW_CARD's
  // logical name must not collide with the checkout cases' expectations.
  // Setup runs per side, so both side-runs see identical rows. =====
  const DK2_A = 'rusttest-dk2-alpha';
  const DK2_B = 'rusttest-dk2-beta';
  const DK2_NAME_A = 'rusttest DK2 Alpha';
  const DK2_NAME_B = 'rusttest DK2 Beta';
  function dk2Clear(d) {
    d.prepare('DELETE FROM deck_cards WHERE deck_id BETWEEN 931011 AND 931018').run();
    d.prepare('DELETE FROM deck_maybe_cards WHERE deck_id BETWEEN 931011 AND 931018').run();
    d.prepare('DELETE FROM decks WHERE id BETWEEN 931011 AND 931018').run();
    d.prepare("DELETE FROM decks WHERE name IN ('DK2 Fresh','DK2 Import')").run();
    d.prepare('DELETE FROM collection WHERE card_id IN (?, ?)').run(DK2_A, DK2_B);
    d.prepare('DELETE FROM price_history WHERE card_id IN (?, ?)').run(DK2_A, DK2_B);
    d.prepare('DELETE FROM card_cache WHERE id IN (?, ?)').run(DK2_A, DK2_B);
  }
  function dk2Prep() {
    dbWrite((d) => {
      dk2Clear(d);
      const uid = dkUid(d);
      const other = d.prepare('SELECT id FROM users WHERE id != ? ORDER BY id LIMIT 1').get(uid);
      d.prepare('INSERT INTO card_cache (id,name,types,subtypes,rarity,price_trend,price_normal,price_holofoil,price_estimated) VALUES (?,?,?,?,?,?,0,0,0)')
        .run(DK2_A, DK2_NAME_A, '["Artifact"]', '[]', 'Rare', 0.1);
      d.prepare('INSERT INTO card_cache (id,name,types,subtypes,rarity,price_trend,price_normal,price_holofoil,price_estimated) VALUES (?,?,?,?,?,?,0,0,0)')
        .run(DK2_B, DK2_NAME_B, '["Creature"]', '[]', 'Common', 0.2);
      const ins = d.prepare('INSERT INTO decks (id,user_id,name,description,checked_out,created_at) VALUES (?,?,?,?,?,?)');
      ins.run(931011, uid, 'DK2 Add', '', 0, '2026-01-01 00:00:00');
      ins.run(931012, uid, 'DK2 Checked Out', '', 1, '2026-01-01 00:00:00');
      ins.run(931013, other.id, 'DK2 Foreign', '', 0, '2026-01-01 00:00:00');
      ins.run(931014, uid, 'DK2 Empty', '', 0, '2026-01-01 00:00:00');
      ins.run(931015, uid, 'DK2 Broken', '', 0, '2026-01-01 00:00:00');
      ins.run(931016, uid, 'DK2 Reg', '', 0, '2026-01-01 00:00:00');
      ins.run(931017, uid, 'DK2 Out', '', 0, '2026-01-01 00:00:00');
      ins.run(931018, uid, 'DK2 Short', '', 0, '2026-01-01 00:00:00');
      const dc = d.prepare('INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES (?,?,?)');
      dc.run(931012, CW_CARD, 1);            // checked-out deck: CW_CARD only (see note)
      dc.run(931015, 'rusttest-dk2-unresolved', 1);
      dc.run(931016, DK2_A, 2);
      dc.run(931016, DK2_B, 1);
      dc.run(931017, DK2_A, 2);
      dc.run(931017, DK2_B, 1);
      dc.run(931018, DK2_B, 2);
      d.prepare('UPDATE decks SET checked_out = 1, checked_out_at = CURRENT_TIMESTAMP WHERE id = 931012').run();
    });
  }
  function dk2FundA(qty) {
    dbWrite((d) => {
      const uid = dkUid(d);
      d.prepare('INSERT INTO collection (card_id,quantity,user_id) VALUES (?,?,?)').run(DK2_A, qty, uid);
    });
  }
  function dk2Teardown() { dbWrite((d) => dk2Clear(d)); }
  const dk2Db = (sql, ...args) => dbWrite((d) => d.prepare(sql).all(...args));
  cases.push({
    name: 'dk2_create_201', path: '/api/decks', headers: DK_JSON, method: 'POST',
    body: '{"name":"DK2 Fresh"}', etagReplay: false, strip: ['id'],
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async (other) => {
      // id stripped (autoincrement differs by side); row persisted with Node defaults.
      const rows = dk2Db("SELECT name, description, format, category, accent_color, target_size FROM decks WHERE name='DK2 Fresh'");
      const want = { name: 'DK2 Fresh', description: '', format: 'Standard', category: 'Competitive', accent_color: '#eab308', target_size: 60 };
      return { ok: rows.length === 1 && JSON.stringify(rows[0]) === JSON.stringify(want), why: `create row -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    // Decklist import twin of /^(\d+)x?\s+(.+)$/i — uppercase 'X' line must
    // ALSO match (/i flag); junk line silently skipped; upsert stacks to 5.
    name: 'dk2_create_decklist_201', path: '/api/decks', headers: DK_JSON, method: 'POST',
    body: '{"name":"DK2 Import","decklist_text":"4x rusttest DK2 Alpha\\nnot a line\\n1X rusttest DK2 Alpha\\n"}',
    etagReplay: false, strip: ['id'],
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db(
        "SELECT dc.card_id, dc.quantity FROM deck_cards dc JOIN decks dd ON dd.id = dc.deck_id WHERE dd.name='DK2 Import' ORDER BY dc.card_id",
      );
      return { ok: rows.length === 1 && rows[0].card_id === DK2_A && rows[0].quantity === 5, why: `import rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_create_no_name_400', path: '/api/decks', headers: DK_JSON, method: 'POST',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db("SELECT COUNT(*) n FROM decks WHERE name LIKE 'DK2 Fresh%'");
      return { ok: rows[0].n === 0, why: `400 created no row (${rows[0].n})`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_create_noauth_401', path: '/api/decks', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: '{"name":"DK2 Nope"}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db("SELECT COUNT(*) n FROM decks WHERE name='DK2 Nope'");
      return { ok: rows[0].n === 0, why: `noauth created no row (${rows[0].n})`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_addcard_200', path: '/api/decks/931011/cards', headers: DK_JSON, method: 'POST',
    body: `{"card_id":"${DK2_A}","quantity":"2"}`, etagReplay: false,
    setup: () => { dk2Prep(); dk2FundA(5); }, teardown: dk2Teardown,
    readBack: async () => {
      const cards = dk2Db('SELECT card_id, quantity FROM deck_cards WHERE deck_id=931011 ORDER BY card_id');
      const ph = dk2Db('SELECT COUNT(*) n FROM price_history WHERE card_id=?', DK2_A);
      return { ok: cards.length === 1 && cards[0].card_id === DK2_A && cards[0].quantity === 2 && ph[0].n === 1, why: `added -> ${JSON.stringify(cards)} ph=${ph[0].n}`, seen: { cards, ph } };
    },
  });
  cases.push({
    name: 'dk2_addcard_no_card_400', path: '/api/decks/931011/cards', headers: DK_JSON, method: 'POST',
    body: '{"quantity":1}', etagReplay: false,
    setup: () => { dk2Prep(); dk2FundA(5); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_addcard_foreign_404', path: '/api/decks/931013/cards', headers: DK_JSON, method: 'POST',
    body: `{"card_id":"${DK2_A}","quantity":1}`, etagReplay: false,
    setup: () => { dk2Prep(); dk2FundA(5); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_addcard_checkedout_409', path: '/api/decks/931012/cards', headers: DK_JSON, method: 'POST',
    body: `{"card_id":"${DK2_A}","quantity":1}`, etagReplay: false,
    setup: () => { dk2Prep(); dk2FundA(5); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_addcard_over_owned_400', path: '/api/decks/931011/cards', headers: DK_JSON, method: 'POST',
    body: `{"card_id":"${DK2_A}","quantity":6}`, etagReplay: false,
    setup: () => { dk2Prep(); dk2FundA(5); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_addcard_noauth_401', path: '/api/decks/931011/cards', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: `{"card_id":"${DK2_A}","quantity":1}`, etagReplay: false,
    setup: () => { dk2Prep(); dk2FundA(5); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db('SELECT COUNT(*) n FROM deck_cards WHERE deck_id=931011');
      return { ok: rows[0].n === 0, why: `noauth added nothing (${rows[0].n})`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_register_201', path: '/api/decks/931016/register-collection', headers: DK_JSON, method: 'POST',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db(
        'SELECT card_id, quantity, condition, language, is_trade FROM collection WHERE card_id IN (?, ?) ORDER BY card_id',
        DK2_A, DK2_B,
      );
      const want = [
        { card_id: DK2_A, quantity: 2, condition: 'Near Mint', language: 'English', is_trade: 0 },
        { card_id: DK2_B, quantity: 1, condition: 'Near Mint', language: 'English', is_trade: 0 },
      ];
      return { ok: JSON.stringify(rows) === JSON.stringify(want), why: `registered rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_register_empty_400', path: '/api/decks/931014/register-collection', headers: DK_JSON, method: 'POST',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_register_unresolved_422', path: '/api/decks/931015/register-collection', headers: DK_JSON, method: 'POST',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_register_checkedout_409', path: '/api/decks/931012/register-collection', headers: DK_JSON, method: 'POST',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
  });
  cases.push({
    // express.json runs before auth on this route: the body is parsed (and
    // 500s on bad JSON) even though the handler ignores it.
    name: 'dk2_register_badjson_500', path: '/api/decks/931016/register-collection', headers: DK_JSON, method: 'POST',
    body: '{oops', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_register_noauth_401', path: '/api/decks/931016/register-collection', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db('SELECT COUNT(*) n FROM collection WHERE card_id IN (?, ?)', DK2_A, DK2_B);
      return { ok: rows[0].n === 0, why: `noauth registered nothing (${rows[0].n})`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_checkout_200', path: '/api/decks/931017/checkout', headers: DK_JSON, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); dk2FundA(2); dbWrite((d) => { d.prepare('INSERT INTO collection (card_id,quantity,user_id) VALUES (?,?,?)').run(DK2_B, 1, dkUid(d)); }); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db('SELECT checked_out co, (checked_out_at IS NOT NULL) at_set FROM decks WHERE id=931017');
      return { ok: rows.length === 1 && rows[0].co === 1 && rows[0].at_set === 1, why: `checkout state -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    // Shortfall ladder: DK2_B needed 2, owned 0, in-use 0 -> deterministic
    // details array (unclamped available, raw row values in the template).
    name: 'dk2_checkout_shortage_400', path: '/api/decks/931018/checkout', headers: DK_JSON, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db('SELECT checked_out co FROM decks WHERE id=931018');
      return { ok: rows[0].co === 0, why: `shortfall did not check out (${rows[0].co})`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_checkout_already_409', path: '/api/decks/931012/checkout', headers: DK_JSON, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_checkout_missing_404', path: '/api/decks/999999/checkout', headers: DK_JSON, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_checkout_badjson_500', path: '/api/decks/931017/checkout', headers: DK_JSON, method: 'PUT',
    body: '{oops', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_checkout_noauth_401', path: '/api/decks/931017/checkout', headers: { 'content-type': 'application/json' }, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db('SELECT checked_out co FROM decks WHERE id=931017');
      return { ok: rows[0].co === 0, why: `noauth left deck in (${rows[0].co})`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_return_200', path: '/api/decks/931012/return', headers: DK_JSON, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db('SELECT checked_out co, checked_out_at at FROM decks WHERE id=931012');
      return { ok: rows[0].co === 0 && rows[0].at === null, why: `return cleared flags -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'dk2_return_missing_404', path: '/api/decks/999999/return', headers: DK_JSON, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
  });
  cases.push({
    name: 'dk2_return_noauth_401', path: '/api/decks/931012/return', headers: { 'content-type': 'application/json' }, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: () => { dk2Prep(); }, teardown: dk2Teardown,
    readBack: async () => {
      const rows = dk2Db('SELECT checked_out co FROM decks WHERE id=931012');
      return { ok: rows[0].co === 1, why: `noauth kept deck checked out (${rows[0].co})`, seen: rows };
    },
  });

  // ===== decks_reads (iter 29): GET /api/decks, GET /api/decks/:id,
  // GET /api/decks/:id/locations. Fixed ids, realistic scryfall-local row
  // shape (supertype 'MTG', types = colour words, subtypes = TYPE words
  // split — deckCommander's quoted-word candidate match and sqlIsBasicLand
  // both key off that tokenisation; live-pinned against a throwaway
  // scrybox:3e91010-headbtn container on this same fixture). The cross-side
  // deepDiff (values + array order + key order) is the contract; drPrep's
  // writes bump deck_list_revision, so neither side can leak a stale
  // per-process deck-list cache body. =====
  const DR = {
    etali: 'rusttest-dr-etali', etaliB: 'rusttest-dr-etali-b',
    vial: 'rusttest-dr-vial', vialB: 'rusttest-dr-vial-b',
    plains: 'rusttest-dr-plains', eur: 'rusttest-dr-eur',
  };
  function drClear(d) {
    d.prepare('DELETE FROM deck_cards WHERE deck_id BETWEEN 931101 AND 931106').run();
    d.prepare('DELETE FROM deck_maybe_cards WHERE deck_id BETWEEN 931101 AND 931106').run();
    d.prepare('DELETE FROM decks WHERE id BETWEEN 931101 AND 931106').run();
    d.prepare('DELETE FROM collection WHERE card_id LIKE ?').run('rusttest-dr-%');
    d.prepare('DELETE FROM card_cache WHERE id LIKE ?').run('rusttest-dr-%');
  }
  function drPrep() {
    dbWrite((d) => {
      drClear(d);
      const uid = dkUid(d);
      const other = d.prepare('SELECT id FROM users WHERE id != ? ORDER BY id LIMIT 1').get(uid);
      if (!other) throw new Error('parity: no second user for dr cases');
      const card = d.prepare('INSERT INTO card_cache (id,name,supertype,types,subtypes,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)');
      card.run(DR.etali, 'RustTest DR Etali', 'MTG', '["Red","Green"]', '["Legendary","Creature","Dragon"]', 'Rare', 'drs', 'DR Set', '1', 'img/dr-etali-a', 1, 1, 0, 0, 0, 6, '["R","G"]', 'USD');
      card.run(DR.etaliB, 'RustTest DR Etali ', 'MTG', '["Red","Green"]', '["Legendary","Creature","Dragon"]', 'Rare', 'drsb', 'DR Set B', '2', 'img/dr-etali-b', 0.5, 0.5, 0, 0, 0, 6, '["R","G"]', 'USD');
      card.run(DR.vial, 'RustTest DR Vial', 'MTG', '[]', '["Artifact"]', 'Uncommon', 'drs', 'DR Set', '3', 'img/dr-vial-a', 0.1, 0.1, 0, 0, 0, 1, '[]', 'USD');
      card.run(DR.vialB, 'RustTest DR Vial ', 'MTG', '[]', '["Artifact"]', 'Uncommon', 'drsb', 'DR Set B', '4', 'img/dr-vial-b', 0.05, 0.05, 0, 0, 0, 1, '[]', 'USD');
      card.run(DR.plains, 'RustTest DR Plains', 'MTG', '[]', '["Basic","Land"]', 'Common', 'drs', 'DR Set', '261', null, 0.01, 0.01, 0, 0, 0, 0, '[]', 'USD');
      card.run(DR.eur, 'RustTest DR Eur', 'MTG', '[]', '["Artifact"]', 'Common', 'drs', 'DR Set', '9', null, 2, 2, 0, 0, 0, 1, '[]', 'EUR');
      const deck = d.prepare('INSERT INTO decks (id,user_id,name,description,checked_out,created_at) VALUES (?,?,?,?,?,?)');
      deck.run(931101, uid, 'DR Commander', 'dc', 0, '2026-01-01 00:00:05');
      deck.run(931102, uid, 'DR Coverage', 'cv', 0, '2026-01-01 00:00:04');
      deck.run(931103, uid, 'DR Locked', 'lk', 1, '2026-01-01 00:00:03');
      deck.run(931104, uid, 'DR Broken', 'bd', 0, '2026-01-01 00:00:02');
      deck.run(931105, other.id, 'DR Foreign', 'fd', 0, '2026-01-01 00:00:01');
      deck.run(931106, other.id, 'DR Their Locked', 'tl', 1, '2026-01-01 00:00:00');
      const dc = d.prepare('INSERT INTO deck_cards (deck_id,card_id,quantity,is_commander) VALUES (?,?,?,?)');
      dc.run(931101, DR.etali, 1, 0);
      dc.run(931101, DR.vial, 4, 0);
      dc.run(931102, DR.plains, 3, 0);
      dc.run(931102, DR.vial, 1, 0);
      dc.run(931102, DR.eur, 1, 0);
      dc.run(931103, DR.plains, 3, 0);
      dc.run(931104, 'rusttest-dr-nope', 1, 0);
      dc.run(931106, DR.plains, 9, 0);
      d.prepare('INSERT INTO deck_maybe_cards (deck_id,card_id,quantity) VALUES (?,?,?)').run(931101, DR.etaliB, 2);
      const col = d.prepare('INSERT INTO collection (card_id,quantity,user_id) VALUES (?,?,?)');
      col.run(DR.vialB, 2, uid);
      col.run(DR.plains, 2, uid);
    });
  }
  function drTeardown() { dbWrite((d) => drClear(d)); }
  const drCase = (name, path, headers = authHeaders, method = 'GET') =>
    cases.push({ name, path, headers, method, setup: drPrep, teardown: drTeardown });
  // The list body carries total/ownership/commander/minimum-value per deck,
  // the coverage rows carry locked-exclusion (931103 locks user's own
  // copies; 931106 is another user's locked deck and must NOT), the basic-
  // land flag, and the missing-DESC sort. EUR card exercises the unpriced
  // gate; 931104 exercises the unresolved/422 paths.
  drCase('dr_list_200', '/api/decks');
  drCase('dr_list_noauth_401', '/api/decks', {});
  drCase('dr_detail_200', '/api/decks/931101');
  drCase('dr_detail_locked_200', '/api/decks/931103');
  drCase('dr_detail_foreign_404', '/api/decks/931105');
  drCase('dr_detail_broken_422', '/api/decks/931104');
  drCase('dr_detail_noauth_401', '/api/decks/931101', {});
  drCase('dr_locations_200', '/api/decks/931102/locations');
  drCase('dr_locations_locked_200', '/api/decks/931103/locations');
  drCase('dr_locations_foreign_404', '/api/decks/931105/locations');
  drCase('dr_locations_broken_422', '/api/decks/931104/locations');
  drCase('dr_locations_noauth_401', '/api/decks/931102/locations', {});
  // Lane-A iter-5 decks_combos (Spellbook combos + EDHREC synergy). The
  // provider legs are EXCLUDED per the provider-leg ruling (Spellbook and
  // EDHREC URLs are Node module constants, no env override; the harness
  // never recreates the Node container, so a stub-aim of both servers is
  // impossible). Every arm that answers BEFORE a provider fetch is pinned
  // here: auth, parse-before-auth, 404s, the empty-rows and no-commander
  // 200 shapes (931104's card id misses the card_cache JOIN -> the combos
  // early return; 931102 has no is_commander rows -> the synergy null).
  // The provider arms themselves are stub-proven in
  // crates/scrybox-server/tests/decks_combos.rs via AppState.deck_http.
  cases.push({ name: 'dc_combos_noauth_401', path: '/api/decks/931101/combos', headers: {}, method: 'GET', setup: drPrep, teardown: drTeardown });
  // (No badjson-preauth case here: undici forbids GET bodies — the
  // express.json-before-auth 500 arm is pinned by tests/decks_combos.rs::
  // auth_gate_and_parse_before_auth on the Rust side and the identical
  // Node source order was live-pinned in iter-28.)
  cases.push({ name: 'dc_combos_foreign_404', path: '/api/decks/931105/combos', headers: authHeaders, method: 'GET', setup: drPrep, teardown: drTeardown });
  cases.push({ name: 'dc_combos_missing_404', path: '/api/decks/999999/combos', headers: authHeaders, method: 'GET', setup: drPrep, teardown: drTeardown });
  cases.push({ name: 'dc_combos_empty_rows_200', path: '/api/decks/931104/combos', headers: authHeaders, method: 'GET', setup: drPrep, teardown: drTeardown });
  cases.push({ name: 'dc_synergy_noauth_401', path: '/api/decks/931101/synergy', headers: {}, method: 'GET', setup: drPrep, teardown: drTeardown });
  cases.push({ name: 'dc_synergy_foreign_404', path: '/api/decks/931105/synergy', headers: authHeaders, method: 'GET', setup: drPrep, teardown: drTeardown });
  cases.push({ name: 'dc_synergy_no_commander_200', path: '/api/decks/931102/synergy', headers: authHeaders, method: 'GET', setup: drPrep, teardown: drTeardown });
  cases.push({ name: 'dc_synergy_empty_deck_200', path: '/api/decks/931104/synergy', headers: authHeaders, method: 'GET', setup: drPrep, teardown: drTeardown });
}

// --- iter 31: card-list READS (Lane A port: GET /api/lists,
// /api/lists/card-matches, /api/lists/:id, /api/lists/:id/cardlist;
// RUST_ROUTES=lists_reads). Own id range 931201..931204, own card rows
// rusttest-lr-* (drClear/cwClear ranges never touch them). The collection
// body pins totals + minimum-value fold; card-matches pins the {} short-q
// gate, LIKE escaping and numeric-ascending key order; detail pins the
// logical-card fold (MIN(card_id) rep), owned_qty, current-vs-minimum
// totals, parseCardRow columns; cardlist pins plain/detailed/missing=1 text
// and the text ETag. Writes stay proxied — lr_put_proxy_404 proves the
// /:id dispatcher forwards instead of 405 (Node-side 404 on a dead id, no
// mutation either side).
{
  const LR = {
    alpha: 'rusttest-lr-alpha', alphaB: 'rusttest-lr-alpha-b',
    beta: 'rusttest-lr-beta', est: 'rusttest-lr-est', eur: 'rusttest-lr-eur',
  };
  function lrClear(d) {
    d.prepare('DELETE FROM list_cards WHERE list_id BETWEEN 931201 AND 931204').run();
    d.prepare('DELETE FROM card_lists WHERE id BETWEEN 931201 AND 931204').run();
    d.prepare('DELETE FROM collection WHERE card_id LIKE ?').run('rusttest-lr-%');
    d.prepare('DELETE FROM card_cache WHERE id LIKE ?').run('rusttest-lr-%');
  }
  function lrPrep() {
    // Same bench-session lookup the deck block uses (dkUid lives inside that
    // block's scope; this block gets its own copy).
    const dkUid = (d) => {
      const t = String(authHeaders.Authorization || '').replace(/^Bearer\s+/i, '');
      const s = d.prepare('SELECT user_id FROM sessions WHERE token = ?').get(t);
      if (!s) throw new Error('parity: bench session not found for list cases');
      return Number(s.user_id);
    };
    dbWrite((d) => {
      lrClear(d);
      const uid = dkUid(d);
      const other = d.prepare('SELECT id FROM users WHERE id != ? ORDER BY id LIMIT 1').get(uid);
      if (!other) throw new Error('parity: no second user for lr cases');
      const card = d.prepare('INSERT INTO card_cache (id,name,supertype,types,subtypes,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)');
      card.run(LR.alpha, 'RustTest LR Alpha', 'MTG', '["White"]', '["Creature"]', 'Rare', 'lrs', 'LR Set', '1', 'img/lr-a', 1, 1, 0, 0, 0, 2, '["W"]', 'USD');
      card.run(LR.alphaB, 'RustTest LR Alpha ', 'MTG', '["White"]', '["Creature"]', 'Rare', 'lrb', 'LR Set B', '2', 'img/lr-ab', 0.2, 0.2, 0, 0, 0, 2, '["W"]', 'USD');
      card.run(LR.beta, 'RustTest LR Beta', 'MTG', '["Blue"]', '["Instant"]', 'Common', 'lrp', 'LR Promos', '42', null, 0.5, 0.5, 0, 0, 0, 1, '["U"]', 'USD');
      card.run(LR.est, 'RustTest LR Est', 'MTG', '[]', '["Artifact"]', 'Common', 'lrs', 'LR Set', '7', null, 4, 4, 0, 0, 1, 0, '[]', 'USD');
      card.run(LR.eur, 'RustTest LR Eur', 'MTG', '[]', '["Artifact"]', 'Common', 'lrs', 'LR Set', '8', null, 9, 9, 0, 0, 0, 0, '[]', 'EUR');
      const list = d.prepare('INSERT INTO card_lists (id,user_id,name,description,accent_color,created_at,source,icon,sort_order) VALUES (?,?,?,?,?,?,?,?,?)');
      list.run(931201, uid, 'LR Wishlist', 'w', '#10b981', '2026-01-01 00:00:05', 'web', null, null);
      list.run(931202, uid, 'LR Owned Fold', 'o', '#10b981', '2026-01-01 00:00:04', 'web', null, 0);
      list.run(931203, uid, 'LR Broken', 'b', '#10b981', '2026-01-01 00:00:03', 'web', null, null);
      list.run(931204, other.id, 'LR Foreign', 'f', '#10b981', '2026-01-01 00:00:02', 'web', null, null);
      const lc = d.prepare('INSERT INTO list_cards (list_id,card_id,quantity) VALUES (?,?,?)');
      lc.run(931201, LR.alpha, 1);
      lc.run(931201, LR.alphaB, 1);
      lc.run(931201, LR.beta, 2);
      lc.run(931201, LR.eur, 1);
      lc.run(931202, LR.alpha, 4);
      lc.run(931202, LR.beta, 2);
      lc.run(931202, LR.est, 1);
      lc.run(931203, 'rusttest-lr-nope', 1);
      const col = d.prepare('INSERT INTO collection (card_id,quantity,user_id) VALUES (?,?,?)');
      col.run(LR.beta, 2, uid);
      col.run(LR.alphaB, 1, uid);
    });
  }
  function lrTeardown() { dbWrite((d) => lrClear(d)); }
  const lrCase = (name, path, headers = authHeaders, method = 'GET') =>
    cases.push({ name, path, headers, method, setup: lrPrep, teardown: lrTeardown });
  lrCase('lr_list_200', '/api/lists');
  lrCase('lr_list_noauth_401', '/api/lists', {});
  lrCase('lr_matches_200', '/api/lists/card-matches?q=RustTest');
  lrCase('lr_matches_alpha_200', '/api/lists/card-matches?q=Alpha');
  lrCase('lr_matches_wild_200', '/api/lists/card-matches?q=LR%20Beta%');
  lrCase('lr_matches_short_200', '/api/lists/card-matches?q=x');
  lrCase('lr_matches_none_200', '/api/lists/card-matches?q=zzzznothing');
  lrCase('lr_matches_meta_200', '/api/lists/card-matches?q=%25a%5Fb');
  lrCase('lr_matches_noauth_401', '/api/lists/card-matches?q=Card', {});
  lrCase('lr_detail_200', '/api/lists/931201');
  lrCase('lr_detail_owned_200', '/api/lists/931202');
  lrCase('lr_detail_foreign_404', '/api/lists/931204');
  lrCase('lr_detail_nan_404', '/api/lists/abc');
  lrCase('lr_detail_broken_422', '/api/lists/931203');
  lrCase('lr_detail_noauth_401', '/api/lists/931201', {});
  lrCase('lr_cardlist_200', '/api/lists/931201/cardlist');
  lrCase('lr_cardlist_detailed_200', '/api/lists/931201/cardlist?style=detailed');
  lrCase('lr_cardlist_missing_200', '/api/lists/931202/cardlist?missing=1');
  lrCase('lr_cardlist_foreign_404', '/api/lists/931204/cardlist');
  lrCase('lr_cardlist_broken_422', '/api/lists/931203/cardlist');
  lrCase('lr_cardlist_noauth_401', '/api/lists/931201/cardlist', {});
  // qs array semantics (live-pinned vs the container's qs): a REPEATED key
  // never equals the scalar, so style=detailed&style=x renders PLAIN and
  // missing=1&missing=0 does not filter; bracket q stringifies through
  // Array.join(',') — first-value-only would match 'RustTest' but the joined
  // string must answer {}.
  lrCase('lr_cardlist_style_array_200', '/api/lists/931201/cardlist?style=detailed&style=x');
  lrCase('lr_cardlist_missing_array_200', '/api/lists/931202/cardlist?missing=1&missing=0');
  lrCase('lr_matches_qjoin_200', '/api/lists/card-matches?q%5B%5D=RustTest&q%5B%5D=LR%20Alpha');
  lrCase('lr_matches_qbracket_200', '/api/lists/card-matches?q%5B%5D=RustTest');
  // iter-32: PUT on the /:id shape is NATIVE under lists_writes (the old
  // proxy-passthrough name is stale); a nonexistent id answers Node's own
  // 404 shape without touching any row.
  cases.push({
    name: 'lr_put_proxy_404', path: '/api/lists/931299', method: 'PUT',
    headers: { ...authHeaders, 'content-type': 'application/json' },
    body: '{"name":"LR Proxy Pass"}',
    setup: lrPrep, teardown: lrTeardown,
  });
}

// ===== LANE-A iter-32: card-list META WRITES (lists_writes.rs under
// RUST_ROUTES=lists_writes). Own id range 931211..931214, own card rows
// rusttest-lw-*. Create pins the Node contract (trim, defaults, cleanIcon,
// qty stacking via the all-cached native path) and the whole-request
// Scryfall-hydration FORWARD (uncached name: both sides run Node, and Rust
// must have created NOTHING before forwarding); update pins binding shapes,
// the accent TypeError-500 twin and changes==0 404; delete pins the
// cascade-twin (list_cards rows leave with the parent); order pins the
// Number()-filter survival table with post-filter sort_order indices.
{
  const LW_A = 'rusttest-lw-alpha';
  const LW_NAME_A = 'rusttest LW Alpha';
  function lwClear(d) {
    d.prepare('DELETE FROM list_cards WHERE list_id BETWEEN 931211 AND 931214').run();
    d.prepare('DELETE FROM card_lists WHERE id BETWEEN 931211 AND 931214').run();
    d.prepare("DELETE FROM card_lists WHERE name IN ('LW Fresh','LW Import','LW Native')").run();
    d.prepare('DELETE FROM card_cache WHERE id LIKE ?').run('rusttest-lw-%');
  }
  function lwPrep() {
    const uidOf = (d) => {
      const t = String(authHeaders.Authorization || '').replace(/^Bearer\s+/i, '');
      const s = d.prepare('SELECT user_id FROM sessions WHERE token = ?').get(t);
      if (!s) throw new Error('parity: bench session not found for lw cases');
      return Number(s.user_id);
    };
    dbWrite((d) => {
      lwClear(d);
      const uid = uidOf(d);
      const other = d.prepare('SELECT id FROM users WHERE id != ? ORDER BY id LIMIT 1').get(uid);
      if (!other) throw new Error('parity: no second user for lw cases');
      d.prepare('INSERT INTO card_cache (id,name,supertype,types,subtypes,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)')
        .run(LW_A, LW_NAME_A, 'MTG', '["Artifact"]', '[]', 'Common', 'lws', 'LW Set', '7', null, 0.3, 0.3, 0, 0, 0, 0, '[]', 'USD');
      const list = d.prepare('INSERT INTO card_lists (id,user_id,name,description,accent_color,created_at,source,icon,sort_order) VALUES (?,?,?,?,?,?,?,?,?)');
      list.run(931211, uid, 'LW Upd', 'u', '#10b981', '2026-01-01 00:00:05', 'web', null, null);
      list.run(931212, uid, 'LW Order A', 'a', '#10b981', '2026-01-01 00:00:04', 'web', null, null);
      list.run(931213, other.id, 'LW Foreign', 'f', '#10b981', '2026-01-01 00:00:03', 'web', null, null);
      list.run(931214, uid, 'LW Del', 'd', '#10b981', '2026-01-01 00:00:02', 'web', null, null);
      const lc = d.prepare('INSERT INTO list_cards (list_id,card_id,quantity) VALUES (?,?,?)');
      lc.run(931214, LW_A, 2);
    });
  }
  function lwTeardown() { dbWrite((d) => lwClear(d)); }
  const lwDb = (sql, ...args) => dbWrite((d) => d.prepare(sql).all(...args));
  const LW_JSON = { ...authHeaders, 'content-type': 'application/json' };
  const NOAUTH_JSON = { 'content-type': 'application/json' };
  cases.push({
    name: 'lw_create_201', path: '/api/lists', headers: LW_JSON, method: 'POST',
    body: '{"name":"  LW Fresh  "}', etagReplay: false, strip: ['id'],
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      const rows = lwDb("SELECT name, description, accent_color, icon FROM card_lists WHERE name='LW Fresh'");
      const want = { name: 'LW Fresh', description: '', accent_color: '#10b981', icon: null };
      return { ok: rows.length === 1 && JSON.stringify(rows[0]) === JSON.stringify(want), why: `create row -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    // All-cached list_text: the NATIVE path (no forward). Two lines for the
    // same card stack via ON CONFLICT (2+1=3); the '(lws) 7' setRef tail
    // must be stripped off the name before the cache lookup; icon garbage
    // -> NULL; explicit non-# accent -> default.
    name: 'lw_create_native_listtext_201', path: '/api/lists', headers: LW_JSON, method: 'POST',
    body: '{"name":"LW Native","icon":"Bad Icon!","accent_color":"nope","list_text":"2 rusttest LW Alpha\\nrusttest LW Alpha (lws) 7"}',
    etagReplay: false, strip: ['id'],
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      const rows = lwDb("SELECT l.name, l.icon, l.accent_color, lc.card_id, lc.quantity FROM card_lists l JOIN list_cards lc ON lc.list_id = l.id WHERE l.name='LW Native' ORDER BY lc.card_id");
      const ok = rows.length === 1 && rows[0].card_id === LW_A && rows[0].quantity === 3
        && rows[0].icon === null && rows[0].accent_color === '#10b981';
      return { ok, why: `native rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    // Uncached name -> the hydration leg is Node-side: Rust FORWARDS the
    // whole request before its INSERT (no partial local write). Both sides
    // answer with Node's own matched/unmatched fold.
    name: 'lw_create_uncached_forward_201', path: '/api/lists', headers: LW_JSON, method: 'POST',
    body: '{"name":"LW Import","list_text":"2 rusttest LW Alpha\\nnot-a-real-card-zzq (lws) 9"}',
    etagReplay: false, strip: ['id'],
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      const rows = lwDb("SELECT lc.card_id, lc.quantity FROM card_lists l JOIN list_cards lc ON lc.list_id = l.id WHERE l.name='LW Import' ORDER BY lc.card_id");
      return { ok: rows.length === 1 && rows[0].card_id === LW_A && rows[0].quantity === 2, why: `import rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lw_create_no_name_400', path: '/api/lists', headers: LW_JSON, method: 'POST',
    body: '{"name":"   "}', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      // NOT `name LIKE 'LW %'` — lwPrep seeds four 'LW …' rows, so that
      // count is never 0. The 400 path must create no blank-named row.
      const rows = lwDb("SELECT COUNT(*) n FROM card_lists WHERE TRIM(name) = '' OR name = '   '");
      return { ok: rows[0].n === 0, why: `400 created no row (${rows[0].n})`, seen: rows };
    },
  });
  cases.push({
    name: 'lw_create_noauth_401', path: '/api/lists', headers: NOAUTH_JSON, method: 'POST',
    body: '{"name":"LW Nope"}', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      const rows = lwDb("SELECT COUNT(*) n FROM card_lists WHERE name='LW Nope'");
      return { ok: rows[0].n === 0, why: `noauth created no row (${rows[0].n})`, seen: rows };
    },
  });
  cases.push({
    name: 'lw_create_badjson_500', path: '/api/lists', headers: LW_JSON, method: 'POST',
    body: '{oops', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
  });
  cases.push({
    name: 'lw_update_200', path: '/api/lists/931211', headers: LW_JSON, method: 'PUT',
    body: '{"name":" LW Renamed ","description":5,"accent_color":"#0f0","icon":"ab-1"}', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      const rows = lwDb('SELECT name, description, accent_color, icon FROM card_lists WHERE id=931211');
      const want = { name: 'LW Renamed', description: '5', accent_color: '#0f0', icon: 'ab-1' };
      return { ok: rows.length === 1 && JSON.stringify(rows[0]) === JSON.stringify(want), why: `updated row -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  // accent_color != null && startsWith('#') sits OUTSIDE the route's
  // try/catch (backend/src/routes/lists.js:385, before `try {`): a non-string
  // accent makes the async handler reject with an unhandled TypeError and
  // Node 22 kills the whole process. Sending that payload at the SHARED
  // :3312 container takes it down mid-suite, so no non-string accent case can
  // live here (5, true, {}, [] all crash identically). Rust answers the
  // route-catch shape (500 'Failed to update list', no write) — the same body
  // Node would answer if the line sat inside the try; covered by
  // tests/lists_writes.rs::update_binds_like_node_and_gates (accent 5 -> 500,
  // accent null -> NULL column). String accents (with/without '#') stay
  // crash-free and are covered by lw_update_200 + create cases.
  // Orchestrator-default decision, iter 32.
  cases.push({
    name: 'lw_update_foreign_404', path: '/api/lists/931213', headers: LW_JSON, method: 'PUT',
    body: '{"name":"LW Steal"}', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      const rows = lwDb("SELECT name FROM card_lists WHERE id=931213");
      return { ok: rows[0].name === 'LW Foreign', why: `foreign row touched -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lw_update_no_name_400', path: '/api/lists/931211', headers: LW_JSON, method: 'PUT',
    body: '{"name":""}', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
  });
  cases.push({
    name: 'lw_delete_200', path: '/api/lists/931214', headers: LW_JSON, method: 'DELETE',
    etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      const lists = lwDb('SELECT id FROM card_lists WHERE id=931214');
      const cards = lwDb('SELECT * FROM list_cards WHERE list_id=931214');
      return { ok: lists.length === 0 && cards.length === 0, why: `cascade -> lists=${lists.length} cards=${cards.length}`, seen: { lists, cards } };
    },
  });
  cases.push({
    name: 'lw_delete_missing_404', path: '/api/lists/931299', headers: LW_JSON, method: 'DELETE',
    etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
  });
  cases.push({
    // Number() survival: "931212" string kept, 4.5 dies, null -> 0 KEPT,
    // {} NaN dies; sort_order = POST-FILTER index (931212 -> 0, 931211 -> 1).
    name: 'lw_order_200', path: '/api/lists/order', headers: LW_JSON, method: 'PUT',
    body: '{"ids":["931212",4.5,null,{},"931211"]}', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      // null -> Number(null)=0 PASSES Number.isInteger, so it survives and
      // consumes the post-filter index 1 (updating no row): 931212->0,
      // 0->1 (no match), 931211->2. This pins the survives-and-consumes
      // semantic; 4.5/{} die pre-index.
      const rows = lwDb('SELECT id, sort_order FROM card_lists WHERE id IN (931211,931212) ORDER BY sort_order');
      const ok = rows.length === 2 && rows[0].id === 931212 && rows[0].sort_order === 0 && rows[1].id === 931211 && rows[1].sort_order === 2;
      return { ok, why: `order -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lw_order_empty_400', path: '/api/lists/order', headers: LW_JSON, method: 'PUT',
    body: '{"ids":[]}', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
  });
  cases.push({
    name: 'lw_order_badjson_500', path: '/api/lists/order', headers: LW_JSON, method: 'PUT',
    body: '{oops', etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
  });
  cases.push({
    name: 'lw_delete_noauth_401', path: '/api/lists/931214', headers: NOAUTH_JSON, method: 'DELETE',
    etagReplay: false,
    setup: lwPrep, teardown: lwTeardown,
    readBack: async () => {
      const rows = lwDb('SELECT id FROM card_lists WHERE id=931214');
      return { ok: rows.length === 1, why: `noauth deleted the list`, seen: rows };
    },
  });
}

// --- LANE-A iteration 33: card-list CARD-BODY writes (lists_body_writes) -----
// The seven /api/lists/:id card-body write routes from backend/src/routes/lists.js
// (add SET+fold, bulk add INCREMENT, bulk remove, remove-from-collection,
// cheapest-printings, change printing, remove card). Two cached printings of
// one logical card (cardKey = LOWER(TRIM(name)), cardIdentity.js) drive the
// fold/merge/move semantics; a foreign user's list and a noauth probe pin the
// gates. Hydration-miss forwards are cargo-pinned (tests/lists_body_writes.rs)
// — parity cannot pin them because getCardById hits the real provider.
{
  const LB_A = 'rusttest-lb-a', LB_A2 = 'rusttest-lb-a2', LB_B = 'rusttest-lb-b', LB_C = 'rusttest-lb-c';
  const LB_NAME_A = 'LB Alpha', LB_NAME_B = 'LB Beta', LB_NAME_C = 'LB Gamma';
  const LB_CARDS = [LB_A, LB_A2, LB_B, LB_C];
  const LB_LISTS = [931311, 931312, 931313, 931314];
  const LB_COLL = [931401, 931402];
  function lbClear(d) {
    d.prepare('DELETE FROM list_cards WHERE list_id IN (?,?,?,?,?)').run(...LB_LISTS);
    d.prepare('DELETE FROM card_lists WHERE id IN (?,?,?,?,?)').run(...LB_LISTS);
    // Delete collection rows BY CARD_ID, not by fixed id: the
    // remove-from-collection partial-take path splits a stack into a second
    // row with a fresh autoincrement id, and a card_cache DELETE under FK
    // would then abort on the surviving child (iter-33 flake).
    d.prepare('DELETE FROM collection WHERE card_id IN (?,?,?,?)').run(...LB_CARDS);
    d.prepare('DELETE FROM card_cache WHERE id IN (?,?,?,?)').run(...LB_CARDS);
    d.prepare('DELETE FROM collection WHERE id IN (?,?)').run(...LB_COLL);
  }
  function lbPrep() {
    const uidOf = (d) => {
      const t = String(authHeaders.Authorization || '').replace(/^Bearer\s+/i, '');
      const s = d.prepare('SELECT user_id FROM sessions WHERE token = ?').get(t);
      if (!s) throw new Error('parity: bench session not found for lb cases');
      return Number(s.user_id);
    };
    dbWrite((d) => {
      lbClear(d);
      const uid = uidOf(d);
      const other = d.prepare('SELECT id FROM users WHERE id != ? ORDER BY id LIMIT 1').get(uid);
      if (!other) throw new Error('parity: no second user for lb cases');
      const card = d.prepare('INSERT INTO card_cache (id,name,supertype,types,subtypes,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)');
      // LB_A / LB_A2: same game card (same name), different printings; the
      // 'a2' printing is the cheap one (normal 0.2 vs 5).
      card.run(LB_A, LB_NAME_A, 'MTG', '["Artifact"]', '[]', 'Common', 'lbs', 'LB Set', '1', null, 0, 5, 0, 0, 0, 0, '[]', 'USD');
      card.run(LB_A2, LB_NAME_A, 'MTG', '["Artifact"]', '[]', 'Common', 'lbq', 'LB Promos', '2', null, 0, 0.2, 0, 0, 0, 0, '[]', 'USD');
      // LB_B: already-cheapest single printing (trend 0.5) for the skip path.
      card.run(LB_B, LB_NAME_B, 'MTG', '["Instant"]', '[]', 'Common', 'lbs', 'LB Set', '3', null, 0.5, 0, 0, 0, 0, 1, '[]', 'USD');
      // LB_C: cached, starts off every list.
      card.run(LB_C, LB_NAME_C, 'MTG', '["Land"]', '[]', 'Common', 'lbq', 'LB Promos', '4', null, 0, 0.4, 0, 0, 0, 0, '[]', 'USD');
      const list = d.prepare('INSERT INTO card_lists (id,user_id,name,description,accent_color,created_at,source,icon,sort_order) VALUES (?,?,?,?,?,?,?,?,?)');
      list.run(931311, uid, 'LB Body', 'b', '#10b981', '2026-01-01 00:00:05', 'web', null, null);
      list.run(931312, uid, 'LB Sweep', 's', '#10b981', '2026-01-01 00:00:04', 'web', null, null);
      list.run(931313, other.id, 'LB Foreign', 'f', '#10b981', '2026-01-01 00:00:03', 'web', null, null);
      list.run(931314, uid, 'LB RFC', 'r', '#10b981', '2026-01-01 00:00:02', 'web', null, null);
      const lc = d.prepare('INSERT INTO list_cards (list_id,card_id,quantity) VALUES (?,?,?)');
      lc.run(931311, LB_A, 1);
      lc.run(931311, LB_B, 2);
      lc.run(931312, LB_A, 1);
      lc.run(931312, LB_A2, 3);
      lc.run(931312, LB_B, 2);
      lc.run(931314, LB_A, 1);
      lc.run(931314, LB_B, 2);
      // Owned pile for remove-from-collection: two equivalent printings so
      // the exact-first/newest-first ordering and per-key budget both bite.
      const coll = d.prepare('INSERT INTO collection (id, card_id, quantity, condition, printing, language, added_at, is_trade, user_id, is_proxy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)');
      coll.run(931401, LB_A, 2, 'Near Mint', 'Normal', 'English', '2026-01-03 10:00:00', 0, uid, 0);
      coll.run(931402, LB_A2, 1, 'Near Mint', 'Holofoil', 'English', '2026-01-02 10:00:00', 0, uid, 0);
    });
  }
  function lbTeardown() { dbWrite((d) => lbClear(d)); }
  const lbDb = (sql, ...args) => dbWrite((d) => d.prepare(sql).all(...args));
  const lbRows = (list) => lbDb('SELECT card_id, quantity FROM list_cards WHERE list_id = ? ORDER BY card_id', list)
    .map((r) => [String(r.card_id), Number(r.quantity)]);
  const LB_JSON = { ...authHeaders, 'content-type': 'application/json' };
  const LB_NOAUTH = { 'content-type': 'application/json' };

  cases.push({
    name: 'lb_add_setfold_200', path: '/api/lists/931311/cards', headers: LB_JSON, method: 'POST',
    body: `{"card_id":"${LB_A2}","quantity":"2x"}`, etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    // Node: equivalent row (ORDER BY (lc.card_id = ?) DESC, lc.card_id) picks
    // the EXISTING LB_A row; SET semantics put quantity 2 there, no new row.
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_A, 2], [LB_B, 2]]);
      return { ok, why: `set-fold rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_add_new_clamp_200', path: '/api/lists/931311/cards', headers: LB_JSON, method: 'POST',
    body: `{"card_id":"${LB_C}","quantity":0}`, etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    // parseInt(0)||1 -> 1: a fresh key inserts at quantity 1.
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_A, 1], [LB_B, 2], [LB_C, 1]]);
      return { ok, why: `clamp-insert rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_add_no_cardid_400', path: '/api/lists/931311/cards', headers: LB_JSON, method: 'POST',
    body: '{}', etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_A, 1], [LB_B, 2]]);
      return { ok, why: `400 mutated the list -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_add_foreign_404', path: '/api/lists/931313/cards', headers: LB_JSON, method: 'POST',
    body: `{"card_id":"${LB_C}"}`, etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    readBack: async () => {
      const rows = lbRows(931313);
      return { ok: rows.length === 0, why: `foreign list gained rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_add_noauth_401', path: '/api/lists/931311/cards', headers: LB_NOAUTH, method: 'POST',
    body: `{"card_id":"${LB_C}"}`, etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_A, 1], [LB_B, 2]]);
      return { ok, why: `noauth added a row -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_bulk_add_200', path: '/api/lists/931311/cards/bulk', headers: LB_JSON, method: 'POST',
    body: `{"cards":[{"card_id":"${LB_B}","quantity":3},{"card_id":"${LB_A2}","quantity":"2x"},{"quantity":5},{"card_id":"${LB_C}"}]}`, etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    // INCREMENT semantics: LB_B 2+3=5, the equivalent LB_A row +2, LB_C new 1,
    // falsy card_id skipped; added = 3+2+1 = 6, failed [].
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_A, 3], [LB_B, 5], [LB_C, 1]]);
      return { ok, why: `bulk-add rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_bulk_add_empty_400', path: '/api/lists/931311/cards/bulk', headers: LB_JSON, method: 'POST',
    body: '{"cards":[]}', etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
  });
  cases.push({
    name: 'lb_bulk_remove_200', path: '/api/lists/931311/cards/bulk-remove', headers: LB_JSON, method: 'POST',
    body: `{"cards":[{"card_id":"${LB_A2}","quantity":1},{"card_id":"${LB_B}","quantity":1},{"card_id":"${LB_C}"}]}`, etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    // LB_A equivalent row qty1 -> DELETE (+1); LB_B 2>1 -> UPDATE to 1 (+1);
    // LB_C absent -> silent. removed 2, no failed array.
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_B, 1]]);
      return { ok, why: `bulk-remove rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_rfc_dryrun_200', path: '/api/lists/931314/remove-from-collection', headers: LB_JSON, method: 'POST',
    body: '{"dry_run":true}', etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    readBack: async () => {
      const rows = lbDb('SELECT id, quantity FROM collection WHERE id IN (931401,931402) ORDER BY id')
        .map((r) => [Number(r.id), Number(r.quantity)]);
      const ok = JSON.stringify(rows) === JSON.stringify([[931401, 2], [931402, 1]]);
      return { ok, why: `dry_run mutated the collection -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_rfc_real_200', path: '/api/lists/931314/remove-from-collection', headers: LB_JSON, method: 'POST',
    body: '{"dry_run":false}', etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    // LB_A demand 1: exact printing first (931401) -> 2 becomes 1.
    // LB_B demand 2, owned 0 -> short, untouched.
    readBack: async () => {
      const rows = lbDb('SELECT id, quantity FROM collection WHERE id IN (931401,931402) ORDER BY id')
        .map((r) => [Number(r.id), Number(r.quantity)]);
      const ok = JSON.stringify(rows) === JSON.stringify([[931401, 1], [931402, 1]]);
      return { ok, why: `rfc rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_cheap_200', path: '/api/lists/931311/cheapest-printings', headers: LB_JSON, method: 'PUT',
    body: '{}', etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    // LB_A -> LB_A2 (0.2 vs 5); LB_B already the sole cheapest row -> skip.
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_A2, 1], [LB_B, 2]]);
      return { ok, why: `cheapest rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_printing_200', path: `/api/lists/931311/cards/${LB_A}/printing`, headers: LB_JSON, method: 'PUT',
    body: `{"card_id":"${LB_A2}"}`, etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_A2, 1], [LB_B, 2]]);
      return { ok, why: `printing rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_printing_other_card_400', path: `/api/lists/931311/cards/${LB_A}/printing`, headers: LB_JSON, method: 'PUT',
    body: `{"card_id":"${LB_B}"}`, etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    readBack: async () => {
      const rows = lbRows(931311);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_A, 1], [LB_B, 2]]);
      return { ok, why: `rejected printing still mutated -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_remove_sweep_200', path: `/api/lists/931312/cards/${LB_A}`, headers: LB_JSON, method: 'DELETE',
    etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    // The equivalent sweep deletes LB_A AND LB_A2 (same game card), LB_B stays.
    readBack: async () => {
      const rows = lbRows(931312);
      const ok = JSON.stringify(rows) === JSON.stringify([[LB_B, 2]]);
      return { ok, why: `sweep rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
  cases.push({
    name: 'lb_remove_noauth_401', path: `/api/lists/931312/cards/${LB_A}`, headers: LB_NOAUTH, method: 'DELETE',
    etagReplay: false,
    setup: lbPrep, teardown: lbTeardown,
    readBack: async () => {
      const rows = lbRows(931312);
      const ok = rows.length === 3;
      return { ok, why: `noauth deleted rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
  });
}

// --- Lane B unit 5b: the static trio ---------------------------------------
// Header-level byte parity (file ETag form, Cache-Control classes, Vary
// merges, CSP-trio strip, finalhandler page, 301 twin) is pinned live
// (:3312-vs-:3320 header dumps) and by cargo tests (lib + static_assets_route);
// the battery asserts status/content-type/body equality on every arm —
// including the oddballs: serve-static fallthrough:false miss answers the
// generic-error 500 twin, dotfiles:deny likewise, non-GET on the static
// mounts answers send's 405 (Allow GET, HEAD; CL 0) or, for app.get-only
// routes, the finalhandler default 404. File names are the live content-
// hashed pins under data/models/client-scan (shared volume, both stacks).
cases.push({ name: 'sa_manifest', path: '/scan-assets/manifest.json', headers: {}, method: 'GET' });
cases.push({ name: 'sa_dict', path: '/scan-assets/rec-dict.f7aa897ca828.txt', headers: {}, method: 'GET' });
cases.push({ name: 'sa_gz_head', path: '/scan-assets/scan-index.3c89ac0fafc6.json.gz', headers: {}, method: 'HEAD' });
cases.push({ name: 'sa_missing_500', path: '/scan-assets/no-such-file.json', headers: {}, method: 'GET' });
cases.push({ name: 'sa_dotfile_500', path: '/scan-assets/.hidden', headers: {}, method: 'GET' });
cases.push({ name: 'sa_post_405', path: '/scan-assets/manifest.json', headers: {}, method: 'POST', body: '{}' });
cases.push({ name: 'sa_dir_301', path: '/scan-assets', headers: {}, method: 'GET' });
cases.push({ name: 'sa_dir_slash_500', path: '/scan-assets/', headers: {}, method: 'GET' });
cases.push({ name: 'md_model_head', path: '/models/cornelius.onnx', headers: {}, method: 'HEAD' });
cases.push({ name: 'md_model_post_404', path: '/models/cornelius.onnx', headers: {}, method: 'POST' });
cases.push({ name: 'pt_bare_301', path: '/scryfall-portal', headers: {}, method: 'GET' });
cases.push({ name: 'pt_index', path: '/scryfall-portal/', headers: {}, method: 'GET' });
cases.push({ name: 'pt_head_405', path: '/scryfall-portal/', headers: {}, method: 'HEAD' });
cases.push({ name: 'pt_post_404', path: '/scryfall-portal/', headers: {}, method: 'POST' });
// --- Lane B unit 5c: the SPA mount ------------------------------------------
// Byte parity of the vite tree itself is GUARANTEED by construction: the Rust
// instance serves a docker-cp copy of the container's /app/frontend/dist
// (data/frontend-dist-5c; mtime + md5 verified equal file-by-file, so send's
// W/"{size:x}-{mtimeMs:x}" ETags and Last-Modified agree by construction).
// These cases pin the RESPONSE twins: catch-all sendFile (public, max-age=0),
// setHeaders classes (hashed /assets/ immutable vs public/ no-cache), the
// serve-static directory 301 document, finalhandler non-GET 404, and Node's
// catch-all startsWith('/api') gate (/apifoo is NOT SPA land). The gzip index
// leg rides implicitly: undici always sends accept-encoding and both bodies
// are compared decoded. 304 pairs, dotfile arms and the %zz 500 are pinned by
// cargo (spa.rs lib tests + tests/spa_route.rs).
cases.push({ name: 'spa_index', path: '/', headers: {}, method: 'GET' });
cases.push({ name: 'spa_deep_link', path: '/collection', headers: {}, method: 'GET' });
cases.push({ name: 'spa_deep_link_dots', path: '/deckbuilder/12/xyz', headers: {}, method: 'GET' });
cases.push({ name: 'spa_assets_301', path: '/assets', headers: {}, method: 'GET' });
cases.push({ name: 'spa_logo_nocache_class', path: '/logo.svg', headers: {}, method: 'GET' });
cases.push({ name: 'spa_hashed_js_immutable', path: '/assets/AddCards-BImmNJm_.js', headers: {}, method: 'GET' });
cases.push({ name: 'spa_webmanifest', path: '/manifest.webmanifest', headers: {}, method: 'GET' });
cases.push({ name: 'spa_head_index', path: '/', headers: {}, method: 'HEAD' });
cases.push({ name: 'spa_post_404_finalhandler', path: '/collection', headers: {}, method: 'POST', body: '{}' });
cases.push({ name: 'spa_apifoo_proxied_404', path: '/apifoo', headers: {}, method: 'GET' });



// --- Lane A iter-38: export group (routes/importExport.js) -----------------
// Byte contract: CSV/JSON attachment bytes, Content-Type (+charset),
// Content-Disposition (cmpDisposition), Express body ETag + 304 replay
// (etagReplay default for GETs), /import JSON key ORDER (deepDiff asserts
// key order), and every rejection list verbatim. Import writes go against
// the shared DB copy with a purchase_price marker and are deleted in
// teardown; the read-back reads the shared table, so a Rust write is
// verified through Node's read path and vice versa.
{
  const EX_H = { ...authHeaders, 'content-type': 'application/json' };
  const exUserId = () => dbWrite((d) => d
    .prepare('SELECT user_id FROM sessions WHERE token = ?')
    .get(USER1_TOKEN)?.user_id);
  const EX_MARKER = 0.71237;

  cases.push({ name: 'ex_get_csv', path: '/api/export', headers: authHeaders, method: 'GET', cmpDisposition: true });
  cases.push({ name: 'ex_get_json', path: '/api/export?format=json', headers: authHeaders, method: 'GET', cmpDisposition: true });
  cases.push({ name: 'ex_get_manabox', path: '/api/export?ecosystem=MANABOX', headers: authHeaders, method: 'GET', cmpDisposition: true });
  cases.push({ name: 'ex_get_tcg', path: '/api/export?format=csv&ecosystem=tcgplayer', headers: authHeaders, method: 'GET', cmpDisposition: true });
  cases.push({ name: 'ex_get_empty_eco', path: '/api/export?ecosystem=&format=', headers: authHeaders, method: 'GET', cmpDisposition: true });
  cases.push({ name: 'ex_get_noauth', path: '/api/export', headers: {}, method: 'GET', etagReplay: false });

  cases.push({
    name: 'ex_post_view', path: '/api/export', headers: EX_H, method: 'POST', cmpDisposition: true,
    etagReplay: false,
    setup: async () => {
      const uid = exUserId();
      const rows = dbWrite((d) => d
        .prepare('SELECT id FROM collection WHERE user_id = ? ORDER BY id DESC LIMIT 3')
        .all(uid));
      const ids = rows.map((r) => r.id);
      const body = JSON.stringify({ entry_ids: [...ids].reverse().concat(ids[0] ?? 1) });
      return { body };
    },
  });
  cases.push({
    name: 'ex_post_view_json_eco', path: '/api/export', headers: EX_H, method: 'POST', cmpDisposition: true,
    etagReplay: false,
    setup: async () => {
      const uid = exUserId();
      const rows = dbWrite((d) => d
        .prepare('SELECT id FROM collection WHERE user_id = ? ORDER BY id LIMIT 2')
        .all(uid));
      return { body: JSON.stringify({ entry_ids: rows.map((r) => r.id), format: 'json', ecosystem: 'DragonShield' }) };
    },
  });
  cases.push({ name: 'ex_post_view_nonarray', path: '/api/export', headers: EX_H, method: 'POST', body: '{}', etagReplay: false });
  cases.push({ name: 'ex_post_view_nonints', path: '/api/export', headers: EX_H, method: 'POST', body: '{"entry_ids":["a",{}]}', etagReplay: false });
  cases.push({ name: 'ex_post_view_filter', path: '/api/export', headers: EX_H, method: 'POST', body: '{"entry_ids":["1",true,null,"zz"]}', etagReplay: false });

  // Import — success writes, so every case runs serially with cleanup.
  cases.push({
    name: 'ex_import_json_ok', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false,
    setup: async () => {
      dbWrite((d) => d.prepare('DELETE FROM collection WHERE purchase_price = ?').run(EX_MARKER));
      const card = dbWrite((d) => d
        .prepare("SELECT id FROM card_cache WHERE id LIKE 'mtg-%' ORDER BY rowid LIMIT 1")
        .get());
      return {
        body: JSON.stringify({
          format: 'json',
          data: [{
            game: 'mtg', name: 'Parity Import Card', card_id: card.id,
            quantity: 2, condition: 'Lightly Played', printing: 'Normal',
            language: 'English', purchase_price: EX_MARKER,
          }],
        }),
      };
    },
    readBack: async () => {
      // marker-only predicate: the case's token is the bench user, not the
      // sessions row behind USER1_TOKEN; 0.71237 is unique enough.
      const rows = dbWrite((d) => d.prepare(
        'SELECT quantity, condition, printing, language, purchase_price, is_proxy, card_id FROM collection WHERE purchase_price = ?',
      ).all(EX_MARKER));
      const ok = rows.length === 1;
      return { ok, why: `imported rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
    teardown: async () => {
      dbWrite((d) => d.prepare('DELETE FROM collection WHERE purchase_price = ?').run(EX_MARKER));
    },
  });
  cases.push({
    // all-or-nothing: the two bad rows reject the whole import (incl. the
    // two good ones) — no partial writes, rejected list byte-equal.
    name: 'ex_import_json_all_or_nothing', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false,
    setup: async () => {
      dbWrite((d) => d.prepare('DELETE FROM collection WHERE purchase_price = ?').run(EX_MARKER));
      const card = dbWrite((d) => d
        .prepare("SELECT id FROM card_cache WHERE id LIKE 'mtg-%' ORDER BY rowid LIMIT 1")
        .get());
      return {
        body: JSON.stringify({
          format: 'json',
          data: [
            { game: 'mtg', name: 'Parity Import Card', card_id: card.id, quantity: 1, purchase_price: EX_MARKER },
            { game: 'pokemon', name: 'Not MTG', purchase_price: EX_MARKER },
            { game: 'mtg', name: '', purchase_price: EX_MARKER },
            { name: 'No Game Field', card_id: card.id, quantity: 3, purchase_price: EX_MARKER },
          ],
        }),
      };
    },
    readBack: async () => {
      const rows = dbWrite((d) => d.prepare(
        'SELECT quantity, card_id FROM collection WHERE purchase_price = ?',
      ).all(EX_MARKER));
      const ok = rows.length === 0;
      return { ok, why: `all-or-nothing wrote rows -> ${JSON.stringify(rows)}`, seen: rows };
    },
    teardown: async () => {
      dbWrite((d) => d.prepare('DELETE FROM collection WHERE purchase_price = ?').run(EX_MARKER));
    },
  });
  cases.push({
    name: 'ex_import_csv_rejected', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false,
    body: JSON.stringify({ format: 'tcgplayer', data: 'Card Name,Quantity\nNotARealCard Parity XYZ,2\nAlsoNotReal Parity QRS,1' }),
  });
  cases.push({ name: 'ex_import_csv_empty', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false, body: '{"format":"manabox","data":"Card Name\\n"}' });
  cases.push({ name: 'ex_import_nodata', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false, body: '{}' });
  cases.push({ name: 'ex_import_baddata', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false, body: '{"format":"json","data":{}}' });
  cases.push({ name: 'ex_import_null_item', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false, body: '{"format":"json","data":[null]}' });
  cases.push({ name: 'ex_import_fmt_num', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false, body: '{"format":5,"data":"a,b\\nc,d"}' });
  cases.push({ name: 'ex_import_fmt_null', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false, body: '{"format":null,"data":"a,b\\nc,d"}' });
  cases.push({ name: 'ex_import_badjson_str', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false, body: '{"format":"json","data":"zzz"}' });
  cases.push({ name: 'ex_import_printing_str_null', path: '/api/import', headers: EX_H, method: 'POST', etagReplay: false, body: '{"format":"json","data":[{"game":"mtg","name":"X","printing":"null"}]}' });
  cases.push({ name: 'ex_import_noauth', path: '/api/import', headers: { 'content-type': 'application/json' }, method: 'POST', body: '{}', etagReplay: false });
  // ===== cards_meta (iter 39): POST /api/cards/highlight,
  // GET /api/cards/:id/decks, GET /api/cards/:id/price-history,
  // GET /api/stats/commander-of-the-day, GET /api/suggest under
  // RUST_ROUTES=cards_meta. Fixed ids; setup reseeds per side-run. =====
  const CM_PH = 'rusttest-cm-ph';
  const CM_PH_BAD = 'rusttest-cm-ph-bad';
  const CM_DUP = 'rusttest-cm-dup-emh';
  const CM_PLAIN = 'rusttest-cm-plain';
  function cmUid(d) {
    const t = String(authHeaders.Authorization || '').replace(/^Bearer\s+/i, '');
    const s = d.prepare('SELECT user_id FROM sessions WHERE token = ?').get(t);
    if (!s) throw new Error('parity: bench session not found for cards_meta cases');
    return Number(s.user_id);
  }
  function cmClear(d) {
    d.prepare('DELETE FROM price_history WHERE card_id IN (?,?)').run(CM_PH, CM_PH_BAD);
    d.prepare('DELETE FROM deck_cards WHERE deck_id BETWEEN 931101 AND 931103').run();
    d.prepare('DELETE FROM decks WHERE id BETWEEN 931101 AND 931103').run();
    d.prepare('DELETE FROM card_cache WHERE id IN (?,?)').run(CM_DUP, CM_PLAIN);
  }
  function cmPrep() {
    dbWrite((d) => {
      cmClear(d);
      const uid = cmUid(d);
      const ins = d.prepare('INSERT INTO decks (id,user_id,name,description,checked_out,created_at) VALUES (?,?,?,?,?,?)');
      ins.run(931101, uid, 'CM Checked One', '', 1, '2026-01-01 00:00:00');
      ins.run(931102, uid, 'CM Not Checked', '', 0, '2026-01-01 00:00:00');
      d.prepare("INSERT INTO card_cache (id,name,types,subtypes,rarity,price_trend) VALUES (?,?,?,?,'Rare',0.2)")
        .run(CM_DUP, 'Emergency Medical Hologram', '["Artifact"]', '[]');
      d.prepare("INSERT INTO card_cache (id,supertype,name,types,subtypes,rarity,price_trend) VALUES (?,?,?,?,?, 'Common',0.1)")
        .run(CM_PLAIN, 'Land', 'Plains', '["Land"]', '["Basic","Plains"]');
      const dc = d.prepare('INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES (?,?,?)');
      dc.run(931101, CW_CARD, 2);
      dc.run(931101, CM_DUP, 1);
      dc.run(931101, CM_PLAIN, 4);
      dc.run(931102, CW_CARD, 7);
      const ph = d.prepare('INSERT INTO price_history (card_id,price,recorded_at) VALUES (?,?,?)');
      ph.run(CM_PH, 1.0, '2020-01-01 00:00:00');
      ph.run(CM_PH, 2.5, '2026-01-01 00:00:00');
      ph.run(CM_PH, 2.5, '2026-01-02 00:00:00');
      ph.run(CM_PH, 2.5, '2026-01-03 00:00:00');
      ph.run(CM_PH, 3.0, '2026-01-04 00:00:00');
      ph.run(CM_PH_BAD, 2.5, 'bad-date');
    });
  }
  function cmTeardown() { dbWrite((d) => cmClear(d)); }
  cases.push({ name: 'cm_decks_hit', path: `/api/cards/${CW_CARD}/decks`, headers: authHeaders, setup: cmPrep, teardown: cmTeardown });
  cases.push({ name: 'cm_decks_basic', path: `/api/cards/${CM_PLAIN}/decks`, headers: authHeaders, setup: cmPrep, teardown: cmTeardown });
  cases.push({ name: 'cm_ph_all', path: `/api/cards/${CM_PH}/price-history?range=x`, headers: authHeaders, setup: cmPrep, teardown: cmTeardown });
  cases.push({ name: 'cm_ph_default', path: `/api/cards/${CM_PH}/price-history`, headers: authHeaders, setup: cmPrep, teardown: cmTeardown });
  cases.push({ name: 'cm_ph_bad500', path: `/api/cards/${CM_PH_BAD}/price-history?range=x`, headers: authHeaders, setup: cmPrep, teardown: cmTeardown });
  cases.push({ name: 'cm_ph_ctor', path: `/api/cards/${CM_PH}/price-history?range=constructor`, headers: authHeaders, setup: cmPrep, teardown: cmTeardown });
  cases.push({ name: 'cm_ph_proto', path: `/api/cards/${CM_PH}/price-history?range=__proto__`, headers: authHeaders, setup: cmPrep, teardown: cmTeardown });
  cases.push({ name: 'cm_ph_noauth', path: `/api/cards/${CM_PH}/price-history`, headers: {} });
  cases.push({ name: 'cm_hl_ok', path: '/api/cards/highlight', headers: EX_H, method: 'POST', etagReplay: false,
    body: JSON.stringify({ q: 'goblin', names: ['Goblin Racketeer', 'Island', 'goblin racketeer', ''] }) });
  cases.push({ name: 'cm_hl_val', path: '/api/cards/highlight', headers: EX_H, method: 'POST', etagReplay: false,
    body: JSON.stringify({ q: 'goblin' }) });
  cases.push({ name: 'cm_hl_grammar', path: '/api/cards/highlight', headers: EX_H, method: 'POST', etagReplay: false,
    body: JSON.stringify({ q: 'is:land', names: ['Island'] }) });
  cases.push({ name: 'cm_hl_badjson', path: '/api/cards/highlight', headers: EX_H, method: 'POST', etagReplay: false,
    body: '{oops' });
  cases.push({ name: 'cm_hl_text', path: '/api/cards/highlight', headers: { ...authHeaders, 'content-type': 'text/plain' }, method: 'POST', etagReplay: false,
    body: 'q=goblin' });
  cases.push({ name: 'cm_hl_noauth', path: '/api/cards/highlight', headers: { 'content-type': 'application/json' }, method: 'POST', etagReplay: false,
    body: '{}' });
  cases.push({ name: 'cm_sug', path: '/api/suggest?q=lig', headers: authHeaders });
  cases.push({ name: 'cm_sug_upper', path: '/api/suggest?q=Goblin', headers: authHeaders });
  cases.push({ name: 'cm_sug_short', path: '/api/suggest?q=a', headers: authHeaders });
  cases.push({ name: 'cm_sug_noauth', path: '/api/suggest?q=lig', headers: {} });
  cases.push({ name: 'cm_cotd', path: '/api/stats/commander-of-the-day?day=2026-11-12', headers: authHeaders });
  cases.push({ name: 'cm_cotd_arr', path: '/api/stats/commander-of-the-day?day[]=x', headers: authHeaders });

  // --- iter-40: admin users group (RUST_ROUTES=admin_users;
  // GET/POST /api/admin/users, PUT/DELETE /api/admin/users/:id;
  // backend/src/routes/admin.js:122-272). Every probe row (users, sessions,
  // decks) is dbWrite-owned with a fixed created_at, so both sides see an
  // identical users table at request time (setup runs per side right before
  // the request; cases run serially). The bench session (authHeaders) is
  // 'brenttime' — role admin — so the admin arms ride it. Write arms carry
  // write->read verification on the OPPOSITE server (rule 2); cleanup
  // deletes exactly the probe rows (rule 4). Gate arms: no-token/bad-token
  // 401, member 403, api-key-on-admin-GET 403, api-key write 403 read-only.
  const AU_PW = 'paritypass123';
  const AU_KEY = 'bnd'.concat('_', 'a'.repeat(48)); // bnd_ + 48 hex-ish; distinct from FIXED_KEY
  const AU_T0 = '2024-01-01 00:00:00';   // fixed created_at (cross-side determinism)
  const AU_RO_TOK = 'rusttest-au-ro'.concat('-tok');
  const AU_RO_HDR = 'Bearer '.concat(AU_RO_TOK);
  const AU_BADTOK = 'Bearer '.concat('dead'.repeat(4));
  function auPurge(...names) {
    dbWrite((d) => {
      for (const name of names) {
        const row = d.prepare('SELECT id FROM users WHERE username = ?').get(name);
        if (!row) continue;
        d.prepare('DELETE FROM sessions WHERE user_id = ?').run(row.id);
        d.prepare('DELETE FROM decks WHERE user_id = ?').run(row.id);
        d.prepare('DELETE FROM collection WHERE user_id = ?').run(row.id);
        d.prepare('DELETE FROM users WHERE id = ?').run(row.id);
      }
    });
  }
  function auSeed(name, role, { apiKey = null, id = null } = {}) {
    auPurge(name);
    dbWrite((d) => {
      if (id !== null) {
        // Explicit id (own 9313xx block): both sides must see the SAME id in
        // the GET list body and in readBack `seen`, so probes cannot ride
        // AUTOINCREMENT (it differs per side by construction).
        d.prepare('DELETE FROM sessions WHERE user_id = ?').run(id);
        d.prepare('DELETE FROM decks WHERE user_id = ?').run(id);
        d.prepare('DELETE FROM collection WHERE user_id = ?').run(id);
        d.prepare('DELETE FROM users WHERE id = ?').run(id);
        d.prepare('INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, created_at, api_key) VALUES (?, ?, ?, ?, ?, 0, ?, ?)')
          .run(id, name, '210000:00000000000000000000000000000000:00', role, `rusttest-${name}-shr`, AU_T0, apiKey);
        return;
      }
      d.prepare('INSERT INTO users (username, password_hash, role, share_token, share_enabled, created_at, api_key) VALUES (?, ?, ?, ?, 0, ?, ?)')
        .run(name, '210000:00000000000000000000000000000000:00', role, `rusttest-${name}-shr`, AU_T0, apiKey);
    });
  }
  function auSession(name, tok) {
    dbWrite((d) => {
      d.prepare('DELETE FROM sessions WHERE token = ?').run(tok);
      const row = d.prepare('SELECT id FROM users WHERE username = ?').get(name);
      if (!row) throw new Error(`auSession: no user ${name}`);
      d.prepare('INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)').run(tok, row.id, SESSION_EXP);
    });
  }
  const auId = (name) => dbWrite((d) => d.prepare('SELECT id FROM users WHERE username = ?').get(name)?.id);
  const auList = (base, username) => apiReq(base, 'GET', '/api/admin/users', authHeaders)
    .then((r) => ({ ...r, user: Array.isArray(r.json) ? r.json.find((u) => u.username === username) : undefined }));
  const AU_JSONH = { ...authHeaders, 'content-type': 'application/json' };

  cases.push({ name: 'au_list', path: '/api/admin/users', headers: authHeaders, method: 'GET',
    setup: async () => { auSeed('rusttest_au_member', 'member', { id: 931301 }); },
    teardown: async () => { auPurge('rusttest_au_member'); } });
  cases.push({ name: 'au_list_noauth', path: '/api/admin/users', headers: {}, method: 'GET' });
  cases.push({ name: 'au_list_badtoken', path: '/api/admin/users', headers: { Authorization: AU_BADTOK }, method: 'GET' });
  cases.push({ name: 'au_list_member', path: '/api/admin/users', headers: {}, method: 'GET',
    setup: async () => { auSeed('rusttest_au_ro', 'member'); auSession('rusttest_au_ro', AU_RO_TOK); return { headers: { Authorization: AU_RO_HDR } }; },
    teardown: async () => { auPurge('rusttest_au_ro'); } });
  cases.push({ name: 'au_list_apikey', path: '/api/admin/users', headers: {}, method: 'GET',
    setup: async () => { auSeed('rusttest_au_key', 'member', { apiKey: AU_KEY }); return { headers: { Authorization: `Bearer ${AU_KEY}` } }; },
    teardown: async () => { auPurge('rusttest_au_key'); } });

  cases.push({ name: 'au_post_ok', path: '/api/admin/users', headers: AU_JSONH, method: 'POST',
    body: JSON.stringify({ username: ' RustTest_Au_Guy ', password: AU_PW }),
    setup: async () => { auPurge('rusttest_au_guy'); },
    readBack: async (other) => {
      const r = await auList(other, 'rusttest_au_guy');
      const u = r.user;
      const view = u ? { username: u.username, role: u.role, share_enabled: u.share_enabled, total_cards: u.total_cards, total_value: u.total_value } : null;
      return view && view.role === 'member' && view.share_enabled === 0 && view.total_cards === 0 && view.total_value === 0
        ? { ok: true, why: 'created user visible on the other server with destructure-default shape', seen: view }
        : { ok: false, why: `created user missing/wrong on other server: ${r.status} ${fmt(u)}`, seen: view };
    },
    teardown: async () => { auPurge('rusttest_au_guy'); } });
  cases.push({ name: 'au_post_required', path: '/api/admin/users', headers: AU_JSONH, method: 'POST', body: '{}' });
  cases.push({ name: 'au_post_shortpw', path: '/api/admin/users', headers: AU_JSONH, method: 'POST',
    body: JSON.stringify({ username: 'rusttest_au_x', password: 'short' }) });
  cases.push({ name: 'au_post_badrole', path: '/api/admin/users', headers: AU_JSONH, method: 'POST',
    body: JSON.stringify({ username: 'rusttest_au_x', password: AU_PW, role: 'owner' }) });
  cases.push({ name: 'au_post_dup', path: '/api/admin/users', headers: AU_JSONH, method: 'POST',
    body: JSON.stringify({ username: ' RustTest_Au_Dup ', password: AU_PW }),
    setup: async () => { auSeed('rusttest_au_dup', 'member'); },
    teardown: async () => { auPurge('rusttest_au_dup'); } });
  cases.push({ name: 'au_post_apikey', path: '/api/admin/users', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: JSON.stringify({ username: 'rusttest_au_x', password: AU_PW }),
    setup: async () => { auSeed('rusttest_au_key2', 'member', { apiKey: AU_KEY }); return { headers: { Authorization: `Bearer ${AU_KEY}`, 'content-type': 'application/json' } }; },
    teardown: async () => { auPurge('rusttest_au_key2'); } });
  cases.push({ name: 'au_post_noauth', path: '/api/admin/users', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: JSON.stringify({ username: 'rusttest_au_x', password: AU_PW }) });

  cases.push({ name: 'au_put_role_ok', path: '/api/admin/users/0', headers: AU_JSONH, method: 'PUT',
    body: JSON.stringify({ role: 'admin' }),
    setup: async () => { auSeed('rusttest_au_edit', 'member', { id: 931302 }); const id = auId('rusttest_au_edit'); return { path: `/api/admin/users/${id}`, ctx: { id } }; },
    readBack: async (other) => {
      const r = await auList(other, 'rusttest_au_edit');
      return r.user?.role === 'admin'
        ? { ok: true, why: 'role=admin visible on the other server', seen: r.user }
        : { ok: false, why: `role not persisted on other server: ${r.status} ${fmt(r.user)}`, seen: r.user ?? null };
    },
    teardown: async () => { auPurge('rusttest_au_edit'); } });
  cases.push({ name: 'au_put_pw_ok', path: '/api/admin/users/0', headers: AU_JSONH, method: 'PUT',
    body: JSON.stringify({ password: AU_PW }),
    setup: async () => { auSeed('rusttest_au_pw', 'member'); const id = auId('rusttest_au_pw'); return { path: `/api/admin/users/${id}`, ctx: { id } }; },
    readBack: async (other) => {
      // Cross-side password verification: the seeded hash is garbage, so a
      // successful login on the OTHER server proves the write landed and the
      // hash written by one server verifies on the other (Rust hash ->
      // Node-side check via Node's login path and vice versa).
      const login = await apiReq(other, 'POST', '/api/auth/login', { 'content-type': 'application/json' },
        JSON.stringify({ username: 'rusttest_au_pw', password: AU_PW }));
      return login.status === 200 && typeof login.json?.token === 'string'
        ? { ok: true, why: 'new password logs in on the other server', seen: { role: login.json?.user?.role } }
        : { ok: false, why: `login with new password failed on other server: ${login.status} ${fmt(login.text)}`, seen: null };
    },
    teardown: async () => { auPurge('rusttest_au_pw'); } });
  cases.push({ name: 'au_put_404', path: '/api/admin/users/931399', headers: AU_JSONH, method: 'PUT', body: '{}' });
  cases.push({ name: 'au_put_badrole', path: '/api/admin/users/0', headers: AU_JSONH, method: 'PUT',
    body: JSON.stringify({ role: 'owner' }),
    setup: async () => { auSeed('rusttest_au_brole', 'member'); const id = auId('rusttest_au_brole'); return { path: `/api/admin/users/${id}` }; },
    teardown: async () => { auPurge('rusttest_au_brole'); } });
  cases.push({ name: 'au_put_shortpw', path: '/api/admin/users/0', headers: AU_JSONH, method: 'PUT',
    body: JSON.stringify({ password: 'short' }),
    setup: async () => { auSeed('rusttest_au_spw', 'member'); const id = auId('rusttest_au_spw'); return { path: `/api/admin/users/${id}` }; },
    teardown: async () => { auPurge('rusttest_au_spw'); } });
  cases.push({ name: 'au_put_selfdemote', path: '/api/admin/users/0', headers: AU_JSONH, method: 'PUT',
    body: JSON.stringify({ role: 'member' }),
    setup: async () => {
      // The self-demotion gate answers 400 BEFORE any write on both sides.
      const id = dbWrite((d) => d.prepare("SELECT id FROM users WHERE username = 'brenttime'").get()?.id);
      if (!id) throw new Error('au_put_selfdemote: bench user missing');
      return { path: `/api/admin/users/${id}` };
    } });

  cases.push({ name: 'au_delete_ok', path: '/api/admin/users/0', headers: authHeaders, method: 'DELETE',
    setup: async () => { auSeed('rusttest_au_del', 'member'); const id = auId('rusttest_au_del'); return { path: `/api/admin/users/${id}`, ctx: { id } }; },
    readBack: async (other, ctx) => {
      const gone = dbWrite((d) => !d.prepare('SELECT id FROM users WHERE id = ?').get(ctx.id));
      const r = await auList(other, 'rusttest_au_del');
      return gone && !r.user
        ? { ok: true, why: 'user absent from DB and from the other server list', seen: { deleted: true } }
        : { ok: false, why: `user still present after delete: db=${!gone} other=${fmt(r.user)}`, seen: r.user ?? null };
    },
    teardown: async () => { auPurge('rusttest_au_del'); } });
  cases.push({ name: 'au_delete_409', path: '/api/admin/users/0', headers: authHeaders, method: 'DELETE',
    setup: async () => {
      auSeed('rusttest_au_co', 'member');
      const id = auId('rusttest_au_co');
      dbWrite((d) => d.prepare('INSERT INTO decks (user_id, name, checked_out) VALUES (?, ?, 1)').run(id, 'rusttest au loan deck'));
      return { path: `/api/admin/users/${id}` };
    },
    teardown: async () => { auPurge('rusttest_au_co'); } });
  cases.push({ name: 'au_delete_404', path: '/api/admin/users/931398', headers: authHeaders, method: 'DELETE' });
  cases.push({ name: 'au_delete_self', path: '/api/admin/users/0', headers: authHeaders, method: 'DELETE',
    setup: async () => {
      const id = dbWrite((d) => d.prepare("SELECT id FROM users WHERE username = 'brenttime'").get()?.id);
      if (!id) throw new Error('au_delete_self: bench user missing');
      return { path: `/api/admin/users/${id}` };
    } });
  cases.push({ name: 'au_delete_noauth', path: '/api/admin/users/1', headers: {}, method: 'DELETE' });

  // --- iter-41: MARKETPLACE slice 1 (RUST_ROUTES=marketplace;
  // GET/PUT /api/marketplace/accounts, GET /api/marketplace/recent/:source;
  // backend/src/routes/marketplaceOrders.js + utils/marketplaceOrders.js).
  // The provider legs (list/detail against manapool.com) are excluded from
  // parity per the moxfield-leg ruling — proven here against the injected
  // stub client in crates/scrybox-server/tests/marketplace.rs. Parity covers
  // the masked account view, every PUT arm (clear / enabled-only /
  // validation order / gates / body-parser-before-auth), and the recent
  // gates. Probe rows live in the fixed 9314xx block with fixed manapool
  // values so both sides render the same masked body; write arms read back
  // on the OPPOSITE server.
  const MP_UID = 931401, MP_OFF_UID = 931402, MP_NONE_UID = 931403;
  const MP_TOKV = 'rusttest-mp'.concat('-tok');
  const MP_HDR = { Authorization: 'Bearer '.concat(MP_TOKV) };
  const MP_JSONH = { Authorization: 'Bearer '.concat(MP_TOKV), 'content-type': 'application/json' };
  const MP_SEED_EMAIL = 'mp.probe@example.com';
  const MP_SEED_TOKEN = 'probeTok1234567';
  const MP_SEED_SAVED = '2026-01-02T03:04:05.000Z';
  function mpPurge() {
    dbWrite((d) => {
      for (const id of [MP_UID, MP_OFF_UID, MP_NONE_UID]) {
        d.prepare('DELETE FROM sessions WHERE user_id = ?').run(id);
        d.prepare('DELETE FROM users WHERE id = ?').run(id);
      }
    });
  }
  // id: probe row. configured=true seeds email/token/savedAt (+enabled),
  // configured=false leaves the manapool columns NULL.
  function mpSeed(id, name, { configured = true, enabled = 1 } = {}) {
    dbWrite((d) => {
      d.prepare('DELETE FROM sessions WHERE user_id = ?').run(id);
      d.prepare('DELETE FROM users WHERE id = ?').run(id);
      d.prepare('INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, created_at, manapool_email, manapool_token, manapool_enabled, manapool_saved_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?)')
        .run(id, name, '210000:00000000000000000000000000000000:00', 'member', `rusttest-${name}-shr`, '2024-01-01 00:00:00',
          configured ? MP_SEED_EMAIL : null, configured ? MP_SEED_TOKEN : null, enabled, configured ? MP_SEED_SAVED : null);
      d.prepare('INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)').run(MP_TOKV, id, SESSION_EXP);
    });
  }
  const mpGet = (base) => apiReq(base, 'GET', '/api/marketplace/accounts', MP_HDR);
  const mpView = (j) => j && j.manapool ? { configured: j.manapool.configured, enabled: j.manapool.enabled, email: j.manapool.email, token: j.manapool.token, savedAt: j.manapool.savedAt } : null;

  cases.push({ name: 'mp_accounts_ok', path: '/api/marketplace/accounts', headers: MP_HDR, method: 'GET',
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_accounts_unconfigured', path: '/api/marketplace/accounts', headers: MP_HDR, method: 'GET',
    setup: async () => { mpSeed(MP_NONE_UID, 'rusttest_mp_none', { configured: false }); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_accounts_noauth', path: '/api/marketplace/accounts', headers: {}, method: 'GET' });
  cases.push({ name: 'mp_accounts_badtoken', path: '/api/marketplace/accounts', headers: { Authorization: 'Bearer '.concat('dead'.repeat(4)) }, method: 'GET' });

  // savedAt is new Date().toISOString() stamped by the WRITING server at PUT
  // time (Node: marketplaceOrders.js accountView/save; Rust: marketplace.rs
  // twin). Each side stamps its own wall clock, so the two bodies can never
  // agree on it byte-for-byte — scrub it like the manabox sync header
  // (scrubStrings precedent). Its SHAPE is still pinned: it must be a string
  // on both sides (the scrub only rewrites strings) and readBack gates on
  // typeof savedAt === 'string' before the row is accepted.
  cases.push({ name: 'mp_put_save_ok', path: '/api/marketplace/accounts', headers: MP_JSONH, method: 'PUT',
    body: JSON.stringify({ manapool: { email: ' fresh.probe@example.com ', token: 'freshProbeTok9999', enabled: true } }),
    scrub: [['$.accounts.manapool.savedAt', '.*', '<wallclock>']],
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1', { configured: false }); },
    readBack: async (other) => {
      const r = await mpGet(other);
      const v = mpView(r.json);
      if (!v || v.configured !== true || v.enabled !== true || v.email !== 'fr••••••••@example.com' || v.token !== 'fre\u{2026}9999 (17 chars)' || typeof v.savedAt !== 'string') {
        return { ok: false, why: `saved credential missing/wrong on other server: ${r.status} ${fmt(r.json)}`, seen: v };
      }
      const { savedAt, ...persisted } = v; // wall-clock: gated above, excluded from the cross-side fold
      return { ok: true, why: 'saved credential visible masked on the other server', seen: persisted };
    },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_put_clear_ok', path: '/api/marketplace/accounts', headers: MP_JSONH, method: 'PUT',
    body: JSON.stringify({ manapool: { clear: true } }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    readBack: async (other) => {
      const r = await mpGet(other);
      const v = mpView(r.json);
      return v && v.configured === false && v.enabled === false && v.email === '' && v.savedAt === null
        ? { ok: true, why: 'clear landed: unconfigured on the other server', seen: v }
        : { ok: false, why: `clear not persisted on other server: ${r.status} ${fmt(r.json)}`, seen: v };
    },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_put_enabled_only', path: '/api/marketplace/accounts', headers: MP_JSONH, method: 'PUT',
    body: JSON.stringify({ manapool: { enabled: false } }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    readBack: async (other) => {
      const r = await mpGet(other);
      const v = mpView(r.json);
      return v && v.configured === true && v.enabled === false && v.savedAt === MP_SEED_SAVED
        ? { ok: true, why: 'enabled-only kept the credential and savedAt on the other server', seen: v }
        : { ok: false, why: `enabled-only changed the credential on other server: ${r.status} ${fmt(r.json)}`, seen: v };
    },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_put_nothing', path: '/api/marketplace/accounts', headers: MP_JSONH, method: 'PUT', body: '{}',
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_put_bademail', path: '/api/marketplace/accounts', headers: MP_JSONH, method: 'PUT',
    body: JSON.stringify({ manapool: { email: 'a@b', token: MP_SEED_TOKEN } }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_put_token_required', path: '/api/marketplace/accounts', headers: MP_JSONH, method: 'PUT',
    body: JSON.stringify({ manapool: { email: 'ok@ex.com', token: '  ' } }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_put_noauth', path: '/api/marketplace/accounts', headers: { 'content-type': 'application/json' }, method: 'PUT',
    body: JSON.stringify({ manapool: { clear: true } }) });
  cases.push({ name: 'mp_put_badjson_preauth', path: '/api/marketplace/accounts', headers: { 'content-type': 'application/json' }, method: 'PUT',
    body: '{oops' });
  cases.push({ name: 'mp_put_apikey_readonly', path: '/api/marketplace/accounts', headers: { Authorization: 'Bearer '.concat(FIXED_KEY), 'content-type': 'application/json' }, method: 'PUT',
    body: JSON.stringify({ manapool: { clear: true } }) });

  cases.push({ name: 'mp_recent_noauth', path: '/api/marketplace/recent/bogus', headers: {}, method: 'GET' });
  cases.push({ name: 'mp_recent_badtoken', path: '/api/marketplace/recent/manapool', headers: { Authorization: 'Bearer '.concat('dead'.repeat(4)) }, method: 'GET' });
  cases.push({ name: 'mp_recent_badsource', path: '/api/marketplace/recent/TCGPlayer', headers: MP_HDR, method: 'GET',
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_recent_nocreds', path: '/api/marketplace/recent/manapool', headers: MP_HDR, method: 'GET',
    setup: async () => { mpSeed(MP_NONE_UID, 'rusttest_mp_none', { configured: false }); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_recent_disabled', path: '/api/marketplace/recent/manapool', headers: MP_HDR, method: 'GET',
    setup: async () => { mpSeed(MP_OFF_UID, 'rusttest_mp_off', { configured: true, enabled: 0 }); },
    teardown: async () => { mpPurge(); } });

  // --- iter-4 (lane A): MARKETPLACE slice 2 — POST /preview + POST /add
  // (RUST_ROUTES=marketplace; routes/marketplaceOrders.js orderFromRequest
  // gate ladder). The provider fetch leg (fetchManapoolOrder against
  // manapool.com) is excluded from parity per the moxfield-leg ruling —
  // Node reads MANAPOOL_API_BASE once at module load and lane A must not
  // recreate the Node container, so both sides cannot be aimed at a stub.
  // Provider arms (fetch error status/shape, parse arms, previewOrder/
  // addOrderToCollection resolution + DB writes) are stub-proven in
  // crates/scrybox-server/tests/marketplace.rs (injected mp_http). Parity
  // covers every pre-fetch arm, byte-for-byte, on both servers: auth,
  // body-parser-before-auth, API-key GET-only, and the orderFromRequest
  // ladder source -> order_number -> creds (the row-404 arm is unreachable
  // through auth's sessions JOIN users on either server — DB-race only).
  const MP_ORDER_BODY = (extra) => JSON.stringify({ source: 'ManaPool', order_number: 'MP-12345', ...extra });
  cases.push({ name: 'mp_preview_noauth', path: '/api/marketplace/preview', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: MP_ORDER_BODY() });
  cases.push({ name: 'mp_preview_badtoken', path: '/api/marketplace/preview', headers: { Authorization: 'Bearer '.concat('dead'.repeat(4)), 'content-type': 'application/json' }, method: 'POST',
    body: MP_ORDER_BODY() });
  cases.push({ name: 'mp_preview_apikey_readonly', path: '/api/marketplace/preview', headers: { Authorization: 'Bearer '.concat(FIXED_KEY), 'content-type': 'application/json' }, method: 'POST',
    body: MP_ORDER_BODY() });
  cases.push({ name: 'mp_preview_badjson_preauth', path: '/api/marketplace/preview', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: '{oops' });
  cases.push({ name: 'mp_preview_empty_body', path: '/api/marketplace/preview', headers: MP_JSONH, method: 'POST', body: '{}',
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_preview_badsource', path: '/api/marketplace/preview', headers: MP_JSONH, method: 'POST',
    body: JSON.stringify({ source: 'TCGPlayer', order_number: 'MP-12345' }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_preview_no_ordernum', path: '/api/marketplace/preview', headers: MP_JSONH, method: 'POST',
    body: JSON.stringify({ source: 'manapool' }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_preview_bad_ordernum', path: '/api/marketplace/preview', headers: MP_JSONH, method: 'POST',
    body: JSON.stringify({ source: 'manapool', order_number: 'bad number!' }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_preview_nocreds', path: '/api/marketplace/preview', headers: MP_JSONH, method: 'POST',
    body: MP_ORDER_BODY(),
    setup: async () => { mpSeed(MP_NONE_UID, 'rusttest_mp_none', { configured: false }); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_preview_disabled', path: '/api/marketplace/preview', headers: MP_JSONH, method: 'POST',
    body: MP_ORDER_BODY(),
    setup: async () => { mpSeed(MP_OFF_UID, 'rusttest_mp_off', { configured: true, enabled: 0 }); },
    teardown: async () => { mpPurge(); } });

  cases.push({ name: 'mp_add_noauth', path: '/api/marketplace/add', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: MP_ORDER_BODY() });
  cases.push({ name: 'mp_add_badtoken', path: '/api/marketplace/add', headers: { Authorization: 'Bearer '.concat('dead'.repeat(4)), 'content-type': 'application/json' }, method: 'POST',
    body: MP_ORDER_BODY() });
  cases.push({ name: 'mp_add_apikey_readonly', path: '/api/marketplace/add', headers: { Authorization: 'Bearer '.concat(FIXED_KEY), 'content-type': 'application/json' }, method: 'POST',
    body: MP_ORDER_BODY() });
  cases.push({ name: 'mp_add_badjson_preauth', path: '/api/marketplace/add', headers: { 'content-type': 'application/json' }, method: 'POST',
    body: '{oops' });
  cases.push({ name: 'mp_add_badsource', path: '/api/marketplace/add', headers: MP_JSONH, method: 'POST',
    body: JSON.stringify({ source: 'ebay', order_number: 'MP-12345' }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_add_bad_ordernum', path: '/api/marketplace/add', headers: MP_JSONH, method: 'POST',
    body: JSON.stringify({ source: 'manapool', order_number: 'x'.repeat(65) }),
    setup: async () => { mpSeed(MP_UID, 'rusttest_mp_1'); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_add_nocreds', path: '/api/marketplace/add', headers: MP_JSONH, method: 'POST',
    body: JSON.stringify({ source: 'manapool', order_number: 'MP-12345', condition: 'NM', printing_mode: 'auto', language: 'English', copies: 1 }),
    setup: async () => { mpSeed(MP_NONE_UID, 'rusttest_mp_none', { configured: false }); },
    teardown: async () => { mpPurge(); } });
  cases.push({ name: 'mp_add_disabled', path: '/api/marketplace/add', headers: MP_JSONH, method: 'POST',
    body: MP_ORDER_BODY(),
    setup: async () => { mpSeed(MP_OFF_UID, 'rusttest_mp_off', { configured: true, enabled: 0 }); },
    teardown: async () => { mpPurge(); } });
}

// --- LANE-B iteration 12: POST /api/admin/seed-cards (admin.js dev seed) ----
// The write targets the CALLER's own collection, so the write case runs as a
// dedicated admin user (own 9313xx id block + fixed session token), never as
// the bench admin — wiping brenttime's seeded rows would poison every
// downstream case on the shared DB copy. The response message carries
// Σ-quantity (random, ∈ [208,416]) → stripped; the readBack re-pins the
// message SHAPE (from this side's own json), the exact 208-row count, the
// randomEntry value domains, the seed-set purge and the foreign-row survival
// — all booleans, so the cross-side `seen` fold is deterministic despite the
// per-side RNG. Both legs ride the SAME scryfall-local snapshot as the
// search parity (restart-rust-b.sh pins LOCAL_SCRYFALL_URL identically).
{
  const SEED_UID = 931391;
  const SEED_TOK = 'rusttest-seed-session-4f2a91c6b8d3e70519ac82fd6e0b3c7415d9af2e6c8b041f7a2d5e9c3b6f8012';
  const SEED_T0 = '2024-01-01 00:00:00';
  function seedPurge() {
    dbWrite((d) => {
      d.prepare('DELETE FROM sessions WHERE token = ?').run(SEED_TOK);
      d.prepare('DELETE FROM collection WHERE user_id = ?').run(SEED_UID);
      d.prepare('DELETE FROM users WHERE id = ?').run(SEED_UID);
    });
  }
  function seedPrep() {
    seedPurge();
    return dbWrite((d) => {
      d.prepare('INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, created_at) VALUES (?, ?, ?, ?, ?, 0, ?)')
        .run(SEED_UID, 'rusttest_seed_admin', '210000:00000000000000000000000000000000:00', 'admin', 'rusttest-seed-shr', SEED_T0);
      d.prepare('INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)').run(SEED_TOK, SEED_UID, SESSION_EXP);
      const lea = d.prepare("SELECT id FROM card_cache WHERE set_id = 'lea' ORDER BY id LIMIT 1").get();
      const mh3 = d.prepare("SELECT id FROM card_cache WHERE set_id = 'mh3' ORDER BY id LIMIT 1").get();
      const other = d.prepare("SELECT id FROM card_cache WHERE set_id NOT IN ('lea','mh3') AND set_id IS NOT NULL ORDER BY id LIMIT 1").get();
      if (!lea || !mh3 || !other) return null;
      // condition='Damaged' is legal in the schema but NEVER produced by
      // the seed — it marks the pre-seeded rows so the purge/survival
      // checks survive a random re-draw of the very same card ids.
      d.prepare("INSERT INTO collection (card_id, quantity, user_id, condition) VALUES (?, 2, ?, 'Damaged')").run(lea.id, SEED_UID);
      d.prepare("INSERT INTO collection (card_id, quantity, user_id, condition) VALUES (?, 3, ?, 'Damaged')").run(mh3.id, SEED_UID);
      d.prepare("INSERT INTO collection (card_id, quantity, user_id, condition) VALUES (?, 1, ?, 'Damaged')").run(other.id, SEED_UID);
      return { leaId: lea.id, mh3Id: mh3.id, otherId: other.id };
    });
  }
  const SEED_HDR = () => ({ Authorization: `Bearer ${SEED_TOK}`, 'content-type': 'application/json' });
  const MSG_RE = /^Successfully seeded a large test collection: \d+ cards for admin user\.$/;

  cases.push({
    name: 'seed_ok', path: '/api/admin/seed-cards', headers: SEED_HDR(), method: 'POST', body: '{}',
    strip: ['$.message'],
    setup: async () => {
      const ctx = seedPrep();
      if (!ctx) throw new Error('parity DB copy carries no lea/mh3/other card_cache rows — reseed from prod');
      return { ctx };
    },
    readBack: async (other, ctx, { json, status }) => {
      const v = dbWrite((d) => {
        // Fresh rows only: everything the seed wrote carries a
        // non-Damaged condition (see seedPrep's marker).
        const agg = d.prepare("SELECT COUNT(*) AS rows, SUM(quantity) AS qty FROM collection WHERE user_id = ? AND condition != 'Damaged'").get(SEED_UID);
        const bad = d.prepare(
          `SELECT COUNT(*) AS n FROM collection WHERE user_id = ? AND condition != 'Damaged' AND (
             quantity NOT IN (1,2)
             OR condition NOT IN ('Near Mint','Lightly Played','Moderately Played','Heavily Played')
             OR printing NOT IN ('Normal','Holofoil')
             OR language NOT IN ('English','Japanese')
             OR purchase_price IS NULL OR purchase_price < 0 OR purchase_price > 10
             OR ABS(ROUND(purchase_price * 100) - purchase_price * 100) > 1e-6
           )`).get(SEED_UID).n;
        const purged = d.prepare(
          "SELECT COUNT(*) AS n FROM collection WHERE user_id = ? AND condition = 'Damaged' AND card_id IN (?, ?)").get(SEED_UID, ctx.leaId, ctx.mh3Id).n;
        const keep = d.prepare("SELECT quantity AS q FROM collection WHERE user_id = ? AND card_id = ? AND condition = 'Damaged'").get(SEED_UID, ctx.otherId);
        return { rows: agg.rows, qty: agg.qty, bad, purged, keepQ: keep ? keep.q : null };
      });
      const added = (() => {
        const m = String(json?.message || '').match(/collection: (\d+) cards/);
        return m ? Number(m[1]) : -1;
      })();
      const ok = status === 200 && MSG_RE.test(String(json?.message || ''))
        && added >= 208 && added <= 416 && added === v.qty
        && v.rows === 208 && v.bad === 0 && v.purged === 0 && v.keepQ === 1;
      return {
        ok,
        why: `rows=${v.rows} Σqty=${v.qty} msgAdded=${added} bad=${v.bad} purged=${v.purged} keepQ=${v.keepQ}`,
        seen: { rows: v.rows, bad: v.bad, purged: v.purged, keepQ: v.keepQ, qtyInRange: v.qty >= 208 && v.qty <= 416 },
      };
    },
    teardown: async () => { seedPurge(); },
  });
  // Gate ladder (deterministic, no writes reach seed()).
  cases.push({ name: 'seed_no_token', path: '/api/admin/seed-cards', headers: { 'content-type': 'application/json' }, method: 'POST', body: '{}' });
  cases.push({ name: 'seed_bad_token', path: '/api/admin/seed-cards', headers: { Authorization: 'Bearer '.concat('dead'.repeat(4)), 'content-type': 'application/json' }, method: 'POST', body: '{}' });
  cases.push({ name: 'seed_member', path: '/api/admin/seed-cards', headers: { Authorization: `Bearer ${MEMBER_SESSION}`, 'content-type': 'application/json' }, method: 'POST', body: '{}' });
  cases.push({ name: 'seed_apikey_readonly', path: '/api/admin/seed-cards', headers: { Authorization: 'Bearer '.concat(FIXED_KEY), 'content-type': 'application/json' }, method: 'POST', body: '{}' });
  // Body-parser-before-auth and the POST-only router: both answers belong to
  // Node (Rust Forwards), so this pins the forward branches.
  cases.push({ name: 'seed_badjson_preauth', path: '/api/admin/seed-cards', headers: { 'content-type': 'application/json' }, method: 'POST', body: '{oops' });
  cases.push({ name: 'seed_get_forwards', path: '/api/admin/seed-cards', headers: { Authorization: `Bearer ${SEED_TOK}` }, method: 'GET',
    setup: async () => { const ctx = seedPrep(); return { ctx }; },
    teardown: async () => { seedPurge(); } });
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
