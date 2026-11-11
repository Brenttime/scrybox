//! Rust-owned schema baseline + migrations — the twin of `initDb()` from
//! `backend/src/db.js` (unit 4, Lane B). Every CREATE/ALTER/PRAGMA statement
//! is ported in the SAME ORDER with the SAME guards, so running this against
//! a fresh file produces exactly the schema Node's initDb produces, and
//! running it against a legacy copy performs the same guarded cleanups
//! (Pokemon/grading removal, storage removal, notes removal, index churn).
//!
//! Versioning: `PRAGMA user_version` is written as `TARGET_USER_VERSION` at
//! the end of a full pass. A boot that already sees the target version skips
//! the whole body (the Node twin runs the idempotent ladder every boot;
//! Rust owns the migration under SCRYBOX_SKIP_DB_INIT and the schema is
//! pinned, so a version gate is the honest contract — bump the constant when
//! the Node twin changes).
//!
//! The admin seed branch mirrors the `DEFAULT_ADMIN_PASSWORD` path (PBKDF2
//! hash via `auth_support::hashPassword` twin — same 210000/16-byte-hex
//! format) and the orphan-row adoption. The trailing `cardArt.cleanupRetiredData()`
//! sweep is ported too: it is part of initDb, not of the routes.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::auth_support::{hash_password, random_hex};

/// Schema version stamped into `PRAGMA user_version` after a full pass.
pub const TARGET_USER_VERSION: i64 = 1;

#[derive(Debug, Clone, Default)]
pub struct MigOpts {
    /// `process.env.DEFAULT_ADMIN_PASSWORD` twin. `None`/empty = no seed
    /// (first-visit bootstrap creates the owner), exactly like Node.
    pub admin_password: Option<String>,
    /// `CV_MODEL_DIR` (cardArt.js MODEL_DIR). NOTE: cardArt.js falls back
    /// to `backend/data/models` (relative to the NODE SOURCES) when the env
    /// is unset — a path the Rust front cannot resolve. The scrybox Docker
    /// image always sets CV_MODEL_DIR (Dockerfile ENV), so pair-mode
    /// containers never hit the fallback; a bare `node server.js` source
    /// install paired with RUST_MIGRATIONS without the env would lose the
    /// retired-model sweep — main.rs warns at startup in that case.
    pub model_dir: Option<PathBuf>,
    /// cardArt.js USER_DIR. `None` = `<db dir>/card-art`, like Node.
    pub user_art_dir: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct MigReport {
    /// True when `user_version` was already at target and the ladder was
    /// skipped. The boot-every tail (admin re-seed check, orphan adoption,
    /// retired-data sweep) still ran — see `run_boot_tail`.
    pub skipped: bool,
    pub seeded_admin: bool,
    pub removed_models: usize,
    pub removed_art: usize,
}

/// Open the migration connection with the same connect-time PRAGMAs as
/// `backend/src/db.js` (writer-side: foreign_keys ON matters for the
/// rebuild blocks, busy_timeout for the writer Node may hold).
pub fn open_migration_conn(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -65536;
         PRAGMA mmap_size = 268435456;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(conn)
}

/// Boot-time entry: opens its own connection, runs `migrate`.
pub fn run_boot_migration(path: &Path, opts: &MigOpts) -> rusqlite::Result<MigReport> {
    let path = resolve_legacy_db_path(path);
    let conn = open_migration_conn(&path)?;
    migrate(&conn, &path, opts)
}

/// `resolveDbPath` from backend/src/db.js (lines 22-40): an old
/// pokemon_cards.db is renamed to bindarr.db (WAL sidecars included) BEFORE
/// anything opens it — SQLite happily creates an empty DB at a missing path,
/// which looks exactly like the app wiping the collection. Node does this at
/// module load; in pair mode the Rust front opens the file first, so the
/// twin must run here or Node's rename silently never happens. Returns the
/// path that should actually be opened (legacy on rename failure, exactly
/// like Node's catch arm).
pub fn resolve_legacy_db_path(target: &Path) -> PathBuf {
    const DB_FILENAME: &str = "bindarr.db";
    const LEGACY_DB_FILENAME: &str = "pokemon_cards.db";
    // Only ever migrate INTO the canonical name; a custom DB_PATH is the
    // operator's decision and must not attract someone else's old file.
    if target.file_name().and_then(|s| s.to_str()) != Some(DB_FILENAME) {
        return target.to_path_buf();
    }
    let legacy = target.with_file_name(LEGACY_DB_FILENAME);
    if target.exists() || !legacy.exists() {
        return target.to_path_buf();
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = format!("{}{}", legacy.display(), suffix);
        if std::path::Path::new(&from).exists() {
            let to = format!("{}{}", target.display(), suffix);
            if let Err(err) = std::fs::rename(&from, &to) {
                tracing::warn!(
                    "Could not rename {LEGACY_DB_FILENAME} to {DB_FILENAME} ({err}). \
                     Continuing with {LEGACY_DB_FILENAME} — your data is safe, \
                     but please rename it manually."
                );
                return legacy;
            }
        }
    }
    tracing::info!("Renamed legacy database {LEGACY_DB_FILENAME} -> {DB_FILENAME}.");
    target.to_path_buf()
}

pub fn migrate(conn: &Connection, db_path: &Path, opts: &MigOpts) -> rusqlite::Result<MigReport> {
    let mut report = MigReport::default();
    // Mirror the Node connection-open pragmas (backend/src/db.js:66-68),
    // which ran BEFORE any initDb statement: PRAGMA optimize builds
    // sqlite_stat1 on databases that already carry rows and is a no-op
    // (creates nothing) on an empty file — exactly what the Node goldens
    // show. Runs even on the version-gate skip, like every Node open.
    // Node runs these via sqlite3 `dbConnection.run(...)` with no callback —
    // an error is logged, never propagated, and the open proceeds. Mirror
    // that: optimize must never kill the boot.
    let _ = conn.execute_batch("PRAGMA analysis_limit = 1000;\nPRAGMA optimize = 0x10002;");
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= TARGET_USER_VERSION {
        // Node runs the WHOLE initDb every boot; the ladder above is
        // idempotent so skipping it once stamped is invisible — EXCEPT for
        // the tail below, which is not ladder-shaped: the DEFAULT_ADMIN_
        // PASSWORD re-seed (if users ever empties out), orphan adoption of
        // rows that gain user_id IS NULL later, and cardArt.cleanupRetired-
        // Data all run on every single Node boot. Replicate that: the gate
        // skips the ladder only, never the tail.
        report.skipped = true;
        run_boot_tail(conn, db_path, opts, &mut report)?;
        return Ok(report);
    }

    // ---- CREATE TABLE block (db.js 195-435) ----
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS users (\n      id INTEGER PRIMARY KEY AUTOINCREMENT,\n      username TEXT UNIQUE NOT NULL,\n      password_hash TEXT NOT NULL,\n      role TEXT CHECK(role IN ('admin', 'member')) NOT NULL DEFAULT 'member',\n      share_token TEXT UNIQUE NOT NULL,\n      share_enabled INTEGER DEFAULT 0,\n      oidc_sub TEXT,\n      created_at DATETIME DEFAULT CURRENT_TIMESTAMP\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS sessions (\n      token TEXT PRIMARY KEY,\n      user_id INTEGER NOT NULL,\n      expires_at DATETIME NOT NULL,\n      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS app_settings (\n      id INTEGER PRIMARY KEY CHECK (id = 1),\n      public_base_url TEXT DEFAULT '',\n      oracle_tags_checked_at INTEGER,\n      oracle_id_backfill_completed_at INTEGER\n    )\n  ",
    )?;
    conn.execute("INSERT OR IGNORE INTO app_settings (id, public_base_url) VALUES (1, '')", [])?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS sets (\n      id TEXT PRIMARY KEY,\n      name TEXT NOT NULL,\n      series TEXT,\n      printed_total INTEGER,\n      total INTEGER,\n      release_date TEXT,\n      set_code TEXT,\n      symbol_url TEXT,\n      logo_url TEXT\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS card_cache (\n      id TEXT PRIMARY KEY,\n      oracle_id TEXT,\n      scryfall_search_eligible INTEGER CHECK(scryfall_search_eligible IN (0, 1)),\n      name TEXT NOT NULL,\n      supertype TEXT,\n      subtypes TEXT,\n      types TEXT,\n      rarity TEXT,\n      set_id TEXT,\n      set_name TEXT,\n      number TEXT,\n      image_url TEXT,\n      price_trend REAL,\n      price_normal REAL,\n      price_holofoil REAL,\n      price_etched REAL,\n      price_currency TEXT DEFAULT 'USD',\n      price_source TEXT,\n      price_estimated INTEGER NOT NULL DEFAULT 0,\n      cmc REAL,\n      color_identity TEXT,\n      language TEXT DEFAULT 'English',\n      printed_name TEXT,\n      tcgplayer_product_id INTEGER,\n      last_updated DATETIME DEFAULT CURRENT_TIMESTAMP\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS collection (\n      id INTEGER PRIMARY KEY AUTOINCREMENT,\n      card_id TEXT NOT NULL,\n      quantity INTEGER DEFAULT 1,\n      condition TEXT CHECK(condition IN ('Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged')) DEFAULT 'Near Mint',\n      printing TEXT CHECK(printing IN ('Normal', 'Holofoil')) DEFAULT 'Normal',\n      language TEXT DEFAULT 'English',\n      purchase_price REAL,\n      favorite INTEGER DEFAULT 0,\n      is_trade INTEGER DEFAULT 0,\n      added_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      FOREIGN KEY(card_id) REFERENCES card_cache(id)\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS price_history (\n      card_id TEXT NOT NULL,\n      price REAL NOT NULL,\n      recorded_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      PRIMARY KEY(card_id, recorded_at)\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS set_data_gaps (\n      language TEXT NOT NULL,\n      set_id TEXT NOT NULL,\n      reason TEXT,\n      seen_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      PRIMARY KEY (language, set_id)\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS decks (\n      id INTEGER PRIMARY KEY AUTOINCREMENT,\n      user_id INTEGER NOT NULL,\n      name TEXT NOT NULL,\n      description TEXT,\n      checked_out INTEGER DEFAULT 0,\n      checked_out_at DATETIME,\n      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      source TEXT DEFAULT 'manual',\n      moxfield_public_id TEXT,\n      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS deck_cards (\n      deck_id INTEGER NOT NULL,\n      card_id TEXT NOT NULL,\n      quantity INTEGER DEFAULT 1,\n      checked_out INTEGER DEFAULT 0,\n      PRIMARY KEY(deck_id, card_id),\n      FOREIGN KEY(deck_id) REFERENCES decks(id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS search_history (\n      user_id INTEGER PRIMARY KEY,\n      data BLOB NOT NULL,\n      updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS deck_maybe_cards (\n      deck_id INTEGER NOT NULL,\n      card_id TEXT NOT NULL,\n      quantity INTEGER DEFAULT 1,\n      PRIMARY KEY(deck_id, card_id),\n      FOREIGN KEY(deck_id) REFERENCES decks(id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS card_lists (\n      id INTEGER PRIMARY KEY AUTOINCREMENT,\n      user_id INTEGER NOT NULL,\n      name TEXT NOT NULL,\n      description TEXT,\n      accent_color TEXT DEFAULT '#10b981',\n      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS list_cards (\n      list_id INTEGER NOT NULL,\n      card_id TEXT NOT NULL,\n      quantity INTEGER DEFAULT 1,\n      PRIMARY KEY(list_id, card_id),\n      FOREIGN KEY(list_id) REFERENCES card_lists(id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS collection_query_cache (\n      query TEXT NOT NULL,\n      lang TEXT NOT NULL DEFAULT '',\n      names TEXT NOT NULL,\n      upstream_total INTEGER,\n      fetched_count INTEGER NOT NULL DEFAULT 0,\n      complete INTEGER NOT NULL DEFAULT 1,\n      resolved_at INTEGER NOT NULL,\n      expires_at INTEGER NOT NULL,\n      PRIMARY KEY (query, lang)\n    )\n  ",
    )?;
    // v1 of this table had no completeness columns: drop and recreate
    // (cache data only; rows re-resolve on demand). db.js 419-435.
    let cqc_cols = table_cols(conn, "collection_query_cache")?;
    if !cqc_cols.is_empty() && !cqc_cols.iter().any(|c| c == "upstream_total") {
        conn.execute_batch("DROP TABLE collection_query_cache")?;
        conn.execute_batch(
            "\n      CREATE TABLE IF NOT EXISTS collection_query_cache (\n        query TEXT NOT NULL,\n        lang TEXT NOT NULL DEFAULT '',\n        names TEXT NOT NULL,\n        upstream_total INTEGER,\n        fetched_count INTEGER NOT NULL DEFAULT 0,\n        complete INTEGER NOT NULL DEFAULT 1,\n        resolved_at INTEGER NOT NULL,\n        expires_at INTEGER NOT NULL,\n        PRIMARY KEY (query, lang)\n      )\n    ",
        )?;
    }

    // ---- app_settings ladder (db.js 437-492; cols snapshot taken once) ----
    let app_settings_cols = table_cols(conn, "app_settings")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "mtg_prices_swept_at", "DATETIME")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "price_refresh_days", "INTEGER NOT NULL DEFAULT 1")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "allow_member_set_builds", "INTEGER NOT NULL DEFAULT 0")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "scan_exclude_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "scan_exclude_art_cards", "INTEGER NOT NULL DEFAULT 0")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "scan_exclude_jumpstart", "INTEGER NOT NULL DEFAULT 0")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "scan_exclude_promos", "INTEGER NOT NULL DEFAULT 0")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "setup_complete", "INTEGER NOT NULL DEFAULT 0")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "oracle_tags_checked_at", "INTEGER")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "oracle_id_backfill_completed_at", "INTEGER")?;

    // ---- card_cache ladder (db.js 494-582; one snapshot) ----
    let card_cache_cols = table_cols(conn, "card_cache")?;
    add_col_if_missing(conn, &card_cache_cols, "card_cache", "oracle_id", "TEXT")?;
    if !card_cache_cols.iter().any(|c| c == "scryfall_search_eligible") {
        conn.execute_batch(
            "ALTER TABLE card_cache ADD COLUMN scryfall_search_eligible INTEGER\n      CHECK(scryfall_search_eligible IN (0, 1))",
        )?;
        conn.execute("UPDATE app_settings SET oracle_id_backfill_completed_at = NULL WHERE id = 1", [])?;
    }
    add_col_if_missing(conn, &card_cache_cols, "card_cache", "language", "TEXT DEFAULT 'English'")?;
    add_col_if_missing(conn, &card_cache_cols, "card_cache", "printed_name", "TEXT")?;
    for col in ["tcgplayer_url", "cardmarket_url"] {
        add_col_if_missing(conn, &card_cache_cols, "card_cache", col, "TEXT")?;
    }
    add_col_if_missing(conn, &card_cache_cols, "card_cache", "price_currency", "TEXT DEFAULT 'USD'")?;
    if !card_cache_cols.iter().any(|c| c == "price_source") {
        conn.execute_batch("ALTER TABLE card_cache ADD COLUMN price_source TEXT")?;
        conn.execute(
            "UPDATE card_cache SET price_source = 'scryfall', price_currency = 'USD' WHERE id LIKE 'mtg-%'",
            [],
        )?;
    }
    add_col_if_missing(conn, &card_cache_cols, "card_cache", "price_estimated", "INTEGER NOT NULL DEFAULT 0")?;
    add_col_if_missing(conn, &card_cache_cols, "card_cache", "price_etched", "REAL")?;
    add_col_if_missing(conn, &card_cache_cols, "card_cache", "oracle_text", "TEXT")?;
    add_col_if_missing(conn, &card_cache_cols, "card_cache", "oracle_faces", "TEXT")?;
    if !card_cache_cols.iter().any(|c| c == "tcgplayer_product_id") {
        conn.execute_batch("ALTER TABLE card_cache ADD COLUMN tcgplayer_product_id INTEGER")?;
        // Backfill from the URLs already cached. Two encodings (affiliate
        // redirect encodes /product/ as %2Fproduct%2F). CAST stops at the
        // first non-digit; > 0 drops non-numeric. db.js 572-581.
        for (needle, skip) in [("%2Fproduct%2F", 13_i64), ("/product/", 9_i64)] {
            conn.execute(
                "UPDATE card_cache
                    SET tcgplayer_product_id = CAST(substr(tcgplayer_url, instr(tcgplayer_url, ?) + ?) AS INTEGER)
                  WHERE tcgplayer_product_id IS NULL
                    AND instr(tcgplayer_url, ?) > 0
                    AND CAST(substr(tcgplayer_url, instr(tcgplayer_url, ?) + ?) AS INTEGER) > 0",
                params![needle, skip, needle, needle, skip],
            )?;
        }
    }

    // ---- Oracle Tags tables (db.js 584-655) ----
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS oracle_tag_generations (\n      id INTEGER PRIMARY KEY AUTOINCREMENT,\n      source_updated_at TEXT NOT NULL,\n      imported_at INTEGER NOT NULL,\n      active INTEGER NOT NULL DEFAULT 0 CHECK(active IN (0, 1))\n    )\n  ",
    )?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_oracle_tag_one_active\n    ON oracle_tag_generations(active) WHERE active = 1",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS oracle_tags (\n      generation_id INTEGER NOT NULL REFERENCES oracle_tag_generations(id) ON DELETE CASCADE,\n      tag_id TEXT NOT NULL,\n      slug TEXT NOT NULL,\n      label TEXT,\n      description TEXT,\n      PRIMARY KEY (generation_id, tag_id)\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS oracle_tag_aliases (\n      generation_id INTEGER NOT NULL,\n      tag_id TEXT NOT NULL,\n      alias TEXT NOT NULL,\n      PRIMARY KEY (generation_id, tag_id, alias),\n      FOREIGN KEY (generation_id, tag_id)\n        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_oracle_tag_alias_lookup\n    ON oracle_tag_aliases(generation_id, alias, tag_id)",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS oracle_tag_edges (\n      generation_id INTEGER NOT NULL,\n      parent_tag_id TEXT NOT NULL,\n      child_tag_id TEXT NOT NULL,\n      PRIMARY KEY (generation_id, parent_tag_id, child_tag_id),\n      FOREIGN KEY (generation_id, parent_tag_id)\n        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE,\n      FOREIGN KEY (generation_id, child_tag_id)\n        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS oracle_tag_assignments (\n      generation_id INTEGER NOT NULL,\n      tag_id TEXT NOT NULL,\n      oracle_id TEXT NOT NULL,\n      PRIMARY KEY (generation_id, tag_id, oracle_id),\n      FOREIGN KEY (generation_id, tag_id)\n        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE\n    )\n  ",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_oracle_tag_assignment_oracle\n    ON oracle_tag_assignments(generation_id, oracle_id, tag_id)",
    )?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS oracle_tag_closure (\n      generation_id INTEGER NOT NULL,\n      ancestor_tag_id TEXT NOT NULL,\n      descendant_tag_id TEXT NOT NULL,\n      PRIMARY KEY (generation_id, ancestor_tag_id, descendant_tag_id),\n      FOREIGN KEY (generation_id, ancestor_tag_id)\n        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE,\n      FOREIGN KEY (generation_id, descendant_tag_id)\n        REFERENCES oracle_tags(generation_id, tag_id) ON DELETE CASCADE\n    )\n  ",
    )?;

    // ---- collection / card_lists ladder (db.js 657-701) ----
    let collection_cols = table_cols(conn, "collection")?;
    add_col_if_missing(conn, &collection_cols, "collection", "user_id", "INTEGER REFERENCES users(id) ON DELETE CASCADE")?;
    add_col_if_missing(conn, &collection_cols, "collection", "is_trade", "INTEGER DEFAULT 0")?;
    add_col_if_missing(conn, &collection_cols, "collection", "favorite", "INTEGER DEFAULT 0")?;
    add_col_if_missing(conn, &collection_cols, "collection", "notes", "TEXT DEFAULT ''")?;
    add_col_if_missing(conn, &collection_cols, "collection", "source", "TEXT")?;
    add_col_if_missing(conn, &collection_cols, "collection", "is_proxy", "INTEGER DEFAULT 0")?;
    let list_cols = table_cols(conn, "card_lists")?;
    if !list_cols.is_empty() {
        add_col_if_missing(conn, &list_cols, "card_lists", "source", "TEXT")?;
        add_col_if_missing(conn, &list_cols, "card_lists", "icon", "TEXT")?;
        add_col_if_missing(conn, &list_cols, "card_lists", "sort_order", "INTEGER")?;
    }
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS manabox_sync_runs (\n      id INTEGER PRIMARY KEY AUTOINCREMENT,\n      user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,\n      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      summary TEXT NOT NULL,\n      report TEXT NOT NULL\n    )\n  ",
    )?;

    // ---- Pokemon/grading removal, one transaction (db.js 703-921) ----
    {
        with_immediate_transaction(conn, pokedex_removal)?;
        tracing::info!("Legacy non-MTG and grading schema cleanup complete; MTG rows preserved.");
    }

    // ---- Storage removal (db.js 923-1006; table drops OUTSIDE any
    // transaction because PRAGMA foreign_keys is a no-op inside one) ----
    {
        let mig_cols = table_cols(conn, "collection")?;
        let has_placement = mig_cols
            .iter()
            .any(|c| c == "location_id" || c == "compartment_id" || c == "position");
        let storage_tables: Vec<String> = stmt_strings(
            conn,
            "SELECT name FROM sqlite_master
              WHERE type = 'table'
                AND name IN ('locations', 'compartments', 'compartment_assignments')",
        )?;
        if has_placement || !storage_tables.is_empty() {
            tracing::info!("Removing storage schema (locations, compartments, placement columns)...");
            let keep_list = mig_cols
                .iter()
                .filter(|c| *c != "location_id" && *c != "compartment_id" && *c != "position")
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");

            if !storage_tables.is_empty() {
                conn.execute_batch("PRAGMA foreign_keys = OFF")?;
                let r = conn.execute_batch(
                    "DROP TABLE IF EXISTS compartment_assignments;
                     DROP TABLE IF EXISTS compartments;
                     DROP TABLE IF EXISTS locations;",
                );
                // finally: always re-enable, even on a failed drop.
                conn.execute_batch("PRAGMA foreign_keys = ON")?;
                r?;
            }
            conn.execute_batch("DROP INDEX IF EXISTS idx_collection_comp_user_qty")?;
            conn.execute_batch("DROP INDEX IF EXISTS idx_collection_loc_pos")?;

            if has_placement {
                with_immediate_transaction(conn, |c| {
                    c.execute_batch("DROP TABLE IF EXISTS collection_new")?;
                    c.execute_batch(
                        "\n            CREATE TABLE collection_new (\n              id INTEGER PRIMARY KEY AUTOINCREMENT,\n              card_id TEXT NOT NULL,\n              quantity INTEGER DEFAULT 1,\n              condition TEXT CHECK(condition IN ('Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged')) DEFAULT 'Near Mint',\n              printing TEXT CHECK(printing IN ('Normal', 'Holofoil')) DEFAULT 'Normal',\n              language TEXT DEFAULT 'English',\n              purchase_price REAL,\n              favorite INTEGER DEFAULT 0,\n              is_trade INTEGER DEFAULT 0,\n              added_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n              notes TEXT DEFAULT '',\n              user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,\n              source TEXT,\n              is_proxy INTEGER DEFAULT 0,\n              FOREIGN KEY(card_id) REFERENCES card_cache(id)\n            )\n          ",
                    )?;
                    c.execute_batch(&format!(
                        "INSERT INTO collection_new ({k}) SELECT {k} FROM collection",
                        k = keep_list
                    ))?;
                    c.execute_batch("DROP TABLE collection")?;
                    c.execute_batch("ALTER TABLE collection_new RENAME TO collection")?;
                    Ok(())
                })?;
            }
            conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_collection_user ON collection(user_id)")?;
            tracing::info!("Storage schema removed; collection rows preserved.");
        }
    }

    // ---- notes table removal (db.js 1008-1017) ----
    if has_table(conn, "notes")? {
        tracing::info!("Removing notes table...");
        conn.execute_batch("DROP TABLE IF EXISTS notes")?;
        tracing::info!("Notes table removed.");
    }

    // ---- users ladder (db.js 1019-1056; one snapshot) ----
    let users_cols = table_cols(conn, "users")?;
    add_col_if_missing(conn, &users_cols, "users", "api_key", "TEXT")?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_users_api_key ON users(api_key) WHERE api_key IS NOT NULL",
    )?;
    for (col, ty) in [
        ("manapool_email", "TEXT"),
        ("manapool_token", "TEXT"),
        ("manapool_enabled", "INTEGER NOT NULL DEFAULT 0"),
        ("manapool_saved_at", "TEXT"),
        ("tcgplayer_cookies", "TEXT"),
        ("tcgplayer_enabled", "INTEGER NOT NULL DEFAULT 0"),
        ("tcgplayer_saved_at", "TEXT"),
    ] {
        add_col_if_missing(conn, &users_cols, "users", col, ty)?;
    }
    add_col_if_missing(conn, &users_cols, "users", "oidc_sub", "TEXT")?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_users_oidc_sub ON users(oidc_sub) WHERE oidc_sub IS NOT NULL",
    )?;

    // ---- deck_cards ladder (db.js 1058-1066) ----
    let deck_cards_cols = table_cols(conn, "deck_cards")?;
    add_col_if_missing(conn, &deck_cards_cols, "deck_cards", "checked_out", "INTEGER DEFAULT 0")?;
    add_col_if_missing(conn, &deck_cards_cols, "deck_cards", "is_commander", "INTEGER NOT NULL DEFAULT 0")?;

    // ---- Moxfield sync tables (db.js 1068-1149) ----
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS moxfield_authors (\n      id INTEGER PRIMARY KEY AUTOINCREMENT,\n      user_id INTEGER NOT NULL,\n      moxfield_user TEXT NOT NULL,\n      display_name TEXT,\n      profile_image_url TEXT,\n      last_decklist_sync_at DATETIME,\n      last_content_check_at DATETIME,\n      last_error TEXT,\n      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,\n      UNIQUE(user_id, moxfield_user)\n    )\n  ",
    )?;
    let mfx_author_cols = table_cols(conn, "moxfield_authors")?;
    add_col_if_missing(conn, &mfx_author_cols, "moxfield_authors", "last_content_check_at", "DATETIME")?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_mfx_author_user ON moxfield_authors(user_id)")?;
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS moxfield_decks (\n      id INTEGER PRIMARY KEY AUTOINCREMENT,\n      author_id INTEGER NOT NULL REFERENCES moxfield_authors(id) ON DELETE CASCADE,\n      public_id TEXT NOT NULL,\n      name TEXT NOT NULL,\n      format TEXT,\n      mainboard_count INTEGER,\n      sideboard_count INTEGER,\n      maybeboard_count INTEGER,\n      commander_count INTEGER,\n      last_updated_at TEXT,\n      last_synced_updated_at TEXT,\n      bindarr_deck_id INTEGER,\n      last_content_sync_at DATETIME,\n      last_error TEXT,\n      enabled INTEGER NOT NULL DEFAULT 1,\n      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      UNIQUE(author_id, public_id)\n    )\n  ",
    )?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_mfx_deck_author ON moxfield_decks(author_id)")?;
    let mfx_deck_cols = table_cols(conn, "moxfield_decks")?;
    add_col_if_missing(conn, &mfx_deck_cols, "moxfield_decks", "enabled", "INTEGER NOT NULL DEFAULT 1")?;
    let mfx_decks_cols = table_cols(conn, "decks")?;
    add_col_if_missing(conn, &mfx_decks_cols, "decks", "source", "TEXT DEFAULT 'manual'")?;
    add_col_if_missing(conn, &mfx_decks_cols, "decks", "moxfield_public_id", "TEXT")?;
    conn.execute_batch("DROP INDEX IF EXISTS idx_decks_mfx_public_id")?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_decks_user_mfx_public_id\n    ON decks(user_id, moxfield_public_id) WHERE moxfield_public_id IS NOT NULL",
    )?;

    // Moxfield cadence — NOTE: reuses the appSettingsCols snapshot read
    // BEFORE the moxfield tables existed (db.js 1154 reads the same
    // `appSettingsCols` const from line 442). Faithful: same snapshot.
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "moxfield_decklist_interval_min", "INTEGER NOT NULL DEFAULT 60")?;
    add_col_if_missing(conn, &app_settings_cols, "app_settings", "moxfield_content_interval_min", "INTEGER NOT NULL DEFAULT 1")?;

    let decks_cols = table_cols(conn, "decks")?;
    add_col_if_missing(conn, &decks_cols, "decks", "format", "TEXT DEFAULT 'Standard'")?;
    add_col_if_missing(conn, &decks_cols, "decks", "category", "TEXT DEFAULT 'Competitive'")?;
    add_col_if_missing(conn, &decks_cols, "decks", "accent_color", "TEXT DEFAULT '#eab308'")?;
    add_col_if_missing(conn, &decks_cols, "decks", "target_size", "INTEGER DEFAULT 60")?;

    // ---- revision counters + triggers (db.js 1175-1279) ----
    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS collection_snapshot (\n      id INTEGER PRIMARY KEY CHECK(id = 1),\n      revision INTEGER NOT NULL DEFAULT 0\n    )\n  ",
    )?;
    conn.execute("INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0)", [])?;
    for op in ["INSERT", "UPDATE", "DELETE"] {
        conn.execute_batch(&format!(
            "\n      CREATE TRIGGER IF NOT EXISTS collection_snapshot_after_{name}\n      AFTER {op} ON collection\n      BEGIN\n        UPDATE collection_snapshot SET revision = revision + 1 WHERE id = 1;\n      END\n    ",
            name = op.to_lowercase(),
            op = op
        ))?;
    }

    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS deck_list_revision (\n      id INTEGER PRIMARY KEY CHECK(id = 1),\n      revision INTEGER NOT NULL DEFAULT 0\n    )\n  ",
    )?;
    conn.execute("INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0)", [])?;
    const DECK_LIST_BUMP: &str = "UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1;";
    for table in ["decks", "deck_cards", "collection", "list_cards"] {
        for op in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!(
                "\n        CREATE TRIGGER IF NOT EXISTS deck_list_rev_{table}_{name}\n        AFTER {op} ON {table}\n        BEGIN {bump} END\n      ",
                name = op.to_lowercase(),
                op = op,
                bump = DECK_LIST_BUMP
            ))?;
        }
    }
    for op in ["INSERT", "DELETE"] {
        conn.execute_batch(&format!(
            "\n      CREATE TRIGGER IF NOT EXISTS deck_list_rev_card_cache_{name}\n      AFTER {op} ON card_cache\n      BEGIN {bump} END\n    ",
            name = op.to_lowercase(),
            op = op,
            bump = DECK_LIST_BUMP
        ))?;
    }
    conn.execute_batch(&format!(
        "\n    CREATE TRIGGER IF NOT EXISTS deck_list_rev_card_cache_update\n    AFTER UPDATE ON card_cache\n    WHEN OLD.price_trend IS NOT NEW.price_trend OR OLD.price_normal IS NOT NEW.price_normal\n      OR OLD.price_holofoil IS NOT NEW.price_holofoil OR OLD.price_etched IS NOT NEW.price_etched\n      OR OLD.price_currency IS NOT NEW.price_currency OR OLD.image_url IS NOT NEW.image_url\n      OR OLD.name IS NOT NEW.name OR OLD.oracle_id IS NOT NEW.oracle_id\n      OR OLD.color_identity IS NOT NEW.color_identity OR OLD.subtypes IS NOT NEW.subtypes\n    BEGIN {bump} END\n  ",
        bump = DECK_LIST_BUMP
    ))?;

    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS collection_read_revision (\n      id INTEGER PRIMARY KEY CHECK(id = 1),\n      revision INTEGER NOT NULL DEFAULT 0\n    )\n  ",
    )?;
    conn.execute("INSERT OR IGNORE INTO collection_read_revision (id, revision) VALUES (1, 0)", [])?;
    for table in ["collection", "card_cache"] {
        for op in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!(
                "\n        CREATE TRIGGER IF NOT EXISTS collection_read_rev_{table}_{name}\n        AFTER {op} ON {table}\n        BEGIN UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1; END\n      ",
                name = op.to_lowercase(),
                op = op
            ))?;
        }
    }

    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS stats_aux_revision (\n      id INTEGER PRIMARY KEY CHECK(id = 1),\n      revision INTEGER NOT NULL DEFAULT 0\n    )\n  ",
    )?;
    conn.execute("INSERT OR IGNORE INTO stats_aux_revision (id, revision) VALUES (1, 0)", [])?;
    for table in ["price_history", "sets"] {
        for op in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!(
                "\n        CREATE TRIGGER IF NOT EXISTS stats_aux_rev_{table}_{name}\n        AFTER {op} ON {table}\n        BEGIN UPDATE stats_aux_revision SET revision = revision + 1 WHERE id = 1; END\n      ",
                name = op.to_lowercase(),
                op = op
            ))?;
        }
    }

    conn.execute_batch(
        "\n    CREATE TABLE IF NOT EXISTS tcg_imported_orders (\n      user_id INTEGER NOT NULL,\n      order_number TEXT NOT NULL,\n      imported_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n      PRIMARY KEY (user_id, order_number)\n    )\n  ",
    )?;

    // ---- performance indexes (db.js 1292-1314) ----
    conn.execute_batch("DROP INDEX IF EXISTS idx_collection_user_game")?;
    conn.execute_batch("DROP INDEX IF EXISTS idx_collection_user")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_collection_user_added\n    ON collection(user_id, added_at DESC, id DESC)",
    )?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_card_cache_set_num ON card_cache(set_id, number)")?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_card_cache_oracle_id ON card_cache(oracle_id)")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_card_cache_logical_key ON card_cache(LOWER(TRIM(COALESCE(name, ''))))",
    )?;
    conn.execute_batch("DROP INDEX IF EXISTS idx_card_cache_logical_name")?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_deck_cards_checkout ON deck_cards(deck_id, checked_out)")?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_collection_card_user ON collection(card_id, user_id)")?;
    conn.execute_batch("DROP INDEX IF EXISTS idx_collection_tags_tag_id")?;
    conn.execute_batch("DROP INDEX IF EXISTS idx_audit_logs_user_date")?;

    // ---- boot-every tail: seed + orphan adoption + retired-data sweep ----
    run_boot_tail(conn, db_path, opts, &mut report)?;

    conn.execute_batch(&format!("PRAGMA user_version = {}", TARGET_USER_VERSION))?;
    Ok(report)
}

/// The three steps Node's initDb runs on EVERY boot, ladder or not: the
/// DEFAULT_ADMIN_PASSWORD re-seed check (db.js 1316-1331), orphan adoption
/// (db.js 1333-1349) and cardArt.cleanupRetiredData (db.js 1351-1354;
/// cardArt.js 109-151). Called both at the end of a full ladder pass and on
/// the user_version-gated skip, so pair-mode Rust keeps those every-boot
/// effects Node had.
fn run_boot_tail(
    conn: &Connection,
    db_path: &Path,
    opts: &MigOpts,
    report: &mut MigReport,
) -> rusqlite::Result<()> {
    // Statements Node's initDb issues on EVERY boot that are not pure
    // no-ops once the schema exists (found by the unit-4 line-by-line
    // review): the row upserts and the orphan price-point delete. Without
    // them a gated boot diverges from a Node boot (deleted settings row
    // never returns; price orphans created outside the ladder survive).
    conn.execute("INSERT OR IGNORE INTO app_settings (id, public_base_url) VALUES (1, '')", [])?;
    conn.execute(
        "DELETE FROM price_history WHERE card_id NOT IN (SELECT id FROM card_cache)",
        [],
    )?;
    conn.execute("INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0)", [])?;
    conn.execute("INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0)", [])?;
    conn.execute("INSERT OR IGNORE INTO collection_read_revision (id, revision) VALUES (1, 0)", [])?;
    conn.execute("INSERT OR IGNORE INTO stats_aux_revision (id, revision) VALUES (1, 0)", [])?;

    let user_count: i64 = conn.query_row("SELECT COUNT(*) as count FROM users", [], |r| r.get(0))?;
    let mut admin_id: Option<i64> = None;
    if user_count == 0 {
        if let Some(pw) = opts.admin_password.as_deref().filter(|s| !s.is_empty()) {
            let default_pass_hash = hash_password(pw);
            let default_share_token = random_hex(16);
            conn.execute(
                "INSERT INTO users (username, password_hash, role, share_token, share_enabled)
                 VALUES (?, ?, ?, ?, ?)",
                params!["admin", default_pass_hash, "admin", default_share_token, 0],
            )?;
            admin_id = Some(conn.last_insert_rowid());
            report.seeded_admin = true;
            tracing::info!("Created admin user \"admin\" from DEFAULT_ADMIN_PASSWORD.");
        } else {
            tracing::info!("No accounts yet. Open the web UI to create the owner account.");
        }
    } else {
        admin_id = conn
            .query_row("SELECT id FROM users WHERE username = ?", params!["admin"], |r| r.get(0))
            .ok();
    }
    if let Some(admin_id) = admin_id {
        adopt_orphan_rows(conn, admin_id)?;
    }

    let (removed_models, removed_art) = cleanup_retired_data(conn, db_path, opts)?;
    report.removed_models = removed_models;
    report.removed_art = removed_art;
    if removed_models > 0 || removed_art > 0 {
        // Node also invalidates cardArt's process-local listIds cache here;
        // Rust card_art has no process cache to poison (iter-3 note).
        tracing::info!(
            "Removed {} retired scan artifact(s) and {} orphan custom-art file(s).",
            removed_models,
            removed_art
        );
    }
    Ok(())
}

/// `adoptOrphanRows` (db.js 1363-1365).
pub fn adopt_orphan_rows(conn: &Connection, user_id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE collection SET user_id = ? WHERE user_id IS NULL", params![user_id])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// JS `String(v)` over a SQLite value (node-sqlite3 returns JS numbers for
/// INTEGER/REAL, so `String(r.id)` on a numeric id renders the number, and
/// null becomes "null"). Used by the cleanupRetiredData retained set.
fn js_string(v: rusqlite::types::Value) -> String {
    use rusqlite::types::Value;
    match v {
        Value::Text(s) => s,
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => {
            if f == f.trunc() && f.is_finite() && f.abs() < 9.007199254740992e15 {
                // JS String() of an integral double has no ".0"
                format!("{}", f as i64)
            } else {
                // Shortest round-trip form; matches JS String() for the
                // id space at hand (uuid strings never hit this arm).
                format!("{f}")
            }
        }
        Value::Null => "null".to_string(),
        Value::Blob(b) => String::from_utf8_lossy(&b).into_owned(),
    }
}

/// `withTransaction` from backend/src/db.js (BEGIN IMMEDIATE / COMMIT /
/// ROLLBACK on the same connection). Node's single-connection process makes
/// nesting impossible by construction; our migration path is likewise
/// single-connection and never nests.
fn with_immediate_transaction<F, T>(conn: &Connection, f: F) -> rusqlite::Result<T>
where
    F: FnOnce(&Connection) -> rusqlite::Result<T>,
{
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f(conn) {
        Ok(v) => {
            conn.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn table_cols(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?", params![name], |_r| Ok(()))
        .is_ok())
}

fn col_exists(conn: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    Ok(table_cols(conn, table)?.iter().any(|c| c == col))
}

fn add_col_if_missing(
    conn: &Connection,
    snapshot: &[String],
    table: &str,
    col: &str,
    def: &str,
) -> rusqlite::Result<()> {
    if !snapshot.iter().any(|c| c == col) {
        conn.execute_batch(&format!("ALTER TABLE {} ADD COLUMN {} {}", table, col, def))?;
    }
    Ok(())
}

fn stmt_strings(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// dropIndexesReferencing (db.js 733-743): only user-created indexes
/// (origin 'c') are droppable; autoindexes ('u'/'pk') are skipped.
fn drop_indexes_referencing(conn: &Connection, table: &str, cols: &[&str]) -> rusqlite::Result<()> {
    let target: std::collections::HashSet<&str> = cols.iter().copied().collect();
    let mut names = Vec::new();
    {
        let mut stmt = conn.prepare(&format!("PRAGMA index_list({})", table))?;
        let rows = stmt.query_map([], |r| {
            // PRAGMA index_list: seq(0) | name(1) | unique(2,INT) | origin(3) | partial(4,INT)
            Ok((r.get::<_, String>(1)?, r.get::<_, Option<String>>(3)?))
        })?;
        for row in rows {
            let (name, origin) = row?;
            if origin.as_deref() != Some("c") {
                continue;
            }
            let mut istmt = conn.prepare(&format!("PRAGMA index_info({})", name))?;
            let mut hit = false;
            let irows = istmt.query_map([], |r| r.get::<_, Option<String>>(2))?;
            for ir in irows {
                if let Some(cn) = ir? {
                    if target.contains(cn.as_str()) {
                        hit = true;
                    }
                }
            }
            if hit {
                names.push(name);
            }
        }
    }
    for name in names {
        conn.execute_batch(&format!("DROP INDEX {}", name))?;
    }
    Ok(())
}

/// dropCol (db.js 744-749).
fn drop_col(conn: &Connection, table: &str, col: &str) -> rusqlite::Result<()> {
    if col_exists(conn, table, col)? {
        drop_indexes_referencing(conn, table, &[col])?;
        conn.execute_batch(&format!("ALTER TABLE {} DROP COLUMN {}", table, col))?;
    }
    Ok(())
}

/// The Pokemon/grading removal transaction (db.js 723-921), verbatim order.
fn pokedex_removal(conn: &Connection) -> rusqlite::Result<()> {
    // ptcgo_code -> set_code (db.js 755-762).
    if col_exists(conn, "sets", "ptcgo_code")? {
        if col_exists(conn, "sets", "set_code")? {
            conn.execute_batch("UPDATE sets SET set_code = COALESCE(NULLIF(set_code, ''), ptcgo_code)")?;
            drop_col(conn, "sets", "ptcgo_code")?;
        } else {
            conn.execute_batch("ALTER TABLE sets RENAME COLUMN ptcgo_code TO set_code")?;
        }
    }

    // Children first: tcgplayer_product has an FK-named reference to
    // card_cache without ON DELETE (db.js 770-776).
    if has_table(conn, "tcgplayer_product")? {
        conn.execute_batch("DROP INDEX IF EXISTS idx_tcgplayer_product_pid")?;
        conn.execute_batch("DROP TABLE tcgplayer_product")?;
    }
    for table in ["collection", "card_cache", "decks", "card_lists", "sets"] {
        if col_exists(conn, table, "game")? {
            conn.execute_batch(&format!("DELETE FROM {} WHERE game = 'pokemon' OR game IS NULL", table))?;
        }
    }
    conn.execute_batch("DELETE FROM price_history WHERE card_id NOT IN (SELECT id FROM card_cache)")?;

    for table in ["sets", "card_cache", "collection", "decks", "card_lists"] {
        drop_col(conn, table, "game")?;
    }
    for col in ["market_value_source", "market_value_at", "market_value", "cert_number", "grade", "grader"] {
        drop_col(conn, "collection", col)?;
    }
    if has_table(conn, "psa_cert")? {
        conn.execute_batch("DROP TABLE psa_cert")?;
    }
    for col in ["price_reverse_holofoil", "price_1st_edition", "price_avg1", "price_avg7", "price_avg30"] {
        drop_col(conn, "card_cache", col)?;
    }

    // Rebuild an upgraded collection whose table-level CHECK still admits
    // retired finishes (db.js 811-859). The mtg CHECK is detected textually
    // (regex /CHECK\s*\(\s*printing\s+IN\s*\(\s*'Normal'\s*,\s*'Holofoil'\s*\)\s*\)/i).
    let collection_def: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'collection'",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    if !collection_def.as_deref().map(has_mtg_printing_check).unwrap_or(false) {
        let index_sql = stmt_strings(
            conn,
            "SELECT sql FROM sqlite_master
             WHERE type = 'index' AND tbl_name = 'collection' AND sql IS NOT NULL
             ORDER BY name",
        )?;
        conn.execute_batch("DROP TABLE IF EXISTS collection_new")?;
        conn.execute_batch(
            "\n          CREATE TABLE collection_new (\n            id INTEGER PRIMARY KEY AUTOINCREMENT,\n            card_id TEXT NOT NULL,\n            quantity INTEGER DEFAULT 1,\n            condition TEXT CHECK(condition IN ('Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged')) DEFAULT 'Near Mint',\n            printing TEXT CHECK(printing IN ('Normal', 'Holofoil')) DEFAULT 'Normal',\n            language TEXT DEFAULT 'English',\n            purchase_price REAL,\n            favorite INTEGER DEFAULT 0,\n            is_trade INTEGER DEFAULT 0,\n            list_type TEXT DEFAULT 'collection',\n            added_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n            notes TEXT DEFAULT '',\n            user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,\n            source TEXT,\n            is_proxy INTEGER DEFAULT 0,\n            FOREIGN KEY(card_id) REFERENCES card_cache(id)\n          )\n        ",
        )?;
        conn.execute_batch(
            "INSERT INTO collection_new
              (id, card_id, quantity, condition, printing, language, purchase_price,
               favorite, is_trade, list_type, added_at, notes, user_id, source, is_proxy)
            SELECT id, card_id, quantity, condition,
                   CASE WHEN printing = 'Holofoil' THEN 'Holofoil' ELSE 'Normal' END,
                   language, purchase_price, favorite, is_trade, list_type,
                   added_at, notes, user_id, source, COALESCE(is_proxy, 0)
            FROM collection",
        )?;
        conn.execute_batch("DROP TABLE collection")?;
        conn.execute_batch("ALTER TABLE collection_new RENAME TO collection")?;
        for sql in index_sql {
            conn.execute_batch(&sql)?;
        }
    }

    drop_col(conn, "users", "psa_api_token")?;
    drop_col(conn, "users", "graded_price_api_key")?;
    drop_col(conn, "users", "tcg_api_key")?;

    // app_settings Pokemon-era settings: plain ALTER DROP (no index clear —
    // faithful to db.js 870-875, which does NOT use dropCol here).
    for col in ["pokemon_provider", "pokemon_prices_swept_at", "tcgdex_prices_swept_at", "tcgcsv_prices_swept_at", "scan_exclude_digital"] {
        if col_exists(conn, "app_settings", col)? {
            conn.execute_batch(&format!("ALTER TABLE app_settings DROP COLUMN {}", col))?;
        }
    }

    // Wishlist removal (db.js 877-886).
    if col_exists(conn, "collection", "list_type")? {
        conn.execute_batch("DELETE FROM collection WHERE list_type <> 'collection'")?;
        drop_col(conn, "collection", "list_type")?;
    }

    // set_data_gaps key rebuild (db.js 888-911): copy only MTG rows, dynamic
    // column list from the live schema minus `game`.
    if has_table(conn, "set_data_gaps")? {
        let old_cols = table_cols(conn, "set_data_gaps")?;
        if old_cols.iter().any(|c| c == "game") {
            let keep = old_cols
                .iter()
                .filter(|n| n.as_str() != "game")
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            conn.execute_batch("DROP TABLE IF EXISTS set_data_gaps_new")?;
            conn.execute_batch(
                "CREATE TABLE set_data_gaps_new (\n            language TEXT NOT NULL,\n            set_id TEXT NOT NULL,\n            reason TEXT,\n            seen_at DATETIME DEFAULT CURRENT_TIMESTAMP,\n            PRIMARY KEY (language, set_id)\n          )",
            )?;
            conn.execute_batch(&format!(
                "INSERT INTO set_data_gaps_new ({k}) SELECT {k} FROM set_data_gaps WHERE game = 'mtg'",
                k = keep
            ))?;
            conn.execute_batch("DROP TABLE set_data_gaps")?;
            conn.execute_batch("ALTER TABLE set_data_gaps_new RENAME TO set_data_gaps")?;
        }
    }

    if has_table(conn, "tcgplayer_catalog")? {
        conn.execute_batch("DROP INDEX IF EXISTS idx_tcgplayer_catalog_pid")?;
        conn.execute_batch("DROP TABLE tcgplayer_catalog")?;
    }
    Ok(())
}

/// Textual twin of `/CHECK\s*\(\s*printing\s+IN\s*\(\s*'Normal'\s*,\s*'Holofoil'\s*\)\s*\)/i`.
/// Token-by-token scan over the lowercased SQL; `\s*` = any run of ASCII
/// whitespace between tokens (JS `\s` also matches NBSP etc. — those never
/// occur in table DDL this check ever sees; ASCII set is what both agree on).
pub fn has_mtg_printing_check(sql: &str) -> bool {
    let s = sql.to_lowercase();
    let b = s.as_bytes();
    let ws = |c: u8| c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' || c == 0x0b || c == 0x0c;
    let mut i = 0usize;
    'outer: while i < b.len() {
        // find "check"
        if !b[i..].starts_with(b"check") {
            i += 1;
            continue;
        }
        let mut j = i + 5;
        let tokens: [&[u8]; 10] =
            [b"(", b"printing", b"in", b"(", b"'normal'", b",", b"'holofoil'", b")", b")", b""];
        // \s* before each token, and the last token is just the trailing ws run
        for (k, tok) in tokens.iter().enumerate().take(9) {
            while j < b.len() && ws(b[j]) {
                j += 1;
            }
            if !b[j..].starts_with(tok) {
                i += 1; // advance PAST the failed 'check' candidate — without
                       // this, `continue 'outer` would re-match "check" at the
                       // same i forever (caught by the cargo golden tests).
                continue 'outer;
            }
            j += tok.len();
            let _ = k;
        }
        while j < b.len() && ws(b[j]) {
            j += 1;
        }
        return true;
    }
    false
}

/// cardArt.js `cleanupRetiredData` (lines 109-151): retired scan-catalog
/// names in MODEL_DIR + orphaned user PNGs in USER_DIR. Idempotent.
fn cleanup_retired_data(
    conn: &Connection,
    db_path: &Path,
    opts: &MigOpts,
) -> rusqlite::Result<(usize, usize)> {
    let mut removed_models = 0usize;
    let mut removed_art = 0usize;

    if let Some(model_dir) = &opts.model_dir {
        // `try { readdirSync } catch { absent = clean }` twin.
        let mut names: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(model_dir) {
            for e in rd.filter_map(|e| e.ok()) {
                // Node's fs.readdirSync lossily-decodes non-UTF-8 names;
                // they still take part in the sweep, so mirror that.
                names.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        for name in names {
            if !retired_catalog_name(&name) {
                continue;
            }
            let target = model_dir.join(&name);
            if std::fs::metadata(&target).map(|m| m.is_file()).unwrap_or(false) {
                if std::fs::remove_file(&target).is_ok() {
                    removed_models += 1;
                }
            }
        }
    }

    let user_dir = opts
        .user_art_dir
        .clone()
        .unwrap_or_else(|| db_path.parent().unwrap_or(Path::new(".")).join("card-art"));
    let mut retained: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id FROM card_cache\n    UNION\n    SELECT card_id AS id FROM collection\n  ",
        )?;
        // Node maps rows with `String(r.id)`: a TEXT-affinity column can
        // still hold INTEGER/REAL rows in SQLite, and String() renders them
        // like a JS number. get::<String> would hard-fail on those.
        let rows = stmt.query_map([], |r| r.get::<_, rusqlite::types::Value>(0))?;
        for r in rows {
            retained.insert(js_string(r?));
        }
    }
    if let Ok(rd) = std::fs::read_dir(&user_dir) {
        let mut names = Vec::new();
        for e in rd.filter_map(|e| e.ok()) {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        for name in names {
            if !name.to_lowercase().ends_with(".png") {
                continue;
            }
            let id = &name[..name.len() - 4];
            if retained.contains(id) {
                continue;
            }
            let target = user_dir.join(&name);
            if std::fs::metadata(&target).map(|m| m.is_file()).unwrap_or(false) {
                if std::fs::remove_file(&target).is_ok() {
                    removed_art += 1;
                }
            }
        }
    }
    Ok((removed_models, removed_art))
}

/// `/^milo-pokemon(?:-.*)?\.(?:npz|bin|json)(?:\.tmp)?$/i` twin.
pub fn retired_catalog_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    if !lower.starts_with("milo-pokemon") {
        return false;
    }
    for ext in [".npz", ".bin", ".json", ".npz.tmp", ".bin.tmp", ".json.tmp"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            // (?:-.*)? — stem is "milo-pokemon" or "milo-pokemon<anything>"
            // BUT must start right after the literal, i.e. stem is the
            // prefix optionally followed by '-' + anything.
            let rest = stem.strip_prefix("milo-pokemon").unwrap_or("");
            if rest.is_empty() || rest.starts_with('-') {
                return true;
            }
        }
    }
    false
}
