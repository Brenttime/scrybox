const express = require('express');
const db = require('../db');
const scryfallApi = require('../scryfallApi');
const cvScan = require('../cvScan');
const languages = require('../utils/languages');
const cardApi = require('../utils/cardApi');
const { searchLimiter } = require('../middleware/auth');
const { resolveCardPrice, parseCardRow, recordPrice } = require('../utils/priceHelpers');
const { parseSetList } = require('../utils/setQuery');
const { checkedOutAllocation, logicalInventoryStatus, withAllocationLock, setStackQuantity } = require('../utils/collectionHelpers');
const { sqlCardKey } = require('../utils/cardIdentity');
const { validateDeckAddition } = require('../utils/deckRules');
const { splitPrice } = require('../utils/splitPrice');
const { buildCardListText } = require('../../../shared/cardListText.js');

const router = express.Router();

// The one shared definition of a legal collection.printing value — the add,
// bulk-add and bulk-set routes all write the column, so they must agree with
// the CHECK constraint in db.js. Anything outside this list is rejected before
// it can be stored.
const PRINTING_VALUES = ['Normal', 'Holofoil'];

// Stamp each result with how many copies the user already owns, so browsing a
// set shows what is already in the binder instead of inviting duplicate adds.
// A collection-scope search already reports owned_qty from its own join.
async function attachOwnedQty(cards, userId) {
  if (!Array.isArray(cards) || cards.length === 0 || !userId) return;
  // Carry enough provider metadata into SQLite to identify results that have
  // not been cached yet. Some internal callers expose the printing id as
  // `card_id`, while normalized provider cards use `id`.
  const targets = cards.map(card => ({
    // Collection-shaped rows can contain both: `id` is then the physical row
    // id and `card_id` is the provider printing id, so the explicit alias wins.
    card_id: card && (card.card_id || card.id) ? (card.card_id || card.id) : null,
    name: card && card.name ? card.name : null,
  }));
  const rows = await db.all(
    `WITH target_cards AS (
       SELECT
         CAST(key AS INTEGER) AS target_index,
         json_extract(value, '$.card_id') AS card_id,
         json_extract(value, '$.name') AS name
       FROM json_each(?)
     ),
     target_identity AS (
       SELECT
         target_cards.target_index,
         COALESCE(
           NULLIF(${sqlCardKey('cached_target')}, ''),
           NULLIF(LOWER(TRIM(target_cards.name)), '')
         ) AS card_key
       FROM target_cards
       LEFT JOIN card_cache cached_target
         ON cached_target.id = target_cards.card_id
     ),
     owned AS MATERIALIZED (
       SELECT ${sqlCardKey('owned_cc')} AS card_key, SUM(c.quantity) AS qty
       FROM collection c
       JOIN card_cache owned_cc ON owned_cc.id = c.card_id
       WHERE c.user_id = ? AND c.quantity > 0
       GROUP BY ${sqlCardKey('owned_cc')}
     )
     SELECT target_identity.target_index, COALESCE(owned.qty, 0) AS qty
     FROM target_identity
     LEFT JOIN owned ON owned.card_key = target_identity.card_key
     ORDER BY target_identity.target_index`,
    [JSON.stringify(targets), userId]
  );
  const owned = new Map(rows.map(row => [row.target_index, row.qty]));
  // Every search result keeps its own art/set/collector number, but the owned
  // badge answers "how many of this game card do I own?" across all printings.
  for (let i = 0; i < cards.length; i++) cards[i].owned_qty = owned.get(i) || 0;
}

// 1. Search cards (Scryfall + database cache).

async function searchCards(req, res) {
  const query = req.method === 'POST' ? { ...req.query, ...req.body } : req.query;
  const { name, number, set, scope = 'database', lang, prints, image, cropped } = query;
  // `q` is a raw Scryfall-syntax query (e.g. "is:land color:g rarity:rare").
  // When present it replaces the name/number/set fields entirely; the backend
  // passes it through to Scryfall verbatim.
  const q = typeof query.q === 'string' ? query.q : '';
  // 1-based page over `limit`-sized pages. Collection-catalog pages are local
  // rows and may be large (bulk-selection uses them); ordinary internet search
  // remains capped at 250 so one request cannot walk an excessive Scryfall
  // window.
  const page = Math.max(1, parseInt(query.page, 10) || 1);
  const maxLimit = scope === 'collection' && q.trim() ? 2000 : 250;
  const limit = Math.min(maxLimit, Math.max(1, parseInt(query.limit, 10) || 60));
  const hasCatalogWindow = query.page != null || query.limit != null;
  try {
    // A raw query against the collection scope that uses an operator the
    // stored rows cannot answer (otag:, availability:, artist:, ...) is
    // resolved LIVE against Scryfall and intersected with what this user
    // owns — that is the only way a tag can be answered at all. Queries made
    // of data-backed operators only (is:land color:g …) stay out of the API:
    // the client evaluates those against its loaded rows, and the field-filter
    // path below keeps searchCards' pinned no-API contract for collection scope.
    if (scope === 'collection' && q.trim()) {
      const { classifyQuery, QuerySyntaxError } = require('../../../shared/scryfallQuery.js');
      let mode;
      try {
        mode = classifyQuery(q).mode;
      } catch (e) {
        // A genuine syntax error is a fixable query, not a down API.
        if (e instanceof QuerySyntaxError) {
          return res.status(400).json({ error: 'INVALID_QUERY' });
        }
        throw e;
      }
      // Local-mode queries are answered from the owned rows in SQLite (no API);
      // catalog-mode ones resolve on Scryfall / the Oracle Tags index. Either way
      // the response is the user's own collection rows.
      if (mode === 'catalog' || mode === 'local') {
        const includeTotal = query.count !== '0';
        const { cards, total, complete, upstreamTotal, cacheStatus, snapshot } = await scryfallApi.resolveCollectionQuery({
          q, userId: req.user.id, lang,
          // Preserve the historical API contract: callers that omit page and
          // limit receive the complete owned result. Only explicit pagination
          // opts into a bounded row window.
          rowLimit: hasCatalogWindow ? limit : null,
          rowOffset: hasCatalogWindow ? (page - 1) * limit : 0,
          includeTotal,
          expectedSnapshot: typeof query.snapshot === 'string' ? query.snapshot : null,
        });
        if (total != null) res.set('X-Total-Count', String(total));
        // Completeness: a walk that hit its page cap answered from a real
        // PREFIX of the tag's match list. Tell the UI so it can say "may be
        // incomplete" instead of presenting a truncated result as the whole.
        res.set('X-Catalog-Complete', complete ? '1' : '0');
        if (upstreamTotal != null) res.set('X-Catalog-Upstream-Total', String(upstreamTotal));
        // 'fresh' = served from the durable cache (no upstream this request);
        // 'stale' = cache served + background refresh in flight; 'resolved' =
        // this request paid for the walk.
        res.set('X-Catalog-Cache', cacheStatus || 'unknown');
        if (snapshot) res.set('X-Catalog-Snapshot', snapshot);
        res.set('Access-Control-Expose-Headers', 'X-Total-Count, X-Catalog-Complete, X-Catalog-Upstream-Total, X-Catalog-Cache, X-Catalog-Snapshot');
        return res.json(cards);
      }
    }
    let { cards, total, source } = await scryfallApi.searchCards({
      name, number, set, q, scope, userId: req.user.id, lang,
      allPrints: prints === '1', page, limit,
    });
    // A scanner-assisted search keeps Scryfall's candidate recall, then orders
    // those printings by similarity to the captured card. This is MTG-only even
    // though the upstream implementation was game-generic.
    if (image && Array.isArray(cards) && cards.length > 0) {
      const base64 = typeof image === 'string' ? image.replace(/^data:image\/\w+;base64,/, '') : '';
      if (base64) {
        const langName = languages.toName(lang);
        if (cvScan.isBuilt('mtg', langName)) {
          cards = await cvScan.scoreCards(Buffer.from(base64, 'base64'), 'mtg', cards, {
            lang: langName,
            cropped: !!cropped,
          });
        }
      }
    }
    if (scope !== 'collection') await attachOwnedQty(cards, req.user.id);
    // Header, not the body: every existing caller expects a bare array here.
    if (total != null) {
      res.set('X-Total-Count', String(total));
      res.set('Access-Control-Expose-Headers', 'X-Total-Count');
    }
    // Where the answer came from: 'cache' (answered from the local card_cache —
    // instant, no rate limit) or 'scryfall' (a live call). The UI uses this to
    // show "local" vs "live" so a user knows when a search is instant.
    if (source) {
      res.set('X-Source', source);
      res.set('Access-Control-Expose-Headers', 'X-Total-Count, X-Source');
    }
    res.json(cards);
  } catch (error) {
    console.error(error);
    if (error.message === 'INVALID_API_KEY') {
      return res.status(403).json({ error: 'Invalid API Key' });
    }
    if (error.message === 'RATE_LIMIT_EXCEEDED') {
      return res.status(429).json({ error: 'Rate limit exceeded' });
    }
    if (error.message === 'INVALID_QUERY') {
      return res.status(400).json({ error: 'INVALID_QUERY' });
    }
    if (error.message === 'CATALOG_SNAPSHOT_CHANGED') {
      return res.status(409).json({ error: 'CATALOG_SNAPSHOT_CHANGED' });
    }
    if (error.message === 'UPSTREAM_UNAVAILABLE') {
      return res.status(503).json({ error: 'Card API is having trouble. Try again in a moment.' });
    }
    res.status(500).json({ error: 'Search failed' });
  }
}

// Search is intentionally read-only. POST exists solely to carry an optional
// base64 scan image that is too large and sensitive for a query string.
router.get('/search', searchLimiter, searchCards);
router.post('/search', searchLimiter, searchCards);

// 1b. Identify a scanned card image by visual-feature match.
// Which sets the scanner can actually answer for, and how completely.
//
// Not admin-only: this is what the set filter needs to stop offering sets that
// match nothing. Read-only counts, no build controls.
router.get('/scan-sets', async (req, res) => {
  const { lang } = req.query;
  try {
    // `builtLangs` rides along so the scanner's language picker knows which
    // languages a local catalog actually exists for.
    res.json({
      ...await require('../catalog').setCounts('mtg', languages.toName(lang)),
      builtLangs: cvScan.builtLangs('mtg'),
    });
  } catch (e) {
    console.error('scan-sets failed:', e.message);
    res.status(500).json({ error: 'Could not read catalog set counts' });
  }
});

router.post('/scan-match', searchLimiter, async (req, res) => {
  try {
    const { image, set = '', lang, cropped = false } = req.body || {};
    if (!image || typeof image !== 'string') return res.status(400).json({ error: 'Missing image' });
    const base64 = image.includes(',') ? image.slice(image.indexOf(',') + 1) : image;
    const buf = Buffer.from(base64, 'base64');
    if (buf.length < 100) return res.status(400).json({ error: 'Invalid image data' });

    const langName = languages.toName(lang);

    // Set scoping is a FILTER over the catalog, not a different pipeline. The ORB
    // path needed a per-set index built before it could scope a scan (that was the
    // client's "preparing set" wait); a cosine sweep just skips rows that do not
    // belong. A scoped scan is the same scan over fewer candidates — cheaper and
    // more accurate, with nothing to build.
    //
    // Language is not a gate either. A card's artwork is the same in every
    // language, so the English catalog answers "which card is this" for a Spanish
    // or Japanese photo; when a catalog exists in the scanned language cvScan uses
    // it instead. Measured 100% right-card on Spanish/Japanese/French/Italian MTG
    // off the English catalog.
    if (!cvScan.isBuilt('mtg', langName)) {
      // There is no second matcher to fall back to. Say what is missing and what
      // fixes it, rather than an empty candidate list that reads to the user as
      // "your card could not be identified".
      return res.status(503).json({
        lang: langName, candidates: [], verified: false, notBuilt: true,
        error: 'No scan catalog is built for this language yet. An admin can build one from Settings → Scan catalogs.',
      });
    }

    const result = await cvScan.match(buf, 'mtg', 8, { sets: parseSetList(set), lang: langName, cropped: !!cropped });

    result.candidates = await Promise.all(result.candidates.map(async (cand) => {
      // getCardById fetches and caches a printing this install has never seen,
      // so a match the cache has no row for still resolves.
      if (cand.cardId) {
        const card = await scryfallApi.getCardById(cand.cardId).catch(() => null);
        if (!card) return cand;
        // The catalog that answered is whichever one exists, and for most
        // installs that is the English one — the artwork is identical across
        // languages, so an English catalog identifies a Japanese card perfectly
        // well and then hands back the ENGLISH printing. Re-express it in the
        // scanned language here (cvScan.load defers to the route for exactly
        // this), or the picker shows English names and art, and the copy gets
        // filed as the English printing.
        //
        // Same set and collector number, different Scryfall id: the localized
        // card IS a different printing row, which is the one the collection
        // should reference.
        const localized = await scryfallApi
          .getPrintingInLang(card.set_id, card.number, langName)
          .catch(() => null);
        const use = localized || card;
        return { ...cand, name: use.name, set: use.set_id, number: use.number, card: use };
      }
      return cand;
    }));
    return res.json(result);
  } catch (error) {
    console.error('scan-match failed:', error.message);
    res.status(500).json({ error: 'Scan match failed' });
  }
});

// Text card list of what the user owns — the shape ManaBox / TCGplayer
// buylist tools paste: "qty Name" (plain) or "qty Name (SET) num" (detailed).
// The frontend bulk bar formats the same rows itself, so this endpoint exists
// for scripts and API-key users who want the identical text without pulling
// the whole /api/collection payload.
router.get('/collection/cardlist', async (req, res) => {
  try {
    const style = req.query.style === 'detailed' ? 'detailed' : 'plain';
    const unresolved = await db.get(`
      SELECT COUNT(*) AS count
      FROM collection c
      LEFT JOIN card_cache cc ON cc.id = c.card_id
      WHERE c.user_id = ? AND c.quantity > 0 AND cc.id IS NULL
    `, [req.user.id]);
    if (Number(unresolved && unresolved.count) > 0) {
      return res.status(422).json({
        error: 'The collection contains cards whose details are unavailable. Repair their metadata before exporting.'
      });
    }
    const rows = await db.all(
      `SELECT c.quantity, cc.name, cc.set_id, cc.number
       FROM collection c
       JOIN card_cache cc ON c.card_id = cc.id
       WHERE c.user_id = ? AND c.quantity > 0
       ORDER BY c.added_at DESC`,
      [req.user.id]
    );
    res.type('text/plain').send(buildCardListText(rows, style));
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to build card list' });
  }
});

// 2. Get User's Collection
router.get('/collection', async (req, res) => {
  try {
    const isTrade = req.query.is_trade;
    const paginated = req.query.limit !== undefined || req.query.offset !== undefined;
    const limit = Math.min(2000, Math.max(1, parseInt(req.query.limit, 10) || 100));
    const offset = Math.max(0, parseInt(req.query.offset, 10) || 0);

    let filterSql = `WHERE c.user_id = ?`;
    let filterParams = [req.user.id];

    if (isTrade !== undefined) {
      filterSql += ` AND c.is_trade = ?`;
      filterParams.push(isTrade === 'true' || isTrade === '1' ? 1 : 0);
    }

    const query = `
      SELECT
        c.id as entry_id,
        c.card_id,
        c.quantity,
        c.condition,
        c.printing,
        c.language,
        c.purchase_price,
        c.added_at,
        c.is_trade,
        c.notes,
        cc.name,
        -- The localized name for a non-English printing, so every view that
        -- renders a collection card can show it as the card actually reads.
        cc.printed_name,
        cc.supertype,
        cc.subtypes,
        cc.types,
        cc.cmc,
        cc.color_identity,
        cc.rarity,
        cc.set_id,
        cc.set_name,
        cc.number,
        cc.image_url,
        cc.price_trend,
        cc.price_normal,
        cc.price_holofoil,
        cc.price_currency,
        cc.price_source,
        cc.tcgplayer_url,
        cc.cardmarket_url,
        cc.tcgplayer_product_id
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      ${filterSql}
      ORDER BY c.added_at DESC, c.id DESC
      ${paginated ? 'LIMIT ? OFFSET ?' : ''}
    `;
    if (paginated && req.query.count !== '0') {
      const countRow = await db.get(
        `SELECT COUNT(*) AS count
         FROM collection c
         JOIN card_cache cc ON c.card_id = cc.id
         ${filterSql}`,
        filterParams
      );
      res.set('X-Total-Count', String(Number(countRow && countRow.count) || 0));
      res.set('Access-Control-Expose-Headers', 'X-Total-Count');
    }

    const queryParams = paginated ? [...filterParams, limit, offset] : filterParams;
    const rows = await db.all(query, queryParams);

    // Keep the legacy full-response contract for API consumers. The paginated
    // collection UI does not render checked_out_qty, so its cold path skips this
    // collection-wide calculation unless a caller explicitly asks for it.
    const alloc = !paginated || req.query.include_allocation === '1'
      ? await checkedOutAllocation(req.user.id)
      : null;

    const formatted = rows.map(row => {
      const card = {
        ...parseCardRow(row),
        price_trend: resolveCardPrice(row)
      };
      if (alloc) card.checked_out_qty = alloc.get(row.entry_id) || 0;
      return card;
    });

    res.json(formatted);
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to fetch collection' });
  }
});

// Shared by the single add below and the bulk add after it, so one card and two
// hundred cards travel exactly the same path (cache lookup, localization,
// insertion, price history). Throws AddCardError for caller-visible
// failures; anything else is a genuine 500.
class AddCardError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}

async function addCardToCollection(user, body) {
  const {
    card_id,
    quantity = 1,
    condition = 'Near Mint',
    printing = 'Normal',
    language = 'English',
    purchase_price = 0,
    is_trade = 0,
    stackable = false
  } = body;
  const req = { user, body };

  if (!card_id) {
    throw new AddCardError(400, 'card_id is required');
  }
  if (!PRINTING_VALUES.includes(printing)) {
    throw new AddCardError(400, `Invalid printing: ${printing}`);
  }

  {
    // A card matched by a set-scoped scan was cached from a set brief:
    // name, number and art only. Fill it in before it enters the collection, or it
    // is stored with no price, no marketplace link and a defaulted rarity — which
    // is what it then shows in the inspector forever.
    await cardApi.hydrate(card_id);

    let card = await db.get(`SELECT * FROM card_cache WHERE id = ?`, [card_id]);
    if (!card) {
      card = await cardApi.getCardById(card_id);
      if (!card) {
        throw new AddCardError(404, `Card ID ${card_id} not found.`);
      }
    }

    // File the copy against the printing it actually IS. The card was picked in
    // whatever language the search ran in, but `language` is set separately — Quick
    // Add's dropdown, a scan the English catalog answered — so a Japanese copy
    // routinely arrived pointing at the English row, and then showed the English
    // name, art and price in every view. Null (never printed in that language)
    // keeps the row that was picked.
    let cardId = card_id;
    const localized = await cardApi.printingInLanguage(card, language);
    if (localized) {
      card = localized;
      cardId = localized.id;
    }

    let lastInsertedId = null;
    const insertedIds = [];
    const count = Math.max(1, parseInt(quantity, 10) || 1);
    // Stacking is quantity-on-one-row, which is meaningful only for
    // interchangeable copies.
    const stack = !!stackable;

    if (stack) {
      const result = await db.run(`
        INSERT INTO collection (
          card_id, user_id, quantity, condition, printing, language, purchase_price,
          is_trade
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
      `, [
        cardId, req.user.id, count, condition, printing, language, purchase_price || 0,
        is_trade ? 1 : 0
      ]);
      lastInsertedId = result.lastID;
      insertedIds.push(result.lastID);
    } else {
      for (let i = 0; i < count; i++) {
        const result = await db.run(`
          INSERT INTO collection (
            card_id, user_id, quantity, condition, printing, language, purchase_price,
            is_trade
          ) VALUES (?, ?, 1, ?, ?, ?, ?, ?)
        `, [
          cardId, req.user.id, condition, printing, language, purchase_price || 0,
          is_trade ? 1 : 0
        ]);
        lastInsertedId = result.lastID;
        insertedIds.push(result.lastID);
      }
    }

    await recordPrice(cardId, card.price_trend);

    return {
      message: 'Card added to collection',
      id: lastInsertedId,
      ids: insertedIds
    };
  }
}

// 3. Add Card to Collection
router.post('/collection', async (req, res) => {
  try {
    res.status(200).json(await addCardToCollection(req.user, req.body));
  } catch (error) {
    if (error instanceof AddCardError) {
      return res.status(error.status).json({ error: error.message });
    }
    console.error(error);
    res.status(500).json({ error: 'Failed to add card' });
  }
});

// 3b. Bulk add: one shared condition/printing/quantity across many cards, so a
// set browse can be added in one action instead of one drawer per card.
const BULK_ADD_MAX = 250;

// The shared bulk-add core, used by the route below AND by the Secret Lair
// importer (utils/secretLair.js), which resolves a whole product's cards and
// then files them through here. Extracted rather than duplicated so a bulk
// import and a bulk tray add can never drift on what "added" means: same
// per-card service, same sequential order, same per-card error reporting.
//
// `entries` are card ids, or { card_id, quantity } when the caller knows a
// per-card count (a Secret Lair drop ships 4x of one card and 1x of another).
// An entry's own quantity wins over the shared one, which is what lets the
// route keep passing a flat id list with one shared multiplier.
//
// `purchase_price` follows the same rule for the same reason: a marketplace
// order's lines each carry their OWN unit price (this foil was $4.10, that
// common $0.25), so an entry that knows its price must not be flattened to one
// shared figure the way a set browse is. Quantity alone was safe before
// because every existing caller prices uniformly or not at all; an order
// import is the first caller with per-line truth, so the override rides along
// here rather than a second, subtly-different bulk path being born beside it.
// Returns { added, failed, quantity } — `added` counts card TYPES that landed,
// `quantity` is the fallback multiplier, so a caller can build "12 card types
// (x2 each)" copy without re-deriving it.
async function bulkAddToCollection(user, entries, shared) {
  const added = [];
  const failed = [];
  // Sequential on purpose: preserve input order and avoid overlapping cache
  // hydration/insertion work for duplicate cards in the same request.
  for (const entry of entries) {
    const keyed = entry && typeof entry === 'object';
    const card_id = keyed ? entry.card_id : entry;
    const perCard = {};
    if (keyed && entry.quantity != null && entry.quantity !== '') perCard.quantity = entry.quantity;
    if (keyed && entry.purchase_price != null && entry.purchase_price !== '') {
      perCard.purchase_price = entry.purchase_price;
    }
    // Same argument as purchase_price, and from the same caller: an order line
    // knows its OWN grade (this slab is NM, that one LP), so a per-entry
    // condition must not be flattened to the one shared default either.
    if (keyed && entry.condition != null && entry.condition !== '') perCard.condition = entry.condition;
    // Scanner rows each carry their own finish (one foil among nonfoils).
    if (keyed && entry.printing != null && entry.printing !== '') perCard.printing = entry.printing;

    try {
      const result = await addCardToCollection(user, { ...shared, ...perCard, card_id });
      added.push({ card_id, id: result.id, ids: result.ids });
    } catch (error) {
      if (!(error instanceof AddCardError)) console.error(error);
      failed.push({ card_id, error: error instanceof AddCardError ? error.message : 'Failed to add card' });
    }
  }
  return { added, failed, quantity: Math.max(1, parseInt(shared && shared.quantity, 10) || 1) };
}

router.post('/collection/bulk-add', async (req, res) => {
  const { card_ids = [], ...shared } = req.body;
  if (!Array.isArray(card_ids) || card_ids.length === 0) {
    return res.status(400).json({ error: 'card_ids is required' });
  }
  if (card_ids.length > BULK_ADD_MAX) {
    return res.status(400).json({ error: `Cannot add more than ${BULK_ADD_MAX} cards at once.` });
  }
  const { added, failed, quantity: qty } = await bulkAddToCollection(req.user, card_ids, shared);
  res.status(failed.length && !added.length ? 500 : 200).json({
    message: failed.length
      ? `Added ${added.length} of ${card_ids.length} cards; ${failed.length} failed.`
      : `Added ${added.length} card${added.length === 1 ? '' : 's'}${qty > 1 ? ` (x${qty} each)` : ''} to collection.`,
    added: added.length,
    // Row ids per card, so a caller (Scan Cards' Undo) can remove exactly the
    // copies it just added and nothing older.
    entries: added,
    failed
  });
});

// 4. Update Collection Entry
router.put('/collection/:id', async (req, res) => {
  const { id } = req.params;
  const {
    quantity, condition, printing, language, purchase_price,
    is_trade, notes
  } = req.body;

  try {
    const entry = await db.get(`SELECT * FROM collection WHERE id = ? AND user_id = ?`, [id, req.user.id]);
    if (!entry) return res.status(404).json({ error: 'Collection entry not found' });

    const updates = [];
    const params = [];

    // Absolute, not additive: see the reconcile below. Deliberately NOT part of
    // the UPDATE — setStackQuantity owns the quantity column so the two can
    // never disagree about how many copies the row stands for.
    const requestedQty = quantity !== undefined ? Math.max(1, parseInt(quantity, 10) || 1) : null;
    if (condition !== undefined) { updates.push('condition = ?'); params.push(condition); }
    if (printing !== undefined) {
      // Reject before the DB: the CHECK constraint would reject it too, but only
      // after this request is already half through its writes.
      if (!PRINTING_VALUES.includes(printing)) {
        return res.status(400).json({ error: `Invalid printing: ${printing}` });
      }
      updates.push('printing = ?'); params.push(printing);
    }
    if (language !== undefined) { updates.push('language = ?'); params.push(language); }
    if (purchase_price !== undefined) { updates.push('purchase_price = ?'); params.push(purchase_price); }
    if (is_trade !== undefined) { updates.push('is_trade = ?'); params.push(is_trade ? 1 : 0); }
    if (notes !== undefined) { updates.push('notes = ?'); params.push(notes); }

    const touchesPhysicalStack = requestedQty !== null
      || condition !== undefined || printing !== undefined || language !== undefined;

    const saveEntry = async () => {
      // The row may have changed while this request was waiting for the shared
      // allocation lock. Re-read it inside the lock so a concurrent metadata
      // move cannot make the quantity guard inspect one stack and reconcile a
      // different one.
      const currentEntry = touchesPhysicalStack
        ? await db.get(`SELECT * FROM collection WHERE id = ? AND user_id = ?`, [id, req.user.id])
        : entry;
      if (!currentEntry) {
        const missing = new Error('collection-entry-not-found');
        missing.code = 'COLLECTION_ENTRY_NOT_FOUND';
        throw missing;
      }

      if (requestedQty !== null) {
        // Metadata edits can move this row into another physical stack before
        // setStackQuantity reconciles that stack. Guard the stack that will
        // actually exist after the edit, not the row's old stack; otherwise a
        // one-copy row moved into a four-copy stack and saved as quantity=1
        // could delete four copies that a checked-out deck already reserved.
        const targetCondition = condition !== undefined ? condition : currentEntry.condition;
        const targetPrinting = printing !== undefined ? printing : currentEntry.printing;
        const targetLanguage = language !== undefined ? language : currentEntry.language;
        const targetSiblings = await db.all(`
          SELECT quantity
          FROM collection
          WHERE user_id = ? AND card_id = ? AND condition = ? AND printing = ?
            AND language = ? AND id != ? AND quantity > 0
        `, [
          req.user.id, currentEntry.card_id, targetCondition, targetPrinting,
          targetLanguage, id
        ]);
        const projectedStackQty = Math.max(0, Number(currentEntry.quantity) || 0)
          + targetSiblings.reduce(
            (sum, sibling) => sum + Math.max(0, Number(sibling.quantity) || 0),
            0
          );
        const reduction = Math.max(0, projectedStackQty - requestedQty);
        if (reduction > 0) {
          const status = await logicalInventoryStatus(db, req.user.id, currentEntry.card_id);
          if (!status || Number(status.owned_qty) - reduction < Number(status.locked_qty)) {
            const conflict = new Error('allocation-conflict');
            conflict.code = 'ALLOCATION_CONFLICT';
            throw conflict;
          }
        }
      }

      if (updates.length > 0) {
        const updateParams = [...params, id, req.user.id];
        await db.run(`UPDATE collection SET ${updates.join(', ')} WHERE id = ? AND user_id = ?`, updateParams);
      }

      // Quantity is absolute — it is how many copies the user says they own, and
      // in the stacked collection view the number in the form is the total across
      // the identical rows, not this row alone. So reconcile the whole stack to
      // it, up or down. It used to only ever insert (quantity - 1) extra rows,
      // which made lowering the number a no-op and made every save duplicate the
      // entry instead of editing it.
      if (requestedQty !== null) {
        await setStackQuantity(db, req.user.id, id, requestedQty);
      }
    };

    if (touchesPhysicalStack) await withAllocationLock(saveEntry);
    else await saveEntry();

    res.json({ message: 'Collection entry updated successfully' });
  } catch (error) {
    if (error && error.code === 'COLLECTION_ENTRY_NOT_FOUND') {
      return res.status(404).json({ error: 'Collection entry not found' });
    }
    if (error && error.code === 'ALLOCATION_CONFLICT') {
      return res.status(409).json({
        error: 'Check in the deck using this card before reducing the owned quantity.'
      });
    }
    console.error(error);
    res.status(500).json({ error: 'Failed to update entry' });
  }
});

// 5. Delete Card from Collection
router.delete('/collection/:id', async (req, res) => {
  const { id } = req.params;
  try {
    const outcome = await withAllocationLock(async () => {
      const entry = await db.get(`SELECT id, card_id, quantity FROM collection WHERE id = ? AND user_id = ?`, [id, req.user.id]);
      if (!entry) return { status: 404, error: 'Collection entry not found' };
      const status = await logicalInventoryStatus(db, req.user.id, entry.card_id);
      if (!status || Number(status.owned_qty) - Number(entry.quantity) < Number(status.locked_qty)) {
        return { status: 409, error: 'Check in the deck using this card before removing it.' };
      }
      const result = await db.run(`DELETE FROM collection WHERE id = ? AND user_id = ?`, [id, req.user.id]);
      if (result.changes === 0) return { status: 404, error: 'Collection entry not found' };
      return { status: 200 };
    });
    if (outcome.error) return res.status(outcome.status).json({ error: outcome.error });
    res.json({ message: 'Card removed from collection' });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to remove card' });
  }
});

// 5b. Bulk actions
const BULK_ACTIONS = ['delete', 'trade', 'untrade', 'condition', 'printing', 'purchase_split', 'add_to_deck'];
// Allowed field values mirror the collection table CHECK constraints in db.js.
const BULK_CONDITIONS = ['Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged'];
const BULK_PRINTINGS = PRINTING_VALUES;
router.post('/collection/bulk', async (req, res) => {
  const { entry_ids = [], action, value } = req.body;
  if (!Array.isArray(entry_ids) || entry_ids.length === 0) {
    return res.status(400).json({ error: 'entry_ids is required' });
  }
  if (!BULK_ACTIONS.includes(action)) {
    return res.status(400).json({ error: 'Invalid action' });
  }
  const ids = entry_ids.map(n => parseInt(n, 10)).filter(Number.isInteger);
  if (ids.length === 0) return res.status(400).json({ error: 'No valid entry_ids' });
  const placeholders = ids.map(() => '?').join(',');

  try {
    if (action === 'add_to_deck') {
      const deckId = parseInt(value, 10);
      if (!deckId) return res.status(400).json({ error: 'Invalid deck_id' });
      const deck = await db.get(`SELECT id, checked_out FROM decks WHERE id = ? AND user_id = ?`, [deckId, req.user.id]);
      if (!deck) return res.status(404).json({ error: 'Deck not found' });
      if (deck.checked_out) {
        return res.status(409).json({ error: 'Check this deck in before changing its cards.' });
      }

      // Selected rows can be different physical printings of one game card.
      // Combine them before enforcing deck quantities.
      const rows = await db.all(
        `SELECT MIN(c.card_id) AS card_id, SUM(c.quantity) AS total_qty
         FROM collection c
         JOIN card_cache selected_cc ON selected_cc.id = c.card_id
         WHERE c.id IN (${placeholders}) AND c.user_id = ? AND c.quantity > 0
         GROUP BY ${sqlCardKey('selected_cc')}`,
        [...ids, req.user.id]
      );

      let added = 0;
      const rejected = [];
      for (const row of rows) {
        const equivalent = await db.get(`
          SELECT dc.card_id
          FROM deck_cards dc
          JOIN card_cache existing_cc ON existing_cc.id = dc.card_id
          JOIN card_cache target_cc ON target_cc.id = ?
          WHERE dc.deck_id = ?
            AND ${sqlCardKey('existing_cc')} = ${sqlCardKey('target_cc')}
          ORDER BY (dc.card_id = ?) DESC, dc.card_id
          LIMIT 1
        `, [row.card_id, deckId, row.card_id]);
        const effectiveCardId = equivalent ? equivalent.card_id : row.card_id;
        const existing = await db.get(`
          SELECT COALESCE(SUM(dc.quantity), 0) AS quantity
          FROM deck_cards dc
          JOIN card_cache existing_cc ON existing_cc.id = dc.card_id
          JOIN card_cache target_cc ON target_cc.id = ?
          WHERE dc.deck_id = ?
            AND ${sqlCardKey('existing_cc')} = ${sqlCardKey('target_cc')}
        `, [effectiveCardId, deckId]);
        const current = existing ? existing.quantity : 0;
        const newQty = current + row.total_qty;
        const check = await validateDeckAddition({
          deckId,
          userId: req.user.id,
          cardId: effectiveCardId,
          newQty
        });
        if (!check.ok) { rejected.push(check.error); continue; }
        if (equivalent) {
          const updated = await db.run(`
            UPDATE deck_cards
            SET quantity = CASE WHEN card_id = ? THEN ? ELSE 0 END
            WHERE deck_id = ?
              AND EXISTS (
                SELECT 1 FROM decks guarded_deck
                WHERE guarded_deck.id = deck_cards.deck_id
                  AND guarded_deck.user_id = ?
                  AND guarded_deck.checked_out = 0
              )
              AND card_id IN (
                SELECT existing_cc.id
                FROM card_cache existing_cc
                JOIN card_cache target_cc ON target_cc.id = ?
                WHERE ${sqlCardKey('existing_cc')} = ${sqlCardKey('target_cc')}
              )
          `, [effectiveCardId, newQty, deckId, req.user.id, effectiveCardId]);
          if (!updated.changes) {
            return res.status(409).json({ error: 'The deck was checked out before all selected cards could be added.' });
          }
          await db.run(`DELETE FROM deck_cards WHERE deck_id = ? AND quantity <= 0`, [deckId]);
        } else {
          const inserted = await db.run(
            `INSERT INTO deck_cards (deck_id, card_id, quantity)
             SELECT ?, ?, ?
             WHERE EXISTS (
               SELECT 1 FROM decks guarded_deck
               WHERE guarded_deck.id = ?
                 AND guarded_deck.user_id = ?
                 AND guarded_deck.checked_out = 0
             )`,
            [deckId, effectiveCardId, newQty, deckId, req.user.id]
          );
          if (!inserted.changes) {
            return res.status(409).json({ error: 'The deck was checked out before all selected cards could be added.' });
          }
        }
        added += row.total_qty;
      }
      const msg = rejected.length
        ? (added ? `Added ${added} card(s). ${rejected[0]}` : rejected[0])
        : `Added ${added} card(s) to deck`;
      return res.json({ message: msg, affected: added, rejected: rejected.length });
    }

    if (action === 'delete') {
      const outcome = await withAllocationLock(async () => {
        const removals = await db.all(`
          SELECT MIN(c.card_id) AS card_id, SUM(c.quantity) AS remove_qty
          FROM collection c
          JOIN card_cache selected_cc ON selected_cc.id = c.card_id
          WHERE c.id IN (${placeholders}) AND c.user_id = ? AND c.quantity > 0
          GROUP BY ${sqlCardKey('selected_cc')}
        `, [...ids, req.user.id]);
        for (const removal of removals) {
          const status = await logicalInventoryStatus(db, req.user.id, removal.card_id);
          if (!status || Number(status.owned_qty) - Number(removal.remove_qty) < Number(status.locked_qty)) {
            return { status: 409, error: 'Check in the deck using the selected card before removing it.' };
          }
        }
        const result = await db.run(`DELETE FROM collection WHERE id IN (${placeholders}) AND user_id = ?`, [...ids, req.user.id]);
        return { status: 200, affected: result.changes };
      });
      if (outcome.error) return res.status(outcome.status).json({ error: outcome.error });
      return res.json({ message: `Deleted ${outcome.affected} card(s)`, affected: outcome.affected });
    }

    if (action === 'trade' || action === 'untrade') {
      const result = await db.run(`UPDATE collection SET is_trade = ? WHERE id IN (${placeholders}) AND user_id = ?`, [action === 'trade' ? 1 : 0, ...ids, req.user.id]);
      return res.json({ message: `Updated ${result.changes} card(s)`, affected: result.changes });
    }

    if (action === 'condition' || action === 'printing') {
      const allowed = action === 'condition' ? BULK_CONDITIONS : BULK_PRINTINGS;
      if (!allowed.includes(value)) return res.status(400).json({ error: `Invalid ${action}` });
      // Column name is action, drawn from the BULK_ACTIONS whitelist (not user
      // input), so it is safe to interpolate.
      // Moving a row between physical stacks must serialize with single-row
      // quantity reconciliation. Otherwise the quantity path can inspect the
      // old stack while this bulk edit moves the row, then trim the new stack
      // without having guarded the copies it removes.
      const result = await withAllocationLock(() => db.run(
        `UPDATE collection SET ${action} = ? WHERE id IN (${placeholders}) AND user_id = ?`,
        [value, ...ids, req.user.id]
      ));
      return res.json({ message: `Set ${action} on ${result.changes} card(s)`, affected: result.changes });
    }

    // Distribute a total price paid (a pack/deck) across the selected entries,
    // writing each entry's per-card purchase_price. method 'weighted' splits
    // proportional to market value (price_trend); 'equal' splits evenly. Weighted
    // falls back to equal when no selected card has a market price.
    if (action === 'purchase_split') {
      const total = parseFloat(value && value.total);
      const method = value && value.method === 'equal' ? 'equal' : 'weighted';
      if (!(total >= 0)) return res.status(400).json({ error: 'total must be a non-negative number' });
      const rows = await db.all(
        `SELECT c.id, COALESCE(cc.price_trend, 0) AS price FROM collection c
         LEFT JOIN card_cache cc ON cc.id = c.card_id
         WHERE c.id IN (${placeholders}) AND c.user_id = ?`,
        [...ids, req.user.id]
      );
      if (rows.length === 0) return res.status(400).json({ error: 'No valid entries' });
      const sum = rows.reduce((s, r) => s + (r.price || 0), 0);
      const weighted = method === 'weighted' && sum > 0;
      const shares = splitPrice(rows.map(r => r.price || 0), total, method);
      for (let i = 0; i < rows.length; i++) {
        await db.run(`UPDATE collection SET purchase_price = ? WHERE id = ? AND user_id = ?`, [shares[i], rows[i].id, req.user.id]);
      }
      return res.json({ message: `Split $${total.toFixed(2)} across ${rows.length} card(s) (${weighted ? 'by value' : 'evenly'})`, affected: rows.length });
    }

    return res.status(400).json({ error: 'Unknown action' });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Bulk action failed' });
  }
});

module.exports = router;
// Shared with the Secret Lair importer, which resolves a product's whole card
// list and then files it through the same bulk-add core the tray uses. The
// router is what server.js mounts, so the helper rides along as a property
// rather than replacing the export.
module.exports.bulkAddToCollection = bulkAddToCollection;
