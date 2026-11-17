// Regenerates scryq_expected.json by running NODE's own parser
// (shared/scryfallQuery.js) and SQL compiler (backend/src/utils/rawQuerySql.js)
// over scryq_fixture.sql for every query in scryq_corpus.json.
//
//   node server/crates/scrybox-server/tests/fixtures/scryq_gen.js
//
// Uses node:sqlite (Node >= 22.5) so no npm install is needed.
const fs = require('fs');
const path = require('path');
const { DatabaseSync } = require('node:sqlite');
const root = path.resolve(__dirname, '../../../../..');
const sq = require(path.join(root, 'shared/scryfallQuery.js'));
const rawSql = require(path.join(root, 'backend/src/utils/rawQuerySql.js'));

// oracleTags.js LOCAL_COLLECTION_OPERATORS / supportsLocalCollectionQuery
// (inlined: oracleTags.js pulls axios at require time).
const LCO = new Set(['color', 'identity', 'rarity', 'lang', 'mv', 'otag',
  'include', 'unique', 'order', 'sort', 'direction', 'prefer', 'display']);
const supportsLocal = a => !!a && a.operators.includes('otag') && a.operators.every(op => LCO.has(op));

const db = new DatabaseSync(':memory:');
db.exec(fs.readFileSync(path.join(__dirname, 'scryq_fixture.sql'), 'utf8'));
const corpus = JSON.parse(fs.readFileSync(path.join(__dirname, 'scryq_corpus.json'), 'utf8'));

const out = [];
for (const q of corpus) {
  const rec = { q };
  let a;
  try {
    a = sq.analyze(q.trim());
  } catch (e) {
    if (!(e instanceof sq.QuerySyntaxError)) throw e;
    rec.error = true;
    out.push(rec);
    continue;
  }
  rec.error = false;
  rec.mode = a.mode;
  rec.printing = a.printing;
  rec.supportsLocal = supportsLocal(a);
  rec.toScryfall = sq.toScryfall(a.ast);
  const stripped = sq.stripDirectives(a.ast);
  rec.walkBase = stripped ? sq.toScryfall(stripped) : null;
  if (a.mode === 'local' || rec.supportsLocal) {
    rec.collection = {};
    for (const userId of [1, 2]) {
      for (const eligibleOnly of [false, true]) {
        const c = rawSql.compileCollectionQuery({ ast: a.ast, userId, limit: null, eligibleOnly });
        const rows = db.prepare(c.sql).all(...c.params).map(r => r.entry_id);
        const n = db.prepare(c.countSql).get(...c.countParams).n;
        rec.collection[`${userId}:${eligibleOnly ? 1 : 0}`] = { rows, n };
      }
    }
    const w = rawSql.compileCollectionQuery({ ast: a.ast, userId: 1, limit: 3, offset: 2, eligibleOnly: false });
    rec.window = db.prepare(w.sql).all(...w.params).map(r => r.entry_id);
    rec.raw = {};
    for (const language of ['English', 'Japanese']) {
      const r = rawSql.compileRawQuery({ ast: a.ast, language, limit: 1000, offset: 0 });
      rec.raw[language] = {
        rows: db.prepare(r.sql).all(...r.params).map(x => x.id),
        n: db.prepare(r.countSql).get(...r.countParams).n,
      };
    }
  }
  out.push(rec);
}
fs.writeFileSync(path.join(__dirname, 'scryq_expected.json'), JSON.stringify(out, null, 1) + '\n');
console.log(`wrote ${out.length} expectations`);
