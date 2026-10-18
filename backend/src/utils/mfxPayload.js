// Pure helpers for the Moxfield sync. Kept apart from moxfieldSync.js on
// purpose: this module has no database or network dependencies, so it can be
// unit-checked with plain node the way every other utils/*.test.js does.
// moxfieldSync.js re-exports everything here unchanged.

// Boards to mirror from a Moxfield /v3/decks/all payload. Everything else
// (attractions, stickers, maybeboard, the token boards) is a Moxfield display
// concern with no Scrybox equivalent. Commander decks put their commander(s)
// in `commanders`, not the mainboard — a 99+1 layout — so they mirror into
// the same 100-card slot.
const MIRROR_BOARDS = ['mainboard', 'commanders', 'sideboard'];

// Pull the board maps out of a /v3/decks/all payload into a flat, ordered list
// of { board, card, quantity } — the shape the rest of the sync works on.
// Moxfield's payload shape (verified live, 2026-08):
//   details.boards = { mainboard: { count, cards: { <key>: { quantity, card } } }, ... }
// Tokens / proxies that carry no scryfall_id are skipped (they have no
// Scrybox card to map onto).
function extractDeckCards(details) {
  const boards = (details && details.boards) || {};
  const out = [];
  for (const boardName of MIRROR_BOARDS) {
    const board = boards[boardName];
    const cards = board && board.cards;
    if (!cards || typeof cards !== 'object') continue;
    for (const entry of Object.values(cards)) {
      const card = entry && entry.card;
      if (!card || !card.scryfall_id) continue;
      const quantity = Math.max(0, parseInt(entry.quantity, 10) || 0);
      if (quantity === 0) continue;
      out.push({ board: boardName, card, quantity });
    }
  }
  return out;
}

// Maybeboard entries ({ board: 'maybeboard', card, quantity }), kept apart from
// extractDeckCards so they never reach deck_cards (stats, checkout, value).
function extractMaybeCards(details) {
  const board = details && details.boards && details.boards.maybeboard;
  const cards = board && board.cards;
  if (!cards || typeof cards !== 'object') return [];
  const out = [];
  for (const entry of Object.values(cards)) {
    const card = entry && entry.card;
    if (!card || !card.scryfall_id) continue;
    const quantity = Math.max(0, parseInt(entry.quantity, 10) || 0);
    if (quantity === 0) continue;
    out.push({ board: 'maybeboard', card, quantity });
  }
  return out;
}

// Sum of quantities per board, for the summary columns.
function boardCounts(entries) {
  const counts = { mainboard: 0, sideboard: 0, maybeboard: 0, commanders: 0 };
  for (const e of entries) counts[e.board] += e.quantity;
  return counts;
}

// Map a Moxfield card onto Scrybox's card_cache id: mtg-<scryfall uuid>.
// The scryfall_id Moxfield hands us is exactly the UUID that lives after the
// 'mtg-' prefix in card_cache.id — verified against the running collection.
function scryboxCardId(card) {
  return `mtg-${card.scryfall_id}`;
}

// Moxfield's format string → the label the deck builder stores.
function mfxFormatLabel(format) {
  switch (String(format || '').toLowerCase()) {
    case 'commander':
    case 'edh':
    case 'oathbreaker':
      return 'Commander / EDH';
    case 'standard': return 'Standard';
    case 'modern': return 'Modern';
    case 'pioneer': return 'Pioneer';
    case 'legacy': return 'Legacy';
    case 'vintage': return 'Vintage';
    case 'pauper': return 'Pauper';
    default: return format || 'Commander / EDH';
  }
}

// Moxfield target_size follows the format the way the deck builder does:
// constructed formats with a sideboard get 75, everything else (commander) 100.
function targetSizeForFormat(format) {
  return ['standard', 'modern', 'pioneer', 'legacy', 'vintage']
    .includes(String(format || '').toLowerCase()) ? 75 : 100;
}

// A few cards Moxfield lists — most often sealed-product printings like Mystery
// Booster — carry a `scryfall_id` that Scryfall has no record for, so the id
// backfill 404s and would leave a dangling deck_card -> card_cache reference
// (a blank slot in the UI). Moxfield still hands us the card's own data, so
// synthesize a minimal card_cache row from it. The shape matches what
// scryfallApi.normalizeCard produces, so it can be written straight through
// cacheCards. price_source='moxfield' is what tells the price sweep and any
// other Scryfall-keyed feature that this row is not a real Scryfall card.
const COLOR_NAMES = { W: 'White', U: 'Blue', B: 'Black', R: 'Red', G: 'Green' };
function synthesizeMoxfieldCard(scryboxId, card) {
  const c = card || {};
  const typeLine = c.type_line || '';
  const colors = Array.isArray(c.colors) ? c.colors : [];
  const colorWords = colors.map(x => COLOR_NAMES[x] || x);
  return {
    id: scryboxId,
    name: c.name || 'Unknown',
    supertype: 'MTG',
    subtypes: typeLine.split(/[^A-Za-z]+/).filter(Boolean),
    types: colorWords,
    rarity: 'Common',
    set_id: c.set || '',
    set_name: c.set_name || '',
    number: String(c.cn != null ? c.cn : ''),
    image_url: (c.image_uris && (c.image_uris.normal || c.image_uris.large)) || '',
    price_trend: 0,
    price_normal: null,
    price_holofoil: null,
    cmc: c.cmc != null && !Number.isNaN(Number(c.cmc)) ? Number(c.cmc) : null,
    color_identity: colorWords,
    language: 'English',
    printed_name: null,
    tcgplayer_url: null,
    cardmarket_url: null,
    tcgplayer_product_id: null,
    price_currency: 'USD',
    price_source: 'moxfield'
  };
}

module.exports = {
  MIRROR_BOARDS,
  extractDeckCards,
  extractMaybeCards,
  boardCounts,
  scryboxCardId,
  mfxFormatLabel,
  targetSizeForFormat,
  synthesizeMoxfieldCard
};
