// Ground-truth probe: Node :3312 scan-match over the corpus.
// node node-scan-match.mjs [--rust URL]  (default Node only)
import fs from 'node:fs';
import path from 'node:path';
const TOK = process.env.AB_TOKEN;
if (!TOK) { console.error('AB_TOKEN required'); process.exit(1); }
const BASE = process.argv[3] === '--rust' ? process.argv[2].replace('--rust', '') : 'http://127.0.0.1:3312';
const dir = process.env.CORPUS_DIR || new URL('./corpus', import.meta.url).pathname;
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
fs.writeFileSync(new URL('./node-truth.json', import.meta.url).pathname, JSON.stringify(out, null, 1));
