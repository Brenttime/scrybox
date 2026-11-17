#!/usr/bin/env node
// Auth bootstrap/register live parity (iter 23; orchestrator audit ruling
// 2026-11-10 #1: bootstrap-success and register-success cannot be verified
// against the shared DB copy — users table is non-empty and this deployment
// runs ALLOW_REGISTRATION=false). So spin up TWO throwaway instances (one
// per implementation: fresh DB + fresh Node container + fresh Rust front,
// ALLOW_REGISTRATION=true), run the identical first-boot ladder on each, and
// compare: config(empty) -> bootstrap(201) -> cross-side session read ->
// register(201) -> duplicate register(400) -> cross-side login of the new
// user -> config(non-empty) -> bootstrap-again(409). Random values (tokens,
// share tokens) are stripped and instead asserted by shape + cross-server
// visibility (owner ruling iter 21: a write must read back from the OTHER
// server). Everything is torn down; the shared DB copy and the 3310/3312
// pair are never touched.
//
// usage: node bench/parity-auth-ephemeral.mjs [--keep] [--verbose]
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const SERVER_DIR = path.resolve(HERE, '..');
const RUST_BIN = path.join(SERVER_DIR, 'target', 'release', 'scrybox-server');
const KEEP = process.argv.includes('--keep');
const VERBOSE = process.argv.includes('--verbose');

const IMAGE = execFileSync('docker', ['inspect', '-f', '{{.Config.Image}}', 'scrybox-rusttest-node']).toString().trim();
if (!IMAGE) throw new Error('cannot resolve scrybox-rusttest-node image');

const BOOT_PW = 'BootstrapPw!42';
const NEWBIE = 'paritynewbie';
const NEWBIE_PW = 'NewbiePass!42';

function sh(args, opt = {}) {
  return execFileSync(args[0], args.slice(1), { encoding: 'utf8', ...opt });
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function req(base, method, p, { headers = {}, body } = {}) {
  const res = await fetch(base + p, { method, headers: { ...headers, 'x-forwarded-for': '203.0.113.77', 'cache-control': 'no-cache' }, body, redirect: 'manual' });
  const text = await res.text();
  let json;
  try { json = JSON.parse(text); } catch { /* non-json */ }
  return { status: res.status, ctype: (res.headers.get('content-type') || '').split(';')[0].trim().toLowerCase(), json, text };
}
function strip(obj, paths) {
  if (!obj || typeof obj !== 'object') return obj;
  const c = structuredClone(obj);
  for (const p of paths) {
    const parts = p.replace(/^\$\.?/, '').split('.');
    let cur = c;
    for (let i = 0; i < parts.length - 1 && cur; i++) cur = cur[parts[i]];
    if (cur && typeof cur === 'object') delete cur[parts[parts.length - 1]];
  }
  return c;
}

async function waitHttp(base, ms, label) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    try { const r = await fetch(base + '/api/health', { signal: AbortSignal.timeout(1500) }); if (r.ok) return; } catch { /* not yet */ }
    await sleep(500);
  }
  throw new Error(`${label} did not come up within ${ms}ms`);
}

async function bootSide(tag, nodePort, rustPort) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `scrybox-eparity-${tag}-`));
  const cname = `scrybox-parity-${tag}`;
  try { sh(['docker', 'inspect', cname], { stdio: 'ignore' }); sh(['docker', 'rm', '-f', cname]); } catch { /* fresh */ }
  sh(['docker', 'create', '--name', cname, '-p', `127.0.0.1:${nodePort}:3001`,
    '-v', `${dir}:/app/database`,
    '-e', 'ALLOW_REGISTRATION=true',
    '-e', 'LOCAL_SCRYFALL_ENABLED=false',
    '-e', 'TRUST_PROXY=loopback, uniquelocal',
    IMAGE]);
  sh(['docker', 'start', cname]);
  const nodeBase = `http://127.0.0.1:${nodePort}`;
  await waitHttp(nodeBase, 90000, `node ${tag}`);
  // The Node container created the schema; the Rust front opens the same file.
  const env = {
    ...process.env,
    SCRYBOX_BIND: `127.0.0.1:${rustPort}`,
    SCRYBOX_NODE_UPSTREAM: nodeBase,
    SCRYBOX_DB_PATH: path.join(dir, 'bindarr.db'),
    RUST_ROUTES: 'auth',
    ALLOW_REGISTRATION: 'true',
    RUST_LOG: 'warn',
  };
  delete env.SCRYBOX_SIDECAR_URL;
  const rust = spawn(RUST_BIN, [], { env, stdio: ['ignore', 'pipe', 'pipe'] });
  let rustLog = '';
  rust.stdout.on('data', (d) => { rustLog += d; });
  rust.stderr.on('data', (d) => { rustLog += d; });
  const rustBase = `http://127.0.0.1:${rustPort}`;
  try { await waitHttp(rustBase, 30000, `rust ${tag}`); }
  catch (e) { e.message += ` | rust log: ${rustLog.slice(0, 400)}`; throw e; }
  return { tag, dir, cname, nodeBase, rustBase, rust };
}

function teardown(side) {
  try { side.rust.kill('SIGTERM'); } catch { /* gone */ }
  if (!KEEP) {
    try { sh(['docker', 'stop', side.cname]); sh(['docker', 'rm', '-f', side.cname]); } catch { /* gone */ }
    try { fs.rmSync(side.dir, { recursive: true, force: true }); } catch { /* gone */ }
  }
}

// The first-boot ladder. `base` is this side's implementation endpoint
// (rustBase for RUST, nodeBase for NODE); `other` is the OPPOSITE
// IMPLEMENTATION OF THE SAME PAIR — it shares this side's fresh DB file, so
// "the write landed and a different implementation can serve reads of it"
// has the same cross-verification meaning as in the shared-DB harness.
async function ladder(base, other) {
  const steps = [];
  const rec = (name, r, ok, why, seen) => steps.push({ name, status: r.status, json: r.json, ok, why, seen });

  let r = await req(base, 'GET', '/api/auth/config');
  rec('config_empty', r, r.status === 200 && r.json?.setupRequired === true && r.json?.registrationEnabled === true, `setupRequired=${r.json?.setupRequired} registration=${r.json?.registrationEnabled}`);

  r = await req(base, 'POST', '/api/auth/bootstrap', { headers: { 'content-type': 'application/json' }, body: JSON.stringify({ password: BOOT_PW }) });
  const bootTok = r.json?.token;
  const bootOk = r.status === 201 && typeof bootTok === 'string' && /^[0-9a-f]{64}$/.test(bootTok)
    && r.json?.user?.username === 'admin' && r.json?.user?.role === 'admin'
    && /^[0-9a-f]{32}$/.test(r.json?.user?.share_token || '');
  rec('bootstrap_201', r, bootOk, `status=${r.status} tok_shape=${typeof bootTok === 'string' && bootTok.length === 64} role=${r.json?.user?.role}`, bootOk ? { message: r.json.message, role: r.json.user.role, username: r.json.user.username, share_enabled: r.json.user.share_enabled, api_key: r.json.user.api_key } : null);
  const bootShare = r.json?.user?.share_token;

  if (bootOk) {
    const me = await req(other, 'GET', '/api/auth/me', { headers: { Authorization: `Bearer ${bootTok}` } });
    rec('bootstrap_session_cross_read', me, me.status === 200 && me.json?.user?.username === 'admin' && me.json?.user?.share_token === bootShare, `opposite-server read of bootstrap session -> ${me.status}`);
  }

  r = await req(base, 'POST', '/api/auth/register', { headers: { 'content-type': 'application/json' }, body: JSON.stringify({ username: 'ParityNewbie ', password: NEWBIE_PW }) });
  const regTok = r.json?.token;
  const regOk = r.status === 201 && typeof regTok === 'string' && /^[0-9a-f]{64}$/.test(regTok)
    && r.json?.user?.username === NEWBIE && r.json?.user?.role === 'member'
    && /^[0-9a-f]{32}$/.test(r.json?.user?.share_token || '');
  rec('register_201', r, regOk, `status=${r.status} username=${r.json?.user?.username} role=${r.json?.user?.role}`, regOk ? { message: r.json.message, username: r.json.user.username, role: r.json.user.role, share_enabled: r.json.user.share_enabled } : null);

  r = await req(base, 'POST', '/api/auth/register', { headers: { 'content-type': 'application/json' }, body: JSON.stringify({ username: NEWBIE, password: NEWBIE_PW }) });
  rec('register_duplicate_400', r, r.status === 400 && r.json?.error === 'Username is already taken', `status=${r.status} error=${JSON.stringify(r.json?.error)}`);

  r = await req(base, 'POST', '/api/auth/register', { headers: { 'content-type': 'application/json' }, body: JSON.stringify({ username: NEWBIE, password: 'short' }) });
  rec('register_short_pw_400', r, r.status === 400, `status=${r.status}`);

  const li = await req(other, 'POST', '/api/auth/login', { headers: { 'content-type': 'application/json' }, body: JSON.stringify({ username: NEWBIE, password: NEWBIE_PW }) });
  rec('register_cross_login', li, li.status === 200 && li.json?.user?.username === NEWBIE && typeof li.json?.token === 'string', `opposite-server login of registered user -> ${li.status}`, li.status === 200 ? { message: li.json.message, username: li.json.user.username, role: li.json.user.role, api_key: li.json.user.api_key } : null);

  r = await req(base, 'GET', '/api/auth/config');
  rec('config_nonempty', r, r.status === 200 && r.json?.setupRequired === false, `setupRequired=${r.json?.setupRequired}`);

  r = await req(base, 'POST', '/api/auth/bootstrap', { headers: { 'content-type': 'application/json' }, body: JSON.stringify({ password: BOOT_PW }) });
  rec('bootstrap_again_409', r, r.status === 409 && !!r.json?.error, `status=${r.status}`);

  return steps;
}

const diffs = [];
function compareSteps(name, rs, ns) {
  if (rs.ok !== ns.ok) diffs.push(`${name}: gate ${rs.ok ? 'rust-ok' : 'rust-FAIL(' + rs.why + ')'} vs ${ns.ok ? 'node-ok' : 'node-FAIL(' + ns.why + ')'}`);
  if (rs.status !== ns.status) diffs.push(`${name}: status ${rs.status} vs ${ns.status}`);
  const [sj, nj] = [rs.json, ns.json];
  if ((sj === undefined) !== (nj === undefined)) diffs.push(`${name}: json presence ${sj === undefined} vs ${nj === undefined}`);
  else if (sj !== undefined) {
    const d = [];
    (function dd(a, b, p) {
      if (d.length >= 4) return;
      if (Array.isArray(a) || Array.isArray(b)) { if (JSON.stringify(a) !== JSON.stringify(b)) d.push(`${p}: array`); return; }
      if (a && b && typeof a === 'object' && typeof b === 'object') {
        const ka = Object.keys(a).join('\0'), kb = Object.keys(b).join('\0');
        if (ka !== kb) { d.push(`${p}: keys ${ka} vs ${kb}`); return; }
        for (const k of Object.keys(a)) dd(a[k], b[k], `${p}.${k}`);
        return;
      }
      if (a !== b) d.push(`${p}: ${JSON.stringify(a)} vs ${JSON.stringify(b)}`);
    })(sj, nj, '$');
    d.forEach((x) => diffs.push(`${name}: ${x}`));
  }
  try { deepSeen(name, rs.seen, ns.seen); } catch (e) { diffs.push(`${name}: seen ${e.message}`); }
}
function deepSeen(name, a, b, p = '$seen') {
  if (a === undefined && b === undefined) return;
  if (JSON.stringify(a) !== JSON.stringify(b)) diffs.push(`${name}: ${p} ${JSON.stringify(a)} vs ${JSON.stringify(b)}`);
}

let R, N;
let fatal = null;
try {
  if (!fs.existsSync(RUST_BIN)) throw new Error(`missing ${RUST_BIN} (cargo build --release)`);
  console.log(`ephemeral auth ladder — image=${IMAGE} (throwaway pair per side, ALLOW_REGISTRATION=true)`);
  R = await bootSide('r', 3314, 3313);
  N = await bootSide('n', 3316, 3315);
  // Rust ladder runs first (writes pair R's DB through the Rust front; the
  // Node container of the same pair then serves the cross-read of those
  // rows), then the Node ladder against pair N (cross-read via that pair's
  // Rust front). Serial keeps each ladder's cross-read deterministic.
  const rs = await ladder(R.rustBase, R.nodeBase);
  const ns = await ladder(N.nodeBase, N.rustBase);
  for (let i = 0; i < rs.length; i++) {
    const a = rs[i], b = ns[i];
    if (!a || a.name !== b.name) { diffs.push(`step order diverged at ${i}`); continue; }
    if (VERBOSE) console.log(`  ${a.name}: rust(${a.status},${a.ok}) node(${b.status},${b.ok})`);
    compareSteps(a.name, stripJson(a), stripJson(b));
  }
} catch (e) {
  fatal = e;
} finally {
  if (R) teardown(R);
  if (N) teardown(N);
}

function stripJson(step) {
  const s = { ...step };
  if (s.json) s.json = strip(s.json, ['$.token', '$.user.share_token']);
  return s;
}

if (fatal) { console.log(`FATAL: ${fatal.message}`); process.exit(2); }
if (diffs.length) { console.log(`FAIL (${diffs.length} diffs):`); diffs.forEach((d) => console.log('  · ' + d)); process.exit(1); }
console.log('PASS: bootstrap/register first-boot ladder Rust-vs-Node equal (13 assertions)');
