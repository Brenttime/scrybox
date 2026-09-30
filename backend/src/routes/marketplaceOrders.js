// Marketplace order import routes ("buy from order list").
//
//   GET  /api/marketplace/accounts         -> masked per-provider status (what is
//        configured, when it was saved; never a token, never a cookie jar)
//   PUT  /api/marketplace/accounts         -> save/clear one provider's credentials
//   POST /api/marketplace/preview          -> {source, order_number} -> normalised lines
//   POST /api/marketplace/add              -> same + filing options -> bulk add
//
// The credential columns live on the users row (see db.js migration); this router
// only ever writes them through explicit user action, and GET never returns a
// secret, so a screen-share or a shoulder-peek over the Settings page cannot
// leak a session. TCGplayer cookies are session-grade: possession alone logs a
// caller in anywhere, so they stay server-side and masked here, and the same
// argument is why no /test endpoint echoes a provider response body.
const express = require('express');
const db = require('../db');
const { authenticateToken } = require('../middleware/auth');
const {
  normalizeCookies, cookieCount, maskSecret, maskEmail, customerIdHints,
  fetchManapoolOrder, fetchTcgOrder, parseOrderPayload, addOrderToCollection, previewOrder,
  fetchManapoolOrderList, manapoolRecentOrderSummaries, fetchTcgRecentOrders,
  resolveTcgPageLines,
  recentOrderSummaries, RECENT_LIMIT,
} = require('../utils/marketplaceOrders');

const router = express.Router();
router.use(authenticateToken);

const SOURCES = ['manapool', 'tcgplayer'];

async function loadRow(userId) {
  return db.get(
    `SELECT manapool_email, manapool_token, manapool_enabled, manapool_saved_at,
            tcgplayer_cookies, tcgplayer_enabled, tcgplayer_saved_at
       FROM users WHERE id = ?`,
    [userId]
  );
}

// What GET may always show: presence booleans + masked hints + timestamps. A
// cookie jar is only ever counted, never reproduced.
function accountView(row) {
  return {
    manapool: {
      configured: !!(row.manapool_email && row.manapool_token),
      enabled: !!row.manapool_enabled,
      email: maskEmail(row.manapool_email),
      token: maskSecret(row.manapool_token),
      savedAt: row.manapool_saved_at || null,
    },
    tcgplayer: {
      configured: !!cookieCount(row.tcgplayer_cookies),
      enabled: !!row.tcgplayer_enabled,
      cookies: cookieCount(row.tcgplayer_cookies) ? `${cookieCount(row.tcgplayer_cookies)} cookies saved` : '',
      hint: customerIdHints(row.tcgplayer_cookies).length ? `customer id hint: ${customerIdHints(row.tcgplayer_cookies)[0]}` : '',
      savedAt: row.tcgplayer_saved_at || null,
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
  const { manapool, tcgplayer } = req.body || {};
  if (manapool === undefined && tcgplayer === undefined) {
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

  if (tcgplayer !== undefined) {
    if (tcgplayer === null || tcgplayer.clear) {
      sets.push('tcgplayer_cookies = NULL', 'tcgplayer_enabled = 0', 'tcgplayer_saved_at = NULL');
    } else {
      const jar = normalizeCookies(tcgplayer.cookies ?? req.body.tcgplayer_cookies);
      if (!jar) {
        // Enabled-only update: the field is write-only, so toggling the master
        // switch on an already-configured account arrives with no jar. Keep the
        // stored credential (and its saved_at) and move only the switch.
        if (row.tcgplayer_cookies) {
          sets.push('tcgplayer_enabled = ?');
          params.push(tcgplayer.enabled === false ? 0 : 1);
        } else {
          return res.status(400).json({ error: 'No usable cookies — paste the Cookie header from tcgplayer.com, or an array of {name,value}' });
        }
      } else {
        if (jar.length > 64 * 1024) return res.status(400).json({ error: 'Cookie jar is too large' });
        sets.push('tcgplayer_cookies = ?', 'tcgplayer_enabled = ?', 'tcgplayer_saved_at = ?');
        params.push(jar, tcgplayer.enabled === false ? 0 : 1, now);
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
// rather than echoing provider text, which on TCGplayer can include page HTML.
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
    if (source === 'manapool') {
      // fetchManapoolOrder's own enabled-gate is unreachable from this route
      // because this lookup happens first — the gate has to live here too, or
      // turning the switch off in Settings is cosmetic for preview/add.
      if (!row.manapool_email || !row.manapool_token || !row.manapool_enabled) return fail(400, { error: 'ManaPool is not configured or is turned off' });
      fetched = await fetchManapoolOrder({ email: row.manapool_email, token: row.manapool_token, orderNumber: number });
    } else {
      // Same master-switch gate as ManaPool above: the *_enabled columns are the
      // user's on/off, and nothing else in the read path consults them.
      if (!row.tcgplayer_cookies || !row.tcgplayer_enabled) return fail(400, { error: 'TCGplayer is not configured or is turned off' });
      fetched = await fetchTcgOrder({
        cookies: row.tcgplayer_cookies,
        customerId: req.body.customer_id || customerIdHints(row.tcgplayer_cookies)[0] || null,
        orderNumber: number,
      });
    }
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
// too), and the same no-secret-echo rule on errors. Both sources answer a
// list call; ManaPool's card counts need one detail fetch each (its list rows
// carry none), so its summaries come from manapoolRecentOrderSummaries and
// TCGplayer's from the shared reader. A source that will not answer its list
// reports list_unavailable and the UI keeps the manual number field.
router.get('/recent/:source', async (req, res) => {
  const source = String(req.params.source || '').toLowerCase();
  if (!SOURCES.includes(source)) return res.status(400).json({ error: 'unknown source' });
  const row = await loadRow(req.user.id);
  if (!row) return res.status(404).json({ error: 'User not found' });
  try {
    if (source === 'manapool') {
      if (!row.manapool_email || !row.manapool_token || !row.manapool_enabled) {
        return res.status(400).json({ error: 'ManaPool is not configured or is turned off' });
      }
      const offset = Math.max(0, parseInt(req.query.offset, 10) || 0);
      const meta = {};
      const orders = await manapoolRecentOrderSummaries({ email: row.manapool_email, token: row.manapool_token, limit: RECENT_LIMIT, offset, meta });
      return res.json({ source, orders, offset, total: meta.total ?? null, hasMore: meta.total != null ? offset + RECENT_LIMIT < meta.total : false });
    } else {
      if (!row.tcgplayer_cookies || !row.tcgplayer_enabled) {
        return res.status(400).json({ error: 'TCGplayer is not configured or is turned off' });
      }
      const tcg = await fetchTcgRecentOrders({ cookies: row.tcgplayer_cookies, customerId: req.query.customer_id || null });
      return res.json({ source, orders: recentOrderSummaries(tcg.body, RECENT_LIMIT) });
    }
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

// TCGplayer page import: the bookmarklet already read the order off the user's
// logged-in TCGplayer tab, so no credential is involved here at all. Lines are
// keyed by TCGplayer product id and resolved by identity (see
// resolveTcgPageLines); preview and add run the same resolver, so what the
// preview shows is what gets filed.
async function tcgPageLines(req, res) {
  const raw = req.body && req.body.lines;
  if (!Array.isArray(raw) || !raw.length) { res.status(400).json({ error: 'lines are required' }); return null; }
  try {
    const lines = await resolveTcgPageLines(raw);
    if (!lines.length) { res.status(400).json({ error: 'No TCGplayer product lines were readable' }); return null; }
    return lines;
  } catch (err) {
    res.status(502).json({ error: 'Could not resolve the TCGplayer products' });
    return null;
  }
}

router.post('/tcg-page/preview', async (req, res) => {
  const lines = await tcgPageLines(req, res);
  if (!lines) return;
  try {
    const preview = await previewOrder({ lines, userId: req.user.id });
    const orders = Array.isArray(req.body.orders) ? req.body.orders.map(String).slice(0, 50) : [];
    res.json({
      number: orders.join(', ') || null,
      lines,
      lineCount: lines.length,
      unmatchedNames: lines.__unmatched,
      ...preview,
      unresolved: lines.__unmatched.length,
      extras: 0,
    });
  } catch (err) {
    res.status(500).json({ error: err.message || 'Could not resolve the order lines' });
  }
});

router.post('/tcg-page/add', async (req, res) => {
  const lines = await tcgPageLines(req, res);
  if (!lines) return;
  const { condition, printing_mode, language } = req.body || {};
  try {
    const result = await addOrderToCollection({ user: req.user, lines, condition, printingMode: printing_mode, language });
    res.json({
      added: result.added,
      resolved: result.resolved,
      totalListed: lines.length,
      unresolved: lines.__unmatched.length,
      failed: result.failed,
      message: result.added
        ? `Added ${result.added} card${result.added === 1 ? '' : 's'} from TCGplayer.`
        : 'Nothing to add — no cards on that page matched a known printing.',
    });
  } catch (err) {
    res.status(err && err.status ? err.status : 500).json({ error: err.message || 'Adding to collection failed' });
  }
});

module.exports = router;
