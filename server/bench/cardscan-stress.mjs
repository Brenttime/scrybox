// Cardscan sidecar STRESS harness (lane A, iter-61 queue item: "cardscan
// sidecar stress + recognition eval re-run"). Complements cardscan-ab.mjs:
// that proves byte parity under no load; this proves the proxy legs behave
// the SAME under load and that the shared sidecar survives it.
//
// Legs: NODE = http://127.0.0.1:3312 (axios proxy) and RUST = :3320 (native
// cardscan.rs hyper proxy — lane B's server; :3310 keeps cardscan on the
// Node proxy fallback per its RUST_ROUTES). Both proxy onto the SAME
// sidecar container (cardscan:8321), so the two legs contend for one
// single-work-queue backend — tail behaviour comparison is only meaningful
// run solo-leg-per-window (sequenced below), per the bench policy.
//
// State safety: ONLY stateless legs are stressed. POST /detect runs the
// sidecar detection on the posted bytes with no session state; GET /status
// is read-only. POST /cards, /scan, /choices touch sidecar session state and
// /telemetry lands in a DB table — those are covered by cardscan-ab.mjs
// zero-to-no-load parity, never stressed here.
//
// Gates reported per window: rps, p50/p99, status histogram (a Rust-only
// 5xx class vs Node under identical load IS the finding), sidecar liveness
// pre/post (direct container-IP probe: 200 AND the sidecar's own JSON, not
// just the proxy), and a post-stress cardscan-ab smoke (status+detect
// probes must still be byte-equal).
//
// Usage: node cardscan-stress.mjs [--token TOK] [--concurrency N]
//        [--duration S] [--legs node,rust] [--xff-base 127.0.0.]
//        [--bodies synthetic|corpus] [--corpus-dir DIR]
//
// --bodies corpus (iter-62, sidecar-queue attribution): the detect bodies are
// the REAL eval-corpus images (scan*.jpg + card*.png, 100-520 KB each) sent
// as application/octet-stream — the one content-type BOTH proxies accept raw
// (express.raw image/jpeg|octet-stream; Rust cardscan.rs mirrors it) and that
// the sidecar sniffs regardless. Unlike the 230-byte synthetic body (which
// the sidecar rejects fast — "Truncated File Read"), real images make the
// sidecar run full detection per request, so the window measures the proxy
// legs queued behind real sidecar work. Bodies rotate per request over the
// sorted file list.
import { spawnSync } from 'node:child_process';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';

const NODE_BASE = process.env.NODE_BASE || 'http://127.0.0.1:3312';
const RUST_BASE = process.env.RUST_BASE || 'http://127.0.0.1:3320';

function arg(name, dflt) {
  const i = process.argv.indexOf('--' + name);
  return i >= 0 ? process.argv[i + 1] : dflt;
}
const CONC = parseInt(arg('concurrency', '8'), 10);
const DUR_MS = parseFloat(arg('duration', '10')) * 1000;
const XFF_BASE = arg('xff-base', '127.0.0.');
// Bucket-contention lesson from iter-18/60: every harness leg rotates its
// own x-forwarded-for so the rate limiter never collapses the fleet into one
// bucket. Distinct 4th octet per worker (2..2+CONC).
const tok = process.env.AB_TOKEN || arg('token', '');
if (!tok) { console.error('AB_TOKEN required (session token from the shared DB)'); process.exit(2); }

function img(n) { // same synthetic jpeg the A/B harness uses
  const b = Buffer.alloc(n, 7);
  b.write('\xFF\xD8\xFF\xE0', 0, 'binary');
  b.write('\xFF\xD9', n - 2, 'binary');
  return b;
}
const DETECT_BODY = img(230);

// corpus mode: real images make the sidecar do real work per request.
const BODIES_MODE = arg('bodies', 'synthetic');
const CORPUS_DIR = arg('corpus-dir', '/home/brenttime/projects/scrybox-rusttest/data/scan-eval-corpus');
let CORPUS = [];
if (BODIES_MODE === 'corpus') {
  CORPUS = readdirSync(CORPUS_DIR)
    .filter((f) => /\.(jpe?g|png)$/i.test(f))
    .sort()
    .map((f) => ({ name: f, buf: readFileSync(path.join(CORPUS_DIR, f)) }));
  if (CORPUS.length < 10) {
    console.error(`corpus mode: only ${CORPUS.length} images in ${CORPUS_DIR} — refusing`);
    process.exit(2);
  }
}
const detectBody = (i) => (BODIES_MODE === 'corpus' ? CORPUS[i % CORPUS.length] : { name: 'synthetic', buf: DETECT_BODY });

// Workload mix: mirrors a live scanner client — mostly frames of detect with
// a status poll every ~10th request (the client polls status between scans).
const MIX = [
  { name: 'detect', method: 'POST', path: '/api/cardscan/detect', ct: BODIES_MODE === 'corpus' ? 'application/octet-stream' : 'image/jpeg', rotate: BODIES_MODE === 'corpus', body: DETECT_BODY, w: 9 },
  { name: 'status', method: 'GET', path: '/api/cardscan/status', w: 1 },
];
const MIX_EXPANDED = MIX.flatMap((m) => Array(m.w).fill(m));

async function leg(base, mix, i, saw) {
  const h = {
    authorization: `Bearer ${tok}`,
    'x-forwarded-for': `${XFF_BASE}${2 + (i % 250)}`,
  };
  const m = mix[i % mix.length];
  if (m.ct) h['content-type'] = m.ct;
  const body = m.rotate ? detectBody(i).buf : m.body;
  const t0 = process.hrtime.bigint();
  try {
    const r = await fetch(base + m.path, {
      method: m.method, headers: h, body: body ?? undefined, redirect: 'manual',
    });
    const buf = Buffer.from(await r.arrayBuffer());
    const ms = Number(process.hrtime.bigint() - t0) / 1e6;
    let cls = `${r.status}`;
    if (m.name === 'detect' && r.status === 200) {
      // classify the payload so a degenerate 200 (empty/err body) is visible;
      // for real images also record the first top-level array's size (the
      // candidate-card count the sidecar found) so both legs must agree on
      // the workload actually having run, not just on the status line.
      let b = buf;
      const enc = r.headers.get('content-encoding');
      if (enc === 'gzip' && buf.length > 2 && buf[0] === 0x1f && buf[1] === 0x8b) b = zlib.gunzipSync(buf);
      try {
        const j = JSON.parse(b.toString('utf8'));
        if (j.error) cls = '200-error-body:' + String(j.error).slice(0, 40);
        else {
          const arr = Object.values(j).find((v) => Array.isArray(v));
          cls = arr ? `200-ok[arr=${arr.length}]` : '200-ok';
        }
      } catch { cls = '200-unparseable'; }
    }
    saw.push({ name: m.name, ms, cls });
  } catch (e) {
    const ms = Number(process.hrtime.bigint() - t0) / 1e6;
    const code = e.cause?.code || e.cause?.name || (e.cause ? String(e.cause).slice(0, 60) : e.name);
    saw.push({ name: m.name, ms, cls: 'transport:' + code });
  }
}

const pct = (a, p) => { a.sort((x, y) => x - y); return a.length ? a[Math.min(a.length - 1, Math.floor(p * a.length))] : NaN; };

async function runWindow(base, legName) {
  const saw = [];
  let i = 0;
  const stop = Date.now() + DUR_MS;
  const drivers = Array.from({ length: CONC }, () => (async () => {
    while (Date.now() < stop) await leg(base, MIX_EXPANDED, i++, saw);
  })());
  const t0 = Date.now();
  await Promise.all(drivers);
  const wall = (Date.now() - t0) / 1000;
  const byName = {};
  for (const m of ['detect', 'status']) {
    const rows = saw.filter((r) => r.name === m);
    const hist = {};
    for (const r of rows) hist[r.cls] = (hist[r.cls] || 0) + 1;
    byName[m] = { n: rows.length, rps: +(rows.length / wall).toFixed(1),
      p50: +pct(rows.map((r) => r.ms), 0.5).toFixed(1),
      p99: +pct(rows.map((r) => r.ms), 0.99).toFixed(1), hist };
  }
  return byName;
}

// Sidecar liveness probed BYPASSING both proxies: container IP + the status
// route the Node container itself uses. A 200 here while a proxy leg errors
// isolates the fault to the proxy, not the sidecar.
function sidecarIp() {
  const r = spawnSync('docker', ['inspect', '-f', '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}', 'cardscan'], { encoding: 'utf8' });
  return r.stdout.trim();
}
async function sidecarProbe() {
  const ip = sidecarIp();
  if (!ip) return { ok: false, why: 'no container ip' };
  try {
    const t0 = Date.now();
    const r = await fetch(`http://${ip}:8321/api/health`, { signal: AbortSignal.timeout(15000) });
    const txt = await r.text();
    let jsonOk = false;
    try { jsonOk = typeof JSON.parse(txt) === 'object'; } catch {}
    return { ok: r.status === 200 && jsonOk, status: r.status, ms: Date.now() - t0 };
  } catch (e) { return { ok: false, why: String(e).slice(0, 120) }; }
}

async function main() {
  const legs = arg('legs', 'node,rust').split(',');
  console.log(`# cardscan sidecar stress  sidecar=${await sidecarIp()}  concurrency=${CONC} duration=${DUR_MS / 1000}s mix=detect:9/status:1 bodies=${BODIES_MODE}${BODIES_MODE === 'corpus' ? `(${CORPUS.length} imgs)` : ''}`);
  const pre = await sidecarProbe();
  console.log('sidecar pre-stress:', JSON.stringify(pre));
  if (!pre.ok) { console.error('REFUSING to stress: sidecar not healthy pre-flight'); process.exit(3); }

  const results = {};
  for (const l of legs) {
    const base = l === 'node' ? NODE_BASE : l === 'rust' ? RUST_BASE : l;
    const la = (await import('node:os')).default.loadavg();
    const t = await runWindow(base, l);
    results[l] = { load1: +la[0].toFixed(2), ...t };
    const mid = await sidecarProbe();
    console.log(`leg ${l} @${base}: ${JSON.stringify(t)} sidecar-mid=${JSON.stringify(mid)}`);
    if (!mid.ok) { console.error(`SIDECAR UNHEALTHY mid-run after leg ${l}`); }
  }

  // Fault isolation for any 5xx class seen on one leg only.
  const err = (t) => Object.entries(t.hist).filter(([k]) => !k.startsWith('200-ok') && k !== '200').map(([k, v]) => `${k}=${v}`);
  let flags = [];
  if (legs.length === 2 && results.node && results.rust) {
    for (const m of ['detect', 'status']) {
      const n = err(results.node[m]), r = err(results.rust[m]);
      const only = (a, b) => a.filter(([k]) => !b.some(([k2]) => k2 === k));
      const nOnly = only(n, r), rOnly = only(r, n);
      if (nOnly.length || rOnly.length) {
        flags.push(`${m}: node-only[${nOnly}] rust-only[${rOnly}]`);
      }
    }
  }
  const post = await sidecarProbe();
  console.log('sidecar post-stress:', JSON.stringify(post));
  console.log('MIX:', JSON.stringify(results, null, 1));
  console.log(flags.length ? 'ASYMMETRIC-ERRORS ' + flags.join(' | ') : 'ERROR-CLASSES SYMMETRIC');
  // Median-vs-spread policy: single window per leg here; loop callers compare.
  process.exit(post.ok && !flags.length ? 0 : 1);
}
main().catch((e) => { console.error('HARNESS ERROR', e); process.exit(3); });
