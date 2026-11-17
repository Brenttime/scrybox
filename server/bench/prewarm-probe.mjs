#!/usr/bin/env node
// Pre-warm mechanism probe (iter 14). NOT a throughput bench — under foreign
// load absolute times inflate, but the OFF/ON contrast is robust: after a
// write settles, is the next full-collection request a store hit (transfer
// only) or does it pay the blocking rebuild?
//
//   node prewarm-probe.mjs [--reps 6] [--label off]
//
// Per rep: PUT edit + revert through Node (:3312, same as --write-mix), wait
// past the refresher's settle (debounce 500 ms + build), time three GETs of
// the full route against Rust (:3310); report each GET's ms. ON expectation:
// all three are transfer-only (~equal, no 400+ ms first). OFF expectation:
// the first is build+transfer (hundreds of ms), the rest are hits.

import fs from 'node:fs';
import http from 'node:http';

const opt = (k, d) => { const i = process.argv.indexOf(k); return i > -1 ? process.argv[i + 1] : d; };
const scen = JSON.parse(fs.readFileSync('scen-varied.json', 'utf8'));
const auth = scen.scenarios.find((s) => s.headers?.Authorization).headers.Authorization;
const reps = Number(opt('--reps', 6));
const label = opt('--label', 'probe');
const rust = { host: '127.0.0.1', port: 3310 };
const node = { host: '127.0.0.1', port: 3312 };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function req(base, path, method = 'GET', headers = {}) {
  return new Promise((resolve, reject) => {
    const t0 = process.hrtime.bigint();
    const r = http.request({ ...base, path, method, headers: { ...headers, connection: 'close' } }, (res) => {
      let n = 0;
      res.on('data', (c) => { n += c.length; });
      res.on('end', () => resolve({ ms: Number(process.hrtime.bigint() - t0) / 1e6, status: res.statusCode, bytes: n }));
    });
    r.on('error', reject);
    r.end();
  });
}

async function editRevert(id) {
  const mk = (v) => { const body = JSON.stringify({ is_trade: v }); return { body, h: { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body), Authorization: auth } }; };
  const e = mk(true), r = mk(false);
  const a = await req(node, `/api/collection/${id}`, 'PUT', { ...e.h });
  const b = await req(node, `/api/collection/${id}`, 'PUT', { ...r.h });
  if (a.status !== 200 || b.status !== 200) throw new Error(`write-mix failed ${a.status}/${b.status}`);
}

const fullPaths = scen.scenarios.find((s) => s.name === 'collection_full_varied').paths;
(async () => {
  // Warm the candidate set (touch full + a page shape) so the refresher has
  // active keys, then reps of write -> settle -> timed GETs.
  for (const p of fullPaths.slice(0, 2)) await req(rust, p, 'GET', { Authorization: auth });
  const out = [];
  for (let i = 0; i < reps; i++) {
    await editRevert(4);
    await sleep(1300); // debounce 500 ms + full build margin
    const timed = [];
    for (let j = 0; j < 3; j++) timed.push(Math.round((await req(rust, fullPaths[i % fullPaths.length], 'GET', { Authorization: auth })).ms));
    out.push(timed);
    console.log(`${label} rep${i}: ${timed.join(' / ')} ms`);
  }
  fs.writeFileSync(`results/prewarm-${label}.json`, JSON.stringify({ label, reps: out }, null, 1));
})();
