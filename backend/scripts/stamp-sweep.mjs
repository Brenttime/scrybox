// Blur/scale sweep of the List-stamp scorer over Scryfall original/plst pairs.
import fs from 'node:fs'; import path from 'node:path'; import { createRequire } from 'node:module';
import { cardToFrame } from '../../shared/clientScan/imaging.mjs';
import { readListStamp } from '../../shared/clientScan/stamp.mjs';
const require = createRequire(import.meta.url); const sharp = require('sharp');
const [dir, out] = process.argv.slice(2);
const rows = [];
for (const f of fs.readdirSync(dir).filter(f => f.endsWith('.jpg')).sort()) {
  for (const hpx of [936, 700, 560, 450]) for (const s of [0, 1, 2, 3, 4, 6, 8]) {
    let img = sharp(path.join(dir, f)).resize({ height: hpx });
    if (s) img = img.blur(Math.max(0.3, s * hpx / 700));
    const { data, info } = await img.ensureAlpha().raw().toBuffer({ resolveWithObject: true });
    const W = info.width, H = info.height;
    const m = cardToFrame([{ x: 0, y: 0 }, { x: W, y: 0 }, { x: W, y: H }, { x: 0, y: H }]);
    const r = readListStamp(new Uint8ClampedArray(data.buffer, data.byteOffset, data.length), W, H, m);
    rows.push({ f, stamped: f.includes('plst'), hpx, s, ncc: r.ncc, c: r.contrast, sharp: r.sharp, v: r.verdict });
  }
}
fs.writeFileSync(out, JSON.stringify(rows)); console.log(rows.length);
