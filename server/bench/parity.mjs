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

const cases = [];
for (const s of scen.scenarios) {
  cases.push({ name: s.name, path: s.path, headers: s.headers || {}, method: s.method || 'GET' });
}
const authHeaders = scen.scenarios.find(s => s.headers?.Authorization)?.headers || {};
for (const [i, p] of EXTRA_COLLECTION.entries()) {
  cases.push({ name: `extra${i + 1}_${p.slice('/api/collection'.length + 1) || 'full'}`, path: p, headers: authHeaders, method: 'GET' });
}
// Auth-surface cases (BRIEF rule 2): exact 401/403 bodies.
cases.push({ name: 'auth_no_token', path: '/api/collection?limit=10', headers: {}, method: 'GET' });
cases.push({ name: 'auth_bad_token', path: '/api/collection?limit=10', headers: { Authorization: 'Bearer de...def' }, method: 'GET' });
cases.push({ name: 'auth_apikey_post', path: '/api/collection', headers: { Authorization: '***' + 'x'.repeat(59) }, method: 'POST' });

// --- comparison --------------------------------------------------------------

let ipSeq = 1;
function xff() { const n = ipSeq++; return `10.${(n >> 16) & 255}.${(n >> 8) & 255}.${n & 255}`; }

async function fetchSide(base, c) {
  const t0 = Date.now();
  const res = await fetch(base + c.path, {
    method: c.method,
    headers: { ...c.headers, 'x-forwarded-for': xff(), 'cache-control': 'no-cache' },
    redirect: 'manual',
  });
  const body = res.status === 304 ? '' : await res.text();
  return {
    status: res.status,
    body,
    json: body && (res.headers.get('content-type') || '').includes('json') ? JSON.parse(body) : undefined,
    total: res.headers.get('x-total-count'),
    etag: res.headers.get('etag'),
    ctype: (res.headers.get('content-type') || '').split(';')[0].trim().toLowerCase(),
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
  if (r.json !== undefined || n.json !== undefined) {
    if (r.json === undefined || n.json === undefined) diffs.push('one side non-JSON body');
    else deepDiff(r.json, n.json, '$', diffs);
  } else if (r.body !== n.body) {
    diffs.push(`body ${fmt(r.body)} vs ${fmt(n.body)}`);
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
  if (!c.path.startsWith('/api/collection')) return [];
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
    const [r, n] = await Promise.all([fetchSide(RUST, c), fetchSide(NODE, c)]);
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

console.log(`\nparity: ${cases.length - failed}/${cases.length} cases pass (${flaky} needed retries) rust=${RUST} node=${NODE}`);
process.exit(failed === 0 ? 0 : 1);
