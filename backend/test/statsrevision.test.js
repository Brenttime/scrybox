// statsCache revision: the stats cache key changes on background writes to every
// table the stats read (collection, card_cache, decks/deck_cards, price_history,
// sets), not only on HTTP writes by the user. Run: `node test/statsrevision.test.js`
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-statsrevision-${process.pid}.db`);
delete process.env.STATS_CACHE;
const db = require('../src/db');
const { dataRevision } = require('../src/utils/statsCache');

(async () => {
  await db.initDb();
  await db.run(`INSERT INTO users (id, username, password_hash, share_token) VALUES (903, 'sr', 'x', 'sr-share')`);
  const r0 = await dataRevision();
  assert.ok(r0 && !r0.includes('-'), `all three counters exist: ${r0}`);
  assert.strictEqual(await dataRevision(), r0, 'stable while nothing changes');

  const steps = [
    ['card_cache insert', `INSERT INTO card_cache (id, name, price_trend) VALUES ('mtg-sr', 'SR', 1)`],
    ['collection insert', `INSERT INTO collection (card_id, user_id, quantity) VALUES ('mtg-sr', 903, 1)`],
    ['price sweep update', `UPDATE card_cache SET price_trend = 2 WHERE id = 'mtg-sr'`],
    ['price_history insert', `INSERT INTO price_history (card_id, price, recorded_at) VALUES ('mtg-sr', 2, '2026-01-01 00:00:00')`],
    ['sets insert', `INSERT INTO sets (id, name) VALUES ('mtg-srs', 'SR Set')`],
    ['deck insert', `INSERT INTO decks (user_id, name) VALUES (903, 'SR deck')`],
  ];
  let prev = r0;
  for (const [label, sql] of steps) {
    await db.run(sql);
    const next = await dataRevision();
    assert.notStrictEqual(next, prev, `${label} changes the stats revision`);
    prev = next;
  }
  console.log('statsrevision.test.js: all assertions passed');
})().then(() => 0, (e) => { console.error(e); return 1; }).then((code) => {
  for (const s of ['', '-wal', '-shm']) { try { fs.unlinkSync(process.env.DB_PATH + s); } catch { /* gone */ } }
  process.exit(code);
});
