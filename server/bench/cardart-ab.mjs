// Live A/B wire parity for the card-art WRITE lane (Lane B unit 6d):
// Node :3312 vs Rust lane B :3320. Runs the full cardArt.js arm matrix
// against each side IN ORDER (both sides end in the same disk state),
// compares status + filtered headers + raw body bytes per arm, then
// decode-compares the stored PNGs (dimensions exact, pixels exact with an
// encoder tolerance) and the bytes/width response contract. Exits non-zero
// on any diff. AB_TOKEN = the pre-seeded session token in the shared DB.
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';

function rawGet(base, p, headers) {
  return new Promise((resolve, reject) => {
    const u = new URL(base);
    const req = http.request({ host: u.hostname, port: u.port, path: p, method: 'GET',
      headers: { 'x-forwarded-for': XFF, ...headers } }, (res) => {
      let bytes = 0;
      res.on('data', (c) => { bytes += c.length; });
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, bytes }));
    });
    req.on('error', reject);
    req.end();
  });
}

const NODE = 'http://127.0.0.1:3312';
const RUST = 'http://127.0.0.1:3320';
const XFF = '127.0.0.6';
const TOK = process.env.AB_TOKEN;
if (!TOK) throw new Error('AB_TOKEN required');
const here = path.dirname(fileURLToPath(import.meta.url));
const DB_ART = '/home/brenttime/projects/scrybox-rusttest/data/card-art';
const CID = 'mtg-0001c639-8bd0-426f-89cb-4ca61f3cc054'; // exists in card_cache
const UP = (f) => fs.readFileSync(path.join(here, 'fixtures', f), 'utf8');
const JSON_H = { 'content-type': 'application/json' };
const AUTH_H = { ...JSON_H, authorization: `Bearer ${TOK}` };

async function req(base, method, p, headers, body) {
  const h = { 'x-forwarded-for': XFF, ...headers };
  const r = await fetch(base + p, { method, headers: h, body: body ?? undefined, redirect: 'manual' });
  const buf = Buffer.from(await r.arrayBuffer());
  const hd = [];
  r.headers.forEach((v, k) => hd.push([k, v]));
  return { status: r.status, hd, buf };
}
// Parser-emitted arms (413 from body-parser, 500 from the global error
// handler) are answered OUTSIDE Node's compression handler's status filter,
// so Node never appends Accept-Encoding to Vary there while our merged layer
// does — a transport-layer twin artifact that shipped identically with the
// admin_users group (parity.mjs ignores Vary). Normalized for those arms.
const norm = (r, { pngBody = false, dropVaryAE = false } = {}) => {
  let body;
  if (pngBody) {
    const d = decodePng(r.buf);
    body = `PNG ${d.w}x${d.h}`; // SHARED user dir: each side fetched its own
    // encoder's bytes — raw bytes are meaningless cross-side here; pixels are
    // decode-compared in the stored-art section instead.
  } else body = r.buf.toString('latin1');
  let hd = r.hd
    .filter(([k]) => !['date', 'connection', 'keep-alive'].includes(k.toLowerCase()));
  if (pngBody) {
    // Each side serves its OWN encoder's file from the shared dir: CL, ETag
    // and Last-Modified legitimately differ — pin their shape, not their value.
    hd = hd.map(([k, v]) => (k.toLowerCase() === 'content-length' ? ['content-length', 'len']
      : k.toLowerCase() === 'etag' ? ['etag', /^W\/"[0-9a-f]+-[0-9a-f]+"$/.test(v) ? 'W/ok' : `BAD:${v}`]
        : k.toLowerCase() === 'last-modified' ? ['last-modified', /^[\w, :]+GMT$/.test(v) ? 'date' : `BAD:${v}`] : [k, v]));
  }
  if (dropVaryAE) {
    hd = hd.map(([k, v]) => (k.toLowerCase() === 'vary'
      ? ['vary', v.replace(/, Accept-Encoding$/, '')] : [k, v]));
  }
  return {
    status: r.status,
    hd: hd.sort((a, b) => a[0].localeCompare(b[0]) || a[1].localeCompare(b[1])),
    body,
  };
};

// Minimal PNG (8-bit RGB, non-interlaced) decoder — the encoders on both
// servers only ever emit filter types 0..4 for this shape.
function decodePng(buf) {
  let p = 8, w, h, depth, ctype, interlace;
  const idat = [];
  while (p < buf.length) {
    const len = buf.readUInt32BE(p);
    const type = buf.toString('ascii', p + 4, p + 8);
    const data = buf.subarray(p + 8, p + 8 + len);
    if (type === 'IHDR') { w = data.readUInt32BE(0); h = data.readUInt32BE(4); depth = data[8]; ctype = data[9]; interlace = data[12]; }
    else if (type === 'IDAT') idat.push(data);
    p += 12 + len;
  }
  if (depth !== 8 || ctype !== 2 || interlace !== 0) {
    throw new Error(`unsupported png depth=${depth} ct=${ctype} il=${interlace}`);
  }
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const bpp = 3, stride = w * bpp;
  const out = Buffer.alloc(h * stride);
  const paeth = (a, b, c) => {
    const q = a + b - c, pa = Math.abs(q - a), pb = Math.abs(q - b), pc = Math.abs(q - c);
    return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
  };
  let rp = 0;
  let prev = Buffer.alloc(stride);
  for (let y = 0; y < h; y++) {
    const f = raw[rp++];
    const line = raw.subarray(rp, rp + stride);
    rp += stride;
    const cur = out.subarray(y * stride, (y + 1) * stride);
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? cur[x - bpp] : 0, b = prev[x], c = x >= bpp ? prev[x - bpp] : 0;
      let v = line[x];
      if (f === 1) v += a; else if (f === 2) v += b; else if (f === 3) v += (a + b) >> 1;
      else if (f === 4) v += paeth(a, b, c); else if (f !== 0) throw new Error('filter ' + f);
      cur[x] = v & 0xff;
    }
    prev = cur;
  }
  return { w, h, px: out };
}

// Arms run in order per side. `file: name` copies USER_DIR/CID.png after
// the arm for cross-side decode comparison. Success arms opt out of the
// raw-body equality (bytes/width contract checked separately).
const arms = [
  { name: 'post_noauth_401', m: 'POST', p: `/api/card-art/${CID}` },
  { name: 'post_noauth_badid_401', m: 'POST', p: '/api/card-art/a..b' },
  { name: 'post_badid_400', m: 'POST', p: '/api/card-art/a..b', h: AUTH_H, b: '{}' },
  { name: 'post_unknown_404', m: 'POST', p: '/api/card-art/mtg-parity-nope', h: AUTH_H, b: '{"image":"YWJj"}' },
  { name: 'post_noimg_400', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: '{}' },
  { name: 'post_empty_img_400', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: '{"image":""}' },
  { name: 'post_array_400', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: '[]' },
  { name: 'post_scalar_500', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: '"str"' },
  { name: 'post_badjson_500', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: '{oops' },
  { name: 'post_oversize_413', m: 'POST', p: `/api/card-art/${CID}`, b: `{"image":"${'A'.repeat(1_200_000)}"}`, h: JSON_H },
  { name: 'post_apikey_403', m: 'POST', p: `/api/card-art/${CID}`, h: { ...JSON_H, authorization: `Bearer ${process.env.AB_API_KEY}` }, b: '{}' },
  { name: 'post_garbage_400', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: '{"image":"notbase64!!@@@"}' },
  { name: 'post_data_nocomma_400', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: '{"image":"data:image/png"}' },
  { name: 'post_pct_400', m: 'POST', p: '/api/card-art/foo%2Fbar', h: AUTH_H, b: '{}' },
  { name: 'post_pct_bad_500', m: 'POST', p: '/api/card-art/%ZZ', h: AUTH_H, b: '{}' },
  { name: 'up_small_200', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: UP('ca-up-small.png.json'), ok: { width: 100 }, file: 'small' },
  { name: 'up_big_200', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: UP('ca-up-big.jpg.json'), ok: { width: 745, dims: [745, 1043] }, file: 'big' },
  { name: 'up_exif_200', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H, b: UP('ca-up-exif.jpg.json'), ok: { width: 745, dims: [745, 1118] }, file: 'exif' },
  { name: 'up_dataurl_small_200', m: 'POST', p: `/api/card-art/${CID}`, h: AUTH_H,
    b: JSON.stringify({ image: 'data:image/png;base64,' + JSON.parse(UP('ca-up-small.png.json')).image }),
    ok: { width: 100 }, file: 'small-dataurl' },
  { name: 'get_png_200', m: 'GET', p: `/api/card-art/${CID}.png`, pngBody: true },
  { name: 'get_png_cond_pair', m: 'GET', p: `/api/card-art/${CID}.png`, inmPair: true, pngBody: true },
  { name: 'get_index_after_write', m: 'GET', p: '/api/card-art/index' },
  { name: 'del_200', m: 'DELETE', p: `/api/card-art/${CID}`, h: { authorization: `Bearer ${TOK}` }, ok: { hasBundled: false } },
  { name: 'del_again_404', m: 'DELETE', p: `/api/card-art/${CID}`, h: { authorization: `Bearer ${TOK}` } },
  { name: 'del_badid_404', m: 'DELETE', p: '/api/card-art/a..b', h: { authorization: `Bearer ${TOK}` } },
  { name: 'del_noauth_401', m: 'DELETE', p: `/api/card-art/${CID}` },
  { name: 'del_apikey_403', m: 'DELETE', p: `/api/card-art/${CID}`, h: { authorization: `Bearer ${process.env.AB_API_KEY}` } },
  { name: 'put_other_proxied', m: 'PUT', p: `/api/card-art/${CID}`, h: { authorization: `Bearer ${TOK}` } },
];

const fmt = (o) => JSON.stringify(o, null, 1);
const results = {};   // name -> {node, rust, statusN/R, ...}
const files = {};     // fileTag -> {node:buf, rust:buf}

async function runSide(base, tag) {
  for (const a of arms) {
    const r = await req(base, a.m, a.p, a.h, a.b);
    (results[a.name] ??= {})[tag] = norm(r, {
      pngBody: !!a.pngBody,
      dropVaryAE: /_(500|413)$/.test(a.name),
    });
    if (a.inmPair) {
      // Conditional GET pair against THIS side's own fresh file: replay the
      // returned ETag and expect 304 with send's carried headers. RAW
      // node:http for the replay (parity.mjs's ruling: undici's own header
      // set corrupts the freshness contract and synthesizes CT/CL on 304s).
      // The etag VALUE is side-specific (shared dir, own encoder's bytes) —
      // the cross-side contract is status + carried-header SET + shape.
      const etag = r.hd.find(([k]) => k.toLowerCase() === 'etag')?.[1];
      const lm = r.hd.find(([k]) => k.toLowerCase() === 'last-modified')?.[1];
      const raw2 = await rawGet(base, a.p, { 'if-none-match': etag });
      const n2 = { status: raw2.status, hd: Object.entries(raw2.headers)
        .filter(([k]) => !['date', 'connection', 'keep-alive'].includes(k))
        .map(([k, v]) => [k, String(v)])
        .sort((x, y) => x[0].localeCompare(y[0]) || String(x[1]).localeCompare(String(y[1])))
        .map(([k, v]) => (k === 'etag'
          ? ['etag', /^W\/"[0-9a-f]+-[0-9a-f]+"$/.test(v) ? 'W/ok' : `BAD:${v}`]
          : k === 'last-modified' ? ['last-modified', /^[\w, :]+GMT$/.test(v) ? 'date' : `BAD:${v}`] : [k, v])),
      body: '' };
      // And the IMS leg from the 200's Last-Modified (raw, same rule).
      const raw3 = await rawGet(base, a.p, { 'if-modified-since': lm });
      results[a.name][`${tag}304`] = n2;
      results[a.name][`${tag}304ims`] = { status: raw3.status,
        hd: Object.entries(raw3.headers).filter(([k]) => !['date', 'connection', 'keep-alive'].includes(k))
          .map(([k, v]) => [k, String(v)])
          .sort((x, y) => x[0].localeCompare(y[0]))
          .map(([k, v]) => (['etag', 'last-modified'].includes(k) ? [k, 'tag'] : [k, v])),
        body: '' };
    }
    if (a.file) {
      const f = path.join(DB_ART, `${CID}.png`);
      files[a.file] = (files[a.file] ??= {});
      files[a.file][tag] = fs.existsSync(f) ? fs.readFileSync(f) : null;
    }
    if (r.status === 200 && a.ok) {
      const j = JSON.parse(r.buf.toString());
      Object.entries(a.ok).forEach(([k, v]) => {
        if (k === 'dims') return;
        if (j[k] !== v) throw new Error(`${tag} ${a.name}: ${k}=${j[k]} want ${v}`);
      });
      (results[a.name][`${tag}Json`] = j);
      if (a.file) {
        // bytes must be the encoder's shipped length on EACH side (cross-side
        // equality is the forbidden encoder claim; self-consistency is the
        // contract, cross-side sanity is the shared >100 floor).
        const f = files[a.file][tag];
        if (!f || j.bytes !== f.length) {
          throw new Error(`${tag} ${a.name}: bytes ${j.bytes} != stored ${f?.length}`);
        }
      }
    }
  }
}

async function main() {
  fs.rmSync(path.join(DB_ART, `${CID}.png`), { force: true });
  await runSide(NODE, 'node');
  await runSide(RUST, 'rust');
  // Final cleanup through NODE (invalidates Node's process-local idCache too).
  await req(NODE, 'DELETE', `/api/card-art/${CID}`, { authorization: `Bearer ${TOK}` });
  fs.rmSync(path.join(DB_ART, `${CID}.png`), { force: true });

  const diffs = [];
  for (const a of arms) {
    const pair = results[a.name];
    if (a.ok) {
      // bytes is the encoder's output length — BY DESIGN not equal across
      // encoders. Contract: equal {success,url,width} + self-consistent
      // bytes (checked in runSide vs the stored file).
      const nj = pair.nodeJson, rj = pair.rustJson;
      for (const k of ['success', 'url', 'width']) {
        if (nj[k] !== rj[k]) diffs.push(`${a.name}: json ${k} ${fmt(nj[k])} vs ${fmt(rj[k])}`);
      }
      if (a.file && !(nj.bytes > 100 && rj.bytes > 100)) diffs.push(`${a.name}: bytes not sized ${nj.bytes}/${rj.bytes}`);
      // headers: ETag rides over different body lengths — compare shape only;
      // Content-Length is driven by the encoder-dependent bytes field, so it
      // is normalized here (the bytes↔stored-file equality is asserted
      // per-side in runSide instead).
      const stripEtag = (x) => x.hd.map(([k, v]) => k === 'etag'
        ? ['etag', /^W\/"[0-9a-f]+-[^"]+"$/.test(v) ? 'W/ok' : `BAD:${v}`]
        : k === 'content-length' ? ['content-length', 'len'] : [k, v]);
      const hn = stripEtag(pair.node), hr = stripEtag(pair.rust);
      if (fmt(hn) !== fmt(hr)) diffs.push(`${a.name}: headers\n${fmt(hn)}\n---\n${fmt(hr)}`);
      if (pair.node.status !== pair.rust.status) diffs.push(`${a.name}: status ${pair.node.status} vs ${pair.rust.status}`);
    } else if (a.inmPair) {
      if (fmt(pair.node) !== fmt(pair.rust)) diffs.push(`${a.name} 200:\n${fmt(pair.node)}\n---\n${fmt(pair.rust)}`);
      if (fmt(pair.node304) !== fmt(pair.rust304)) diffs.push(`${a.name} 304:\n${fmt(pair.node304)}\n---\n${fmt(pair.rust304)}`);
      if (fmt(pair.node304ims) !== fmt(pair.rust304ims)) diffs.push(`${a.name} 304-ims:\n${fmt(pair.node304ims)}\n---\n${fmt(pair.rust304ims)}`);
    } else if (a.ok) {
      // DELETE-style ok arm: body compared via the json view (key order
      // preserved) — raw body equality would also pin content-length drift.
      const bn = pair.node, br = pair.rust;
      if (bn.status !== br.status) diffs.push(`${a.name}: status ${bn.status} vs ${br.status}`);
      const jj = (x) => JSON.stringify(JSON.parse(x.body));
      if (jj(bn) !== jj(br)) diffs.push(`${a.name}: json ${jj(bn)} vs ${jj(br)}`);
      const hn = pair.node.hd.filter(([k]) => k !== 'content-length' && k !== 'etag');
      const hr = pair.rust.hd.filter(([k]) => k !== 'content-length' && k !== 'etag');
      if (fmt(hn) !== fmt(hr)) diffs.push(`${a.name}: headers\n${fmt(hn)}\n---\n${fmt(hr)}`);
    } else {
      if (fmt(pair.node) !== fmt(pair.rust)) diffs.push(`${a.name}:\n${fmt(pair.node)}\n---\n${fmt(pair.rust)}`);
    }
  }
  // Stored-art decode comparison: dimensions EXACT, pixels within a small
  // per-channel tolerance (decoder-exact inputs differ only via the
  // lanczos kernels + encoders on the resize legs).
  for (const [tag, side] of Object.entries(files)) {
    if (!side.node || !side.rust) { diffs.push(`stored ${tag}: file missing ${!side.node ? 'node' : 'rust'}`); continue; }
    const dn = decodePng(side.node), dr = decodePng(side.rust);
    if (dn.w !== dr.w || dn.h !== dr.h) { diffs.push(`stored ${tag}: dims ${dn.w}x${dn.h} vs ${dr.w}x${dr.h}`); continue; }
    let max = 0, sum = 0;
    for (let i = 0; i < dn.px.length; i++) { const d = Math.abs(dn.px[i] - dr.px[i]); if (d > max) max = d; sum += d; }
    const mean = sum / dn.px.length;
    console.log(`stored ${tag}: ${dn.w}x${dn.h} px-max=${max} px-mean=${mean.toFixed(4)} node=${side.node.length}B rust=${side.rust.length}B`);
    const tol = tag.startsWith('small') ? 0 : 3; // resize legs: kernel tolerance
    if (max > tol) diffs.push(`stored ${tag}: pixel max-diff ${max} > ${tol}`);
  }
  if (diffs.length) {
    console.error(`FAIL (${diffs.length}):\n` + diffs.join('\n\n'));
    process.exit(1);
  }
  console.log(`ALL PASS: ${arms.length} arms x 2 sides + ${Object.keys(files).length} stored-art decode comparisons`);
}
main();
