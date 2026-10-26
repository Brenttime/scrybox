// Per-user cache for GET /api/decks (~0.4-1s: value, ownership and commander
// scans over every deck). Keyed on deck_list_revision, which DB triggers bump on
// any write to a table the list reads, so a hit is never stale, whoever wrote.
const db = require('../db');

const store = new Map(); // userId -> { revision, body }

async function currentRevision(conn = db) {
  const row = await conn.get(`SELECT revision FROM deck_list_revision WHERE id = 1`);
  return row ? row.revision : null;
}

function enabled() {
  return process.env.DECK_LIST_CACHE !== '0';
}

async function getOrBuild(userId, build, conn = db) {
  if (!enabled()) return { body: await build(), hit: false };
  const revision = await currentRevision(conn);
  const hit = store.get(userId);
  if (hit && revision != null && hit.revision === revision) return { body: hit.body, hit: true };
  const body = await build();
  // Only store if nothing moved while building; otherwise the next call rebuilds.
  if (revision != null && (await currentRevision(conn)) === revision) store.set(userId, { revision, body });
  return { body, hit: false };
}

function clear() { store.clear(); }

module.exports = { getOrBuild, currentRevision, clear };
