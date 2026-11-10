#!/usr/bin/env node
// Minimal closed-loop HTTP load generator for the Rust-migration baseline (plan section 8).
// Rotates X-Forwarded-For per request so the express-rate-limit windows
// (search 300/15m, auth 20/15m) do not turn the benchmark into a 429 benchmark;
// the target must run with TRUST_PROXY=true. Run ONLY against a DB copy.
//
// usage: node http-bench.mjs <scenarios.json> [--pid <server pid>] [--duration 10] [--concurrency 16]
//        [--write-mix 2000] [--write-base http://127.0.0.1:3312] [--write-id 4]
//
// Realistic mode (BRIEF "honest-benchmark requirement"):
//  - a scenario may carry `paths: [...]`; each request rotates through it
//    round-robin, so no two consecutive requests hit the same URL (defeats
//    the whole-response cache the hot bench slams).
//  - --write-mix <ms> fires a metadata-only collection edit (PUT
//    /collection/<id> {is_trade}) through the Node backend every <ms> and
//    reverts it immediately, so caches invalidate DURING the run. Edits go
//    to --write-base (default: base with :3310 -> :3312) using the auth
//    header of the first scenario that has one. The row must belong to that
//    token's user. The revert is checked at process exit.
import fs from 'node:fs';
import http from 'node:http';

const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args[i + 1] : d; };
const scenarios = JSON.parse(fs.readFileSync(args[0], 'utf8'));
const pid = opt('--pid', null);
const duration = Number(opt('--duration', 10)) * 1000;
const concurrency = Number(opt('--concurrency', 16));
const agent = new http.Agent({ keepAlive: true, maxSockets: concurrency });

const rssKb = () => {
  if (!pid) return null;
  const m = fs.readFileSync(`/proc/${pid}/status`, 'utf8').match(/VmRSS:\s+(\d+)/);
  return m ? Number(m[1]) : null;
};

let ipSeq = 1;
function once(s, base, path) {
  return new Promise((resolve) => {
    const u = new URL(path ?? s.path, base);
    const body = s.body ? JSON.stringify(s.body) : null;
    const n = ipSeq++;
    const headers = { ...(s.headers || {}), 'x-forwarded-for': `10.${(n >> 16) & 255}.${(n >> 8) & 255}.${n & 255}` };
    if (body) { headers['content-type'] = 'application/json'; headers['content-length'] = Buffer.byteLength(body); }
    const t0 = process.hrtime.bigint();
    const req = http.request(u, { method: s.method || 'GET', headers, agent }, (res) => {
      res.on('data', () => {});
      res.on('end', () => resolve({ ms: Number(process.hrtime.bigint() - t0) / 1e6, status: res.statusCode }));
    });
    req.on('error', () => resolve({ ms: Number(process.hrtime.bigint() - t0) / 1e6, status: 0 }));
    if (body) req.write(body);
    req.end();
  });
}

const pct = (a, p) => a[Math.min(a.length - 1, Math.floor((p / 100) * a.length))];
const base = scenarios.base;

// --- write-mix (realistic mode) ---------------------------------------------
const writeMixMs = Number(opt('--write-mix', 0));
const writeBase = opt('--write-base', base.replace(':3310', ':3312'));
const writeId = opt('--write-id', '4');
const writeAuth = scenarios.scenarios.find(s => s.headers?.Authorization)?.headers?.Authorization;
let mixTicks = 0, mixErrors = 0, mixInFlight = null;
function putTrade(v) {
  return new Promise((resolve) => {
    const body = JSON.stringify({ is_trade: v });
    const n = ipSeq++;
    const req = http.request(new URL(`/api/collection/${writeId}`, writeBase), {
      method: 'PUT',
      headers: { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body), Authorization: writeAuth, 'x-forwarded-for': `10.9.${(n >> 8) & 255}.${n & 255}` },
    }, (res) => { res.resume(); res.on('end', () => resolve(res.statusCode)); });
    req.on('error', () => resolve(0));
    req.write(body); req.end();
  });
}
function mixTick() {
  // Edit then revert, one pair per tick; a failed revert is counted loudly —
  // the DB copy MUST be left as found (BRIEF honest-benchmark rule).
  return putTrade(1).then((a) => putTrade(0).then((b) => {
    mixTicks++;
    if (a !== 200 || b !== 200) { mixErrors++; console.error(`write-mix tick failed: PUT1=${a} PUT0=${b}`); }
  }));
}
let mixer = null;
function ensureMixer() {
  if (writeMixMs > 0 && !mixer) {
    if (!writeAuth) throw new Error('--write-mix needs a scenario with an Authorization header');
    mixer = setInterval(() => { mixInFlight = mixTick(); }, writeMixMs);
  }
}

const results = [];
for (const s of scenarios.scenarios) {
  const c = s.concurrency || concurrency;
  const paths = s.paths && s.paths.length ? s.paths : null;
  let rot = 0;
  const nextPath = () => (paths ? paths[rot++ % paths.length] : undefined);
  for (let i = 0; i < 3; i++) await once(s, base, nextPath()); // warm-up
  ensureMixer();
  const lat = []; const codes = {};
  let peak = rssKb() || 0;
  const rssTimer = setInterval(() => { peak = Math.max(peak, rssKb() || 0); }, 100);
  const end = Date.now() + (s.duration ? s.duration * 1000 : duration);
  const t0 = Date.now();
  await Promise.all(Array.from({ length: c }, async () => {
    while (Date.now() < end) {
      const r = await once(s, base, nextPath());
      lat.push(r.ms); codes[r.status] = (codes[r.status] || 0) + 1;
    }
  }));
  clearInterval(rssTimer);
  const secs = (Date.now() - t0) / 1000;
  lat.sort((a, b) => a - b);
  const row = {
    name: s.name, conc: c, n: lat.length, variants: paths ? paths.length : 1, rps: +(lat.length / secs).toFixed(1),
    p50_ms: +pct(lat, 50).toFixed(2), p99_ms: +pct(lat, 99).toFixed(2),
    peak_rss_mb: peak ? +(peak / 1024).toFixed(1) : null, codes,
  };
  results.push(row);
  console.error(JSON.stringify(row));
}
if (mixer) clearInterval(mixer);
if (mixInFlight) await mixInFlight;
console.log(JSON.stringify({ at: new Date().toISOString(), concurrency, duration_s: duration / 1000, write_mix: writeMixMs ? { every_ms: writeMixMs, ticks: mixTicks, errors: mixErrors, target: `${writeBase}/api/collection/${writeId}` } : null, results }, null, 2));
agent.destroy();
