// Small per-user response cache for the dashboard's heavy read endpoints
// (/stats ~0.5s, /stats/history ~0.9s, /stats/networth ~0.3s).
//
// Correctness rules:
//  - Entries are keyed on the DB data revision (collection_read_revision +
//    deck_list_revision + stats_aux_revision). DB triggers bump those on every
//    write to a table the stats read (collection, card_cache, decks/deck_cards,
//    price_history, sets), so background writers (price sweeps, Moxfield sync,
//    catalog hydration) invalidate immediately instead of after the TTL.
//  - ANY non-GET /api request by a user still bumps that user's generation
//    (belt and braces for writes to tables without triggers).
//  - The TTL now only bounds time drift: /stats and /stats/history compute
//    rolling 7d/30d windows from "now", which move without any write.
const TTL_MS = 5 * 60 * 1000;
const MAX_ENTRIES = 500;
const gen = new Map();      // userId -> generation
const store = new Map();    // key -> { at, body }

function bump(userId) {
  if (userId == null) return;
  gen.set(userId, (gen.get(userId) || 0) + 1);
}
function bumpAll() { store.clear(); }

// One cheap read of the three trigger-maintained counters. Null when the
// tables are missing, which falls back to the generation + TTL rules alone.
async function dbRevision() {
  try {
    const db = require('../db'); // lazy: requiring db opens the connection
    const row = await db.get(`
      SELECT (SELECT revision FROM collection_read_revision WHERE id = 1) AS c,
             (SELECT revision FROM deck_list_revision WHERE id = 1) AS d,
             (SELECT revision FROM stats_aux_revision WHERE id = 1) AS a
    `);
    if (!row) return null;
    return `${row.c ?? '-'}.${row.d ?? '-'}.${row.a ?? '-'}`;
  } catch {
    return null;
  }
}
let revisionSource = dbRevision;
function dataRevision() { return revisionSource(); }
// Tests swap the revision source instead of opening a database.
function setRevisionSource(fn) { revisionSource = fn || dbRevision; }

function keyFor(userId, rev, name) { return `${userId}|${gen.get(userId) || 0}|${rev ?? ''}|${name}`; }

function get(key) {
  const hit = store.get(key);
  if (!hit) return null;
  if (Date.now() - hit.at > TTL_MS) { store.delete(key); return null; }
  return hit.body;
}
function set(key, body) {
  if (store.size >= MAX_ENTRIES) store.delete(store.keys().next().value);
  store.set(key, { at: Date.now(), body });
}

// Express middleware: after auth, any write by this user invalidates their cache.
function invalidateOnWrite(req, res, next) {
  if (req.method !== 'GET' && req.method !== 'HEAD' && req.user) bump(req.user.id);
  next();
}

// Disabled under NODE_ENV=test: tests mutate the DB directly and assert fresh
// numbers each call.
function disabled() {
  return process.env.NODE_ENV === 'test' || process.env.STATS_CACHE === '0';
}

// Wrap a GET handler: serve cached JSON, else capture res.json and store it.
// `opts.onHit(body)` may return a per-request copy (e.g. a fresh timestamp).
function cached(nameFn, handler, opts = {}) {
  return async (req, res, next) => {
    const uid = disabled() ? null : (req.user && req.user.id);
    if (uid == null) return handler(req, res, next);
    const name = nameFn(req);
    const rev = await dataRevision();
    const key = keyFor(uid, rev, name);
    const hit = get(key);
    if (hit) {
      if (typeof res.set === 'function') res.set('X-Stats-Cache', 'hit');
      return res.json(opts.onHit ? opts.onHit(hit) : hit);
    }
    const orig = res.json.bind(res);
    res.json = (body) => {
      if (res.statusCode < 400) set(key, body);
      if (typeof res.set === 'function') res.set('X-Stats-Cache', 'miss');
      return orig(body);
    };
    return handler(req, res, next);
  };
}

module.exports = { cached, invalidateOnWrite, bump, bumpAll, dataRevision, setRevisionSource, disabled, TTL_MS };
