// Lane B unit 4: generate the Node baseline for the Rust-owned migrations.
// Run INSIDE the scrybox-rusttest-node container (has /app/src/db.js +
// node_modules/sqlite3):
//   docker cp server/bench/mig-node-baseline.cjs scrybox-rusttest-node:/tmp/
//   docker exec -e CV_MODEL_DIR= scrybox-rusttest-node node /tmp/mig-node-baseline.cjs fresh   /app/database/mig-base/fresh.db
//   docker exec -e CV_MODEL_DIR= scrybox-rusttest-node node /tmp/mig-node-baseline.cjs upgrade /app/database/mig-base/legacy1.db /app/database/mig-base/legacy_schema.sql
// Writes the canonical dump to stdout; commit it as tests/migrations_golden/<mode><N>.txt.
//
// The dump format must stay byte-identical to dump() in
// crates/scrybox-server/tests/migrations.rs:
//   S|type|name|esc(sql)                  (sqlite_master, ORDER BY type,name)
//   T|table|col1,col2,...                 (PRAGMA table_info cid order)
//   R|table|rowid|v1\x1fv2...             (CAST(col AS TEXT), NULL = \N)
// esc: backslash, then \n, then \r, then \x1f -> literal escapes.
const fs = require('fs');
const sqlite3 = require('sqlite3');

const [mode, dbPath, legacySqlPath] = process.argv.slice(2);
if (!mode || !dbPath || !['fresh', 'upgrade', 'attach'].includes(mode)) {
  console.error('usage: mig-node-baseline.cjs <fresh|upgrade|attach> <dbPath> [legacy.sql]');
  process.exit(2);
}

const esc = (s) =>
  String(s)
    .replace(/\\/g, '\\\\')
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r')
    .replace(/\u001f/g, '\\u001f');

async function main() {
  fs.mkdirSync(require('path').dirname(dbPath), { recursive: true });
  if (mode !== 'attach') {
    for (const suffix of ['', '-wal', '-shm']) {
      try { fs.rmSync(dbPath + suffix); } catch { /* absent */ }
    }
  }

  if (legacySqlPath) {
    const seed = new sqlite3.Database(dbPath);
    await new Promise((res, rej) =>
      seed.exec(fs.readFileSync(legacySqlPath, 'utf8'), (e) => (e ? rej(e) : res())));
    await new Promise((res) => seed.close(res));
  }

  process.env.DB_PATH = dbPath;
  delete process.env.DEFAULT_ADMIN_PASSWORD;
  delete process.env.CV_MODEL_DIR; // cardArt MODEL_DIR absent = clean sweep
  const db = require('/app/backend/src/db.js');
  await db.initDb();

  const d = new sqlite3.Database(dbPath, sqlite3.OPEN_READONLY);
  const all = (sql) => new Promise((res, rej) => d.all(sql, (e, rows) => (e ? rej(e) : res(rows))));
  const out = [];
  const master = await all('SELECT type, name, sql FROM sqlite_master ORDER BY type, name');
  for (const r of master) out.push(`S|${r.type}|${r.name}|${r.sql === null ? '\\N' : esc(r.sql)}`);
  const tables = await all(
    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
  );
  for (const t of tables) {
    const cols = await all(`PRAGMA table_info("${t.name}")`);
    const names = cols.map((c) => c.name);
    out.push(`T|${t.name}|${names.join(',')}`);
    if (!names.length) continue;
    const casts = names
      .map(
        (n, i) =>
          `CASE WHEN typeof("${n}") = 'blob' THEN hex("${n}") ELSE CAST("${n}" AS TEXT) END AS "c${i}"`
      )
      .join(', ');
    const rows = await all(`SELECT rowid AS __rid, ${casts} FROM "${t.name}" ORDER BY rowid`);
    for (const row of rows) {
      const vals = names.map((_, i) => (row[`c${i}`] === null ? '\\N' : esc(row[`c${i}`])));
      out.push(`R|${t.name}|${row.__rid}|${vals.join('\u001f')}`);
    }
  }
  await new Promise((res) => d.close(res));
  process.stdout.write(out.join('\n') + '\n');
}

main().catch((e) => { console.error(e); process.exit(1); });
