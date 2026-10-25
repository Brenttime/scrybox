// Routing decision matrix for Scryfall-syntax search under the two Scryfall
// modes. This pins the fix for the set:fra bug:
//
//   card_cache is a PARTIAL snapshot — an install that never fetched set:fra
//   holds zero fra rows — so "every operator is data-backed" never proved
//   "the cached rows are the whole answer". With LOCAL_SCRYFALL_ENABLED the
//   live path (the sidecar: ms latency, no rate limit) must answer every
//   database-scope raw query, and answerLocally() survives only as the
//   fallback for when the sidecar is unreachable. Upstream mode (flag off)
//   must stay byte-identical to the old cache-first behavior.
//
// LOCAL_SCRYFALL is a module-load constant (the axios baseURL is baked at
// require time), so each mode gets its own freshly-required instance of
// scryfallApi against one shared throwaway DB; a stubbed axios adapter is
// the fake engine and the call log. The sidecar stub deliberately answers
// with cards the cache could NOT have produced, so every assertion says
// which engine actually answered. Each simulated-outage case uses its own
// query string: scryPage caches successful pages for 5 minutes, so a
// REPEAT of a query that already succeeded would correctly never hit the
// network and would mask the outage under test.
//
// Run: `node test/localrawrouting.test.js`
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

process.env.SCRYFALL_GAP_SCALE = '0';
process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-localrawroute-${process.pid}.db`);
const db = require('../src/db');

const SIDECAR_HOST = 'sidecar.test';

// Seed rows in the exact shape rawquerylocal.test.js uses: the cache this
// install "has" — 5 English lea cards, and ZERO rows for any other set.
const SEED = [
  { id: 'mtg-seed-0001', name: 'Lightning Bolt', supertype: '', subtypes: ['Instant'], types: ['Red'], rarity: 'Common', set_id: 'lea', set_name: 'Alpha', number: '56', cmc: 1, color_identity: ['Red'], language: 'English' },
  { id: 'mtg-seed-0002', name: 'Black Lotus', supertype: '', subtypes: ['Artifact'], types: ['Black'], rarity: 'Mythic', set_id: 'lea', set_name: 'Alpha', number: '85', cmc: 0, color_identity: ['Black'], language: 'English' },
  { id: 'mtg-seed-0003', name: 'Swamp', supertype: 'Land', subtypes: ['Basic', 'Land'], types: [], rarity: 'Basic', set_id: 'lea', set_name: 'Alpha', number: '383', cmc: null, color_identity: [], language: 'English' },
  { id: 'mtg-seed-0004', name: 'Llanowar Elves', supertype: 'Creature', subtypes: ['Creature', 'Elf'], types: ['Green'], rarity: 'Common', set_id: 'lea', set_name: 'Alpha', number: '353', cmc: 1, color_identity: ['Green'], language: 'English' },
  { id: 'mtg-seed-0005', name: "Sage's Cursed Statue", supertype: 'Creature', subtypes: ['Creature', 'Artifact', 'Golem'], types: ['Colorless'], rarity: 'Uncommon', set_id: 'lea', set_name: 'Alpha', number: '111', cmc: 5, color_identity: [], language: 'English' },
];

async function seed() {
  for (const c of SEED) {
    await db.run(
      `INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number,
        image_url, cmc, color_identity, language, last_updated)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '', ?, ?, ?, CURRENT_TIMESTAMP)`,
      [c.id, c.name, c.supertype, JSON.stringify(c.subtypes), JSON.stringify(c.types),
       c.rarity, c.set_id, c.set_name, c.number, c.cmc, JSON.stringify(c.color_identity), c.language]
    );
  }
}

// The fake engine. `behavior` steers it: 'normal' answers known queries,
// 'down' fails every call like a stopped sidecar (no response at all),
// 'reject400' makes it 400 the query the shared parser itself rejects.
let behavior = 'normal';
let calls = [];

const ok = (data, total) => ({
  status: 200, statusText: 'OK', headers: {}, config: {},
  data: { data, has_more: false, total_cards: total },
});

// fra cards only the live engine can produce — proof of which engine answered.
const FRA_CARDS = [1, 2, 3].map(i => ({
  id: `fra-live-${i}`, name: `Fra Live ${i}`, type_line: 'Instant', rarity: 'common',
  set: 'fra', set_name: 'Reality Fracture', collector_number: String(i),
}));
// A "creatures" answer made of cards the CACHE cannot match (different set
// and type line), so the cache-fallback assertions stay clean even after the
// live path's cacheCards() warming lands them in card_cache.
const LIVE_CREATURES = [1, 2].map(i => ({
  id: `live-ans-${i}`, name: `Live Answer ${i}`, type_line: 'Instant', rarity: 'common',
  set: '9ed', set_name: 'Ninth Edition', collector_number: String(i),
}));
const CATALOG_ONE = [{ id: 'cat-1', name: 'Catalog Card 1', type_line: 'Instant', rarity: 'common', set: 'cmm', collector_number: '1' }];
// The query the shared parser rejects: a real Scryfall engine is the oracle
// on its own grammar, so the stub accepts what it is handed.
const PARADOX = [{ id: 'tst-paradox', name: 'Paradox Answer', type_line: 'Instant', rarity: 'rare', set: 'tst', set_name: 'Test', collector_number: '9' }];

const ROUTES = {
  'set:fra': () => ok(FRA_CARDS, 3),
  // NOTE: keys are the CANONICAL form toScryfall emits — 'is:creature' and
  // 't:creature' both serialize to 't:creature', so each test case also needs
  // its own distinct query string or the successful-page cache would answer
  // the second spelling without any network call at all.
  'set:lea t:creature': () => ok(LIVE_CREATURES, 2),
  'set:lea int-check': () => ok(LIVE_CREATURES, 2),
  'otag:seed': () => ok(CATALOG_ONE, 1),
  'otag:down': () => ok(CATALOG_ONE, 1),
  'set:lea and': () => ok(PARADOX, 1),
  'set:lea or': () => ok(PARADOX, 1),
};

async function adapter(config) {
  const url = new URL(config.url, config.baseURL);
  const q = url.searchParams.get('q');
  calls.push({ host: url.hostname, path: url.pathname, q });
  if (behavior === 'down') {
    const e = new Error('connect ECONNREFUSED 127.0.0.1:8420');
    e.code = 'ECONNREFUSED';
    throw e;
  }
  if (behavior === 'reject400' && (q === 'set:lea and' || q === 'set:lea or')) {
    const e = new Error('Request failed with status code 400');
    e.response = { status: 400, data: {} };
    throw e;
  }
  const route = ROUTES[q];
  if (route) return route();
  const e = new Error('Request failed with status code 404');
  e.response = { status: 404, data: {} };
  throw e; // unmatched: "nothing matched"
}

function loadApi(local) {
  const p = require.resolve('../src/scryfallApi');
  delete require.cache[p];
  process.env.LOCAL_SCRYFALL_ENABLED = local ? 'true' : 'false';
  process.env.LOCAL_SCRYFALL_URL = `http://${SIDECAR_HOST}:8420`;
  const api = require(p);
  assert.strictEqual(api.LOCAL_SCRYFALL, local, 'module loaded in the requested mode');
  api.client.defaults.adapter = adapter;
  return api;
}

async function expectThrows(fn, message, label) {
  try { await fn(); } catch (e) {
    assert.strictEqual(e.message, message, `${label}: error message`);
    return;
  }
  assert.fail(`${label}: expected it to throw ${message}`);
}

const names = (r) => r.cards.map(c => c.name).sort();
const S = { scope: 'database', lang: 'en', page: 1, limit: 60 };

async function main() {
  await db.initDb();
  await seed();

  // --------------------------------------------------------------- upstream
  // LOCAL_SCRYFALL_ENABLED=false: behavior byte-identical to before the fix.
  const up = loadApi(false);

  calls = [];
  let r = await up.searchCards({ q: 'set:lea is:creature', ...S });
  assert.deepStrictEqual(calls, [], 'upstream: fully-data-backed database query must not touch the API');
  assert.strictEqual(r.source, 'cache', 'upstream: cache-first is preserved');
  assert.deepStrictEqual(names(r), ['Llanowar Elves', "Sage's Cursed Statue"]);
  assert.strictEqual(r.total, 2);

  // THE BUG, preserved in upstream mode: an uncached set answers 0 from the
  // cache with zero provider calls. The parent's brief pins upstream mode as
  // byte-identical, so this stale-but-historical behavior must stay.
  calls = [];
  r = await up.searchCards({ q: 'set:fra', ...S });
  assert.deepStrictEqual(calls, [], 'upstream: set:fra still answered from cache');
  assert.strictEqual(r.source, 'cache');
  assert.deepStrictEqual(names(r), []);
  assert.strictEqual(r.total, 0, 'upstream: the false-premise 0 stands in upstream mode');

  calls = [];
  r = await up.searchCards({ q: 'otag:seed', ...S });
  assert.strictEqual(calls.length, 1, 'upstream: catalog operator goes live');
  assert.strictEqual(calls[0].host, 'api.scryfall.com', 'upstream: live path faces the real API');
  assert.strictEqual(calls[0].path, '/cards/search');
  assert.strictEqual(r.source, 'scryfall');

  calls = [];
  r = await up.searchCards({ q: 'set:lea is:creature', scope: 'internet', lang: 'en', page: 1, limit: 60 });
  assert.ok(calls.length >= 1, 'upstream: internet scope always asks the API');
  assert.strictEqual(r.source, 'scryfall');

  // Sidecar-down fallback exists upstream too (it predates the fix): an
  // internet-scope failure still degrades to cache, never an error page.
  // Fresh query string — a repeat of one already answered live would be
  // served by the successful-page cache and never see the outage.
  calls = []; behavior = 'down';
  r = await up.searchCards({ q: 'set:lea is:land', scope: 'internet', lang: 'en', page: 1, limit: 60 });
  assert.ok(calls.length >= 1, 'upstream: internet tried live first');
  assert.strictEqual(r.source, 'cache', 'upstream: internet falls back to cache when live is down');
  behavior = 'normal';

  // Upstream keeps the parser as the final authority: a rejected query is a
  // 400, and the API is NEVER contacted (not even to double-check).
  calls = [];
  await expectThrows(
    () => up.searchCards({ q: 'set:lea and', ...S }),
    'INVALID_QUERY', 'upstream: syntax error is named locally'
  );
  assert.deepStrictEqual(calls, [], 'upstream: a rejected query must not reach the API');

  calls = [];
  r = await up.searchCards({ q: 'set:lea', scope: 'collection', userId: 1, lang: 'en', page: 1, limit: 60 });
  assert.deepStrictEqual(r.cards, [], 'collection scope ignores raw queries');
  assert.deepStrictEqual(calls, [], 'collection scope never leaves the process');

  // ------------------------------------------------------------------ local
  // LOCAL_SCRYFALL_ENABLED=true: the sidecar answers, the cache is fallback.
  const lo = loadApi(true);

  // The regression itself: set:fra has ZERO cached rows at decision time.
  calls = [];
  r = await lo.searchCards({ q: 'set:fra', ...S });
  assert.ok(calls.length >= 1, 'local: set:fra MUST reach the sidecar, not answer 0 from the cache');
  assert.strictEqual(calls[0].host, SIDECAR_HOST, 'local: database-scope live traffic faces the sidecar');
  assert.strictEqual(calls[0].path, '/cards/search', 'local: hits the Scryfall-compatible search path');
  assert.strictEqual(r.source, 'sidecar', 'local: meta.source says which engine answered');
  assert.deepStrictEqual(names(r), ['Fra Live 1', 'Fra Live 2', 'Fra Live 3']);
  assert.strictEqual(r.total, 3, 'local: total is the sidecar count, not the cache count');
  const fraCached = await db.get(`SELECT COUNT(*) AS n FROM card_cache WHERE set_id = 'fra'`);
  assert.strictEqual(fraCached.n, 3, 'local: the sidecar answer warms card_cache');

  // Even with cacheable rows present, a fully-data-backed database query is
  // answered live — cache rows are no longer presumed authoritative.
  calls = [];
  r = await lo.searchCards({ q: 'set:lea is:creature', ...S });
  assert.ok(calls.length >= 1, 'local: a data-backed query with cached rows still goes live');
  assert.strictEqual(r.source, 'sidecar');
  assert.deepStrictEqual(names(r), ['Live Answer 1', 'Live Answer 2'], 'the sidecar answer, not the cache rows');

  calls = [];
  r = await lo.searchCards({ q: 'otag:seed', ...S });
  assert.strictEqual(calls.length, 1, 'local: catalog operator stays live');
  assert.strictEqual(calls[0].host, SIDECAR_HOST, 'local: catalog traffic stays on the sidecar');
  assert.strictEqual(r.source, 'sidecar');

  calls = [];
  r = await lo.searchCards({ q: 'set:lea int-check', scope: 'internet', lang: 'en', page: 1, limit: 60 });
  assert.ok(calls.length >= 1, 'local: internet scope stays live');
  assert.strictEqual(calls[0].host, SIDECAR_HOST);
  assert.strictEqual(r.source, 'sidecar');
  assert.deepStrictEqual(names(r), ['Live Answer 1', 'Live Answer 2']);

  // The designed fallback: sidecar DOWN, a data-backed query still gets an
  // answer — the stale cache — and says 'cache', not 'sidecar'.
  calls = []; behavior = 'down';
  r = await lo.searchCards({ q: 'set:lea is:land', ...S });
  assert.ok(calls.length >= 1, 'local: tried the sidecar before falling back');
  assert.ok(calls.every(c => c.host === SIDECAR_HOST), 'local: fallback never leaks to the real API');
  assert.strictEqual(r.source, 'cache', 'local: sidecar down falls back to the cache');
  assert.deepStrictEqual(names(r), ['Swamp'], 'fallback answer is the cache rows');

  // A catalog query cannot be evaluated by the cache: honest error, no
  // silent cache-shaped lie.
  await expectThrows(
    () => lo.searchCards({ q: 'otag:down', ...S }),
    'UPSTREAM_UNAVAILABLE', 'local: sidecar down + catalog operator'
  );
  behavior = 'normal';

  // Scryfall, not this parser, is the oracle on Scryfall syntax: a query the
  // shared parser rejects is forwarded VERBATIM (no AST to rewrite from) and
  // the sidecar's answer is served.
  calls = [];
  r = await lo.searchCards({ q: 'set:lea and', ...S });
  assert.ok(calls.length >= 1, 'local: parser-rejected query still reaches the sidecar');
  assert.strictEqual(calls[0].q, 'set:lea and', 'forwarded verbatim, un-rewritten');
  assert.strictEqual(calls[0].host, SIDECAR_HOST);
  assert.strictEqual(r.source, 'sidecar');
  assert.deepStrictEqual(names(r), ['Paradox Answer']);

  // ...and when the sidecar ALSO rejects it, its 400 becomes INVALID_QUERY —
  // the 400 path stays reachable even though the early-throw path is gone.
  behavior = 'reject400';
  await expectThrows(
    () => lo.searchCards({ q: 'set:lea or', ...S }),
    'INVALID_QUERY', 'local: the sidecar 400 names the typo'
  );
  behavior = 'normal';

  // Collection scope untouched in local mode as well: raw queries yield
  // nothing and no traffic.
  calls = [];
  r = await lo.searchCards({ q: 'set:lea', scope: 'collection', userId: 1, lang: 'en', page: 1, limit: 60 });
  assert.deepStrictEqual(r.cards, []);
  assert.deepStrictEqual(calls, [], 'local: collection scope stays untouched');

  console.log('localrawrouting: all routing-matrix assertions passed (upstream + sidecar modes).');
}

main()
  .then(() => {
    for (const s of ['', '-wal', '-shm']) { try { fs.unlinkSync(process.env.DB_PATH + s); } catch { /* gone */ } }
    process.exit(0);
  })
  .catch(err => {
    console.error('FAIL localrawrouting:', (err && err.stack) || err);
    try { fs.unlinkSync(process.env.DB_PATH); } catch { /* gone */ }
    process.exit(1);
  });
