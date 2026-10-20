const express = require('express');
const db = require('../db');
const cardApi = require('../utils/cardApi');
const { parseCardRow, recordPrice } = require('../utils/priceHelpers');
const { validateDeckAddition, isBasicLand } = require('../utils/deckRules');
const { sqlCardKey, sqlIsBasicLand } = require('../utils/cardIdentity');
const { withAllocationLock } = require('../utils/collectionHelpers');
const { getDeckMinimumValues, emptyDeckMinimumValue, getCheapestPrices, currentPrintingPrice, deckCurrentPrintValues } = require('../utils/deckPricing');
const { getDeckCommanders } = require('../utils/deckCommander');
const { getDeckOwnership } = require('../utils/deckOwnership');

const router = express.Router();

async function unresolvedDeckCardCount(deckId) {
  const row = await db.get(`
    SELECT COUNT(*) AS count
    FROM deck_cards dc
    LEFT JOIN card_cache cc ON cc.id = dc.card_id
    WHERE dc.deck_id = ? AND dc.quantity > 0 AND cc.id IS NULL
  `, [deckId]);
  return Number(row && row.count) || 0;
}

// One printing-agnostic coverage query shared by the checkout guide and the
// checkout mutation's diagnostics. A deck may contain several printing ids for
// one card; requirements are summed by canonical English name, collection copies
// are summed across every printing, and checked-out decks reserve that same
// logical pool.
//
// Ownership and lock totals are computed once per user (two bounded scans) and
// joined on in the final SELECT. The previous shape correlated a full
// collection scan and a full deck_cards scan INTO every card row, so a 90-card
// deck cost ~90 x 20k-row scans per request.
async function getDeckCoverageRows(deckId, userId) {
  return db.all(`
    WITH requested AS (
      SELECT
        ${sqlCardKey('deck_cc')} AS card_key,
        MIN(dc.card_id) AS representative_id,
        SUM(dc.quantity) AS required_qty
      FROM deck_cards dc
      JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
      WHERE dc.deck_id = ? AND dc.quantity > 0
      GROUP BY ${sqlCardKey('deck_cc')}
    ),
    owned AS (
      SELECT ${sqlCardKey('owned_cc')} AS card_key, SUM(owned.quantity) AS owned_qty
      FROM collection owned
      JOIN card_cache owned_cc ON owned_cc.id = owned.card_id
      WHERE owned.user_id = ? AND owned.quantity > 0
      GROUP BY ${sqlCardKey('owned_cc')}
    ),
    locked AS (
      SELECT ${sqlCardKey('locked_cc')} AS card_key, SUM(locked_dc.quantity) AS locked_qty
      FROM deck_cards locked_dc
      JOIN card_cache locked_cc ON locked_cc.id = locked_dc.card_id
      JOIN decks locked_deck ON locked_deck.id = locked_dc.deck_id
      WHERE locked_deck.checked_out = 1
        AND locked_dc.quantity > 0
        AND locked_deck.user_id = ?
        AND locked_deck.id != ?
      GROUP BY ${sqlCardKey('locked_cc')}
    )
    SELECT
      requested.representative_id AS card_id,
      cc.name, cc.printed_name, cc.supertype, cc.subtypes,
      cc.set_name, cc.number, cc.image_url,
      requested.required_qty,
      COALESCE(owned.owned_qty, 0) AS owned_qty,
      COALESCE(locked.locked_qty, 0) AS locked_qty
    FROM requested
    JOIN card_cache cc ON cc.id = requested.representative_id
    LEFT JOIN owned ON owned.card_key = requested.card_key
    LEFT JOIN locked ON locked.card_key = requested.card_key
  `, [deckId, userId, userId, deckId]);
}

// Get User Decks
router.get('/', async (req, res) => {
  try {
    const query = `
      SELECT
        d.id,
        d.name,
        d.description,
        d.format,
        d.category,
        d.accent_color,
        d.target_size,
        d.created_at,
        d.checked_out,
        d.checked_out_at,
        d.source,
        -- Moxfield mirror decks keep their remote public id; the deck list uses it
        -- to offer "open on Moxfield" next to the source badge, the same link the
        -- Moxfield sync panel shows. NULL for hand-made/precon decks.
        d.moxfield_public_id,
        COUNT(DISTINCT CASE
          WHEN dc.quantity > 0 THEN CASE
            WHEN deck_cc.id IS NULL THEN 'missing:' || dc.card_id
            ELSE ${sqlCardKey('deck_cc')}
          END
        END) as total_card_types,
        COALESCE(SUM(CASE WHEN dc.quantity > 0 THEN dc.quantity ELSE 0 END), 0) as total_cards,
        COUNT(DISTINCT CASE WHEN dc.quantity > 0 AND deck_cc.id IS NULL THEN dc.card_id END) as unresolved_card_types
      FROM decks d
      LEFT JOIN deck_cards dc ON d.id = dc.deck_id
      LEFT JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
      WHERE d.user_id = ?
      GROUP BY d.id
      ORDER BY d.created_at DESC
    `;
    const rows = await db.all(query, [req.user.id]);
    // Value and commander art are both deck-wide scans that must stay one-pass,
    // but they are independent, so they run together.
    const [values, commanders, ownership] = await Promise.all([
      getDeckMinimumValues(db, rows.map(row => row.id)),
      getDeckCommanders(db, req.user.id),
      getDeckOwnership(db, req.user.id),
    ]);
    res.json(rows.map(row => {
      const commander = commanders.get(Number(row.id));
      const own = ownership.get(Number(row.id)) || { missing_card_types: 0, missing_copies: 0 };
      return {
        ...row,
        // Same basics-exempt, printing-agnostic math as the editor's
        // "N cards still missing" (utils/deckOwnership.js).
        missing_card_types: own.missing_card_types,
        missing_copies: own.missing_copies,
        ...(values.get(Number(row.id)) || emptyDeckMinimumValue()),
        commander_name: commander ? commander.commander_name : null,
        commander_image_url: commander ? commander.commander_image_url : null,
        // The cache id behind the art. The frontend resolves contributed art
        // from the card id, not from the provider URL.
        commander_card_id: commander ? commander.commander_card_id : null,
      };
    }));
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to retrieve decks' });
  }
});

// Create Deck
router.post('/', async (req, res) => {
  const { 
    name, 
    description = '', 
    format = 'Standard',
    category = 'Competitive',
    accent_color = '#eab308',
    target_size = 60,
    decklist_text = ''
  } = req.body;
  
  if (!name) {
    return res.status(400).json({ error: 'Deck name is required' });
  }

  const targetSizeNum = parseInt(target_size, 10) || 60;

  try {
    const result = await db.run(
      `INSERT INTO decks (name, description, format, category, accent_color, target_size, user_id) VALUES (?, ?, ?, ?, ?, ?, ?)`,
      [name, description, format, category, accent_color, targetSizeNum, req.user.id]
    );
    const newDeckId = result.lastID;

    // Optional decklist import
    if (decklist_text && typeof decklist_text === 'string') {
      const lines = decklist_text.split('\n');
      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed) continue;
        const match = trimmed.match(/^(\d+)x?\s+(.+)$/i);
        if (match) {
          const qty = parseInt(match[1], 10);
          const cardName = match[2].trim();
          const card = await db.get(`SELECT id FROM card_cache WHERE LOWER(name) = LOWER(?) LIMIT 1`, [cardName]);
          if (card) {
            await db.run(
              `INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (?, ?, ?) ON CONFLICT(deck_id, card_id) DO UPDATE SET quantity = quantity + EXCLUDED.quantity`,
              [newDeckId, card.id, qty]
            );
          }
        }
      }
    }

    res.status(201).json({ message: 'Deck created successfully', id: newDeckId });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to create deck' });
  }
});

// Get Deck Details (with Cards)
router.get('/:id', async (req, res) => {
  const { id } = req.params;
  try {
    const deck = await db.get(`SELECT * FROM decks WHERE id = ? AND user_id = ?`, [id, req.user.id]);
    if (!deck) {
      return res.status(404).json({ error: 'Deck not found' });
    }
    if (await unresolvedDeckCardCount(id)) {
      return res.status(422).json({
        error: 'This deck contains cards whose details are unavailable. Remove or re-import them before continuing.'
      });
    }

    // Owned copies are aggregated once per user and joined on per row. The
    // previous per-row correlated subquery rescanned the whole collection for
    // every card in the deck (~80 ms x deck size on a 20k-row collection).
    const cardsQuery = `
      WITH requested AS (
        SELECT
          ${sqlCardKey('deck_cc')} AS card_key,
          MIN(dc.card_id) AS representative_id,
          SUM(dc.quantity) AS quantity,
          MAX(COALESCE(dc.is_commander, 0)) AS is_commander
        FROM deck_cards dc
        JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
        WHERE dc.deck_id = ? AND dc.quantity > 0
        GROUP BY ${sqlCardKey('deck_cc')}
      ),
      owned AS (
        SELECT ${sqlCardKey('owned_cc')} AS card_key, SUM(owned.quantity) AS owned_qty
        FROM collection owned
        JOIN card_cache owned_cc ON owned_cc.id = owned.card_id
        WHERE owned.user_id = ? AND owned.quantity > 0
        GROUP BY ${sqlCardKey('owned_cc')}
      )
      SELECT
        requested.quantity,
        requested.card_key,
        requested.is_commander,
        cc.cmc,
        cc.color_identity,
        cc.oracle_text,
        cc.oracle_faces,
        cc.id,
        cc.name, cc.printed_name,
        cc.supertype,
        cc.subtypes,
        cc.types,
        cc.rarity,
        cc.set_id,
        cc.set_name,
        cc.number,
        cc.image_url,
        cc.price_trend,
        cc.price_normal,
        cc.price_holofoil,
        cc.price_etched,
        cc.price_currency,
        COALESCE(owned.owned_qty, 0) AS owned_qty
      FROM requested
      JOIN card_cache cc ON cc.id = requested.representative_id
      LEFT JOIN owned ON owned.card_key = requested.card_key
      ORDER BY cc.name
    `;
    const cards = await db.all(cardsQuery, [id, req.user.id]);

    // Two value axes on the same card row: current_price is what THIS deck's
    // printing costs (cheapestEligiblePrice over the representative row's own
    // fields, USD-gated exactly like the floor query), cheapest_price is the
    // logical card's cheapest known USD printing — getCheapestPrices, the same
    // helper behind minimum_value, so screen and total can't disagree.
    const cheapestByKey = await getCheapestPrices(db, cards.map(c => c.card_key).filter(Boolean));
    const formatted = cards.map(row => ({
      ...parseCardRow(row),
      current_price: currentPrintingPrice(row),
      cheapest_price: cheapestByKey.has(row.card_key) ? cheapestByKey.get(row.card_key) : null,
    }));

    const values = await getDeckMinimumValues(db, [deck.id]);

    // Current-printings deck total: summed from EVERY deck_cards row, not the
    // grouped representatives above — a deck holding two printings of one
    // logical card pays each printing its own price, the same row-level
    // accounting getDeckRequirements sums for the floor.
    const currentRows = await db.all(`
      SELECT dc.quantity,
             cc.price_normal, cc.price_holofoil, cc.price_etched, cc.price_trend, cc.price_currency
      FROM deck_cards dc
      JOIN card_cache cc ON cc.id = dc.card_id
      WHERE dc.deck_id = ? AND dc.quantity > 0
    `, [id]);
    const currentValues = deckCurrentPrintValues(currentRows.map(row => ({
      quantity: row.quantity,
      current_price: currentPrintingPrice(row),
    })));

    // Maybeboard: shown on its own tab with own/missing, never in deck stats.
    const maybeRows = await db.all(`
      WITH owned AS (
        SELECT ${sqlCardKey('owned_cc')} AS card_key, SUM(owned.quantity) AS owned_qty
        FROM collection owned
        JOIN card_cache owned_cc ON owned_cc.id = owned.card_id
        WHERE owned.user_id = ? AND owned.quantity > 0
        GROUP BY ${sqlCardKey('owned_cc')}
      )
      SELECT dm.quantity, ${sqlCardKey('cc')} AS card_key,
        cc.id, cc.name, cc.printed_name, cc.supertype, cc.subtypes, cc.types, cc.rarity,
        cc.set_id, cc.set_name, cc.number, cc.image_url, cc.cmc, cc.color_identity,
        cc.price_trend, cc.price_normal, cc.price_holofoil, cc.price_etched, cc.price_currency,
        COALESCE(owned.owned_qty, 0) AS owned_qty
      FROM deck_maybe_cards dm
      JOIN card_cache cc ON cc.id = dm.card_id
      LEFT JOIN owned ON owned.card_key = ${sqlCardKey('cc')}
      WHERE dm.deck_id = ? AND dm.quantity > 0
      ORDER BY cc.name
    `, [req.user.id, id]);
    const maybeboard = maybeRows.map(row => ({ ...parseCardRow(row), current_price: currentPrintingPrice(row) }));

    res.json({
      ...deck,
      ...(values.get(Number(deck.id)) || emptyDeckMinimumValue()),
      ...currentValues,
      cards: formatted,
      maybeboard
    });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to retrieve deck details' });
  }
});

// Checkout coverage for a deck: how many of each card are available.
// "Available" = owned minus copies already locked by other checked-out decks
// (the same math PUT /:id/checkout validates against). The wizard turns this
// into an in-collection vs. missing checklist.
router.get('/:id/locations', async (req, res) => {
  const { id } = req.params;
  try {
    const deck = await db.get(`SELECT id FROM decks WHERE id = ? AND user_id = ?`, [id, req.user.id]);
    if (!deck) return res.status(404).json({ error: 'Deck not found' });

    const unresolved = await db.get(`
      SELECT COUNT(*) AS count
      FROM deck_cards dc
      LEFT JOIN card_cache cc ON cc.id = dc.card_id
      WHERE dc.deck_id = ? AND dc.quantity > 0 AND cc.id IS NULL
    `, [id]);
    if (unresolved && unresolved.count > 0) {
      return res.status(422).json({ error: 'Deck contains cards with missing metadata' });
    }

    const cards = await getDeckCoverageRows(id, req.user.id);

    const results = cards.map(card => {
      const available = card.owned_qty - card.locked_qty;
      const missing = Math.max(0, card.required_qty - available);
      return {
        card_id: card.card_id,
        name: card.name,
        card_name: card.printed_name || card.name,
        set_name: card.set_name,
        number: card.number,
        image_url: card.image_url,
        required: card.required_qty,
        owned: card.owned_qty,
        in_use: card.locked_qty,
        available: Math.max(0, available),
        missing,
        covered: missing === 0,
        // Checkout ignores basic lands: they are not counted against coverage.
        // The same helper the deck builder uses, so the two can never drift.
        is_basic_land: isBasicLand({ name: card.name, supertype: card.supertype, subtypes: card.subtypes })
      };
    });

    // Missing cards first (most missing on top), then alphabetical.
    results.sort((a, b) => (b.missing - a.missing) || a.name.localeCompare(b.name));

    res.json(results);
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to retrieve checkout coverage' });
  }
});

// Update Deck Metadata
router.put('/:id', async (req, res) => {
  const { id } = req.params;
  const { name, description } = req.body;

  if (!name) {
    return res.status(400).json({ error: 'Deck name is required' });
  }

  try {
    const result = await db.run(
      `UPDATE decks SET name = ?, description = ? WHERE id = ? AND user_id = ?`,
      [name, description || '', id, req.user.id]
    );

    if (result.changes === 0) {
      return res.status(404).json({ error: 'Deck not found or unauthorized' });
    }

    res.json({ message: 'Deck updated successfully' });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to update deck' });
  }
});

// Delete Deck
router.delete('/:id', async (req, res) => {
  const { id } = req.params;
  try {
    const outcome = await withAllocationLock(async () => db.withDedicatedTransaction(async tx => {
      const deck = await tx.get(`SELECT id, checked_out FROM decks WHERE id = ? AND user_id = ?`, [id, req.user.id]);
      if (!deck) return { status: 404, error: 'Deck not found or unauthorized' };
      if (deck.checked_out) return { status: 409, error: 'Check this deck in before changing its cards.' };

      await tx.run(`DELETE FROM deck_cards WHERE deck_id = ?`, [id]);
      await tx.run(`DELETE FROM deck_maybe_cards WHERE deck_id = ?`, [id]);
      const deleted = await tx.run(`DELETE FROM decks WHERE id = ? AND user_id = ?`, [id, req.user.id]);
      if (deleted.changes !== 1) throw new Error('Deck disappeared during deletion');
      return { status: 200 };
    }));
    if (outcome.error) return res.status(outcome.status).json({ error: outcome.error });
    res.json({ message: 'Deck deleted successfully' });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to delete deck' });
  }
});

// Add/Update Card in Deck
router.post('/:id/cards', async (req, res) => {
  const { id } = req.params;
  const { card_id, quantity = 1 } = req.body;

  if (!card_id) {
    return res.status(400).json({ error: 'card_id is required' });
  }

  try {
    // Verify deck ownership
    const deck = await db.get(`SELECT id, checked_out FROM decks WHERE id = ? AND user_id = ?`, [id, req.user.id]);
    if (!deck) {
      return res.status(404).json({ error: 'Deck not found or unauthorized' });
    }
    if (deck.checked_out) {
      return res.status(409).json({ error: 'Check this deck in before changing its cards.' });
    }

    // Ensure card metadata exists in cache.
    let card = await db.get(`SELECT id FROM card_cache WHERE id = ?`, [card_id]);
    if (!card) {
      console.log(`Card ${card_id} not in cache. Fetching...`);
      const apiCard = await cardApi.getCardById(card_id);
      if (!apiCard) {
        return res.status(404).json({ error: `Card ${card_id} not found on any card provider.` });
      }
    }

    // If this deck already carries the same game card under another printing,
    // update that logical row instead of creating a second printing-specific row.
    const equivalent = await db.get(`
      SELECT dc.card_id
      FROM deck_cards dc
      JOIN card_cache existing_cc ON existing_cc.id = dc.card_id
      JOIN card_cache target_cc ON target_cc.id = ?
      WHERE dc.deck_id = ?
        AND ${sqlCardKey('existing_cc')} = ${sqlCardKey('target_cc')}
      ORDER BY (dc.card_id = ?) DESC, dc.card_id
      LIMIT 1
    `, [card_id, id, card_id]);
    const effectiveCardId = equivalent ? equivalent.card_id : card_id;

    // Enforce the logical owned-copy cap + max 4 per name. quantity here is the
    // absolute new count for the game card, regardless of which art was selected.
    const check = await validateDeckAddition({
      deckId: id,
      userId: req.user.id,
      cardId: effectiveCardId,
      newQty: quantity
    });
    if (!check.ok) return res.status(400).json({ error: check.error });

    const parsedQuantity = parseInt(quantity, 10);
    if (equivalent) {
      // Preserve one representative id and zero any legacy reprint rows in one
      // statement so grouped reads immediately equal the requested quantity.
      const updated = await db.run(`
        UPDATE deck_cards
        SET quantity = CASE WHEN card_id = ? THEN ? ELSE 0 END
        WHERE deck_id = ?
          AND EXISTS (
            SELECT 1 FROM decks guarded_deck
            WHERE guarded_deck.id = deck_cards.deck_id
              AND guarded_deck.user_id = ?
              AND guarded_deck.checked_out = 0
          )
          AND card_id IN (
            SELECT existing_cc.id
            FROM card_cache existing_cc
            JOIN card_cache target_cc ON target_cc.id = ?
            WHERE ${sqlCardKey('existing_cc')} = ${sqlCardKey('target_cc')}
          )
      `, [effectiveCardId, parsedQuantity, id, req.user.id, effectiveCardId]);
      if (!updated.changes) {
        return res.status(409).json({ error: 'The deck was checked out before the card update could be saved.' });
      }
      await db.run(`DELETE FROM deck_cards WHERE deck_id = ? AND quantity <= 0`, [id]);
    } else {
      const inserted = await db.run(
        `INSERT INTO deck_cards (deck_id, card_id, quantity)
         SELECT ?, ?, ?
         WHERE EXISTS (
           SELECT 1 FROM decks guarded_deck
           WHERE guarded_deck.id = ?
             AND guarded_deck.user_id = ?
             AND guarded_deck.checked_out = 0
         )`,
        [id, effectiveCardId, parsedQuantity, id, req.user.id]
      );
      if (!inserted.changes) {
        return res.status(409).json({ error: 'The deck was checked out before the card could be added.' });
      }
    }

    // Record initial price history trend if card is added
    const cacheCard = await db.get(`SELECT price_trend FROM card_cache WHERE id = ?`, [effectiveCardId]);
    if (cacheCard) await recordPrice(effectiveCardId, cacheCard.price_trend);

    res.json({ message: 'Card added/updated in deck successfully' });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to add card to deck' });
  }
});

// Remove Card from Deck
router.delete('/:id/cards/:card_id', async (req, res) => {
  const { id, card_id } = req.params;
  try {
    // Verify deck ownership
    const deck = await db.get(`SELECT id, checked_out FROM decks WHERE id = ? AND user_id = ?`, [id, req.user.id]);
    if (!deck) {
      return res.status(404).json({ error: 'Deck not found or unauthorized' });
    }
    if (deck.checked_out) {
      return res.status(409).json({ error: 'Check this deck in before changing its cards.' });
    }

    // A deck card is a logical game card. Removing any displayed printing removes
    // all legacy rows for that same name from this deck.
    const removed = await db.run(`
      DELETE FROM deck_cards
      WHERE deck_id = ?
        AND EXISTS (
          SELECT 1 FROM decks guarded_deck
          WHERE guarded_deck.id = deck_cards.deck_id
            AND guarded_deck.user_id = ?
            AND guarded_deck.checked_out = 0
        )
        AND (
          card_id = ?
          OR card_id IN (
            SELECT existing_cc.id
            FROM card_cache existing_cc
            JOIN card_cache target_cc ON target_cc.id = ?
            WHERE ${sqlCardKey('existing_cc')} = ${sqlCardKey('target_cc')}
          )
        )
    `, [id, req.user.id, card_id, card_id]);
    if (!removed.changes) {
      const current = await db.get(`SELECT checked_out FROM decks WHERE id = ? AND user_id = ?`, [id, req.user.id]);
      if (current && current.checked_out) {
        return res.status(409).json({ error: 'The deck was checked out before the card could be removed.' });
      }
    }
    res.json({ message: 'Card removed from deck successfully' });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to remove card from deck' });
  }
});

// Register every physical card represented by this deck as newly owned.
// This is deliberately additive: buying a precon (for example) adds another
// Sol Ring even when the collection already contains one from a different deck.
// One stacked collection row per printing keeps the write small while preserving
// the deck's exact quantities. The transaction prevents a half-registered deck.
router.post('/:id/register-collection', async (req, res) => {
  const { id } = req.params;
  try {
    const deck = await db.get(
      `SELECT id, name FROM decks WHERE id = ? AND user_id = ?`,
      [id, req.user.id]
    );
    if (!deck) return res.status(404).json({ error: 'Deck not found or unauthorized' });

    // Keep the checked-out guard and the complete collection write in one SQLite
    // statement. This makes register-vs-checkout linearizable: the INSERT either
    // happens before checkout's guarded state becomes visible, or inserts nothing.
    // The NOT EXISTS clause also fails closed if legacy/imported deck rows refer to
    // card metadata that is no longer cached; an inner join must never turn "every
    // card" into a partial registration.
    const inserted = await db.all(`
      INSERT INTO collection (
        card_id, user_id, quantity, condition, printing, language,
        purchase_price, is_trade
      )
      SELECT
        dc.card_id,
        d.user_id,
        dc.quantity,
        'Near Mint',
        'Normal',
        COALESCE(NULLIF(cc.language, ''), 'English'),
        0,
        0
      FROM decks d
      JOIN deck_cards dc ON dc.deck_id = d.id
      JOIN card_cache cc ON cc.id = dc.card_id
      WHERE d.id = ?
        AND d.user_id = ?
        AND d.checked_out = 0
        AND dc.quantity > 0
        AND NOT EXISTS (
          SELECT 1
          FROM deck_cards unresolved
          LEFT JOIN card_cache cached ON cached.id = unresolved.card_id
          WHERE unresolved.deck_id = d.id
            AND unresolved.quantity > 0
            AND cached.id IS NULL
        )
      ORDER BY dc.card_id
      RETURNING card_id, quantity
    `, [id, req.user.id]);

    if (inserted.length === 0) {
      const current = await db.get(
        `SELECT checked_out FROM decks WHERE id = ? AND user_id = ?`,
        [id, req.user.id]
      );
      if (!current) return res.status(404).json({ error: 'Deck not found or unauthorized' });
      if (current.checked_out) {
        return res.status(409).json({ error: 'This deck is already checked out for play.' });
      }

      const sourceState = await db.get(`
        SELECT
          COUNT(*) AS card_types,
          COALESCE(SUM(CASE WHEN cc.id IS NULL THEN 1 ELSE 0 END), 0) AS unresolved
        FROM deck_cards dc
        LEFT JOIN card_cache cc ON cc.id = dc.card_id
        WHERE dc.deck_id = ? AND dc.quantity > 0
      `, [id]);
      if (!sourceState.card_types) {
        return res.status(400).json({ error: 'This deck has no cards to register.' });
      }
      if (sourceState.unresolved) {
        return res.status(422).json({
          error: 'This deck contains cards whose details are unavailable. Refresh or re-import it before registering.'
        });
      }

      // The deck changed between the guarded statement and diagnostics. Nothing
      // was inserted; ask the client to reload rather than claiming success.
      return res.status(409).json({ error: 'The deck changed before it could be registered. Refresh and try again.' });
    }

    // Price history is auxiliary bookkeeping. Registration has already committed
    // atomically, so a history write must not turn a successful inventory change
    // into a misleading 500 response.
    const placeholders = inserted.map(() => '?').join(',');
    const prices = await db.all(
      `SELECT id, price_trend FROM card_cache WHERE id IN (${placeholders})`,
      inserted.map(card => card.card_id)
    );
    for (const card of prices) {
      try {
        await recordPrice(card.id, card.price_trend);
      } catch (priceError) {
        console.error(`Failed to record registration price for ${card.id}:`, priceError);
      }
    }

    const total = inserted.reduce((sum, card) => sum + Number(card.quantity || 0), 0);
    res.status(201).json({
      message: `Added ${total} cards from ${deck.name} to your collection.`,
      added: total,
      card_types: inserted.length
    });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to register deck in collection' });
  }
});

// Checkout Deck (mark as in play)
router.put('/:id/checkout', async (req, res) => {
  const { id } = req.params;
  try {
    // The availability guard and state change are one SQLite statement. Concurrent
    // checkouts serialize here: once one deck becomes checked out, the next
    // statement sees its same-name reservations. This prevents two clients from
    // both spending the same physical copies after separate preflight reads.
    //
    // The supply math (owned minus locked, per logical card) is computed once
    // per user in CTEs and joined against this deck's requirements, instead of
    // rescanning the collection and every checked-out deck for each card group
    // inside the HAVING clause. Same predicate, one pass each.
    const checkedOut = await withAllocationLock(() => db.all(`
      WITH owned AS (
        SELECT ${sqlCardKey('owned_cc')} AS card_key, SUM(owned.quantity) AS owned_qty
        FROM collection owned
        JOIN card_cache owned_cc ON owned_cc.id = owned.card_id
        WHERE owned.user_id = ?
          AND owned.quantity > 0
        GROUP BY ${sqlCardKey('owned_cc')}
      ),
      locked AS (
        SELECT ${sqlCardKey('locked_cc')} AS card_key, SUM(locked_dc.quantity) AS locked_qty
        FROM deck_cards locked_dc
        JOIN card_cache locked_cc ON locked_cc.id = locked_dc.card_id
        JOIN decks locked_deck ON locked_deck.id = locked_dc.deck_id
        WHERE locked_deck.user_id = ?
          AND locked_deck.checked_out = 1
          AND locked_dc.quantity > 0
          AND locked_deck.id != ?
        GROUP BY ${sqlCardKey('locked_cc')}
      )
      UPDATE decks AS target
      SET checked_out = 1, checked_out_at = CURRENT_TIMESTAMP
      WHERE target.id = ?
        AND target.user_id = ?
        AND target.checked_out = 0
        AND EXISTS (
          SELECT 1 FROM deck_cards present
          WHERE present.deck_id = target.id AND present.quantity > 0
        )
        AND NOT EXISTS (
          SELECT 1
          FROM deck_cards unresolved
          LEFT JOIN card_cache unresolved_cc ON unresolved_cc.id = unresolved.card_id
          WHERE unresolved.deck_id = target.id
            AND unresolved.quantity > 0
            AND unresolved_cc.id IS NULL
        )
        AND NOT EXISTS (
          SELECT 1
          FROM decks orphan_deck
          JOIN deck_cards orphan_dc ON orphan_dc.deck_id = orphan_deck.id
          LEFT JOIN card_cache orphan_cc ON orphan_cc.id = orphan_dc.card_id
          WHERE orphan_deck.user_id = target.user_id
            AND orphan_deck.checked_out = 1
            AND orphan_dc.quantity > 0
            AND orphan_cc.id IS NULL
        )
        AND NOT EXISTS (
          SELECT 1
          FROM (
            SELECT ${sqlCardKey('needed_cc')} AS card_key, SUM(needed_dc.quantity) AS needed_qty
            FROM deck_cards needed_dc
            JOIN card_cache needed_cc ON needed_cc.id = needed_dc.card_id
            WHERE needed_dc.deck_id = ?
              AND needed_dc.quantity > 0
              AND NOT ${sqlIsBasicLand('needed_cc')}
            GROUP BY ${sqlCardKey('needed_cc')}
          ) needed
          LEFT JOIN owned ON owned.card_key = needed.card_key
          LEFT JOIN locked ON locked.card_key = needed.card_key
          WHERE needed.needed_qty >
            COALESCE(owned.owned_qty, 0) - COALESCE(locked.locked_qty, 0)
        )
      RETURNING id
    `, [req.user.id, req.user.id, id, id, req.user.id, id]));

    if (checkedOut.length) {
      return res.json({ message: 'Deck checked out successfully' });
    }

    // Diagnose a guarded no-op without weakening the atomic mutation above.
    const deck = await db.get(
      `SELECT id, checked_out FROM decks WHERE id = ? AND user_id = ?`,
      [id, req.user.id]
    );
    if (!deck) return res.status(404).json({ error: 'Deck not found or unauthorized' });
    if (deck.checked_out) {
      return res.status(409).json({ error: 'This deck is already checked out for play.' });
    }

    const sourceState = await db.get(`
      SELECT
        COUNT(*) AS card_types,
        COALESCE(SUM(CASE WHEN cc.id IS NULL THEN 1 ELSE 0 END), 0) AS unresolved
      FROM deck_cards dc
      LEFT JOIN card_cache cc ON cc.id = dc.card_id
      WHERE dc.deck_id = ? AND dc.quantity > 0
    `, [id]);
    if (!sourceState.card_types) {
      return res.status(400).json({ error: 'This deck has no cards to check out.' });
    }
    if (sourceState.unresolved) {
      return res.status(422).json({
        error: 'This deck contains cards whose details are unavailable. Refresh or re-import it before checkout.'
      });
    }

    const cards = await getDeckCoverageRows(id, req.user.id);
    const errors = [];
    for (const card of cards) {
      if (isBasicLand(card)) continue;
      const available = Number(card.owned_qty || 0) - Number(card.locked_qty || 0);
      if (Number(card.required_qty || 0) > available) {
        const deficit = Number(card.required_qty) - available;
        errors.push(`Missing ${deficit}x ${card.name} (Owned: ${card.owned_qty}, In Use: ${card.locked_qty})`);
      }
    }
    if (errors.length) {
      return res.status(400).json({
        error: 'Not enough cards available to check out this deck.',
        details: errors
      });
    }

    // The deck or another allocation changed between the guarded statement and
    // these diagnostics. Nothing was reserved; a reload is the safe response.
    return res.status(409).json({
      error: 'Card availability changed before checkout completed. Refresh and try again.'
    });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to checkout deck' });
  }
});

// Return Deck (mark as back in the collection)
router.put('/:id/return', async (req, res) => {
  const { id } = req.params;
  try {
    const deck = await db.get(`SELECT id FROM decks WHERE id = ? AND user_id = ?`, [id, req.user.id]);
    if (!deck) {
      return res.status(404).json({ error: 'Deck not found or unauthorized' });
    }
    await db.run(
      `UPDATE decks SET checked_out = 0, checked_out_at = NULL WHERE id = ?`,
      [id]
    );
    res.json({ message: 'Deck returned to collection successfully' });
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to return deck' });
  }
});

module.exports = router;
