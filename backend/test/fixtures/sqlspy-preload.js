// Trace executed SQL AND rawQuerySql compiler invocations to a JSONL file so
// a test can prove WHICH engine answered a collection search. Loaded into the
// server child via NODE_OPTIONS=--require, which runs it before server.js
// pins any db/rawQuerySql references — the wrappers land on the very module
// instances the routes use later. 'compile' lines mean the divergent local
// SQL compiler was asked to answer; a sidecar-mode syntax search must produce
// ZERO of them. Never throws into the server: a broken spy is invisible.
const fs = require('fs');
const path = require('path');

const outFile = process.env.SCRYBOX_SQLSPY_FILE;
if (!outFile) throw new Error('sqlspy: SCRYBOX_SQLSPY_FILE is not set');
const db = require(path.join(__dirname, '..', '..', 'src', 'db.js'));
const rawQuerySql = require(path.join(__dirname, '..', '..', 'src', 'utils', 'rawQuerySql.js'));

const note = (obj) => {
  try { fs.appendFileSync(outFile, JSON.stringify(obj) + '\n'); } catch { /* never break the server */ }
};
const flatten = (sql) => String(sql).replace(/\s+/g, ' ').trim().slice(0, 400);

for (const name of ['run', 'get', 'all']) {
  const orig = db[name];
  db[name] = function (sql, params) { note({ t: 'sql', sql: flatten(sql) }); return orig.call(db, sql, params); };
}
const origTx = db.withDedicatedReadTransaction;
db.withDedicatedReadTransaction = function (fn) {
  return origTx.call(db, async (client) => {
    const wrapped = Object.create(client);
    for (const name of ['run', 'get', 'all']) {
      const orig = client[name].bind(client);
      wrapped[name] = (sql, params) => { note({ t: 'sql', sql: flatten(sql) }); return orig(sql, params); };
    }
    return fn(wrapped);
  });
};
const origCompile = rawQuerySql.compileCollectionQuery;
rawQuerySql.compileCollectionQuery = function (opts) {
  const out = origCompile.call(this, opts);
  note({ t: 'compile', sql: flatten(out.sql) });
  return out;
};
