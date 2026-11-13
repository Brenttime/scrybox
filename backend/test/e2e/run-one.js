// Single-suite e2e runner (lane-B harness convenience). Mirrors run.js's
// child env exactly — including the DEFAULT_ADMIN_PASSWORD it pins — but
// takes the suite file as argv so a single suite can be driven standalone.
'use strict';
const fs = require('fs');
const path = require('path');
const net = require('net');
const cp = require('child_process');

const TEST_DIR = __dirname;
const runnerSrc = fs.readFileSync(path.join(TEST_DIR, 'run.js'), 'utf8');
const pin = runnerSrc.match(/DEFAULT_ADMIN_PASSWORD:\s*'([^']+)'/);
if (!pin) { console.error('could not read admin pin from run.js'); process.exit(2); }

function freePort() {
  return new Promise((resolve) => {
    const s = net.createServer();
    s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); });
  });
}

(async () => {
  const file = process.argv[2];
  const RUST_MODE = process.env.SCRYBOX_E2E_RUST === '1';
  const childEnv = { ...process.env, DEFAULT_ADMIN_PASSWORD: pin[1] };
  if (RUST_MODE) {
    childEnv.NODE_OPTIONS = [process.env.NODE_OPTIONS, `--require=${path.join(TEST_DIR, 'rust-e2e-preload.js')}`]
      .filter(Boolean).join(' ');
    childEnv.E2E_RUST_MOCK_BASE = String((await freePort()) + 1);
  }
  const child = cp.spawn('node', [path.join(TEST_DIR, file)], { env: childEnv });
  child.stdout.on('data', d => process.stdout.write(d));
  child.stderr.on('data', d => process.stderr.write(d));
  child.on('exit', code => process.exit(code || 0));
})();
