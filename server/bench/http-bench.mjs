#!/usr/bin/env node
// Minimal closed-loop HTTP load generator for the Rust-migration baseline (plan section 8).
// Rotates X-Forwarded-For per request so the express-rate-limit windows
// (search 300/15m, auth 20/15m) do not turn the benchmark into a 429 benchmark;
// the target must run with TRUST_PROXY=true. Run ONLY against a DB copy.
//
// usage: node http-bench.mjs <scenarios.json> [--pid <server pid>] [--duration 10] [--concurrency 16]
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
function once(s, base) {
  return new Promise((resolve) => {
    const u = new URL(s.path, base);
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
const results = [];
for (const s of scenarios.scenarios) {
  const c = s.concurrency || concurrency;
  for (let i = 0; i < 3; i++) await once(s, base); // warm-up
  const lat = []; const codes = {};
  let peak = rssKb() || 0;
  const rssTimer = setInterval(() => { peak = Math.max(peak, rssKb() || 0); }, 100);
  const end = Date.now() + (s.duration ? s.duration * 1000 : duration);
  const t0 = Date.now();
  await Promise.all(Array.from({ length: c }, async () => {
    while (Date.now() < end) {
      const r = await once(s, base);
      lat.push(r.ms); codes[r.status] = (codes[r.status] || 0) + 1;
    }
  }));
  clearInterval(rssTimer);
  const secs = (Date.now() - t0) / 1000;
  lat.sort((a, b) => a - b);
  const row = {
    name: s.name, conc: c, n: lat.length, rps: +(lat.length / secs).toFixed(1),
    p50_ms: +pct(lat, 50).toFixed(2), p99_ms: +pct(lat, 99).toFixed(2),
    peak_rss_mb: peak ? +(peak / 1024).toFixed(1) : null, codes,
  };
  results.push(row);
  console.error(JSON.stringify(row));
}
console.log(JSON.stringify({ at: new Date().toISOString(), concurrency, duration_s: duration / 1000, results }, null, 2));
agent.destroy();
