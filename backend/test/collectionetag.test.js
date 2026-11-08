// collectionEtag: the pre-query tag for paginated GET /api/collection changes
// on every collection or card_cache write (any path), and differs per user/query.
// Run: `node test/collectionetag.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-collectionetag-${process.pid}.db`);
const db = require('../src/db');
const { collectionEtag, matches } = require('../src/utils/collectionEtag');

(async () => {
  await db.initDb();
  await db.run(`INSERT INTO users (id, username, password_hash, share_token) VALUES (901, 'ce', 'x', 'ce-share')`);
  await db.run(`INSERT INTO card_cache (id, name, price_trend) VALUES ('mtg-y', 'Y', 1)`);
  const url = '/api/collection?limit=96&offset=0';
  const tag = () => collectionEtag(db, 901, url);

  const t0 = await tag();
  assert.ok(t0 && t0.startsWith('W/"col-'), 'weak tag');
  assert.strictEqual(await tag(), t0, 'stable while nothing changes');
  assert.notStrictEqual(await collectionEtag(db, 902, url), t0, 'per user');
  assert.notStrictEqual(await collectionEtag(db, 901, url + '&x=1'), t0, 'per query');

  await db.run(`INSERT INTO collection (card_id, user_id, quantity) VALUES ('mtg-y', 901, 1)`);
  const t1 = await tag();
  assert.notStrictEqual(t1, t0, 'collection insert changes the tag');

  await db.run(`UPDATE card_cache SET last_updated = CURRENT_TIMESTAMP WHERE id = 'mtg-y'`);
  const t2 = await tag();
  assert.notStrictEqual(t2, t1, 'any card_cache update (e.g. price sweep, rarity fix) changes the tag');

  await db.run(`DELETE FROM collection WHERE user_id = 901`);
  assert.notStrictEqual(await tag(), t2, 'delete changes the tag');

  const fake = (inm) => ({ get: (h) => (h === 'If-None-Match' ? inm : undefined) });
  const t3 = await tag();
  assert.strictEqual(matches(fake(t3), t3), true);
  assert.strictEqual(matches(fake(`W/"a", ${t3}`), t3), true, 'list form');
  assert.strictEqual(matches(fake(t2), t3), false, 'old tag is not a match');
  assert.strictEqual(matches(fake(undefined), t3), false);
  console.log('collectionetag.test.js: all assertions passed');
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });
