-- Lane B unit 4: a legacy (pre-cleanup) Scrybox database — the shape the
-- Pokemon/grading/storage/wishlist/set_data_gaps migrations were written
-- against. Applied identically (byte-identical file) on the Node baseline
-- run and the Rust test run, then each side runs its initDb twin. All
-- timestamp columns are set explicitly so the dumps are deterministic.
PRAGMA foreign_keys=OFF;

CREATE TABLE users (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  username TEXT UNIQUE NOT NULL,
  password_hash TEXT NOT NULL,
  role TEXT CHECK(role IN ('admin', 'member')) NOT NULL DEFAULT 'member',
  share_token TEXT UNIQUE NOT NULL,
  share_enabled INTEGER DEFAULT 0,
  psa_api_token TEXT,
  graded_price_api_key TEXT,
  tcg_api_key TEXT,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, psa_api_token, graded_price_api_key, tcg_api_key, created_at)
  VALUES (1, 'admin', 'legacy-hash', 'admin', 'tok-admin', 1, 'PSA-1', 'GPA-1', 'TCG-1', '2024-01-01 00:00:00');

CREATE TABLE sessions (
  token TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL,
  expires_at DATETIME NOT NULL,
  FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
);
INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2030-01-01 00:00:00');

CREATE TABLE app_settings (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  public_base_url TEXT DEFAULT '',
  pokemon_provider TEXT DEFAULT 'pkm',
  pokemon_prices_swept_at DATETIME,
  tcgdex_prices_swept_at DATETIME,
  tcgcsv_prices_swept_at DATETIME,
  scan_exclude_digital INTEGER DEFAULT 0
);
INSERT INTO app_settings (id, public_base_url, pokemon_provider, pokemon_prices_swept_at, tcgdex_prices_swept_at, tcgcsv_prices_swept_at, scan_exclude_digital)
  VALUES (1, '', 'pkm', '2024-01-02 00:00:00', '2024-01-02 00:00:00', '2024-01-02 00:00:00', 1);

CREATE TABLE sets (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  series TEXT,
  printed_total INTEGER,
  total INTEGER,
  release_date TEXT,
  ptcgo_code TEXT,
  symbol_url TEXT,
  logo_url TEXT,
  game TEXT DEFAULT 'mtg'
);
INSERT INTO sets (id, name, series, printed_total, total, release_date, ptcgo_code, symbol_url, logo_url, game)
  VALUES ('10e', 'Tenth Edition', 'MTG', 383, 383, '2007-07-13', '10E', NULL, NULL, 'mtg');
INSERT INTO sets (id, name, series, printed_total, total, release_date, ptcgo_code, symbol_url, logo_url, game)
  VALUES ('me1', 'Master''s Edition 1', 'Pokemon', 78, 78, '2023-05-05', 'ME1', NULL, NULL, 'pokemon');
INSERT INTO sets (id, name, series, printed_total, total, release_date, ptcgo_code, symbol_url, logo_url, game)
  VALUES ('pset', 'Null Game Set', NULL, 1, 1, NULL, NULL, NULL, NULL, NULL);

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
  price_reverse_holofoil REAL,
  price_1st_edition REAL,
  price_avg1 REAL,
  price_avg7 REAL,
  price_avg30 REAL,
  cmc REAL,
  color_identity TEXT,
  tcgplayer_url TEXT,
  cardmarket_url TEXT,
  last_updated DATETIME DEFAULT CURRENT_TIMESTAMP,
  game TEXT
);
INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, image_url, price_trend, price_normal, price_holofoil, price_reverse_holofoil, price_1st_edition, price_avg1, price_avg7, price_avg30, cmc, color_identity, tcgplayer_url, cardmarket_url, last_updated, game)
  VALUES ('mtg-a', 'Card Alpha', 'Legendary Creature', 'Goblin', 'Creature', 'Rare', '10e', 'Tenth Edition', '1', 'https://img/a', 0.5, 1.25, 2.5, 3.0, 4.0, 1.1, 1.2, 1.3, 3.0, 'R', 'https://www.tcgplayer.com/product/12345/card%2Fproduct%2F98765%3Fpage%3D1', 'https://cardmarket/a', '2024-02-01 00:00:00', 'mtg');
INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, image_url, price_trend, price_normal, price_holofoil, price_reverse_holofoil, price_1st_edition, price_avg1, price_avg7, price_avg30, cmc, color_identity, tcgplayer_url, cardmarket_url, last_updated, game)
  VALUES ('mtg-b', 'Card Beta', 'Instant', NULL, 'Instant', 'Common', '10e', 'Tenth Edition', '2', 'https://img/b', 0.0, 0.10, NULL, NULL, NULL, 0.1, 0.1, 0.1, 1.0, 'U', '/product/67890foo', NULL, '2024-02-01 00:00:00', 'mtg');
INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, image_url, price_trend, price_normal, price_holofoil, price_reverse_holofoil, price_1st_edition, price_avg1, price_avg7, price_avg30, cmc, color_identity, tcgplayer_url, cardmarket_url, last_updated, game)
  VALUES ('mtg-c', 'Card Gamma', 'Land', 'Forest', 'Land', 'Common', '10e', 'Tenth Edition', '3', 'https://img/c', 0.0, 0.05, NULL, NULL, NULL, 0.05, 0.05, 0.05, 0.0, 'G', 'https://www.tcgplayer.com/search/mtg/alpha', NULL, '2024-02-01 00:00:00', 'mtg');
INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, image_url, price_trend, price_normal, price_holofoil, price_reverse_holofoil, price_1st_edition, price_avg1, price_avg7, price_avg30, cmc, color_identity, tcgplayer_url, cardmarket_url, last_updated, game)
  VALUES ('mtg-d', 'Card Delta', NULL, NULL, NULL, NULL, '10e', 'Tenth Edition', '4', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, '2024-02-01 00:00:00', 'mtg');
INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, image_url, price_trend, price_normal, price_holofoil, price_reverse_holofoil, price_1st_edition, price_avg1, price_avg7, price_avg30, cmc, color_identity, tcgplayer_url, cardmarket_url, last_updated, game)
  VALUES ('pkm-1', 'Pikachu', NULL, NULL, NULL, NULL, 'me1', 'Master''s Edition 1', '1', NULL, NULL, 9.99, NULL, NULL, NULL, 9.9, 9.9, 9.9, NULL, NULL, NULL, NULL, '2024-02-01 00:00:00', 'pokemon');
INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, image_url, price_trend, price_normal, price_holofoil, price_reverse_holofoil, price_1st_edition, price_avg1, price_avg7, price_avg30, cmc, color_identity, tcgplayer_url, cardmarket_url, last_updated, game)
  VALUES ('pkm-null', 'Gameless Card', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, '2024-02-01 00:00:00', NULL);

CREATE TABLE price_history (
  card_id TEXT NOT NULL,
  price REAL NOT NULL,
  recorded_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(card_id, recorded_at)
);
INSERT INTO price_history (card_id, price, recorded_at) VALUES ('mtg-a', 1.25, '2024-02-01 00:00:00');
INSERT INTO price_history (card_id, price, recorded_at) VALUES ('mtg-a', 1.50, '2024-02-02 00:00:00');
INSERT INTO price_history (card_id, price, recorded_at) VALUES ('pkm-1', 9.99, '2024-02-01 00:00:00');
INSERT INTO price_history (card_id, price, recorded_at) VALUES ('ghost', 5.00, '2024-02-01 00:00:00');

CREATE TABLE set_data_gaps (
  game TEXT NOT NULL,
  language TEXT,
  set_id TEXT,
  reason TEXT,
  seen_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (game, language, set_id)
);
INSERT INTO set_data_gaps (game, language, set_id, reason, seen_at) VALUES ('mtg', 'English', '10e', '404', '2024-02-01 00:00:00');
INSERT INTO set_data_gaps (game, language, set_id, reason, seen_at) VALUES ('pokemon', 'English', 'me1', '404', '2024-02-01 00:00:00');

CREATE TABLE locations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  sort_order INTEGER,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO locations (id, user_id, name, sort_order, created_at) VALUES (1, 1, 'Office', 1, '2024-01-05 00:00:00');

CREATE TABLE compartments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  location_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  color TEXT,
  sort_order INTEGER,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY(location_id) REFERENCES locations(id) ON DELETE CASCADE
);
INSERT INTO compartments (id, location_id, name, color, sort_order, created_at) VALUES (1, 1, 'Drawer 1', '#fff', 1, '2024-01-05 00:00:00');

CREATE TABLE compartment_assignments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL,
  compartment_id INTEGER NOT NULL,
  label TEXT,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO compartment_assignments (id, user_id, compartment_id, label, created_at) VALUES (1, 1, 1, 'box', '2024-01-05 00:00:00');

-- Legacy collection: retired printing CHECK, list_type, grading columns,
-- game. NOTE: no storage placement columns here — the ancient CHECK-era
-- table predates storage (mixing them is a state real installs never had;
-- the storage-block path is covered by legacy2_schema.sql). The storage
-- tables themselves still exist so the storage block's table-drop branch
-- fires.
CREATE TABLE collection (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  card_id TEXT NOT NULL,
  quantity INTEGER DEFAULT 1,
  condition TEXT CHECK(condition IN ('Near Mint', 'Lightly Played', 'Moderately Played', 'Heavily Played', 'Damaged')) DEFAULT 'Near Mint',
  printing TEXT CHECK(printing IN ('Normal', 'Holofoil', 'Reverse Holofoil', '1st Edition')) DEFAULT 'Normal',
  language TEXT DEFAULT 'English',
  purchase_price REAL,
  favorite INTEGER DEFAULT 0,
  is_trade INTEGER DEFAULT 0,
  list_type TEXT DEFAULT 'collection',
  notes TEXT DEFAULT '',
  user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
  game TEXT,
  grader TEXT,
  grade TEXT,
  cert_number TEXT,
  market_value REAL,
  market_value_source TEXT,
  market_value_at DATETIME,
  source TEXT,
  is_proxy INTEGER DEFAULT 0,
  added_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY(card_id) REFERENCES card_cache(id)
);
CREATE INDEX idx_collection_game ON collection(game);
CREATE INDEX idx_collection_user ON collection(user_id);
INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price, favorite, is_trade, list_type, notes, user_id, game, grader, grade, cert_number, market_value, market_value_source, market_value_at, source, is_proxy, added_at)
  VALUES (1, 'mtg-a', 1, 'Near Mint', 'Reverse Holofoil', 'English', 1.00, 0, 0, 'collection', 'first', NULL, 'mtg', 'PSA', '10', '12345678', 2.5, 'tcgplayer', '2024-02-03 00:00:00', NULL, 0, '2024-02-03 00:00:00');
INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price, favorite, is_trade, list_type, notes, user_id, game, grader, grade, cert_number, market_value, market_value_source, market_value_at, source, is_proxy, added_at)
  VALUES (2, 'mtg-b', 2, 'Lightly Played', 'Holofoil', 'English', 0.10, 1, 0, 'wishlist', 'wish', 1, 'mtg', NULL, NULL, NULL, NULL, NULL, NULL, 'manabox', 0, '2024-02-03 00:00:00');
INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price, favorite, is_trade, list_type, notes, user_id, game, grader, grade, cert_number, market_value, market_value_source, market_value_at, source, is_proxy, added_at)
  VALUES (3, 'pkm-1', 1, 'Near Mint', 'Normal', 'English', 9.99, 0, 1, 'collection', '', 1, 'pokemon', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, '2024-02-03 00:00:00');
INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price, favorite, is_trade, list_type, notes, user_id, game, grader, grade, cert_number, market_value, market_value_source, market_value_at, source, is_proxy, added_at)
  VALUES (4, 'mtg-c', 1, 'Damaged', 'Normal', 'English', NULL, 0, 0, 'collection', '', NULL, 'mtg', NULL, NULL, NULL, NULL, NULL, NULL, 'manabox', 1, '2024-02-03 00:00:00');

CREATE TABLE decks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  checked_out INTEGER DEFAULT 0,
  checked_out_at DATETIME,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  game TEXT DEFAULT 'mtg',
  FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
);
INSERT INTO decks (id, user_id, name, description, checked_out, checked_out_at, created_at, game)
  VALUES (1, 1, 'Deck A', 'mtg deck', 0, NULL, '2024-01-10 00:00:00', 'mtg');
INSERT INTO decks (id, user_id, name, description, checked_out, checked_out_at, created_at, game)
  VALUES (2, 1, 'PK Deck', 'pkm deck', 0, NULL, '2024-01-10 00:00:00', 'pokemon');

CREATE TABLE deck_cards (
  deck_id INTEGER NOT NULL,
  card_id TEXT NOT NULL,
  quantity INTEGER DEFAULT 1,
  PRIMARY KEY(deck_id, card_id),
  FOREIGN KEY(deck_id) REFERENCES decks(id) ON DELETE CASCADE
);
INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (1, 'mtg-a', 4);

CREATE TABLE card_lists (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  accent_color TEXT DEFAULT '#10b981',
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  game TEXT,
  FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
);
INSERT INTO card_lists (id, user_id, name, description, accent_color, created_at, game)
  VALUES (1, 1, 'Binder List', 'mtg list', '#10b981', '2024-01-11 00:00:00', 'mtg');
INSERT INTO card_lists (id, user_id, name, description, accent_color, created_at, game)
  VALUES (2, 1, 'PK List', 'pkm list', '#10b981', '2024-01-11 00:00:00', 'pokemon');

CREATE TABLE list_cards (
  list_id INTEGER NOT NULL,
  card_id TEXT NOT NULL,
  quantity INTEGER DEFAULT 1,
  PRIMARY KEY(list_id, card_id),
  FOREIGN KEY(list_id) REFERENCES card_lists(id) ON DELETE CASCADE
);
INSERT INTO list_cards (list_id, card_id, quantity) VALUES (1, 'mtg-a', 2);

CREATE TABLE notes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL,
  content TEXT,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO notes (id, user_id, content, created_at) VALUES (1, 1, 'legacy note', '2024-01-12 00:00:00');

CREATE TABLE psa_cert (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  collection_id INTEGER,
  cert_number TEXT,
  grade TEXT,
  verified_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO psa_cert (id, collection_id, cert_number, grade, verified_at) VALUES (1, 1, '12345678', '10', '2024-01-13 00:00:00');

CREATE TABLE tcgplayer_product (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tcgplayer_id INTEGER,
  name TEXT,
  price REAL,
  synced_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_tcgplayer_product_pid ON tcgplayer_product(tcgplayer_id);
INSERT INTO tcgplayer_product (id, tcgplayer_id, name, price, synced_at) VALUES (1, 12345, 'Card Alpha', 1.25, '2024-01-14 00:00:00');

CREATE TABLE tcgplayer_catalog (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tcgplayer_id INTEGER,
  name TEXT,
  price REAL,
  synced_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_tcgplayer_catalog_pid ON tcgplayer_catalog(tcgplayer_id);
INSERT INTO tcgplayer_catalog (id, tcgplayer_id, name, price, synced_at) VALUES (1, 67890, 'Card Beta', 0.1, '2024-01-14 00:00:00');

-- v1 collection_query_cache: no completeness columns.
CREATE TABLE collection_query_cache (
  query TEXT NOT NULL,
  lang TEXT NOT NULL DEFAULT '',
  names TEXT NOT NULL,
  resolved_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  PRIMARY KEY (query, lang)
);
INSERT INTO collection_query_cache (query, lang, names, resolved_at, expires_at) VALUES ('alpha', 'en', '["Card Alpha"]', 1700000000, 1700003600);

-- Retired feature stubs whose indexes the migration drops unconditionally.
CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);
CREATE TABLE audit_logs (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER, action TEXT, created_at DATETIME DEFAULT CURRENT_TIMESTAMP);
CREATE INDEX idx_collection_tags_tag_id ON tags(id);
CREATE INDEX idx_audit_logs_user_date ON audit_logs(user_id, created_at);
