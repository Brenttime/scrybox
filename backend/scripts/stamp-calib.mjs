// Per-frame List-stamp scores (ncc, contrast, sharp, in-frame) for calibration.
import fs from 'node:fs'; import path from 'node:path'; import { createRequire } from 'node:module';
import { createReader, CORN_SIZE } from '../../shared/clientScan/pipeline.mjs';
import { cardToFrame } from '../../shared/clientScan/imaging.mjs';
import { readListStamp } from '../../shared/clientScan/stamp.mjs';
const require = createRequire(import.meta.url); const ort = require('onnxruntime-node'); const sharp = require('sharp');
const [modelDir, ...dirs] = process.argv.slice(2);
const cornelius = await ort.InferenceSession.create(path.join(modelDir, 'cornelius.onnx'));
const reader = createReader({ ort, cornelius });
const rows = [];
for (const dir of dirs) for (const f of fs.readdirSync(dir).filter(f => f.endsWith('.jpg')).sort()) {
  const img = sharp(path.join(dir, f)).rotate();
  const full = await img.clone().ensureAlpha().raw().toBuffer({ resolveWithObject: true });
  const small = await img.clone().resize(CORN_SIZE, CORN_SIZE, { fit: 'fill' }).removeAlpha().raw().toBuffer();
  reader.reset();
  const q = await reader.probe(small, 3, full.info.width, full.info.height); if (!q) continue;
  const m = cardToFrame(q); const W = full.info.width, H = full.info.height;
  const s = readListStamp(new Uint8ClampedArray(full.data.buffer, full.data.byteOffset, full.data.length), W, H, m);
  let names = [];
  try { const j = JSON.parse(fs.readFileSync(path.join(dir, f.replace('.jpg', '.json')))); names = (j.results || []).map(r => r.title + (r.ok ? ' ' + r.set + ' ' + r.num : '')).filter(Boolean); } catch {}
  rows.push({ f, dir: path.basename(dir), ...s, names });
}
fs.writeFileSync(process.env.OUT || '/tmp/sa/calib.json', JSON.stringify(rows));
console.log(rows.length);
