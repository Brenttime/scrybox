// R2-#4 probe: new resizeLanczos3 vs the reference on real saved frames:
// pixel-identical output (hence identical cornelius input and corners).
// Timing is indicative only: the reference recomputes its weights per call
// (v1 cached them), so it is NOT a clean production A/B.
//   node scripts/lanczos-equivalence.mjs <frames dir> [N]
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { resizeLanczos3, resizeLanczos3Reference } from '../../shared/clientScan/imaging.mjs';
const require = createRequire(import.meta.url);
const sharp = require('sharp');
const [dir, N = '20'] = process.argv.slice(2);
const files = fs.readdirSync(dir).filter(f => f.endsWith('.jpg')).sort().slice(0, Number(N));
let diff = 0, tNew = 0, tRef = 0;
for (const f of files) {
  const { data, info } = await sharp(path.join(dir, f)).rotate().ensureAlpha().raw().toBuffer({ resolveWithObject: true });
  const src = new Uint8ClampedArray(data.buffer, data.byteOffset, data.length);
  let t = performance.now(); const a = resizeLanczos3Reference(src, info.width, info.height, 4, 384, 384); tRef += performance.now() - t;
  t = performance.now(); const b = resizeLanczos3(src, info.width, info.height, 4, 384, 384); tNew += performance.now() - t;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) diff++;
}
console.log(JSON.stringify({ frames: files.length, differingBytes: diff, refUncachedMsPerFrame: +(tRef / files.length).toFixed(1), newMsPerFrame: +(tNew / files.length).toFixed(1) }));
process.exitCode = diff ? 1 : 0;
