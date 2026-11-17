-- Lane B unit 3: the tables the Moxfield sync engine touches, DDL copied
-- verbatim from backend/src/db.js (initDb + the ALTER TABLE migrations that
-- add the later columns). Load after schema_fixture.sql + scry_schema.sql.

-- db.js: maybeboard lives OUT of deck_cards on purpose (totals/stats/value/
-- checkout allocation must never count it).
CREATE TABLE IF NOT EXISTS deck_maybe_cards (
  deck_id INTEGER NOT NULL,
  card_id TEXT NOT NULL,
  quantity INTEGER DEFAULT 1,
  PRIMARY KEY(deck_id, card_id),
  FOREIGN KEY(deck_id) REFERENCES decks(id) ON DELETE CASCADE
);

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
);
CREATE INDEX IF NOT EXISTS idx_mfx_author_user ON moxfield_authors(user_id);

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
);
CREATE INDEX IF NOT EXISTS idx_mfx_deck_author ON moxfield_decks(author_id);

-- The two instance-wide cadences (db.js ALTERs; scry_schema.sql created the
-- base app_settings row).
ALTER TABLE app_settings ADD COLUMN moxfield_decklist_interval_min INTEGER NOT NULL DEFAULT 60;
ALTER TABLE app_settings ADD COLUMN moxfield_content_interval_min INTEGER NOT NULL DEFAULT 1;

-- decks.source / moxfield_public_id / format / target_size are already
-- inline in schema_fixture.sql (lane A seeded them from db.js ALTERs).
