// Per-deck ownership for the deck list: how many of each deck's cards the
// user owns, and what is still missing. Same rules as the deck editor's
// "N cards still missing" (frontend utils/deckText.js missingEntries):
//   - cards are keyed by canonical name, so every printing counts toward it
//   - owned copies are the user's whole collection, summed across printings
//   - basic lands are exempt (always treated as owned)
//   - missing_card_types counts NAMES short (the header's number);
//     missing_copies counts the copies short (for "owned X / Y")
// One grouped query for every deck of the user; no per-deck round trips.
const { sqlCardKey, sqlIsBasicLand } = require('./cardIdentity');

const OWNERSHIP_SQL = `
  WITH need AS (
    SELECT
      dc.deck_id,
      ${sqlCardKey('cc')} AS card_key,
      SUM(dc.quantity) AS need_qty,
      MAX(${sqlIsBasicLand('cc')}) AS is_basic
    FROM deck_cards dc
    JOIN decks d ON d.id = dc.deck_id
    JOIN card_cache cc ON cc.id = dc.card_id
    WHERE d.user_id = ? AND dc.quantity > 0
    GROUP BY dc.deck_id, ${sqlCardKey('cc')}
  ),
  owned AS (
    SELECT ${sqlCardKey('occ')} AS card_key, SUM(o.quantity) AS owned_qty
    -- Only keys some deck needs; card_cache first so collection is probed by
    -- idx_collection_card_user instead of aggregated whole.
    FROM card_cache occ
    CROSS JOIN collection o
    WHERE o.card_id = occ.id AND o.user_id = ? AND o.quantity > 0
      AND ${sqlCardKey('occ')} IN (SELECT card_key FROM need)
    GROUP BY ${sqlCardKey('occ')}
  )
  SELECT
    need.deck_id,
    SUM(CASE WHEN need.is_basic = 0 AND COALESCE(owned.owned_qty, 0) < need.need_qty
             THEN 1 ELSE 0 END) AS missing_card_types,
    SUM(CASE WHEN need.is_basic = 0 AND COALESCE(owned.owned_qty, 0) < need.need_qty
             THEN need.need_qty - COALESCE(owned.owned_qty, 0) ELSE 0 END) AS missing_copies
  FROM need
  LEFT JOIN owned ON owned.card_key = need.card_key
  GROUP BY need.deck_id
`;

async function getDeckOwnership(db, userId) {
  const rows = await db.all(OWNERSHIP_SQL, [userId, userId]);
  const out = new Map();
  for (const r of rows) {
    out.set(Number(r.deck_id), {
      missing_card_types: Number(r.missing_card_types) || 0,
      missing_copies: Number(r.missing_copies) || 0,
    });
  }
  return out;
}

module.exports = { getDeckOwnership };
