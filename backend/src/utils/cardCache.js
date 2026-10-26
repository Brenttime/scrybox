// The one place that knows card_cache's column list.
//
// Every provider used to carry its own copy of this INSERT, so adding a column
// meant finding all of them — and a third provider would have made a fourth copy.
// They all produce the same normalized card shape before writing, so they all
// write through here instead.
const db = require('../db');

const COLUMNS = [
  'id', 'oracle_id', 'scryfall_search_eligible', 'name', 'supertype', 'subtypes', 'types', 'rarity', 'set_id', 'set_name',
  'number', 'image_url', 'price_trend', 'price_normal', 'price_holofoil', 'price_etched',
  'cmc', 'color_identity', 'language', 'printed_name',
  'tcgplayer_url', 'cardmarket_url', 'tcgplayer_product_id',
  'price_currency', 'price_source',
  'oracle_text', 'oracle_faces',
];

// A page of results can be 250 cards and one round trip per card cost more than
// the provider fetch did, so rows go in batched — chunked small enough that the
// bound-parameter count stays well inside SQLite's limit.
const CHUNK = 50;

const num = (v) => (v == null || v === '' || Number.isNaN(Number(v)) ? null : Number(v));

// Upsert, not INSERT OR REPLACE. REPLACE deletes the row and inserts a new one, so
// every column this list does NOT name is silently reset to its default — which
// would cost the English name learned below the same way.
const PRICE_COLUMNS = new Set(['price_trend', 'price_normal', 'price_holofoil', 'price_etched', 'price_currency', 'price_source']);
const SET_CLAUSE = COLUMNS
  .filter(c => c !== 'id' && c !== 'name')
  // Partial set/catalog rows may not carry identity or layout eligibility.
  // Never erase durable values learned from a complete Scryfall card response.
  .map(c => (c === 'oracle_id' || c === 'scryfall_search_eligible' || c === 'oracle_text' || c === 'oracle_faces')
    ? `${c} = COALESCE(excluded.${c}, card_cache.${c})`
    // A row with no price_source came from a price-less source (local sidecar,
    // partial catalog rows): keep the last real quote rather than zero it.
    : PRICE_COLUMNS.has(c)
      ? `${c} = CASE WHEN excluded.price_source IS NULL THEN card_cache.${c} ELSE excluded.${c} END`
      : `${c} = excluded.${c}`)
  .join(', ');

// `name` is the one column an incoming row can be WORSE at. A non-English card
// carries its localized name in both `name` and `printed_name` when the source
// has no English name to give — but one may since have been learned from the
// card's English printing, and that is what makes a Japanese card findable by
// typing its English name. Keep it: an incoming name that IS just the printed
// name is not new information.
const NAME_CLAUSE = `name = CASE
    WHEN excluded.name = excluded.printed_name AND card_cache.name <> card_cache.printed_name
    THEN card_cache.name ELSE excluded.name END`;

// Upsert already-normalized cards.
//
// `opts.incomplete` marks rows written from a partial source that carries only
// id/name/number/image, no rarity, types or prices. Such a row is stamped as
// already-stale so every freshness check in the app treats it as needing a
// refetch, instead of it looking current forever and showing a real card with
// $0.00 and the wrong rarity.
async function cacheNormalizedCards(cards, opts = {}) {
  const stamp = opts.incomplete ? `'1970-01-01 00:00:00'` : 'CURRENT_TIMESTAMP';
  const rowSql = `(${COLUMNS.map(() => '?').join(', ')}, ${stamp})`;
  const rows = (cards || []).filter(c => c && c.id);
  for (let i = 0; i < rows.length; i += CHUNK) {
    const chunk = rows.slice(i, i + CHUNK);
    const params = [];
    for (const c of chunk) {
      params.push(
        c.id, c.oracle_id || null,
        c.scryfall_search_eligible == null ? null : Number(c.scryfall_search_eligible),
        c.name || '', c.supertype || '',
        JSON.stringify(c.subtypes || []), JSON.stringify(c.types || []),
        c.rarity || 'Common', c.set_id || '', c.set_name || '', c.number || '',
        c.image_url || '', num(c.price_trend), num(c.price_normal),
        num(c.price_holofoil), num(c.price_etched), num(c.cmc),
        JSON.stringify(c.color_identity || []),
        c.language || 'English', c.printed_name || null,
        c.tcgplayer_url || null, c.cardmarket_url || null,
        num(c.tcgplayer_product_id),
        c.price_currency || 'USD', c.price_source || null,
        c.oracle_text == null ? null : String(c.oracle_text),
        c.oracle_faces == null ? null : (typeof c.oracle_faces === 'string' ? c.oracle_faces : JSON.stringify(c.oracle_faces)),
      );
    }
    await db.run(
      `INSERT INTO card_cache (${COLUMNS.join(', ')}, last_updated)
       VALUES ${chunk.map(() => rowSql).join(', ')}
       ON CONFLICT(id) DO UPDATE SET ${NAME_CLAUSE}, ${SET_CLAUSE}, last_updated = ${stamp}`,
      params
    );
  }
}

module.exports = { cacheNormalizedCards, CARD_CACHE_COLUMNS: COLUMNS };
