// Lane B — cardscan sidecar stress: POST /api/cardscan/{detect,frame,scan}
// through the Rust front :3320 and the Node twin :3312.
//
// The sidecar is globally single-flight (_SCAN_LOCK in /app/server.py):
// /api/detect-cards fails non-blocking and /api/scan-frame waits 3 s, then
// both answer {"ok":false,"retry":true,"error":"scanner busy"} 429. Under a
// burst the sidecar's single-threaded acceptor (request_queue_size=5) also
// sheds connections -> the fronts' upstreamError mapping (ECONNREFUSED/
// ENOTFOUND/EAI_AGAIN -> 503, every other axios failure -> 502, mirrored in
// cardscan.rs) decides the envelope. Both fronts relay busy without a
// server-side retry.
//
// Phase S (serial): legs fire one after the other with a lock-drain gap —
// sidecar uncontended, statuses must agree, normalised bodies must match 100%.
// Phase B (burst): CONC workers fire at both fronts; the shared single-flight
// sidecar makes pairwise comparison a coin-flip, so the burst asserts PER-LEG
// invariants instead: no client timeouts (hangs), statuses confined to the
// documented saturation set, every 429 a verbatim busy envelope, every 502/
// 503 the front's own error envelope. Histograms/percentiles are evidence.
//
// Env: AB_TOKEN (required), CORPUS_DIR (default bench/scan-eval/corpus),
//      CONC (default 16), ROUNDS (default 3), PIDS="rust:PID,node:PID".
const NODE = 'http://127.0.0.1:3312';
const RUST = 'http://127.0.0.1:3320';

import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';

const TOK = process.env.AB_TOKEN;
if (!TOK) { console.error('AB_TOKEN required'); process.exit(2); }
const HERE = path.dirname(fileURLToPath(import.meta.url));
const CORPUS = process.env.CORPUS_DIR || path.join(HERE, 'scan-eval', 'corpus');
const CONC = parseInt(process.env.CONC || '16', 10);
const ROUNDS = parseInt(process.env.ROUNDS || '3', 10);

const files = fs.readdirSync(CORPUS).filter((f) => /\.(png|jpe?g)$/i.test(f)).sort();
if (files.length < 4) { console.error('corpus too small: ' + CORPUS); process.exit(2); }
const manifest = JSON.parse(fs.readFileSync(path.join(CORPUS, 'manifest.json'), 'utf8'));
const byFile = Object.fromEntries(manifest.map((m) => [m.file, m]));

function imgBuf(f) { return fs.readFileSync(path.join(CORPUS, f)); }
function b64(f) { return imgBuf(f).toString('base64'); }

function payloadFor(i) {
  const f = files[i % files.length];
  const m = byFile[f] || { id: 'mtg-00000000-0000-4000-8000-000000000000', name: 'Unknown' };
  const kind = ['detect', 'frame', 'scan'][i % 3];
  if (kind === 'scan') {
    return {
      kind, path: '/api/cardscan/scan', ct: 'application/json',
      body: Buffer.from(JSON.stringify({
        cards: [{ image: b64(f), card: { name: m.name, scryfallId: m.id.replace(/^mtg-/, '') }, box: [0, 0, 488, 680] }],
      })),
    };
  }
  return { kind, path: `/api/cardscan/${kind}`, ct: 'image/jpeg', body: imgBuf(f) };
}

async function post(base, p, xff) {
  const t0 = process.hrtime.bigint();
  let status = 0, buf = Buffer.alloc(0);
  try {
    const r = await fetch(base + p.path, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${TOK}`, 'content-type': p.ct,
        'x-forwarded-for': xff, // unique per worker: keeps the ratelet limiter out of the stress
      },
      body: p.body,
      signal: AbortSignal.timeout(25000),
    });
    status = r.status;
    const ce = r.headers.get('content-encoding');
    buf = Buffer.from(await r.arrayBuffer());
    if (ce === 'gzip' && buf.length > 2 && buf[0] === 0x1f && buf[1] === 0x8b) buf = zlib.gunzipSync(buf);
  } catch (e) {
    status = -1; // client-side timeout/transport error — a hang, always FAIL
  }
  return { status, ms: Number(process.hrtime.bigint() - t0) / 1e6, body: buf.toString('latin1') };
}

const VOLATILE = [
  [/<_io\.BytesIO object at 0x[0-9a-f]+>/g, '<_io.BytesIO>'],
  [/"request_ms":\d+/g, '"request_ms":N'],
  [/"[a-z_]+_ms":\d+/g, '"$1_ms":N'],
];
const norm = (r) => VOLATILE.reduce((s, [re, to]) => s.replace(re, to), r.body);

const BUSY_RE = /^\{"ok":false,"retry":true,"error":"scanner busy"\}$/;
const ERR502_RE = /^\{"ok":false,"unavailable":false,"error":"Scanner service error"\}$/;
const ERR503_RE = /^\{"ok":false,"unavailable":true,"error":"Scanner service is not running"\}$/;
const SATURATION = new Set([200, 429, 502, 503]);

function firstDiff(a, b) {
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  const s = Math.max(0, i - 70);
  return `@${i}\n    node=…${JSON.stringify(a.slice(s, i + 70))}\n    rust=…${JSON.stringify(b.slice(s, i + 70))}`;
}

function pct(a, q) { const s = [...a].sort((x, y) => x - y); return s[Math.min(s.length - 1, Math.floor(q * s.length))]; }
function hist(h) { return Object.fromEntries(Object.entries(h).sort((a, b) => a[0] - b[0])); }

function rssOf(pid) {
  try {
    const st = fs.readFileSync(`/proc/${pid}/status`, 'utf8');
    return { rss: +st.match(/VmRSS:\s+(\d+)/)[1], hwm: +st.match(/VmHWM:\s+(\d+)/)[1] };
  } catch { return null; }
}
const PIDS = Object.fromEntries((process.env.PIDS || '').split(',').filter(Boolean).map((kv) => kv.split(':').map((x, i) => i ? +x : x)));

async function sidecarStatus() {
  try {
    const r = await fetch(NODE + '/api/cardscan/status', { headers: { authorization: `Bearer ${TOK}` }, signal: AbortSignal.timeout(10000) });
    return `${r.status} ${(await r.text()).slice(0, 120)}`;
  } catch (e) { return 'ERR ' + String(e).slice(0, 80); }
}

function newStats() { return { [NODE]: { ms: [], hist: {} }, [RUST]: { ms: [], hist: {} } }; }
function record(stats, base, x) {
  const s = stats[base];
  s.ms.push(x.ms);
  s.hist[x.status] = (s.hist[x.status] || 0) + 1;
}

async function main() {
  console.log(`stress: CONC=${CONC} ROUNDS=${ROUNDS} files=${files.length} (${CONC * ROUNDS} probes per base in burst)`);
  console.log('sidecar before:', await sidecarStatus());

  let fails = 0;

  // ---- Phase S — serial, strict pairwise ----
  const sstats = newStats();
  const S_N = Math.min(files.length, 9);
  let serialOk = 0;
  for (let i = 0; i < S_N; i++) {
    const p = payloadFor(i);
    const xff = '127.0.9.9';
    const a = await post(NODE, p, xff);
    await new Promise((r) => setTimeout(r, 150)); // lock drain
    const b = await post(RUST, p, xff);
    await new Promise((r) => setTimeout(r, 150));
    record(sstats, NODE, a); record(sstats, RUST, b);
    if (a.status !== b.status || norm(a) !== norm(b)) {
      fails++;
      console.log(`SERIAL FAIL ${p.kind} ${files[i % files.length]}: status ${a.status}/${b.status}`);
      console.log('  ' + firstDiff(norm(a), norm(b)));
    } else serialOk++;
  }
  console.log(`serial phase: ${serialOk}/${S_N} byte-parity`);

  // ---- Phase B — burst, per-leg invariants ----
  const bstats = newStats();
  const bad = [];
  const t0 = Date.now();
  await Promise.all(Array.from({ length: CONC * ROUNDS }, (_, i) => (async () => {
    const p = payloadFor(i);
    const w = Math.floor(i / ROUNDS);
    const xff = `127.0.${9 + (w >> 8)}.${w & 0xff}`;
    const [a, b] = await Promise.all([post(NODE, p, xff), post(RUST, p, xff)]);
    for (const [base, x] of [[NODE, a], [RUST, b]]) {
      record(bstats, base, x);
      if (x.status === -1) bad.push(`${base} ${p.kind} ${files[i % files.length]}: CLIENT TIMEOUT (hang)`);
      else if (!SATURATION.has(x.status)) bad.push(`${base} ${p.kind} ${files[i % files.length]}: unexpected status ${x.status} body=${JSON.stringify(norm(x).slice(0, 160))}`);
      else if (x.status === 429 && !BUSY_RE.test(norm(x))) bad.push(`${base} ${p.kind} ${files[i % files.length]}: 429 not busy envelope: ${JSON.stringify(norm(x).slice(0, 160))}`);
      else if (x.status === 502 && !ERR502_RE.test(norm(x))) bad.push(`${base} ${p.kind} ${files[i % files.length]}: 502 not front envelope: ${JSON.stringify(norm(x).slice(0, 160))}`);
      else if (x.status === 503 && !ERR503_RE.test(norm(x))) bad.push(`${base} ${p.kind} ${files[i % files.length]}: 503 not front envelope: ${JSON.stringify(norm(x).slice(0, 160))}`);
    }
  })()));
  const wall = Date.now() - t0;
  fails += bad.length;
  for (const m of bad.slice(0, 20)) console.log('BURST FAIL ' + m);
  console.log(`burst phase: n=${CONC * ROUNDS} probes, wall=${wall}ms, invariant violations=${bad.length}`);

  console.log('sidecar after: ', await sidecarStatus());
  for (const base of [NODE, RUST]) {
    const s = bstats[base];
    const sat = Object.keys(s.hist).reduce((acc, k) => acc + s.hist[k], 0);
    console.log(`${base}: n=${s.ms.length} hist=${JSON.stringify(hist(s.hist))} p50=${pct(s.ms, 0.5).toFixed(0)}ms p95=${pct(s.ms, 0.95).toFixed(0)}ms max=${Math.max(...s.ms).toFixed(0)}ms`);
    const pid = PIDS[base === RUST ? 'rust' : 'node'];
    if (pid) { const m = rssOf(pid); m && console.log(`  RSS ${m.rss} kB (HWM ${m.hwm} kB)`); }
  }
  console.log(`TOTAL: fails=${fails}`);
  process.exit(fails ? 1 : 0);
}
main();
