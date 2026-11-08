// Add Cards search answers from the local scryfall sidecar, which serves cards
// WITHOUT prices. Before a result page leaves the server, fill each price-less
// printing from card_cache (the last real quote), and only for printings that
// have never been priced ask the real Scryfall API through the queued,
// rate-limited bulkFetchByIdentifier({ upstream: true }). Writing those quotes
// back goes through cacheCards, whose upsert never lets a price-less row
// overwrite a real quote.
const PRICE_FIELDS = ['price_trend', 'price_normal', 'price_holofoil', 'price_etched', 'price_currency', 'price_source', 'price_estimated'];
const UPSTREAM_CAP = 150;
// A cached row can carry price_source but no number at all: it was quoted
// before the market had one (a set fetched before release, e.g. Mystery
// Booster Commander). That is "never priced", not a real quote, so it must
// not block the upstream fetch forever. Printings that are still price-less
// upstream are remembered for EMPTY_RETRY_MS so a search does not re-ask
// Scryfall for them on every page view.
const EMPTY_RETRY_MS = 6 * 60 * 60 * 1000;
const emptyCheckedAt = new Map();

// An estimate (cheapest sibling's quote) is not this printing's quote.
const hasQuote = (r) => !Number(r.price_estimated) && ([r.price_normal, r.price_holofoil, r.price_etched]
  .some(v => v != null && Number(v) > 0) || Number(r.price_trend) > 0);

function copyPrices(target, src) {
  for (const f of PRICE_FIELDS) if (src[f] !== undefined) target[f] = src[f];
}

// How long a search page may wait for upstream quotes before it is returned
// with what card_cache had; the fetch keeps running and caches for next time.
const UPSTREAM_WAIT_MS = 1500;

// deps: { db, bulkFetchByIdentifier, cacheCards, timeoutMs, coolingDown }
async function mergeSearchPrices(cards, deps) {
  if (!Array.isArray(cards) || cards.length === 0) return { cached: 0, fetched: 0 };
  const need = cards.filter(c => c && c.id && c.price_source == null);
  if (need.length === 0) return { cached: 0, fetched: 0 };
  const ids = [...new Set(need.map(c => c.id))];
  const rows = await deps.db.all(
    `SELECT id, ${PRICE_FIELDS.join(', ')} FROM card_cache
     WHERE id IN (SELECT value FROM json_each(?)) AND price_source IS NOT NULL`,
    [JSON.stringify(ids)]
  );
  const byId = new Map(rows.map(r => [r.id, r]));
  let cached = 0;
  const missing = [];
  const now = deps.now ? deps.now() : Date.now();
  for (const c of need) {
    const r = byId.get(c.id);
    if (r && hasQuote(r)) { copyPrices(c, r); cached++; continue; }
    if (r) copyPrices(c, r); // show the empty quote unless upstream has a real one
    const checked = emptyCheckedAt.get(c.id);
    if (r && checked && now - checked < EMPTY_RETRY_MS) continue;
    missing.push(c);
  }
  if (missing.length === 0 || !deps.bulkFetchByIdentifier) return { cached, fetched: 0 };
  const uniq = [...new Map(missing.map(c => [c.id, c])).values()].slice(0, UPSTREAM_CAP);
  const work = (async () => {
    const { cards: fresh } = await deps.bulkFetchByIdentifier(uniq.map(c => ({ id: c.id })), { upstream: true });
    const priced = (fresh || []).filter(f => f.price_source != null && hasQuote(f));
    const pricedIds = new Set(priced.map(f => f.id));
    for (const c of uniq) if (!pricedIds.has(c.id)) emptyCheckedAt.set(c.id, now);
    if (priced.length && deps.cacheCards) await deps.cacheCards(priced);
    return priced;
  })();
  // Scryfall rate-limited us: the queue is paused (60 s), so waiting would only
  // stall the page. Answer now; the queued fetch prices the next view.
  if (deps.coolingDown && deps.coolingDown()) { work.catch(() => {}); return { cached, fetched: 0 }; }
  let priced = null;
  try {
    priced = await Promise.race([
      work,
      new Promise(resolve => setTimeout(() => resolve(null), deps.timeoutMs ?? UPSTREAM_WAIT_MS)),
    ]);
  } catch (e) {
    console.error('search price fetch failed:', e.message);
    return { cached, fetched: 0 };
  }
  // Timed out: let the fetch finish in the background so the next page view
  // is priced from card_cache.
  if (!priced) { work.catch(() => {}); return { cached, fetched: 0 }; }
  const freshById = new Map(priced.map(f => [f.id, f]));
  let fetched = 0;
  for (const c of missing) {
    const f = freshById.get(c.id);
    if (f) { copyPrices(c, f); fetched++; }
  }
  return { cached, fetched };
}

module.exports = { mergeSearchPrices, PRICE_FIELDS };
