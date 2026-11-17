-- Extra tables the Scryfall job tests need (app_settings is created by
-- backend/src/db.js migrations in production).
CREATE TABLE IF NOT EXISTS app_settings (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  public_base_url TEXT DEFAULT '',
  oracle_tags_checked_at INTEGER,
  oracle_id_backfill_completed_at INTEGER,
  mtg_prices_swept_at DATETIME,
  price_refresh_days INTEGER NOT NULL DEFAULT 1,
  scan_exclude_tokens INTEGER NOT NULL DEFAULT 0,
  scan_exclude_art_cards INTEGER NOT NULL DEFAULT 0,
  scan_exclude_jumpstart INTEGER NOT NULL DEFAULT 0,
  scan_exclude_promos INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO app_settings (id) VALUES (1);
CREATE TABLE IF NOT EXISTS list_cards (
  list_id INTEGER NOT NULL,
  card_id TEXT NOT NULL,
  quantity INTEGER DEFAULT 1,
  PRIMARY KEY (list_id, card_id)
);
