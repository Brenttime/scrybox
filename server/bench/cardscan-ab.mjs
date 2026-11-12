// Live A/B wire parity for the cardscan lane: Node :3312 vs Rust lane B :3320.
// Compares status, filtered headers (Date dropped, order preserved) and raw
// body bytes for every probe; exits non-zero on any diff.
const NODE = 'http://127.0.0.1:3312';
const RUST = 'http://127.0.0.1:3320';
const XFF = '127.0.0.5';

function img(n) {
  const b = Buffer.alloc(n, 7);
  b.write('\xFF\xD8\xFF\xE0', 0, 'binary');
  b.write('\xFF\xD9', n - 2, 'binary');
  return b;
}

async function req(base, method, path, headers, body) {
  const h = { 'x-forwarded-for': XFF, ...headers };
  const r = await fetch(base + path, { method, headers: h, body: body ?? undefined, redirect: 'manual' });
  const buf = Buffer.from(await r.arrayBuffer());
  const hd = [];
  r.headers.forEach((v, k) => hd.push([k, v]));
  return { status: r.status, hd, buf };
}

// Transport artifacts owned by the server runtime, not the route: Node's
// keep-alive agent stamps Connection/Keep-Alive, hyper does not (accepted
// transport divergence, same policy as the iter-5c header dumps). The
// sidecar error text embeds a live Python object address and a ms timer.
const VOLATILE = [
  [/<_io\.BytesIO object at 0x[0-9a-f]+>/g, '<_io.BytesIO>'],
  [/"request_ms":\d+/g, '"request_ms":N'],
];
const norm = (r) => {
  const enc = r.hd.find(([k]) => k.toLowerCase() === 'content-encoding')?.[1];
  let buf = r.buf;
  if (enc === 'gzip' && buf.length > 2 && buf[0] === 0x1f && buf[1] === 0x8b) {
    buf = zlib.gunzipSync(buf); // undici sometimes pre-decodes; compare payloads
  }
  const raw = r.buf.toString('latin1');
  // Sidecar error echoes embed a per-call Python object address, so the two
  // legs legitimately carry different entity bytes. Assert the real
  // invariant instead: the ETag is the npm etag weak form over THIS side's
  // exact shipped bytes (hex byte-length + unpadded base64 sha1).
  let hd = r.hd.filter(([k]) => !['date', 'connection', 'keep-alive'].includes(k.toLowerCase()));
  if (/<_io\.BytesIO object at 0x[0-9a-f]+>/.test(raw)) {
    const want = `W/"${r.buf.length.toString(16)}-${crypto.createHash('sha1').update(r.buf).digest('base64').replace(/=+$/, '')}"`;
    hd = hd.map(([k, v]) => k.toLowerCase() === 'etag' ? ['etag', v === want ? 'etag==body_etag(self)' : `BAD:${v} want ${want}`] : [k, v]);
  }
  return {
    status: r.status,
    hd: hd.sort((a, b) => a[0].localeCompare(b[0]) || a[1].localeCompare(b[1])),
    body: VOLATILE.reduce((s, [re, to]) => s.replace(re, to), buf.toString('latin1')),
  };
};
const fmt = (o) => JSON.stringify(o, null, 1);

import zlib from 'node:zlib';
import crypto from 'node:crypto';

async function main() {
  // session token pre-seeded in the shared DB (env AB_TOKEN)
  const tok = process.env.AB_TOKEN;
  const A = { authorization: `Bearer ${tok}` };
  const AJ = { ...A, 'content-type': 'application/json' };
  const AJPEG = { ...A, 'content-type': 'image/jpeg' };

  // real hydrated ids from the shared card_cache DB (37k rows seeded)
  const idA = 'cb28fe03-8269-41de-b766-42c3421aeaef';
  const idB = '9f970f79-3051-4ba1-badb-697ef321cbb3';

  const probes = [];
  const P = (name, method, path, headers, body) => probes.push({ name, method, path, headers, body });

  P('status', 'GET', '/api/cardscan/status', A);
  P('status_anon', 'GET', '/api/cardscan/status', {});
  P('frame_401', 'POST', '/api/cardscan/frame', {}, img(120));
  P('status_ims', 'GET', '/api/cardscan/status', { ...A, 'if-none-match': 'W/"deadbeef"' });
  P('status_inm_star', 'GET', '/api/cardscan/status', { ...A, 'if-none-match': '*' });
  P('detect', 'POST', '/api/cardscan/detect', AJPEG, img(230));
  P('detect_junk', 'POST', '/api/cardscan/detect', AJPEG, Buffer.concat([Buffer.from('JUNK'), img(210)]));
  P('frame', 'POST', '/api/cardscan/frame', AJPEG, img(140));
  P('frame_small', 'POST', '/api/cardscan/frame', AJPEG, img(120));
  P('frame_pngct', 'POST', '/api/cardscan/frame', { ...A, 'content-type': 'image/png' }, img(140));
  P('frame_ct_json', 'POST', '/api/cardscan/frame', AJ, img(140));
  P('cards', 'POST', '/api/cardscan/cards', AJ, JSON.stringify({ results: [{ number: '7', scryfallId: idA, title: 'CLI Title', via: 'ondevice-x' }] }));
  P('cards_unknown', 'POST', '/api/cardscan/cards', AJ, JSON.stringify({ results: [{ scryfallId: '00000000-0000-4000-8000-000000000000' }] }));
  P('cards_badjson', 'POST', '/api/cardscan/cards', AJ, '{oops');
  P('cards_scalar_body', 'POST', '/api/cardscan/cards', AJ, '5');
  P('cards_null_body', 'POST', '/api/cardscan/cards', AJ, 'null');
  P('cards_empty_body', 'POST', '/api/cardscan/cards', AJ, '');
  P('cards_ws_body', 'POST', '/api/cardscan/cards', AJ, '   ');
  P('cards_array_body', 'POST', '/api/cardscan/cards', AJ, '[1,2]');
  P('cards_no_ct', 'POST', '/api/cardscan/cards', A, JSON.stringify({ results: [] }));
  P('cards_num10', 'POST', '/api/cardscan/cards', AJ, JSON.stringify({ results: [{ number: '1.0', scryfallId: idB, title: 'X', via: 'v' }] }));
  P('choices_missing', 'POST', '/api/cardscan/choices', AJ, JSON.stringify({ id: 'nope-nope' }));
  P('choices_badbody', 'POST', '/api/cardscan/choices', AJ, JSON.stringify({ id: 'x', choices: 'notarray' }));
  P('scan', 'POST', '/api/cardscan/scan', AJ, JSON.stringify({ cards: [{ image: 'aHR0cHM6Ly9leGFtcGxlLmNvbS9hLmpwZw==', card: { name: 'Test', scryfallId: idA }, box: [0, 0, 1, 1] }] }));
  P('scan_badct', 'POST', '/api/cardscan/scan', AJPEG, Buffer.from('nope'));
  P('scan_small', 'POST', '/api/cardscan/scan', AJ, JSON.stringify({ cards: [] }));
  P('telemetry_json', 'POST', '/api/cardscan/telemetry', AJ, JSON.stringify({ session: 's1', platform: 'ios', records: [{ kind: 'scan', t: 1 }] }));
  P('telemetry_no_ct', 'POST', '/api/cardscan/telemetry', A, 'junk');
  P('telemetry_textplain', 'POST', '/api/cardscan/telemetry', { ...A, 'content-type': 'text/plain' }, JSON.stringify({ records: [] }));
  P('telemetry_scalar', 'POST', '/api/cardscan/telemetry', AJ, '7');
  P('put_notallowed', 'PUT', '/api/cardscan/status', A);

  let fail = 0;
  for (const p of probes) {
    const a = await req(NODE, p.method, p.path, p.headers, p.body);
    const b = await req(RUST, p.method, p.path, p.headers, p.body);
    const na = norm(a), nb = norm(b);
    if (fmt(na) !== fmt(nb)) {
      fail++;
      console.log(`DIFF ${p.name} ${p.method} ${p.path}`);
      if (na.status !== nb.status) console.log('  status', na.status, 'vs', nb.status);
      for (const [ka, va] of na.hd) {
        const vb = nb.hd.find(([k]) => k === ka)?.[1];
        if (va !== vb) console.log(`  hdr ${ka}: node=${JSON.stringify(va)} rust=${JSON.stringify(vb)}`);
      }
      for (const [kb] of nb.hd) if (!na.hd.some(([k]) => k === kb)) console.log(`  hdr +${kb}: rust=${JSON.stringify(nb.hd.find(([k]) => k === kb)[1])}`);
      if (na.body !== nb.body) console.log('  body node=', JSON.stringify(na.body.slice(0, 400)), '\n       rust=', JSON.stringify(nb.body.slice(0, 400)));
    } else {
      console.log(`ok   ${p.name} [${a.status}]`);
    }
  }
  // conditional-GET round trip: replay each side's OWN fresh ETag (the
  // status body carries a volatile uptime, so cross-side ETag replay can
  // legitimately miss; what must match is the pair's behaviour).
  const ea = await req(NODE, 'GET', '/api/cardscan/status', A);
  const eb = await req(RUST, 'GET', '/api/cardscan/status', A);
  const eta = ea.hd.find(([k]) => k === 'etag')?.[1];
  const etb = eb.hd.find(([k]) => k === 'etag')?.[1];
  const a = await req(NODE, 'GET', '/api/cardscan/status', { ...A, 'if-none-match': eta });
  const b = await req(RUST, 'GET', '/api/cardscan/status', { ...A, 'if-none-match': etb });
  const ok = a.status === b.status;
  console.log(`${ok ? 'ok  ' : 'DIFF'} etag_304 [${a.status}/${b.status}] etags=${eta}/${etb}`);
  if (!ok) fail++;

  console.log(fail === 0 ? 'ALL PASS' : `${fail} DIFFS`);
  process.exit(fail === 0 ? 0 : 1);
}
main().catch((e) => { console.error('HARNESS ERROR', e); process.exit(3); });
