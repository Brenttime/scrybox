// Lane B unit 7b — e2e-on-Rust runner (SCRYBOX_E2E_RUST=1, see run.js).
//
// Loaded via NODE_OPTIONS=--require into each e2e SUITE process. It rewrites
// the one thing every suite does identically — spawn('node', [...server.js])
// — into a spawn of the Rust binary with the standalone (Node-free) env, so
// the 13 suites run untouched with the Node container stopped.
//
// Env translation (Node server.js knobs -> scrybox-server knobs):
//   PORT              -> SCRYBOX_BIND=127.0.0.1:PORT
//   DB_PATH           -> SCRYBOX_DB_PATH (schema+admin come from RUST_MIGRATIONS,
//                        the db.js initDb twin; suites poll waitForDatabase)
//   (unset)           -> SCRYBOX_NODE_UPSTREAM='' (standalone: unported routes 502)
//   (unset)           -> RUST_ROUTES='*' (every ported group native)
//   HTTPS_PORT        -> forced '' (no TLS listener; matches the suites)
//   SCRYFALL_*        -> pinned to a per-spawn fixture server (rust-e2e-mock-server.js,
//                        a child process that preloads scryfall-mock.js — the SAME
//                        fixture module the Node suites -r preload, so fixtures
//                        are reused verbatim, not duplicated). MOCK_SCRYFALL_*
//                        flags ride the child env, as the suites set them.
// The axios prototype patch deliberately lives in the mock-server PROCESS,
// never here: patching it in the suite process would intercept the suite's own
// HTTP helpers, a semantic the Node-mode suites never have.
'use strict';
const path = require('path');
const fs = require('fs');
const cp = require('child_process');

const SERVER_MARK = path.join('backend', 'src', 'server.js');
const projectRoot = path.resolve(__dirname, '..', '..', '..');
const BIN = process.env.SCRYBOX_RUST_BIN
  || ['target/iter', 'target/release']
       .map(d => path.join(projectRoot, 'server', d, 'scrybox-server'))
       .find(p => fs.existsSync(p));
if (!BIN) {
  console.error('[rust-e2e] scrybox-server binary not found under server/target/{iter,release}; '
    + 'run ../restart-rust-b.sh (or cargo build --release) or set SCRYBOX_RUST_BIN');
  process.exit(2);
}
const MOCK = path.join(__dirname, 'rust-e2e-mock-server.js');
// Orphan guard: a suite that throws leaves its spawned server holding its
// port; the NEXT run's spawn then fails to bind while waitForServer happily
// answers from the stale process (an old binary without the routes under
// test — the F3-TC4 false negative). Track every BIN child and SIGKILL any
// still alive when the suite process exits. Only OUR children; the :3320
// lane service is never in this list.
const rustChildren = [];
process.on('exit', () => {
  for (const c of rustChildren) { try { c.kill('SIGKILL'); } catch (e) { /* dead */ } }
});
// Base port for this suite's fixture servers, allocated (and released) by
// run.js right before the suite spawns. +seq per server spawn (scryfall.test
// boots up to 9). E2E_RUST_MOCK_BASE lets a single suite run standalone.
let nextMockPort = Number(process.env.E2E_RUST_MOCK_BASE) || (39000 + (process.pid % 900));

const realSpawn = cp.spawn;
cp.spawn = function (cmd, args, opts) {
  if (cmd === 'node' && Array.isArray(args) && args.some(a => typeof a === 'string' && a.endsWith(SERVER_MARK))) {
    const env = { ...(opts && opts.env ? opts.env : process.env) };
    const port = env.PORT || '3001';
    const mp = nextMockPort++;
    // Fixture server per server-boot (matches Node semantics: one mock state
    // per server process, incl. per-boot MOCK_SCRYFALL_ERROR/DELAY flags).
    // stdio ignore + unref: never keeps the suite alive; ppid watchdog in the
    // mock child reaps orphans if a suite forgets process.exit().
    const mock = realSpawn(process.execPath, [MOCK, String(mp)], {
      stdio: ['ignore', 'ignore', 'inherit'], env: { ...env },
    });
    if (typeof mock.unref === 'function') mock.unref();
    env.SCRYBOX_BIND = `127.0.0.1:${port}`;
    if (env.DB_PATH) env.SCRYBOX_DB_PATH = env.DB_PATH;
    env.SCRYBOX_NODE_UPSTREAM = '';
    env.RUST_ROUTES = '*';
    env.RUST_MIGRATIONS = 'true';
    env.HTTPS_PORT = '';
    env.SCRYFALL_GAP_SCALE = '0';
    env.SCRYFALL_BASE_URL = `http://127.0.0.1:${mp}/api.scryfall.com`;
    // Upstream mode — matching how the Node suites run (they never set
    // LOCAL_SCRYFALL_ENABLED, so scryfallApi answers mode-local queries from
    // SQLite and resolves catalog queries via the upstream base). Pinning
    // LOCAL_SCRYFALL here would put the Rust side in sidecar mode, which the
    // Node suites never exercise (and which made e.g. `is:land` unanswerable
    // against the fixture: the sidecar walk leg proxies it in hybrid, and
    // there is no hybrid here). The sidecar URL stays: it is the walk-leg
    // transport (the fixture answers both shapes) and the kill-switch target
    // for the iter-10 sidecar-off cases.
    delete env.LOCAL_SCRYFALL_ENABLED;
    delete env.LOCAL_SCRYFALL_URL;
    env.SCRYBOX_SIDECAR_URL = `http://127.0.0.1:${mp}/api.scryfall.com`;
    env.SCRYBOX_MTGJSON_BASE = `http://127.0.0.1:${mp}/mtgjson.com/api/v5`;
    env.MOXFIELD_API_BASE = `http://127.0.0.1:${mp}/api2.moxfield.com`;
    env.SCRYBOX_DISABLE_MOXFIELD_SCHEDULER = '1';
    env.SCRYBOX_DISABLE_AUTO_BACKUP = '1';
    delete env.PORT;
    delete env.SCRYBOX_STATIC_DIR;
    delete env.RUST_JOBS;
    delete env.RUST_MIGRATIONS_B;
    const child = realSpawn(BIN, [], { ...(opts || {}), env, stdio: process.env.RUST_E2E_VERBOSE === '1' ? 'inherit' : (opts && opts.stdio) });
    rustChildren.push(child);
    // Fail loudly (into the suite's stderr) if the port was already taken:
    // without this the suite just sees the other process's answers.
    child.once('exit', (code) => {
      if (code !== 0 && code !== null) {
        console.error(`[rust-e2e] scrybox-server (port ${port}) exited with code ${code} — bind conflict or crash?`);
      }
    });
    return child;
  }
  return realSpawn.apply(this, arguments);
};
