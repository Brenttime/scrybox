CREATE TABLE users (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      username TEXT UNIQUE NOT NULL,
      password_hash TEXT NOT NULL,
      role TEXT CHECK(role IN ('admin', 'member')) NOT NULL DEFAULT 'member',
      share_token TEXT UNIQUE NOT NULL,
      share_enabled INTEGER DEFAULT 0,
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP
    , share_locations INTEGER DEFAULT 0, api_key TEXT, manapool_email TEXT, manapool_token TEXT, manapool_enabled INTEGER NOT NULL DEFAULT 0, manapool_saved_at TEXT, tcgplayer_cookies TEXT, tcgplayer_enabled INTEGER NOT NULL DEFAULT 0, tcgplayer_saved_at TEXT, oidc_sub TEXT);
CREATE TABLE sessions (
      token TEXT PRIMARY KEY,
      user_id INTEGER NOT NULL,
      expires_at DATETIME NOT NULL,
      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
    );
CREATE TABLE card_cache (
      id TEXT PRIMARY KEY,
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
      price_currency TEXT DEFAULT 'USD',
      price_source TEXT,
      cmc REAL,
      color_identity TEXT,
      language TEXT DEFAULT 'English',
      printed_name TEXT,
      tcgplayer_product_id INTEGER,
      last_updated DATETIME DEFAULT CURRENT_TIMESTAMP
    , tcgplayer_url TEXT, cardmarket_url TEXT, price_etched REAL, oracle_id TEXT, scryfall_search_eligible INTEGER
      CHECK(scryfall_search_eligible IN (0, 1)), oracle_text TEXT, oracle_faces TEXT, price_estimated INTEGER NOT NULL DEFAULT 0);
CREATE TABLE decks (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      user_id INTEGER NOT NULL,
      name TEXT NOT NULL,
      description TEXT,
      checked_out INTEGER DEFAULT 0,
      checked_out_at DATETIME,
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP, format TEXT DEFAULT 'Standard', category TEXT DEFAULT 'Competitive', accent_color TEXT DEFAULT '#eab308', target_size INTEGER DEFAULT 60, source TEXT DEFAULT 'manual', moxfield_public_id TEXT,
      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
    );
CREATE TABLE deck_cards (
      deck_id INTEGER NOT NULL,
      card_id TEXT NOT NULL,
      quantity INTEGER DEFAULT 1,
      checked_out INTEGER DEFAULT 0, is_commander INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY(deck_id, card_id),
      FOREIGN KEY(deck_id) REFERENCES decks(id) ON DELETE CASCADE
    );
CREATE TABLE "collection" (
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
            user_id INTEGER REFERENCES users(id) ON DELETE CASCADE, source TEXT, is_proxy INTEGER DEFAULT 0,
            FOREIGN KEY(card_id) REFERENCES card_cache(id)
          );
-- /stats auxiliary tables (backend/src/db.js): price_history backs the
-- 7d/30d value-change subqueries, sets supplies set sizes for setProgress.
CREATE TABLE IF NOT EXISTS price_history (
  card_id TEXT NOT NULL,
  price REAL NOT NULL,
  recorded_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(card_id, recorded_at)
);

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
);

CREATE TABLE IF NOT EXISTS collection_read_revision (
  id INTEGER PRIMARY KEY CHECK(id = 1),
  revision INTEGER NOT NULL DEFAULT 0
);

-- statsCache.dbRevision's third counter; db.js attaches stats_aux_* triggers
-- on every write to price_history/sets — mirrored so a Rust-side or test-side
-- write invalidates the stats cache the same way a live one would.
CREATE TABLE IF NOT EXISTS deck_list_revision (
  id INTEGER PRIMARY KEY CHECK(id = 1),
  revision INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS stats_aux_revision (
  id INTEGER PRIMARY KEY CHECK(id = 1),
  revision INTEGER NOT NULL DEFAULT 0
);

CREATE TRIGGER IF NOT EXISTS stats_aux_rev_price_history_insert AFTER INSERT ON price_history
BEGIN UPDATE stats_aux_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS stats_aux_rev_price_history_update AFTER UPDATE ON price_history
BEGIN UPDATE stats_aux_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS stats_aux_rev_price_history_delete AFTER DELETE ON price_history
BEGIN UPDATE stats_aux_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS stats_aux_rev_sets_insert AFTER INSERT ON sets
BEGIN UPDATE stats_aux_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS stats_aux_rev_sets_update AFTER UPDATE ON sets
BEGIN UPDATE stats_aux_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS stats_aux_rev_sets_delete AFTER DELETE ON sets
BEGIN UPDATE stats_aux_revision SET revision = revision + 1; END;
CREATE UNIQUE INDEX idx_users_api_key ON users(api_key) WHERE api_key IS NOT NULL;
CREATE INDEX idx_card_cache_set_num ON card_cache(set_id, number);
CREATE INDEX idx_deck_cards_checkout ON deck_cards(deck_id, checked_out);
CREATE UNIQUE INDEX idx_decks_user_mfx_public_id
    ON decks(user_id, moxfield_public_id) WHERE moxfield_public_id IS NOT NULL;
CREATE INDEX idx_card_cache_logical_key ON card_cache(LOWER(TRIM(COALESCE(name, ''))));
CREATE INDEX idx_collection_user_added
    ON collection(user_id, added_at DESC, id DESC);
CREATE INDEX idx_card_cache_oracle_id ON card_cache(oracle_id);
CREATE UNIQUE INDEX idx_users_oidc_sub ON users(oidc_sub) WHERE oidc_sub IS NOT NULL;
CREATE INDEX idx_collection_card_user ON collection(card_id, user_id);
CREATE TRIGGER collection_snapshot_after_insert
      AFTER INSERT ON collection
      BEGIN
        UPDATE collection_snapshot SET revision = revision + 1 WHERE id = 1;
      END;
CREATE TRIGGER collection_snapshot_after_update
      AFTER UPDATE ON collection
      BEGIN
        UPDATE collection_snapshot SET revision = revision + 1 WHERE id = 1;
      END;
CREATE TRIGGER collection_snapshot_after_delete
      AFTER DELETE ON collection
      BEGIN
        UPDATE collection_snapshot SET revision = revision + 1 WHERE id = 1;
      END;
CREATE TRIGGER deck_list_rev_decks_insert
        AFTER INSERT ON decks
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_decks_update
        AFTER UPDATE ON decks
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_decks_delete
        AFTER DELETE ON decks
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_deck_cards_insert
        AFTER INSERT ON deck_cards
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_deck_cards_update
        AFTER UPDATE ON deck_cards
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_deck_cards_delete
        AFTER DELETE ON deck_cards
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_collection_insert
        AFTER INSERT ON collection
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_collection_update
        AFTER UPDATE ON collection
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_collection_delete
        AFTER DELETE ON collection
        BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_card_cache_insert
      AFTER INSERT ON card_cache
      BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_card_cache_delete
      AFTER DELETE ON card_cache
      BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER deck_list_rev_card_cache_update
    AFTER UPDATE ON card_cache
    WHEN OLD.price_trend IS NOT NEW.price_trend OR OLD.price_normal IS NOT NEW.price_normal
      OR OLD.price_holofoil IS NOT NEW.price_holofoil OR OLD.price_etched IS NOT NEW.price_etched
      OR OLD.price_currency IS NOT NEW.price_currency OR OLD.image_url IS NOT NEW.image_url
      OR OLD.name IS NOT NEW.name OR OLD.oracle_id IS NOT NEW.oracle_id
      OR OLD.color_identity IS NOT NEW.color_identity OR OLD.subtypes IS NOT NEW.subtypes
    BEGIN UPDATE deck_list_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER collection_read_rev_collection_insert
        AFTER INSERT ON collection
        BEGIN UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER collection_read_rev_collection_update
        AFTER UPDATE ON collection
        BEGIN UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER collection_read_rev_collection_delete
        AFTER DELETE ON collection
        BEGIN UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER collection_read_rev_card_cache_insert
        AFTER INSERT ON card_cache
        BEGIN UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER collection_read_rev_card_cache_update
        AFTER UPDATE ON card_cache
        BEGIN UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1; END;
CREATE TRIGGER collection_read_rev_card_cache_delete
        AFTER DELETE ON card_cache
        BEGIN UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1; END;