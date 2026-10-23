// Blur / scale / quad-offset sweep of the List-stamp scorer over Scryfall
// original/plst pairs: node stamp-sweep.mjs <pairs dir> <out.json>
// File names: NN-plst*.jpg (stamped) / NN-orig*.jpg (unstamped).
import fs from 'node:fs'; import path from 'node:path'; import { createRequire } from 'node:module';
import { cardToFrame } from '../../shared/clientScan/imaging.mjs';
import { readListStamp, stampClearAbsent } from '../../shared/clientScan/stamp.mjs';
const require = createRequire(import.meta.url); const sharp = require('sharp');
const [dir, out] = process.argv.slice(2);
const rows = [];
const PAD = 60;   // background margin so offset quads can reach outside the card
for (const f of fs.readdirSync(dir).filter(f => f.endsWith('.jpg')).sort()) {
  for (const hpx of [700, 560]) for (const s of [0, 1, 2, 3]) {
    let img = sharp(path.join(dir, f)).resize({ height: hpx });
    if (s) img = img.blur(Math.max(0.3, s * hpx / 700));
    img = img.extend({ top: PAD, bottom: PAD, left: PAD, right: PAD, background: { r: 30, g: 60, b: 30 } });
    const { data, info } = await img.ensureAlpha().raw().toBuffer({ resolveWithObject: true });
    const W = info.width - 2 * PAD, H = info.height - 2 * PAD;
    const px = new Uint8ClampedArray(data.buffer, data.byteOffset, data.length);
    for (const dx of [-0.04, -0.02, 0, 0.02, 0.04]) for (const dy of [-0.02, 0, 0.02]) {
      const ox = PAD + dx * W, oy = PAD + dy * H;
      const m = cardToFrame([{ x: ox, y: oy }, { x: ox + W, y: oy }, { x: ox + W, y: oy + H }, { x: ox, y: oy + H }]);
      const r = readListStamp(px, info.width, info.height, m);
      rows.push({ f, stamped: f.includes('plst'), hpx, s, dx, dy, ncc: r.ncc, c: r.contrast, sharp: r.sharp, v: r.verdict, wide: r.wide, occl: r.occl, ca: stampClearAbsent(r) });
    }
  }
}
fs.writeFileSync(out, JSON.stringify(rows)); console.log(rows.length);
