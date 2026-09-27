// Load .env from the CWD first (how a normal `npm start` from backend/ behaves),
// then from backend/ explicitly. dotenv never overwrites an already-set variable,
// so the first one to define a key still wins and nothing changes for existing
// deployments — this only rescues the case where the server was launched by
// absolute path from some other directory, which silently ignored the file and
// left settings like HTTPS_PORT looking as though they had no effect.
require('dotenv').config();
require('dotenv').config({ path: require('path').join(__dirname, '..', '.env') });

const express = require('express');
const cors = require('cors');
const helmet = require('helmet');
const compression = require('compression');
const path = require('path');
const db = require('./db');
const scryfallApi = require('./scryfallApi');


const authRoutes = require('./routes/auth');
const sharedRoutes = require('./routes/shared');
const adminRoutes = require('./routes/admin');
const collectionRoutes = require('./routes/collection');
const statsRoutes = require('./routes/stats');
const importExportRoutes = require('./routes/importExport');
const manaboxSyncRoutes = require('./routes/manaboxSync');
const setsRoutes = require('./routes/sets');
const decksRoutes = require('./routes/decks');
const listsRoutes = require('./routes/lists');
const preconsRoutes = require('./routes/precons');
const secretLairRoutes = require('./routes/secretLair');
const marketplaceOrdersRoutes = require('./routes/marketplaceOrders');
const settingsRoutes = require('./routes/settings');
const cardArtRoutes = require('./routes/cardArt');
const { authenticateToken } = require('./middleware/auth');
const moxfieldRoutes = require('./routes/moxfield');
const limitedRoutes = require('./routes/limited');
const cardscanRoutes = require('./routes/cardscan');
const rulesRoutes = require('./routes/rules');
const { startHttps, selfSignedTls } = require('./utils/tls');


const app = express();
const PORT = process.env.PORT || 3001;

// The model directory is created here, at startup, as the app's own user —
// deliberately not by the root entrypoint. A directory root creates inside a
// volume that has already been handed over to `node` is one this process can
// never write into, and the first thing to notice was a build dying hours later
// with `EACCES` on its own output.
//
// Probed with a real write rather than fs.access(W_OK): access reports the
// permission bits, which is not the same question as whether this filesystem
// will accept a file (NFS/SMB squash a root-owned mount's bits into a yes it
// does not honour). Neither problem here is fatal — everything except scanning
// still works — so both say so plainly and carry on. Scanning's own failure is a
// 503 from /api/scan-match, which whoever is holding the phone meets long before
// anyone reads these logs.
function checkScanModels() {
  const fs = require('fs');
  const dir = process.env.CV_MODEL_DIR || path.join(__dirname, '..', 'data', 'models');
  try {
    fs.mkdirSync(dir, { recursive: true });
    const probe = path.join(dir, `.write-probe-${process.pid}`);
    fs.writeFileSync(probe, '');
    fs.unlinkSync(probe);
  } catch (err) {
    console.error(`STARTUP: ${dir} is not writable (${err.code}) — catalog builds will fail.`);
    console.error('STARTUP: fix it with  docker exec -u root <container> chown -R node:node /app/database');
    return;
  }
  // The models ship in neither the image nor the repository: they are AGPL-3.0
  // while Scrybox is MIT, so fetching them is the operator's deliberate step. That
  // makes "no models" the ordinary state of a fresh install rather than a fault,
  // and it deserves the command that fixes it instead of silence until someone
  // points a camera at a card.
  const missing = ['cornelius.onnx', 'milo.onnx'].filter(f => !fs.existsSync(path.join(dir, f)));
  if (missing.length) {
    console.warn(`STARTUP: scan models missing from ${dir} (${missing.join(', ')}) — card scanning is disabled.`);
    console.warn('STARTUP: fetch them with  node scripts/fetch-models.mjs   (Docker: docker exec <container> node scripts/fetch-models.mjs)');
  }
}
checkScanModels();

// Behind a reverse proxy (nginx/Traefik/Caddy terminating TLS — effectively
// required, since mobile camera access needs HTTPS), set TRUST_PROXY so req.ip
// and the rate limiters use the real client IP from X-Forwarded-For instead of
// the proxy's. Leave it UNSET when the app is directly exposed: trusting that
// header otherwise lets any client spoof its IP and defeat the rate limiter.
// Accepts a hop count ("1"), "true", or an express trust-proxy string ("loopback").
if (process.env.TRUST_PROXY) {
  const tp = process.env.TRUST_PROXY;
  app.set('trust proxy', tp === 'true' ? true : (Number.isNaN(Number(tp)) ? tp : Number(tp)));
}

// Content Security Policy. Identification is server-side (the client POSTs a
// photo to /api/scan-match); the browser only runs the corner model that draws
// the aiming outline. Either way it needs nothing beyond the app's own bundle
// plus the card-image hosts. Kept Report-Only for now: flip
// `reportOnly` to false to enforce once a production smoke test confirms the
// scan flow and card images load cleanly under these directives.
// ponytail: Report-Only ceiling — enforce after a prod verification pass.
app.use(helmet({
  // HSTS pins the host to HTTPS in the browser. When we terminate TLS ourselves
  // with a self-signed certificate that is a lockout: Chrome stops offering the
  // "proceed anyway" bypass, and http://<host>:3001 gets upgraded too. Left at
  // helmet's default (on) for every other deployment, including a reverse proxy
  // with a real certificate.
  hsts: !selfSignedTls(),
  contentSecurityPolicy: {
    reportOnly: true,
    directives: {
      defaultSrc: ["'self'"],
      // 'wasm-unsafe-eval' is what lets the browser compile WebAssembly. The card
      // scanner runs the cornelius corner model locally through onnxruntime-web to
      // find the card in the frame, and without this the wasm is refused outright.
      // Harmless today because the policy is report-only, but it would silently
      // break scanning the moment that flips.
      scriptSrc: ["'self'", "'wasm-unsafe-eval'"],
      connectSrc: ["'self'"],
      imgSrc: ["'self'", 'data:', 'blob:', 'https://cards.scryfall.io', 'https://c1.scryfall.com', 'https://img.scryfall.com'],
      // index.html loads Antonio, Outfit and Plus Jakarta Sans from Google Fonts,
      // which is two separate origins: the stylesheet comes from fonts.googleapis
      // .com and the .woff2 files it then references come from fonts.gstatic.com.
      // Neither was listed, so every font on every page was a CSP violation —
      // survivable only because the policy is report-only. Enforced as it stood,
      // the whole app would have dropped to the fallback system font.
      styleSrc: ["'self'", "'unsafe-inline'", 'https://fonts.googleapis.com'],
      fontSrc: ["'self'", 'data:', 'https://fonts.gstatic.com'],
      objectSrc: ["'none'"],
      upgradeInsecureRequests: null
    }
  }
}));

// Restrict cross-origin access to known frontend origins. Localhost + private-
// LAN origins are ALWAYS allowed (see PRIVATE_ORIGIN below); CORS_ORIGIN adds
// public origins on top (e.g. a reverse-proxy domain) rather than replacing the
// LAN allowance, so a self-hosted instance behind a proxy stays reachable both
// ways without listing the LAN IP. The Vite dev server runs with host:true +
// HTTPS so the mobile scanner can reach it over the LAN, which makes the
// browser send an Origin like https://192.168.1.20:5173 on writes (PUT/POST/
// DELETE) — GETs are same-origin and send none, which is why only writes were
// being rejected before.
const explicitOrigins = (process.env.CORS_ORIGIN || '')
  .split(',')
  .map(o => o.trim())
  .filter(Boolean);

// The reverse-proxy domain is already configured as PUBLIC_BASE_URL for share
// links, so reuse its origin as an allowed CORS origin — setting it alone is
// enough for proxied logins, no separate CORS_ORIGIN needed.
if (process.env.PUBLIC_BASE_URL) {
  try { explicitOrigins.push(new URL(process.env.PUBLIC_BASE_URL).origin); }
  catch { /* malformed URL — ignore */ }
}

// Loopback + RFC1918 private ranges (10/8, 172.16-31/12, 192.168/16) and
// *.local, with any scheme/port. Not internet-routable, so this is safe for a
// self-hosted app while still blocking arbitrary public websites.
const PRIVATE_ORIGIN = /^https?:\/\/(localhost|127\.\d+\.\d+\.\d+|10\.\d+\.\d+\.\d+|172\.(1[6-9]|2\d|3[01])\.\d+\.\d+|192\.168\.\d+\.\d+|\[::1\]|[a-z0-9-]+\.local)(:\d+)?$/i;

function isAllowedOrigin(origin) {
  if (!origin) return true; // same-origin / non-browser client
  if (PRIVATE_ORIGIN.test(origin)) return true; // localhost + private LAN, always
  // The iOS app's WebView origin. Its scan worker fetches /models and
  // /scan-assets directly (workers bypass CapacitorHttp), so those must answer
  // it. No credentials are allowed cross-origin, and /api stays token-gated.
  if (origin === 'capacitor://localhost') return true;
  return explicitOrigins.includes(origin);
}

app.use(cors({
  origin: (origin, callback) => {
    if (isAllowedOrigin(origin)) {
      callback(null, true);
    } else {
      callback(new Error('Not allowed by CORS'));
    }
  }
}));
// Imports and scanner photos legitimately carry megabytes. All other JSON is ids
// and short strings, so the public/default ceiling stays small. The large parser
// is installed only after the authentication gate below: unauthenticated clients
// must not make the process allocate and parse 15 MB before their token is denied.
const bigJson = express.json({ limit: '15mb' });
const normalJson = express.json({ limit: '1mb' });
const largeJsonPaths = ['/api/import', '/api/scan-match', '/api/search', '/api/cardscan', '/api/manabox-sync'];
app.use((req, res, next) => {
  const needsLargeBody = largeJsonPaths.some(p => req.path === p || req.path.startsWith(`${p}/`));
  return needsLargeBody ? next() : normalJson(req, res, next);
});

// Rebuild any catalog that has fallen behind, right after the weekly set refresh
// discovers new releases.
//
// Without this, a released set lands in the set list and the catalog silently keeps
// answering: a scan of a brand-new card does not fail, it returns the nearest wrong
// card at a confident-looking score. Nobody presses Update because nothing tells
// them to.
//
// Deliberately conservative. It only touches catalogs that ALREADY exist — it never
// creates one, because that is an hours-long job nobody asked for — and it skips
// entirely while a build is running, since catalog.start refuses a second one
// anyway. The embed phase reuses every unchanged vector, so the real cost is
// downloading and embedding only the new cards.
//
// ponytail: env var rather than an app_settings column + settings UI + eleven
// translations. Promote it if users want per-install control from the web UI.
async function autoUpdateCatalogs() {
  if (process.env.CATALOG_AUTO_UPDATE === '0') return;
  const catalog = require('./catalog');
  if (catalog.state()) return;
  try {
    for (const c of await catalog.list()) {
      if (!c.built || !c.newSets) continue;
      console.log(`catalog: ${c.game}/${c.lang} is ${c.newSets} set(s) behind — updating (CATALOG_AUTO_UPDATE=0 to disable)`);
      catalog.start(c.game, c.lang);
      return;   // one at a time; the next tick picks up the next one
    }
  } catch (err) {
    console.error('Catalog auto-update check failed:', err.message);
  }
}

// gzip matters for the API payload as much as for static assets: a
// multi-thousand-card collection's /api/collection response is tens of MB of
// JSON, and must be registered BEFORE the API routes below or it never sees
// them. For the scanner specifically, card detection runs in the browser
// against OpenCV.js, which is an ~11 MB chunk. Uncompressed that is a long
// wait on a phone over wifi and, worse, a stall that looks like a hang.
// Compressed it is ~3.5 MB, and it is immutable-hashed so it is fetched once.
app.use(compression());

// Initialize Database on startup
let dbReady = false;
db.initDb()
  .then(async () => {
    dbReady = true;
    console.log('Database tables verified/created successfully.');

    // Daily Oracle Tags + one-time oracle_id backfill start in the background.
    // Scheduling (not awaiting) here keeps health/application startup independent
    // of both multi-megabyte Scryfall downloads.
    const oracleTags = require('./oracleTags');
    oracleTags.startOracleTagsService();

    // Un-stack legacy multi-quantity entries so every copy is its own row.
    // No-op once migrated.
    const { splitStackedEntries } = require('./utils/collectionHelpers');
    const splitCount = await splitStackedEntries(db);
    if (splitCount > 0) console.log(`Split ${splitCount} stacked collection copies into individual rows.`);

    // Sync sets on startup.
    await scryfallApi.fetchAndCacheSets();
    // Load sets into the card-sort memory cache. The storage half of the old
    // compartmentSort went away with the Storage feature (d2fab84) and took the
    // boot-time import with it; the sort half lives in utils/cardSort, which is
    // what the weekly refresh below still calls. Leaving no declaration in scope
    // made that timer throw ReferenceError on every run, so the catalog
    // auto-update scheduled after it never executed.
    const { loadSetsCache } = require('./utils/cardSort');
    await loadSetsCache(db);


    // Warm the scan models and catalogs. Two ONNX sessions plus an embedding table
    // take ~400 ms to load; paying that on the first scan instead would make the
    // slowest scan of a session the one a user is most likely to judge. Not awaited
    // — listening must not wait on it, and a scan arriving before it finishes just
    // pays the load itself.
    try {
      const cvScan = require('./cvScan');
      const warm = ['mtg'].filter(g => cvScan.isBuilt(g));
      for (const g of warm) {
        cvScan.load(g).catch(err => console.warn(`cvScan ${g} warm-up failed:`, err.message));
      }
      // There is no second matcher to fall back to, so say what is missing. The
      // models alone identify nothing.
      if (!warm.length) console.warn('cvScan: no catalogs present — scanning is disabled until one is built (Settings → Scan catalogs)');
    } catch (err) {
      console.warn('cvScan unavailable:', err.message);
    }

    // Weekly: refresh sets (picks up newly released ones) and reload the
    // in-memory sets cache so chronological sorting stays current without a
    // restart. Scryfall's guidance is that gameplay/set data changes rarely and
    // weekly is plenty — prices are on their own schedule below.
    setInterval(async () => {
      try {
        await scryfallApi.fetchAndCacheSets(true);
        await loadSetsCache(db);
        await autoUpdateCatalogs();
      } catch (err) {
        console.error('Weekly sets refresh failed:', err);
      }
    }, 1000 * 60 * 60 * 24 * 7);

    // Prices. Hourly tick, and NOT forced: shouldSweepPrices decides whether the
    // sweep is actually due, from the admin's price_refresh_days. The daily timer
    // this replaces passed force: true, which skipped that gate entirely and made
    // the configurable interval dead code -- and a daily tick against a daily
    // interval skips on any clock drift at all ('23h 59m elapsed' is not due), so
    // the refresh silently became every other day. An hourly tick that refuses in
    // one indexed read costs nothing and has no such edge.
    //
    // The checkpoint after the sweep retries any WAL truncate that an active
    // startup reader deferred.
    setInterval(() => {
      scryfallApi.updateCollectionPrices().catch((error) => {
        console.error('MTG price update failed:', error.message);
      }).finally(() => {
        oracleTags.checkpointWal();
      });
    }, 1000 * 60 * 60);

    // Shortly after startup, catch up if the last sweep was over a day ago.
    // NOT forced: without that gate this re-ran on every restart, which under
    // nodemon meant a full sweep on every code edit — for data that cannot have
    // changed since the last one.
    setTimeout(async () => {
      // Both jobs use Scryfall. Let the small one-time identity residual finish
      // before a stale install queues thousands of price batches ahead of it;
      // runMaintenance de-duplicates with the Oracle service's own startup run.
      // Unforced, so a sweep still inside the interval does not run.
      await oracleTags.runMaintenance();
      await scryfallApi.updateCollectionPrices().catch((error) => {
        console.error('MTG boot price catch-up failed:', error.message);
      });
      await oracleTags.checkpointWal();
    }, 30000);

    // Periodically purge expired sessions so the table doesn't grow unbounded
    setInterval(() => {
      db.run(`DELETE FROM sessions WHERE expires_at <= DATETIME('now')`).catch(err => {
        console.error('Failed to purge expired sessions:', err);
      });
    }, 1000 * 60 * 60 * 24);

    // Periodic auto-backup (BACKUP_INTERVAL_HOURS, default 24; 0 disables)
    require('./backup').startAutoBackup();

    // Moxfield sync: the 20-second tick polls each tracked author's deck list
    // (slow interval) and checks every tracked deck's contents for changes
    // (fast interval). No-op until somebody adds an author.
    require('./moxfieldScheduler').startMoxfieldScheduler();
  })
  .catch(err => {
    console.error('Failed to initialize database:', err);
  });

// Readiness/liveness probe for orchestrators (Docker HEALTHCHECK, etc.).
// Unauthenticated; pings the DB so a wedged database reads as unhealthy.
// Declared before the /api collection mount so nothing shadows it.
app.get('/api/health', async (req, res) => {
  res.setHeader('X-App-Name', 'Scrybox');
  if (!dbReady) {
    return res.status(503).json({ status: 'db_initializing' });
  }
  try {
    await db.get('SELECT 1');
    res.json({ status: 'ok' });
  } catch (err) {
    res.status(503).json({ status: 'db_unavailable' });
  }
});

// gzip. Mounted HERE, above the routes, because it has to see a response to
// compress it: sitting below the /api mounts it only ever reached the static
// bundle, and every JSON body — a 5000-row collection among them — went out raw.
//
// It matters for the bundle too. The onnxruntime wasm the in-browser detector
// loads is ~13 MB uncompressed, which is a long wait on a phone over wifi and,
// worse, a stall that looks like a hang.
app.use(compression());

// --- PUBLIC API ROUTES ---
// Everything mounted above the gate below is reachable without a session, and
// each one is deliberate: login/register, a public shared collection, and the
// card art that shared collection renders.
app.use('/api/auth', authRoutes);
app.use('/api/shared', sharedRoutes);
// Admin carries its own authenticateToken + requireAdmin, so it sits above the
// gate rather than being authenticated twice.
app.use('/api/admin', adminRoutes);
// Ahead of the bare '/api' mounts so nothing shadows it. Its reads are
// deliberately unauthenticated — a public shared collection renders card art too.
app.use('/api/card-art', cardArtRoutes);

// --- AUTHENTICATION GATE ---
// ONE gate for every remaining /api route, rather than a router.use in each file.
//
// Per-router auth was both a hole and a cost. The hole: routers that forgot it
// (tags, importExport, the audit-log handlers) were protected only because the
// collection router is ALSO mounted at '/api' and its middleware ran first on the
// way past — so authentication depended on the order of the lines below, and
// reordering them would have silently exposed GET /api/export. Worse, an
// unauthenticated request reaching one of those handlers threw on `req.user.id`
// inside an async handler, which Express 4 does not catch: unhandled rejection,
// and launch.js turns that into a process exit.
//
// The cost: bare '/api' mounts stack up, so a late handler re-runs the earlier
// routers' authenticateToken (and its sessions⋈users SELECT) on its way past.
app.use('/api', authenticateToken);

// Parse large payloads only after authentication. The normal parser intentionally
// skipped these paths above so valid imports and scan images retain the 15 MB cap.
app.use('/api/import', bigJson);
app.use('/api/scan-match', bigJson);
app.use('/api/search', bigJson);
app.use('/api/cardscan', bigJson);
app.use('/api/manabox-sync', express.json({ limit: '40mb' }));

// --- AUTHENTICATED API ROUTES ---
app.use('/api', collectionRoutes);
app.use('/api', statsRoutes);
app.use('/api', importExportRoutes);
app.use('/api/sets', setsRoutes);
app.use('/api/decks', decksRoutes);
app.use('/api/lists', listsRoutes);
app.use('/api/precons', preconsRoutes);
app.use('/api/secret-lair', secretLairRoutes);
app.use('/api/marketplace', marketplaceOrdersRoutes);
app.use('/api/settings', settingsRoutes);
app.use('/api/moxfield', moxfieldRoutes);
app.use('/api/limited', limitedRoutes);
app.use('/api/cardscan', cardscanRoutes);
app.use('/api/rules', rulesRoutes);
app.use('/api/manabox-sync', manaboxSyncRoutes);

// The live overlay runs the SAME corner model the scan does, in the browser, so
// what the user aims with and what the server matches cannot disagree. That
// means the browser has to be able to fetch the model — unauthenticated, because
// the outline runs before login is relevant and the file is public weights.
// Immutable: the filename changes when the model does.
// Only the corner model. Serving the whole directory would also expose the
// 56 MB embedding catalog, which the browser never needs and which nobody
// should be able to pull off an install by guessing a filename.
app.get('/models/cornelius.onnx', (req, res) => {
  res.setHeader('Cache-Control', 'public, max-age=2592000, immutable');
  // MODEL_DIR, not a hardcoded backend/data: the container keeps the models on
  // the persisted volume (CV_MODEL_DIR=/app/database/models). Hardcoded, this
  // 404'd in every Docker install, the worker fell back to the pure-JS contour
  // detector, and detection went from ~80ms to 200ms+ with the CPU pegged.
  res.sendFile(path.join(require('./utils/modelAssets').MODEL_DIR, 'cornelius.onnx'), (err) => {
    if (err && !res.headersSent) res.status(404).end();
  });
});

// On-device Scan Cards assets: the OCR recognizer, its dictionary and the
// compact title/printing index (cardscan tools/build_client_index.py writes
// them, content-hashed, plus manifest.json). Hashed names are immutable;
// manifest.json is the one file that must revalidate so a regenerated index is
// picked up. Missing directory = 404s = the client quietly keeps using the
// server scanner.
const clientScanDir = process.env.CLIENT_SCAN_DIR
  || path.join(require('./utils/modelAssets').MODEL_DIR, 'client-scan');
app.use('/scan-assets', express.static(clientScanDir, {
  index: false, dotfiles: 'deny', fallthrough: false,
  setHeaders(res, file) {
    res.setHeader('Cache-Control', file.endsWith('manifest.json')
      ? 'no-cache' : 'public, max-age=31536000, immutable');
  },
}));

// (compression() is registered early, before the API routes — see above.)
const frontendBuildPath = path.join(__dirname, '../../frontend/dist');
// A year, immutable. Vite content-hashes every asset filename, so a changed file
// is a changed URL and a stale cache cannot happen. Without this the browser
// revalidated the entire bundle on every reload — the megabyte-scale wasm
// included — which on a phone is the difference between an instant open and a
// wait. index.html is served by the catch-all below and stays uncached, so a
// deploy is still picked up immediately.
app.use(express.static(frontendBuildPath, { maxAge: '1y', immutable: true, index: false }));

// Catch-all route to serve Index.html in production
app.get('*', (req, res, next) => {
  if (req.path.startsWith('/api')) {
    return next();
  }
  res.sendFile(path.join(frontendBuildPath, 'index.html'));
});

// Generic error handler (e.g. rejected CORS origins) — never leak stack traces to clients
app.use((err, req, res, next) => {
  if (err && err.message === 'Not allowed by CORS') {
    return res.status(403).json({ error: 'Origin not allowed' });
  }
  if (err && err.type === 'entity.too.large') {
    return res.status(413).json({ error: 'Upload too large. Try exporting/importing in smaller batches.' });
  }
  console.error(err);
  res.status(500).json({ error: 'Internal server error' });
});

// Start Express Server
app.listen(PORT, '0.0.0.0', () => {
  console.log(`=========================================`);
  console.log(`Scrybox Server running on port ${PORT}`);
  console.log(`Access local: http://localhost:${PORT}`);
  console.log(`=========================================`);
  // Camera scanning needs a secure context, so a LAN/Docker install serves TLS
  // too when HTTPS_PORT is set. Certificates live beside the database.
  startHttps(app, path.join(path.dirname(db.dbPath), 'ssl'));
});
