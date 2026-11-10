#!/usr/bin/env node
// Generate scen-varied.json (BRIEF honest-benchmark requirement): rotate
// >=50 distinct offsets/limits/queries per route so no two consecutive
// requests hit the same URL. Card names come from the DB copy itself so
// every search variant actually matches rows; the auth token is copied from
// scen-rust.json (tokens are never edited, only echoed). Deterministic:
// names are picked by rowid order, offsets by a fixed LCG — no Math.random,
// so regenerating on a refreshed DB copy reproduces the same *shape*.
//
// usage: node gen-scen-varied.mjs [--db <path>] [--out <path>]
import fs from 'node:fs';
import { DatabaseSync } from 'node:sqlite';

const opt = (k, d) => { const i = process.argv.indexOf(k); return i >= 0 ? process.argv[i + 1] : d; };
const HERE = new URL('.', import.meta.url).pathname;
const ROOT = HERE + '../../../';
const db = new DatabaseSync(opt('--db', ROOT + 'data/bindarr.db'), { readOnly: true });
const scen = JSON.parse(fs.readFileSync(HERE + '../../../scen-rust.json', 'utf8'));
const authOf = (name) => scen.scenarios.find(s => s.name === name)?.headers || {};

// Fixed LCG for stable pseudo-random picks.
let seed = 0x5eed;
const rnd = (n) => ((seed = (seed * 1103515245 + 12345) & 0x7fffffff) % n);

const total = db.prepare('SELECT COUNT(*) c FROM collection WHERE user_id = 2').get().c;
const dbNames = db.prepare(
  "SELECT name FROM (SELECT c.name, COUNT(*) n, MIN(c.rowid) r FROM card_cache c GROUP BY c.name HAVING n BETWEEN 2 AND 60 AND LENGTH(name) BETWEEN 4 AND 24 ORDER BY r) LIMIT 500"
).all().map(r => r.name);
const collNames = db.prepare(
  "SELECT DISTINCT c.name FROM collection col JOIN card_cache c ON c.id = col.card_id WHERE col.user_id = 2 AND LENGTH(c.name) BETWEEN 4 AND 24 LIMIT 500"
).all().map(r => r.name);

const enc = encodeURIComponent;
const collectionPage = [];
const limits = [25, 50, 100, 250];
for (let i = 0; i < 52; i++) {
  const limit = limits[i % limits.length];
  const offset = (i * 719) % Math.max(1, total - limit);
  const f = i % 9 === 0 ? '&is_trade=true' : i % 7 === 0 ? '&count=1' : i % 5 === 0 ? '&style=grid' : '';
  collectionPage.push(`/api/collection?limit=${limit}&offset=${offset}${f}`);
}
// collection "full" shapes (heavy path): plain + count/alloc variations.
const collectionFull = ['', '?count=0', '?count=1', '?include_allocation=1', '?include_allocation=1&count=0', '?count=1&foo=%C3%A9']
  .map((q) => `/api/collection${q}`);
// db search: 52 distinct names x rotating page/limit (native Rust path).
const searchDb = [];
for (let i = 0; i < 52; i++) {
  const name = dbNames[rnd(dbNames.length)];
  const page = (i % 3) + 1, limit = [20, 60, 100][i % 3];
  searchDb.push(`/api/search?scope=database&name=${enc(name)}&page=${page}&limit=${limit}`);
}
// collection-scope search: 39 owned names + 13 operator conjunctions
// (iter 10's native gate) through the shared sidecar walk. The name picks
// consume rnd() on EVERY slot, so widening the shapes never shifts the
// names of the untouched slots.
const OPERATORS = ['t:creature c:g', 'c:wu t:instant', 't:enchantment c:b', 't:instant c:r', 'type:creature colour:green', 't:artifact c:w'];
const searchColl = [];
for (let i = 0; i < 52; i++) {
  const name = collNames[rnd(collNames.length)];
  const limit = [20, 60][i % 2];
  if (i % 4 === 0) {
    searchColl.push(`/api/search?scope=collection&q=${enc(OPERATORS[(i / 4 | 0) % OPERATORS.length])}&limit=${limit}&page=${(i % 3) + 1}`);
  } else {
    searchColl.push(`/api/search?scope=collection&q=${enc(name)}&limit=${limit}&page=1`);
  }
}
// stats: 52 variants. The query is ignored by both handlers (cache name is
// the constant 'stats'), so this exercises per-request routing/parse cost
// without pretending a param the route does not have.
const stats = Array.from({ length: 52 }, (_, i) => `/api/stats?_v=${i}`);

const groups = { collection_page: collectionPage, collection_full: collectionFull, search_db: searchDb, search_coll: searchColl, stats };
const scenarios = [
  { name: 'health', path: '/api/health' },
  { name: 'collection_page_varied', paths: groups.collection_page, headers: authOf('collection_page100') },
  { name: 'collection_full_varied', paths: groups.collection_full, concurrency: 2, duration: 15, headers: authOf('collection_full_27MB') },
  { name: 'stats_varied', paths: groups.stats, headers: authOf('stats') },
  { name: 'search_db_varied', paths: groups.search_db, headers: authOf('search_db_name_sol_ring') },
  { name: 'search_coll_varied', paths: groups.search_coll, headers: authOf('search_coll_sol_ring') },
];
const out = opt('--out', HERE + 'scen-varied.json');
fs.writeFileSync(out, JSON.stringify({ base: scen.base, generated_from: 'gen-scen-varied.mjs', scenarios }, null, 1));
console.error(`${out}: ${scenarios.length} scenarios, ${Object.entries(groups).map(([k, v]) => `${k}=${v.length}`).join(' ')}`);
