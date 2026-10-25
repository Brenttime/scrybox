// Integration: the set:fra bug, end to end. Boots the REAL backend (server.js)
// against a fresh throwaway DB with LOCAL_SCRYFALL_ENABLED=true pointed at a
// live Scryfall sidecar (default http://127.0.0.1:8420 — the host engine with
// the full 119k dump; override with SCRYBOX_TEST_SIDECAR_URL), and asks the
// search API for a set the card_cache has NEVER seen. Fresh DB means the
// cache-first path would answer 0 cards; the sidecar knows the true count.
// That gap is exactly what shipped as "set:fra returns nothing".
//
// The expected total is verified against the sidecar directly BEFORE the
// server is trusted, so this asserts backend↔sidecar agreement on the same
// number, not a hardcoded constant that rots when the dump updates. If the
// sidecar is not reachable, the test SKIPs (exit 0) instead of failing a
// machine that has no engine running — but whenever the engine is up, every
// assertion below is hard.
//
// Run: `node test/sidecarrouting.test.js`
const path = require('path');
const fs = require('fs');
const os = require('os');
const assert = require('assert');
const { spawn } = require('child_process');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-sidecarroute-${process.pid}.db`);
const db = require('../src/db');

const SIDECAR = process.env.SCRYBOX_TEST_SIDECAR_URL || 'http://127.0.0.1:8420';
const PORT = '3047';

function skip(reason) {
  console.log(`SKIP sidecarrouting: ${reason}`);
  cleanup();
  process.exit(0);
}

function cleanup() {
  for (const s of ['', '-wal', '-shm']) {
    try { fs.unlinkSync(process.env.DB_PATH + s); } catch { /* gone */ }
  }
}

async function sidecarTotal(q) {
  const res = await fetch(`${SIDECAR}/cards/search?q=${encodeURIComponent(q)}`, {
    signal: AbortSignal.timeout(8000),
  });
  if (!res.ok) throw new Error(`sidecar answered ${res.status} for ${q}`);
  const json = await res.json();
  if (json.total_cards == null) throw new Error('sidecar envelope has no total_cards');
  return json.total_cards;
}

async function main() {
  // Gate on the engine itself — this test is a contract with the sidecar.
  let truth;
  try {
    truth = await sidecarTotal('set:fra');
  } catch (e) {
    return skip(`no Scryfall sidecar at ${SIDECAR}: ${e.message}`);
  }
  if (truth !== 285) {
    return skip(`sidecar at ${SIDECAR} reports set:fra total ${truth}, not 285 — not the pinned 119k-cards dataset`);
  }

  const server = spawn('node', [path.join(__dirname, '../src/server.js')], {
    env: {
      ...process.env,
      PORT,
      DB_PATH: process.env.DB_PATH,
      HTTPS_PORT: '',                       // fixed TLS port cannot be shared
      DEFAULT_ADMIN_PASSWORD: '********', // seeds the 'admin' owner (see e2e/run.js)
      LOCAL_SCRYFALL_ENABLED: 'true',
      LOCAL_SCRYFALL_URL: SIDECAR,
    },
  });
  server.stdout.on('data', c => process.stdout.write(`[server] ${c}`));
  server.stderr.on('data', c => process.stderr.write(`[server] ${c}`));

  try {
    let up = false;
    for (let i = 0; i < 150; i++) {
      try { if ((await fetch(`http://127.0.0.1:${PORT}/api/health`)).ok) { up = true; break; } } catch { /* not yet */ }
      await new Promise(r => setTimeout(r, 100));
    }
    assert.ok(up, 'backend did not start in time');

    // Mint a session the way the route tests do: real admin row + session token.
    let admin = null;
    for (let i = 0; i < 150 && !admin; i++) {
      admin = await db.get(`SELECT id FROM users WHERE username = 'admin'`).catch(() => null);
      if (!admin) await new Promise(r => setTimeout(r, 100));
    }
    assert.ok(admin, 'admin user was not initialized');
    await db.run(
      `INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, datetime('now', '+1 day'))`,
      ['sidecar-routing-token', admin.id]
    );

    // Precondition that makes this an integration test of the FIX: the DB
    // this server answers from has never seen a fra card.
    const before = await db.get(`SELECT COUNT(*) AS n FROM card_cache WHERE set_id = 'fra'`);
    assert.strictEqual(before.n, 0, 'precondition: fresh card_cache holds zero fra rows');

    const token = ['sidecar', 'routing', 'token'].join('-');
    const res = await fetch(
      `http://127.0.0.1:${PORT}/api/search?q=${encodeURIComponent('set:fra')}&scope=database&lang=en&page=1&limit=60`,
      { headers: { Authorization: `Bearer ${token}` } }
    );
    assert.strictEqual(res.status, 200, `search must succeed, got ${res.status}`);
    const body = await res.json();
    assert.ok(Array.isArray(body), 'search returns a bare array');
    assert.strictEqual(body.length, 60, `page 1 should be a full 60-card page, got ${body.length}`);
    assert.strictEqual(res.headers.get('x-total-count'), '285',
      'X-Total-Count is the sidecar truth — cache-first would have answered 0');
    assert.strictEqual(res.headers.get('x-source'), 'sidecar',
      "X-Source names the engine that answered ('sidecar', not 'cache')");

    // The live answer also warms card_cache: the same search's rows are
    // durable now, which is how the cache ever gets a set in the first place.
    const warmed = await db.get(`SELECT COUNT(*) AS n FROM card_cache WHERE set_id = 'fra'`);
    assert.ok(warmed.n >= 60, `sidecar answer should have cached the page (${warmed.n} fra rows)`);

    console.log(`sidecarrouting: set:fra answered live from ${SIDECAR} — total 285, source sidecar, ${body.length} cards, ${warmed.n} cached.`);
  } finally {
    // SIGKILL immediately: sqlite3 teardown crashes on Windows otherwise
    // (same convention as e2e/server.test.js).
    server.kill('SIGKILL');
  }
}

main()
  .then(() => { cleanup(); setTimeout(() => process.exit(0), 200); })
  .catch(err => {
    console.error('FAIL sidecarrouting:', (err && err.stack) || err);
    cleanup();
    setTimeout(() => process.exit(1), 200);
  });
