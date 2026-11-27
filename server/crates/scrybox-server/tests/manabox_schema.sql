-- Lane B unit 3b fixture: card_lists (+ source from db.js's ALTER), list_cards
-- and manabox_sync_runs, originally transcribed from backend/src/db.js (since
-- removed with the Node backend). tests/manabox_sync.rs::
-- manabox_schema_fixture_shape pins the column set against future edits.
CREATE TABLE card_lists (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      user_id INTEGER NOT NULL,
      name TEXT NOT NULL,
      description TEXT,
      accent_color TEXT DEFAULT '#10b981',
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      source TEXT,
      FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
    );
CREATE TABLE list_cards (
      list_id INTEGER NOT NULL,
      card_id TEXT NOT NULL,
      quantity INTEGER DEFAULT 1,
      PRIMARY KEY(list_id, card_id),
      FOREIGN KEY(list_id) REFERENCES card_lists(id) ON DELETE CASCADE
    );
CREATE TABLE manabox_sync_runs (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
      summary TEXT NOT NULL,
      report TEXT NOT NULL
    );
