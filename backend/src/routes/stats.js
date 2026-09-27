const express = require('express');
const db = require('../db');
const { resolveCardPrice, isVintageSet, parseSqliteUtc, applyProxyRarity } = require('../utils/priceHelpers');

const router = express.Router();

// 7. Get Collection Statistics & Analytics
router.get('/stats', async (req, res) => {
  try {
    const now = Date.now();
    const oneDayMs = 24 * 60 * 60 * 1000;
    const sevenDaysMs = 7 * oneDayMs;
    const thirtyDaysMs = 30 * oneDayMs;
    const cutoff7 = new Date(now - sevenDaysMs).toISOString();
    const cutoff30 = new Date(now - thirtyDaysMs).toISOString();

    // Materialize the user's joined collection once inside SQLite, then reduce it
    // there. The old route copied every wide collection row plus every historical
    // point into JS. This query returns one row and performs three indexed history
    // lookups per distinct owned card; its parameter count is constant.
    const [aggregate] = await db.all(`
      WITH owned AS MATERIALIZED (
        SELECT c.id, c.card_id,
               ROW_NUMBER() OVER (ORDER BY c.added_at DESC, c.id DESC) AS source_order,
               CASE WHEN c.quantity IS NULL OR c.quantity = 0 THEN 1 ELSE c.quantity END AS qty,
               COALESCE(c.purchase_price, 0) AS purchase_price,
               c.condition, c.added_at, c.printing, COALESCE(c.is_proxy, 0) AS is_proxy,
               COALESCE(NULLIF(cc.types, ''), '[]') AS types,
               COALESCE(NULLIF(cc.subtypes, ''), '[]') AS subtypes,
               cc.supertype, CASE WHEN COALESCE(c.is_proxy, 0) = 1 THEN 'Proxy' ELSE cc.rarity END AS rarity,
               cc.set_name, cc.set_id,
               CASE
                 WHEN COALESCE(c.is_proxy, 0) = 1 THEN 0
                 WHEN c.printing = 'Holofoil' AND cc.price_holofoil IS NOT NULL AND cc.price_holofoil > 0 THEN cc.price_holofoil
                 WHEN c.printing = 'Normal' AND cc.price_normal IS NOT NULL AND cc.price_normal > 0 THEN cc.price_normal
                 ELSE COALESCE(cc.price_trend, 0)
               END AS current_price
        FROM collection c
        JOIN card_cache cc ON c.card_id = cc.id
        WHERE c.user_id = ?
      ),
      card_stats AS MATERIALIZED (
        SELECT card_id, types, subtypes, supertype, rarity, set_name, set_id,
               SUM(qty) AS qty, SUM(qty * current_price) AS current_value,
               MIN(source_order) AS first_order
        FROM owned
        GROUP BY card_id
      ),
      rarity_stats AS (
        -- From the owned rows, not card_stats: one card can hold both real and
        -- proxy copies, and the proxies count under their own 'Proxy' rarity.
        SELECT COALESCE(NULLIF(rarity, ''), 'Unknown') AS name, SUM(qty) AS value,
               MIN(source_order) AS first_order
        FROM owned
        GROUP BY COALESCE(NULLIF(rarity, ''), 'Unknown')
      ),
      set_cards AS MATERIALIZED (
        SELECT cs.*,
               ROW_NUMBER() OVER (PARTITION BY set_id ORDER BY first_order) AS set_order
        FROM card_stats cs
      ),
      set_stats AS (
        SELECT COALESCE(CAST(sc.set_id AS TEXT), 'null') AS id,
               COALESCE(NULLIF(MAX(CASE WHEN sc.set_order = 1 THEN sc.set_name END), ''), 'Other') AS name,
               SUM(sc.qty) AS count, SUM(sc.current_value) AS value,
               COUNT(*) AS owned_unique,
               MAX(COALESCE(NULLIF(s.printed_total, 0), NULLIF(s.total, 0))) AS size,
               MIN(sc.first_order) AS first_order
        FROM set_cards sc
        LEFT JOIN sets s ON s.id = sc.set_id
        GROUP BY sc.set_id
      ),
      typed_cards AS MATERIALIZED (
        SELECT cs.*,
               CASE
                 WHEN EXISTS (SELECT 1 FROM json_each(COALESCE(cs.subtypes, '[]')) WHERE value = 'Land')
                   OR cs.supertype = 'Land'
                   OR (json_array_length(COALESCE(cs.types, '[]')) = 0
                       AND EXISTS (SELECT 1 FROM json_each(COALESCE(cs.subtypes, '[]'))
                                   WHERE value IN ('Plains', 'Island', 'Swamp', 'Mountain', 'Forest', 'Land')))
                   THEN 'land'
                 WHEN json_array_length(COALESCE(cs.types, '[]')) = 0 THEN 'colorless'
                 ELSE 'typed'
               END AS type_class
        FROM card_stats cs
      ),
      type_rows AS (
        SELECT card_id, qty, 'Land' AS name, first_order, 0 AS type_order
        FROM typed_cards WHERE type_class = 'land'
        UNION ALL
        SELECT card_id, qty, 'Colorless', first_order, 0 FROM typed_cards WHERE type_class = 'colorless'
        UNION ALL
        SELECT tc.card_id, tc.qty, jt.value, tc.first_order, CAST(jt.key AS INTEGER)
        FROM typed_cards tc, json_each(COALESCE(tc.types, '[]')) jt
        WHERE tc.type_class = 'typed'
      ),
      type_stats AS (
        SELECT name, SUM(qty) AS value,
               MIN((first_order * 1000) + type_order) AS first_position
        FROM type_rows
        GROUP BY name
      ),
      change_owned AS (
        SELECT card_id,
               SUM(CASE WHEN julianday(added_at) <= julianday(?) THEN qty ELSE 0 END) AS qty7,
               SUM(CASE WHEN julianday(added_at) <= julianday(?) THEN qty ELSE 0 END) AS qty30
        FROM owned
        WHERE is_proxy = 0
        GROUP BY card_id
      ),
      change_prices AS MATERIALIZED (
        SELECT co.*,
               (SELECT price FROM price_history ph
                WHERE ph.card_id = co.card_id
                ORDER BY ph.recorded_at DESC LIMIT 1) AS current_history,
               (SELECT price FROM price_history ph
                WHERE ph.card_id = co.card_id AND julianday(ph.recorded_at) <= julianday(?)
                ORDER BY ph.recorded_at DESC LIMIT 1) AS history7,
               (SELECT price FROM price_history ph
                WHERE ph.card_id = co.card_id AND julianday(ph.recorded_at) <= julianday(?)
                ORDER BY ph.recorded_at DESC LIMIT 1) AS history30
        FROM change_owned co
      )
      SELECT COALESCE(SUM(qty), 0) AS total_cards,
             COUNT(*) AS unique_cards,
             COALESCE(SUM(qty * current_price), 0) AS total_value,
             COALESCE(SUM(qty * purchase_price), 0) AS total_spent,
             COALESCE(SUM(CASE WHEN condition = 'Near Mint' THEN qty ELSE 0 END), 0) AS near_mint_count,
             (SELECT json_group_array(json_object('name', name, 'value', value))
                FROM (SELECT name, value FROM rarity_stats ORDER BY first_order)) AS rarities,
             (SELECT json_group_array(json_object('id', id, 'name', name, 'count', count,
                                                   'value', value, 'ownedUnique', owned_unique, 'size', size))
                FROM (SELECT id, name, count, value, owned_unique, size
                      FROM set_stats ORDER BY first_order)) AS sets,
             (SELECT json_group_array(json_object('name', name, 'value', value))
                FROM (SELECT name, value FROM type_stats ORDER BY first_position)) AS types,
             (SELECT COALESCE(SUM(CASE WHEN qty7 <> 0 AND history7 IS NOT NULL AND current_history IS NOT NULL
                                      THEN qty7 * history7 ELSE 0 END), 0) FROM change_prices) AS value7_ago,
             (SELECT COALESCE(SUM(CASE WHEN qty7 <> 0 AND history7 IS NOT NULL AND current_history IS NOT NULL
                                      THEN qty7 * current_history ELSE 0 END), 0) FROM change_prices) AS value7_now,
             (SELECT COALESCE(SUM(CASE WHEN qty30 <> 0 AND history30 IS NOT NULL AND current_history IS NOT NULL
                                      THEN qty30 * history30 ELSE 0 END), 0) FROM change_prices) AS value30_ago,
             (SELECT COALESCE(SUM(CASE WHEN qty30 <> 0 AND history30 IS NOT NULL AND current_history IS NOT NULL
                                      THEN qty30 * current_history ELSE 0 END), 0) FROM change_prices) AS value30_now
      FROM owned
    `, [req.user.id, cutoff7, cutoff30, cutoff7, cutoff30]);

    const totalCards = aggregate.total_cards;
    const uniqueCards = aggregate.unique_cards;
    const totalValue = aggregate.total_value;
    const totalSpent = aggregate.total_spent;
    const nearMintCount = aggregate.near_mint_count;
    const types = JSON.parse(aggregate.types || '[]');
    const rarities = JSON.parse(aggregate.rarities || '[]');
    const allSets = JSON.parse(aggregate.sets || '[]');
    const vintageCount = allSets.reduce((sum, set) => sum + (isVintageSet(set.id) ? set.count : 0), 0);
    const value7dAgo = aggregate.value7_ago;
    const valueNowFor7d = aggregate.value7_now;
    const value30dAgo = aggregate.value30_ago;
    const valueNowFor30d = aggregate.value30_now;

    // Get top most valuable cards (scoped to user)
    const topValuableQuery = `
      SELECT
        c.id AS entry_id,
        c.quantity, c.condition, c.printing, c.language, c.purchase_price, c.is_trade, c.is_proxy,
        cc.id as card_id, cc.name, cc.printed_name, cc.rarity, cc.set_name, cc.set_id, cc.number, cc.image_url,
        cc.supertype, cc.subtypes, cc.types, cc.cmc, cc.color_identity, cc.price_trend,
        cc.price_normal, cc.price_holofoil
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.user_id = ?
      ORDER BY CASE
        WHEN COALESCE(c.is_proxy, 0) = 1 THEN 0
        WHEN c.printing = 'Holofoil' AND cc.price_holofoil IS NOT NULL AND cc.price_holofoil > 0 THEN cc.price_holofoil
        WHEN c.printing = 'Normal' AND cc.price_normal IS NOT NULL AND cc.price_normal > 0 THEN cc.price_normal
        ELSE cc.price_trend
      END DESC
      LIMIT 6
    `;
    const topValuableRows = await db.all(topValuableQuery, [req.user.id]);
    const topValuable = topValuableRows.map(row => ({
      ...applyProxyRarity(row),
      price_trend: resolveCardPrice(row)
    }));

    // Set completion.
    //
    // Sizes come from the `sets` table, which every provider sync fills in — not
    // from a hand-kept map. That map listed a handful of ids and fell back to a
    // flat 150 for everything else, so every set
    // released after 151 was measured against a number nobody chose. printed_total
    // is the right column (the number printed on the card, which is what a player
    // counts to); `total` includes cards outside the base numbered set and is the fallback when a provider
    // gives no printed count.
    //
    // One query for the whole thing, rather than one per set inside a loop: this
    // ran a COUNT(DISTINCT) per set the user owns cards from, which on a broad
    // collection is dozens of round trips to answer a single panel.
    const setProgress = allSets.filter(set => set.size).map(set => ({
      setId: set.id,
      setName: set.name,
      ownedUnique: set.ownedUnique,
      totalCards: set.size,
      percent: Math.min(Math.round((set.ownedUnique / set.size) * 100), 100)
    }));

    // GROUP BY set_id produced SQLite's binary set-id order in the legacy
    // progress query. Keep that as the stable tie-breaker after percentage.
    setProgress.sort((a, b) => {
      const byPercent = b.percent - a.percent;
      if (byPercent) return byPercent;
      return a.setId < b.setId ? -1 : (a.setId > b.setId ? 1 : 0);
    });

    const mintRate = totalCards > 0 ? parseFloat(((nearMintCount / totalCards) * 100).toFixed(1)) : 0.0;
    const vintageRatio = totalCards > 0 ? parseFloat(((vintageCount / totalCards) * 100).toFixed(1)) : 0.0;

    // Recently added cards (most useful "what did I just add" glance)
    const recentRows = await db.all(`
      SELECT c.id AS entry_id,
             c.quantity, c.condition, c.printing, c.language, c.added_at, c.is_trade, c.is_proxy,
             cc.id as card_id, cc.name, cc.printed_name, cc.rarity, cc.set_name, cc.set_id, cc.number, cc.image_url,
             cc.supertype, cc.subtypes, cc.types, cc.cmc, cc.color_identity,
             cc.price_trend, cc.price_normal, cc.price_holofoil
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.user_id = ?
      ORDER BY c.added_at DESC
      LIMIT 6
    `, [req.user.id]);
    const recentAdditions = recentRows.map(row => ({ ...applyProxyRarity(row), price_trend: resolveCardPrice(row) }));

    const gainAbs = totalValue - totalSpent;
    const roi = {
      abs: parseFloat(gainAbs.toFixed(2)),
      pct: totalSpent > 0 ? parseFloat(((gainAbs / totalSpent) * 100).toFixed(1)) : null
    };
    const avgCardValue = totalCards > 0 ? parseFloat((totalValue / totalCards).toFixed(2)) : 0.0;

    res.json({
      summary: {
        totalCards,
        uniqueCards,
        totalValue: parseFloat(totalValue.toFixed(2)),
        totalSpent: parseFloat(totalSpent.toFixed(2)),
        roi,
        avgCardValue,
        duplicateCopies: Math.max(totalCards - uniqueCards, 0),
        mintRate,
        vintageRatio,
        // change7d/change30d compare recorded snapshots over the same subset
        // of cards. Longer windows stay unavailable until the API exposes them.
        change7d: value7dAgo > 0 ? {
          available: true,
          abs: parseFloat((valueNowFor7d - value7dAgo).toFixed(2)),
          pct: parseFloat((((valueNowFor7d - value7dAgo) / value7dAgo) * 100).toFixed(1))
        } : { available: false, abs: null, pct: null },
        change30d: value30dAgo > 0 ? {
          available: true,
          abs: parseFloat((valueNowFor30d - value30dAgo).toFixed(2)),
          pct: parseFloat((((valueNowFor30d - value30dAgo) / value30dAgo) * 100).toFixed(1))
        } : { available: false, abs: null, pct: null },
        change1y: { available: false, abs: null, pct: null },
        change5y: { available: false, abs: null, pct: null }
      },
      types,
      rarities,
      sets: allSets.map(set => ({
        id: set.id,
        name: set.name,
        count: set.count,
        value: parseFloat(set.value.toFixed(2))
      })).sort((a, b) => b.value - a.value).slice(0, 8),
      topValuable,
      recentAdditions,
      setProgress: setProgress.slice(0, 4)
    });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to compute statistics' });
  }
});

// 7b. Get Collection Net Worth Timeline History
router.get('/stats/history', async (req, res) => {
  try {
    const { period = '30d' } = req.query;

    const now = Date.now();
    let step = 0;
    let count = 0;
    let formatLabel = (d) => d.toLocaleDateString();

    if (period === '7d') {
      count = 7;
      step = 24 * 60 * 60 * 1000;
      formatLabel = (d) => d.toLocaleDateString(undefined, { weekday: 'short' });
    } else if (period === '30d') {
      count = 30;
      step = 24 * 60 * 60 * 1000;
      formatLabel = (d) => d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
    } else if (period === '1y') {
      count = 12;
      step = 30 * 24 * 60 * 60 * 1000;
      formatLabel = (d) => d.toLocaleDateString(undefined, { month: 'short', year: '2-digit' });
    } else if (period === '5y') {
      count = 20;
      step = 91 * 24 * 60 * 60 * 1000;
      formatLabel = (d) => d.toLocaleDateString(undefined, { month: 'short', year: 'numeric' });
    } else {
      count = 30;
      step = 24 * 60 * 60 * 1000;
      formatLabel = (d) => d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
    }

    const targets = Array.from({ length: count }, (_, index) => now - ((count - 1 - index) * step));

    // Collection additions and price movements are events. Read each once, then
    // sweep the (at most 30) targets in order. SQLite packs each card's physical
    // entries and recorded prices into JSON so the driver returns one row per card,
    // not tens of thousands of tiny row objects. The CROSS JOIN deliberately keeps
    // those owned cards on the outer side, making SQLite search price_history's
    // primary key per card instead of scanning global history. Its parameter count
    // stays constant; sorting each decoded history by the stored timestamp preserves
    // the old route's complete recorded_at stream and carry-back rules.
    const cards = await db.all(`
      WITH cards AS MATERIALIZED (
        SELECT c.card_id,
               json_group_array(json_array(
                 c.quantity, c.added_at, c.printing, COALESCE(c.is_proxy, 0),
                 cc.price_trend, cc.price_normal, cc.price_holofoil
               )) AS additions
        FROM collection c
        JOIN card_cache cc ON cc.id = c.card_id
        WHERE c.user_id = ?
        GROUP BY c.card_id
      ),
      histories AS MATERIALIZED (
        SELECT ph.card_id,
               json_group_array(json_array(ph.price, ph.recorded_at)) AS history
        FROM cards c
        CROSS JOIN price_history ph
        WHERE ph.card_id = c.card_id
        GROUP BY ph.card_id
      )
      SELECT c.card_id, c.additions, COALESCE(h.history, '[]') AS history
      FROM cards c
      LEFT JOIN histories h ON h.card_id = c.card_id
    `, [req.user.id]);

    for (const card of cards) {
      card.additions = JSON.parse(card.additions || '[]').map(([
        quantity, added_at, printing, is_proxy, price_trend, price_normal, price_holofoil
      ]) => {
        const time = parseSqliteUtc(added_at).getTime();
        if (!Number.isFinite(time)) return null;
        return {
          time,
          quantity,
          currentValue: quantity * resolveCardPrice({
            printing, is_proxy, price_trend, price_normal, price_holofoil
          })
        };
      }).filter(Boolean);
      card.history = JSON.parse(card.history || '[]').map(([price, recordedAt]) => ({
        price,
        recordedAt,
        time: parseSqliteUtc(recordedAt).getTime()
      })).sort((a, b) => a.recordedAt < b.recordedAt ? -1 : (a.recordedAt > b.recordedAt ? 1 : 0));
    }

    const totals = Array(count).fill(0);
    for (const card of cards.values()) {
      card.additions.sort((a, b) => a.time - b.time);
      let additionIndex = 0;
      let ownedQuantity = 0;
      let ownedCurrentValue = 0;
      let historyIndex = -1;

      for (let index = 0; index < targets.length; index++) {
        const target = targets[index];
        while (additionIndex < card.additions.length && card.additions[additionIndex].time <= target) {
          ownedQuantity += card.additions[additionIndex].quantity;
          ownedCurrentValue += card.additions[additionIndex].currentValue;
          additionIndex++;
        }
        if (card.history.length === 0) {
          totals[index] += ownedCurrentValue;
          continue;
        }
        while (historyIndex + 1 < card.history.length && card.history[historyIndex + 1].time <= target) {
          historyIndex++;
        }
        const price = historyIndex >= 0 ? card.history[historyIndex].price : card.history[0].price;
        totals[index] += ownedQuantity * price;
      }
    }

    const historyData = targets.map((target, index) => ({
      date: formatLabel(new Date(target)),
      value: parseFloat(totals[index].toFixed(2))
    }));

    res.json(historyData);
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to compute timeline history' });
  }
});

// 7c. Net worth, on its own, for scripts and dashboards (issue #33).
//
// /stats already contains these numbers, but it also runs the type/rarity/set
// aggregation, a per-set progress query and two top-N queries to get there —
// which is the wrong thing to hand a finance tracker polling every five minutes.
// This is one pass over the collection and nothing else.
//
// Pair it with an API key (Settings -> API access): that credential is read-only
// and does not expire, so an external tracker keeps working without a login.
router.get('/stats/networth', async (req, res) => {
  try {
    const rows = await db.all(`
      SELECT c.quantity, c.purchase_price, c.printing, c.is_proxy, cc.price_currency,
             cc.price_trend, cc.price_normal, cc.price_holofoil
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.user_id = ?
    `, [req.user.id]);

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
    res.json({
      totalValue: round(totalValue),
      totalSpent: round(totalSpent),
      // The unrealized gain, which is the number a finance tracker actually wants
      // next to the total: value minus what it cost. Null percentage rather than
      // zero when nothing has a purchase price, because "0% return" is a claim.
      gain: round(totalValue - totalSpent),
      gainPct: totalSpent > 0 ? parseFloat((((totalValue - totalSpent) / totalSpent) * 100).toFixed(1)) : null,
      totalCards,
      uniqueEntries: rows.length,
      // Prices are USD (Scryfall); kept for consumers converting.
      currencies: [...currencies].sort(),
      asOf: new Date().toISOString(),
    });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to compute net worth' });
  }
});

// Two windows: the last 30 days of snapshots, or everything Scrybox has recorded.
const PRICE_HISTORY_RANGES = { '30d': 30 };

// Rules (Oracle) text for one card. Served from card_cache; rows cached before
// the oracle columns existed are filled from Scryfall on first view.
router.get('/cards/:id/oracle', async (req, res) => {
  try {
    const data = await require('../scryfallApi').getOracleText(req.params.id);
    if (!data) return res.status(404).json({ error: 'Card not found' });
    res.json(data);
  } catch (err) {
    console.error('oracle text lookup failed:', err.message);
    res.status(502).json({ error: 'Could not load rules text' });
  }
});

// Get Card Price History
router.get('/cards/:id/price-history', async (req, res) => {
  const { id } = req.params;
  const rangeKey = String(req.query.range || '30d').toLowerCase();
  const days = PRICE_HISTORY_RANGES[rangeKey]; // undefined => 'all'
  try {
    const recorded = days
      ? await db.all(`
          SELECT price, recorded_at
          FROM price_history
          WHERE card_id = ? AND recorded_at >= datetime('now', ?)
          ORDER BY recorded_at ASC
        `, [id, `-${days} days`])
      : await db.all(`
          SELECT price, recorded_at
          FROM price_history
          WHERE card_id = ?
          ORDER BY recorded_at ASC
        `, [id]);

    const points = recorded.map(h => ({
      price: h.price,
      time: parseSqliteUtc(h.recorded_at).getTime(),
      source: 'recorded'
    }));
    const recordedCount = points.length;

    points.sort((a, b) => a.time - b.time);

    // Collapse flat runs. The sweep used to write a row on every boot whether or
    // not the price moved, so cards carry hundreds of identical snapshots. Only
    // the ENDS of a flat stretch carry information — the interior points draw
    // the same horizontal line. Keeping both ends preserves its true duration.
    const data = [];
    for (let i = 0; i < points.length; i++) {
      const prev = points[i - 1];
      const next = points[i + 1];
      if (prev && next && prev.price === points[i].price && next.price === points[i].price) continue;
      data.push(points[i]);
    }

    const times = points.map(p => p.time);
    const spanDays = times.length >= 2
      ? Math.round((Math.max(...times) - Math.min(...times)) / 86400000)
      : 0;

    res.json({
      data: data.map(p => ({ price: p.price, recorded_at: new Date(p.time).toISOString(), source: p.source })),
      // What the line is actually made of, so the UI can say so rather than
      // implying Scrybox knows more than it does.
      marketCount: 0,
      recordedCount,
      insufficientHistory: data.length < 2,
      spanDays,
      windowDays: days ?? null
    });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to retrieve price history' });
  }
});

module.exports = router;
