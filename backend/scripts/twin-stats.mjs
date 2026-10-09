import fs from 'node:fs'; import zlib from 'node:zlib'; import path from 'node:path';
import { loadIndex, physicalTwins } from '../../shared/clientScan/text.mjs';
const dir=process.argv[2]; const man=JSON.parse(fs.readFileSync(path.join(dir,'manifest.json')));
const ix=loadIndex(JSON.parse(zlib.gunzipSync(fs.readFileSync(path.join(dir,man.index)))));
let single=0, singleTwin=0, withTwin=0; const ex=[];
for (const [t,pool] of Object.entries(ix.byTitle)) {
  const keys=new Set(pool.map(p=>`${ix.phys[p].set}:${ix.phys[p].num}`));
  const hasSheet=pool.some(p=>ix.phys[p].sheet);
  if (hasSheet) withTwin++;
  if (keys.size===1 && pool.length>1) { singleTwin++; if (ex.length<15) ex.push(t+' '+pool.map(p=>ix.printings[p][1]+':'+ix.printings[p][2]).join(',')); }
}
console.log({titles:Object.keys(ix.byTitle).length, withTwin, singleTwin}); console.log(ex.join('\n'));
for (const t of ['woodland changeling','hullbreacher','whip silk','terrain generator','twisted image','spirit mantle','diabolic tutor','tide shaper','chainer dementia master','refuse to yield','smokestack']) console.log(t, (ix.byTitle[t]||[]).map(p=>ix.printings[p][1]+':'+ix.printings[p][2]).join(','));
