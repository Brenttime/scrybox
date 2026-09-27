const axios = require('axios');
const crypto = require('crypto');
const db = require('./db');
const { parseCardRow, recordPrice, shouldSweepPrices, markPricesSwept, resolveCardPrice } = require('./utils/priceHelpers');
const { parseSetList } = require('./utils/setQuery');
const cardSearchSql = require('./utils/cardSearchSql');
const languages = require('./utils/languages');
const { cacheNormalizedCards } = require('./utils/cardCache');
const { sqlCardKey } = require('./utils/cardIdentity');
const { oracleIdForCard, defaultSearchEligible } = require('./utils/oracleId');

// Scryfall needs no API key but asks callers to identify themselves and accept
// JSON. See https://scryfall.com/docs/api. IDs from Scryfall are UUIDs / set-num
// slugs; we prefix them with "mtg-" so the game is derivable from the id.
const client = axios.create({
  baseURL: 'https://api.scryfall.com',
  timeout: 6000,
  headers: { 'User-Agent': 'Scrybox/1.0', 'Accept': 'application/json' }
});

// Search, per-set fetches and the background price sweep all hit Scryfall and
// can run concurrently, so every request goes through one serialized queue —
// a global limiter beats per-caller delays that can't see each other.
//
// Scryfall publishes HARD, PER-ENDPOINT limits, and the card endpoints this app
// leans on are the strict ones — not the 10/second that applies to everything
// else. From https://scryfall.com/docs/api/rate-limits:
//   /cards/search, /cards/named, /cards/random, /cards/collection — 2/second
//   /cards/manifest — 10/minute
//   all other methods — 10/second
// A single 120ms gap was ~4x over the limit on exactly the endpoints search and
// the price sweep use, which is what earned the 429s.
// SCRYFALL_GAP_SCALE exists so the e2e suite, which stubs the HTTP layer
// entirely, isn't paced against a real API it never contacts. Never set it
// below 1 against api.scryfall.com — exceeding these limits risks a ban.
const GAP_SCALE = Number.isFinite(Number(process.env.SCRYFALL_GAP_SCALE))
  ? Number(process.env.SCRYFALL_GAP_SCALE)
  : 1;
const ENDPOINT_GAPS = [
  [/^\/cards\/(search|named|random|collection)\b/, 500 * GAP_SCALE],
  [/^\/cards\/manifest\b/, 10000 * GAP_SCALE],
];
const SCRYFALL_MIN_GAP_MS = 100 * GAP_SCALE; // floor for "all other methods" (10/second)
// A 429 says "everything you are sending is too much", so backing off only the
// request that got it is useless — the queue behind it keeps firing at full rate
// and keeps the penalty alive. `cooldownUntil` pauses EVERY request until the
// window Scryfall asked for has passed. Default 60s: that is what its 429 body
// asks for when no Retry-After header is sent.
const SCRYFALL_DEFAULT_COOLDOWN_MS = 60000;
let scryfallQueue = Promise.resolve();
let lastScryfallAt = 0;
let cooldownUntil = 0;
// Per-endpoint clocks. The limits are per endpoint, so a search and a /sets call
// don't have to wait on each other beyond the global 10/second floor.
const lastByEndpoint = new Map();

// Which bucket a URL falls in. Callers pass both relative ('/cards/search?...')
// and absolute (Scryfall's own next_page links) URLs, so read just the path.
function endpointGap(url) {
  let path = String(url || '');
  if (/^https?:\/\//i.test(path)) {
    try { path = new URL(path).pathname; } catch { /* fall through to raw */ }
  }
  path = path.split('?')[0];
  for (const [pattern, gap] of ENDPOINT_GAPS) {
    if (pattern.test(path)) return { key: pattern.source, gap };
  }
  return { key: 'default', gap: SCRYFALL_MIN_GAP_MS };
}

// How long this request must wait: its own endpoint's gap, the global floor,
// and any active 429 cooldown — whichever is longest.
function waitFor(url) {
  const now = Date.now();
  const { key, gap } = endpointGap(url);
  return {
    key,
    ms: Math.max(
      cooldownUntil - now,
      gap - (now - (lastByEndpoint.get(key) || 0)),
      SCRYFALL_MIN_GAP_MS - (now - lastScryfallAt),
      0
    )
  };
}

function noteRateLimit(error) {
  if (!error.response || error.response.status !== 429) return false;
  const ra = parseInt(error.response.headers?.['retry-after'], 10);
  const waitMs = Number.isFinite(ra) ? ra * 1000 : SCRYFALL_DEFAULT_COOLDOWN_MS;
  const until = Date.now() + waitMs;
  if (until > cooldownUntil) {
    cooldownUntil = until;
    console.warn(`Scryfall rate-limited us — pausing all Scryfall traffic for ${Math.round(waitMs / 1000)}s.`);
  }
  return true;
}

function scryGet(url, config) {
  const run = scryfallQueue.then(async () => {
    // Re-check after waiting: a 429 may have armed the cooldown while queued.
    for (let w = waitFor(url); w.ms > 0; w = waitFor(url)) {
      await new Promise(r => setTimeout(r, w.ms));
    }
    const { key } = endpointGap(url);
    lastScryfallAt = Date.now();
    lastByEndpoint.set(key, lastScryfallAt);
    try {
      return await client.get(url, config);
    } catch (error) {
      noteRateLimit(error);
      throw error;
    }
  });
  // Keep the chain alive regardless of this request's outcome.
  scryfallQueue = run.then(() => {}, () => {});
  return run;
}

// Scryfall's bulk lookup takes at most 75 identifiers per request.
const COLLECTION_BATCH = 75;

// POST twin of scryGet: same one global queue, same gap, same 429 cooldown, so
// bulk lookups can never race ahead of (or pile on top of) search traffic.
function scryPost(url, body, config) {
  const run = scryfallQueue.then(async () => {
    for (let w = waitFor(url); w.ms > 0; w = waitFor(url)) {
      await new Promise(r => setTimeout(r, w.ms));
    }
    const { key } = endpointGap(url);
    lastScryfallAt = Date.now();
    lastByEndpoint.set(key, lastScryfallAt);
    try {
      return await client.post(url, body, config);
    } catch (error) {
      noteRateLimit(error);
      throw error;
    }
  });
  scryfallQueue = run.then(() => {}, () => {});
  return run;
}

async function scryPostRetried(url, body, config, retries = 4) {
  let lastError;
  for (let i = 0; i < retries; i++) {
    try {
      return await scryPost(url, body, config);
    } catch (error) {
      lastError = error;
      if (error.response && error.response.status === 429 && i < retries - 1) continue;
      throw error;
    }
  }
  throw lastError;
}

// Queue + 429 retry, returning the raw axios response (callers that need
// has_more/next_page/total_cards can't use fetchFromScryfall, which strips to
// .data.data). The wait itself is handled by the shared cooldown above, so a
// retry here just re-queues behind it.
async function scryGetRetried(url, config, retries = 4) {
  let lastError;
  for (let i = 0; i < retries; i++) {
    try {
      return await scryGet(url, config);
    } catch (error) {
      lastError = error;
      if (error.response && error.response.status === 429 && i < retries - 1) continue;
      throw error;
    }
  }
  throw lastError;
}

const COLOR_NAMES = { W: 'White', U: 'Blue', B: 'Black', R: 'Red', G: 'Green' };
const CACHE_AGE_LIMIT_MS = 1000 * 60 * 60 * 24 * 3; // 3 days

// Scryfall has NO `lang` query parameter. Language is a search keyword, and
// non-English printings stay hidden unless include_multilingual is set —
// verified: `q=!"Lightning Bolt" unique:prints&lang=ja` returns 64 English
// prints, while `q=!"Lightning Bolt" lang:ja unique:prints` with
// include_multilingual=true returns the 18 Japanese ones. Passing lang as a
// parameter (what this file did before) is silently ignored, so every "foreign"
// search quietly came back in English.
// English adds nothing to the query: that is already Scryfall's default, and
// staying on the exact old query string keeps the English path byte-identical.
function langSearch(q, lang) {
  const code = languages.resolve(lang).scryfall;
  if (code === 'en') return { q, params: '' };
  return { q: `${q} lang:${code}`, params: '&include_multilingual=true' };
}

// Maps a raw Scryfall card onto the card_cache shape the rest of the app
// already speaks. Double-faced cards carry their art/type on
// card_faces[0] instead of the top level, so fall back to the front face.
function normalizeCard(raw, lang) {
  const face = (!raw.image_uris && Array.isArray(raw.card_faces) && raw.card_faces.length)
    ? raw.card_faces[0]
    : raw;
  // The response's own `lang` is authoritative — a printing only exists in one
  // language, and trusting the requested one mislabels the English fallbacks
  // Scryfall returns when a card was never printed in the language asked for.
  const language = languages.toName(raw.lang || lang);
  const imgSrc = raw.image_uris || face.image_uris || {};
  const typeLine = raw.type_line || face.type_line || '';
  const colors = raw.colors || face.colors || [];
  // USD first, then EUR — and never a mix of the two in one row.
  //
  // Scryfall quotes `usd` from TCGplayer and `eur` from Cardmarket, and which one it
  // has depends on where the printing is actually sold. TCGplayer lists English
  // Magic almost completely (96,090 of 103,656 English rows here carry a usd price)
  // and non-English barely at all, which is why reading usd alone left whole
  // languages at $0.00: Spanish 241 of 1,205 priced, Italian 53 of 194, Simplified
  // Chinese 11 of 61. Those are European and Asian printings sold on Cardmarket,
  // where a eur price usually does exist.
  //
  // The currency is recorded per row (price_currency) rather than converted, because
  // an exchange rate is a live number this app has no source for, and a stale
  // hardcoded one silently misprices a collection. Falling back per row also keeps
  // every column in a row in ONE currency — a normal price in USD next to a foil
  // price in EUR would make the pair meaningless.
  const prices = raw.prices || {};
  const money = (v) => (v != null ? parseFloat(v) : null);
  const eurOnly = money(prices.usd) == null && money(prices.usd_foil) == null
    && money(prices.usd_etched) == null
    && (money(prices.eur) != null || money(prices.eur_foil) != null);
  const currency = eurOnly ? 'EUR' : 'USD';
  const usd = eurOnly ? money(prices.eur) : money(prices.usd);
  const usdFoil = eurOnly ? money(prices.eur_foil) : money(prices.usd_foil);
  // Scryfall has no EUR etched field. An etched quote therefore always selects
  // the USD row instead of being mixed into a EUR-normal/EUR-foil row.
  const usdEtched = eurOnly ? null : money(prices.usd_etched);
  const cmc = raw.cmc != null ? parseFloat(raw.cmc) : null;
  const colorIdentity = raw.color_identity || face.color_identity || [];

  return {
    id: `mtg-${raw.id}`,
    // Stable game-card identity shared by every printing. Oracle Tags assign to
    // this UUID, so preserving it on every fetch makes future collection tag
    // searches fully local.
    oracle_id: oracleIdForCard(raw),
    scryfall_search_eligible: defaultSearchEligible(raw),
    name: face.name || raw.name || '',
    // `supertype` tags these as Magic cards for UI that keys off it.
    supertype: 'MTG',
    subtypes: typeLine.split(/[^A-Za-z]+/).filter(Boolean),
    types: colors.map(c => COLOR_NAMES[c] || c),
    rarity: raw.rarity ? raw.rarity.charAt(0).toUpperCase() + raw.rarity.slice(1) : 'Common',
    set_id: raw.set || '',
    set_name: raw.set_name || '',
    number: raw.collector_number || '',
    image_url: imgSrc.normal || imgSrc.large || imgSrc.small || '',
    price_trend: usd != null ? usd : (usdFoil != null ? usdFoil : (usdEtched != null ? usdEtched : 0)),
    price_normal: usd,
    price_holofoil: usdFoil,
    price_etched: usdEtched,
    cmc: cmc,
    color_identity: colorIdentity.map(c => COLOR_NAMES[c] || c),
    // Which printing this row IS. The quick-add form defaults the copy's language
    // to it, so adding a Japanese card no longer files it as English.
    language,
    // The name as actually printed on a non-English card ("稲妻"). `name` above
    // stays English on purpose: it is what deck lists, marketplace links and the
    // next Scryfall lookup need. Null for English printings, which have none.
    printed_name: raw.printed_name || face.printed_name || null,
    // Scryfall's own marketplace links for THIS printing. Worth storing rather
    // than rebuilding: they search the English name, which is what TCGplayer and
    // Cardmarket actually index, so they resolve for a Japanese printing too.
    tcgplayer_url: raw.purchase_uris?.tcgplayer || null,
    cardmarket_url: raw.purchase_uris?.cardmarket || null,
    // The TCGplayer product id, which `purchase_uris.tcgplayer` only sometimes
    // contains: when Scryfall has no product for a printing it wraps a name
    // SEARCH in the same affiliate link, indistinguishable from the outside
    // without parsing the URL. The id says so plainly — a card either has one or
    // is not listed on TCGplayer.
    //
    // `tcgplayer_etched_id` is the fallback because an etched-only printing (some
    // Commander foils) carries no plain id, and etched is still the right product
    // to link at: it is the one TCGplayer actually sells for that printing.
    tcgplayer_product_id: raw.tcgplayer_id ?? raw.tcgplayer_etched_id ?? null,
    // Scryfall's `prices.usd` is TCGplayer's number and `prices.eur` Cardmarket's,
    // so the currency names the marketplace too (see the fallback above).
    price_source: 'scryfall',
    price_currency: currency,
    ...oracleFields(raw)
  };
}

// Rules text for the card inspector. Multi-face cards (transform/MDFC, split,
// adventure, flip) carry oracle_text per face and none at the top level, so
// keep each face's text alongside a joined fallback string.
function oracleFields(raw) {
  const faces = Array.isArray(raw.card_faces) && raw.card_faces.length > 1
    ? raw.card_faces.map(f => ({
      name: f.name || '',
      mana_cost: f.mana_cost || '',
      type_line: f.type_line || '',
      oracle_text: f.oracle_text || ''
    }))
    : null;
  const text = raw.oracle_text != null
    ? String(raw.oracle_text)
    : (faces ? faces.map(f => f.oracle_text).filter(Boolean).join('\n\n//\n\n') : '');
  return { oracle_text: text, oracle_faces: faces };
}

// The rules text for one cached card, fetched from Scryfall (and cached) the
// first time it is asked for when the row predates the oracle columns.
async function getOracleText(cardId) {
  const row = await db.get(`SELECT id, oracle_text, oracle_faces FROM card_cache WHERE id = ?`, [cardId]);
  const shape = (r) => {
    let faces = null;
    try { faces = r.oracle_faces ? JSON.parse(r.oracle_faces) : null; } catch { faces = null; }
    return { oracle_text: r.oracle_text || '', faces };
  };
  if (row && row.oracle_text != null) return shape(row);
  if (!String(cardId).startsWith('mtg-')) return row ? { oracle_text: '', faces: null } : null;
  let resp;
  try {
    resp = await scryGetRetried(`/cards/${encodeURIComponent(String(cardId).slice(4))}`);
  } catch (err) {
    if (err.response && err.response.status === 404) return null;
    throw err;
  }
  if (!resp || !resp.data) return null;
  const norm = normalizeCard(resp.data);
  // Only the rules-text columns: a full re-cache here would also overwrite the
  // row's prices/art outside the normal refresh path.
  if (row) {
    await db.run(`UPDATE card_cache SET oracle_text = ?, oracle_faces = ? WHERE id = ?`,
      [norm.oracle_text, norm.oracle_faces ? JSON.stringify(norm.oracle_faces) : null, cardId]);
  }
  return { oracle_text: norm.oracle_text || '', faces: norm.oracle_faces };
}

const cacheCards = (cards) => cacheNormalizedCards(cards);


// Look up many known cards in as few requests as possible. Rows are matched by
// Scryfall id when we hold one, else set_id + number, else name. Returns
// normalized cards plus, for each, the row it came from, so callers can write
// back against their own ids without trusting the response to preserve order.
//
// The id form matters for more than precision: /cards/collection identifiers
// have no language field, so a {set, collector_number} lookup always answers
// with the ENGLISH printing. Every row here came from card_cache, whose id IS
// that printing's own Scryfall id — including for a Japanese card — so asking by
// id is the only way a non-English row gets its own prices refreshed instead of
// silently re-fetching the English one.
const scryfallUuid = (id) => {
  const raw = String(id || '').replace(/^mtg-/, '');
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(raw) ? raw : null;
};

async function bulkFetchByIdentifier(rows) {
  const cards = [];
  const pairs = [];
  let notFound = 0;

  for (let i = 0; i < rows.length; i += COLLECTION_BATCH) {
    const chunk = rows.slice(i, i + COLLECTION_BATCH);
    const byKey = new Map();
    const identifiers = chunk.map(row => {
      const uuid = scryfallUuid(row.id || row.card_id);
      if (uuid) {
        byKey.set(`id:${uuid.toLowerCase()}`, row);
        return { id: uuid };
      }
      const setId = row.set_id != null ? String(row.set_id).toLowerCase() : '';
      const num = row.number != null ? String(row.number) : '';
      if (setId && num) {
        byKey.set(`sn:${setId}|${num.toLowerCase()}`, row);
        return { set: setId, collector_number: num };
      }
      byKey.set(`n:${String(row.name || '').toLowerCase()}`, row);
      return { name: row.name || '' };
    });

    const resp = await scryPostRetried('/cards/collection', { identifiers });
    notFound += ((resp.data && resp.data.not_found) || []).length;
    for (const raw of (resp.data && resp.data.data) || []) {
      const norm = normalizeCard(raw);
      cards.push(norm);
      const row = byKey.get(`id:${String(raw.id).toLowerCase()}`)
        || byKey.get(`sn:${String(norm.set_id).toLowerCase()}|${String(norm.number).toLowerCase()}`)
        || byKey.get(`n:${String(norm.name).toLowerCase()}`);
      if (row) pairs.push({ row, card: norm });
    }
  }
  return { cards, pairs, notFound };
}

async function fetchFromScryfall(q, lang, retries = 3) {
  const scoped = langSearch(q, lang);
  const url = `/cards/search?q=${encodeURIComponent(scoped.q)}${scoped.params}`;

  for (let i = 0; i < retries; i++) {
    try {
      const resp = await scryGet(url);
      return (resp.data && resp.data.data) || [];
    } catch (error) {
      // The shared cooldown already holds the whole queue for as long as
      // Scryfall asked, so a retry here just re-queues behind it.
      if (error.response && error.response.status === 429 && i < retries - 1) continue;
      throw error;
    }
  }
}

// Scryfall pages are a fixed 175 cards. Pull the caller's [offset, offset+limit)
// window out of them so search can page by its own limit instead of being capped
// at one Scryfall page. Returns the raw cards plus whether more exist after them.
const SCRY_PAGE_SIZE = 175;

// One upstream /cards/search page, cached for a short TTL. A raw query (any
// operator, any language) walks several of these per request at 2/sec; caching
// each (query, page) pair means a REPEAT of the same search — the far more
// common case, since the UI re-runs a query on every render and "load more"
// re-requests page 1 — costs zero rate-limit budget and returns instantly.
// This generalizes the collection-scope match-list cache (fetchCollectionNames)
// to the whole raw path, internet scope included.
//
// Keyed on the language-scoped query (langSearch already folds in lang:xx and
// include_multilingual) so an English and a Japanese run of the same operators
// never share a page. Bounded so a session of varied queries cannot grow it
// without bound; failures are NOT cached, so a retry re-fetches.
const RAW_PAGE_TTL_MS = 5 * 60 * 1000;
const rawPageCache = new Map(); // key -> { expires, value }
async function scryPage(q, lang, page, order) {
  const scoped = langSearch(q, lang);
  let url = `/cards/search?q=${encodeURIComponent(scoped.q)}&page=${page}${scoped.params}`;
  if (order) url += `&order=${order}`;
  const key = `${scoped.q}\u0000${lang || ''}\u0000${page}\u0000${order || ''}`;
  const now = Date.now();
  const hit = rawPageCache.get(key);
  if (hit && hit.expires > now) return hit.value;
  const resp = await scryGetRetried(url);
  const value = {
    data: (resp.data && resp.data.data) || [],
    total: resp.data && resp.data.total_cards != null ? resp.data.total_cards : null,
    hasMore: !!(resp.data && resp.data.has_more),
  };
  rawPageCache.set(key, { expires: Date.now() + RAW_PAGE_TTL_MS, value });
  // Reap expired pages opportunistically as new ones land, so the map tracks
  // recent use rather than every query ever typed this process.
  if (rawPageCache.size > 400) {
    for (const [k, v] of rawPageCache) if (v.expires <= Date.now()) rawPageCache.delete(k);
  }
  return value;
}

async function fetchWindow(q, lang, offset, limit, order) {
  let page = Math.floor(offset / SCRY_PAGE_SIZE) + 1;
  let skip = offset % SCRY_PAGE_SIZE;
  const out = [];
  let hasMore = false;
  let total = null;
  while (out.length < limit) {
    const p = await scryPage(q, lang, page, order);
    if (p.total != null) total = p.total;
    out.push(...(p.data.slice(skip)));
    skip = 0;
    hasMore = p.hasMore;
    if (!hasMore) break;
    page++;
  }
  return { cards: out.slice(0, limit), hasMore: hasMore || out.length > limit, total };
}

// Public entry point. Returns { cards, total } — `total` is how many matches
// exist upstream in all (null when the answer came from cache, which has no
// such count). Wrapping keeps the many early returns in the body unchanged.
async function searchCards({
  name = '', number = '', set = '', q = '', scope = 'database', userId = null,
  lang = null, allPrints = false, page = 1, limit = 60,
} = {}) {
  const meta = { total: null };
  const cards = await runSearch(meta, name, number, set, q, scope, userId, lang, allPrints, page, limit);
  return { cards, total: meta.total, source: meta.source || null };
}

// Search MTG cards: local card_cache first, then Scryfall.
// `page` is 1-based over `limit`-sized pages; the caller keeps asking for the
// next page while a full page comes back.
async function runSearch(meta, nameQuery = '', numberQuery = '', setQuery = '', rawQuery = '', scope = 'database', userId = null, lang = null, allPrints = false, page = 1, limit = 60) {
  const offset = (page - 1) * limit;
  const cleanName = (nameQuery || '').trim();
  const cleanNumber = (numberQuery || '').trim().replace(/^#/, '').split('/')[0].trim();
  const cleanRaw = String(rawQuery || '').trim();

  // Scryfall-syntax searches ("is:land color:g rarity:rare") are
  // authoritative: they replace the name/number/set fields entirely.
  //
  // They are answered from the local card_cache when they can be — the moment
  // every operator is data-backed (is:, color:, set:, rarity:, ...), the rows
  // this install already holds ARE the answer, and a database query is instant
  // and never touches the 2/sec rate limit. That is the common case: someone
  // typing "set:lea color:g rarity:rare" has a fully cached set they are adding
  // from. Only queries that reach past the cache (otag:, availability:, artist:,
  // t:, ... — catalog operators) fall through to the live API.
  //
  // Collection scope answers "what do I own" with plain field filters, so a raw
  // query has no meaning there and yields nothing rather than a LIKE on the
  // whole operator string.
  if (cleanRaw) {
    if (scope === 'collection') return [];
    const { analyze, toScryfall, QuerySyntaxError } = require('../../shared/scryfallQuery.js');
    let parsed;
    try {
      parsed = analyze(cleanRaw);
    } catch (err) {
      // A genuine syntax error (unbalanced paren, unknown keyword, bad color) is a
      // fixable query, not a down API. Name it exactly like the upstream 400.
      if (err instanceof QuerySyntaxError) throw new Error('INVALID_QUERY');
      throw err;
    }
    const langName = languages.toName(lang);
    // Answer from the local card_cache. Used first for `database` scope when every
    // term is data-backed, and as the fallback when Scryfall is unreachable.
    const answerLocally = async () => {
      const rawSql = require('./utils/rawQuerySql');
      const { sql, params, countSql, countParams } = rawSql.compileRawQuery({
        ast: parsed.ast, language: langName, limit, offset,
      });
      const rows = await db.all(sql, params);
      const count = await db.get(countSql, countParams);
      meta.total = count ? count.n : null;
      meta.source = 'cache';
      const stale = rows.filter(r => (Date.now() - new Date(r.last_updated).getTime()) > CACHE_AGE_LIMIT_MS);
      if (stale.length > 0) {
        (async () => {
          try {
            const { cards: fresh } = await bulkFetchByIdentifier(stale);
            if (fresh.length) await cacheCards(fresh);
          } catch (e) {
            console.error('MTG raw-query background refresh failed:', e.message);
          }
        })();
      }
      return rows.map(parseCardRow);
    };
    // `database` scope + data-backed terms + a language the cache holds -> local.
    // `internet` scope (Add Cards) is authoritative: the local cache holds only
    // the printings this install has seen, so t:elf c:g answered locally would
    // silently drop most elves. Internet scope always asks Scryfall.
    if (parsed.mode === 'local' && scope !== 'internet') {
      const cachedLang = await db.get(
        `SELECT COUNT(*) AS n FROM card_cache WHERE language = ?`, [langName]
      );
      if (cachedLang && cachedLang.n > 0) return answerLocally();
    }
    // Upstream gets the query re-serialized from the AST: Scrybox extensions
    // (is:land -> t:land) become real Scryfall syntax; everything else is verbatim.
    const upstream = toScryfall(parsed.ast);
    try {
      const rawWithLanguageScope = languages.resolve(lang).scryfall === 'en'
        ? upstream
        : `(${upstream})`;
      const { cards: hit, total } = await fetchWindow(rawWithLanguageScope, lang, offset, limit);
      if (total != null) meta.total = total;
      meta.source = 'scryfall';
      const cards = hit.map(c => normalizeCard(c, lang));
      if (cards.length) await cacheCards(cards);
      return cards;
    } catch (err) {
      // 404 = the query matched nothing — that is an answer, not a failure.
      // 422 = a page past the end of the results — also an answer.
      if (err.response && (err.response.status === 404 || err.response.status === 422)) return [];
      if (err.response && err.response.status === 400) throw new Error('INVALID_QUERY');
      console.error('Scryfall raw query failed:', err.message);
      // Upstream down: a data-backed query can still be answered from the cache.
      if (parsed.mode === 'local') return answerLocally();
      if (err.response && err.response.status === 429) throw new Error('RATE_LIMIT_EXCEEDED');
      throw new Error('UPSTREAM_UNAVAILABLE');
    }
  }
  // Set field may list several sets ("ltr, ltc") — match any of them. Scryfall
  // uses `(set:ltr or set:ltc)`; a single set stays the plain `set:ltr` form.
  const setList = parseSetList(setQuery);
  const scrySet = setList.length === 1 ? `set:${setList[0]}` : `(${setList.map(s => `set:${s}`).join(' or ')})`;

  // Scanner path: identify-by-image knows the card but not the printing, so it
  // asks for every printing of an exact name (Scryfall collapses to one printing
  // by default — `unique:prints` returns them all) and lets the user pick the set.
  if (allPrints && cleanName && scope !== 'collection') {
    try {
      // A set code narrows to that printing (exact, usually one result -> fast
      // path in the scanner); without it, return every printing to pick from.
      // -is:digital: MTGO/Arena printings are not cards anyone can scan or own.
      const q = setList.length ? `!"${cleanName}" ${scrySet} unique:prints -is:digital` : `!"${cleanName}" unique:prints -is:digital`;
      // Use the normal paging helper rather than reading only Scryfall's first
      // 175-card page. The caller chooses a bounded window (the scanner asks for
      // 250) so heavily reprinted cards do not silently lose later printings.
      const { cards: raw, total } = await fetchWindow(q, lang, offset, limit);
      if (total != null) meta.total = total;
      meta.source = 'scryfall';
      if (raw.length) {
        const cards = raw.map(c => normalizeCard(c, lang));
        await cacheCards(cards);
        return cards;
      }
    } catch (e) {
      // No exact-name match / error — fall through to the normal search below.
    }
  }
  // Every cache read below is scoped to the requested language, so a cached
  // English printing can't shadow the localized card that was asked for — and,
  // unlike bypassing the cache outright, a repeat Japanese search still gets to
  // answer locally.
  const langName = languages.toName(lang);

  // 1. Collection-only search. What the user owns, in every language they own it
  // in — filtering by the picker's language here would hide their Japanese copies
  // from a deck search. See utils/cardSearchSql.
  if (scope === 'collection') {
    if (!userId) return [];
    const { sql, params } = cardSearchSql.collectionQuery({
      userId, name: cleanName, number: cleanNumber, setList, limit, offset,
    });
    return (await db.all(sql, params)).map(parseCardRow);
  }

  // 2. Local cache first. Kept as a closure because an internet-scope search
  // skips it here but still needs it as a fallback when Scryfall is unreachable.
  const queryLocal = async () => {
    // language is part of the identity of a cached printing, so a Japanese search
    // must not be answered with the English rows sitting next to it.
    const { sql, params } = cardSearchSql.localCacheQuery({
      language: langName, name: cleanName, number: cleanNumber, setList, limit, offset,
    });
    return db.all(sql, params);
  };

  let localResults = [];
  if (scope !== 'internet') {
    localResults = await queryLocal();
    if (localResults.length > 0) {
      // Refresh stale prices in the background; return the cached rows instantly.
      const stale = localResults.filter(r => (Date.now() - new Date(r.last_updated).getTime()) > CACHE_AGE_LIMIT_MS);
      if (stale.length > 0) {
        // Batched: a page is now up to 250 rows, and one request per stale row
        // was a 250-call burst behind a single search.
        (async () => {
          try {
            const { cards: fresh } = await bulkFetchByIdentifier(stale);
            if (fresh.length) await cacheCards(fresh);
          } catch (e) {
            console.error('MTG background refresh failed:', e.message);
          }
        })();
      }
      meta.source = 'cache';
      return localResults.map(parseCardRow);
    }
  }

  // Strip leading zeros from collector numbers — input may arrive as "0488" but
  // Scryfall expects "488".
  const strippedNumber = cleanNumber.replace(/^0+/, '') || cleanNumber;

  // Run specific query (set+cn or name+cn) AND the broad name-only query, then
  // merge results: exact matches first, remaining alternatives sorted by cn.
  // This way the user always sees the likely match at top with other printings below.
  // Scryfall collapses printings to one card per name by default, so a plain
  // "Sol Ring" only ever returned a single arbitrary printing. Manual add needs
  // every printing to pick the one actually being added. Digital-only prints
  // (Alchemy rebalances) are dropped — there is no physical card to own, same
  // rule the scan index uses.
  const PRINTS = ' unique:prints -is:digital';
  const specificQuery = (setList.length && strippedNumber) ? `${scrySet} cn:${strippedNumber}`
    : (cleanName && strippedNumber) ? `${cleanName} cn:${strippedNumber}`
    : null;
  // Constrain the name search to the chosen set(s) so a multi-set search
  // ("ltr, ltc") returns only those sets, not every printing. Set-only (no
  // name) falls back to browsing the set(s).
  const setConstraint = setList.length ? ` ${scrySet}` : '';
  const broadQuery = cleanName ? `${cleanName}${setConstraint}${PRINTS}` : (setList.length ? `${scrySet}${PRINTS}` : null);
  // Last resort: first word only (e.g. "Adamant" from "Adamant Will")
  const firstWord = cleanName.split(/\s+/)[0];
  const fallbackQuery = (firstWord && firstWord !== cleanName) ? `${firstWord}${setConstraint}${PRINTS}` : null;

  // Helper: try a Scryfall query window, return [] on 404/error.
  // Browsing a whole set pages by collector number; a name search keeps
  // Scryfall's relevance order so the card you typed stays on page 1.
  const order = (!cleanName && setList.length) ? 'set' : undefined;
  const tryQuery = async (q, off = 0) => {
    if (!q) return [];
    try {
      const { cards, total } = await fetchWindow(q, lang, off, limit, order);
      // The broad query is the one that defines "how many matches exist"; the
      // specific set+cn probe would report its own tiny count.
      if (q === broadQuery && total != null) meta.total = total;
      return cards.map(c => normalizeCard(c, lang));
    } catch (err) {
      // 404 = no cards matched; 422 = asked for a page past the last one.
      if (err.response && (err.response.status === 404 || err.response.status === 422)) return [];
      throw err; // real error (rate limit, network) — bubble up
    }
  };

  try {
    // The specific (set+cn) query yields at most a printing or two and only
    // makes sense as the head of the first page — later pages just walk the
    // broad query. Overlap from that shift is deduped by the caller on id.
    let exact = page === 1 ? await tryQuery(specificQuery) : [];

    // A set + collector number identifies ONE card. Pairing it with the broad
    // set browse would bury that card under the other 800 in the set, so the
    // browse only runs as a fallback when the number found nothing. With a name
    // typed, the broad query is still wanted — it surfaces other printings.
    const numberPinnedIt = !cleanName && strippedNumber && exact.length > 0;
    let broad = (!numberPinnedIt && broadQuery && broadQuery !== specificQuery)
      ? await tryQuery(broadQuery, offset)
      : [];
    if (numberPinnedIt) meta.total = exact.length;

    // If both empty, try first-word fallback.
    if (exact.length === 0 && broad.length === 0 && fallbackQuery) {
      broad = await tryQuery(fallbackQuery, offset);
    }

    // Merge: exact matches first, then broad alternatives deduped.
    const seen = new Set(exact.map(c => c.id));
    const merged = [...exact, ...broad.filter(c => !seen.has(c.id))];
    if (merged.length === 0) {
      meta.source = 'cache';
      return localResults.map(parseCardRow);
    }

    const cards = merged.slice(0, limit);
    // Sort alternatives (after exact) by collector number. A set browse has no
    // exact match to hoist and is already in set order — re-sorting it per page
    // would only shuffle non-numeric collector numbers to the top of each page.
    const exactIds = new Set(exact.map(c => c.id));
    if (cleanName || strippedNumber) cards.sort((a, b) => {
      // Exact matches always first.
      const aExact = exactIds.has(a.id) ? 0 : 1;
      const bExact = exactIds.has(b.id) ? 0 : 1;
      if (aExact !== bExact) return aExact - bExact;
      const na = parseInt(a.number, 10) || 0;
      const nb = parseInt(b.number, 10) || 0;
      return na - nb;
    });

    await cacheCards(cards);
    meta.source = 'scryfall';
    return cards;
  } catch (err) {
    console.error('Scryfall search failed:', err.message);
    // Serve whatever the cache already knows before giving up. With nothing
    // cached, say the upstream is down rather than "no such card" — a throttled
    // or broken Scryfall is indistinguishable from an empty result otherwise,
    // and reporting it as "no results" is what made #22 look like a search bug.
    const cached = scope === 'internet' ? await queryLocal() : localResults;
    if (cached.length > 0) {
      console.warn(`Scryfall unavailable — serving ${cached.length} cached match(es).`);
      meta.source = 'cache';
      return cached.map(parseCardRow);
    }
    const status = err.response && err.response.status;
    if (status === 429) throw new Error('RATE_LIMIT_EXCEEDED');
    throw new Error('UPSTREAM_UNAVAILABLE');
  }
}

// Fetch a set's cards from Scryfall (dev seed helper): one request, normalized
// and cached like any lookup, so
// the seed route gets a varied MTG pool (all colors/rarities). Takes the first
// page (~175 cards) — plenty for test data, so pagination is skipped.
async function getCardsBySet(setCode) {
  try {
    console.log(`Querying Scryfall for full set: ${setCode}`);
    const raw = await fetchFromScryfall(`set:${setCode}`);
    const cards = raw.map(c => normalizeCard(c));
    if (cards.length > 0) await cacheCards(cards);
    return cards;
  } catch (error) {
    console.error(`Error fetching MTG set ${setCode} from Scryfall:`, error.message);
    return [];
  }
}

// Fetch MTG sets from Scryfall and cache them in the shared `sets` table.
// Set ids are prefixed "mtg-" so a Scryfall set code is unambiguous on the
// primary key. Skips if already populated unless force=true.
async function fetchAndCacheSets(force = false) {
  try {
    const existing = await db.get(`SELECT COUNT(*) as count FROM sets`);
    if (!force && existing && existing.count > 0) {
      console.log(`MTG sets already populated (${existing.count} sets). Skipping fetch.`);
      return;
    }
    console.log('Fetching sets from Scryfall...');
    const resp = await scryGet('/sets');
    const sets = (resp.data && resp.data.data) || [];
    for (const s of sets) {
      await db.run(
        `INSERT OR REPLACE INTO sets (id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        [
          `mtg-${s.code}`, s.name, s.set_type || '', s.card_count || 0, s.card_count || 0,
          s.released_at || '', s.code || '', s.icon_svg_uri || '', s.icon_svg_uri || ''
        ]
      );
    }
    console.log(`Cached ${sets.length} MTG sets.`);
  } catch (error) {
    console.error('Error fetching MTG sets from Scryfall:', error.message);
  }
}

// Refresh prices for every owned MTG printing plus every cached printing of a
// logical card used by a deck. Deck minimum values compare alternate printings,
// so refreshing only the representative printing stored in deck_cards would
// leave the supposedly cheapest option stale.
// `force` bypasses the once-a-day gate (used by the scheduled daily run, which
// is already on the right cadence by construction).
async function updateCollectionPrices(force = false) {
  try {
    const cards = await db.all(`
      WITH deck_card_keys AS (
        SELECT DISTINCT ${sqlCardKey('deck_cc')} AS card_key
        FROM deck_cards dc
        JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
        WHERE dc.quantity > 0
      )
      SELECT DISTINCT c.card_id, cc.set_id, cc.number, cc.name
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.quantity > 0
      UNION
      SELECT DISTINCT cc.id AS card_id, cc.set_id, cc.number, cc.name
      FROM card_cache cc INDEXED BY idx_card_cache_logical_key
      JOIN deck_card_keys deck_key ON deck_key.card_key = ${sqlCardKey('cc')}
    `);
    if (cards.length === 0) return;
    if (!force && !(await shouldSweepPrices('mtg'))) {
      console.log('Skipping MTG price update: already swept within the last 24h (Scryfall updates prices daily).');
      return;
    }
    console.log(`Starting MTG price update for ${cards.length} unique cards...`);

    // One request PER CARD is what got this app rate-limited: a 200-card
    // collection meant 200 Scryfall calls every boot, and nodemon reboots on
    // every code edit. /cards/collection takes 75 identifiers at a time, so the
    // same sweep is a handful of calls. Verified contract: { data, not_found }.
    try {
      const { cards: fresh, pairs, notFound } = await bulkFetchByIdentifier(cards);
      if (fresh.length) await cacheCards(fresh);
      for (const { row, card } of pairs) {
        await recordPrice(row.card_id, card.price_trend);
      }
      await markPricesSwept('mtg');
      console.log(`MTG price update complete: ${pairs.length} priced, ${notFound} not found on Scryfall.`);
    } catch (e) {
      console.error('MTG price update failed:', e.message);
    }
  } catch (err) {
    console.error('Error during MTG price update:', err.message);
  }
}

// The same printing in another language, or null.
//
// Scanning identifies a card from ARTWORK, which is identical in every language,
// so a Japanese card is matched against the English catalog and comes back as the
// English printing (cvScan.load says so outright, and leaves re-expressing the
// answer to the caller — this is that). Set code and collector number ARE
// language-invariant, so they address the printing; /cards/:code/:number/:lang is
// the one Scryfall endpoint that takes a language, unlike /cards/collection.
//
// Returns null rather than throwing when the card was never printed in that
// language (Japanese has no Alpha), so callers keep the English card they had.
async function getPrintingInLang(setCode, number, lang) {
  const code = languages.resolve(lang).scryfall;
  if (code === 'en' || !setCode || !number) return null;
  const name = languages.toName(code);
  const cached = await db.get(
    `SELECT * FROM card_cache WHERE set_id = ? AND number = ? AND language = ? LIMIT 1`,
    [String(setCode).toLowerCase(), String(number), name]
  );
  if (cached) return parseCardRow(cached);
  try {
    const resp = await scryGet(`/cards/${encodeURIComponent(String(setCode).toLowerCase())}/${encodeURIComponent(number)}/${code}`);
    if (!resp.data) return null;
    const norm = normalizeCard(resp.data, name);
    // Only keep it if Scryfall really answered in that language. A 404 is the
    // usual "not printed in it" signal, but the endpoint can also fall back, and
    // caching an English row under a Japanese query would poison the lookup above.
    if (languages.toCode(norm.language) !== languages.toCode(code)) return null;
    await cacheCards([norm]);
    return norm;
  } catch {
    return null;   // 404 (no such printing in that language) or a transient error
  }
}

async function getCardById(cardId) {
  const rawId = cardId.startsWith('mtg-') ? cardId.slice(4) : cardId;
  const cached = await db.get(`SELECT * FROM card_cache WHERE id = ?`, [cardId]);
  if (cached) return parseCardRow(cached);
  try {
    const resp = await scryGet(`/cards/${rawId}`);
    if (resp.data) {
      const norm = normalizeCard(resp.data);
      await cacheCards([norm]);
      return norm;
    }
  } catch (e) {}
  return null;
}

// A CATALOG query can match thousands of game cards, which means walking
// dozens of rate-limited Scryfall pages. That walk is the expensive part and
// it is a property of the QUERY, not of the user — every owner of the same
// collection gets the same match list. Cache the resolved set of card names
// per (canonical query, lang) so:
//   - re-running the same tag (re-render, remount, typo-retry) is instant;
//   - two users on the same install resolve the tag only ONCE;
//   - a failed upstream call is NOT cached, so the next try re-fetches.
//
// The cache lives in the collection_query_cache table, not just in memory:
// the in-process Map is the fast path, but it evaporates on every container
// restart — which turned every restart into a fresh ~20-30s cold walk of the
// first broad tag someone typed. On disk, a restart serves the walk from the
// database in milliseconds.
//
// The durable entry is STALE-WHILE-REVALIDATE, not hard-TTL:
//   - FRESH  (<= CQC_SWR_MAX_AGE_MS): served from cache, no walk, ever.
//   - STALE  (older than that; indefinitely when complete, up to
//     CQC_STALE_MS when incomplete): served immediately with status 'stale',
//     and exactly one
//     background revalidation (deduped by the in-flight map, paced by the
//     global Scryfall limiter) re-walks it. The user never waits on the
//     14-second cold walk twice for the same tag.
//   - COMPLETE entries remain serve-stale indefinitely. Even a month-old known
//     complete result answers immediately while one background refresh runs.
//   - INCOMPLETE entries older than the stale bound synchronously re-resolve.
// Freshness matters less than it looks: the intersect runs against the LIVE
// local collection on every request, so a user who adds cards sees them
// immediately — only Scryfall's own tag assignments (rarely re-curated) can
// be briefly stale while a background refresh is pending.
//
// Completeness: a walk that hit its page cap (hasMore at the cut) is a
// PARTIAL answer. It is still persisted — the names it DID resolve are real
// and useful — but flagged complete=0 so the UI can show "may be incomplete"
// and revalidation keeps retrying (with the cap raised) until the walk
// finishes cleanly. Persisting a partial walk as complete was the old bug:
// otag:card-advantage (6,183 cards) lost its final 183 cards to the 6000
// cap, and that truncation was cached for 24 hours with no warning.
//
// `limit` caps how far one walk may page (20,000 game cards ≈ 115 Scryfall
// pages ≈ 58s at the 2 req/s ceiling — above the broadest otag: in
// practice; if a walk still hits it, the answer is flagged incomplete, not
// silently truncated).
const CQC_SWR_MAX_AGE_MS = 6 * 60 * 60 * 1000;   // fresh for six hours
const CQC_STALE_MS = 7 * 24 * 60 * 60 * 1000;    // serve-stale up to seven days
const CQC_DEFAULT_WALK_LIMIT = 20000;
const collectionQueryCache = new Map();
const inflightCollectionNames = new Map();

// One cache entry, in memory or on disk:
//   names        Set of lowercased match names (DFC faces + full "A // B")
//   fetched      raw upstream cards from the LAST walk (null on any hit) —
//                used only to backfill card_cache, never to answer
//   complete     false when the last walk hit its page cap
//   total        upstream total_cards (null when unknown)
//   fetchedCount cards actually pulled in the last walk
//   resolvedAt   epoch ms of the last successful walk
function cqcEntry({ names, complete = true, total = null, fetchedCount = 0, resolvedAt = Date.now() }) {
  return { names, complete, total, fetchedCount, resolvedAt };
}

// The query as sent upstream already carries lang:xx for non-English (via
// langSearch inside fetchWindow/scryPage), so the cache key uses the CLEAN
// scoped query plus the canonical language code. Normalizing 'en'/absent to
// one key (and reusing the stored code) keeps 'otag:ramp' and 'otag:ramp'
// (lang=en) from each paying for the walk under a different row.
function cqcScope(scoped, lang) {
  return { scoped, langCode: languages.toCode(lang) };
}

async function fetchCollectionNames(scoped, lang, limit, byPrinting = false) {
  const { scoped: base, langCode } = cqcScope(scoped, lang);
  // The persisted key carries the walk shape so a by-printing id list is never
  // read back as a by-name list (or vice versa).
  const q = byPrinting ? `${base} unique:prints` : base;
  const key = `${q}\u0000${langCode}`;
  const now = Date.now();
  let hit = collectionQueryCache.get(key);
  if (!hit || !hit.resolvedAt) {
    // Disk hit: an earlier run (possibly a previous container) already paid
    // for a walk. A read failure must never break a search — fall through to
    // a fresh walk.
    try {
      const row = await db.get(
        'SELECT names, upstream_total, fetched_count, complete, resolved_at FROM collection_query_cache WHERE query = ? AND lang = ?',
        [q, langCode]
      );
      if (row) {
        hit = cqcEntry({
          names: new Set(JSON.parse(row.names)),
          complete: !!row.complete,
          total: row.upstream_total,
          fetchedCount: row.fetched_count || 0,
          resolvedAt: row.resolved_at,
        });
        collectionQueryCache.set(key, hit);
      }
    } catch (e) {
      console.error('collection_query_cache read failed:', e.message);
    }
  }
  const age = hit ? now - hit.resolvedAt : Infinity;
  if (hit && (hit.complete || age <= CQC_STALE_MS)) {
    // Fresh OR stale-but-usable: answer from the cached names immediately —
    // the caller intersects them against the LIVE collection, so the served
    // list is correct for THIS user regardless of cache age. Stale entries
    // also trigger exactly one background revalidation (deduped by the
    // in-flight map, paced by the global Scryfall limiter); a concurrent
    // request finds that map entry and serves the cache instead of joining.
    if (age > CQC_SWR_MAX_AGE_MS && !inflightCollectionNames.has(key)) {
      startRevalidation(q, lang, langCode, key, limit, byPrinting);
    }
    return {
      entry: hit,
      fetched: null,
      status: age <= CQC_SWR_MAX_AGE_MS ? 'fresh' : 'stale',
    };
  }
  // No entry, or an incomplete entry older than the stale bound: a synchronous
  // walk is required before the request can answer. Concurrent callers share it.
  if (inflightCollectionNames.has(key)) return inflightCollectionNames.get(key);
  const p = (async () => {
    const { entry, fetched } = await runWalk(q, lang, langCode, key, limit, byPrinting);
    return { entry, fetched, status: 'resolved' };
  })();
  inflightCollectionNames.set(key, p);
  p.catch(() => {}).finally(() => inflightCollectionNames.delete(key));
  return p;
}

// The walk itself: page Scryfall by game card, build the name set, update the
// in-memory map, persist. Throws on upstream failure — the caller decides
// (a failed walk must never evict a still-usable cached entry).
async function runWalk(q, lang, langCode, key, limit, byPrinting = false) {
  // Walk by GAME CARD, not by printing: the answer is a set of names, and
  // hundreds of printings per card would otherwise inflate the walk ~4x
  // (otag:ramp is 2,287 cards but 9,231 printings → 13 pages instead of
  // 53 at the 2 req/s ceiling). unique:cards returns the same names —
  // English canonical, front face for DFCs — that the intersect matches on.
  let fetched;
  let hasMore;
  let total;
  try {
    ({ cards: fetched, hasMore, total } = await fetchWindow(byPrinting ? q : `${q} unique:cards`, lang, 0, limit));
  } catch (error) {
    // A syntactically valid query with no matches is a complete, durable empty
    // membership. Caching it prevents every repeat from paying for the same 404.
    if (error.response && error.response.status === 404) {
      fetched = [];
      hasMore = false;
      total = 0;
    } else {
      throw error;
    }
  }
  const names = new Set();
  for (const raw of fetched) {
    if (byPrinting) {
      if (raw.id) names.add(`mtg-${raw.id}`);
      continue;
    }
    const full = String(raw.name || '').trim().toLowerCase();
    if (!full) continue;
    names.add(full);
    // A game card matches if owned under the FULL "A // B" string OR under a
    // face (the cache stores DFCs under the front face only).
    if (full.includes(' // ')) {
      full.split(' // ').forEach(part => {
        const p2 = part.trim();
        if (p2) names.add(p2);
      });
    }
  }
  const entry = cqcEntry({
    names,
    // hasMore=true at the cut means the walk stopped before the last card:
    // the names are a REAL but INCOMPLETE prefix. total==null (an upstream
    // that omits the count) is not a reason to doubt completeness.
    complete: !hasMore,
    total,
    fetchedCount: fetched.length,
  });
  collectionQueryCache.set(key, entry);
  // Entries past the stale bound are dead weight (a Set of names per query) —
  // reap them opportunistically as new ones land so the map tracks recent use.
  for (const [k, v] of collectionQueryCache) {
    const a = v.resolvedAt ? Date.now() - v.resolvedAt : Infinity;
    if (!v.complete && a > CQC_STALE_MS) collectionQueryCache.delete(k);
  }
  // Durability: persist the resolved set (+ completeness) for the next
  // restart and for every stale hit in between. Best-effort — a write
  // failure degrades to the in-memory behavior, never to a broken search.
  // A valid EMPTY answer is persisted too: a nonexistent tag would otherwise
  // pay the upstream 404 walk on every retry.
  try {
    await db.run(
      `INSERT INTO collection_query_cache (query, lang, names, upstream_total, fetched_count, complete, resolved_at, expires_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(query, lang) DO UPDATE SET
         names = excluded.names,
         upstream_total = excluded.upstream_total,
         fetched_count = excluded.fetched_count,
         complete = excluded.complete,
         resolved_at = excluded.resolved_at,
         expires_at = excluded.expires_at`,
      [q, langCode, JSON.stringify([...names]), total, fetched.length,
       entry.complete ? 1 : 0, entry.resolvedAt, Date.now() + CQC_STALE_MS]
    );
  } catch (e) {
    console.error('collection_query_cache write failed:', e.message);
  }
  return { entry, fetched };
}

// One background revalidation for a stale cache entry. Fire-and-forget: the
// request that noticed the staleness already answered from the cache, so a
// failure here is logged and the NEXT request simply re-triggers it. The
// in-flight map makes concurrent requests collapse into this single walk.
function startRevalidation(q, lang, langCode, key, limit, byPrinting = false) {
  const p = (async () => {
    const { entry } = await runWalk(q, lang, langCode, key, limit, byPrinting);
    return { entry, fetched: null, status: 'stale-refreshed' };
  })();
  inflightCollectionNames.set(key, p);
  p.catch(e => console.error('catalog cache revalidation failed:', e.message))
   .finally(() => inflightCollectionNames.delete(key));
}

// Resolve a CATALOG-ONLY Scryfall query ("otag:sneak", "availability:modern",
// any operator the stored rows cannot answer) and intersect the result with
// the user's collection. Returns the user's OWNED rows for the matching game
// cards — same projection as the /api/collection endpoint, so they render in
// the collection screen's tiles unchanged.
//
// Why a separate function instead of searchCards: searchCards pages the
// UPSTREAM result, but "what do I own of these" needs the whole match list to
// intersect, then returns LOCAL rows. The query still reaches Scryfall verbatim
// through the same rate-limited queue and language scoping as every other raw
// search; only the answer's shape differs.
//
// `limit` caps how far one walk may page (20,000 game cards ≈ 115 Scryfall
// pages — above the broadest tag in practice; a walk that still hits it
// returns a flagged-incomplete answer, not a silently truncated one).
//
// Returns { cards, total, complete, upstreamTotal, cacheStatus }:
//   cards         the user's owned rows for the matched game cards
//   total         how many owned rows matched (the X-Total-Count)
//   complete      false when the upstream walk hit its page cap — the names
//                 are a real prefix, but cards past the cut are not in the
//                 match list, so the answer may be missing owned copies
//   upstreamTotal Scryfall's total_cards for the tag (null when unknown)
//   cacheStatus   'fresh' (cache, no walk this request) | 'stale' (cache
//                 served + background refresh in flight) | 'resolved'
//                 (this request paid for the walk)
function catalogMembershipVersion(entry) {
  const names = [...(entry?.names || [])].map(String).sort();
  return crypto.createHash('sha256').update(names.join('\u0000')).digest('hex').slice(0, 20);
}

async function readCatalogSnapshot(client, mode, sourceVersion, expectedSnapshot = null) {
  const row = await client.get('SELECT revision FROM collection_snapshot WHERE id = 1');
  const token = `${mode}:${sourceVersion}:${Number(row?.revision) || 0}`;
  if (expectedSnapshot && expectedSnapshot !== token) {
    const error = new Error('CATALOG_SNAPSHOT_CHANGED');
    error.expectedSnapshot = expectedSnapshot;
    error.actualSnapshot = token;
    throw error;
  }
  return token;
}

async function resolveCollectionQuery({
  q = '', userId = null, lang = null,
  rowLimit = 60, rowOffset = 0, walkLimit = CQC_DEFAULT_WALK_LIMIT,
  includeTotal = true, expectedSnapshot = null,
} = {}) {
  const cleanRaw = String(q || '').trim();
  if (!cleanRaw || !userId) return { cards: [], total: includeTotal ? 0 : null, complete: true, upstreamTotal: null, cacheStatus: 'none', snapshot: null };

  // A catalog-classified query made only of data-backed operators plus otag:
  // becomes a single SQLite SELECT as soon as a complete local tag generation
  // and oracle-id backfill are available. Unsupported operators retain the
  // existing Scryfall/SWR path below.
  const { analyze, toScryfall, stripDirectives, QuerySyntaxError } = require('../../shared/scryfallQuery.js');
  const oracleTags = require('./oracleTags');
  let parsed;
  try {
    parsed = analyze(cleanRaw);
  } catch (error) {
    if (error instanceof QuerySyntaxError) throw new Error('INVALID_QUERY');
    throw error;
  }
  // Every term is data-backed: the owned rows ARE the answer — no API, no tag
  // index, no eligibility gate (the user asked about cards they own).
  if (parsed.mode === 'local') {
    return db.withDedicatedReadTransaction(async (readDb) => {
      const snapshot = await readCatalogSnapshot(readDb, 'rows', 0, expectedSnapshot);
      const rawSql = require('./utils/rawQuerySql');
      const compiled = rawSql.compileCollectionQuery({
        ast: parsed.ast, userId, limit: rowLimit, offset: rowOffset, eligibleOnly: false,
      });
      const rows = await readDb.all(compiled.sql, compiled.params);
      const count = includeTotal ? await readDb.get(compiled.countSql, compiled.countParams) : null;
      return {
        cards: rows.map(row => ({ ...parseCardRow(row), price_trend: resolveCardPrice(row) })),
        total: includeTotal ? (Number(count && count.n) || 0) : null,
        complete: true,
        upstreamTotal: null,
        cacheStatus: 'local',
        snapshot,
      };
    });
  }
  if (oracleTags.supportsLocalCollectionQuery(parsed)) {
    const local = await db.withDedicatedReadTransaction(async (readDb) => {
      if (!await oracleTags.isReady(readDb, userId)) return null;
      const generation = await readDb.get(
        'SELECT id FROM oracle_tag_generations WHERE active = 1 LIMIT 1'
      );
      const snapshot = await readCatalogSnapshot(
        readDb, 'local', Number(generation?.id) || 0, expectedSnapshot,
      );
      const rawSql = require('./utils/rawQuerySql');
      const compiled = rawSql.compileCollectionQuery({
        ast: parsed.ast,
        userId,
        limit: rowLimit,
        offset: rowOffset,
      });
      // The coverage gate, rows, count, and active tag generation are pinned to
      // one WAL snapshot. A collection mutation can no longer slip an unresolved
      // owned row between readiness and a falsely-complete local answer.
      const rows = await readDb.all(compiled.sql, compiled.params);
      const count = includeTotal
        ? await readDb.get(compiled.countSql, compiled.countParams)
        : null;
      return {
        cards: rows.map(row => ({ ...parseCardRow(row), price_trend: resolveCardPrice(row) })),
        total: includeTotal ? (Number(count && count.n) || 0) : null,
        complete: true,
        upstreamTotal: null,
        cacheStatus: 'local',
        snapshot,
      };
    });
    if (local) return local;
  }
  // Printing-level terms (set, artist, art tag, price, frame, ...) must be
  // intersected by exact printing: owning a Bolt from M10 does not make it match
  // a:"christopher rush". Game-card terms (oracle text, formats, otag) keep the
  // cheaper by-name walk.
  const byPrinting = !!parsed.printing;
  const filterAst = stripDirectives(parsed.ast);
  // Directives (unique:, order:) are stripped: the walk picks its own shape.
  const upstream = toScryfall(filterAst);
  const rawWithLanguageScope = languages.resolve(lang).scryfall === 'en'
    ? upstream
    : `(${upstream})`;
  let result;
  try {
    result = await fetchCollectionNames(rawWithLanguageScope, lang, walkLimit, byPrinting);
  } catch (err) {
    // A 404 is a valid empty membership and is already cached by runWalk.
    // 400/422 are fixable query errors; throttling and transport failures remain
    // distinct. A failure never evicts a usable cached entry.
    if (err.response && err.response.status === 404) {
      return db.withDedicatedReadTransaction(async (readDb) => ({
        cards: [],
        total: includeTotal ? 0 : null,
        complete: true,
        upstreamTotal: 0,
        cacheStatus: 'resolved',
        snapshot: await readCatalogSnapshot(readDb, 'remote', 'empty', expectedSnapshot),
      }));
    }
    if (err.response && (err.response.status === 400 || err.response.status === 422)) {
      throw new Error('INVALID_QUERY');
    }
    if (err.response && err.response.status === 429) throw new Error('RATE_LIMIT_EXCEEDED');
    console.error('Scryfall collection query failed:', err.message);
    throw new Error('UPSTREAM_UNAVAILABLE');
  }
  const { entry, fetched, status } = result;
  const names = entry.names;
  const membershipVersion = catalogMembershipVersion(entry);
  if (!names.size) {
    return db.withDedicatedReadTransaction(async (readDb) => ({
      cards: [],
      total: includeTotal ? 0 : null,
      complete: entry.complete,
      upstreamTotal: entry.total,
      cacheStatus: status,
      snapshot: await readCatalogSnapshot(readDb, 'remote', membershipVersion, expectedSnapshot),
    }));
  }

  // Backfill the local card cache from a walk EXACTLY once, WITHOUT blocking
  // the answer, and BOUNDED: the cards this user owns are already cached
  // (their owned rows join card_cache), so an unbounded backfill of thousands
  // of representative printings only adds write load for cards they will
  // never open. A cap keeps incidental warming for small tags without
  // turning a tag search into a bulk-import of the catalog.
  if (fetched && fetched.length) {
    const backfill = fetched.slice(0, 500);
    Promise.resolve()
      .then(() => backfill.map(c => normalizeCard(c, lang)))
      .then(cards => cards.length ? cacheCards(cards) : null)
      .catch(e => console.error('Scryfall collection query caching failed:', e.message));
  }

  const { sql, params, countSql, countParams } = byPrinting
    ? cardSearchSql.ownedByCardIds(userId, [...names], { limit: rowLimit, offset: rowOffset })
    : cardSearchSql.ownedByNames(userId, [...names], { limit: rowLimit, offset: rowOffset });
  return db.withDedicatedReadTransaction(async (readDb) => {
    const snapshot = await readCatalogSnapshot(
      readDb, 'remote', membershipVersion, expectedSnapshot,
    );
    const owned = sql ? await readDb.all(sql, params) : [];
    const count = includeTotal && countSql ? await readDb.get(countSql, countParams) : null;
    return {
      // The /api/collection endpoint's row shape, including the resolved
      // price_trend, so the tiles price identically to a normal collection load.
      cards: owned.map(row => ({ ...parseCardRow(row), price_trend: resolveCardPrice(row) })),
      total: includeTotal ? (Number(count && count.n) || 0) : null,
      complete: entry.complete,
      upstreamTotal: entry.total,
      cacheStatus: status,
      snapshot,
    };
  });
}

// `client` and `fetchWindow` are exported for tests that stub the axios
// adapter. `collectionQueryCache` is the in-process half of the durable
// catalog-query cache — exposed so tests can age an entry to drive the
// stale-while-revalidate paths (production code never reads it directly).
module.exports = { getOracleText, scryPostRetried, searchCards, normalizeCard, cacheCards, getCardsBySet, fetchAndCacheSets, updateCollectionPrices, getCardById, getPrintingInLang, bulkFetchByIdentifier, scryGetRetried, client, fetchWindow, resolveCollectionQuery, collectionQueryCache };
