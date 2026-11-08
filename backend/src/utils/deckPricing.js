const { sqlCardKey } = require('./cardIdentity');

const QUERY_CHUNK_SIZE = 500;
const PRICE_FIELDS = ['price_normal', 'price_holofoil', 'price_etched', 'price_trend'];

function chunked(values, size = QUERY_CHUNK_SIZE) {
  const chunks = [];
  for (let offset = 0; offset < values.length; offset += size) {
    chunks.push(values.slice(offset, offset + size));
  }
  return chunks;
}

function cheapestEligiblePrice(row) {
  let cheapest = null;
  for (const field of PRICE_FIELDS) {
    const value = Number(row[field]);
    if (!Number.isFinite(value) || value <= 0) continue;
    if (cheapest === null || value < cheapest) cheapest = value;
  }
  return cheapest;
}

async function getDeckRequirements(client, deckIds) {
  const requirements = [];
  for (const ids of chunked(deckIds)) {
    const placeholders = ids.map(() => '?').join(', ');
    const rows = await client.all(`
      SELECT
        dc.deck_id,
        COALESCE(NULLIF(${sqlCardKey('deck_cc')}, ''), 'missing:' || dc.card_id) AS card_key,
        SUM(dc.quantity) AS quantity
      FROM deck_cards dc
      LEFT JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
      WHERE dc.deck_id IN (${placeholders})
        AND dc.quantity > 0
      GROUP BY dc.deck_id, card_key
    `, ids);
    requirements.push(...rows);
  }
  return requirements;
}

// Lowest USD price per logical card across every cached printing and finish.
async function getCheapestPrices(client, cardKeys) {
  const printings = await getCheapestPrintings(client, cardKeys);
  const cheapestByKey = new Map();
  for (const [cardKey, printing] of printings) cheapestByKey.set(cardKey, printing.price);
  return cheapestByKey;
}

// Same sweep, but keeps WHICH printing holds the floor — the move-to-cheapest
// rewrite needs the target row, not only its number. Ties break on the
// printing id so two equally cheap printings resolve to the same target on
// every run; otherwise a rewrite would shuffle cards whose price never moved
// and the operation would not be idempotent.
async function getCheapestPrintings(client, cardKeys) {
  const bestByKey = new Map();
  for (const keys of chunked(cardKeys)) {
    const valueRows = keys.map(() => '(?)').join(', ');
    const rows = await client.all(`
      WITH required(card_key) AS (VALUES ${valueRows})
      SELECT
        ${sqlCardKey('priced_cc')} AS card_key,
        priced_cc.id,
        priced_cc.price_normal,
        priced_cc.price_holofoil,
        priced_cc.price_etched,
        priced_cc.price_trend
      FROM required
      CROSS JOIN card_cache priced_cc INDEXED BY idx_card_cache_logical_key
      WHERE ${sqlCardKey('priced_cc')} = required.card_key
        AND UPPER(COALESCE(priced_cc.price_currency, 'USD')) = 'USD'
        AND COALESCE(priced_cc.price_estimated, 0) = 0
    `, keys);

    for (const row of rows) {
      const price = cheapestEligiblePrice(row);
      if (price === null) continue;
      const current = bestByKey.get(row.card_key);
      if (current === undefined || price < current.price ||
          (price === current.price && String(row.id) < String(current.card_id))) {
        bestByKey.set(row.card_key, { card_id: row.id, price });
      }
    }
  }
  return bestByKey;
}

async function getDeckMinimumValues(client, deckIds = []) {
  const normalizedDeckIds = [...new Set(deckIds.map(Number).filter(Number.isSafeInteger))];
  if (normalizedDeckIds.length === 0) return new Map();
  const requirements = await getDeckRequirements(client, normalizedDeckIds);
  return computeMinimumValues(client, requirements, normalizedDeckIds, 'deck_id');
}

async function getListRequirements(client, listIds) {
  const requirements = [];
  for (const ids of chunked(listIds)) {
    const placeholders = ids.map(() => '?').join(', ');
    const rows = await client.all(`
      SELECT
        lc.list_id,
        COALESCE(NULLIF(${sqlCardKey('list_cc')}, ''), 'missing:' || lc.card_id) AS card_key,
        SUM(lc.quantity) AS quantity
      FROM list_cards lc
      LEFT JOIN card_cache list_cc ON list_cc.id = lc.card_id
      WHERE lc.list_id IN (${placeholders})
        AND lc.quantity > 0
      GROUP BY lc.list_id, card_key
    `, ids);
    requirements.push(...rows);
  }
  return requirements;
}

// Same floor for card lists (wishlists/buylists): what completing every wanted
// copy costs right now if each card is bought at its cheapest known printing.
async function getListMinimumValues(client, listIds = []) {
  const normalizedListIds = [...new Set(listIds.map(Number).filter(Number.isSafeInteger))];
  if (normalizedListIds.length === 0) return new Map();
  const requirements = await getListRequirements(client, normalizedListIds);
  return computeMinimumValues(client, requirements, normalizedListIds, 'list_id');
}

// Shared aggregation for decks and lists: both reduce to (owner id, logical
// card key, quantity) requirements, and both price them the same way — the
// cheapest USD printing/finish on record, with unpriced demand counted aside
// so the UI can show an honest floor with a "+".
async function computeMinimumValues(client, requirements, ownerIds, ownerField) {
  const values = new Map(ownerIds.map(id => [id, emptyDeckMinimumValue()]));
  const cardKeys = [...new Set(
    requirements
      .map(row => row.card_key)
      .filter(cardKey => cardKey && !cardKey.startsWith('missing:'))
  )];
  const cheapestByKey = await getCheapestPrices(client, cardKeys);

  for (const requirement of requirements) {
    const ownerId = Number(requirement[ownerField]);
    const quantity = Number(requirement.quantity) || 0;
    const value = values.get(ownerId);
    if (!value || quantity <= 0) continue;

    const price = cheapestByKey.get(requirement.card_key);
    if (price === undefined) {
      value.unpriced_cards += quantity;
      value.unpriced_card_types += 1;
    } else {
      value.minimum_value += quantity * price;
    }
  }

  for (const value of values.values()) {
    value.minimum_value = Math.round((value.minimum_value + Number.EPSILON) * 100) / 100;
  }
  return values;
}

function emptyDeckMinimumValue() {
  return {
    minimum_value: 0,
    minimum_value_currency: 'USD',
    unpriced_cards: 0,
    unpriced_card_types: 0,
  };
}

// The deck's OWN printing priced the same way the floor prices any printing:
// cheapestEligiblePrice over the four price fields, and only when the row's
// currency is USD (COALESCE semantics identical to the floor query's). A
// printing with no eligible positive price, or one quoted in another currency,
// yields null — "unpriced" rather than "$0.00", the same honesty rule behind
// the floor's "+".
function currentPrintingPrice(row) {
  if (!row) return null;
  const currency = row.price_currency == null ? 'USD' : String(row.price_currency).toUpperCase();
  if (currency !== 'USD') return null;
  return cheapestEligiblePrice(row);
}

// Deck totals for the current-printings axis, summed from per-card rows that
// already carry {quantity, current_price}. Mirrors computeMinimumValues'
// accounting exactly: unpriced counts COPIES (so the UI's "+" gate matches the
// existing unpriced_cards), priced copies multiply in, and the sum rounds to
// cents the same way.
function deckCurrentPrintValues(rows) {
  let value = 0;
  let unpricedCopies = 0;
  let estimatedCopies = 0;
  for (const row of rows || []) {
    const quantity = Number(row.quantity) || 0;
    if (quantity <= 0) continue;
    const price = row.current_price;
    if (price === null || price === undefined) {
      unpricedCopies += quantity;
    } else {
      value += quantity * price;
      // Priced from the cheapest other printing (utils/priceEstimates): the UI
      // marks the total "~".
      if (Number(row.price_estimated) && price > 0) estimatedCopies += quantity;
    }
  }
  return {
    current_printing_value: Math.round((value + Number.EPSILON) * 100) / 100,
    current_unpriced_cards: unpricedCopies,
    current_estimated_cards: estimatedCopies,
  };
}

module.exports = {
  getDeckMinimumValues,
  getListMinimumValues,
  getCheapestPrintings,
  getCheapestPrices,
  emptyDeckMinimumValue,
  currentPrintingPrice,
  deckCurrentPrintValues,
};
