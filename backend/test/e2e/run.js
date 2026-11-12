const fs = require('fs');
const net = require('net');
const path = require('path');
const { spawn } = require('child_process');

// Lane B unit 7b — e2e-on-Rust mode. With SCRYBOX_E2E_RUST=1 every suite's
// `spawn('node', [server.js])` is rewritten (via the NODE_OPTIONS preload)
// into a scrybox-server boot: standalone (SCRYBOX_NODE_UPSTREAM=''), all
// ported route groups native (RUST_ROUTES=*), and Scryfall/MTGJSON/Moxfield
// pinned to a per-boot fixture server that reuses scryfall-mock.js verbatim.
// The Node container can be fully stopped. See rust-e2e-preload.js.
const RUST_MODE = process.env.SCRYBOX_E2E_RUST === '1';
const RUST_PRELOAD = path.join(__dirname, 'rust-e2e-preload.js');

function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.once('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const p = s.address().port;
      s.close(() => resolve(p));
    });
  });
}

const TEST_DIR = __dirname;
const files = fs.readdirSync(TEST_DIR)
  .filter(f => f.endsWith('.test.js') && f !== 'run.js')
  .sort();

let totalPassed = 0;
let totalFailed = 0;
let passedSuites = 0;
let failedSuites = 0;

async function runTestFile(file) {
  // Fixture-server port pool for this suite (one port per server boot);
  // allocated outside the Promise executor, which is not async.
  const mockBase = RUST_MODE ? (await freePort()) + 1 : null;
  return new Promise((resolve) => {
    console.log(`\n=========================================`);
    console.log(`RUNNING TEST SUITE: ${file}`);
    console.log(`=========================================`);
    
    // Every suite drives the API as the 'admin' account. initDb only seeds that
    // account when a password is pinned — a real fresh install gets its owner
    // from the setup wizard instead, and a headless test has no browser.
    const childEnv = { ...process.env, DEFAULT_ADMIN_PASSWORD: 'test-admin-password' };
    if (RUST_MODE) {
      childEnv.NODE_OPTIONS = [process.env.NODE_OPTIONS, `--require=${RUST_PRELOAD}`]
        .filter(Boolean).join(' ');
      childEnv.E2E_RUST_MOCK_BASE = String(mockBase);
    }
    const child = spawn('node', [path.join(TEST_DIR, file)], {
      env: childEnv,
    });
    
    let filePassed = 0;
    let fileFailed = 0;
    
    child.stdout.on('data', (data) => {
      const output = data.toString();
      process.stdout.write(output);
      
      const passMatches = output.match(/PASS: F\d-TC\d+/g);
      if (passMatches) {
        filePassed += passMatches.length;
      }
    });

    child.stderr.on('data', (data) => {
      const output = data.toString();
      process.stderr.write(output);
      
      const failMatches = output.match(/FAIL: F\d-TC\d+/g);
      if (failMatches) {
        fileFailed += failMatches.length;
      }
    });
    
    child.on('close', (code) => {
      if (code === 0) {
        console.log(`PASS: ${file} executed successfully.`);
        passedSuites++;
      } else {
        console.error(`FAIL: ${file} exited with code ${code}.`);
        failedSuites++;
        if (fileFailed === 0) {
          fileFailed = 1;
        }
      }
      
      totalPassed += filePassed;
      totalFailed += fileFailed;
      
      resolve();
    });
  });
}

async function main() {
  if (RUST_MODE) {
    console.log('E2E RUST MODE: scrybox-server standalone (Node-free), RUST_ROUTES=*.');
    const only = (process.env.E2E_ONLY || '').split(',').map(s => s.trim()).filter(Boolean);
    if (only.length) {
      files.length = 0;
      files.push(...only);
    }
  }
  console.log(`Discovered ${files.length} E2E test files under ${TEST_DIR}.`);
  for (const file of files) {
    await runTestFile(file);
  }
  
  console.log(`\n=========================================`);
  console.log(`E2E TEST RUNNER RESULT SUMMARY:`);
  console.log(`  Passed suites: ${passedSuites} / ${files.length}`);
  console.log(`  Failed suites: ${failedSuites} / ${files.length}`);
  console.log(`  Passed test cases: ${totalPassed}`);
  console.log(`  Failed test cases: ${totalFailed}`);
  console.log(`  Total test cases:  ${totalPassed + totalFailed}`);
  console.log(`=========================================`);
  
  process.exit(failedSuites > 0 ? 1 : 0);
}

main().catch(err => {
  console.error('Unhandled runner error:', err);
  process.exit(1);
});
