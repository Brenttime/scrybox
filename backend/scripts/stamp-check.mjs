import fs from 'node:fs'; import path from 'node:path'; import { createRequire } from 'node:module';
import { createReader, CORN_SIZE } from '../../shared/clientScan/pipeline.mjs';
import { cardToFrame } from '../../shared/clientScan/imaging.mjs';
import { readListStamp } from '../../shared/clientScan/stamp.mjs';
const require = createRequire(import.meta.url); const ort = require('onnxruntime-node'); const sharp = require('sharp');
const [modelDir, dir, every='1'] = process.argv.slice(2);
const cornelius = await ort.InferenceSession.create(path.join(modelDir,'cornelius.onnx'));
const reader = createReader({ ort, cornelius });
const cnt={};
for (const f of fs.readdirSync(dir).filter(f=>f.endsWith('.jpg')).sort().filter((f,i)=>i%Number(every)===0)) {
  const img = sharp(path.join(dir,f)).rotate();
  const full = await img.clone().ensureAlpha().raw().toBuffer({ resolveWithObject: true });
  const small = await img.clone().resize(CORN_SIZE, CORN_SIZE, { fit: 'fill' }).removeAlpha().raw().toBuffer();
  const q = await reader.probe(small, 3, full.info.width, full.info.height); if (!q) continue;
  const t=performance.now();
  const s = readListStamp(new Uint8ClampedArray(full.data.buffer, full.data.byteOffset, full.data.length), full.info.width, full.info.height, cardToFrame(q));
  const ms=performance.now()-t;
  cnt[s.verdict]=(cnt[s.verdict]||0)+1;
  if (s.verdict!=='absent' || process.env.V) console.log(f, s.verdict, s.ncc.toFixed(2), s.contrast.toFixed(2), ms.toFixed(1)+'ms');
}
console.log(cnt);
