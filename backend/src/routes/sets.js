const express = require('express');
const db = require('../db');
const router = express.Router();

router.get('/', async (req, res) => {
  try {
    const { lang } = req.query;
    void lang;
    // The set list changes only when Scryfall releases a set (refreshed every
    // few hours server-side); let the browser reuse it for an hour instead of
    // revalidating ~285 KB on every view. private: responses sit behind auth.
    res.set('Cache-Control', 'private, max-age=3600');
    const sets = await db.all(`
      SELECT id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url
      FROM sets
      ORDER BY release_date ASC
    `);
    // ?tree=1: parents only, each carrying its subsets. A Scryfall release is one
    // parent expansion plus a handful of child sets (tokens, art series, promos,
    // Commander decks) that each have their OWN set code, so a scan scoped to
    // "fdn" silently excludes every token in the box. The scanner's set filter
    // needs the family to offer them, and needs them separable so a user who is
    // not feeding tokens in can drop them. Opt-in because every other caller
    // wants the flat list it has always had.
    if (req.query.tree === '1') {
      const { getMtgChildSetMap } = require('../cardSets');
      const childMap = await getMtgChildSetMap();
      const childCodes = new Set([...childMap.values()].flatMap(cs => cs.map(c => c.code)));
      const code = (s) => s.set_code || String(s.id || '').replace(/^mtg-/, '');
      return res.json(
        sets
          .filter(s => !childCodes.has(code(s)))
          .map(s => ({ ...s, children: childMap.get(code(s).toLowerCase().replace(/[^a-z0-9]/g, '')) || [] }))
      );
    }
    res.json(sets);
  } catch (error) {
    console.error('Error fetching sets:', error);
    res.set('Cache-Control', 'no-store');
    res.status(500).json({ error: 'Failed to retrieve sets' });
  }
});

module.exports = router;
