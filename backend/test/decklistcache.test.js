// deckListCache: a hit only while deck_list_revision is unchanged; DB triggers
// bump it for writes from ANY path (HTTP, Moxfield sync, price sweep).
// Run: `node test/decklistcache.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-decklistcache-${process.pid}.db`);
delete process.env.DECK_LIST_CACHE;
const db = require('../src/db');
const cache = require('../src/utils/deckListCache');

(async () => {
  await db.initDb();
  await db.run(`INSERT INTO users (id, username, password_hash, share_token) VALUES (900, 'dlc', 'x', 'dlc-share')`);
  await db.run(`INSERT INTO card_cache (id, name, price_trend, price_normal) VALUES ('mtg-x', 'X', 1, 1)`);
  let builds = 0;
  const build = async () => ({ n: ++builds });
  const call = () => cache.getOrBuild(900, build);

  assert.strictEqual((await call()).hit, false);
  let r = await call();
  assert.strictEqual(r.hit, true, 'unchanged revision is a hit');
  assert.strictEqual(r.body.n, 1);

  await db.run(`INSERT INTO decks (user_id, name) VALUES (900, 'D')`);
  assert.strictEqual((await call()).hit, false, 'deck insert invalidates');
  assert.strictEqual((await call()).hit, true);

  await db.run(`UPDATE card_cache SET price_trend = 2 WHERE id = 'mtg-x'`);
  assert.strictEqual((await call()).hit, false, 'a price change invalidates');
  assert.strictEqual((await call()).hit, true);

  await db.run(`UPDATE card_cache SET price_trend = 2, last_updated = CURRENT_TIMESTAMP WHERE id = 'mtg-x'`);
  assert.strictEqual((await call()).hit, true, 'a no-op re-cache does not invalidate');

  const deck = await db.get(`SELECT id FROM decks WHERE user_id = 900`);
  await db.run(`INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (?, 'mtg-x', 1)`, [deck.id]);
  assert.strictEqual((await call()).hit, false, 'deck_cards write invalidates');

  process.env.DECK_LIST_CACHE = '0';
  assert.strictEqual((await call()).hit, false, 'kill switch bypasses');
  console.log('decklistcache.test.js: all assertions passed');
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });
