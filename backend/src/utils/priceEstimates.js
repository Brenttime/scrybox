// Estimated prices for printings the market has no quote for.
//
// Scryfall leaves some printings unpriced (The List, promo reprints, sets cached
// before release). Showing $0.00 for those understates every total they sit in,
// so an unpriced printing borrows the CHEAPEST real quote of any other printing
// of the same logical card (same name key the deck floor uses) and is flagged
// price_estimated = 1. The UI prefixes flagged prices with "~".
//
// The estimate is written into price_trend on purpose: every value path in the
// app (collection totals, deck/list values, stats, exports) already reads
// price_trend through resolveCardPrice/currentPrintingPrice, so one write here
// reaches all of them. A real quote always wins: cacheNormalizedCards clears the
// flag whenever a priced source writes the row, and estimated rows never count
// as a floor for another printing.
const { sqlCardKey } = require('./cardIdentity');

const KEY_CHUNK = 400;
const FINISH_FIELDS = ['price_normal', 'price_holofoil', 'price_etched', 'price_trend'];

const isUsd = (r) => r.price_currency == null || String(r.price_currency).toUpperCase() === 'USD';

// Cheapest positive USD price on a row that carries a real quote, else null.
function realFloor(row) {
  if (!row || row.price_estimated || !isUsd(row)) return null;
  let best = null;
  for (const f of FINISH_FIELDS) {
    const v = Number(row[f]);
    if (Number.isFinite(v) && v > 0 && (best === null || v < best)) best = v;
  }
  return best;
}

const hasRealQuote = (row) => realFloor(row) !== null;

// Pure planner: given every cached printing of some logical cards, return the
// row updates that bring their estimates up to date.
function planEstimates(rows) {
  const floorByKey = new Map();
  for (const r of rows) {
    const p = realFloor(r);
    if (p === null) continue;
    const cur = floorByKey.get(r.card_key);
    if (cur === undefined || p < cur) floorByKey.set(r.card_key, p);
  }
  const updates = [];
  for (const r of rows) {
    if (hasRealQuote(r)) continue;
    const floor = floorByKey.get(r.card_key);
    if (floor !== undefined) {
      if (!r.price_estimated || Number(r.price_trend) !== floor) {
        updates.push({ id: r.id, price_trend: floor, price_estimated: 1 });
      }
    } else if (r.price_estimated) {
      // The printing the estimate came from is gone or lost its quote.
      updates.push({ id: r.id, price_trend: 0, price_estimated: 0 });
    }
  }
  return updates;
}

async function estimateKeys(db, keys) {
  let changed = 0;
  const unique = [...new Set(keys.filter(Boolean))];
  for (let i = 0; i < unique.length; i += KEY_CHUNK) {
    const chunk = unique.slice(i, i + KEY_CHUNK);
    const rows = await db.all(
      `WITH k(card_key) AS (SELECT value FROM json_each(?))
       SELECT ${sqlCardKey('cc')} AS card_key, cc.id, cc.price_trend, cc.price_normal,
              cc.price_holofoil, cc.price_etched, cc.price_currency, cc.price_estimated
       FROM k CROSS JOIN card_cache cc INDEXED BY idx_card_cache_logical_key
       WHERE ${sqlCardKey('cc')} = k.card_key`,
      [JSON.stringify(chunk)]
    );
    for (const u of planEstimates(rows)) {
      await db.run(
        `UPDATE card_cache SET price_trend = ?, price_estimated = ?,
           price_currency = CASE WHEN ? = 1 THEN 'USD' ELSE price_currency END
         WHERE id = ?`,
        [u.price_trend, u.price_estimated, u.price_estimated, u.id]
      );
      changed++;
    }
  }
  return changed;
}

// Re-estimate the logical cards behind these printing ids (after a cache write).
async function refreshEstimatesForIds(db, ids) {
  const list = [...new Set((ids || []).filter(Boolean))];
  if (!list.length) return 0;
  const keys = [];
  for (let i = 0; i < list.length; i += KEY_CHUNK) {
    const rows = await db.all(
      `SELECT DISTINCT ${sqlCardKey('cc')} AS card_key FROM card_cache cc
       WHERE cc.id IN (SELECT value FROM json_each(?))`,
      [JSON.stringify(list.slice(i, i + KEY_CHUNK))]
    );
    keys.push(...rows.map(r => r.card_key));
  }
  return estimateKeys(db, keys);
}

const UNPRICED_SQL = `(COALESCE(cc.price_estimated, 0) = 1 OR UPPER(COALESCE(cc.price_currency, 'USD')) <> 'USD' OR (
  COALESCE(cc.price_trend, 0) <= 0 AND COALESCE(cc.price_normal, 0) <= 0
  AND COALESCE(cc.price_holofoil, 0) <= 0 AND COALESCE(cc.price_etched, 0) <= 0))`;

// Every logical card with an unpriced or estimated printing.
async function refreshAllEstimates(db) {
  const rows = await db.all(`SELECT DISTINCT ${sqlCardKey('cc')} AS card_key FROM card_cache cc WHERE ${UNPRICED_SQL}`);
  return estimateKeys(db, rows.map(r => r.card_key));
}

// Printings the user actually holds (collection, decks, lists) that are unpriced
// AND have no priced sibling cached. Only these need an upstream lookup.
async function cardsMissingFloor(db, limit = 150) {
  return db.all(
    `WITH used AS (
       SELECT card_id FROM collection WHERE quantity > 0
       UNION SELECT card_id FROM deck_cards WHERE quantity > 0
       UNION SELECT card_id FROM list_cards WHERE quantity > 0
     )
     SELECT DISTINCT cc.oracle_id, ${sqlCardKey('cc')} AS card_key
     FROM used JOIN card_cache cc ON cc.id = used.card_id
     WHERE cc.oracle_id IS NOT NULL AND ${UNPRICED_SQL}
       AND COALESCE(cc.price_estimated, 0) = 0
     LIMIT ?`,
    [limit]
  );
}

// Pull every printing of each floorless card from the real Scryfall API (the
// sidecar serves no prices), cache them, then estimate. Paced by the shared
// upstream queue inside scryGetRetried. Remembered per process so a card that
// has no price anywhere is not re-asked on every sweep.
const askedOracle = new Set();
async function fillMissingFloors(db, deps) {
  const missing = (await cardsMissingFloor(db)).filter(r => !askedOracle.has(r.oracle_id));
  if (!missing.length) return { asked: 0, estimated: 0 };
  let asked = 0;
  for (const { oracle_id: oid } of missing) {
    try {
      let url = `/cards/search?q=${encodeURIComponent(`oracleid:${oid}`)}&unique=prints&include_extras=true`;
      for (let page = 0; url && page < 5; page++) {
        // Background job: well under Scryfall's 2/s search limit, and no retries.
        await new Promise(r => setTimeout(r, deps.gapMs ?? 1500));
        const resp = await deps.scryGet(url, { upstream: true });
        const data = (resp.data && resp.data.data) || [];
        const cards = data.map(raw => deps.normalizeCard(raw)).filter(c => c.price_source != null);
        if (cards.length) await deps.cacheCards(cards);
        url = resp.data && resp.data.has_more ? resp.data.next_page : null;
      }
      asked++;
      askedOracle.add(oid);
    } catch (e) {
      const status = e.response && e.response.status;
      if (status === 404) { askedOracle.add(oid); continue; }
      // Rate-limited or down: stop and let the next sweep pick up the rest
      // instead of holding the shared upstream queue in 60 s cooldowns.
      console.error(`price floor lookup stopped at ${oid}:`, e.message);
      break;
    }
  }
  const estimated = await estimateKeys(db, missing.map(r => r.card_key));
  return { asked, estimated };
}

module.exports = {
  planEstimates,
  realFloor,
  estimateKeys,
  refreshEstimatesForIds,
  refreshAllEstimates,
  fillMissingFloors,
};
