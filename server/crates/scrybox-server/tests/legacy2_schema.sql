-- Lane B unit 4, variant 2: the TRANSITIONAL schema — collection already
-- rebuilt to the MTG printing CHECK (so the pokedex-block rebuild is
-- skipped) but still carrying list_type and storage placement columns while
-- the storage tables themselves are already gone. Exercises the storage
-- block's collection_new rebuild + idx_collection_user re-creation.
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
  VALUES (1, 'admin', 'legacy-hash-2', 'admin', 'tok-admin-2', 0, NULL, NULL, NULL, '2024-03-01 00:00:00');

CREATE TABLE app_settings (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  public_base_url TEXT DEFAULT ''
);
INSERT INTO app_settings (id, public_base_url) VALUES (1, '');

CREATE TABLE sets (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  series TEXT,
  printed_total INTEGER,
  total INTEGER,
  release_date TEXT,
  set_code TEXT,
  symbol_url TEXT,
  logo_url TEXT,
  game TEXT DEFAULT 'mtg'
);
INSERT INTO sets (id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url, game)
  VALUES ('domu', 'Dominaria United', 'MTG', 281, 281, '2022-09-09', 'DMU', NULL, NULL, 'mtg');
INSERT INTO sets (id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url, game)
  VALUES ('p1', 'Pokemon Leftovers', 'Pokemon', 1, 1, NULL, 'P1', NULL, NULL, 'pokemon');

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
  VALUES ('mtg-x', 'Transitional Card', 'Creature', 'Cat', 'Creature', 'Uncommon', 'domu', 'Dominaria United', '7', 'https://img/x', 0.1, 0.42, 0.9, NULL, NULL, 0.4, 0.4, 0.4, 2.0, 'W', 'https://www.tcgplayer.com/product/555/ transitional', NULL, '2024-03-02 00:00:00', 'mtg');
INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, image_url, price_trend, price_normal, price_holofoil, price_reverse_holofoil, price_1st_edition, price_avg1, price_avg7, price_avg30, cmc, color_identity, tcgplayer_url, cardmarket_url, last_updated, game)
  VALUES ('pkm-x', 'Pokemon Leftover', NULL, NULL, NULL, NULL, 'p1', 'Pokemon Leftovers', '1', NULL, NULL, 1.0, NULL, NULL, NULL, 1.0, 1.0, 1.0, NULL, NULL, NULL, NULL, '2024-03-02 00:00:00', 'pokemon');

CREATE TABLE price_history (
  card_id TEXT NOT NULL,
  price REAL NOT NULL,
  recorded_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(card_id, recorded_at)
);
INSERT INTO price_history (card_id, price, recorded_at) VALUES ('mtg-x', 0.42, '2024-03-02 00:00:00');
INSERT INTO price_history (card_id, price, recorded_at) VALUES ('pkm-x', 1.0, '2024-03-02 00:00:00');

CREATE TABLE collection (
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
  location_id INTEGER,
  compartment_id INTEGER,
  position INTEGER,
  notes TEXT DEFAULT '',
  user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
  game TEXT,
  source TEXT,
  is_proxy INTEGER DEFAULT 0,
  added_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY(card_id) REFERENCES card_cache(id)
);
CREATE INDEX idx_collection_comp_user_qty ON collection(user_id, compartment_id, quantity);
CREATE INDEX idx_collection_loc_pos ON collection(location_id, position);
CREATE INDEX idx_collection_keep_me ON collection(notes);
INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price, favorite, is_trade, list_type, location_id, compartment_id, position, notes, user_id, game, source, is_proxy, added_at)
  VALUES (1, 'mtg-x', 3, 'Near Mint', 'Holofoil', 'English', 0.90, 0, 0, 'collection', 7, 2, 5, 'placed', NULL, 'mtg', NULL, 0, '2024-03-03 00:00:00');
INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price, favorite, is_trade, list_type, location_id, compartment_id, position, notes, user_id, game, source, is_proxy, added_at)
  VALUES (2, 'mtg-x', 1, 'Near Mint', 'Normal', 'English', NULL, 0, 0, 'wishlist', NULL, NULL, NULL, 'no', 1, 'mtg', 'manabox', 0, '2024-03-03 00:00:00');

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
  VALUES (1, 1, 'Trans Deck', NULL, 0, NULL, '2024-03-04 00:00:00', 'mtg');

CREATE TABLE deck_cards (
  deck_id INTEGER NOT NULL,
  card_id TEXT NOT NULL,
  quantity INTEGER DEFAULT 1,
  PRIMARY KEY(deck_id, card_id),
  FOREIGN KEY(deck_id) REFERENCES decks(id) ON DELETE CASCADE
);
INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (1, 'mtg-x', 4);

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
  VALUES (1, 1, 'Trans List', NULL, '#10b981', '2024-03-04 00:00:00', 'mtg');

CREATE TABLE list_cards (
  list_id INTEGER NOT NULL,
  card_id TEXT NOT NULL,
  quantity INTEGER DEFAULT 1,
  PRIMARY KEY(list_id, card_id),
  FOREIGN KEY(list_id) REFERENCES card_lists(id) ON DELETE CASCADE
);
INSERT INTO list_cards (list_id, card_id, quantity) VALUES (1, 'mtg-x', 1);
