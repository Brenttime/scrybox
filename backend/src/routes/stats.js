const express = require('express');
const db = require('../db');
const { resolveCardPrice, isVintageSet, parseSqliteUtc, applyProxyRarity } = require('../utils/priceHelpers');

const router = express.Router();
const { cached, dataRevision } = require('../utils/statsCache');
const { getDeckOwnership } = require('../utils/deckOwnership');

// 7. Get Collection Statistics & Analytics
router.get('/stats', cached(() => 'stats', async (req, res) => {
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
               END AS current_price,
               COALESCE(cc.price_estimated, 0) AS price_estimated
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
      color_value_stats AS (
        -- Collection value by colour: every card counted once so the slices
        -- add up to the total (gold cards are 'Multicolor').
        SELECT CASE
                 WHEN type_class = 'land' THEN 'Land'
                 WHEN type_class = 'colorless' THEN 'Colorless'
                 WHEN json_array_length(types) > 1 THEN 'Multicolor'
                 ELSE json_extract(types, '$[0]')
               END AS name,
               SUM(current_value) AS value, SUM(qty) AS count
        FROM typed_cards
        GROUP BY 1
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
             COALESCE(SUM(CASE WHEN price_estimated = 1 AND current_price > 0 THEN qty ELSE 0 END), 0) AS estimated_cards,
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
             (SELECT json_group_array(json_object('name', name, 'value', ROUND(value, 2), 'count', count))
                FROM (SELECT name, value, count FROM color_value_stats WHERE value > 0 ORDER BY value DESC)) AS color_values,
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
    const colorValues = JSON.parse(aggregate.color_values || '[]');
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
        cc.price_normal, cc.price_holofoil, cc.price_estimated
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
             cc.price_trend, cc.price_normal, cc.price_holofoil, cc.price_estimated
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.user_id = ?
      ORDER BY c.added_at DESC
      LIMIT 6
    `, [req.user.id]);
    const recentAdditions = recentRows.map(row => ({ ...applyProxyRarity(row), price_trend: resolveCardPrice(row) }));

    // Deck tile counts for the Dashboard, so it needs no full /api/decks.
    // Same rules as the Dashboard used on the deck list: all decks, Built =
    // checked out, "need cards" = not built and missing copies (deckOwnership).
    const deckRows = await db.all(`SELECT id, checked_out FROM decks WHERE user_id = ?`, [req.user.id]);
    const deckOwnership = deckRows.length ? await getDeckOwnership(db, req.user.id) : new Map();
    const decks = {
      count: deckRows.length,
      built: deckRows.filter(d => d.checked_out).length,
      needCards: deckRows.filter(d => !d.checked_out
        && Number((deckOwnership.get(Number(d.id)) || {}).missing_copies) > 0).length,
    };

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
        estimatedCards: aggregate.estimated_cards,
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
      colorValues,
      rarities,
      sets: allSets.map(set => ({
        id: set.id,
        name: set.name,
        count: set.count,
        value: parseFloat(set.value.toFixed(2))
      })).sort((a, b) => b.value - a.value).slice(0, 8),
      topValuable,
      recentAdditions,
      setProgress: setProgress.slice(0, 4),
      decks
    });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to compute statistics' });
  }
}));

// The expensive half of /stats/history: every owned card's additions and full
// recorded price stream (~0.6 s of SQL + JSON decoding on a 25k-copy
// collection). It depends only on DB contents, not on "now", so it is cached
// per user on the stats data revision (collection + card_cache + price_history
// triggers) and every request still runs the cheap sweep against its own
// targets. Results are therefore identical to recomputing from scratch.
const historyCardCache = new Map(); // userId -> { rev, cards }

async function loadHistoryCards(userId) {
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
  `, [userId]);

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
    }).filter(Boolean).sort((a, b) => a.time - b.time);
    const history = JSON.parse(card.history || '[]').map(([price, recordedAt]) => ({
      price,
      recordedAt,
      time: parseSqliteUtc(recordedAt).getTime()
    })).sort((a, b) => a.recordedAt < b.recordedAt ? -1 : (a.recordedAt > b.recordedAt ? 1 : 0));
    // Kept as two flat number arrays (the cache lives across requests, and
    // ~430k small objects with timestamp strings would hold tens of MB).
    card.historyTimes = history.map(h => h.time);
    card.historyPrices = history.map(h => h.price);
    delete card.history;
  }
  return cards;
}

async function historyCards(userId) {
  if (process.env.STATS_CACHE === '0') return loadHistoryCards(userId);
  const rev = await dataRevision();
  const hit = historyCardCache.get(userId);
  if (rev != null && hit && hit.rev === rev) return hit.cards;
  const cards = await loadHistoryCards(userId);
  // Store only if nothing moved while loading, so a racing write can't pin
  // pre-write rows under the post-write revision.
  if (rev != null && (await dataRevision()) === rev) historyCardCache.set(userId, { rev, cards });
  return cards;
}

// 7b. Get Collection Net Worth Timeline History
router.get('/stats/history', cached((req) => `history:${String(req.query.period || '30d')}`, async (req, res) => {
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

    const cards = await historyCards(req.user.id);

    const totals = Array(count).fill(0);
    for (const card of cards) {
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
        const times = card.historyTimes;
        if (times.length === 0) {
          totals[index] += ownedCurrentValue;
          continue;
        }
        while (historyIndex + 1 < times.length && times[historyIndex + 1] <= target) {
          historyIndex++;
        }
        const price = historyIndex >= 0 ? card.historyPrices[historyIndex] : card.historyPrices[0];
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
}));

// 7c. Net worth, on its own, for scripts and dashboards (issue #33).
//
// /stats already contains these numbers, but it also runs the type/rarity/set
// aggregation, a per-set progress query and two top-N queries to get there —
// which is the wrong thing to hand a finance tracker polling every five minutes.
// This is one pass over the collection and nothing else.
//
// Pair it with an API key (Settings -> API access): that credential is read-only
// and does not expire, so an external tracker keeps working without a login.
// Cached in the stats cache on the DB revision (statsCache.js).
router.get('/stats/networth', cached(() => 'networth', async (req, res) => {
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
}, {
  // Values are current for the cached revision, so a hit is "as of now".
  onHit: (body) => ({ ...body, asOf: new Date().toISOString() }),
}));

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

// Tagger oracle tags: direct + inherited (ancestor) tags, like tagger.scryfall.com.
router.get('/cards/:id/oracle-tags', async (req, res) => {
  try {
    const fallback = typeof req.query.oracle_id === 'string' ? req.query.oracle_id : null;
    res.json(await require('../utils/cardOracleTags').cardOracleTags(req.params.id, fallback));
  } catch (err) {
    console.error('oracle tags lookup failed:', err.message);
    res.status(500).json({ error: 'Could not load tags' });
  }
});

// Which of the requested Tagger tags (slugs) each card carries, directly or
// via an ancestor (closure). Drives the deck Tag view. Body: { ids, slugs }.
router.post('/cards/role-tags', async (req, res) => {
  const ids = Array.isArray(req.body?.ids) ? [...new Set(req.body.ids.filter(x => typeof x === 'string'))] : [];
  const slugs = Array.isArray(req.body?.slugs) ? [...new Set(req.body.slugs.filter(x => typeof x === 'string'))] : [];
  if (!ids.length || !slugs.length || ids.length > 1000 || slugs.length > 100) {
    return res.status(400).json({ error: 'ids (1-1000) and slugs (1-100) are required' });
  }
  try {
    const gen = await db.get(`SELECT id FROM oracle_tag_generations WHERE active = 1`);
    if (!gen) return res.json({ ready: false, tags: {} });
    const rows = await db.all(`
      SELECT DISTINCT cc.id AS card_id, anc.slug AS slug
      FROM card_cache cc
      JOIN oracle_tag_assignments a ON a.generation_id = ? AND a.oracle_id = cc.oracle_id
      JOIN oracle_tag_closure c ON c.generation_id = a.generation_id AND c.descendant_tag_id = a.tag_id
      JOIN oracle_tags anc ON anc.generation_id = c.generation_id AND anc.tag_id = c.ancestor_tag_id
      WHERE cc.id IN (${ids.map(() => '?').join(',')})
        AND anc.slug IN (${slugs.map(() => '?').join(',')})
    `, [gen.id, ...ids, ...slugs]);
    const tags = {};
    for (const r of rows) (tags[r.card_id] ||= []).push(r.slug);
    res.json({ ready: true, tags });
  } catch (err) {
    console.error('role tags lookup failed:', err.message);
    res.status(500).json({ error: 'Could not load tags' });
  }
});

// Deck "Oracle tags" widget: the most common Tagger tags (direct or inherited)
// across the given cards. Trivia/meta tags (cycles, art, name jokes, generic
// ability shapes) are dropped so the list reads like deck roles.
const TAG_NOISE = /^(cycle|synergy-|card-names|alliteration|flavors-of|staple-with|cheaper-than|triggered-ability|activated-ability|static-ability|tap-|discount-|reprint|single-target|multi-target|.*-art$|art-|pun|reference|joke|flavor|vanilla|french-vanilla|keyword-|mechanic-|cast-trigger|etb-trigger|death-trigger|virtual-|unique-type-line|type-line-|name-|has-|mentions-|references-)/;
router.post('/cards/tag-stats', async (req, res) => {
  const ids = Array.isArray(req.body?.ids) ? [...new Set(req.body.ids.filter(x => typeof x === 'string'))] : [];
  if (!ids.length || ids.length > 1000) return res.status(400).json({ error: 'ids (1-1000) are required' });
  try {
    const gen = await db.get(`SELECT id FROM oracle_tag_generations WHERE active = 1`);
    if (!gen) return res.json({ ready: false, tags: [] });
    const rows = await db.all(`
      SELECT anc.slug AS slug, anc.label AS label, cc.id AS card_id
      FROM card_cache cc
      JOIN oracle_tag_assignments a ON a.generation_id = ? AND a.oracle_id = cc.oracle_id
      JOIN oracle_tag_closure c ON c.generation_id = a.generation_id AND c.descendant_tag_id = a.tag_id
      JOIN oracle_tags anc ON anc.generation_id = c.generation_id AND anc.tag_id = c.ancestor_tag_id
      WHERE cc.id IN (${ids.map(() => '?').join(',')})
    `, [gen.id, ...ids]);
    const bySlug = new Map();
    for (const r of rows) {
      if (TAG_NOISE.test(r.slug)) continue;
      if (!bySlug.has(r.slug)) bySlug.set(r.slug, { slug: r.slug, label: r.label || r.slug, cards: new Set() });
      bySlug.get(r.slug).cards.add(r.card_id);
    }
    const tags = [...bySlug.values()]
      .filter(t => t.cards.size >= 2)
      .sort((a, b) => b.cards.size - a.cards.size || a.slug.localeCompare(b.slug))
      .slice(0, 40)
      .map(t => ({ slug: t.slug, label: t.label, cards: [...t.cards] }));
    res.json({ ready: true, tags });
  } catch (err) {
    console.error('tag stats failed:', err.message);
    res.status(500).json({ error: 'Could not load tags' });
  }
});

// Deck Highlighter: which of `names` match a full Scryfall-syntax query `q`.
// Answered by the (local) Scryfall engine as `(q) (!"A" or !"B" ...)`, so
// every operator works (o:, otag:, is:, kw:, pow ...), not only the subset
// the stored rows can evaluate. Names go in chunks to keep queries short.
const HIGHLIGHT_CHUNK = 100;
router.post('/cards/highlight', async (req, res) => {
  const q = typeof req.body?.q === 'string' ? req.body.q.trim() : '';
  const names = Array.isArray(req.body?.names)
    ? [...new Set(req.body.names.filter(n => typeof n === 'string' && n.trim()))] : [];
  if (!q || q.length > 500 || !names.length || names.length > 1000) {
    return res.status(400).json({ error: 'q (1-500 chars) and names (1-1000) are required' });
  }
  try {
    require('../../../shared/scryfallQuery.js').analyze(q);
  } catch (err) {
    return res.status(400).json({ error: err.message || 'Invalid query' });
  }
  try {
    const { fetchWindow } = require('../scryfallApi');
    const exact = n => `!"${n.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
    const wanted = new Set(names.map(n => n.toLowerCase()));
    const matched = new Set();
    for (let i = 0; i < names.length; i += HIGHLIGHT_CHUNK) {
      const chunk = names.slice(i, i + HIGHLIGHT_CHUNK);
      const { cards } = await fetchWindow(`(${q}) (${chunk.map(exact).join(' or ')})`, null, 0, chunk.length * 4, 'name');
      for (const c of cards) {
        const full = String(c.name || '');
        const front = full.split(' // ')[0];
        for (const n of [full, front]) if (wanted.has(n.toLowerCase())) matched.add(n.toLowerCase());
      }
    }
    res.json({ names: names.filter(n => matched.has(n.toLowerCase()) || matched.has(n.split(' // ')[0].toLowerCase())) });
  } catch (err) {
    const status = err.status || err.response?.status;
    if (status === 400 || status === 422 || /syntax|invalid|didn.t match/i.test(err.message || '')) {
      return res.status(400).json({ error: err.message || 'Invalid query' });
    }
    if (status === 404) return res.json({ names: [] });
    console.error('highlight failed:', err.message);
    res.status(502).json({ error: 'Search engine unavailable' });
  }
});

// Built (checked-out) decks that use this card, matched by logical card like
// the checkout allocation (any printing). Basic lands are never pulled, so
// they are never listed.
router.get('/cards/:id/decks', async (req, res) => {
  try {
    const { sqlCardKey, sqlIsBasicLand } = require('../utils/cardIdentity');
    const decks = await db.all(`
      SELECT d.id, d.name, SUM(dc.quantity) AS quantity
      FROM decks d
      CROSS JOIN deck_cards dc ON dc.deck_id = d.id
      CROSS JOIN card_cache dcc ON dcc.id = dc.card_id
      CROSS JOIN card_cache target ON target.id = ?
      WHERE ${sqlCardKey('dcc')} = ${sqlCardKey('target')}
        AND d.user_id = ?
        AND d.checked_out = 1
        AND dc.quantity > 0
        AND NOT ${sqlIsBasicLand('target')}
      GROUP BY d.id, d.name
      ORDER BY d.name COLLATE NOCASE
    `, [req.params.id, req.user.id]);
    res.json({ decks: decks.map(d => ({ id: d.id, name: d.name, quantity: Number(d.quantity) || 0 })) });
  } catch (err) {
    console.error('card deck usage lookup failed:', err.message);
    res.status(500).json({ error: 'Could not load decks' });
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

// Commander of the day: one legal commander per US-Eastern calendar day,
// chosen deterministically from the date so every device agrees all day.
// `show` says whether the dashboard should feature it today instead of the
// top card (roughly every other day, also date-seeded). Served from the local
// Scryfall mirror; cached in memory per day.
const COTD_QUERY = 'is:commander legal:commander game:paper -is:digital';
const cotdCache = new Map(); // day -> payload
function easternDay(d = new Date()) {
  return new Intl.DateTimeFormat('en-CA', { timeZone: 'America/New_York', year: 'numeric', month: '2-digit', day: '2-digit' }).format(d);
}
function dayHash(str) {
  let h = 2166136261;
  for (let i = 0; i < str.length; i++) { h ^= str.charCodeAt(i); h = Math.imul(h, 16777619); }
  return h >>> 0;
}
function edhrecSlug(n) {
  return String(n || '').split(' // ')[0].normalize('NFD').replace(/[\u0300-\u036f]/g, '')
    .toLowerCase().replace(/['’,.]/g, '').replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
}
router.get('/stats/commander-of-the-day', async (req, res) => {
  try {
    const day = typeof req.query.day === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(req.query.day) ? req.query.day : easternDay();
    if (cotdCache.has(day)) return res.json(cotdCache.get(day));
    const { fetchWindow } = require('../scryfallApi');
    const first = await fetchWindow(COTD_QUERY, null, 0, 1, 'name');
    const total = first.total || 0;
    if (!total) return res.status(503).json({ error: 'No commanders available' });
    const idx = dayHash(`cotd:${day}`) % total;
    const { cards } = idx === 0 ? first : await fetchWindow(COTD_QUERY, null, idx, 1, 'name');
    const raw = cards[0];
    if (!raw) return res.status(503).json({ error: 'No commander found' });
    const face = (raw.card_faces && raw.card_faces[0]) || raw;
    const img = raw.image_uris || face.image_uris || {};
    const payload = {
      day,
      show: (dayHash(`show:${day}`) % 2) === 0,
      card: {
        id: raw.id,
        name: raw.name,
        type_line: raw.type_line || face.type_line || '',
        color_identity: raw.color_identity || [],
        set: raw.set,
        image_url: img.normal || img.large || '',
        art_crop: img.art_crop || '',
        edhrec_url: `https://edhrec.com/commanders/${edhrecSlug(raw.name)}`,
      },
    };
    cotdCache.set(day, payload);
    if (cotdCache.size > 7) cotdCache.delete(cotdCache.keys().next().value);
    res.json(payload);
  } catch (err) {
    console.error('commander-of-the-day failed:', err.message);
    res.status(502).json({ error: 'Commander of the day unavailable' });
  }
});

module.exports = router;
