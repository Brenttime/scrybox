// Add Cards search answers from the local scryfall sidecar, which serves cards
// WITHOUT prices. Before a result page leaves the server, fill each price-less
// printing from card_cache (the last real quote), and only for printings that
// have never been priced ask the real Scryfall API through the queued,
// rate-limited bulkFetchByIdentifier({ upstream: true }). Writing those quotes
// back goes through cacheCards, whose upsert never lets a price-less row
// overwrite a real quote.
const PRICE_FIELDS = ['price_trend', 'price_normal', 'price_holofoil', 'price_etched', 'price_currency', 'price_source'];
const UPSTREAM_CAP = 150;

function copyPrices(target, src) {
  for (const f of PRICE_FIELDS) if (src[f] !== undefined) target[f] = src[f];
}

// deps: { db, bulkFetchByIdentifier, cacheCards, timeoutMs }
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
  for (const c of need) {
    const r = byId.get(c.id);
    if (r) { copyPrices(c, r); cached++; } else missing.push(c);
  }
  if (missing.length === 0 || !deps.bulkFetchByIdentifier) return { cached, fetched: 0 };
  const uniq = [...new Map(missing.map(c => [c.id, c])).values()].slice(0, UPSTREAM_CAP);
  const work = (async () => {
    const { cards: fresh } = await deps.bulkFetchByIdentifier(uniq.map(c => ({ id: c.id })), { upstream: true });
    const priced = (fresh || []).filter(f => f.price_source != null);
    if (priced.length && deps.cacheCards) await deps.cacheCards(priced);
    return priced;
  })();
  let priced = null;
  try {
    priced = await Promise.race([
      work,
      new Promise(resolve => setTimeout(() => resolve(null), deps.timeoutMs ?? 6000)),
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
