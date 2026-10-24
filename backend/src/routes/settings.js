const express = require('express');
const axios = require('axios');
const db = require('../db');
const { requireAdmin } = require('../middleware/auth');

const router = express.Router();

// --- Version + update check ---

// backend/package.json is what the release workflow bumps, so it is the running
// build's version. The repo-root package.json is not bumped and would lie.
const APP_VERSION = require('../../package.json').version;
const RELEASES_API = 'https://api.github.com/repos/Brenttime/scrybox/releases/latest';
const RELEASES_PAGE = 'https://github.com/Brenttime/scrybox/releases';
// GitHub allows 60 unauthenticated calls/hour per IP, shared by every user of
// this instance. Cache hard: a new release is not urgent to the minute.
const UPDATE_CACHE_MS = 1000 * 60 * 60 * 6;
let updateCache = { at: 0, data: null };

// "1.4.9" < "1.4.10" — string compare gets this wrong, so compare numerically
// part by part. Anything non-numeric (a "-beta" suffix) is ignored.
function isNewer(candidate, current) {
  const parts = v => String(v).replace(/^v/i, '').split('.').map(n => parseInt(n, 10) || 0);
  const a = parts(candidate);
  const b = parts(current);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if ((a[i] || 0) !== (b[i] || 0)) return (a[i] || 0) > (b[i] || 0);
  }
  return false;
}

async function checkForUpdate() {
  if (updateCache.data && Date.now() - updateCache.at < UPDATE_CACHE_MS) return updateCache.data;
  const resp = await axios.get(RELEASES_API, {
    timeout: 8000,
    headers: { 'User-Agent': 'Scrybox', Accept: 'application/vnd.github+json' }
  });
  const latest = String(resp.data.tag_name || '').replace(/^v/i, '');
  const data = {
    latest,
    update_available: !!latest && isNewer(latest, APP_VERSION),
    release_url: resp.data.html_url || RELEASES_PAGE,
    published_at: resp.data.published_at || null
  };
  updateCache = { at: Date.now(), data };
  return data;
}

// Current version always answers offline; the update check is best-effort and
// reports its own failure rather than pretending the app is up to date.
router.get('/version', async (req, res) => {
  const base = { version: APP_VERSION, releases_url: RELEASES_PAGE };
  if (req.query.check !== '1') return res.json(base);
  try {
    res.json({ ...base, ...(await checkForUpdate()) });
  } catch (error) {
    console.warn('Update check failed:', error.message);
    res.json({ ...base, check_failed: true });
  }
});

async function getEffectiveSettings() {
  const row = await db.get(`
    SELECT public_base_url, price_refresh_days,
           scan_exclude_tokens, scan_exclude_art_cards, scan_exclude_jumpstart, scan_exclude_promos,
           setup_complete,
           moxfield_decklist_interval_min, moxfield_content_interval_min
    FROM app_settings WHERE id = 1
  `);
  const public_base_url = (row && row.public_base_url) || process.env.PUBLIC_BASE_URL || '';
  const scan_exclude_tokens = !!(row && row.scan_exclude_tokens);
  const scan_exclude_art_cards = !!(row && row.scan_exclude_art_cards);
  const scan_exclude_jumpstart = !!(row && row.scan_exclude_jumpstart);
  const scan_exclude_promos = !!(row && row.scan_exclude_promos);
  const setup_complete = !!(row && row.setup_complete);
  // Moxfield sync cadence. The defaults mirror the migration (60 / 1) so a
  // pre-migration read never looks like "off".
  const moxfield_decklist_interval_min = row ? (row.moxfield_decklist_interval_min || 60) : 60;
  const moxfield_content_interval_min = row ? (row.moxfield_content_interval_min || 1) : 1;
  // Daily unless an admin says otherwise, matching what every install did before
  // the column existed. 0 means automatic refreshes are off.
  const n = Number(row && row.price_refresh_days);
  const price_refresh_days = Number.isInteger(n) && n >= 0 ? n : 1;
  return {
    public_base_url,
    price_refresh_days,
    scan_exclude_tokens,
    scan_exclude_art_cards,
    scan_exclude_jumpstart,
    scan_exclude_promos,
    setup_complete,
    moxfield_decklist_interval_min,
    moxfield_content_interval_min,
    // Read-only env-driven flags (same "effective settings" pattern as
    // public_base_url, which falls back to its env var): the frontend uses
    // these to surface the sidecar/portal only when they are actually on.
    scryfall_local: process.env.LOCAL_SCRYFALL_ENABLED === 'true',
    scryfall_portal_enabled: process.env.LOCAL_WEB_PORTAL_ENABLED === 'true',
  };
}

// Any logged-in user can read effective settings (needed to render share links)
router.get('/', async (req, res) => {
  try {
    res.json(await getEffectiveSettings());
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to retrieve settings' });
  }
});

// Only admins can override settings
router.put('/', requireAdmin, async (req, res) => {
  const {
    public_base_url,

    price_refresh_days,
    scan_exclude_tokens,
    scan_exclude_art_cards,
    scan_exclude_jumpstart,
    scan_exclude_promos,
    setup_complete,
    moxfield_decklist_interval_min,
    moxfield_content_interval_min,
  } = req.body;


  if (price_refresh_days !== undefined) {
    // Rejected rather than clamped: a typo that silently becomes 365 is a
    // collection whose prices quietly stop moving, which looks like the app
    // being broken and not like a setting being wrong.
    const n = Number(price_refresh_days);
    if (!Number.isInteger(n) || n < 0 || n > 30) {
      return res.status(400).json({ error: 'Price refresh interval must be a whole number of days from 0 (off) to 30.' });
    }
    await db.run(`UPDATE app_settings SET price_refresh_days = ? WHERE id = 1`, [n]);
  }

  if (scan_exclude_tokens !== undefined) {
    await db.run(`UPDATE app_settings SET scan_exclude_tokens = ? WHERE id = 1`, [scan_exclude_tokens ? 1 : 0]);
  }
  if (scan_exclude_art_cards !== undefined) {
    await db.run(`UPDATE app_settings SET scan_exclude_art_cards = ? WHERE id = 1`, [scan_exclude_art_cards ? 1 : 0]);
  }
  if (scan_exclude_jumpstart !== undefined) {
    await db.run(`UPDATE app_settings SET scan_exclude_jumpstart = ? WHERE id = 1`, [scan_exclude_jumpstart ? 1 : 0]);
  }
  if (scan_exclude_promos !== undefined) {
    await db.run(`UPDATE app_settings SET scan_exclude_promos = ? WHERE id = 1`, [scan_exclude_promos ? 1 : 0]);
  }
  // Moxfield cadence: minutes, clamped to [1, 1440]. A non-numeric value is
  // rejected rather than coerced to the default, so a typo can't silently
  // re-sync every deck every minute.
  const clampInterval = (v, fallback) => {
    const n = parseInt(v, 10);
    if (!Number.isFinite(n) || n < 1) return null;
    return Math.min(n, 24 * 60);
  };
  if (moxfield_decklist_interval_min !== undefined) {
    const n = clampInterval(moxfield_decklist_interval_min, 60);
    if (n === null) return res.status(400).json({ error: 'Moxfield decklist interval must be a whole number of minutes (1–1440)' });
    await db.run(`UPDATE app_settings SET moxfield_decklist_interval_min = ? WHERE id = 1`, [n]);
  }
  if (moxfield_content_interval_min !== undefined) {
    const n = clampInterval(moxfield_content_interval_min, 1);
    if (n === null) return res.status(400).json({ error: 'Moxfield content interval must be a whole number of minutes (1–1440)' });
    await db.run(`UPDATE app_settings SET moxfield_content_interval_min = ? WHERE id = 1`, [n]);
  }

  if (setup_complete !== undefined) {
    await db.run(`UPDATE app_settings SET setup_complete = ? WHERE id = 1`, [setup_complete ? 1 : 0]);
  }

  if (public_base_url !== undefined) {
    const trimmed = public_base_url.trim();
    if (trimmed && !/^https?:\/\//i.test(trimmed)) {
      return res.status(400).json({ error: 'Public base URL must start with http:// or https://' });
    }
    const cleaned = trimmed.replace(/\/+$/, '');
    await db.run(`UPDATE app_settings SET public_base_url = ? WHERE id = 1`, [cleaned]);
  }

  try {
    res.json(await getEffectiveSettings());
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'Failed to update settings' });
  }
});

module.exports = router;
// Exported for tests.
module.exports.isNewer = isNewer;
