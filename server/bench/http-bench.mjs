#!/usr/bin/env node
// Minimal closed-loop HTTP load generator for the Rust-migration baseline (plan section 8).
// Rotates X-Forwarded-For per request so the express-rate-limit windows
// (search 300/15m, auth 20/15m) do not turn the benchmark into a 429 benchmark;
// the target must run with TRUST_PROXY=true. Run ONLY against a DB copy.
//
// usage: node http-bench.mjs <scenarios.json> [--pid <server pid>] [--duration 10] [--concurrency 16]
//        [--write-mix 2000] [--write-base http://127.0.0.1:3312] [--write-id 4]
//        [--workers N]
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
//
// Multi-process mode (--workers N, iter 11): a single Node bench process
// saturates one CPU core at ~7k req/s (measured on the 4-core bench host),
// so hot runs capped stats at 7.2k rps with the SERVER at 0.56 cores — the
// harness, not the server, was the ceiling. --workers N spawns N child bench
// processes (per-worker concurrency, disjoint XFF ranges via a per-worker
// ipSeq offset, staggered round-robin phase) and merges counts/percentiles
// across them over the shared wall-clock window. The write-mix runs in
// worker 0 ONLY, so "every ~2 s" keeps its meaning. Scenarios carrying
// `"workers": 1` (the 27MB full-collection routes — bandwidth/memcpy-bound,
// where extra streams only add noise) are excluded from children and run
// once inline by the parent, keeping their single-stream shape.
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import { spawn } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';

const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args[i + 1] : d; };
const scenarios = JSON.parse(fs.readFileSync(args[0], 'utf8'));
const pid = opt('--pid', null);
const duration = Number(opt('--duration', 10)) * 1000;
const concurrency = Number(opt('--concurrency', 16));
const workers = Number(opt('--workers', 1));
const isChild = args.includes('--_child');
const outPath = opt('--_out', null);
// Child role: --_rot <i> staggers the paths round-robin phase; the ipSeq
// offset below keeps X-Forwarded-For values disjoint across workers (the
// express rate limiter keys on client IP).
const rotOffset = Number(opt('--_rot', '0'));
const agent = new http.Agent({ keepAlive: true, maxSockets: concurrency });

const rssKb = () => {
  if (!pid) return null;
  const m = fs.readFileSync(`/proc/${pid}/status`, 'utf8').match(/VmRSS:\s+(\d+)/);
  return m ? Number(m[1]) : null;
};

let ipSeq = 1 + rotOffset * 4000000; // disjoint XFF ranges per worker
function once(s, base, path) {
  return new Promise((resolve) => {
    const u = new URL(path ?? s.path, base);
    const body = s.body ? JSON.stringify(s.body) : null;
    const n = ipSeq++;
    // 198.18.0.0/15 (RFC 2544 benchmarking range): MUST stay untrusted under
    // the Rust front's TRUST_PROXY="loopback, uniquelocal" — rotating into
    // 10.x made every synthetic IP a "trusted proxy" there, collapsing all
    // bench traffic onto the socket IP's single search bucket (429 storm).
    const headers = { ...(s.headers || {}), 'x-forwarded-for': `198.${18 + ((n >> 16) & 1)}.${(n >> 8) & 255}.${n & 255}` };
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

// --- fast keep-alive client (iter 11) ----------------------------------------
// Node's built-in http client costs ~138us CPU/req (profiled: ClientRequest
// ctor, agent bookkeeping, header emit, parser glue) — at closed-loop
// concurrency 16 that saturated the client's own event loop at ~7.2k rps and
// turned p50 into client-side queueing (p50 == conc/rps in a closed loop).
// curl shows true server latency <0.5ms, so the harness was lying. FastConn
// talks HTTP/1.1 keep-alive on a raw socket: request bytes are assembled by
// string concat (~1us), response is parsed against content-length (chunked
// fallback). Status line is verified; bodies are counted, not parsed
// (parity.mjs is the correctness gate). --slow-client falls back to the
// built-in client.
const useFast = !args.includes('--slow-client');
const _u = new URL(base);
const FAST_HOST = _u.hostname, FAST_PORT = Number(_u.port || 80);

class FastConn {
  static all = [];
  constructor() {
    this.buf = Buffer.alloc(0);
    this.pending = null;
    this.sock = null;
    FastConn.all.push(this);
    this.connect();
  }
  static closeAll() { for (const c of FastConn.all) { c.buf = Buffer.alloc(0); c.sock?.destroy(); c.sock = null; } FastConn.all = []; }
  connect() {
    this.sock = net.connect(FAST_PORT, FAST_HOST);
    this.sock.on('data', (d) => { this.buf = this.buf.length ? Buffer.concat([this.buf, d]) : d; this.drain(); });
    const fail = () => { // drop the socket; the next request reconnects.
      if (this.pending) { const p = this.pending; this.pending = null; p.resolve({ ms: Number(process.hrtime.bigint() - p.t0) / 1e6, status: 0 }); }
      this.buf = Buffer.alloc(0);
      this.sock?.destroy();
      this.sock = null;
    };
    this.sock.on('error', fail);
    this.sock.on('close', fail);
  }
  request(reqBytes) {
    if (!this.sock) this.connect();
    return new Promise((resolve) => {
      const fire = () => {
        const t0 = process.hrtime.bigint();
        this.pending = { t0, resolve };
        this.sock.write(reqBytes);
      };
      if (this.sock.writable && !this.pending) fire();
      else this.sock.once('connect', fire);
    });
  }
  drain() {
    while (this.pending) {
      const hEnd = this.buf.indexOf('\r\n\r\n');
      if (hEnd < 0) return;
      const head = this.buf.toString('latin1', 0, hEnd);
      const status = Number(head.slice(9, 12));
      const clMatch = /content-length: *(\d+)/i.exec(head);
      const chunked = /transfer-encoding: *chunked/i.test(head);
      if (chunked) {
        // minimal chunked framing walk; incomplete → return and wait.
        let off = hEnd + 4;
        let done = false;
        for (;;) {
          const nl = this.buf.indexOf('\r\n', off);
          if (nl < 0) return;
          const size = parseInt(this.buf.toString('latin1', off, nl), 16);
          if (Number.isNaN(size)) { this.failProtocol(); return; }
          if (size === 0) { done = true; off = nl + 2; break; }
          off = nl + 2 + size + 2;
          if (off > this.buf.length) return;
        }
        if (!done) return;
        this.complete(status, off);
        return;
      }
      const need = hEnd + 4 + (clMatch ? Number(clMatch[1]) : 0); // 304/204: no body
      if (this.buf.length < need) return;
      this.complete(status, need);
    }
  }
  failProtocol() {
    const p = this.pending; this.pending = null;
    this.buf = Buffer.alloc(0);
    p?.resolve({ ms: Number(process.hrtime.bigint() - p.t0) / 1e6, status: -1 }); // loud in codes{}
    this.sock?.destroy(); this.sock = null;
  }
  complete(status, consumed) {
    const p = this.pending; this.pending = null;
    this.buf = this.buf.length > consumed ? this.buf.subarray(consumed) : Buffer.alloc(0);
    p.resolve({ ms: Number(process.hrtime.bigint() - p.t0) / 1e6, status });
  }
}

function fastRequestBytes(s, xffPath) {
  const p = xffPath ?? s.path;
  let m = `${s.method || 'GET'} ${p} HTTP/1.1\r\nHost: ${FAST_HOST}:${FAST_PORT}\r\n`;
  if (s.headers) for (const [k, v] of Object.entries(s.headers)) m += `${k}: ${v}\r\n`;
  const n = ipSeq++;
  m += `x-forwarded-for: 198.${18 + ((n >> 16) & 1)}.${(n >> 8) & 255}.${n & 255}\r\nconnection: keep-alive\r\n\r\n`;
  return Buffer.from(m, 'latin1');
}

// --- write-mix (realistic mode) ---------------------------------------------
// Worker 0 only (rotOffset 0): N workers each firing the edit/revert pair
// every 2 s would multiply the write pressure by N.
const writeMixMs = isChild && rotOffset !== 0 ? 0 : Number(opt('--write-mix', 0));
// Same foot-gun class as parity.mjs's old NODE default (lane-B queue):
// replacing ':3310' in a non-default --base silently no-ops and the write
// mix lands on the bench target itself instead of the Node twin. Derive
// from the scenario base, and refuse a same-target write mix.
const writeBase = opt('--write-base', scenarios.base.replace(':3310', ':3312'));
if (writeMixMs > 0 && writeBase === base) {
  console.error(`http-bench: write-base resolves to the bench base (${base}) — writes would hit the measured server. Pass --write-base explicitly.`);
  process.exit(2);
}
const writeId = opt('--write-id', '4');
const writeAuth = scenarios.scenarios.find(s => s.headers?.Authorization)?.headers?.Authorization;
let mixTicks = 0, mixErrors = 0, mixInFlight = null;
function putTrade(v) {
  return new Promise((resolve) => {
    const body = JSON.stringify({ is_trade: v });
    const n = ipSeq++;
    const req = http.request(new URL(`/api/collection/${writeId}`, writeBase), {
      method: 'PUT',
      headers: { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body), Authorization: writeAuth, 'x-forwarded-for': `198.19.${(n >> 8) & 255}.${n & 255}` },
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

async function runScenarios(list, tag) {
  const results = [];
  for (const s of list) {
    const c = s.concurrency || concurrency;
    const paths = s.paths && s.paths.length ? s.paths : null;
    let rot = rotOffset; // stagger workers so they don't hit the same URL together
    const nextPath = () => (paths ? paths[rot++ % paths.length] : undefined);
    // Fast path: raw keep-alive sockets. Built-in client for POST bodies and
    // for the 27MB `"workers": 1` routes (accumulating a 27MB body in
    // FastConn.buf would re-Buffer.concat per chunk — stream it instead).
    const fast = useFast && !s.body && s.workers !== 1;
    const fire = fast
      ? (() => { const conns = Array.from({ length: c }, () => new FastConn()); return (i) => conns[i].request(fastRequestBytes(s, nextPath())); })()
      : (i) => once(s, base, nextPath());
    for (let i = 0; i < 3; i++) await fire(0); // warm-up
    ensureMixer();
    const lat = []; const codes = {};
    let peak = rssKb() || 0;
    const rssTimer = setInterval(() => { peak = Math.max(peak, rssKb() || 0); }, 100);
    const end = Date.now() + (s.duration ? s.duration * 1000 : duration);
    const t0 = Date.now();
    await Promise.all(Array.from({ length: c }, async (_, i) => {
      while (Date.now() < end) {
        const r = await fire(i);
        lat.push(r.ms); codes[r.status] = (codes[r.status] || 0) + 1;
      }
    }));
    clearInterval(rssTimer);
    const secs = (Date.now() - t0) / 1000;
    lat.sort((a, b) => a - b);
    const row = {
      name: s.name, conc: c, n: lat.length, variants: paths ? paths.length : 1, rps: +(lat.length / secs).toFixed(1),
      p50_ms: +pct(lat, 50).toFixed(2), p99_ms: +pct(lat, 99).toFixed(2),
      // Tail visibility (iter 13): the SWR grace-expiry blocks land past
      // p99 at these rps, so p99 alone cannot tell "tail blocks" from
      // "flat scheduling noise". max_ms ~500 every 2 s = block signature.
      p999_ms: +pct(lat, 99.9).toFixed(2), max_ms: lat.length ? +lat[lat.length - 1].toFixed(2) : 0,
      peak_rss_mb: peak ? +(peak / 1024).toFixed(1) : null, codes,
    };
    results.push({ row, lat, secs, t0, t1: Date.now() });
    console.error(JSON.stringify({ ...row, bench: tag }));
  }
  return results;
}

const topSummary = (results, extra = {}) => ({
  at: new Date().toISOString(),
  concurrency, workers: workers > 1 ? workers : undefined, duration_s: duration / 1000,
  write_mix: writeMixMs ? { every_ms: writeMixMs, ticks: mixTicks, errors: mixErrors, target: `${writeBase}/api/collection/${writeId}` } : null,
  results, ...extra,
});

// --- parent orchestration (--workers N) --------------------------------------
if (workers > 1 && !isChild) {
  // Heavy scenarios (27MB routes) run ONCE inline before the children start:
  // they are bandwidth/memcpy-bound single-stream shapes, and running them
  // beside N busy client processes would only measure contention.
  const heavy = scenarios.scenarios.filter((s) => s.workers === 1);
  const light = scenarios.scenarios.filter((s) => s.workers !== 1);
  const inline = await runScenarios(heavy, 'parent');
  if (mixer) clearInterval(mixer);
  if (mixInFlight) await mixInFlight;
  const parentMix = writeMixMs ? { ticks: mixTicks, errors: mixErrors } : null;
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'http-bench-'));
  const outs = await Promise.all(Array.from({ length: workers }, (_, i) => new Promise((resolve, reject) => {
    const out = path.join(tmp, `w${i}.json`);
    const p = spawn(process.execPath, [
      process.argv[1], args[0], ...args.slice(1), '--_child', '--_out', out, '--_rot', String(i),
    ], { stdio: ['ignore', 'ignore', 'inherit'] });
    p.on('exit', (code) => (code === 0 ? resolve(out) : reject(new Error(`worker ${i} exit ${code}`))));
  })));
  const byName = new Map();
  let childMix = null;
  for (const out of outs) {
    const w = JSON.parse(fs.readFileSync(out, 'utf8'));
    if (w.write_mix && (w.write_mix.ticks || w.write_mix.errors)) childMix = w.write_mix;
    for (const r of w.results) {
      let m = byName.get(r.name);
      if (!m) { m = { conc: r.conc, variants: r.variants, n: 0, codes: {}, lat: [], t0: r.t0, t1: r.t1, peak: 0 }; byName.set(r.name, m); }
      m.n += r.n; m.t0 = Math.min(m.t0, r.t0); m.t1 = Math.max(m.t1, r.t1);
      m.peak = Math.max(m.peak, r.peak_rss_mb || 0);
      for (const [k, v] of Object.entries(r.codes)) m.codes[k] = (m.codes[k] || 0) + Number(v);
      for (const x of r.lat) m.lat.push(x);
    }
  }
  fs.rmSync(tmp, { recursive: true, force: true });
  const merged = inline.map((r) => r.row);
  for (const [name, m] of byName) {
    m.lat.sort((a, b) => a - b);
    const secs = (m.t1 - m.t0) / 1000;
    const row = {
      name, conc: m.conc * workers, n: m.n, variants: m.variants,
      rps: +(m.n / secs).toFixed(1),
      p50_ms: +pct(m.lat, 50).toFixed(2), p99_ms: +pct(m.lat, 99).toFixed(2),
      p999_ms: +pct(m.lat, 99.9).toFixed(2), max_ms: m.lat.length ? +m.lat[m.lat.length - 1].toFixed(2) : 0,
      peak_rss_mb: m.peak ? +(m.peak / 1024).toFixed(1) : null, codes: m.codes,
    };
    merged.push(row);
    console.error(JSON.stringify({ ...row, bench: `merged/${workers}w` }));
  }
  const mixTicksAll = (parentMix?.ticks || 0) + (childMix?.ticks || 0);
  const mixErrorsAll = (parentMix?.errors || 0) + (childMix?.errors || 0);
  console.log(JSON.stringify({
    at: new Date().toISOString(), concurrency, workers, duration_s: duration / 1000,
    write_mix: writeMixMs ? { every_ms: writeMixMs, ticks: mixTicksAll, errors: mixErrorsAll, target: `${writeBase}/api/collection/${writeId}` } : null,
    results: merged,
  }, null, 2));
  agent.destroy();
  process.exit(0);
}

// --- worker (child) / single-process path -------------------------------------
// Children never run `"workers": 1` scenarios — the parent ran them inline.
const run = await runScenarios(
  isChild ? scenarios.scenarios.filter((s) => s.workers !== 1) : scenarios.scenarios,
  isChild ? `w${rotOffset}` : 'single',
);
if (mixer) clearInterval(mixer);
if (mixInFlight) await mixInFlight;

if (isChild && outPath) {
  fs.writeFileSync(outPath, JSON.stringify({
    write_mix: writeMixMs ? { ticks: mixTicks, errors: mixErrors } : null,
    results: run.map((r) => ({
      ...r.row, lat: r.lat.map((x) => Math.round(x * 1000) / 1000),
      secs: r.secs, t0: r.t0, t1: r.t1,
    })),
  }));
} else {
  console.log(JSON.stringify(topSummary(run.map((r) => r.row)), null, 2));
}
FastConn.closeAll();
agent.destroy();
