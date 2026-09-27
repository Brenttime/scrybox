// SQLite's CURRENT_TIMESTAMP stores UTC but as a naive "YYYY-MM-DD HH:MM:SS"
// string with no timezone marker. JS's Date parser treats a string like that
// as LOCAL time, so on any server not running in UTC, a value that's really
// "now" gets parsed as hours off — enough to misorder it against a properly
// UTC-tagged timestamp (e.g. an ISO string with a trailing Z). Always read
// SQLite datetimes through this so they compare correctly against Date.now()
// or other real UTC timestamps.
function parseSqliteUtc(str) {
  if (!str) return new Date(NaN);
  return /Z$|[+-]\d\d:\d\d$/.test(str) ? new Date(str) : new Date(str.replace(' ', 'T') + 'Z');
}

function resolveCardPrice(card) {
  if (!card) return 0;
  // A proxy copy is worth nothing, whatever the real card trades at.
  if (card.is_proxy) return 0;
  if (card.printing === 'Holofoil' && card.price_holofoil !== null && card.price_holofoil > 0) {
    return card.price_holofoil;
  }
  if (card.printing === 'Normal' && card.price_normal !== null && card.price_normal > 0) {
    return card.price_normal;
  }
  return card.price_trend || 0;
}

// An owned proxy copy reads as its own 'Proxy' rarity. base_rarity keeps the
// printed rarity so the UI can restore it when the flag is cleared. Rows with
// no is_proxy column (card_cache-only rows) are returned unchanged.
const PROXY_RARITY = 'Proxy';
function applyProxyRarity(row) {
  if (!row || row.is_proxy === undefined) return row;
  const base = row.base_rarity !== undefined ? row.base_rarity : row.rarity;
  return { ...row, is_proxy: row.is_proxy ? 1 : 0, base_rarity: base, rarity: row.is_proxy ? PROXY_RARITY : base };
}

// Hydrate a raw card_cache row: its array columns are stored as JSON strings,
// so parse them back to arrays. Missing columns (e.g. color_identity on a row
// cached before it existed) become []. Returns a shallow copy; the raw row is
// untouched.
function parseCardRow(row) {
  if (!row) return row;
  return {
    ...row,
    ...applyProxyRarity(row),
    subtypes: JSON.parse(row.subtypes || '[]'),
    types: JSON.parse(row.types || '[]'),
    color_identity: JSON.parse(row.color_identity || '[]'),
  };
}

const isVintageSet = (setId) => {
  const id = (setId || '').toLowerCase();
  // MTG's vintage era: the core sets, expansions and masters released before
  // 2000 (per Scryfall). Everything from March 2000 (Onslaught) onward is
  // modern-era.
  return ['lea', 'leb', '2ed', 'arn', 'atq', '3ed', 'leg', 'drk', 'fem', '4bb', '4ed',
    'ice', 'bchr', 'chr', 'ren', 'rin', 'hml', 'all', 'mir', 'vis', '5ed', 'wth',
    'tmp', 'sth', 'exo', 'ugl', 'usg', 'ulg', '6ed', 'uds', 'mmq'].includes(id);
};

// Record a price point, but only when it actually moved. The price sweep runs
// on every boot and nodemon reboots on every code edit, so the unguarded insert
// was writing a fresh row per card per restart — 17k rows in a single day, all
// the same number. A price series only needs the points where the price
// changed; the flat stretches between them are implied by the line.
async function recordPrice(cardId, price) {
  if (!cardId || !(price > 0)) return false;
  const db = require('../db');
  // Millisecond resolution, not CURRENT_TIMESTAMP. recorded_at is part of the
  // primary key, and the default is second-resolution. Read both the latest
  // value and timestamp inside this INSERT so concurrent callers cannot both
  // decide that the same price is a new movement.
  const res = await db.run(
    `WITH latest(price, recorded_at) AS (
       SELECT price, recorded_at
       FROM price_history
       WHERE card_id = ?
       ORDER BY recorded_at DESC, rowid DESC
       LIMIT 1
     ), candidate(ts) AS (SELECT strftime('%Y-%m-%d %H:%M:%f', 'now'))
     INSERT INTO price_history (card_id, price, recorded_at)
     SELECT ?, ?, CASE
       WHEN (SELECT recorded_at FROM latest) IS NULL
         OR (SELECT recorded_at FROM latest) < candidate.ts THEN candidate.ts
       ELSE strftime('%Y-%m-%d %H:%M:%f', (SELECT recorded_at FROM latest), '+0.001 seconds')
     END
     FROM candidate
     WHERE NOT EXISTS (SELECT 1 FROM latest WHERE price = ?)`,
    [cardId, cardId, price, price]
  );
  return !!(res && res.changes === 1);
}

// Scryfall: "We only update prices for cards once per day. Fetching card data
// more frequently than 24 hours will not yield new prices."
// (https://scryfall.com/docs/api/rate-limits). Sweeping more often than daily
// is pure load for zero new data, so the sweep gates on this.
const PRICE_SWEEP_INTERVAL_MS = 1000 * 60 * 60 * 24;
// Every provider that sweeps needs an entry here, and an unknown key is treated as
// "do not sweep" — so a provider added to server.js but forgotten here goes quiet
// instead of loud: shouldSweepPrices returns false, the boot catch-up skips, and
// markPricesSwept no-ops.
const SWEEP_COLUMN = {
  mtg: 'mtg_prices_swept_at',
};

// How often the automatic sweep is allowed to run, as configured. 0 turns it off.
//
// Upstream added this because one of the providers it can be pointed at bills per
// card refreshed, so a daily sweep of a large collection was a recurring charge.
// Nothing here costs money -- Scryfall is free -- but the setting still earns its
// keep: a collector who looks at their portfolio once a month should not pay for
// thirty sweeps, and a very large collection makes each sweep real work. Kept
// behaviourally identical to upstream so the next sync does not re-conflict.
//
// Unreadable or missing settings fall back to daily, which is what every install
// did before this existed.
const DEFAULT_PRICE_REFRESH_DAYS = 1;

async function priceRefreshDays() {
  const db = require('../db');
  try {
    const row = await db.get(`SELECT price_refresh_days FROM app_settings WHERE id = 1`);
    const n = Number(row && row.price_refresh_days);
    return Number.isInteger(n) && n >= 0 ? n : DEFAULT_PRICE_REFRESH_DAYS;
  } catch {
    return DEFAULT_PRICE_REFRESH_DAYS;
  }
}

// Has this game's price sweep gone stale enough to be worth running again?
//
// This is now the ONLY thing deciding when an automatic sweep runs. server.js
// used to pass force: true from its daily timer, on the reasoning that the timer
// was itself the right cadence — which meant this function's answer was ignored
// in the only case that mattered, and that a provider missing from SWEEP_COLUMN
// looked fine because the forced path never asked. The timer now ticks hourly and
// unforced, and this decides.
//
// Hourly rather than daily on purpose: with a daily tick and a daily interval,
// any drift at all leaves "23h 59m elapsed" at the moment of the tick, which
// skips and turns a daily refresh into an every-other-day one. Checking often
// and refusing cheaply has no such edge.
async function shouldSweepPrices(game) {
  const col = SWEEP_COLUMN[game];
  if (!col) return false;
  const days = await priceRefreshDays();
  if (days === 0) return false;            // automatic refresh switched off
  const db = require('../db');
  try {
    const row = await db.get(`SELECT ${col} AS sweptAt FROM app_settings WHERE id = 1`);
    if (!row || !row.sweptAt) return true;
    return Date.now() - parseSqliteUtc(row.sweptAt).getTime() >= days * PRICE_SWEEP_INTERVAL_MS;
  } catch {
    return true; // never block the sweep on a bookkeeping failure
  }
}

async function markPricesSwept(game) {
  const col = SWEEP_COLUMN[game];
  if (!col) return;
  const db = require('../db');
  try {
    await db.run(`UPDATE app_settings SET ${col} = CURRENT_TIMESTAMP WHERE id = 1`);
  } catch (e) {
    console.warn(`Could not record ${game} price sweep time:`, e.message);
  }
}

module.exports = {
  parseSqliteUtc,
  shouldSweepPrices,
  markPricesSwept,
  priceRefreshDays,
  DEFAULT_PRICE_REFRESH_DAYS,
  PRICE_SWEEP_INTERVAL_MS,
  resolveCardPrice,
  parseCardRow,
  applyProxyRarity,
  PROXY_RARITY,
  isVintageSet,
  recordPrice
};
