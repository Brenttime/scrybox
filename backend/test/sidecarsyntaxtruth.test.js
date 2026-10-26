// TRUTH test: sidecar-mode collection search answered by the REAL engine.
//
// Boots the actual backend (server.js) against a throwaway DB with
// LOCAL_SCRYFALL_ENABLED=true pointed at the standalone sidecar (default
// http://127.0.0.1:8420 — the repo-built engine over the full 119k dump;
// override with SCRYBOX_TEST_SIDECAR_URL), seeds a fixture user whose owned
// cards are REAL printings pulled from that engine, and then asks the
// collection search API the queries the card_cache compiler used to answer
// itself. Every expectation is computed FROM THE ENGINE at runtime (never a
// hardcoded constant that rots with the dump), so this asserts:
//
//   · set:fra              → the user's owned fra PRINTINGS (exact-id walk)
//   · color:u              → the same rows the pre-change SQL path returned
//                            (golden compare against rawQuerySql itself)
//   · is:land              → engine truth (is: → t: canonicalization included)
//   · rarity:r are:snow    → local-mode combo answered by the engine
//   · otag:ramp id:rb      → catalog-mode control (874-card guardrail query)
//
//   and for every one of them the server's SQL spy (fixtures/sqlspy-preload.js
//   via NODE_OPTIONS) proves rawQuerySql.compileCollectionQuery was NEVER
//   asked to answer — the engine is the sole syntax authority in sidecar mode.
//
// The test process also speaks to the engine directly over ordinary HTTP; a
// separate test asserts the server's SQL trace, so the engine contract is:
// healthz reports formats:23, guardrail otag:ramp id:rb totals 874, set:fra
// totals 285 (the pinned 119k dataset). No engine → SKIP (exit 0), same
// convention as sidecarrouting.test.js. Never touches :3002 or any prod
// volume: throwaway DB_PATH, own port, engine queried read-only.
//
// Run: `node test/sidecarsyntaxtruth.test.js`
const path = require('path');
const fs = require('fs');
const os = require('os');
const assert = require('assert');
const { spawn } = require('child_process');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-sidecartruth-${process.pid}.db`);
const db = require('../src/db');
const scryfallApi = require('../src/scryfallApi'); // upstream-mode instance: only normalizeCard/cacheCards used
const { analyze } = require('../../shared/scryfallQuery.js');
const rawQuerySql = require('../src/utils/rawQuerySql');

const SIDECAR = process.env.SCRYBOX_TEST_SIDECAR_URL || 'http://127.0.0.1:8420';
const PORT = '3049';
const SPY_FILE = path.join(os.tmpdir(), `scrybox-sqlspy-${process.pid}.jsonl`);
const PRELOAD = path.join(__dirname, 'fixtures', 'sqlspy-preload.js');

function skip(reason) {
  console.log(`SKIP sidecarsyntaxtruth: ${reason}`);
  cleanup();
  process.exit(0);
}

function cleanup() {
  for (const s of ['', '-wal', '-shm']) {
    try { fs.unlinkSync(process.env.DB_PATH + s); } catch { /* gone */ }
  }
  try { fs.unlinkSync(SPY_FILE); } catch { /* gone */ }
}

async function engineJson(p, params) {
  const url = new URL(p, SIDECAR);
  for (const [k, v] of Object.entries(params || {})) url.searchParams.set(k, String(v));
  const res = await fetch(url, { signal: AbortSignal.timeout(15000) });
  if (!res.ok) throw new Error(`engine answered ${res.status} for ${url.search}`);
  return res.json();
}

// The engine's OWN answer, walked to the end. This is the truth the API is
// compared against — the test pages the engine the same way the backend does.
async function engineAllPaged(q) {
  const cards = [];
  let page = 1;
  let total = null;
  for (;;) {
    const json = await engineJson('/cards/search', { q, page });
    if (json.total_cards != null) total = json.total_cards;
    cards.push(...(json.data || []));
    if (!json.has_more) break;
    if (++page > 300) throw new Error(`engine walk for "${q}" did not end within 300 pages`);
  }
  return { cards, total };
}

function spyLines(since) {
  if (!fs.existsSync(SPY_FILE)) return [];
  const all = fs.readFileSync(SPY_FILE, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l));
  return all.slice(since);
}
const spyCount = () => (fs.existsSync(SPY_FILE) ? fs.readFileSync(SPY_FILE, 'utf8').split('\n').filter(Boolean).length : 0);

async function main() {
  // ---- gate on the engine itself (contract, like sidecarrouting) ----------
  let health;
  try {
    health = await (await fetch(`${SIDECAR}/api/healthz`, { signal: AbortSignal.timeout(5000) })).json();
  } catch (e) {
    return skip(`no Scryfall sidecar at ${SIDECAR}: ${e.message}`);
  }
  if (health.formats !== 23) return skip(`sidecar reports formats:${health.formats}, not 23 — not the current engine`);
  const guardrail = await engineJson('/cards/search', { q: 'otag:ramp id:rb', limit: 1 });
  if (guardrail.total_cards !== 874) return skip(`guardrail otag:ramp id:rb totals ${guardrail.total_cards}, not 874 — not the pinned dataset`);
  const fraGate = await engineJson('/cards/search', { q: 'set:fra', limit: 1 });
  if (fraGate.total_cards !== 285) return skip(`set:fra totals ${fraGate.total_cards}, not 285 — not the pinned 119k dataset`);

  fs.writeFileSync(SPY_FILE, '');
  const server = spawn('node', [path.join(__dirname, '../src/server.js')], {
    env: {
      ...process.env,
      PORT,
      DB_PATH: process.env.DB_PATH,
      HTTPS_PORT: '',
      DEFAULT_ADMIN_PASSWORD: '********',
      LOCAL_SCRYFALL_ENABLED: 'true',
      LOCAL_SCRYFALL_URL: SIDECAR,
      NODE_OPTIONS: `${process.env.NODE_OPTIONS || ''} --require ${PRELOAD}`.trim(),
      SCRYBOX_SQLSPY_FILE: SPY_FILE,
    },
  });
  server.stdout.on('data', c => process.stdout.write(`[server] ${c}`));
  server.stderr.on('data', c => process.stderr.write(`[server] ${c}`));

  const token = 'sidecar-syntax-truth-token';
  try {
    let up = false;
    for (let i = 0; i < 200; i++) {
      try { if ((await fetch(`http://127.0.0.1:${PORT}/api/health`)).ok) { up = true; break; } } catch { /* not yet */ }
      await new Promise(r => setTimeout(r, 100));
    }
    assert.ok(up, 'backend did not start in time');

    let admin = null;
    for (let i = 0; i < 150 && !admin; i++) {
      admin = await db.get(`SELECT id FROM users WHERE username = 'admin'`).catch(() => null);
      if (!admin) await new Promise(r => setTimeout(r, 100));
    }
    assert.ok(admin, 'admin user was not initialized');
    const member = await db.run(
      `INSERT INTO users (username, password_hash, role, share_token) VALUES ('truthy', 'x', 'member', 'share-truthy')`
    );
    const userId = member.lastID;
    await db.run(
      `INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, datetime('now', '+1 day'))`,
      [token, userId]
    );

    // ---- seed: owned cards are REAL engine printings ----------------------
    // The cache rows come from the same normalizeCard the live sidecar path
    // writes, so the fixture is byte-faithful to a warmed cache.
    const plainName = /^[A-Za-z][A-Za-z0-9' .,&-]*$/;
    const seenIds = new Set();
    async function pick(q, count, filter) {
      const json = await engineJson('/cards/search', { q, limit: 100 });
      const picks = [];
      for (const raw of json.data || []) {
        if (picks.length >= count) break;
        if (seenIds.has(raw.id)) continue; // one collection row per card
        if (!filter || filter(raw)) { seenIds.add(raw.id); picks.push(raw); }
      }
      assert.ok(picks.length >= count, `engine q="${q}" yielded ${picks.length}/${count} usable cards`);
      return picks;
    }
    const ownedRaw = [
      ...(await pick('set:fra', 3)),                      // printing-level set query
      ...(await pick('color:u', 3, c => plainName.test(c.name) && !c.name.includes('//'))),
      ...(await pick('is:land', 2, c => /Land/.test(c.type_line || '') && plainName.test(c.name))),
      ...(await pick('rarity:r are:snow', 2)),             // local-mode combo incl. is:-family word
      ...(await pick("t:instant c:r", 1, c => plainName.test(c.name) && !c.name.includes('//'))), // color:u negative
    ];
    // The catalog control's owned rows, chosen from the guardrail's own answer.
    ownedRaw.push(...(await pick('otag:ramp id:rb', 2, c => plainName.test(c.name))));

    const normalized = ownedRaw.map(raw => scryfallApi.normalizeCard(raw, null));
    await scryfallApi.cacheCards(normalized);
    for (const n of normalized) {
      await db.run(`INSERT INTO collection (user_id, card_id, quantity) VALUES (?, ?, 1)`, [userId, n.id]);
    }
    const ownedIds = new Set(normalized.map(n => n.id));
    const ownedNames = new Set(normalized.map(n => String(n.name).toLowerCase()));

    // ---- the queries under test ------------------------------------------
    // Each expectation is the ENGINE's full answer ∩ owned, computed fresh.
    const api = async (q) => {
      const res = await fetch(
        `http://127.0.0.1:${PORT}/api/search?scope=collection&lang=en&page=1&limit=1000&q=${encodeURIComponent(q)}`,
        { headers: { Authorization: `Bearer ${token}` } }
      );
      return res;
    };
    const checked = [];
    async function ask(q, { byId = false, expectCache = 'resolved' } = {}) {
      const t0 = Date.now();
      const before = spyCount();
      const res = await api(q);
      assert.strictEqual(res.status, 200, `${q}: API must answer, got ${res.status}`);
      const rows = await res.json();
      const lines = spyLines(before);
      const elapsed = ((Date.now() - t0) / 1000).toFixed(2);
      // THE core assertion: in sidecar mode the local SQL compiler is never
      // asked for a syntax query — zero compile invocations, zero compiled
      // statements executed. (The owned-row intersect SQL is expected.)
      const compiles = lines.filter(l => l.t === 'compile');
      assert.deepStrictEqual(compiles, [], `${q}: rawQuerySql must not answer a sidecar-mode syntax query (${compiles.length} compile(s) executed)`);
      assert.strictEqual(res.headers.get('x-catalog-cache'), expectCache, `${q}: X-Catalog-Cache`);
      assert.strictEqual(res.headers.get('x-source'), 'sidecar', `${q}: X-Source names the engine that answered`);
      const truth = await engineAllPaged(q);
      const match = byId
        ? new Set(truth.cards.map(c => `mtg-${c.id}`).filter(id => ownedIds.has(id)))
        : (() => {
            const names = new Set(truth.cards.map(c => String(scryfallApi.normalizeCard(c, null).name).toLowerCase()));
            return new Set([...ownedNames].filter(n => names.has(n)));
          })();
      const got = byId
        ? new Set(rows.map(r => String(r.card_id)))
        : new Set(rows.map(r => String(r.name).toLowerCase()));
      assert.deepStrictEqual(got, match, `${q}: rows must be exactly (engine answer ∩ owned)`);
      assert.strictEqual(res.headers.get('x-total-count'), String(match.size), `${q}: X-Total-Count`);
      checked.push({ q, owned: match.size, engineTotal: truth.total, seconds: Number(elapsed) });
      return { rows, res };
    }

    // 1. The PR #107 bug, this time in collection search: fra printings.
    //    Printed rows exist in the cache (seeded above) — matching is still
    //    done by the ENGINE, by exact printing id.
    const fra = await ask('set:fra', { byId: true });
    assert.strictEqual(fra.rows.length, 3, 'the three owned fra printings come back');
    assert.ok(fra.rows.every(r => r.set_id === 'fra'), 'every fra row is an actual fra printing');
    assert.strictEqual(fra.res.headers.get('x-catalog-complete'), '1', 'a finished walk is complete');

    // 2. color:u — the golden compare. The pre-change local answer was this
    //    exact compiler against these exact rows; the engine must agree.
    const compiled = rawQuerySql.compileCollectionQuery({
      ast: analyze('color:u').ast, userId, limit: null, offset: 0, eligibleOnly: false,
    });
    const sqlRows = await db.all(compiled.sql, compiled.params);
    const golden = new Set(sqlRows.map(r => String(r.name).toLowerCase()));
    const blue = await ask('color:u');
    assert.ok(golden.size >= 3, 'the pre-change SQL path produced a non-trivial golden set');
    assert.deepStrictEqual(new Set(blue.rows.map(r => String(r.name).toLowerCase())), golden,
      'color:u now returns EXACTLY the rows the SQL path returned pre-change');

    // 3. is:land — local-mode supertype query via the engine (is: → t: canonical).
    await ask('is:land');

    // 4. rarity:r are:snow — a local-mode COMBO including an is:-family word.
    await ask('rarity:r are:snow');

    // 5. Catalog control: the guardrail query through the collection route.
    const ctl = await ask('otag:ramp id:rb');
    assert.strictEqual(ctl.res.headers.get('x-catalog-upstream-total'), '874',
      'the 874-card guardrail total rides through as X-Catalog-Upstream-Total');

    // 6. The durable walk cache engaged: the same query is served 'fresh'.
    await ask('is:land', { expectCache: 'fresh' });

    // ---- honest perf numbers ----------------------------------------------
    const summary = checked.map(c => `${c.q} (${c.engineTotal} engine cards → ${c.owned} owned rows) ${c.seconds}s`).join('; ');
    console.log(`sidecarsyntaxtruth: engine-authority answered ${checked.length} queries against ${SIDECAR}: ${summary}`);
    const slowest = checked.reduce((a, b) => (a.seconds > b.seconds ? a : b));
    console.log(`sidecarsyntaxtruth: slowest walk = ${slowest.q} at ${slowest.seconds}s (full pages, ms per page)`);
  } finally {
    server.kill('SIGKILL');
  }
}

main()
  .then(() => { cleanup(); setTimeout(() => process.exit(0), 200); })
  .catch(err => {
    console.error('FAIL sidecarsyntaxtruth:', (err && err.stack) || err);
    cleanup();
    setTimeout(() => process.exit(1), 200);
  });
