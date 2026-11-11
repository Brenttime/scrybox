// stats-subs expected-value oracle (iter 25).
//
// Runs INSIDE scrybox-rusttest-node against a copy of the test fixture DB
// (schema_fixture.sql + stats_seed.sql) and prints the byte-exact
// JSON.stringify output the REAL handlers in backend/src/routes/stats.js
// would send for /stats/networth and /stats/top-mover — using the real
// utils/priceHelpers.js and the SQL extracted VERBATIM from the route source
// at runtime, so a drift in either shows up as an oracle change, not a
// silent test pass. Expected constants in tests/stats_subs.rs were pasted
// from this script's output (owner rule: node-generated vectors, never hand
// computed).
//
// Usage (from wt/server/crates/scrybox-server/tests):
//   python3 - <<'EOF'   # rebuild the fixture DB if the .sql fixtures changed
//   import sqlite3
//   conn = sqlite3.connect('/tmp/subs-fixture.db')
//   for f in ('schema_fixture.sql','stats_seed.sql'):
//       conn.executescript(open(f).read())
//   conn.execute("INSERT INTO collection_read_revision (id, revision) VALUES (1, 1)")
//   conn.execute("INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0)")
//   conn.execute("INSERT OR IGNORE INTO stats_aux_revision (id, revision) VALUES (1, 0)")
//   conn.commit()
//   EOF
//   docker cp /tmp/subs-fixture.db scrybox-rusttest-node:/tmp/subs-fixture.db
//   docker cp stats_subs_oracle.mjs scrybox-rusttest-node:/tmp/oracle.mjs
//   docker exec scrybox-rusttest-node node /tmp/oracle.mjs

const fs = require('fs');
const { DatabaseSync } = require('node:sqlite');
const { resolveCardPrice } = require('/app/backend/src/utils/priceHelpers.js');

const db = new DatabaseSync('/tmp/subs-fixture.db');

// --- SQL extracted verbatim from the live route source at run time ---------
const src = fs.readFileSync('/app/backend/src/routes/stats.js', 'utf8');
function between(startMarker, endMarker) {
  const i = src.indexOf(startMarker);
  if (i < 0) throw new Error(`marker not found: ${startMarker}`);
  const j = src.indexOf(endMarker, i);
  if (j < 0) throw new Error(`end marker not found: ${endMarker}`);
  return src.slice(i, j + endMarker.length);
}
const NETWORTH_SQL = between(
  'SELECT c.quantity, c.purchase_price, c.printing, c.is_proxy, cc.price_currency,',
  'WHERE c.user_id = ?',
);
const TOP_MOVER_SQL = between('WITH o AS (', 'ORDER BY gain DESC\n      LIMIT 1');

// --- /stats/networth (stats.js lines 489-522) ------------------------------
function networth(userId) {
  const rows = db.prepare(NETWORTH_SQL).all(userId);
  let totalCards = 0, totalValue = 0, totalSpent = 0;
  const currencies = new Set();
  for (const row of rows) {
    const qty = row.quantity || 1;
    const value = qty * resolveCardPrice(row);
    totalCards += qty;
    totalValue += value;
    totalSpent += qty * (row.purchase_price || 0);
    if (value > 0) currencies.add(row.price_currency || 'USD');
  }
  const round = (n) => parseFloat(n.toFixed(2));
  return JSON.stringify({
    totalValue: round(totalValue),
    totalSpent: round(totalSpent),
    gain: round(totalValue - totalSpent),
    gainPct: totalSpent > 0 ? parseFloat((((totalValue - totalSpent) / totalSpent) * 100).toFixed(1)) : null,
    totalCards,
    uniqueEntries: rows.length,
    currencies: [...currencies].sort(),
    asOf: '__ASOF__',
  });
}

// --- /stats/top-mover (stats.js lines 817-853) -----------------------------
function topMover(userId) {
  const row = db.prepare(TOP_MOVER_SQL).get(userId);
  if (!row) return JSON.stringify({ card: null });
  const { cur, prev, gain, ...card } = row;
  return JSON.stringify({
    card: { ...card, price_trend: cur },
    prev: Math.round(prev * 100) / 100,
    price: Math.round(cur * 100) / 100,
    gain: Math.round(gain * 100) / 100,
    pct: Math.round((gain / prev) * 1000) / 10,
  });
}

console.log(JSON.stringify({
  networth_user1: networth(1),
  networth_user2: networth(2),
  top_mover_user1: topMover(1),
  top_mover_user2: topMover(2),
}, null, 1));
