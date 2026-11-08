// Tagger oracle tags for one card, the way tagger.scryfall.com lists them:
// the tags put on the card directly, plus every ancestor tag those imply
// (inherited). oracle_tag_closure includes self rows, so ancestors are the
// closure rows whose descendant is a direct tag, minus the direct tags.
const db = require('../db');

async function cardOracleTags(cardId, fallbackOracleId = null, database = db) {
  const gen = await database.get(`SELECT id FROM oracle_tag_generations WHERE active = 1`);
  if (!gen) return { ready: false, oracle_id: null, tags: [], inherited: [] };

  const row = await database.get(`SELECT oracle_id FROM card_cache WHERE id = ?`, [cardId]);
  const oracleId = (row && row.oracle_id) || fallbackOracleId || null;
  if (!oracleId) return { ready: true, oracle_id: null, tags: [], inherited: [] };

  const tags = await database.all(`
    SELECT t.tag_id, t.slug, t.label, t.description
    FROM oracle_tag_assignments a
    JOIN oracle_tags t ON t.generation_id = a.generation_id AND t.tag_id = a.tag_id
    WHERE a.generation_id = ? AND a.oracle_id = ?
    ORDER BY t.slug
  `, [gen.id, oracleId]);

  const edges = await database.all(`
    SELECT anc.tag_id, anc.slug, anc.label, anc.description, d.slug AS via
    FROM oracle_tag_assignments a
    JOIN oracle_tag_closure c ON c.generation_id = a.generation_id AND c.descendant_tag_id = a.tag_id
    JOIN oracle_tags anc ON anc.generation_id = c.generation_id AND anc.tag_id = c.ancestor_tag_id
    JOIN oracle_tags d ON d.generation_id = a.generation_id AND d.tag_id = a.tag_id
    WHERE a.generation_id = ? AND a.oracle_id = ? AND c.ancestor_tag_id != c.descendant_tag_id
    ORDER BY anc.slug, d.slug
  `, [gen.id, oracleId]);

  const direct = new Set(tags.map(t => t.tag_id));
  const inheritedById = new Map();
  for (const e of edges) {
    if (direct.has(e.tag_id)) continue;
    if (!inheritedById.has(e.tag_id)) {
      inheritedById.set(e.tag_id, { tag_id: e.tag_id, slug: e.slug, label: e.label, description: e.description, via: [] });
    }
    const item = inheritedById.get(e.tag_id);
    if (!item.via.includes(e.via)) item.via.push(e.via);
  }
  const strip = ({ tag_id, ...rest }) => rest; // eslint-disable-line no-unused-vars
  return {
    ready: true,
    oracle_id: oracleId,
    tags: tags.map(strip),
    inherited: [...inheritedById.values()].map(strip),
  };
}

module.exports = { cardOracleTags };
