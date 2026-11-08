const express = require('express');
const zlib = require('zlib');
const db = require('../db');

// Local Scryfall search history, per user, server side. The list lives in one
// row per user as deflated JSON: [[query, unixSeconds], ...], newest first.
const MAX_ITEMS = 100;
const MAX_QUERY = 500;

const router = express.Router();

function decode(blob) {
  if (!blob) return [];
  try {
    const list = JSON.parse(zlib.inflateRawSync(blob).toString('utf8'));
    return Array.isArray(list) ? list.filter(e => Array.isArray(e) && typeof e[0] === 'string') : [];
  } catch {
    return [];
  }
}

const encode = (list) => zlib.deflateRawSync(Buffer.from(JSON.stringify(list), 'utf8'), { level: 9 });

async function load(userId) {
  const row = await db.get(`SELECT data FROM search_history WHERE user_id = ?`, [userId]);
  return decode(row && row.data);
}

async function save(userId, list) {
  await db.run(
    `INSERT INTO search_history (user_id, data, updated_at) VALUES (?, ?, CURRENT_TIMESTAMP)
     ON CONFLICT(user_id) DO UPDATE SET data = excluded.data, updated_at = CURRENT_TIMESTAMP`,
    [userId, encode(list)]
  );
}

// Read-modify-write per user, serialized so two quick searches can't drop one.
const chains = new Map();
function mutate(userId, fn) {
  const prev = chains.get(userId) || Promise.resolve();
  const next = prev.catch(() => {}).then(async () => {
    const list = fn(await load(userId)).slice(0, MAX_ITEMS);
    await save(userId, list);
    return list;
  });
  chains.set(userId, next);
  next.finally(() => { if (chains.get(userId) === next) chains.delete(userId); }).catch(() => {});
  return next;
}

const view = (list) => list.map(([q, at]) => ({ q, at: at * 1000 }));
const cleanQuery = (q) => (typeof q === 'string' ? q.trim().slice(0, MAX_QUERY) : '');

router.get('/', async (req, res) => {
  try {
    res.json(view(await load(req.user.id)));
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to load search history' });
  }
});

// Record one search (moves it to the top), or import several at once:
// { q } or { items: [{ q, at }] } (used once to bring over device history).
router.post('/', async (req, res) => {
  try {
    const now = Math.floor(Date.now() / 1000);
    const incoming = Array.isArray(req.body.items)
      ? req.body.items.map(i => [cleanQuery(i && i.q), Math.floor(Number(i && i.at) / 1000) || now])
      : [[cleanQuery(req.body.q), now]];
    const add = incoming.filter(([q]) => q);
    if (!add.length) return res.status(400).json({ error: 'q is required' });
    const list = await mutate(req.user.id, (cur) => {
      const merged = new Map();
      for (const [q, at] of [...add, ...cur]) {
        if (!merged.has(q) || merged.get(q) < at) merged.set(q, at);
      }
      return [...merged.entries()].sort((a, b) => b[1] - a[1]);
    });
    res.json(view(list));
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to save search history' });
  }
});

// Remove one query ({ q }) or, with no q, clear everything.
router.delete('/', async (req, res) => {
  try {
    const q = cleanQuery(req.body && req.body.q);
    const list = await mutate(req.user.id, (cur) => (q ? cur.filter(([x]) => x !== q) : []));
    res.json(view(list));
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to update search history' });
  }
});

module.exports = router;
module.exports._codec = { encode, decode };
