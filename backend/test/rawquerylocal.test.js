// Local raw-query search: a Scryfall-syntax query whose operators the stored
// card_cache rows can answer (is:, color:, set:, rarity:, ...) must be answered
// FROM the database — instantly, with zero provider calls — and must agree
// with the shared JS evaluator (the browser's collection filter runs that one
// on the same rows). Catalog operators (otag:, ...) still go live, and their
// pages are cached so a repeat costs no rate-limit budget.
// Run: `node test/rawquerylocal.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');

process.env.SCRYFALL_GAP_SCALE = '0';
process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-scrylocal-${process.pid}.db`);
const db = require('../src/db');
const scryfallApi = require('../src/scryfallApi');
const { matches } = require('../../shared/scryfallQuery.js');

const SCRY_PAGE = 175;

// Seed a small English card_cache that the local path can actually answer.
// Rows use the exact storage shape cacheNormalizedCards writes: subtypes and
// color_identity are JSON-stringified arrays, rarity a display word.
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

// Row shape parseCardRow produces: array columns parsed back to arrays.
const asRow = (c) => ({ ...c, subtypes: JSON.parse(JSON.stringify(c.subtypes)), types: JSON.parse(JSON.stringify(c.types)), color_identity: JSON.parse(JSON.stringify(c.color_identity)) });

async function jsMatches(q) {
  const out = [];
  for (const c of SEED) if (matches(asRow(c), q)) out.push(c.name);
  return out.sort();
}

// Stub the axios adapter: count every Scryfall call and answer one catalog
// query with two pages so the repeat-call assertion has real data to walk.
let scryCalls = 0;
scryfallApi.client.defaults.adapter = async (config) => {
  scryCalls += 1;
  const url = new URL(config.url, 'https://api.scryfall.com');
  const q = url.searchParams.get('q');
  const page = parseInt(url.searchParams.get('page'), 10) || 1;
  if (q === 'otag:seed') {
    const start = (page - 1) * 2;
    const data = [];
    for (let i = start; i < Math.min(start + 2, 3); i++) {
      data.push({ id: `cat-${i}`, name: `Catalog Card ${i}`, type_line: 'Instant', rarity: 'common', set: 'cmm', collector_number: String(i + 1) });
    }
    return { status: 200, statusText: 'OK', headers: {}, config,
      data: { data, has_more: page === 1, total_cards: 3 } };
  }
  const err = new Error('Request failed with status code 404');
  err.response = { status: 404, data: {} };
  throw err; // any other query: "matched nothing"
};

async function main() {
  await db.initDb();
  await seed();

  // 1. A data-backed query is answered from the cache — zero provider calls.
  scryCalls = 0;
  let r = await scryfallApi.searchCards({ q: 'set:lea is:creature', scope: 'database', lang: 'en', page: 1, limit: 60 });
  assert.strictEqual(scryCalls, 0, 'local raw query must not call Scryfall');
  assert.strictEqual(r.source, 'cache', 'answer is flagged cache');
  assert.deepStrictEqual(r.cards.map(c => c.name).sort(), ['Llanowar Elves', "Sage's Cursed Statue"]);
  assert.strictEqual(r.total, 2, 'total is the true cache count');

  // 2. The cache answer agrees with the shared JS evaluator, query by query.
  const BATTERY = [
    'set:lea color:g rarity:rare',
    'set:lea c:c',
    'set:lea is:land',
    'set:lea colorless',
    'set:lea number:85',
    'set:lea mv:1',
    'set:lea cmc>=1',
    'set:lea c:g',
    'set:lea id<=g',
    'set:lea r>=uncommon',
    '!"lightning bolt"',
    'set:lea cn>100',
    'set:lea unique:prints order:name',
    'set:lea rarity:m',
    'set:lea type:artifact',
    'set:lea t:artifact',        // Scryfall's short form for type: — must route local too
    'set:lea t:land',
    'is:creature or is:land',
    'set:lea -color:g',
    'set:lea name:swamp',
  ];
  for (const q of BATTERY) {
    scryCalls = 0;
    const local = await scryfallApi.searchCards({ q, scope: 'database', lang: 'en', page: 1, limit: 60 });
    assert.strictEqual(scryCalls, 0, `${q}: must not call Scryfall`);
    assert.deepStrictEqual(
      local.cards.map(c => c.name).sort(),
      await jsMatches(q),
      `${q}: cache answer must match the JS evaluator`
    );
  }

  // 2b. Alias routing: t: is a verified alias of type:, so it is LOCAL; the
  // distinct operator subtype: is NOT an alias and must stay CATALOG.
  const { analyze } = require('../../shared/scryfallQuery.js');
  assert.strictEqual(analyze('t:creature').mode, 'local', 't: must route local like type:');
  assert.strictEqual(analyze('type:creature').mode, 'local', 'type: routes local');
  assert.strictEqual(analyze('subtype:creature').mode, 'catalog', 'subtype: is distinct and stays catalog');
  assert.deepStrictEqual(
    jsMatches('t:creature'),
    jsMatches('type:creature'),
    't:creature and type:creature must match the same cards'
  );

  // 3. Paging a local query walks the offset with no provider traffic.
  r = await scryfallApi.searchCards({ q: 'set:lea', scope: 'database', lang: 'en', page: 2, limit: 3 });
  assert.strictEqual(scryCalls, 0);
  assert.strictEqual(r.cards.length, 2, 'page 2 of 5 seeded lea rows');
  assert.strictEqual(r.total, 5);

  // 4. A catalog operator (otag:) still resolves LIVE against Scryfall.
  scryCalls = 0;
  r = await scryfallApi.searchCards({ q: 'otag:seed', scope: 'internet', lang: 'en', page: 1, limit: 60 });
  assert.strictEqual(r.source, 'scryfall', 'catalog answer is flagged scryfall');
  assert.strictEqual(r.cards.length, 3);
  assert.strictEqual(r.total, 3);
  const firstCalls = scryCalls;
  assert.ok(firstCalls > 0, 'catalog query must call Scryfall');

  // 5. Repeating the SAME catalog query costs no further rate-limit budget:
  //    the raw pages are cached, so a second identical search is served from
  //    the raw-page cache (or the just-cached rows) without a new walk.
  const before = scryCalls;
  await scryfallApi.searchCards({ q: 'otag:seed', scope: 'internet', lang: 'en', page: 1, limit: 60 });
  assert.strictEqual(scryCalls, before, 'repeat of a cached catalog query must not refetch');

  // 6. Collection scope: a raw query with only data-backed operators still
  //    yields nothing from the API path (the browser filters loaded rows).
  r = await scryfallApi.searchCards({ q: 'is:land', scope: 'collection', userId: 1 });
  assert.deepStrictEqual(r.cards, []);

  // 7. A syntax error is a named 400-shaped error, not a crash or a live call.
  await assert.rejects(
    () => scryfallApi.searchCards({ q: '(is:land', scope: 'internet' }),
    (err) => err.message === 'INVALID_QUERY'
  );

  console.log('rawquerylocal.test.js: all assertions passed');
}

main().then(() => process.exit(0)).catch(err => { console.error(err); process.exit(1); });
