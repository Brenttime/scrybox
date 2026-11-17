-- Oracle Tags closure tables (backend/src/oracleTags.js schema, subset the
-- /api/cards tag views read). tag_id is TEXT; generations.id INTEGER.
CREATE TABLE IF NOT EXISTS oracle_tag_generations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_updated_at TEXT,
  imported_at INTEGER,
  active INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS oracle_tags (
  generation_id INTEGER NOT NULL,
  tag_id TEXT NOT NULL,
  slug TEXT NOT NULL,
  label TEXT,
  description TEXT,
  PRIMARY KEY (generation_id, tag_id)
);
CREATE TABLE IF NOT EXISTS oracle_tag_assignments (
  generation_id INTEGER NOT NULL,
  tag_id TEXT NOT NULL,
  oracle_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ota_lookup ON oracle_tag_assignments(generation_id, oracle_id);
CREATE TABLE IF NOT EXISTS oracle_tag_closure (
  generation_id INTEGER NOT NULL,
  ancestor_tag_id TEXT NOT NULL,
  descendant_tag_id TEXT NOT NULL,
  PRIMARY KEY (generation_id, ancestor_tag_id, descendant_tag_id)
);
