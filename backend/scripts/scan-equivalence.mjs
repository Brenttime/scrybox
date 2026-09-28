// Equivalence probes for client-scan optimisations (review R2 #4, #5, #11).
//
//   node scripts/scan-equivalence.mjs <assets dir> <telemetry.jsonl|json>... [--frames dir]
//
// titles:  every recorded title_raw string through findCardByOcr (memoized)
//          vs findCardByOcrUncached vs the reference in <base>/shared (if
//          BASE_SHARED is set): identical {name, score} required, twice
//          (cold and warm memo).
// Exits non-zero on any mismatch.
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { loadIndex, findCardByOcr, findCardByOcrUncached } from '../../shared/clientScan/text.mjs';

const args = process.argv.slice(2);
const assets = args[0];
const tels = args.slice(1).filter(a => !a.startsWith('--'));
const manifest = JSON.parse(fs.readFileSync(path.join(assets, 'manifest.json'), 'utf8'));
const raw = JSON.parse(zlib.gunzipSync(fs.readFileSync(path.join(assets, manifest.index))).toString('utf8'));
const ix = loadIndex(raw);
const ref = process.env.BASE_SHARED ? await import(path.resolve(process.env.BASE_SHARED, 'clientScan/text.mjs')) : null;
const ixRef = ref ? ref.loadIndex(raw) : null;

const texts = new Set();
const walk = (rec) => {
  for (const tr of rec?.local?.title_raw || []) if (Array.isArray(tr) && typeof tr[0] === 'string') texts.add(tr[0]);
  for (const o of rec?.local?.ocr || []) if (typeof o === 'string') texts.add(o);
};
for (const f of tels) {
  const s = fs.readFileSync(f, 'utf8');
  if (s.trimStart().startsWith('[')) JSON.parse(s).forEach(walk);
  else for (const line of s.split('\n')) { if (!line.trim()) continue; try { const j = JSON.parse(line); (j.records || [j]).forEach(walk); } catch { /* skip */ } }
}
// Adversarial near-names: every recorded string with one char dropped/swapped.
const adv = [];
for (const t of [...texts].slice(0, 400)) {
  if (t.length > 3) { adv.push(t.slice(1)); adv.push(t.slice(0, -1)); adv.push(t.replace(/l/g, 'I')); adv.push(t.replace(/e/g, 'c')); }
}
const all = [...texts, ...adv, 'X', 'x', '', 'the', 'ley Reception'];
let bad = 0, n = 0;
const eq = (a, b) => a.name === b.name && a.score === b.score;
for (let pass = 0; pass < 2; pass++) {
  for (const t of all) {
    n++;
    const a = findCardByOcr(ix, t), b = findCardByOcrUncached(ix, t);
    const c = ref ? ref.findCardByOcr(ixRef, t) : b;
    if (!eq(a, b) || !eq(b, c)) { bad++; if (bad < 10) console.log('MISMATCH', JSON.stringify(t), a, b, c); }
  }
}
console.log(JSON.stringify({ titles: texts.size, adversarial: adv.length, lookups: n, mismatches: bad, ref: !!ref }));
process.exitCode = bad ? 1 : 0;
