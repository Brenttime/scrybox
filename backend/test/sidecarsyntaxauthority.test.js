// The sidecar is the SOLE SYNTAX AUTHORITY for collection searches when
// LOCAL_SCRYFALL_ENABLED=true: a mode 'local' syntax query (is:/color:/set:/
// rarity: — everything rawQuerySql used to answer itself) is sent VERBATIM to
// the engine, walked, and intersected with owned rows; the card_cache SQL
// compiler must not answer at all. Prove both directions:
//
//   · sidecar mode  — engine answers (source 'sidecar'), zero compileCollectionQuery
//                     executions, printing-level (set:) intersect still by exact
//                     printing id, transport failure falls back to the SQL
//                     compiler LOUDLY ('local-fallback'), the engine's own 400
//                     is final (no silent SQL fallback), a query THIS parser
//                     rejects still reaches the engine instead of a local 400.
//   · upstream mode — byte-identical pre-change behavior: local-mode queries
//                     answer from SQLite with ZERO HTTP egress (the mock
//                     client proves it), parser rejections 400 before any
//                     request, catalog queries still walk api.scryfall.com.
//
// No framework — plain node + assert, same adapter-stub pattern as
// localrawrouting.test.js. The sidecar is never contacted over real network:
// the axios adapter stands in for it (the live-engine counterpart of every
// assertion here is sidecarsyntaxtruth.test.js).
// Run: `node test/sidecarsyntaxauthority.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');

process.env.SCRYFALL_GAP_SCALE = '0';
process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-sidecarauth-${process.pid}.db`);

const db = require('../src/db');
const rawQuerySql = require('../src/utils/rawQuerySql');
const { classifyQuery } = require('../../shared/scryfallQuery.js');
const SIDECAR = 'http://127.0.0.1:8420';

// ---- spies -----------------------------------------------------------------
// compileCollectionQuery is THE door to the divergent second implementation
// of Scryfall syntax. Every recorded call gets its SQL captured so the test
// can also assert none of those statements ever reached the database.
let compiledCalls = [];
let executedSql = [];
{
  const origCompile = rawQuerySql.compileCollectionQuery;
  rawQuerySql.compileCollectionQuery = function (opts) {
    const out = origCompile.call(this, opts);
    compiledCalls.push(out.sql);
    return out;
  };
  const origAll = db.all;
  db.all = function (sql, params) { executedSql.push(String(sql)); return origAll.call(db, sql, params); };
  const origTx = db.withDedicatedReadTransaction;
  db.withDedicatedReadTransaction = function (fn) {
    return origTx.call(db, async (client) => {
      const wrapped = Object.create(client);
      for (const name of ['all', 'get', 'run']) {
        const orig = client[name].bind(client);
        wrapped[name] = (sql, params) => { executedSql.push(String(sql)); return orig(sql, params); };
      }
      return fn(wrapped);
    });
  };
}
const resetSpies = () => { compiledCalls = []; executedSql = []; };
const sqlCompilerAnswered = () => compiledCalls.length > 0
  || executedSql.some(sql => compiledCalls.some(c => sql === c));

// ---- fixtures ---------------------------------------------------------------
// owned by u7: three fra printings, a DIFFERENT-SET printing of one of the
// same names (the decoy the by-printing walk must not match), two blues, a
// red, a land, two rare snow cards, one white card. u8 owns one snow card
// (must never leak).
const CACHE_ROWS = [
  { id: 'mtg-fra-a1', name: 'Academic Ascent', set_id: 'fra', number: '2', rarity: 'Common', types: ['White'], subtypes: ['Instant'] },
  { id: 'mtg-fra-a2', name: 'Aerid Konstrari', set_id: 'fra', number: '121', rarity: 'Mythic Rare', types: ['Red', 'Green'], subtypes: ['Creature', 'Human', 'Wizard'] },
  { id: 'mtg-fra-a3', name: 'Afterthought Sentry', set_id: 'fra', number: '166', rarity: 'Common', types: [], subtypes: ['Creature', 'Construct'] },
  { id: 'mtg-m10-a3', name: 'Afterthought Sentry', set_id: 'm10', number: '301', rarity: 'Common', types: [], subtypes: ['Creature', 'Construct'] },
  { id: 'mtg-blu-1', name: 'A Little Chat', set_id: 'snc', number: '47', rarity: 'Uncommon', types: ['Blue'], subtypes: ['Instant'] },
  { id: 'mtg-blu-2', name: 'Cancel', set_id: 'm11', number: '38', rarity: 'Common', types: ['Blue'], subtypes: ['Instant'] },
  { id: 'mtg-red-1', name: 'Lightning Bolt', set_id: 'lea', number: '6', rarity: 'Common', types: ['Red'], subtypes: ['Instant'] },
  { id: 'mtg-lnd-1', name: 'Ancient Tomb', set_id: 'eos', number: '1', rarity: 'Rare', types: [], subtypes: ['Land'] },
  { id: 'mtg-snw-1', name: 'Blood on the Snow', set_id: 'khm', number: '79', rarity: 'Rare', types: ['Black'], subtypes: ['Snow', 'Sorcery'] },
  { id: 'mtg-snw-2', name: 'Snow-Covered Forest', set_id: 'sld', number: '5', rarity: 'Rare', types: ['Green'], subtypes: ['Basic', 'Snow', 'Land', 'Forest'] },
  { id: 'mtg-wht-1', name: 'Oblivion Ring', set_id: 'm11', number: '26', rarity: 'Common', types: ['White'], subtypes: ['Enchantment'] },
];
const OWNED = {
  u7: ['mtg-fra-a1', 'mtg-fra-a2', 'mtg-fra-a3', 'mtg-m10-a3', 'mtg-blu-1', 'mtg-blu-2', 'mtg-red-1', 'mtg-lnd-1', 'mtg-snw-1', 'mtg-snw-2', 'mtg-wht-1'],
  u8: ['mtg-snw-1'],
};

// What the engine answers per walk. Keys are the EXACT q the stub must see —
// the canonical re-serialization plus the walk's unique: shape.
const ENGINE = {
  'set:fra unique:prints': [
    { id: 'fra-a1', name: 'Academic Ascent' },
    { id: 'fra-a2', name: 'Aerid Konstrari' },
    { id: 'fra-a3', name: 'Afterthought Sentry' },
  ],
  'color:u unique:cards': [
    { id: 'blu-1', name: 'A Little Chat' },
    { id: 'blu-2', name: 'Cancel' },
    { id: 'blu-9', name: 'Not Owned Blue' },
  ],
  // is:snow canonicalizes to t:snow on the way out (Scrybox extension).
  'rarity:r t:snow unique:cards': [
    { id: 'snw-1', name: 'Blood on the Snow' },
    { id: 'snw-2', name: 'Snow-Covered Forest' },
  ],
  // Distinct tag names per mode — the durable collection_query_cache is
  // shared by the DB, and a control query must not be served from the other
  // mode's walk.
  'otag:upctl unique:cards': [
    { id: 'blu-2', name: 'Cancel' },
    { id: 'zz-1', name: 'Not Owned Either' },
  ],
  'otag:ctrltag unique:cards': [
    { id: 'blu-2', name: 'Cancel' },
    { id: 'zz-1', name: 'Not Owned Either' },
  ],
  'set:lea and unique:cards': [
    { id: 'fra-a1', name: 'Academic Ascent' },
  ],
  'color:w unique:cards': [
    { id: 'wht-1', name: 'Oblivion Ring' },
  ],
};

// ---- module under test, booted per mode (module-load-time consts) ---------
function loadApi(local) {
  // Purge ONLY the module whose mode consts freeze at load time. db and
  // rawQuerySql stay cached — the spies above wrap THOSE instances.
  for (const k of Object.keys(require.cache)) {
    if (k.endsWith('scryfallApi.js') || k.includes('shared/scryfallQuery.js')) delete require.cache[k];
  }
  if (local) {
    process.env.LOCAL_SCRYFALL_ENABLED = 'true';
    process.env.LOCAL_SCRYFALL_URL = SIDECAR;
  } else {
    delete process.env.LOCAL_SCRYFALL_ENABLED;
    delete process.env.LOCAL_SCRYFALL_URL;
  }
  return require('../src/scryfallApi');
}

let behavior = 'ok'; // 'ok' | 'down' | 'reject400'
let calls = [];
function installAdapter(api) {
  calls = [];
  api.client.defaults.adapter = async (config) => {
    const url = new URL(config.url, config.baseURL);
    const q = url.searchParams.get('q');
    calls.push({ q, host: url.host, path: url.pathname });
    if (behavior === 'down') {
      const err = new Error('connect ECONNREFUSED 127.0.0.1:8420');
      err.code = 'ECONNREFUSED';
      throw err;
    }
    if (behavior === 'reject400') {
      const err = new Error('Request failed with status code 400');
      err.response = { status: 400, data: { details: 'invalid query' } };
      throw err;
    }
    const data = ENGINE[q];
    if (!data) {
      const err = new Error('Request failed with status code 404');
      err.response = { status: 404 };
      throw err;
    }
    return {
      status: 200, statusText: 'OK', headers: {}, config: null,
      data: { data, has_more: false, total_cards: data.length },
    };
  };
}

async function main() {
  await db.initDb();
  const u7 = await db.run(`INSERT INTO users (username, password_hash, role, share_token) VALUES ('sa7', 'x', 'member', 'share-sa7')`);
  const u8 = await db.run(`INSERT INTO users (username, password_hash, role, share_token) VALUES ('sa8', 'x', 'member', 'share-sa8')`);
  for (const r of CACHE_ROWS) {
    await db.run(
      `INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, price_trend, price_normal, language, scryfall_search_eligible)
       VALUES (?, ?, 'MTG', ?, ?, ?, ?, UPPER(?), ?, 1, 1, 'English', 1)`,
      [r.id, r.name, JSON.stringify(r.subtypes), JSON.stringify(r.types), r.rarity, r.set_id, r.set_name || r.set_id, r.number]
    );
  }
  for (const [user, ids] of Object.entries(OWNED)) {
    const uid = user === 'u7' ? u7.lastID : u8.lastID;
    for (const id of ids) await db.run(`INSERT INTO collection (user_id, card_id, quantity) VALUES (?, ?, 1)`, [uid, id]);
  }

  // A local-mode combo and a printing-mode query, for both modes.
  assert.strictEqual(classifyQuery('rarity:r is:snow').mode, 'local');
  assert.strictEqual(classifyQuery('set:fra').mode, 'local');
  assert.strictEqual(classifyQuery('otag:ctrltag').mode, 'catalog');

  // =========================================================================
  // 1. UPSTREAM MODE — the rate-limit optimization stays, byte-identical.
  // =========================================================================
  {
    const api = loadApi(false);
    installAdapter(api);

    // classifyQuery mode pinned + ZERO HTTP egress for local-mode queries —
    // the mock client must see nothing, not even an attempted request.
    resetSpies();
    const local = await api.resolveCollectionQuery({ q: 'rarity:r is:snow', userId: u7.lastID });
    assert.deepStrictEqual(calls.map(c => c.q), [], 'upstream: local-mode query must make ZERO client calls');
    assert.strictEqual(local.cacheStatus, 'local', 'upstream: SQL compiler still answers local mode');
    assert.strictEqual(local.source, 'local');
    assert.ok(sqlCompilerAnswered(), 'upstream: the compiler answered, so its door must have been used');
    assert.deepStrictEqual(local.cards.map(c => c.name).sort(), ['Blood on the Snow', 'Snow-Covered Forest']);

    resetSpies();
    const setQ = await api.resolveCollectionQuery({ q: 'set:lea', userId: u7.lastID });
    assert.deepStrictEqual(calls, [], 'upstream: set: stays offline too');
    assert.strictEqual(setQ.cacheStatus, 'local');
    assert.deepStrictEqual(setQ.cards.map(c => c.name), ['Lightning Bolt']);

    // A parser rejection is a 400 BEFORE any request — no engine round trip.
    resetSpies();
    await assert.rejects(
      () => api.resolveCollectionQuery({ q: 'set:lea and', userId: u7.lastID }),
      (e) => e.message === 'INVALID_QUERY'
    );
    assert.deepStrictEqual(calls, [], 'upstream: rejected syntax never leaves the process');
    assert.ok(!sqlCompilerAnswered(), 'upstream: rejected syntax never reaches the compiler either');

    // Catalog control: still walks api.scryfall.com.
    behavior = 'ok';
    resetSpies();
    const ctl = await api.resolveCollectionQuery({ q: 'otag:upctl', userId: u7.lastID });
    assert.strictEqual(calls.length, 1, 'upstream: catalog query makes exactly one walk');
    assert.strictEqual(calls[0].host, 'api.scryfall.com', 'upstream: catalog host stays api.scryfall.com');
    assert.strictEqual(ctl.source, 'scryfall', 'upstream: source names scryfall');
    assert.strictEqual(ctl.cacheStatus, 'resolved');
  }

  // =========================================================================
  // 2. SIDECAR MODE — the engine answers everything; SQL is never consulted.
  // =========================================================================
  {
    const api = loadApi(true);
    assert.strictEqual(api.LOCAL_SCRYFALL, true);
    installAdapter(api);

    // 2a. A pure local-mode combo goes to the engine verbatim, not to SQL.
    behavior = 'ok';
    resetSpies();
    const snow = await api.resolveCollectionQuery({ q: 'rarity:r is:snow', userId: u7.lastID });
    assert.strictEqual(calls.length, 1, 'sidecar: local-mode query hits the engine exactly once');
    assert.strictEqual(calls[0].q, 'rarity:r t:snow unique:cards',
      'sidecar: the canonical query is walked verbatim (is: → t: rewrite, walk adds unique:cards)');
    assert.strictEqual(calls[0].host, new URL(SIDECAR).host, 'sidecar: the walk goes to the sidecar, not api.scryfall.com');
    assert.ok(!sqlCompilerAnswered(), 'sidecar: ZERO card_cache SQL executions for a syntax query');
    assert.strictEqual(snow.cacheStatus, 'resolved');
    assert.strictEqual(snow.source, 'sidecar', 'meta says the engine answered');
    assert.deepStrictEqual(snow.cards.map(c => c.name).sort(), ['Blood on the Snow', 'Snow-Covered Forest'],
      'owned rows for the engine-matched names — same rows the SQL path returned pre-change');
    assert.ok(!snow.cards.some(c => c.user_id === u8.lastID), "another user's rows never leak");

    // 2b. color:u — same rows the compiler used to produce (golden).
    resetSpies();
    const blue = await api.resolveCollectionQuery({ q: 'color:u', userId: u7.lastID });
    assert.strictEqual(calls[1].q, 'color:u unique:cards');
    assert.ok(!sqlCompilerAnswered(), 'sidecar: color:u answered without the compiler');
    assert.strictEqual(blue.source, 'sidecar');
    assert.deepStrictEqual(blue.cards.map(c => c.name).sort(), ['A Little Chat', 'Cancel']);

    // 2c. PRINTING_OPERATORS keep exact-printing semantics: set:fra matched
    // by printing ID — the same-name m10 row must not sneak in on a name hit.
    resetSpies();
    const fra = await api.resolveCollectionQuery({ q: 'set:fra', userId: u7.lastID });
    assert.strictEqual(calls[2].q, 'set:fra unique:prints',
      'sidecar: a printing-level query walks unique:prints (exact printings), not unique:cards');
    assert.ok(!sqlCompilerAnswered(), 'sidecar: set: answered without the compiler');
    assert.strictEqual(fra.source, 'sidecar');
    const fraIds = fra.cards.map(c => c.card_id).sort();
    assert.deepStrictEqual(fraIds, ['mtg-fra-a1', 'mtg-fra-a2', 'mtg-fra-a3'],
      'only the owned FRA printings come back — the m10 same-name printing is excluded by id');
    assert.strictEqual(fra.total, 3);

    // 2d. Control catalog query still behaves as catalog mode always has.
    resetSpies();
    const ctl = await api.resolveCollectionQuery({ q: 'otag:ctrltag', userId: u7.lastID });
    assert.strictEqual(calls[3].q, 'otag:ctrltag unique:cards');
    assert.strictEqual(ctl.source, 'sidecar');
    assert.deepStrictEqual(ctl.cards.map(c => c.name), ['Cancel']);

    // 2e. analyze() must not throw FIRST: a query this parser rejects is sent
    // to the engine verbatim; the engine answering ends the debate.
    resetSpies();
    const rejectedByParser = await api.resolveCollectionQuery({ q: 'set:lea and', userId: u7.lastID });
    assert.strictEqual(calls[4].q, 'set:lea and unique:cards',
      'sidecar: parser-rejected query reaches the engine verbatim (no local 400 pre-empt)');
    assert.strictEqual(rejectedByParser.source, 'sidecar');
    assert.ok(!sqlCompilerAnswered(), 'sidecar: the engine answered, not the compiler');
    assert.deepStrictEqual(rejectedByParser.cards.map(c => c.name), ['Academic Ascent']);

    // 2f. The engine's OWN 400 is final — validity is the authority's call,
    // so a 400 must NOT quietly fall back to the SQL compiler.
    resetSpies();
    behavior = 'reject400';
    await assert.rejects(
      () => api.resolveCollectionQuery({ q: 'color:b', userId: u7.lastID }),
      (e) => e.message === 'INVALID_QUERY'
    );
    assert.ok(!sqlCompilerAnswered(), 'sidecar: engine 400 never falls through to SQL');

    // 2g. An engine ANSWER of empty (404 = no matches) is not an outage:
    // resolved, source sidecar, zero rows, no fallback.
    resetSpies();
    behavior = 'ok';
    const empty = await api.resolveCollectionQuery({ q: 'set:zzz9', userId: u7.lastID });
    assert.strictEqual(empty.cacheStatus, 'resolved');
    assert.strictEqual(empty.source, 'sidecar');
    assert.deepStrictEqual(empty.cards, []);
    assert.ok(!sqlCompilerAnswered(), 'sidecar: empty engine answer does not trigger the SQL fallback');

    // 2h. TRANSPORT failure mid-walk: the pre-change compiler answers, LOUDLY.
    resetSpies();
    behavior = 'down';
    const fellBack = await api.resolveCollectionQuery({ q: 'color:w', userId: u7.lastID });
    assert.ok(calls.length >= 1, 'sidecar: the walk was attempted before falling back');
    assert.strictEqual(fellBack.cacheStatus, 'local-fallback', 'meta/X-Catalog-Cache say the fallback answered');
    assert.strictEqual(fellBack.source, 'local-fallback');
    assert.ok(sqlCompilerAnswered(), 'sidecar: the outage earned the SQL compiler');
    assert.deepStrictEqual(fellBack.cards.map(c => c.name), ['Oblivion Ring']);

    // 2i. A catalog-mode query with the engine down has no SQL fallback
    //     (the compiler cannot answer what it was built to decline).
    resetSpies();
    behavior = 'down';
    await assert.rejects(
      () => api.resolveCollectionQuery({ q: 'otag:downcheck', userId: u7.lastID }),
      (e) => e.message === 'UPSTREAM_UNAVAILABLE'
    );
    assert.ok(!sqlCompilerAnswered(), 'sidecar: catalog queries never fall back to SQL');
    behavior = 'ok';
  }

  console.log('sidecarsyntaxauthority: sidecar mode delegates ALL syntax to the engine (verbatim walk, printing-id intersect, loud fallback, engine-final 400, parser never shadows the authority); upstream mode still answers local queries offline with zero egress.');
}

main()
  .then(() => setTimeout(() => process.exit(0), 100))
  .catch(err => {
    console.error('FAIL sidecarsyntaxauthority:', (err && err.stack) || err);
    setTimeout(() => process.exit(1), 100);
  });
