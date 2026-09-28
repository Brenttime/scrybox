// Replay saved phone frames through the in-browser card reader under Node.
//
// Runs shared/clientScan/pipeline.mjs — the exact module the scan worker
// imports — with onnxruntime-node standing in for onnxruntime-web, and scores
// it against the cardscan sidecar's saved answer for each frame
// (CARDSCAN_DEBUG_DIR writes <stamp>.jpg + <stamp>.json).
//
//   node scripts/client-scan-replay.mjs <assets dir> <frames dir> [--limit N] [--threads 1] [--json out.json]
//
// <assets dir> holds manifest.json + the files it names (cardscan
// tools/build_client_index.py). Cornelius comes from CV_MODEL_DIR/data/models.
//
// A WRONG answer is a client printing that differs from the server's printing
// for the same frame. That count must be zero; misses are covered by the
// server fallback.
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { createRequire } from 'node:module';
import { createReader, CORN_SIZE } from '../../shared/clientScan/pipeline.mjs';
import { buildCharset, loadIndex } from '../../shared/clientScan/text.mjs';

const require = createRequire(import.meta.url);
const ort = require('onnxruntime-node');
const sharp = require('sharp');

const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args[i + 1] : d; };
const [assets, framesDir] = args.filter((a, i) => !a.startsWith('--') && !(i > 0 && args[i - 1].startsWith('--')));
if (!assets || !framesDir) { console.error('usage: client-scan-replay.mjs <assets> <frames> [--limit N]'); process.exit(2); }
const limit = Number(opt('--limit', 0)) || Infinity;
const threads = Number(opt('--threads', 1));

const modelDir = process.env.CV_MODEL_DIR || path.join(import.meta.dirname, '..', 'data', 'models');
const manifest = JSON.parse(fs.readFileSync(path.join(assets, 'manifest.json'), 'utf8'));
const sessOpts = { executionProviders: ['cpu'], intraOpNumThreads: threads, interOpNumThreads: 1, graphOptimizationLevel: 'all' };

const tLoad = Date.now();
const index = loadIndex(JSON.parse(zlib.gunzipSync(fs.readFileSync(path.join(assets, manifest.index))).toString('utf8')));
const tIndex = Date.now() - tLoad;
const chars = buildCharset(fs.readFileSync(path.join(assets, manifest.dict), 'utf8'));
const rec = await ort.InferenceSession.create(path.join(assets, manifest.rec), sessOpts);
const cornelius = await ort.InferenceSession.create(path.join(modelDir, 'cornelius.onnx'), sessOpts);
console.log(`loaded: index ${tIndex} ms, total ${Date.now() - tLoad} ms; ${index.names.length} names, ${index.printings.length} printings`);

const reader = createReader({ ort, cornelius, rec, chars, index, footerStages: process.env.STAGES ? JSON.parse(process.env.STAGES) : undefined});

function serverAnswer(j) {
  const ok = (j.results || []).filter(r => r.ok && r.card);
  return {
    cards: (j.candidates || []).length,
    ids: ok.map(r => r.card.id),
    label: ok.map(r => `${r.card.name}[${r.card.set} ${r.card.num}]`).join('; '),
    eligible: (j.candidates || []).filter(c => c.eligible).length,
    // Titles the server read even when it could not prove the printing.
    titles: (j.results || []).map(r => r.title || r.card?.name).filter(Boolean),
  };
}

const every = Number(opt('--every', 1));
const files = fs.readdirSync(framesDir).filter(f => f.endsWith('.jpg')).sort().filter((f, i) => i % every === 0).slice(0, limit);
const rows = [];
for (const f of files) {
  const jsonPath = path.join(framesDir, f.replace(/\.jpg$/, '.json'));
  const srv = fs.existsSync(jsonPath) ? serverAnswer(JSON.parse(fs.readFileSync(jsonPath, 'utf8'))) : null;
  const img = sharp(path.join(framesDir, f)).rotate();
  const full = await img.clone().ensureAlpha().raw().toBuffer({ resolveWithObject: true });
  const small = await img.clone().resize(CORN_SIZE, CORN_SIZE, { fit: 'fill' }).removeAlpha().raw().toBuffer();
  // Every frame is independent here: the saved stream has gaps, and the
  // identity cache would turn later frames into free hits. Reset per frame so
  // each number below is a cold read.
  // --stream keeps cross-frame evidence (consecutive frames of one card), still
  // clearing the identity cache so every frame is a real read.
  if (args.includes('--stream')) reader.resetCache(); else reader.reset();
  const before = { ...reader.stats };
  const t0 = performance.now();
  const out = await reader.read(
    { data: new Uint8ClampedArray(full.data.buffer, full.data.byteOffset, full.data.length), width: full.info.width, height: full.info.height },
    small, { smallChannels: 3 });
  const ms = performance.now() - t0;
  const r = out.results[0];
  const cand = out.candidates[0];
  rows.push({
    frame: f, ms: Math.round(ms), recCalls: reader.stats.recCalls - before.recCalls,
    status: cand ? cand.status : 'no card', sharp: cand?.sharpness, stage: r?.footer_stage, err: r?.error, ok: !!r?.ok, id: r?.scryfallId || null,
    title: r?.title || null, footer: r?.footer_ocr,
    got: r?.ok ? `${r.title}[${r.set} ${r.num}] via ${r.via}` : (r ? `${r.error}${r.title ? ` (${r.title})` : ''}` : ''),
    server: srv, timings: out.timings,
  });
  process.stdout.write('.');
}
process.stdout.write('\n');

// --- scoring ---------------------------------------------------------------
let titleConflict = 0, wrong = 0, agree = 0, clientOnly = 0, serverOnly = 0, bothMiss = 0, multi = 0;
const wrongRows = [];
for (const row of rows) {
  const s = row.server;
  if (s && s.cards > 1) multi++;
  if (row.ok && s && s.ids.length) {
    if (s.ids.includes(row.id)) agree++; else { wrong++; wrongRows.push(row); }
  } else if (row.ok) {
    clientOnly++;
    // No server printing to compare with: the title must still agree with
    // any title the server read for the same frame.
    const norm = (t) => String(t).toLowerCase().replace(/[^a-z0-9]+/g, ' ').trim();
    if (s?.titles.length && !s.titles.some(t => norm(t) === norm(row.title))) { titleConflict++; wrongRows.push(row); }
  }
  else if (s && s.ids.length) serverOnly++;
  else bothMiss++;
}
const read = rows.filter(r => r.status === 'ready' && !r.timings.cached);
const pct = (a, p) => { const s = [...a].sort((x, y) => x - y); return s.length ? s[Math.min(s.length - 1, Math.floor(p * s.length))] : 0; };
const okMs = read.filter(r => r.ok).map(r => r.ms);
const failMs = read.filter(r => !r.ok).map(r => r.ms);
const gateMs = rows.filter(r => r.status !== 'ready').map(r => r.ms);
const summary = {
  frames: rows.length, multiCardFrames: multi,
  clientMatched: rows.filter(r => r.ok).length,
  serverMatched: rows.filter(r => r.server?.ids.length).length,
  agree, wrong, titleConflict, clientOnly, serverOnly, bothMiss,
  gated: Object.fromEntries(Object.entries(rows.reduce((a, r) => { a[r.status] = (a[r.status] || 0) + 1; return a; }, {}))),
  ms: {
    matched: { n: okMs.length, p50: pct(okMs, 0.5), p90: pct(okMs, 0.9), max: pct(okMs, 1) },
    failedRead: { n: failMs.length, p50: pct(failMs, 0.5), p90: pct(failMs, 0.9), max: pct(failMs, 1) },
    gatedOrEmpty: { n: gateMs.length, p50: pct(gateMs, 0.5), max: pct(gateMs, 1) },
  },
  recCallsPerMatch: okMs.length ? +(read.filter(r => r.ok).reduce((s, r) => s + r.recCalls, 0) / okMs.length).toFixed(2) : 0,
};
console.log(JSON.stringify(summary, null, 1));
for (const r of wrongRows) console.log('WRONG', r.frame, r.got, 'server:', r.server.label);
for (const r of rows.filter(x => !x.ok && x.server?.ids.length).slice(0, 25)) console.log('miss', r.frame, r.status, r.got, 'server:', r.server.label);
const jsonOut = opt('--json');
if (jsonOut) fs.writeFileSync(jsonOut, JSON.stringify({ summary, rows }, null, 1));
const byCard = {};
for (const r of rows.filter(x => x.ok)) byCard[r.got.replace(/ via .*/, '')] = (byCard[r.got.replace(/ via .*/, '')] || 0) + 1;
console.log('client printings:', JSON.stringify(byCard));
process.exitCode = wrong || titleConflict ? 1 : 0;
