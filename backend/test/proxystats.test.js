// Proxy copies count $0 in dashboard totals and are their own 'Proxy' rarity.
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const dbPath = path.join(os.tmpdir(), `scrybox-proxystats-${process.pid}.db`);
process.env.DB_PATH = dbPath;
const db = require('../src/db');
const statsRouter = require('../src/routes/stats');

function call(routePath, userId) {
  const layer = statsRouter.stack.find(l => l.route && l.route.path === routePath);
  return new Promise((resolve, reject) => {
    const res = { statusCode: 200, status(c) { this.statusCode = c; return this; }, json(body) { resolve(body); } };
    Promise.resolve(layer.route.stack[0].handle({ user: { id: userId }, query: {} }, res)).catch(reject);
  });
}

async function main() {
  try {
    await db.initDb();
    const cols = await db.all(`PRAGMA table_info(collection)`);
    assert.ok(cols.some(c => c.name === 'is_proxy'), 'collection.is_proxy exists');
    const user = (await db.run(`INSERT INTO users (username, password_hash, role, share_token) VALUES ('px', 'x', 'member', 'px-token')`)).lastID;
    await db.run(`INSERT INTO card_cache (id, name, set_id, set_name, rarity, types, subtypes, price_trend, price_normal)
                  VALUES ('bolt', 'Bolt', 'lea', 'Alpha', 'Rare', '["Instant"]', '[]', 100, 100)`);
    await db.run(`INSERT INTO collection (user_id, card_id, quantity, printing, purchase_price) VALUES (?, 'bolt', 1, 'Normal', 5)`, [user]);
    const proxyId = (await db.run(`INSERT INTO collection (user_id, card_id, quantity, printing, purchase_price, is_proxy) VALUES (?, 'bolt', 2, 'Normal', 1, 1)`, [user])).lastID;

    const stats = await call('/stats', user);
    assert.strictEqual(stats.summary.totalValue, 100, 'only the real copy is valued');
    assert.strictEqual(stats.summary.totalCards, 3, 'proxies still count as cards');
    const rarities = Object.fromEntries(stats.rarities.map(r => [r.name, r.value]));
    assert.deepStrictEqual(rarities, { Rare: 1, Proxy: 2 });
    const net = await call('/stats/networth', user);
    assert.strictEqual(net.totalValue, 100);

    await db.run(`UPDATE collection SET is_proxy = 0 WHERE id = ?`, [proxyId]);
    const restored = await call('/stats', user);
    assert.strictEqual(restored.summary.totalValue, 300, 'unmarking restores value');
    const cache = await db.get(`SELECT price_normal FROM card_cache WHERE id = 'bolt'`);
    assert.strictEqual(cache.price_normal, 100, 'card_cache price untouched');
    console.log('proxystats.test.js: all assertions passed');
  } finally {
    for (const s of ['', '-wal', '-shm']) { try { fs.unlinkSync(dbPath + s); } catch { /* gone */ } }
  }
}
main().then(() => process.exit(0)).catch(e => { console.error(e); process.exit(1); });
