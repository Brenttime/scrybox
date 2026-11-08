// A deck list (Moxfield, precon, manual add) names a printing, but the cards
// that actually go in the box are the user's own copies. Checkout reserves by
// logical card (name), never by printing, so the deck view shows the printing
// the user owns instead of the one the list happened to name.
//
// Display only: deck_cards keeps the list's printing so Moxfield sync and the
// name-based add/remove/checkout routes are untouched.
const { sqlCardKey } = require('./cardIdentity');

// Fields that describe the physical printing. Oracle fields (name, text, cmc,
// colors, types) are identical across printings and stay on the deck row.
const PRINTING_FIELDS = [
  'id', 'printed_name', 'rarity', 'set_id', 'set_name', 'number', 'image_url',
  'price_trend', 'price_normal', 'price_holofoil', 'price_etched', 'price_currency', 'price_estimated',
];

// Per logical card, prefer the deck's own printing when it is owned; otherwise
// the owned printing with the most copies (ties by id, so the pick is stable).
function choosePrintings(rows, owned) {
  const byKey = new Map();
  for (const o of owned) {
    if (!byKey.has(o.card_key)) byKey.set(o.card_key, []);
    byKey.get(o.card_key).push(o);
  }
  const picks = new Map();
  for (const row of rows) {
    const candidates = byKey.get(row.card_key);
    if (!candidates || !candidates.length) continue;
    if (candidates.some(c => c.card_id === row.id)) continue;
    const best = [...candidates].sort((a, b) =>
      (b.qty - a.qty) || String(a.card_id).localeCompare(String(b.card_id)))[0];
    picks.set(row.card_key, best.card_id);
  }
  return picks;
}

async function applyOwnedPrintings(db, userId, rows) {
  const keys = [...new Set(rows.map(r => r.card_key).filter(Boolean))];
  if (!keys.length) return rows;
  const marks = keys.map(() => '?').join(',');
  const owned = await db.all(`
    SELECT ${sqlCardKey('cc')} AS card_key, c.card_id, SUM(c.quantity) AS qty
    FROM card_cache cc
    CROSS JOIN collection c
    WHERE c.card_id = cc.id AND c.user_id = ? AND c.quantity > 0
      AND ${sqlCardKey('cc')} IN (${marks})
    GROUP BY c.card_id
  `, [userId, ...keys]);
  const picks = choosePrintings(rows, owned);
  if (!picks.size) return rows;

  const ids = [...new Set(picks.values())];
  const printings = await db.all(
    `SELECT ${PRINTING_FIELDS.join(', ')} FROM card_cache WHERE id IN (${ids.map(() => '?').join(',')})`,
    ids
  );
  const byId = new Map(printings.map(p => [p.id, p]));
  return rows.map(row => {
    const printing = byId.get(picks.get(row.card_key));
    if (!printing) return row;
    return { ...row, ...printing, deck_list_card_id: row.id };
  });
}

module.exports = { applyOwnedPrintings, choosePrintings };
