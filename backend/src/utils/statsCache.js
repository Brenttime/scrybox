// Small per-user response cache for the dashboard's heavy read endpoints
// (/stats ~0.5s, /stats/history ~0.9s). Correctness rule: ANY non-GET /api
// request by a user bumps that user's generation, so an add/edit/delete is
// never followed by a stale dashboard. A TTL bounds staleness from things a
// user did not do (nightly price refreshes).
const TTL_MS = 5 * 60 * 1000;
const MAX_ENTRIES = 500;
const gen = new Map();      // userId -> generation
const store = new Map();    // key -> { at, body }

function bump(userId) {
  if (userId == null) return;
  gen.set(userId, (gen.get(userId) || 0) + 1);
}
function bumpAll() { store.clear(); }
function keyFor(userId, name) { return `${userId}|${gen.get(userId) || 0}|${name}`; }

function get(userId, name) {
  const k = keyFor(userId, name);
  const hit = store.get(k);
  if (!hit) return null;
  if (Date.now() - hit.at > TTL_MS) { store.delete(k); return null; }
  return hit.body;
}
function set(userId, name, body) {
  if (store.size >= MAX_ENTRIES) store.delete(store.keys().next().value);
  store.set(keyFor(userId, name), { at: Date.now(), body });
}

// Express middleware: after auth, any write by this user invalidates their cache.
function invalidateOnWrite(req, res, next) {
  if (req.method !== 'GET' && req.method !== 'HEAD' && req.user) bump(req.user.id);
  next();
}

// Wrap a GET handler: serve cached JSON, else capture res.json and store it.
function cached(nameFn, handler) {
  return async (req, res, next) => {
    // Disabled under NODE_ENV=test: tests mutate the DB directly (no HTTP
    // write to trigger invalidation) and assert fresh numbers each call.
    const uid = process.env.NODE_ENV === 'test' || process.env.STATS_CACHE === '0' ? null : (req.user && req.user.id);
    const name = nameFn(req);
    const hit = uid != null ? get(uid, name) : null;
    if (hit) { if (typeof res.set === 'function') res.set('X-Stats-Cache', 'hit'); return res.json(hit); }
    const orig = res.json.bind(res);
    res.json = (body) => {
      if (uid != null && res.statusCode < 400) set(uid, name, body);
      if (typeof res.set === 'function') res.set('X-Stats-Cache', 'miss');
      return orig(body);
    };
    return handler(req, res, next);
  };
}

module.exports = { cached, invalidateOnWrite, bump, bumpAll, TTL_MS };
