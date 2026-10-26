// Marketplace order import routes ("buy from order list").
//
//   GET  /api/marketplace/accounts         -> masked per-provider status (what is
//        configured, when it was saved; never a token)
//   PUT  /api/marketplace/accounts         -> save/clear one provider's credentials
//   POST /api/marketplace/preview          -> {source, order_number} -> normalised lines
//   POST /api/marketplace/add              -> same + filing options -> bulk add
//
// The credential columns live on the users row (see db.js migration); this router
// only ever writes them through explicit user action, and GET never returns a
// secret, so a screen-share or a shoulder-peek over the Settings page cannot
// leak a credential, and no /test endpoint echoes a provider response body.
//
// TCGplayer order import was removed (it never worked reliably). The
// tcgplayer_* user columns and tcg_imported_orders table are left in place
// (stored data is kept) but nothing reads or writes them any more.
const express = require('express');
const db = require('../db');
const { authenticateToken } = require('../middleware/auth');
const {
  maskSecret, maskEmail,
  fetchManapoolOrder, parseOrderPayload, addOrderToCollection, previewOrder,
  manapoolRecentOrderSummaries, RECENT_LIMIT,
} = require('../utils/marketplaceOrders');

const router = express.Router();
router.use(authenticateToken);

const SOURCES = ['manapool'];

async function loadRow(userId) {
  return db.get(
    `SELECT manapool_email, manapool_token, manapool_enabled, manapool_saved_at
       FROM users WHERE id = ?`,
    [userId]
  );
}

// What GET may always show: presence booleans + masked hints + timestamps.
function accountView(row) {
  return {
    manapool: {
      configured: !!(row.manapool_email && row.manapool_token),
      enabled: !!row.manapool_enabled,
      email: maskEmail(row.manapool_email),
      token: maskSecret(row.manapool_token),
      savedAt: row.manapool_saved_at || null,
    },
  };
}

router.get('/accounts', async (req, res) => {
  const row = await loadRow(req.user.id);
  if (!row) return res.status(404).json({ error: 'User not found' });
  res.json(accountView(row));
});

// Save (or clear) one provider's credentials. Only the submitting, authenticated
// user's own row is touched. Presence of the provider key in the body is what
// triggers a write; omitted means "don't touch this provider".
router.put('/accounts', async (req, res) => {
  const { manapool } = req.body || {};
  if (manapool === undefined) {
    return res.status(400).json({ error: 'Nothing to save' });
  }
  const row = await loadRow(req.user.id);
  if (!row) return res.status(404).json({ error: 'User not found' });

  const sets = [];
  const params = [];
  const now = new Date().toISOString();

  if (manapool !== undefined) {
    if (manapool === null || manapool.clear) {
      sets.push('manapool_email = NULL', 'manapool_token = NULL', 'manapool_enabled = 0', 'manapool_saved_at = NULL');
    } else {
      const email = String(manapool.email || '').trim();
      const token = String(manapool.token || '').trim();
      if (!email && !token && row.manapool_email && row.manapool_token) {
        // Enabled-only update: the fields are write-only (see GET), so a Save
        // with blank fields on a configured account means "move the switch,
        // keep the credential and its stamp".
        sets.push('manapool_enabled = ?');
        params.push(manapool.enabled === false ? 0 : 1);
      } else if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) {
        return res.status(400).json({ error: 'ManaPool email is not valid' });
      } else if (!token || token.length > 4096) {
        return res.status(400).json({ error: 'A ManaPool access token is required' });
      } else {
        sets.push('manapool_email = ?', 'manapool_token = ?', 'manapool_enabled = ?', 'manapool_saved_at = ?');
        params.push(email, token, manapool.enabled === false ? 0 : 1, now);
      }
    }
  }

  await db.run(`UPDATE users SET ${sets.join(', ')} WHERE id = ?`, [...params, req.user.id]);
  res.json({ ok: true, accounts: accountView(await loadRow(req.user.id)) });
});

// Shared read path for preview+add: resolve provider creds, fetch, parse.
// Sends the error response itself and returns null on any failure, so both
// callers can `if (!out) return;` and stop.
//
// Returning null (never the response object) is load-bearing, not cosmetic:
// an Express `res` is an object, so `return res.status(400).json(...)` here
// would hand the caller a TRUTHY value, sail straight through its
// `if (!out) return;` guard, and let the handler keep going and send a SECOND
// response on an already-finished request — which surfaces as
// ERR_HTTP_HEADERS_SENT and an unhandled rejection, not a clean 400. An earlier
// revision of this helper did exactly that and only the end-to-end pass caught
// it, because every unit test injects the fetcher and skips this read path.
//
// Error shapes are deliberately generic ("source did not return an order")
// rather than echoing provider text.
async function orderFromRequest(req, res) {
  const fail = (status, payload) => { res.status(status).json(payload); return null; };

  const source = String(req.body.source || '').toLowerCase();
  if (!SOURCES.includes(source)) return fail(400, { error: `source must be one of: ${SOURCES.join(', ')}` });
  const number = String(req.body.order_number || '').trim();
  if (!/^[A-Za-z0-9_-]{1,64}$/.test(number)) return fail(400, { error: 'order_number is required' });
  const row = await loadRow(req.user.id);
  if (!row) return fail(404, { error: 'User not found' });

  let fetched;
  try {
    // fetchManapoolOrder's own enabled-gate is unreachable from this route
    // because this lookup happens first — the gate has to live here too, or
    // turning the switch off in Settings is cosmetic for preview/add.
    if (!row.manapool_email || !row.manapool_token || !row.manapool_enabled) return fail(400, { error: 'ManaPool is not configured or is turned off' });
    fetched = await fetchManapoolOrder({ email: row.manapool_email, token: row.manapool_token, orderNumber: number });
  } catch (err) {
    const status = err && err.status ? err.status : 502;
    return fail(status, { error: err.message || 'Order fetch failed' });
  }

  const includeExtras = req.body.include_extras === true;

  try {
    return { parsed: parseOrderPayload(fetched.body, number, { includeExtras }) };
  } catch (err) {
    const status = err && err.status ? err.status : 502;
    return fail(status, { error: err.message || 'Order could not be read', observedKeys: err.observedKeys });
  }
}

// The most recent orders for one source, for the picker list. Same credential
// and enabled gates as the fetch path (a disabled source stays silent here
// too), and the same no-secret-echo rule on errors. ManaPool's card counts
// need one detail fetch each (its list rows carry none). A source that will not answer its list
// reports list_unavailable and the UI keeps the manual number field.
router.get('/recent/:source', async (req, res) => {
  const source = String(req.params.source || '').toLowerCase();
  if (!SOURCES.includes(source)) return res.status(400).json({ error: 'unknown source' });
  const row = await loadRow(req.user.id);
  if (!row) return res.status(404).json({ error: 'User not found' });
  try {
    if (!row.manapool_email || !row.manapool_token || !row.manapool_enabled) {
      return res.status(400).json({ error: 'ManaPool is not configured or is turned off' });
    }
    const offset = Math.max(0, parseInt(req.query.offset, 10) || 0);
    const meta = {};
    const orders = await manapoolRecentOrderSummaries({ email: row.manapool_email, token: row.manapool_token, limit: RECENT_LIMIT, offset, meta });
    return res.json({ source, orders, offset, total: meta.total ?? null, hasMore: meta.total != null ? offset + RECENT_LIMIT < meta.total : false });
  } catch (err) {
    const status = err && err.status ? err.status : 502;
    res.status(status).json({
      error: err.message || 'Recent orders could not be read',
      list_unavailable: err.listUnavailable === true || status === 502,
    });
  }
});

router.post('/preview', async (req, res) => {

  const out = await orderFromRequest(req, res);
  if (!out) return;
  try {
    const preview = await previewOrder({
      lines: out.parsed.lines,
      userId: req.user.id,
      includeExtras: req.body.include_extras === true,
    });
    res.json({ ...out.parsed, ...preview });
  } catch (err) {
    res.status(500).json({ error: err.message || 'Could not resolve the order lines' });
  }
});

// The only route in here that can write to the collection: it funnels through
// the shared bulk-add core (bulkAddToCollection) exactly like the bulk-tray and
// Secret Lair paths do, so quantity/printing/language/condition validation and
// the price-history side effects all behave the same way.
router.post('/add', async (req, res) => {
  const out = await orderFromRequest(req, res);
  if (!out) return;
  const { condition, printing_mode, language, copies } = req.body || {};
  try {
    const result = await addOrderToCollection({
      user: req.user,
      lines: out.parsed.lines,
      condition,
      printingMode: printing_mode,
      language,
      copies,
    });
    res.json({
      order: { number: out.parsed.number, lineCount: out.parsed.lineCount },
      added: result.added,
      resolved: result.resolved,
      totalListed: result.totalListed,
      unresolved: result.unresolved,
      failed: result.failed,
      message: result.added
        ? `Added ${result.added} card${result.added === 1 ? '' : 's'} from order ${out.parsed.number || '?'}.`
        : 'Nothing to add — no cards in the order resolved to catalogue cards.',
    });
  } catch (err) {
    const status = err && err.status ? err.status : 500;
    res.status(status).json({ error: err.message || 'Adding to collection failed' });
  }
});

module.exports = router;
