// Lane B unit 6b A/B: POST /api/scan-match — Node :3312 vs Rust :3320.
// Strict: status, JSON key order, candidate ids/langs, hydrated fields,
// flags. Tolerances (accepted divergences, see cvscan.rs): scores/margin/gap/
// sharpness within ABS (decoder LSB drift through lanczos), crop compared as
// valid JPEG data-URL (mozjpeg vs image crate encoders never agree bytes).
const NODE = 'http://127.0.0.1:3312';
const RUST = 'http://127.0.0.1:3320';
import fs from 'node:fs';
import path from 'node:path';

const CORPUS = process.env.CORPUS_DIR;
const TOK = process.env.AB_TOKEN;
if (!CORPUS || !TOK) {
  console.error('CORPUS_DIR and AB_TOKEN required');
  process.exit(2);
}

function imgBody(file) {
  const b = fs.readFileSync(path.join(CORPUS, file));
  const mime = file.endsWith('.png') ? 'image/png' : 'image/jpeg';
  return JSON.stringify({ image: `data:${mime};base64,${b.toString('base64')}` });
}

async function post(base, bodyObj, { token = TOK, headers = {} } = {}) {
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      return await postOnce(base, bodyObj, token, headers);
    } catch (e) {
      if (attempt === 2 || !/UND_ERR_SOCKET|fetch failed|ECONNRESET/.test(String(e) + String(e?.cause))) throw e;
      await new Promise((r) => setTimeout(r, 400));
    }
  }
}

async function postOnce(base, bodyObj, token, headers) {
  const h = { 'content-type': 'application/json', ...headers };
  if (token) h.authorization = `Bearer ${token}`;
  const r = await fetch(base + '/api/scan-match', {
    method: 'POST',
    headers: h,
    body: bodyObj === undefined ? undefined : (typeof bodyObj === 'string' ? bodyObj : JSON.stringify(bodyObj)),
  });
  const text = await r.text();
  return { status: r.status, text, ct: r.headers.get('content-type') };
}

const ABS_SCORE = 5e-4;
const ABS_SHARP = 5e-3;
let fails = 0;
let maxScoreDelta = 0;
let maxSharpDelta = 0;

function near(a, b, eps, label, name) {
  const d = Math.abs(a - b);
  if (!(d <= eps)) { console.log(`FAIL ${name}: ${label} ${a} vs ${b} (d=${d})`); return false; }
  return true;
}

function compareJson(name, a, b) {
  let ok = true;
  if (a.status !== b.status) { console.log(`FAIL ${name}: status ${a.status} vs ${b.status}`); return false; }
  if (a.status !== 200) {
    if (a.text !== b.text) { console.log(`FAIL ${name}: body ${JSON.stringify(a.text).slice(0,200)} vs ${JSON.stringify(b.text).slice(0,200)}`); return false; }
    return true;
  }
  let ja, jb;
  try { ja = JSON.parse(a.text); jb = JSON.parse(b.text); }
  catch (e) { console.log(`FAIL ${name}: json parse ${e.message}`); return false; }
  const ka = Object.keys(ja).join(','), kb = Object.keys(jb).join(',');
  if (ka !== kb) { console.log(`FAIL ${name}: key order\n ${ka}\n ${kb}`); ok = false; }
  for (const k of ['game', 'verified', 'lang', 'engine', 'detected', 'notInCatalog']) {
    if (ja[k] !== jb[k]) { console.log(`FAIL ${name}: ${k} ${ja[k]} vs ${jb[k]}`); ok = false; }
  }
  for (const k of ['scoped', 'scopedRows']) {
    if ((k in ja) !== (k in jb)) { console.log(`FAIL ${name}: ${k} presence`); ok = false; }
    if (k in ja && ja[k] !== jb[k]) { console.log(`FAIL ${name}: ${k} ${ja[k]} vs ${jb[k]}`); ok = false; }
  }
  if ((ja.catalogs || []).join(',') !== (jb.catalogs || []).join(',')) { console.log(`FAIL ${name}: catalogs`); ok = false; }
  if (!ja.crop?.startsWith('data:image/jpeg;base64,') || !jb.crop?.startsWith('data:image/jpeg;base64,')) {
    console.log(`FAIL ${name}: crop not jpeg data-url`); ok = false;
  } else {
    const ca = Buffer.from(ja.crop.split(',')[1], 'base64'), cb = Buffer.from(jb.crop.split(',')[1], 'base64');
    if (!(ca[0] === 0xff && ca[1] === 0xd8 && cb[0] === 0xff && cb[1] === 0xd8)) { console.log(`FAIL ${name}: crop not SOI`); ok = false; }
    if (Math.abs(ca.length - cb.length) > cb.length * 2) { console.log(`FAIL ${name}: crop size ${ca.length} vs ${cb.length}`); ok = false; }
  }
  for (const [k, eps, mx] of [['sharpness', ABS_SHARP, 'maxSharpDelta'], ['margin', ABS_SCORE, 'maxScoreDelta'], ['gap', ABS_SCORE, 'maxScoreDelta']]) {
    const va = ja[k], vb = jb[k];
    if ((va === null) !== (vb === null)) { console.log(`FAIL ${name}: ${k} null ${va} vs ${vb}`); ok = false; }
    else if (va !== null && vb !== null) {
      const d = Math.abs(va - vb);
      if (k === 'sharpness') maxSharpDelta = Math.max(maxSharpDelta, d);
      else maxScoreDelta = Math.max(maxScoreDelta, d);
      if (!(d <= eps)) { console.log(`FAIL ${name}: ${k} ${va} vs ${vb} (d=${d})`); ok = false; }
      // gate agreement
      if (k === 'sharpness' && (va > 0.02) !== (vb > 0.02)) { console.log(`FAIL ${name}: sharpness gate flip`); ok = false; }
    }
  }
  const ca = ja.candidates || [], cb = jb.candidates || [];
  if (ca.length !== cb.length) { console.log(`FAIL ${name}: cand len ${ca.length} vs ${cb.length}`); return false; }
  for (let i = 0; i < ca.length; i++) {
    for (const k of ['cardId', 'catalogLang', 'name', 'set', 'number']) {
      if (ca[i][k] !== cb[i][k]) { console.log(`FAIL ${name}: cand[${i}].${k} ${JSON.stringify(ca[i][k])} vs ${JSON.stringify(cb[i][k])}`); ok = false; }
    }
    if (!near(ca[i].score, cb[i].score, ABS_SCORE, `cand[${i}].score`, name)) ok = false;
    // hydrated card object must agree field-for-field (JSON text compare)
    if (JSON.stringify(ca[i].card) !== JSON.stringify(cb[i].card)) {
      const ka2 = Object.keys(ca[i].card || {}).join(','), kb2 = Object.keys(cb[i].card || {}).join(',');
      if (ka2 !== kb2) console.log(`FAIL ${name}: cand[${i}].card keys differ`);
      else for (const k of Object.keys(ca[i].card)) {
        if (JSON.stringify(ca[i].card[k]) !== JSON.stringify(cb[i].card[k])) {
          // price/availability can legitimately drift between legs only if a
          // fetch raced — flag it; both legs read the same frozen DB though.
          console.log(`FAIL ${name}: cand[${i}].card.${k} differs`);
          ok = false;
          break;
        }
      }
    }
  }
  return ok;
}

function run(name, p) {
  return (async () => {
    const [a, b] = await Promise.all([p(NODE), p(RUST)]);
    const ok = compareJson(name, a, b);
    console.log(`${ok ? 'PASS' : 'FAIL'} ${name}`);
    if (!ok) fails++;
  })();
}

async function main() {
  const files = fs.readdirSync(CORPUS).filter((f) => /\.(png|jpg)$/.test(f)).sort();
  // --- sequential run (single-model serialisation keeps timings stable) ---
  for (const f of files) {
    const a = await post(NODE, imgBody(f));
    const b = await post(RUST, imgBody(f));
    const ok = compareJson(`img:${f}`, a, b);
    console.log(`${ok ? 'PASS' : 'FAIL'} img:${f}`);
    if (!ok) fails++;
  }
  const cases = [
    ['noauth', () => post(NODE, { image: 'a'.repeat(200) }, { token: null }), () => post(RUST, { image: 'a'.repeat(200) }, { token: null })],
    ['missing_image', () => post(NODE, {}), () => post(RUST, {})],
    ['image_not_string', () => post(NODE, { image: 42 }), () => post(RUST, { image: 42 })],
    ['short_image', () => post(NODE, { image: 'data:image/jpeg;base64,' + Buffer.alloc(50, 9).toString('base64') }), () => post(RUST, { image: 'data:image/jpeg;base64,' + Buffer.alloc(50, 9).toString('base64') })],
    ['junk_image', () => post(NODE, { image: 'data:image/jpeg;base64,' + Buffer.concat([Buffer.from('JUNKJUNK'), Buffer.alloc(160, 3)]).toString('base64') }), () => post(RUST, { image: 'data:image/jpeg;base64,' + Buffer.concat([Buffer.from('JUNKJUNK'), Buffer.alloc(160, 3)]).toString('base64') })],
    ['notstring_json_body', () => post(NODE, '[1,2,3]'), () => post(RUST, '[1,2,3]')],
    ['scalar_body', () => post(NODE, '7'), () => post(RUST, '7')],
    ['no_ct_body', () => post(NODE, '{"image":"x"}', { headers: { 'content-type': '' } }), () => post(RUST, '{"image":"x"}', { headers: { 'content-type': '' } })],
    ['cropped', () => post(NODE, JSON.parse(imgBody(files[0])) && { ...JSON.parse(imgBody(files[0])), cropped: true }), () => post(RUST, { ...JSON.parse(imgBody(files[0])), cropped: true })],
    ['lang_de_notbuilt', () => post(NODE, { ...JSON.parse(imgBody(files[0])), lang: 'de' }), () => post(RUST, { ...JSON.parse(imgBody(files[0])), lang: 'de' })],
    ['set_number_500', () => post(NODE, { ...JSON.parse(imgBody(files[0])), set: 5 }), () => post(RUST, { ...JSON.parse(imgBody(files[0])), set: 5 })],
    ['set_scope', () => post(NODE, { ...JSON.parse(imgBody(files[0])), set: 'who,zho' }), () => post(RUST, { ...JSON.parse(imgBody(files[0])), set: 'who,zho' })],
  ];
  for (const [name, pa, pb] of cases) {
    const a = await pa();
    const b = await pb();
    const ok = compareJson(name, a, b);
    console.log(`${ok ? 'PASS' : 'FAIL'} ${name}`);
    if (!ok) fails++;
  }
  console.log(`\nmax score delta ${maxScoreDelta.toExponential(2)}, max sharpness delta ${maxSharpDelta.toExponential(2)}`);
  console.log(fails === 0 ? 'ALL PASS' : `${fails} FAILURES`);
  process.exit(fails === 0 ? 0 : 1);
}
main();
