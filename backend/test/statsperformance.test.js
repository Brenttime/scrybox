// Regression coverage for dashboard statistics semantics and bounded query shape.
// Plain node + assert; auto-discovered by test/run.js.
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const dbPath = path.join(os.tmpdir(), `scrybox-stats-performance-${process.pid}.db`);
process.env.DB_PATH = dbPath;
const db = require('../src/db');
const statsRouter = require('../src/routes/stats');

const DAY = 24 * 60 * 60 * 1000;
const NOW = Date.parse('2026-08-31T12:00:00.000Z');
const stamp = daysAgo => new Date(NOW - daysAgo * DAY).toISOString();

function expectedLabels(count, stepDays, options) {
  return Array.from({ length: count }, (_, index) => {
    const daysAgo = (count - 1 - index) * stepDays;
    return new Date(NOW - daysAgo * DAY).toLocaleDateString(undefined, options);
  });
}

function handlerFor(routePath) {
  const layer = statsRouter.stack.find(candidate => candidate.route && candidate.route.path === routePath);
  assert.ok(layer, `missing ${routePath} route`);
  return layer.route.stack[0].handle;
}

async function request(routePath, userId, query = {}) {
  const handler = handlerFor(routePath);
  return new Promise((resolve, reject) => {
    const req = { user: { id: userId }, query };
    const res = {
      statusCode: 200,
      status(code) { this.statusCode = code; return this; },
      json(body) { resolve({ status: this.statusCode, body }); }
    };
    Promise.resolve(handler(req, res)).catch(reject);
  });
}

async function addCard(id, name, setId, setName, rarity, types, subtypes, prices = {}) {
  await db.run(`
    INSERT INTO card_cache
      (id, name, set_id, set_name, rarity, types, subtypes, supertype,
       price_trend, price_normal, price_holofoil)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
  `, [id, name, setId, setName, rarity, JSON.stringify(types), JSON.stringify(subtypes),
    prices.supertype || null, prices.trend ?? null, prices.normal ?? null, prices.holo ?? null]);
}

async function own(userId, cardId, quantity, printing, addedDaysAgo, purchasePrice = null, condition = 'Near Mint') {
  await db.run(`
    INSERT INTO collection
      (user_id, card_id, quantity, printing, added_at, purchase_price, condition)
    VALUES (?, ?, ?, ?, ?, ?, ?)
  `, [userId, cardId, quantity, printing, stamp(addedDaysAgo), purchasePrice, condition]);
}

async function history(cardId, daysAgo, price) {
  await db.run(`INSERT INTO price_history (card_id, price, recorded_at) VALUES (?, ?, ?)`,
    [cardId, price, stamp(daysAgo)]);
}

async function main() {
  const originalDateNow = Date.now;
  Date.now = () => NOW;
  try {
    await db.initDb();
    const smallUser = (await db.run(`
      INSERT INTO users (username, password_hash, role, share_token)
      VALUES ('stats-small', 'x', 'member', 'stats-small-token')
    `)).lastID;
    const largeUser = (await db.run(`
      INSERT INTO users (username, password_hash, role, share_token)
      VALUES ('stats-large', 'x', 'member', 'stats-large-token')
    `)).lastID;
    const edgeUser = (await db.run(`
      INSERT INTO users (username, password_hash, role, share_token)
      VALUES ('stats-edge', 'x', 'member', 'stats-edge-token')
    `)).lastID;

    await db.run(`INSERT INTO sets (id, name, printed_total, total) VALUES
      ('lea', 'Alpha', 295, 295), ('mrd', 'Mirrodin', 306, 306),
      ('xyz', 'Example', 1, 1), ('abc', 'Artifacts', 10, 10),
      ('zset', 'Zed', 10, 10), ('aset', 'Aye', 10, 10), ('same', 'Shared', 10, 10)`);
    await addCard('multi', 'Many Finishes', 'lea', 'Alpha', 'Rare', ['Creature'], ['Wizard'],
      { trend: 11, normal: 10, holo: 20 });
    await addCard('fallback', 'No History Land', 'mrd', 'Mirrodin', 'Common', [], ['Island'],
      { trend: 1, normal: 2, holo: 6 });
    await addCard('late-history', 'Late History', 'xyz', 'Example', 'Mythic', [], [],
      { trend: 100, normal: 100 });
    await addCard('boundary', 'Boundary', 'abc', 'Artifacts', 'Uncommon', ['Artifact', 'Creature'], [],
      { trend: 3, normal: 3 });

    // Four physical rows for one printing identity exercise stacked ownership;
    // history remains card-id scoped, while present value remains finish-aware.
    await own(smallUser, 'multi', 2, 'Normal', 60, 3);
    await own(smallUser, 'multi', 3, 'Holofoil', 40, 4, 'Lightly Played');
    await own(smallUser, 'multi', 4, 'Holofoil', 3, 5);
    await own(smallUser, 'multi', 5, 'Normal', 20, 2);
    await own(smallUser, 'fallback', 2, 'Normal', 10);
    await own(smallUser, 'fallback', 1, 'Holofoil', 10);
    await own(smallUser, 'late-history', 1, 'Normal', 100, 10);
    await own(smallUser, 'boundary', 2, 'Normal', 7, 1);

    await history('multi', 45, 4);
    await history('multi', 30, 5);
    await history('multi', 7, 7);
    await history('multi', 1, 9);
    await history('late-history', 2, 50); // carried backward by timeline semantics
    await history('boundary', 50, 1);
    await history('boundary', 7, 2);      // exact cutoff is inclusive
    await history('boundary', 0, 3);

    // Exercise every timestamp form accepted by parseSqliteUtc. The legacy route
    // orders stored strings and then compares their parsed instants, so these are
    // deliberately equivalent replacements rather than invented extra points.
    await db.run(`UPDATE price_history SET recorded_at = '2026-08-01 12:00:00'
                  WHERE card_id = 'multi' AND price = 5`);
    await db.run(`UPDATE price_history SET recorded_at = '2026-08-30T08:00:00-04:00'
                  WHERE card_id = 'multi' AND price = 9`);
    await db.run(`UPDATE price_history SET recorded_at = '2026-08-24 12:00:00'
                  WHERE card_id = 'boundary' AND price = 2`);
    await db.run(`UPDATE collection SET added_at = '2026-08-21 12:00:00'
                  WHERE user_id = ? AND card_id = 'fallback'`, [smallUser]);

    // A user's timeline must probe history by their owned card IDs. Populate a
    // much larger unrelated global history so the exact route plan can prove it
    // does not choose price_history as the outer side of the join.
    const UNOWNED_HISTORY_ROWS = 50000;
    await db.run(`
      WITH RECURSIVE ids(n) AS (
        VALUES(1) UNION ALL SELECT n + 1 FROM ids WHERE n < ?
      )
      INSERT INTO price_history (card_id, price, recorded_at)
      SELECT printf('unowned-history-%05d', n), 1, '2026-08-01T12:00:00.000Z'
      FROM ids
    `, [UNOWNED_HISTORY_ROWS]);

    // Independent-review regressions: equal-value sets keep first-seen order,
    // a set keeps the name from its first encountered card, and legacy empty
    // JSON text is treated exactly like a missing [] field.
    await addCard('order-z', 'Order Z', 'zset', 'Zed', 'Common', [], [], { trend: 1 });
    await addCard('order-a', 'Order A', 'aset', 'Aye', 'Common', [], [], { trend: 1 });
    await addCard('same-first', 'Same First', 'same', 'Aardvark', 'Common', [], [], { trend: 1 });
    await addCard('same-later', 'Same Later', 'same', 'Zulu', 'Common', [], [], { trend: 1 });
    await db.run(`UPDATE card_cache SET types = '', subtypes = '' WHERE id = 'order-z'`);
    await own(edgeUser, 'order-z', 1, 'Normal', 1);
    await own(edgeUser, 'order-a', 1, 'Normal', 2);
    await own(edgeUser, 'same-first', 1, 'Normal', 3);
    await own(edgeUser, 'same-later', 1, 'Normal', 4);

    const stats = await request('/stats', smallUser);
    assert.strictEqual(stats.status, 200);
    assert.deepStrictEqual(stats.body.summary, {
      totalCards: 20,
      uniqueCards: 8,
      totalValue: 326,
      totalSpent: 60,
      roi: { abs: 266, pct: 443.3 },
      avgCardValue: 16.3,
      duplicateCopies: 12,
      estimatedCards: 0,
      mintRate: 85,
      vintageRatio: 70,
      change7d: { available: true, abs: 22, pct: 29.7 },
      change30d: { available: true, abs: 20, pct: 80 },
      change1y: { available: false, abs: null, pct: null },
      change5y: { available: false, abs: null, pct: null }
    });
    assert.deepStrictEqual(stats.body.types, [
      { name: 'Creature', value: 16 },
      { name: 'Artifact', value: 2 },
      { name: 'Land', value: 3 },
      { name: 'Colorless', value: 1 }
    ]);
    assert.deepStrictEqual(stats.body.rarities, [
      { name: 'Rare', value: 14 },
      { name: 'Uncommon', value: 2 },
      { name: 'Common', value: 3 },
      { name: 'Mythic', value: 1 }
    ]);
    assert.deepStrictEqual(stats.body.sets, [
      { id: 'lea', name: 'Alpha', count: 14, value: 210 },
      { id: 'xyz', name: 'Example', count: 1, value: 100 },
      { id: 'mrd', name: 'Mirrodin', count: 3, value: 10 },
      { id: 'abc', name: 'Artifacts', count: 2, value: 6 }
    ]);
    assert.strictEqual(stats.body.topValuable[0].card_id, 'late-history');
    assert.strictEqual(stats.body.topValuable[0].price_trend, 100);
    assert.strictEqual(stats.body.topValuable.filter(x => x.card_id === 'multi' && x.printing === 'Holofoil')[0].price_trend, 20);
    assert.deepStrictEqual(stats.body.setProgress, [
      { setId: 'xyz', setName: 'Example', ownedUnique: 1, totalCards: 1, percent: 100 },
      { setId: 'abc', setName: 'Artifacts', ownedUnique: 1, totalCards: 10, percent: 10 },
      { setId: 'lea', setName: 'Alpha', ownedUnique: 1, totalCards: 295, percent: 0 },
      { setId: 'mrd', setName: 'Mirrodin', ownedUnique: 1, totalCards: 306, percent: 0 }
    ]);

    const edgeStats = await request('/stats', edgeUser);
    assert.strictEqual(edgeStats.status, 200, JSON.stringify(edgeStats.body));
    assert.deepStrictEqual(edgeStats.body.types, [{ name: 'Colorless', value: 4 }]);
    assert.deepStrictEqual(edgeStats.body.rarities, [{ name: 'Common', value: 4 }]);
    assert.deepStrictEqual(edgeStats.body.sets, [
      { id: 'same', name: 'Aardvark', count: 2, value: 2 },
      { id: 'zset', name: 'Zed', count: 1, value: 1 },
      { id: 'aset', name: 'Aye', count: 1, value: 1 }
    ]);
    assert.strictEqual(edgeStats.body.setProgress.find(set => set.setId === 'same').setName, 'Aardvark');

    let historyQuery;
    const originalAllForPlan = db.all;
    db.all = async (sql, params = []) => {
      if (sql.includes('WITH cards AS MATERIALIZED') && sql.includes('price_history ph')) {
        historyQuery = { sql, params };
      }
      return originalAllForPlan(sql, params);
    };
    let sevenDays;
    try {
      sevenDays = await request('/stats/history', smallUser, { period: '7d' });
    } finally {
      db.all = originalAllForPlan;
    }
    assert.strictEqual(sevenDays.status, 200);
    assert.deepStrictEqual(sevenDays.body, expectedLabels(7, 1, { weekday: 'short' }).map((date, index) => ({
      date,
      value: [134, 134, 134, 162, 162, 190, 192][index]
    })), '7d timeline must preserve labels, carryback, finish fallback, additions, and inclusive cutoffs');

    assert.ok(historyQuery, 'timeline SQL must be captured for plan verification');
    const historyPlan = await db.all(`EXPLAIN QUERY PLAN ${historyQuery.sql}`, historyQuery.params);
    const historyPlanText = historyPlan.map(row => row.detail).join('\n');
    assert.match(historyPlanText,
      /SEARCH ph USING INDEX sqlite_autoindex_price_history_1 \(card_id=\?\)/i,
      `timeline must probe indexed history for each owned card:\n${historyPlanText}`);
    assert.doesNotMatch(historyPlanText, /SCAN ph(?:\s|$)/i,
      `timeline must not scan global price history:\n${historyPlanText}`);

    const thirtyDays = await request('/stats/history', smallUser, { period: '30d' });
    const thirtyDayValues = [
      ...Array(9).fill(75), ...Array(10).fill(100), ...Array(3).fill(110),
      ...Array(4).fill(134), ...Array(2).fill(162), 190, 192
    ];
    assert.deepStrictEqual(thirtyDays.body,
      expectedLabels(30, 1, { month: 'short', day: 'numeric' }).map((date, index) => ({
        date, value: thirtyDayValues[index]
      })), '30d timeline must preserve every label and value');

    const oneYear = await request('/stats/history', smallUser, { period: '1y' });
    assert.deepStrictEqual(oneYear.body,
      expectedLabels(12, 30, { month: 'short', year: '2-digit' }).map((date, index) => ({
        date, value: [...Array(8).fill(0), 50, 58, 75, 192][index]
      })), '1y timeline must preserve 30-day bucket labels and values');

    const fiveYears = await request('/stats/history', smallUser, { period: '5y' });
    assert.deepStrictEqual(fiveYears.body,
      expectedLabels(20, 91, { month: 'short', year: 'numeric' }).map((date, index) => ({
        date, value: [...Array(18).fill(0), 50, 192][index]
      })), '5y timeline must preserve 91-day bucket labels and values');

    // More than SQLite's common 32,766 variable ceiling: neither dashboard route
    // may turn owned IDs into one generated placeholder list.
    const DISTINCT_IDS = 33000;
    await db.run(`
      WITH RECURSIVE ids(n) AS (
        VALUES(1) UNION ALL SELECT n + 1 FROM ids WHERE n < ?
      )
      INSERT INTO card_cache (id, name, set_id, set_name, rarity, types, subtypes, price_trend)
      SELECT printf('scale-%05d', n), printf('Scale card %05d', n), 'scale', 'Scale',
             'Common', '["Creature"]', '[]', 1
      FROM ids
    `, [DISTINCT_IDS]);
    await db.run(`
      INSERT INTO collection (user_id, card_id, quantity, printing, added_at)
      SELECT ?, id, 1, 'Normal', ? FROM card_cache WHERE id LIKE 'scale-%'
    `, [largeUser, stamp(100)]);

    const observedParamCounts = [];
    const originalAll = db.all;
    db.all = async (sql, params = []) => {
      observedParamCounts.push(params.length);
      return originalAll(sql, params);
    };
    try {
      const largeStats = await request('/stats', largeUser);
      assert.strictEqual(largeStats.status, 200, JSON.stringify(largeStats.body));
      assert.strictEqual(largeStats.body.summary.uniqueCards, DISTINCT_IDS);
      const largeHistory = await request('/stats/history', largeUser, { period: '7d' });
      assert.strictEqual(largeHistory.status, 200, JSON.stringify(largeHistory.body));
      assert.deepStrictEqual(largeHistory.body.map(point => point.value), Array(7).fill(DISTINCT_IDS));
    } finally {
      db.all = originalAll;
    }
    assert.ok(Math.max(...observedParamCounts) < 32766,
      `dashboard query used ${Math.max(...observedParamCounts)} bound variables`);

    console.log('statsperformance.test.js: all assertions passed');
  } finally {
    Date.now = originalDateNow;
    for (const suffix of ['', '-wal', '-shm']) {
      try { fs.unlinkSync(dbPath + suffix); } catch { /* already gone */ }
    }
  }
}

main().then(() => process.exit(0)).catch(error => {
  console.error(error);
  process.exit(1);
});
