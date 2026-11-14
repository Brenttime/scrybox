// Ground-truth probe: Node :3312 scan-match over the corpus.
// node node-scan-match.mjs [--rust URL]  (default Node only)
//
// Corpus (lane-B iter-57): the DEFAULT is the lane eval corpus
// <repo-root>/data/scan-eval-corpus — the SAME dir node_truth_parity_on_eval_corpus
// consumes (CORPUS_DIR override in the unit test). The stale ./corpus copy in this
// dir holds same-NAMED but DIFFERENT images (regenerated Nov 12 via mkcorpus/
// mkvariants); reading it by default silently captured truth over the wrong images
// (measured: different top candidates/scores per file). Regenerate BOTH dirs or
// pass CORPUS_DIR explicitly.
//
// Truth write (lane-B iter-57): node-truth.json (unit-test ground truth, via
// include_str!) is only rewritten when WRITE_TRUTH=1 AND this is a Node :3312
// capture. Everything else — --rust runs, exploratory Node runs — writes
// node-truth.last.json, so a comparison run can never clobber the ground truth.
import fs from 'node:fs';
import path from 'node:path';
const TOK = process.env.AB_TOKEN;
if (!TOK) { console.error('AB_TOKEN required'); process.exit(1); }
const IS_RUST = process.argv[3] === '--rust';
const BASE = IS_RUST ? process.argv[2].replace('--rust', '') : 'http://127.0.0.1:3312';
const dir = process.env.CORPUS_DIR || new URL('../../../../data/scan-eval-corpus', import.meta.url).pathname;
const files = fs.readdirSync(dir).filter(f => f.startsWith('scan')).sort();
const out = {};
for (const f of files) {
  const img = 'data:image/jpeg;base64,' + fs.readFileSync(path.join(dir, f)).toString('base64');
  const t0 = Date.now();
  const r = await fetch(BASE + '/api/scan-match', {
    method: 'POST',
    headers: { authorization: `Bearer ${TOK}`, 'content-type': 'application/json' },
    body: JSON.stringify({ image: img }),
  });
  const j = await r.json().catch(() => null);
  out[f] = { status: r.status, ms: Date.now() - t0, json: j };
  const c0 = j?.candidates?.[0];
  console.log(f, r.status, `${Date.now() - t0}ms`, 'detected=' + j?.detected, 'sharp=' + (j?.sharpness ?? '').toString().slice(0, 6),
    'verified=' + j?.verified, 'gap=' + (j?.gap ?? null)?.toString().slice(0, 6), 'notInCat=' + j?.notInCatalog,
    c0 ? `top=${c0.cardId} score=${c0.score}` : JSON.stringify(j)?.slice(0, 120));
  if (j?.crop) out[f].cropLen = j.crop.length;
}
const truthPath = new URL('./node-truth.json', import.meta.url).pathname;
const lastPath = new URL('./node-truth.last.json', import.meta.url).pathname;
const writeTruth = process.env.WRITE_TRUTH === '1' && !IS_RUST;
const dest = writeTruth ? truthPath : lastPath;
fs.writeFileSync(dest, JSON.stringify(out, null, 1));
console.error(`wrote ${dest}${writeTruth ? '' : ' (node-truth.json untouched — WRITE_TRUTH=1 + Node capture required to rewrite ground truth)'}`);
