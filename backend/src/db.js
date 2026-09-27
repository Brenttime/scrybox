const sqlite3 = require('sqlite3').verbose();
const path = require('path');
const fs = require('fs');
const crypto = require('crypto');

// The app began life as "PokeKeep", a Pokémon-only tracker, so its database was
// called pokemon_cards.db. It has handled Magic since v1.4.x, and the file name
// is the last thing still carrying the old name.
const DB_FILENAME = 'bindarr.db';
const LEGACY_DB_FILENAME = 'pokemon_cards.db';

// Rename an existing pokemon_cards.db to the new name, WAL sidecars included.
// Getting this wrong loses collections: point SQLite at a name that isn't there
// and it cheerfully creates an empty database, which looks exactly like the app
// wiping everything. So: never overwrite, move the -wal/-shm files with the
// main one (un-checkpointed transactions live in the WAL), and on any failure
// keep using the old file rather than silently starting fresh.
// Returns the path that should actually be opened.
function resolveDbPath(target) {
  // Only ever migrate INTO the canonical name. A custom DB_PATH is the
  // operator's decision and must not attract someone else's old file.
  if (path.basename(target) !== DB_FILENAME) return target;

  const legacy = path.join(path.dirname(target), LEGACY_DB_FILENAME);
  if (fs.existsSync(target) || !fs.existsSync(legacy)) return target;

  try {
    for (const suffix of ['', '-wal', '-shm']) {
      if (fs.existsSync(legacy + suffix)) fs.renameSync(legacy + suffix, target + suffix);
    }
    console.log(`Renamed legacy database ${LEGACY_DB_FILENAME} -> ${DB_FILENAME}.`);
    return target;
  } catch (err) {
    console.error(
      `Could not rename ${LEGACY_DB_FILENAME} to ${DB_FILENAME} (${err.message}). ` +
      `Continuing with ${LEGACY_DB_FILENAME} — your data is safe, but please rename it manually.`
    );
    return legacy;
  }
}

// Ensure database directory exists
const requestedDbPath = process.env.DB_PATH || path.join(__dirname, `../database/${DB_FILENAME}`);
const dbDir = path.dirname(requestedDbPath);
if (!fs.existsSync(dbDir)) {
  fs.mkdirSync(dbDir, { recursive: true });
}
const dbPath = resolveDbPath(requestedDbPath);

console.log(`Connecting to SQLite database at: ${dbPath}`);
const dbConnection = new sqlite3.Database(dbPath, (err) => {
  if (err) {
    console.error('Error opening database:', err.message);
  } else {
    console.log('Database connection established successfully.');
    dbConnection.run('PRAGMA foreign_keys = ON');
    dbConnection.run('PRAGMA journal_mode = WAL');
    dbConnection.run('PRAGMA busy_timeout = 5000');
  }
});

// Helper wrappers for Promise-based SQL operations
function run(sql, params = []) {
  return new Promise((resolve, reject) => {
    dbConnection.run(sql, params, function (err) {
      if (err) reject(err);
      else resolve({ lastID: this.lastID, changes: this.changes });
    });
  });
}

function get(sql, params = []) {
  return new Promise((resolve, reject) => {
    dbConnection.get(sql, params, (err, row) => {
      if (err) reject(err);
      else resolve(row);
    });
  });
}

function all(sql, params = []) {
  return new Promise((resolve, reject) => {
    dbConnection.all(sql, params, (err, rows) => {
      if (err) reject(err);
      else resolve(rows);
    });
  });
}

// Run fn inside BEGIN IMMEDIATE / COMMIT, rolling back if it throws.
//
// fn takes no argument on purpose. It used to be handed a `tx` object, but that
// object was `{ run, get, all, withTransaction }` — the module's own exports under
// a different name. Callers gained nothing from it, and it read as statement-level
// isolation this does not provide: there is ONE sqlite3 connection here, so every
// query in the process is already inside whatever transaction is open. Use `db`
// directly and the scope is honest.
async function withTransaction(fn) {
  await run('BEGIN IMMEDIATE TRANSACTION');
  try {
    const result = await fn();
    await run('COMMIT');
    return result;
  } catch (error) {
    await run('ROLLBACK');
    throw error;
  }
}

// Run genuinely atomic multi-statement work on a dedicated connection. The
// process-wide connection cannot provide transaction ownership across awaits:
// unrelated requests can enqueue work into its open transaction. A dedicated
// connection keeps rollback boundaries honest while BEGIN IMMEDIATE excludes
// competing writers until commit.
async function withDedicatedSqliteTransaction(fn, beginSql) {
  const connection = await new Promise((resolve, reject) => {
    const candidate = new sqlite3.Database(dbPath, (error) => {
      if (error) reject(error);
      else resolve(candidate);
    });
  });
  const client = {
    run(sql, params = []) {
      return new Promise((resolve, reject) => {
        connection.run(sql, params, function (error) {
          if (error) reject(error);
          else resolve({ lastID: this.lastID, changes: this.changes });
        });
      });
    },
    get(sql, params = []) {
      return new Promise((resolve, reject) => {
        connection.get(sql, params, (error, row) => error ? reject(error) : resolve(row));
      });
    },
    all(sql, params = []) {
      return new Promise((resolve, reject) => {
        connection.all(sql, params, (error, rows) => error ? reject(error) : resolve(rows));
      });
    }
  };

  try {
    await client.run('PRAGMA foreign_keys = ON');
    await client.run('PRAGMA busy_timeout = 5000');
    await client.run(beginSql);
    try {
      const result = await fn(client);
      await client.run('COMMIT');
      return result;
    } catch (error) {
      try { await client.run('ROLLBACK'); } catch (rollbackError) {
        console.error('SQLite rollback failed:', rollbackError.message);
      }
      throw error;
    }
  } finally {
    await new Promise((resolve, reject) => {
      connection.close(error => error ? reject(error) : resolve());
    });
  }
}

async function withDedicatedTransaction(fn) {
  return withDedicatedSqliteTransaction(fn, 'BEGIN IMMEDIATE TRANSACTION');
}

// Pin several read statements to one WAL snapshot without taking the writer
// reservation used by mutation transactions. Coverage-gated local indexes use
// this so readiness and the reported-complete answer cannot race each other.
async function withDedicatedReadTransaction(fn) {
  return withDedicatedSqliteTransaction(fn, 'BEGIN TRANSACTION');
}

const PBKDF2_ITERATIONS = 210000;

function hashPassword(password) {
  const salt = crypto.randomBytes(16).toString('hex');
  const hash = crypto.pbkdf2Sync(password, salt, PBKDF2_ITERATIONS, 64, 'sha512').toString('hex');
  return `${PBKDF2_ITERATIONS}:${salt}:${hash}`;
}

// Initialize tables
async function initDb() {
  // Create users table
  await run(`
    CREATE TABLE IF NOT EXISTS users (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      username TEXT UNIQUE NOT NULL,
      password_hash TEXT NOT NULL,
      role TEXT CHECK(role IN ('admin', 'member')) NOT NULL DEFAULT 'member',
      share_token TEXT UNIQUE NOT NULL,
      share_enabled INTEGER DEFAULT 0,
      oidc_sub TEXT,
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP
    )
  `);

  // Create sessions table
  await run(`
    CREATE TABLE IF NOT EXISTS sessions (
      token TEXT PRIMARY KEY,
      user_id INTEGER NOT NULL,
      expires_at DATETIME NOT NULL,
      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
    )
  `);

  await run(`
    CREATE TABLE IF NOT EXISTS app_settings (
      id INTEGER PRIMARY KEY CHECK (id = 1),
      public_base_url TEXT DEFAULT '',
      oracle_tags_checked_at INTEGER,
      oracle_id_backfill_completed_at INTEGER
    )
  `);
  await run(`INSERT OR IGNORE INTO app_settings (id, public_base_url) VALUES (1, '')`);

  await run(`
    CREATE TABLE IF NOT EXISTS sets (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      series TEXT,
      printed_total INTEGER,
      total INTEGER,
      release_date TEXT,
      set_code TEXT,
      symbol_url TEXT,
      logo_url TEXT
    )
  `);

  await run(`
    CREATE TABLE IF NOT EXISTS card_cache (
      id TEXT PRIMARY KEY,
      oracle_id TEXT,
      scryfall_search_eligible INTEGER CHECK(scryfall_search_eligible IN (0, 1)),
      name TEXT NOT NULL,
      supertype TEXT,
      subtypes TEXT,
      types TEXT,
      rarity TEXT,
      set_id TEXT,
      set_name TEXT,
      number TEXT,
      image_url TEXT,
      price_trend REAL,
      price_normal REAL,
      price_holofoil REAL,
      price_etched REAL,
      price_currency TEXT DEFAULT 'USD',
      price_source TEXT,
      cmc REAL,
      color_identity TEXT,
      language TEXT DEFAULT 'English',
      printed_name TEXT,
      tcgplayer_product_id INTEGER,
      last_updated DATETIME DEFAULT CURRENT_TIMESTAMP
    )
  `);

  await run(`
    CREATE TABLE IF NOT EXISTS collection (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      card_id TEXT NOT NULL,
      quantity INTEGER DEFAULT 1,
      condition TEXT CHECK(condition IN ('Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged')) DEFAULT 'Near Mint',
      printing TEXT CHECK(printing IN ('Normal', 'Holofoil')) DEFAULT 'Normal',
      language TEXT DEFAULT 'English',
      purchase_price REAL,
      favorite INTEGER DEFAULT 0,
      is_trade INTEGER DEFAULT 0,
      added_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      FOREIGN KEY(card_id) REFERENCES card_cache(id)
    )
  `);

  await run(`
    CREATE TABLE IF NOT EXISTS price_history (
      card_id TEXT NOT NULL,
      price REAL NOT NULL,
      recorded_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      PRIMARY KEY(card_id, recorded_at)
    )
  `);

  // Sets the provider LISTS but has no usable card data for — no cards at all, or
  // cards with no artwork, which a scan catalog cannot use either way.
  //
  // Recorded so "N sets have no cards here yet" stops counting them.
  await run(`
    CREATE TABLE IF NOT EXISTS set_data_gaps (
      language TEXT NOT NULL,
      set_id TEXT NOT NULL,
      reason TEXT,
      seen_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      PRIMARY KEY (language, set_id)
    )
  `);

  await run(`
    CREATE TABLE IF NOT EXISTS decks (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      user_id INTEGER NOT NULL,
      name TEXT NOT NULL,
      description TEXT,
      checked_out INTEGER DEFAULT 0,
      checked_out_at DATETIME,
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      source TEXT DEFAULT 'manual',
      moxfield_public_id TEXT,
      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
    )
  `);

  await run(`
    CREATE TABLE IF NOT EXISTS deck_cards (
      deck_id INTEGER NOT NULL,
      card_id TEXT NOT NULL,
      quantity INTEGER DEFAULT 1,
      checked_out INTEGER DEFAULT 0,
      PRIMARY KEY(deck_id, card_id),
      FOREIGN KEY(deck_id) REFERENCES decks(id) ON DELETE CASCADE
    )
  `);

  // Card lists: ManaBox-style wishlists / buylists / missing-card lists — cards
  // the user tracks but does not necessarily own. Deliberately NOT the
  // collection table (no location, condition or price bookkeeping) and NOT a
  // deck (no format, 4-copy rule or checkout): a list is a plain quantity-per-
  // card set. Quantities are "wanted" amounts with no ownership ceiling — a
  // buylist wants the card regardless of what is already owned.
  await run(`
    CREATE TABLE IF NOT EXISTS card_lists (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      user_id INTEGER NOT NULL,
      name TEXT NOT NULL,
      description TEXT,
      accent_color TEXT DEFAULT '#10b981',
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
    )
  `);

  await run(`
    CREATE TABLE IF NOT EXISTS list_cards (
      list_id INTEGER NOT NULL,
      card_id TEXT NOT NULL,
      quantity INTEGER DEFAULT 1,
      PRIMARY KEY(list_id, card_id),
      FOREIGN KEY(list_id) REFERENCES card_lists(id) ON DELETE CASCADE
    )
  `);

  // Resolved match lists for CATALOG-ONLY collection queries (otag:,
  // availability:, ...). The upstream walk is the expensive part and is a
  // property of the query, not the user — so the resolved set of game-card
  // names is persisted here, not just in memory, and survives container
  // restarts. The intersect against the LIVE collection still runs on every
  // request, so only the (rarely-changing) upstream tag assignments can go
  // stale, up to the TTL.
  //
  // Completeness metadata (upstream_total, fetched_count, complete,
  // resolved_at): a walk that hit its page cap is a PARTIAL answer. Marking
  // it as such stops a truncated result from being served (and re-served for
  // a week) as if it were the whole tag — the UI shows "may be incomplete"
  // and the background revalidation keeps retrying.
  await run(`
    CREATE TABLE IF NOT EXISTS collection_query_cache (
      query TEXT NOT NULL,
      lang TEXT NOT NULL DEFAULT '',
      names TEXT NOT NULL,
      upstream_total INTEGER,
      fetched_count INTEGER NOT NULL DEFAULT 0,
      complete INTEGER NOT NULL DEFAULT 1,
      resolved_at INTEGER NOT NULL,
      expires_at INTEGER NOT NULL,
      PRIMARY KEY (query, lang)
    )
  `);
  // v1 of this table (added for the otag: dedup work) had no completeness
  // columns and a 24h hard TTL. It holds only short-lived cache data — drop
  // and recreate rather than ALTER; existing rows re-resolve on demand.
  const cqcCols = await all('PRAGMA table_info(collection_query_cache)');
  if (cqcCols.length && !cqcCols.some(c => c.name === 'upstream_total')) {
    await run('DROP TABLE collection_query_cache');
    await run(`
      CREATE TABLE IF NOT EXISTS collection_query_cache (
        query TEXT NOT NULL,
        lang TEXT NOT NULL DEFAULT '',
        names TEXT NOT NULL,
        upstream_total INTEGER,
        fetched_count INTEGER NOT NULL DEFAULT 0,
        complete INTEGER NOT NULL DEFAULT 1,
        resolved_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        PRIMARY KEY (query, lang)
      )
    `);
  }

  // --- MIGRATIONS ---
  // When the price sweep last ran. Scryfall updates prices once a day, so a
  // sweep more often than that cannot return anything new — and the boot sweep
  // would otherwise re-run on every restart (constantly, under nodemon).
  // Persisted rather than in-memory precisely because restarts are the problem.
  const appSettingsCols = await all(`PRAGMA table_info(app_settings)`);
  if (!appSettingsCols.some(c => c.name === 'mtg_prices_swept_at')) {
    await run(`ALTER TABLE app_settings ADD COLUMN mtg_prices_swept_at DATETIME`);
  }

  // How many days between automatic price refreshes; 0 switches them off.
  // Daily is what every install did before this column existed, so that is the
  // default and nothing changes for anyone who never touches it.
  if (!appSettingsCols.some(c => c.name === 'price_refresh_days')) {
    await run(`ALTER TABLE app_settings ADD COLUMN price_refresh_days INTEGER NOT NULL DEFAULT 1`);
  }


  // VESTIGIAL. This gated non-admin members building an individual per-set ORB
  // index, and there are no per-set indexes any more — scanning is CollectorVision
  // embeddings over a catalog, and catalog builds are admin-only (they walk a whole
  // provider and hammer its rate limits). Nothing reads the column; the migration
  // stays so an existing database still matches the schema this code expects, and
  // dropping it would mean a table rebuild for no gain.
  if (!appSettingsCols.some(c => c.name === 'allow_member_set_builds')) {
    await run(`ALTER TABLE app_settings ADD COLUMN allow_member_set_builds INTEGER NOT NULL DEFAULT 0`);
  }
  if (!appSettingsCols.some(c => c.name === 'scan_exclude_tokens')) {
    await run(`ALTER TABLE app_settings ADD COLUMN scan_exclude_tokens INTEGER NOT NULL DEFAULT 0`);
  }
  if (!appSettingsCols.some(c => c.name === 'scan_exclude_art_cards')) {
    await run(`ALTER TABLE app_settings ADD COLUMN scan_exclude_art_cards INTEGER NOT NULL DEFAULT 0`);
  }
  if (!appSettingsCols.some(c => c.name === 'scan_exclude_jumpstart')) {
    await run(`ALTER TABLE app_settings ADD COLUMN scan_exclude_jumpstart INTEGER NOT NULL DEFAULT 0`);
  }
  if (!appSettingsCols.some(c => c.name === 'scan_exclude_promos')) {
    await run(`ALTER TABLE app_settings ADD COLUMN scan_exclude_promos INTEGER NOT NULL DEFAULT 0`);
  }

  // Whether the first-run wizard has been seen through to the end. Server-side,
  // not localStorage, so the wizard follows the install rather than the browser:
  // an admin who starts setup on a laptop finishes it on a phone. Every install
  // starts at 0, upgrades included: the wizard is where catalogs, games and the
  // provider keys are explained, and an install that predates it has never been
  // offered that tour. It is one screen with a "Skip setup" button that marks it
  // done for good, so the cost to someone who wants nothing from it is one click.
  if (!appSettingsCols.some(c => c.name === 'setup_complete')) {
    await run(`ALTER TABLE app_settings ADD COLUMN setup_complete INTEGER NOT NULL DEFAULT 0`);
  }
  if (!appSettingsCols.some(c => c.name === 'oracle_tags_checked_at')) {
    await run(`ALTER TABLE app_settings ADD COLUMN oracle_tags_checked_at INTEGER`);
  }
  if (!appSettingsCols.some(c => c.name === 'oracle_id_backfill_completed_at')) {
    await run(`ALTER TABLE app_settings ADD COLUMN oracle_id_backfill_completed_at INTEGER`);
  }

  // A non-English printing is its own card, not a display variant of the English
  // one: it has its own provider id, its own art and its own name. `language`
  // records which printing a cached row IS (collection.language still records
  // what the user OWNS), and `printed_name` holds the localized name — `name`
  // stays English so search, deck lists and marketplace links keep working.
  const cardCacheCols = await all(`PRAGMA table_info(card_cache)`);
  if (!cardCacheCols.some(c => c.name === 'oracle_id')) {
    await run(`ALTER TABLE card_cache ADD COLUMN oracle_id TEXT`);
  }
  if (!cardCacheCols.some(c => c.name === 'scryfall_search_eligible')) {
    await run(`ALTER TABLE card_cache ADD COLUMN scryfall_search_eligible INTEGER
      CHECK(scryfall_search_eligible IN (0, 1))`);
    // Existing rows need their default /cards/search eligibility populated from
    // Scryfall's card layout alongside Oracle identity.
    await run(`UPDATE app_settings SET oracle_id_backfill_completed_at = NULL WHERE id = 1`);
  }
  if (!cardCacheCols.some(c => c.name === 'language')) {
    await run(`ALTER TABLE card_cache ADD COLUMN language TEXT DEFAULT 'English'`);
  }
  if (!cardCacheCols.some(c => c.name === 'printed_name')) {
    await run(`ALTER TABLE card_cache ADD COLUMN printed_name TEXT`);
  }
  // Marketplace links as the PROVIDER gives them. Building them from name+set+number
  // only works for English cards: those marketplaces index English names, so a
  // localized name searches to nothing. Scryfall hands back a real product/search
  // URL per card, so store it.
  for (const col of ['tcgplayer_url', 'cardmarket_url']) {
    if (!cardCacheCols.some(c => c.name === col)) {
      await run(`ALTER TABLE card_cache ADD COLUMN ${col} TEXT`);
    }
  }
  // Which marketplace a row's prices came from, and in what currency.
  //
  // The price columns have always been unit-less, and until now they mixed
  // TCGplayer USD (English printings) with Cardmarket EUR (the non-English ones)
  // while the UI rendered one '$' over both. Recording the source per row is what
  // lets the inspector say which number it is showing, instead of inferring it
  // from whether a Cardmarket URL happens to exist.
  if (!cardCacheCols.some(c => c.name === 'price_currency')) {
    await run(`ALTER TABLE card_cache ADD COLUMN price_currency TEXT DEFAULT 'USD'`);
  }
  if (!cardCacheCols.some(c => c.name === 'price_source')) {
    await run(`ALTER TABLE card_cache ADD COLUMN price_source TEXT`);
    // Backfill from what each row's id already tells us: Scryfall is the only
    // source that ever wrote rows, and its ids are prefixed. No network needed.
    await run(`UPDATE card_cache SET price_source = 'scryfall', price_currency = 'USD' WHERE id LIKE 'mtg-%'`);
  }
  if (!cardCacheCols.some(c => c.name === 'price_etched')) {
    await run(`ALTER TABLE card_cache ADD COLUMN price_etched REAL`);
  }
  // Rules text. NULL means "not fetched yet" (rows cached before this column
  // existed); '' means the card genuinely has none (vanilla creatures, basics).
  // oracle_faces holds [{name, mana_cost, type_line, oracle_text}] for
  // multi-face cards (DFC, split, adventure), NULL otherwise.
  if (!cardCacheCols.some(c => c.name === 'oracle_text')) {
    await run(`ALTER TABLE card_cache ADD COLUMN oracle_text TEXT`);
  }
  if (!cardCacheCols.some(c => c.name === 'oracle_faces')) {
    await run(`ALTER TABLE card_cache ADD COLUMN oracle_faces TEXT`);
  }
  if (!cardCacheCols.some(c => c.name === 'tcgplayer_product_id')) {
    await run(`ALTER TABLE card_cache ADD COLUMN tcgplayer_product_id INTEGER`);
    // Backfill from the URLs already cached, rather than re-fetching 100k+ rows
    // from Scryfall to learn something we are already holding: a product-page URL
    // literally contains the id. Runs once, inside the column-added branch, so a
    // reboot does not re-scan the table.
    //
    // Two encodings, because Scryfall wraps its link in an affiliate redirect that
    // percent-encodes the inner URL ('%2Fproduct%2F') while a plain product URL
    // does not ('/product/'). The SEARCH form is '%2Fproduct%3F' — a different
    // string, so it cannot match either pattern and correctly stays NULL.
    //
    // CAST stops at the first non-digit, which is how the trailing '%3Fpage%3D1'
    // is discarded; the > 0 guard drops anything that produced no digits at all.
    for (const [needle, skip] of [['%2Fproduct%2F', 13], ['/product/', 9]]) {
      await run(
        `UPDATE card_cache
            SET tcgplayer_product_id = CAST(substr(tcgplayer_url, instr(tcgplayer_url, ?) + ?) AS INTEGER)
          WHERE tcgplayer_product_id IS NULL
            AND instr(tcgplayer_url, ?) > 0
            AND CAST(substr(tcgplayer_url, instr(tcgplayer_url, ?) + ?) AS INTEGER) > 0`,
        [needle, skip, needle, needle, skip]
      );
    }
  }

  // Scryfall's daily Oracle Tags export. A refresh writes a complete inactive
  // generation, then flips this one-row active pointer in the same dedicated
  // transaction. Failed/interrupted imports therefore leave the previous
  // generation queryable.
  await run(`
    CREATE TABLE IF NOT EXISTS oracle_tag_generations (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      source_updated_at TEXT NOT NULL,
      imported_at INTEGER NOT NULL,
      active INTEGER NOT NULL DEFAULT 0 CHECK(active IN (0, 1))
    )
  `);
  await run(`CREATE UNIQUE INDEX IF NOT EXISTS idx_oracle_tag_one_active
    ON oracle_tag_generations(active) WHERE active = 1`);
  await run(`
    CREATE TABLE IF NOT EXISTS oracle_tags (
      generation_id INTEGER NOT NULL REFERENCES oracle_tag_generations(id) ON DELETE CASCADE,
      tag_id TEXT NOT NULL,
      slug TEXT NOT NULL,
      label TEXT,
      description TEXT,
      PRIMARY KEY (generation_id, tag_id)
    )
  `);
  await run(`
    CREATE TABLE IF NOT EXISTS oracle_tag_aliases (
      generation_id INTEGER NOT NULL,
      tag_id TEXT NOT NULL,
      alias TEXT NOT NULL,
      PRIMARY KEY (generation_id, tag_id, alias),
      FOREIGN KEY (generation_id, tag_id)
        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE
    )
  `);
  await run(`CREATE INDEX IF NOT EXISTS idx_oracle_tag_alias_lookup
    ON oracle_tag_aliases(generation_id, alias, tag_id)`);
  await run(`
    CREATE TABLE IF NOT EXISTS oracle_tag_edges (
      generation_id INTEGER NOT NULL,
      parent_tag_id TEXT NOT NULL,
      child_tag_id TEXT NOT NULL,
      PRIMARY KEY (generation_id, parent_tag_id, child_tag_id),
      FOREIGN KEY (generation_id, parent_tag_id)
        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE,
      FOREIGN KEY (generation_id, child_tag_id)
        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE
    )
  `);
  await run(`
    CREATE TABLE IF NOT EXISTS oracle_tag_assignments (
      generation_id INTEGER NOT NULL,
      tag_id TEXT NOT NULL,
      oracle_id TEXT NOT NULL,
      PRIMARY KEY (generation_id, tag_id, oracle_id),
      FOREIGN KEY (generation_id, tag_id)
        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE
    )
  `);
  await run(`CREATE INDEX IF NOT EXISTS idx_oracle_tag_assignment_oracle
    ON oracle_tag_assignments(generation_id, oracle_id, tag_id)`);
  await run(`
    CREATE TABLE IF NOT EXISTS oracle_tag_closure (
      generation_id INTEGER NOT NULL,
      ancestor_tag_id TEXT NOT NULL,
      descendant_tag_id TEXT NOT NULL,
      PRIMARY KEY (generation_id, ancestor_tag_id, descendant_tag_id),
      FOREIGN KEY (generation_id, ancestor_tag_id)
        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE,
      FOREIGN KEY (generation_id, descendant_tag_id)
        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE
    )
  `);

  const collectionCols = await all(`PRAGMA table_info(collection)`);
  if (!collectionCols.some(c => c.name === 'user_id')) {
    await run(`ALTER TABLE collection ADD COLUMN user_id INTEGER REFERENCES users(id) ON DELETE CASCADE`);
  }
  if (!collectionCols.some(c => c.name === 'is_trade')) {
    await run(`ALTER TABLE collection ADD COLUMN is_trade INTEGER DEFAULT 0`);
  }
  if (!collectionCols.some(c => c.name === 'favorite')) {
    await run(`ALTER TABLE collection ADD COLUMN favorite INTEGER DEFAULT 0`);
  }
  if (!collectionCols.some(c => c.name === 'notes')) {
    await run(`ALTER TABLE collection ADD COLUMN notes TEXT DEFAULT ''`);
  }
  // Where a copy came from: 'manabox' for rows written by ManaBox sync (or the
  // original ManaBox migration), NULL for anything added in Scrybox. Sync only
  // ever removes 'manabox' rows, so a card scanned or searched in Scrybox is
  // never taken away by a later ManaBox export.
  if (!collectionCols.some(c => c.name === 'source')) {
    await run(`ALTER TABLE collection ADD COLUMN source TEXT`);
  }
  // A proxy copy (a stand-in, not the real card): valued at $0 and shown with
  // rarity 'Proxy'. Zeroed at read time; card_cache prices and purchase_price
  // are untouched.
  if (!collectionCols.some(c => c.name === 'is_proxy')) {
    await run(`ALTER TABLE collection ADD COLUMN is_proxy INTEGER DEFAULT 0`);
  }
  const listCols = await all(`PRAGMA table_info(card_lists)`);
  if (listCols.length && !listCols.some(c => c.name === 'source')) {
    await run(`ALTER TABLE card_lists ADD COLUMN source TEXT`);
  }
  if (listCols.length && !listCols.some(c => c.name === 'icon')) {
    await run(`ALTER TABLE card_lists ADD COLUMN icon TEXT`);
  }
  await run(`
    CREATE TABLE IF NOT EXISTS manabox_sync_runs (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      summary TEXT NOT NULL,
      report TEXT NOT NULL
    )
  `);

  // --- Pokemon and grading removal (2026-08) ---
  // The app was Pokemon-agnostic in schema but Pokemon-flavoured in code; the
  // Pokemon game and the graded-slab feature are now gone. Databases created
  // before the removal still carry:
  //   - rows with game = 'pokemon' (and NULL, the pre-column default)
  //   - the game column on sets/card_cache/collection/decks/card_lists
  //   - the graded columns on collection (grader/grade/cert_number/market_value*)
  //   - card_cache.price_reverse_holofoil / price_1st_edition
  //     (Pokemon finishes: Magic has only Normal and Holofoil)
  //   - users.psa_api_token / users.graded_price_api_key
  //   - app_settings.pokemon_provider / pokemon_prices_swept_at /
  //     tcgdex_prices_swept_at / tcgcsv_prices_swept_at / scan_exclude_digital
  //   - the psa_cert, tcgplayer_catalog and tcgplayer_product tables
  //   - a 3-column set_data_gaps table with a (game, language, set_id) key
  //   - price_history rows for the deleted Pokemon cards (no FK, orphaned by id)
  // Drop all of it while keeping every MTG row. The whole block is one
  // transaction: any newly discovered dependency (a child table, an index, a
  // constraint) aborts the migration atomically instead of leaving a
  // half-migrated database the server would keep re-failing on every boot.
  // Guarded and idempotent — a database already in the new shape is a no-op.
  {
    const hasTable = async (name) => {
      const r = await get(`SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?`, [name]);
      return !!r;
    };
    const colExists = async (table, col) =>
      (await all(`PRAGMA table_info(${table})`)).some(c => c.name === col);
    // DROP COLUMN fails when an index still names the column, so dropCol clears
    // any user-created index on the table that references the target column
    // first. Autoindexes (origin 'u'/'pk') cannot be dropped and are skipped.
    const dropIndexesReferencing = async (table, cols) => {
      const target = new Set(cols);
      const indexes = await all(`PRAGMA index_list(${table})`);
      for (const ix of indexes) {
        if (ix.origin !== 'c') continue; // only user-created indexes are droppable
        const colsOfIndex = await all(`PRAGMA index_info(${ix.name})`);
        if (colsOfIndex.some(c => target.has(c.name))) {
          await run(`DROP INDEX ${ix.name}`);
        }
      }
    };
    const dropCol = async (table, col) => {
      if (await colExists(table, col)) {
        await dropIndexesReferencing(table, [col]);
        await run(`ALTER TABLE ${table} DROP COLUMN ${col}`);
      }
    };

    await withTransaction(async () => {
      // Older schemas called the universal set-code field `ptcgo_code`, after
      // the retired client that first supplied it. Preserve the values while
      // giving the active MTG schema a provider-neutral name.
      if (await colExists('sets', 'ptcgo_code')) {
        if (await colExists('sets', 'set_code')) {
          await run(`UPDATE sets SET set_code = COALESCE(NULLIF(set_code, ''), ptcgo_code)`);
          await dropCol('sets', 'ptcgo_code');
        } else {
          await run(`ALTER TABLE sets RENAME COLUMN ptcgo_code TO set_code`);
        }
      }

      // 1. Rows, children FIRST. tcgplayer_product.card_id is a real FK to
      // card_cache without ON DELETE, so the table (not just its Pokemon rows)
      // must be gone before the card_cache delete — otherwise the delete dies
      // on the FK. collection has no FK to the cache rows it holds, so it only
      // needs its own Pokemon rows cleared. On a fresh database none of this
      // ever existed, so all of it is a no-op.
      if (await hasTable('tcgplayer_product')) {
        // (The card_cache.tcgplayer_product_id COLUMN is different: Scryfall
        // still hands out Magic product ids, and that column is what the
        // TCGplayer link buttons use.)
        await run(`DROP INDEX IF EXISTS idx_tcgplayer_product_pid`);
        await run(`DROP TABLE tcgplayer_product`);
      }
      for (const table of ['collection', 'card_cache', 'decks', 'card_lists', 'sets']) {
        if (await colExists(table, 'game')) {
          await run(`DELETE FROM ${table} WHERE game = 'pokemon' OR game IS NULL`);
        }
      }

      // price_history has no foreign key — its only tie to card_cache is the
      // card_id value — so the Pokemon cards' price points would survive the
      // cache-row delete as orphans.
      await run(`DELETE FROM price_history WHERE card_id NOT IN (SELECT id FROM card_cache)`);

      // 2. The game column everywhere it lived.
      for (const table of ['sets', 'card_cache', 'collection', 'decks', 'card_lists']) {
        await dropCol(table, 'game');
      }

      // 3. Graded columns. collection has no FKs naming them, so they drop in
      // place.
      for (const col of ['market_value_source', 'market_value_at', 'market_value',
        'cert_number', 'grade', 'grader']) {
        await dropCol('collection', col);
      }
      if (await hasTable('psa_cert')) await run(`DROP TABLE psa_cert`);

      // card_cache prices for finishes that no longer exist in the app, plus
      // rolling averages that belonged to the retired provider schema. Price
      // history is now the price_history table exclusively.
      // reverse holo is a Pokemon finish and 1st Edition pricing was a
      // Pokemon-era feature. The MTG resolver reads price_normal/price_holofoil.
      for (const col of ['price_reverse_holofoil', 'price_1st_edition',
        'price_avg1', 'price_avg7', 'price_avg30']) {
        await dropCol('card_cache', col);
      }

      // DROP COLUMN leaves the table-level printing CHECK untouched. Rebuild an
      // upgraded collection whose old CHECK still admits retired finishes, while
      // preserving every current column, row and surviving user-created index.
      // Rows are normalized before they meet the stricter constraint.
      const collectionDef = await get(
        `SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'collection'`
      );
      const mtgPrintingCheck = /CHECK\s*\(\s*printing\s+IN\s*\(\s*'Normal'\s*,\s*'Holofoil'\s*\)\s*\)/i;
      if (!collectionDef || !mtgPrintingCheck.test(collectionDef.sql || '')) {
        const indexSql = (await all(`
          SELECT sql FROM sqlite_master
          WHERE type = 'index' AND tbl_name = 'collection' AND sql IS NOT NULL
          ORDER BY name
        `)).map(r => r.sql);
        await run(`DROP TABLE IF EXISTS collection_new`);
        await run(`
          CREATE TABLE collection_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            card_id TEXT NOT NULL,
            quantity INTEGER DEFAULT 1,
            condition TEXT CHECK(condition IN ('Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged')) DEFAULT 'Near Mint',
            printing TEXT CHECK(printing IN ('Normal', 'Holofoil')) DEFAULT 'Normal',
            language TEXT DEFAULT 'English',
            purchase_price REAL,
            favorite INTEGER DEFAULT 0,
            is_trade INTEGER DEFAULT 0,
            list_type TEXT DEFAULT 'collection',
            added_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            notes TEXT DEFAULT '',
            user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
            source TEXT,
            is_proxy INTEGER DEFAULT 0,
            FOREIGN KEY(card_id) REFERENCES card_cache(id)
          )
        `);
        await run(`
          INSERT INTO collection_new
            (id, card_id, quantity, condition, printing, language, purchase_price,
             favorite, is_trade, list_type, added_at, notes, user_id, source, is_proxy)
          SELECT id, card_id, quantity, condition,
                 CASE WHEN printing = 'Holofoil' THEN 'Holofoil' ELSE 'Normal' END,
                 language, purchase_price, favorite, is_trade, list_type,
                 added_at, notes, user_id, source, COALESCE(is_proxy, 0)
          FROM collection
        `);
        await run(`DROP TABLE collection`);
        await run(`ALTER TABLE collection_new RENAME TO collection`);
        for (const sql of indexSql) await run(sql);
      }

      await dropCol('users', 'psa_api_token');
      await dropCol('users', 'graded_price_api_key');
      // The user-level provider key was the pokemontcg.io key — with the Pokemon
      // provider gone it names nothing, so the column goes with it.
      await dropCol('users', 'tcg_api_key');

      // app_settings: drop the Pokemon-era settings. Idempotent via PRAGMA.
      // (The scan_exclude_* columns stay: cardSets.js still reads them to
      // filter MTG child sets — tokens, memorabilia, jumpstart, promos.)
      for (const col of ['pokemon_provider', 'pokemon_prices_swept_at',
        'tcgdex_prices_swept_at', 'tcgcsv_prices_swept_at', 'scan_exclude_digital']) {
        if (await colExists('app_settings', col)) {
          await run(`ALTER TABLE app_settings DROP COLUMN ${col}`);
        }
      }

      // Wishlist removal (2026-08). collection.list_type existed only to split
      // the owned rows ('collection') from wishlist rows; with the wishlist
      // feature gone the column is a constant, so the wishlist rows go and the
      // column drops with them. Trade-sharing rows are flagged by the separate
      // is_trade flag, not by list_type, so nothing here touches them. Idempotent
      // via the column guard (fresh databases never had it).
      if (await colExists('collection', 'list_type')) {
        await run(`DELETE FROM collection WHERE list_type <> 'collection'`);
        await dropCol('collection', 'list_type');
      }

      // 4. set_data_gaps kept its MTG rows but its PRIMARY KEY changed from
      // (game, language, set_id) to (language, set_id). SQLite cannot alter a
      // table's PRIMARY KEY in place, so rebuild it — copying only the MTG
      // rows: the old key allowed the SAME language+set_id in both games (the
      // Pokemon and Magic worlds shared short ids like 'me1'-'me4'), which would
      // collide on the new key and abort the migration.
      if (await hasTable('set_data_gaps')) {
        const oldCols = await all(`PRAGMA table_info(set_data_gaps)`);
        if (oldCols.some(c => c.name === 'game')) {
          const keep = oldCols.map(c => c.name).filter(n => n !== 'game').join(', ');
          await run(`DROP TABLE IF EXISTS set_data_gaps_new`);
          await run(`CREATE TABLE set_data_gaps_new (
            language TEXT NOT NULL,
            set_id TEXT NOT NULL,
            reason TEXT,
            seen_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (language, set_id)
          )`);
          await run(`INSERT INTO set_data_gaps_new (${keep})
            SELECT ${keep} FROM set_data_gaps WHERE game = 'mtg'`);
          await run(`DROP TABLE set_data_gaps`);
          await run(`ALTER TABLE set_data_gaps_new RENAME TO set_data_gaps`);
        }
      }

      // 5. The TCGplayer product catalogue only existed to name scans against
      // the ready-made Pokemon catalog; nothing reads it now.
      if (await hasTable('tcgplayer_catalog')) {
        await run(`DROP INDEX IF EXISTS idx_tcgplayer_catalog_pid`);
        await run(`DROP TABLE tcgplayer_catalog`);
      }
    });
    console.log('Legacy non-MTG and grading schema cleanup complete; MTG rows preserved.');
  }

  // --- Storage removal (2026-08) ---
  // The physical storage feature (binder/box locations, compartments, card
  // placement) is gone. Databases created before it is removed still hold the
  // locations/compartments/compartment_assignments tables plus the
  // location_id/compartment_id/position columns on collection. Drop all of it
  // while keeping every card row.
  //
  // The columns are dropped by REBUILDING the table, not DROP COLUMN: SQLite
  // refuses to drop a column that a foreign-key definition still names
  // ("unknown column in foreign key definition"), and collection's FKs name
  // both placement columns. Guarded and idempotent — a database already in the
  // new shape is a no-op.
  {
    const migCols = await all(`PRAGMA table_info(collection)`);
    const hasPlacement = migCols.some(c => ['location_id', 'compartment_id', 'position'].includes(c.name));
    const storageTables = await all(`
      SELECT name FROM sqlite_master
      WHERE type = 'table'
        AND name IN ('locations', 'compartments', 'compartment_assignments')
    `);
    if (hasPlacement || storageTables.length > 0) {
      console.log('Removing storage schema (locations, compartments, placement columns)...');
      const keepList = migCols
        .filter(c => !['location_id', 'compartment_id', 'position'].includes(c.name))
        .map(c => c.name)
        .join(', ');

      if (storageTables.length > 0) {
        // The storage tables drop OUTSIDE any transaction: PRAGMA foreign_keys
        // only takes effect when no transaction is open, and the drops need it
        // off (compartment_assignments -> compartments -> locations). Always turn
        // enforcement back on, including when a malformed legacy table fails to
        // drop, so the process never continues with referential checks disabled.
        await run(`PRAGMA foreign_keys = OFF`);
        try {
          await run(`DROP TABLE IF EXISTS compartment_assignments`);
          await run(`DROP TABLE IF EXISTS compartments`);
          await run(`DROP TABLE IF EXISTS locations`);
        } finally {
          await run(`PRAGMA foreign_keys = ON`);
        }
      }
      await run(`DROP INDEX IF EXISTS idx_collection_comp_user_qty`);
      await run(`DROP INDEX IF EXISTS idx_collection_loc_pos`);

      if (hasPlacement) {
        // Rebuild collection without the placement columns. keepList is built
        // from the LIVE column list, so an old database missing some newer
        // columns still copies over cleanly; the new table's defaults fill in.
        // (The game and graded columns were already dropped by the earlier
        // Pokemon-and-graded migration, so they are not in keepList.)
        await withTransaction(async () => {
          await run(`DROP TABLE IF EXISTS collection_new`);
          await run(`
            CREATE TABLE collection_new (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              card_id TEXT NOT NULL,
              quantity INTEGER DEFAULT 1,
              condition TEXT CHECK(condition IN ('Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged')) DEFAULT 'Near Mint',
              printing TEXT CHECK(printing IN ('Normal', 'Holofoil')) DEFAULT 'Normal',
              language TEXT DEFAULT 'English',
              purchase_price REAL,
              favorite INTEGER DEFAULT 0,
              is_trade INTEGER DEFAULT 0,
              added_at DATETIME DEFAULT CURRENT_TIMESTAMP,
              notes TEXT DEFAULT '',
              user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
              source TEXT,
              is_proxy INTEGER DEFAULT 0,
              FOREIGN KEY(card_id) REFERENCES card_cache(id)
            )
          `);
          await run(`INSERT INTO collection_new (${keepList}) SELECT ${keepList} FROM collection`);
          await run(`DROP TABLE collection`);
          await run(`ALTER TABLE collection_new RENAME TO collection`);
        });
      }

      // The rebuild dropped collection's indexes with the old table; recreate
      // the one that outlived storage.
      await run(`CREATE INDEX IF NOT EXISTS idx_collection_user ON collection(user_id)`);
      console.log('Storage schema removed; collection rows preserved.');
    }
  }

  // --- Notes removal (2026-08) ---
  // The standalone scratchpad Notes feature (its own `notes` table) is gone.
  // Databases created before removal still hold the table. It is not referenced
  // by any foreign key, so it drops cleanly with foreign_keys on. Idempotent.
  const notesTable = await get(`SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'notes'`);
  if (notesTable) {
    console.log('Removing notes table...');
    await run(`DROP TABLE IF EXISTS notes`);
    console.log('Notes table removed.');
  }

  const usersCols = await all(`PRAGMA table_info(users)`);
  // Read-only key for scripts and dashboards (issue #33): a Bearer credential that
  // does not expire the way a session does, so a finance tracker polling net worth
  // is not logged out overnight. authenticateToken refuses anything but GET on it,
  // which is what makes a long-lived credential acceptable in the first place.
  if (!usersCols.some(c => c.name === 'api_key')) {
    await run(`ALTER TABLE users ADD COLUMN api_key TEXT`);
  }
  await run(`CREATE UNIQUE INDEX IF NOT EXISTS idx_users_api_key ON users(api_key) WHERE api_key IS NOT NULL`);

  // --- Marketplace order-import credentials (per user) ---
  // ManaPool exposes a real buyer-order API (email + access token from its
  // integration settings); TCGplayer has no buyer-order API at all, so the only
  // server-callable path is its cookie-authenticated SPA gateway, which needs a
  // pasted session jar. These columns hold whichever the user configured; a
  // member sees only their own (the router scopes every read/write to req.user.id).
  //   - the token and the cookie jar are secrets: they are never returned by any
  //     GET (the UI shows masked presence), and a TCGplayer jar is possession-
  //     equivalent to a login, so it must not be echoed in logs or error bodies.
  //   - `*_enabled` is a master on/off the user controls, so an old saved
  //     credential cannot silently keep being used; a fetch requires both the
  //     credential AND it being enabled.
  for (const [col, type] of [
    ['manapool_email', 'TEXT'], ['manapool_token', 'TEXT'],
    ['manapool_enabled', 'INTEGER NOT NULL DEFAULT 0'], ['manapool_saved_at', 'TEXT'],
    ['tcgplayer_cookies', 'TEXT'],
    ['tcgplayer_enabled', 'INTEGER NOT NULL DEFAULT 0'], ['tcgplayer_saved_at', 'TEXT'],
  ]) {
    if (!usersCols.some(c => c.name === col)) {
      await run(`ALTER TABLE users ADD COLUMN ${col} ${type}`);
    }
  }
  // OIDC / SSO unique identifier (subject claim `sub`). Links an external IdP
  // user account to a Scrybox user row.
  if (!usersCols.some(c => c.name === 'oidc_sub')) {
    await run(`ALTER TABLE users ADD COLUMN oidc_sub TEXT`);
  }
  await run(`CREATE UNIQUE INDEX IF NOT EXISTS idx_users_oidc_sub ON users(oidc_sub) WHERE oidc_sub IS NOT NULL`);

  const deckCardsCols = await all(`PRAGMA table_info(deck_cards)`);
  if (!deckCardsCols.some(c => c.name === 'checked_out')) {
    await run(`ALTER TABLE deck_cards ADD COLUMN checked_out INTEGER DEFAULT 0`);
  }
  // The deck's declared commander(s): set from Moxfield's commanders board and
  // a precon's commander section. The deck list prefers these over guessing.
  if (!deckCardsCols.some(c => c.name === 'is_commander')) {
    await run(`ALTER TABLE deck_cards ADD COLUMN is_commander INTEGER NOT NULL DEFAULT 0`);
  }

  // --- Moxfield sync ---
  // A Moxfield author whose public decks we mirror into this instance. The
  // deck list (who exists, and when a deck changed) is polled on one interval;
  // the card contents of every tracked deck on another, faster one. Both
  // intervals live in app_settings below so the UI can tune them without a
  // restart.
  await run(`
    CREATE TABLE IF NOT EXISTS moxfield_authors (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      user_id INTEGER NOT NULL,
      moxfield_user TEXT NOT NULL,
      display_name TEXT,
      profile_image_url TEXT,
      last_decklist_sync_at DATETIME,
      last_content_check_at DATETIME,
      last_error TEXT,
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
      UNIQUE(user_id, moxfield_user)
    )
  `);
  // Databases created before the per-minute check got its own timestamp keep
  // their author rows; add the column in place so "last content check" is
  // observable for existing installations too.
  const mfxAuthorCols = await all(`PRAGMA table_info(moxfield_authors)`);
  if (!mfxAuthorCols.some(c => c.name === 'last_content_check_at')) {
    await run(`ALTER TABLE moxfield_authors ADD COLUMN last_content_check_at DATETIME`);
  }
  await run(`CREATE INDEX IF NOT EXISTS idx_mfx_author_user ON moxfield_authors(user_id)`);

  // One row per Moxfield deck we track. `bindarr_deck_id` is NULL until the
  // decklist sync has created the local deck; `last_updated_at` is Moxfield's
  // own last-update stamp — the cheap change detector. When it differs from
  // `last_synced_updated_at` the content sync pulls the full deck.
  await run(`
    CREATE TABLE IF NOT EXISTS moxfield_decks (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      author_id INTEGER NOT NULL REFERENCES moxfield_authors(id) ON DELETE CASCADE,
      public_id TEXT NOT NULL,
      name TEXT NOT NULL,
      format TEXT,
      mainboard_count INTEGER,
      sideboard_count INTEGER,
      maybeboard_count INTEGER,
      commander_count INTEGER,
      last_updated_at TEXT,
      last_synced_updated_at TEXT,
      bindarr_deck_id INTEGER,
      last_content_sync_at DATETIME,
      last_error TEXT,
      enabled INTEGER NOT NULL DEFAULT 1,
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      UNIQUE(author_id, public_id)
    )
  `);
  await run(`CREATE INDEX IF NOT EXISTS idx_mfx_deck_author ON moxfield_decks(author_id)`);

  // Per-deck on/off switch: unchecked decks stay tracked (their Moxfield
  // listing is kept) but are never imported — no local mirror, no content
  // pulls. Added after the fact; existing rows default to ON so nothing stops
  // syncing without an explicit opt-out.
  const mfxDeckCols = await all(`PRAGMA table_info(moxfield_decks)`);
  if (!mfxDeckCols.some(c => c.name === 'enabled')) {
    await run(`ALTER TABLE moxfield_decks ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1`);
  }

  // Which Moxfield deck a local deck mirrors, when it is one. NULL for every
  // hand-made deck; the sync only ever edits decks that carry this link, so a
  // local deck stays untouched unless it was imported from Moxfield.
  const mfxDecksCols = await all(`PRAGMA table_info(decks)`);
  if (!mfxDecksCols.some(c => c.name === 'source')) {
    await run(`ALTER TABLE decks ADD COLUMN source TEXT DEFAULT 'manual'`);
  }
  if (!mfxDecksCols.some(c => c.name === 'moxfield_public_id')) {
    await run(`ALTER TABLE decks ADD COLUMN moxfield_public_id TEXT`);
  }
  // A public Moxfield deck may legitimately be mirrored by several Scrybox
  // users. Uniqueness is per owner, not global. Drop the former global index
  // before creating its user-scoped replacement under a new stable name.
  await run(`DROP INDEX IF EXISTS idx_decks_mfx_public_id`);
  await run(`CREATE UNIQUE INDEX IF NOT EXISTS idx_decks_user_mfx_public_id
    ON decks(user_id, moxfield_public_id) WHERE moxfield_public_id IS NOT NULL`);

  // Moxfield sync cadence, per instance: how often the decklist refreshes
  // (author exists? new decks? removed decks? which changed?) and how often
  // the contents of tracked decks are checked. Minutes, clamped server-side.
  if (!appSettingsCols.some(c => c.name === 'moxfield_decklist_interval_min')) {
    await run(`ALTER TABLE app_settings ADD COLUMN moxfield_decklist_interval_min INTEGER NOT NULL DEFAULT 60`);
  }
  if (!appSettingsCols.some(c => c.name === 'moxfield_content_interval_min')) {
    await run(`ALTER TABLE app_settings ADD COLUMN moxfield_content_interval_min INTEGER NOT NULL DEFAULT 1`);
  }

  const decksCols = await all(`PRAGMA table_info(decks)`);
  if (!decksCols.some(c => c.name === 'format')) {
    await run(`ALTER TABLE decks ADD COLUMN format TEXT DEFAULT 'Standard'`);
  }
  if (!decksCols.some(c => c.name === 'category')) {
    await run(`ALTER TABLE decks ADD COLUMN category TEXT DEFAULT 'Competitive'`);
  }
  if (!decksCols.some(c => c.name === 'accent_color')) {
    await run(`ALTER TABLE decks ADD COLUMN accent_color TEXT DEFAULT '#eab308'`);
  }
  if (!decksCols.some(c => c.name === 'target_size')) {
    await run(`ALTER TABLE decks ADD COLUMN target_size INTEGER DEFAULT 60`);
  }

  // Monotonic collection snapshot used by progressively paginated catalog
  // searches. Offset pages are separate HTTP requests, so deterministic order
  // alone cannot prevent a concurrent collection write from producing a hybrid
  // result. Triggers make every INSERT/UPDATE/DELETE invalidate the token that
  // page 1 returned. A single global revision is deliberately conservative:
  // another user's write may restart hydration, but can never let mixed pages
  // look complete.
  await run(`
    CREATE TABLE IF NOT EXISTS collection_snapshot (
      id INTEGER PRIMARY KEY CHECK(id = 1),
      revision INTEGER NOT NULL DEFAULT 0
    )
  `);
  await run(`INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0)`);
  for (const operation of ['INSERT', 'UPDATE', 'DELETE']) {
    await run(`
      CREATE TRIGGER IF NOT EXISTS collection_snapshot_after_${operation.toLowerCase()}
      AFTER ${operation} ON collection
      BEGIN
        UPDATE collection_snapshot SET revision = revision + 1 WHERE id = 1;
      END
    `);
  }

  // --- PERFORMANCE INDEXES ---
  // `user_id` first, because it is the predicate on essentially every read in the
  // app. The collection page also reads newest-first; carrying that order in the
  // index prevents SQLite from sorting tens of thousands of rows into a temporary
  // B-tree on every visit while still serving user-only stats predicates.
  await run(`DROP INDEX IF EXISTS idx_collection_user_game`);
  await run(`DROP INDEX IF EXISTS idx_collection_user`);
  await run(`CREATE INDEX IF NOT EXISTS idx_collection_user_added
    ON collection(user_id, added_at DESC, id DESC)`);
  await run(`CREATE INDEX IF NOT EXISTS idx_card_cache_set_num ON card_cache(set_id, number)`);
  await run(`CREATE INDEX IF NOT EXISTS idx_card_cache_oracle_id ON card_cache(oracle_id)`);
  await run(`CREATE INDEX IF NOT EXISTS idx_card_cache_logical_key ON card_cache(LOWER(TRIM(COALESCE(name, ''))))`);
  await run(`DROP INDEX IF EXISTS idx_card_cache_logical_name`);
  await run(`CREATE INDEX IF NOT EXISTS idx_deck_cards_checkout ON deck_cards(deck_id, checked_out)`);
  // Indexes on the retired tags/audit_logs tables. A fresh database never creates
  // those tables at all now; an upgraded one keeps them (dropping a table is not
  // something a migration should do to data it cannot restore), but nothing reads
  // them, so the indexes are pure write cost.
  await run(`DROP INDEX IF EXISTS idx_collection_tags_tag_id`);
  await run(`DROP INDEX IF EXISTS idx_audit_logs_user_date`);

  // --- SEED DATA & MIGRATION TO DEFAULT ADMIN ---
  const userCount = await get(`SELECT COUNT(*) as count FROM users`);
  let adminId = null;
  // An empty users table stays empty on purpose. The first visit to the web UI
  // asks for a password and creates the owner account itself
  // (POST /api/auth/bootstrap), so no generated password is ever printed to a log
  // and left in place. DEFAULT_ADMIN_PASSWORD still seeds an account up front for
  // scripted deploys that need credentials before anyone opens a browser.
  //
  // Either way the account is named `admin`: the name is not the owner's to choose,
  // because it is what the orphan-row adoption below looks up and what the
  // DEFAULT_ADMIN_PASSWORD path has to hardcode anyway (nobody could guess a name
  // the server picked). Nothing in the app renames a user, so it stays true.
  if (userCount.count === 0 && process.env.DEFAULT_ADMIN_PASSWORD) {
    const defaultPassHash = hashPassword(process.env.DEFAULT_ADMIN_PASSWORD);
    const defaultShareToken = crypto.randomBytes(16).toString('hex');
    const result = await run(`
      INSERT INTO users (username, password_hash, role, share_token, share_enabled)
      VALUES (?, ?, ?, ?, ?)
    `, ['admin', defaultPassHash, 'admin', defaultShareToken, 0]);
    adminId = result.lastID;
    console.log('Created admin user "admin" from DEFAULT_ADMIN_PASSWORD.');
  } else if (userCount.count === 0) {
    console.log('No accounts yet. Open the web UI to create the owner account.');
  } else {
    const adminUser = await get(`SELECT id FROM users WHERE username = ?`, ['admin']);
    if (adminUser) {
      adminId = adminUser.id;
    }
  }

  if (adminId) {
    await adoptOrphanRows(adminId);
  }

  // Persisted files live outside SQLite, so schema cleanup cannot reach them.
  // Run the bounded, idempotent sweep only after the retained cache/collection
  // rows are final, so it has an authoritative keep-set for custom art.
  await require('./cardArt').cleanupRetiredData();
}

// Cards from before multi-user carry `user_id IS NULL`. They belong to whoever
// owns the install. Runs from initDb when an admin already exists (or
// DEFAULT_ADMIN_PASSWORD just made one), and from the bootstrap route when the
// owner account is created through the UI instead — a database with orphan rows
// and no users reaches the app that way and would otherwise show an empty
// collection.
async function adoptOrphanRows(userId) {
  await run(`UPDATE collection SET user_id = ? WHERE user_id IS NULL`, [userId]);
}

module.exports = {
  dbConnection,
  dbPath,
  run,
  get,
  all,
  withTransaction,
  withDedicatedTransaction,
  withDedicatedReadTransaction,
  initDb,
  adoptOrphanRows,
  hashPassword,
  // Exported for tests — the rename runs at module load, so it can't be
  // exercised through a normal require.
  resolveDbPath,
  DB_FILENAME,
  LEGACY_DB_FILENAME
};
