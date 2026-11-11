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
function dbWrite(fn) { const d = new DatabaseSync(AUTH_DB); try { fn(d); } finally { d.close(); } }
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
  if (c.teardown) { try { await c.teardown(base, ctx); } catch (e) { console.log(`  (teardown ${c.name}: ${e})`); } }
  return {
    status: res.status,
    body: resBody,
    json: parsed,
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

function compareCase(c, r, n) {
  const diffs = [];
  if (r.status !== n.status) diffs.push(`status ${r.status} vs ${n.status}`);
  if (r.ctype !== n.ctype) diffs.push(`content-type ${r.ctype} vs ${n.ctype}`);
  if ((r.total ?? undefined) !== (n.total ?? undefined)) diffs.push(`x-total-count ${r.total} vs ${n.total}`);
  if ((r.source ?? undefined) !== (n.source ?? undefined)) diffs.push(`x-source ${r.source} vs ${n.source}`);
  if (r.json !== undefined || n.json !== undefined) {
    if (r.json === undefined || n.json === undefined) diffs.push('one side non-JSON body');
    else deepDiff(stripPaths(r.json, c.strip), stripPaths(n.json, c.strip), '$', diffs);
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
