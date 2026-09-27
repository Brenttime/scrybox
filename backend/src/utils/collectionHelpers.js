// Shared helpers for the collection route. Kept in one neutral module so the
// split route files never have to import each other.
const db = require('../db');
const { sqlCardKey, sqlIsBasicLand } = require('./cardIdentity');

// Checkout and inventory reductions must be linearizable with respect to each
// other. Scrybox intentionally runs one Node process, so this FIFO mutex closes
// the gap between a reduction's availability read and its multi-row stack write
// without pretending the shared sqlite3 connection provides request-local
// transactions. Every operation that can reserve or shrink logical supply uses
// this lock. Physical stack-key edits (condition, finish, language) also use it
// because they can race a quantity reconciliation that trims the target stack.
let allocationTail = Promise.resolve();
async function withAllocationLock(fn) {
  const previous = allocationTail;
  let release;
  allocationTail = new Promise(resolve => { release = resolve; });
  await previous;
  try {
    return await fn();
  } finally {
    release();
  }
}

// How many copies of each collection entry are physically pulled for a
// checked-out deck. Sums required quantity per card across all of the user's
// checked-out decks, then allocates greedily onto their owned entries (newest
// first), so the collection view can grey out the same copies the checkout
// checklist told them to grab.
async function checkedOutAllocation(userId) {
  const required = await db.all(`
    SELECT ${sqlCardKey('needed_cc')} AS card_key, SUM(dc.quantity) AS req
    FROM deck_cards dc
    JOIN card_cache needed_cc ON needed_cc.id = dc.card_id
    JOIN decks d ON dc.deck_id = d.id
    WHERE d.user_id = ?
      AND d.checked_out = 1
      AND dc.quantity > 0
      AND NOT ${sqlIsBasicLand('needed_cc')}
    GROUP BY ${sqlCardKey('needed_cc')}
  `, [userId]);
  if (required.length === 0) return new Map();

  // Fetch owned rows once. The old implementation ran one collection query per
  // required logical card, so a pair of checked-out Commander decks caused about
  // 200 scans of a 20k-row collection before the page could respond. Grouping in
  // memory preserves the same newest-first greedy allocation with two bounded SQL
  // reads regardless of deck size.
  const entries = await db.all(`
    SELECT
      c.id AS entry_id,
      c.quantity,
      ${sqlCardKey('owned_cc')} AS card_key
    FROM collection c
    JOIN card_cache owned_cc ON owned_cc.id = c.card_id
    WHERE c.user_id = ?
      AND c.quantity > 0
    ORDER BY c.added_at DESC, c.id DESC
  `, [userId]);

  const remaining = new Map(required.map(({ card_key, req }) => [card_key, Number(req) || 0]));
  const alloc = new Map();
  for (const entry of entries) {
    const need = remaining.get(entry.card_key) || 0;
    if (need <= 0) continue;
    // Deliberately span every printing with this English card name. The chosen
    // physical rows are still exact collection entries, so the collection can
    // grey out the copies actually allocated while checkout stays art-agnostic.
    const take = Math.min(Number(entry.quantity) || 0, need);
    if (take <= 0) continue;
    remaining.set(entry.card_key, need - take);
    alloc.set(entry.entry_id, take);
  }
  return alloc;
}

async function logicalInventoryStatus(database, userId, cardId) {
  const dbClient = database || db;
  return dbClient.get(`
    SELECT
      target.id AS card_id,
      ${sqlCardKey('target')} AS card_key,
      CASE WHEN ${sqlIsBasicLand('target')} THEN 1 ELSE 0 END AS is_basic_land,
      (
        SELECT COALESCE(SUM(c.quantity), 0)
        FROM collection c
        JOIN card_cache owned_cc ON owned_cc.id = c.card_id
        WHERE c.user_id = ?
          AND c.quantity > 0
          AND ${sqlCardKey('owned_cc')} = ${sqlCardKey('target')}
      ) AS owned_qty,
      (
        SELECT COALESCE(SUM(dc.quantity), 0)
        FROM deck_cards dc
        JOIN card_cache locked_cc ON locked_cc.id = dc.card_id
        JOIN decks d ON d.id = dc.deck_id
        WHERE d.user_id = ?
          AND d.checked_out = 1
          AND dc.quantity > 0
          AND NOT ${sqlIsBasicLand('locked_cc')}
          AND ${sqlCardKey('locked_cc')} = ${sqlCardKey('target')}
      ) AS locked_qty
    FROM card_cache target
    WHERE target.id = ?
  `, [userId, userId, cardId]);
}

// The rows the collection view stacks together with this one: same card, same
// printing details. Ordered newest-first so the newest copies are the
// trim candidates. The edited row itself is excluded: it is never the row
// deleted.
async function stackSiblings(dbClient, userId, row, entryId) {
  return dbClient.all(`
    SELECT id, quantity FROM collection
    WHERE user_id = ? AND card_id = ? AND condition = ? AND printing = ?
      AND language = ? AND id != ? AND quantity > 0
      AND COALESCE(is_proxy, 0) = ?
    ORDER BY id DESC
  `, [userId, row.card_id, row.condition, row.printing, row.language, entryId, row.is_proxy ? 1 : 0]);
}

// Make the number of copies this stack represents equal `target`, keeping the
// edited row. The quantity field in the card popup is absolute — "how many of
// these I own" — and the stacked collection view sums the identical rows into
// that one number, so the edit has to reconcile the whole group both ways.
// Growing inserts single-card rows mirroring the edited row's attributes;
// shrinking removes the trim candidates first and only then reduces the edited
// row's own quantity. Returns the net change in copies.
async function setStackQuantity(database, userId, entryId, target) {
  const dbClient = database || db;
  const row = await dbClient.get(`SELECT * FROM collection WHERE id = ? AND user_id = ?`, [entryId, userId]);
  if (!row) return 0;
  const siblings = await stackSiblings(dbClient, userId, row, entryId);

  const start = Math.max(0, Number(row.quantity) || 0)
    + siblings.reduce((n, s) => n + Math.max(0, Number(s.quantity) || 0), 0);
  let current = start;

  for (let i = 0; current < target; i++, current++) {
    await dbClient.run(`
      INSERT INTO collection (
        card_id, user_id, quantity, condition, printing, language, purchase_price,
        is_trade, favorite, is_proxy
      ) VALUES (?, ?, 1, ?, ?, ?, ?, ?, ?, ?)
    `, [
      row.card_id, userId, row.condition, row.printing, row.language, row.purchase_price,
      row.is_trade, row.favorite, row.is_proxy ? 1 : 0
    ]);
  }

  for (const s of siblings) {
    if (current <= target) break;
    const have = Math.max(0, Number(s.quantity) || 0);
    const drop = Math.min(have, current - target);
    current -= drop;
    if (drop >= have) {
      await dbClient.run(`DELETE FROM collection WHERE id = ? AND user_id = ?`, [s.id, userId]);
    } else {
      await dbClient.run(`UPDATE collection SET quantity = quantity - ? WHERE id = ? AND user_id = ?`, [drop, s.id, userId]);
    }
  }

  if (current > target) {
    await dbClient.run(`UPDATE collection SET quantity = ? WHERE id = ? AND user_id = ?`, [target, entryId, userId]);
    current = target;
  }

  return current - start;
}

// One card = one row. Split any legacy stacked entry (quantity > 1) into that
// many single-card rows so each copy is its own row (the pre-storage schema
// kept one row per physical card). Idempotent: once run there are no
// quantity>1 rows, so a re-run is a no-op.
async function splitStackedEntries(database) {
  const dbClient = database || db;
  const stacked = await dbClient.all(`SELECT * FROM collection WHERE quantity > 1`);
  if (stacked.length === 0) return 0;
  let created = 0;
  for (const e of stacked) {
    const copies = e.quantity;
    await dbClient.run(`UPDATE collection SET quantity = 1 WHERE id = ?`, [e.id]);
    for (let i = 1; i < copies; i++) {
      await dbClient.run(`
        INSERT INTO collection (
          card_id, user_id, quantity, condition, printing, language, purchase_price,
          is_trade, favorite
        ) VALUES (?, ?, 1, ?, ?, ?, ?, ?, ?)
      `, [
        e.card_id, e.user_id, e.condition, e.printing, e.language, e.purchase_price,
        e.is_trade, e.favorite
      ]);
      created++;
    }
  }
  return created;
}

module.exports = {
  checkedOutAllocation,
  logicalInventoryStatus,
  withAllocationLock,
  setStackQuantity,
  splitStackedEntries,
};
