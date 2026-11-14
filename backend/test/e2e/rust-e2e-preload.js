// Lane B unit 7b — e2e-on-Rust runner (SCRYBOX_E2E_RUST=1, see run.js).
// SCRYBOX_E2E_DOCKER_IMAGE=<image> (iter-51) swaps the host binary for
// `docker run` of the Node-free image (container arm, see dockerArgsFor).
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
if (!BIN && !process.env.SCRYBOX_E2E_DOCKER_IMAGE) {
  console.error('[rust-e2e] scrybox-server binary not found under server/target/{iter,release}; '
    + 'run ../restart-rust-b.sh (or cargo build --release) or set SCRYBOX_RUST_BIN');
  process.exit(2);
}
const MOCK = path.join(__dirname, 'rust-e2e-mock-server.js');

// Lane B iter-51 — container arm. With SCRYBOX_E2E_DOCKER_IMAGE set (and
// SCRYBOX_E2E_RUST=1), the per-boot server is `docker run` of the Node-free
// image instead of the host-built binary, so the e2e suite gates the exact
// artifact Gate D measures, not just the binary. Semantics held identical to
// the host-binary arm:
//   * --network host — the suites' mock fixture servers and polls live on
//     the host loopback; the container shares it, so every URL the rust arm
//     builds (127.0.0.1:<mock>, 127.0.0.1:<port>) works unchanged.
//   * DB sharing: the directory holding the suite's tmpDb is bind-mounted at
//     /e2e-db and SCRYBOX_DB_PATH points into it. Same inode both sides —
//     WAL/SHM sidecars included — so suites that open tmpDb with
//     better-sqlite3 (schema, deck-value, server) see the server's writes
//     exactly as in host-binary mode. Host uid is 1000 == the image's
//     scrybox uid, so the entrypoint's ownership walk is a no-op on mounts.
//   * Host-path knobs (ORT_DYLIB_PATH, SCRYBOX_JPEG_LIB, CV_MODEL_DIR,
//     MOXFIELD_CURL_BIN, …) are stripped so the container resolves them from
//     the image's own baked paths — that IS the thing under test. Models are
//     mounted from the harness at the image default CV_MODEL_DIR if present.
//   * Lifecycle: named container + --rm; every boot is `docker rm -f`'d when
//     its child exits and again on suite exit (SIGKILLing the docker CLI
//     does not stop a running container).
const DOCKER_IMAGE = process.env.SCRYBOX_E2E_DOCKER_IMAGE || '';
const DOCKER_MODE = !!DOCKER_IMAGE;
const DOCKER_MODELS_DIR = process.env.SCRYBOX_E2E_MODELS_DIR
  || path.resolve(projectRoot, '..', 'data', 'models');
const dockerContainers = [];
// Keys that carry HOST paths or host-process semantics — meaningless (or
// actively wrong) inside the container, where the image bakes its own.
const HOST_ONLY_ENV = new Set([
  'ORT_DYLIB_PATH', 'SCRYBOX_JPEG_LIB', 'CV_MODEL_DIR', 'MOXFIELD_CURL_BIN',
  'CARDSCAN_URL', 'SCRYBOX_STATIC_DIR', 'SCRYBOX_SHARED_DIR', 'DB_PATH',
  'PORT', 'PATH', 'NODE_OPTIONS', 'NODE',
]);
function dockerArgsFor(env, port) {
  const dbHost = env.DB_PATH || env.SCRYBOX_DB_PATH;
  if (!dbHost) {
    console.error('[rust-e2e] docker mode requires DB_PATH in the spawn env');
    process.exit(2);
  }
  const name = `scrybox-e2e-${process.pid}-${dockerContainers.length}`;
  const args = ['run', '--rm', '--name', name, '--network', 'host',
    '-v', `${path.dirname(dbHost)}:/e2e-db`];
  if (fs.existsSync(DOCKER_MODELS_DIR)) {
    args.push('-v', `${DOCKER_MODELS_DIR}:/app/database/models:ro`);
  }
  for (const [k, v] of Object.entries(env)) {
    if (HOST_ONLY_ENV.has(k) || v === undefined || v === null) continue;
    args.push('-e', `${k}=${v}`);
  }
  args.push('-e', `SCRYBOX_DB_PATH=/e2e-db/${path.basename(dbHost)}`);
  args.push('-e', `SCRYBOX_BIND=127.0.0.1:${port}`);
  args.push(DOCKER_IMAGE);
  return { name, args };
}
// Orphan guard: a suite that throws leaves its spawned server holding its
// port; the NEXT run's spawn then fails to bind while waitForServer happily
// answers from the stale process (an old binary without the routes under
// test — the F3-TC4 false negative). Track every BIN child and SIGKILL any
// still alive when the suite process exits. Only OUR children; the :3320
// lane service is never in this list.
const rustChildren = [];
process.on('exit', () => {
  for (const c of rustChildren) { try { c.kill('SIGKILL'); } catch (e) { /* dead */ } }
  // The docker arm's children are docker CLIs; SIGKILLing the CLI leaves the
  // container running. Reap every named container this process booted.
  for (const name of dockerContainers) {
    try { cp.spawnSync('docker', ['rm', '-f', name], { stdio: 'ignore' }); } catch (e) { /* dead */ }
  }
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
    let child;
    if (DOCKER_MODE) {
      const { name, args } = dockerArgsFor(env, port);
      dockerContainers.push(name);
      child = realSpawn('docker', args, { ...(opts || {}), env, stdio: process.env.RUST_E2E_VERBOSE === '1' ? 'inherit' : (opts && opts.stdio) });
      // --rm cleans up on a clean stop, but a killed CLI or a crash can leave
      // the container up holding the port; reap it when the CLI child goes.
      child.on('exit', () => {
        try { cp.spawnSync('docker', ['rm', '-f', name], { stdio: 'ignore' }); } catch (e) { /* dead */ }
      });
    } else {
      child = realSpawn(BIN, [], { ...(opts || {}), env, stdio: process.env.RUST_E2E_VERBOSE === '1' ? 'inherit' : (opts && opts.stdio) });
    }
    rustChildren.push(child);
    // Fail loudly (into the suite's stderr) if the port was already taken:
    // without this the suite just sees the other process's answers.
    child.once('exit', (code) => {
      if (code !== 0 && code !== null) {
        console.error(`[rust-e2e] ${DOCKER_MODE ? `docker run ${DOCKER_IMAGE}` : 'scrybox-server'} (port ${port}) exited with code ${code} — bind conflict or crash?`);
      }
    });
    return child;
  }
  return realSpawn.apply(this, arguments);
};
