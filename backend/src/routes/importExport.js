const express = require('express');
const router = express.Router();
const db = require('../db');
const { parseThirdPartyCSV } = require('../utils/csvMappers');
const { generateExportCSV } = require('../utils/csvExporters');
const { resolveCardPrice } = require('../utils/priceHelpers');
const { bulkFetchByIdentifier, cacheCards } = require('../scryfallApi');

const EXPORT_SELECT = `
      SELECT
        c.id as entry_id,
        c.quantity,
        c.condition,
        c.printing,
        c.language,
        c.purchase_price,
        c.added_at,
        c.is_proxy,
        cc.id as card_id,
        cc.name as name,
        cc.supertype,
        cc.types,
        cc.rarity,
        cc.set_id as set_code,
        cc.set_name,
        cc.number as collector_number,
        cc.image_url,
        cc.price_trend,
        cc.price_normal,
        cc.price_holofoil
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.user_id = ?`;

// Collection rows for an export. With entryIds, only those rows (the user's
// own; foreign ids just drop out) in the order given, so "export current
// view" matches what is on screen.
async function exportRows(userId, entryIds = null) {
  let raw;
  if (entryIds) {
    raw = [];
    for (let i = 0; i < entryIds.length; i += 500) {
      const chunk = entryIds.slice(i, i + 500);
      raw.push(...await db.all(`${EXPORT_SELECT} AND c.id IN (${chunk.map(() => '?').join(',')})`, [userId, ...chunk]));
    }
    const order = new Map(entryIds.map((id, i) => [String(id), i]));
    raw.sort((a, b) => order.get(String(a.entry_id)) - order.get(String(b.entry_id)));
  } else {
    raw = await db.all(EXPORT_SELECT, [userId]);
  }
  // market_price used to be cc.price_trend flat, which exported the wrong number
  // for every foil — the same case resolveCardPrice exists to get right.
  // price_trend is destructured OUT along with the per-printing columns: the CSV
  // strategies read `item.price_trend || item.market_price`, so leaving it in
  // would win over the resolved number and export the raw price anyway.
  return raw.map(({ entry_id, price_trend, price_normal, price_holofoil, ...keep }) => ({
    ...keep,
    market_price: resolveCardPrice({ price_trend, price_normal, price_holofoil, ...keep }),
  }));
}

function sendExport(res, rows, format, targetFormat, base) {
  if (String(format).toLowerCase() === 'json') {
    res.setHeader('Content-Type', 'application/json');
    res.setHeader('Content-Disposition', `attachment; filename=${base}_${targetFormat}.json`);
    return res.json(rows);
  }
  res.setHeader('Content-Type', 'text/csv');
  res.setHeader('Content-Disposition', `attachment; filename=${base}_${targetFormat}.csv`);
  return res.send(generateExportCSV(rows, targetFormat));
}

// Export endpoint
router.get('/export', async (req, res) => {
  const { format = 'csv', ecosystem = 'internal' } = req.query;
  const targetFormat = (ecosystem || format || 'internal').toLowerCase();
  try {
    return sendExport(res, await exportRows(req.user.id), format, targetFormat, 'scrybox_collection');
  } catch (error) {
    res.status(500).json({ error: 'Export failed', message: error.message });
  }
});

// Export just some rows (Collection's "Export view"): body
// { entry_ids: [...], ecosystem: 'internal'|'manabox'|'tcgplayer'|..., format: 'csv'|'json' }.
router.post('/export', async (req, res) => {
  const { entry_ids: ids, format = 'csv', ecosystem = 'internal' } = req.body || {};
  if (!Array.isArray(ids) || ids.length === 0 || ids.length > 200000) {
    return res.status(400).json({ error: 'entry_ids must be a non-empty array' });
  }
  const entryIds = [...new Set(ids.map(Number).filter(Number.isInteger))];
  if (!entryIds.length) return res.status(400).json({ error: 'entry_ids must be integers' });
  const targetFormat = String(ecosystem || 'internal').toLowerCase();
  try {
    return sendExport(res, await exportRows(req.user.id, entryIds), format, targetFormat, 'scrybox_view');
  } catch (error) {
    res.status(500).json({ error: 'Export failed', message: error.message });
  }
});

// Import endpoint
router.post('/import', async (req, res) => {
  const { format = 'internal', data } = req.body;
  if (!data) {
    return res.status(400).json({ error: 'No data provided' });
  }

  try {
    let rawItems = [];

    if (format.toLowerCase() === 'json') {
      rawItems = typeof data === 'string' ? JSON.parse(data) : data;
    } else {
      let lines = [];
      if (typeof data === 'string') {
        lines = data.split('\n').map(l => l.trim()).filter(Boolean);
      }
      if (lines.length <= 1) {
        return res.status(400).json({ error: 'CSV file is empty or missing headers' });
      }

      const parseCSVLine = (line) => {
        const result = [];
        let current = '';
        let inQuotes = false;
        for (let i = 0; i < line.length; i++) {
          const char = line[i];
          if (char === '"') {
            inQuotes = !inQuotes;
          } else if (char === ',' && !inQuotes) {
            result.push(current.trim());
            current = '';
          } else {
            current += char;
          }
        }
        result.push(current.trim());
        return result;
      };

      const headers = parseCSVLine(lines[0]).map(h => h.replace(/^"|"$/g, ''));
      const parsedRows = [];

      for (let i = 1; i < lines.length; i++) {
        const values = parseCSVLine(lines[i]).map(v => v.replace(/^"|"$/g, ''));
        if (values.length < headers.length) continue;

        const rowObj = {};
        headers.forEach((h, idx) => {
          rowObj[h] = values[idx];
        });
        parsedRows.push(rowObj);
      }

      rawItems = parseThirdPartyCSV(parsedRows, format);
    }

    if (!Array.isArray(rawItems)) {
      return res.status(400).json({ error: 'Invalid data payload' });
    }

    // This install is MTG-only now, but an export made BEFORE the Pokemon
    // removal still carries Pokemon cards (game = 'pokemon') and Pokemon-era
    // finishes (Reverse Holofoil / 1st Edition / Promo). Without this guard such
    // a file re-imports exactly what the migration just deleted — the collection
    // INSERT would even fail the new CHECK constraint, but only mid-transaction.
    // Reject the offending rows up front with a clear message instead.
    const allowedPrintings = ['Normal', 'Holofoil'];
    const rejected = [];
    const prepared = [];
    const authoritativeCards = new Map();
    for (let i = 0; i < rawItems.length; i++) {
      const item = rawItems[i];
      const game = String(item.game ?? '').toLowerCase();
      if (game && game !== 'mtg') {
        rejected.push({ index: i, reason: `game '${item.game}' is not supported (this install is MTG-only)` });
        continue;
      }
      if (item.printing !== undefined && item.printing !== '' &&
          !allowedPrintings.includes(item.printing)) {
        rejected.push({ index: i, reason: `printing '${item.printing}' is not supported` });
        continue;
      }

      // A cached id has already crossed the Scryfall boundary. An uncached row
      // has not: legacy exports often omit `game`, and client-supplied names,
      // sets, types and rarity are not evidence that a card is Magic. Resolve
      // every such row through Scryfall and use its canonical id instead of
      // synthesizing a cache row from the import payload.
      const suppliedId = item.card_id || item.id || null;
      const cached = suppliedId
        ? await db.get(`SELECT id FROM card_cache WHERE id = ?`, [suppliedId])
        : null;
      if (cached && String(cached.id).startsWith('mtg-')) {
        prepared.push({ item, cardId: cached.id });
        continue;
      }
      if (cached) {
        rejected.push({ index: i, reason: 'cached card id is not an MTG printing' });
        continue;
      }

      const lookup = {
        id: suppliedId,
        set_id: item.set_code || item.set_id || '',
        number: item.collector_number || item.number || '',
        name: item.name || ''
      };
      if (!lookup.id && !(lookup.set_id && lookup.number) && !lookup.name) {
        rejected.push({ index: i, reason: 'card has no MTG identifier Scryfall can validate' });
        continue;
      }
      const result = await bulkFetchByIdentifier([lookup]);
      const match = result.pairs[0]?.card;
      if (!match || !String(match.id).startsWith('mtg-')) {
        rejected.push({ index: i, reason: 'card was not recognized by Scryfall as an MTG printing' });
        continue;
      }
      authoritativeCards.set(match.id, match);
      prepared.push({ item, cardId: match.id });
    }
    if (rejected.length > 0) {
      return res.status(400).json({
        error: `Import rejected: ${rejected.length} of ${rawItems.length} row(s) are not valid MTG printings. No rows were imported.`,
        rejected
      });
    }

    let importedCount = 0;

    await db.withTransaction(async () => {
      if (authoritativeCards.size) await cacheCards([...authoritativeCards.values()]);
      for (const { item, cardId } of prepared) {

        await db.run(
          `INSERT INTO collection 
           (card_id, user_id, quantity, condition, printing, language, purchase_price, added_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)`,
          [
            cardId,
            req.user.id,
            item.quantity || 1,
            item.condition || 'Near Mint',
            item.printing || 'Normal',
            item.language || 'English',
            item.purchase_price || 0
          ]
        );
        importedCount++;
      }
    });

    return res.json({ success: true, count: importedCount, message: `Successfully imported ${importedCount} items.` });
  } catch (error) {
    return res.status(500).json({ error: 'Import failed', message: error.message });
  }
});

module.exports = router;
