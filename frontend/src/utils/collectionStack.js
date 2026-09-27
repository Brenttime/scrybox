// Shared "Stack Duplicates" grouping for the collection views (CollectionList
// and SharedCollection). Both views must group identically, so the key and the
// merge live here instead of being copied into each component.
//
// `printing` is ALWAYS part of the key and is deliberately not a toggle: a foil
// and a non-foil copy of the same Scryfall printing share one `card_id` and
// differ only by `printing`, so keying on `card_id` alone stacked them into a
// single "x2" row wearing whichever row came first -- the foil read as a
// non-foil and even priced off the wrong column (priceHelpers.resolveCardPrice
// picks price_holofoil vs price_normal off the row's printing). A foil is a
// physically different card and is never interchangeable with its non-foil twin,
// so it can not be an opt-in. Condition stays opt-in (`byCondition`).

const DEFAULT_PRINTING = 'Normal';

// A missing/NULL printing means "the ordinary copy" -- the same value the
// collection UI and price resolution treat it as.
const printingOf = (item) => item.printing || DEFAULT_PRINTING;

// Quantity coalescing follows SharedCollection's long-standing `|| 1` behaviour,
// which is also what the schema guarantees (`collection.quantity INTEGER
// DEFAULT 1`): for every real row both views produce the same numbers they did
// before. CollectionList previously did a bare `+=`, so the only place this
// differs is a degenerate NULL/0/absent quantity, where the old code produced
// NaN and now produces a countable copy instead. Number() also keeps a quantity
// that arrives as text adding numerically instead of concatenating.
const quantityOf = (item) => {
  const value = Number(item.quantity);
  return Number.isFinite(value) && value > 0 ? value : 1;
};

// Stack identity for one owned row. JSON.stringify keeps the tuple
// unambiguous: `card_id` and `condition` are free-text, so a hand-built
// `${card_id}-${printing}` string could be collided by a value that itself
// contains the separator.
export function stackKey(item, { byCondition = false } = {}) {
  // A proxy is never interchangeable with a real copy, so it stacks apart.
  return JSON.stringify([item.card_id, printingOf(item), byCondition ? item.condition ?? null : null, item.is_proxy ? 1 : 0]);
}

// Group stacked rows, keeping the FIRST row of each group as the representative
// row (it carries the artwork, set, price and entry_id the row renders from) and
// summing the quantities of every copy folded into it. Insertion order is
// preserved, matching the previous `Object.values(groups)` behaviour.
export function stackCollection(rows, { byCondition = false } = {}) {
  const groups = new Map();
  for (const item of rows) {
    const key = stackKey(item, { byCondition });
    const representative = groups.get(key);
    if (!representative) {
      groups.set(key, { ...item });
    } else {
      representative.quantity = quantityOf(representative) + quantityOf(item);
    }
  }
  return Array.from(groups.values());
}
