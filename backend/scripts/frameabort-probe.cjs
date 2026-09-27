// Probe: /frame must not cancel a normal upload (req 'close' fires on body end),
// and must cancel the upstream call when the client disconnects.
const express = require('express');
const axios = require('axios');
const http = require('http');

const upstream = express();
let upstreamAborted = 0, upstreamDone = 0;
upstream.post('/api/scan-frame', express.raw({ type: '*/*', limit: '5mb' }), (req, res) => {
  const t = setTimeout(() => { upstreamDone++; res.json({ ok: true, results: [], candidates: [], frame: {} }); }, 300);
  res.on('close', () => { if (!res.writableEnded) { upstreamAborted++; clearTimeout(t); } });
});

const app = express();
const client = axios.create({ baseURL: 'http://127.0.0.1:18321' });
app.post('/frame', express.raw({ type: 'image/jpeg', limit: '5mb' }), async (req, res) => {
  const ctl = new AbortController();
  const cancel = () => { if (!res.writableEnded) ctl.abort(); };
  res.on('close', cancel);
  try {
    const r = await client.post('/api/scan-frame', req.body, { headers: { 'Content-Type': 'image/jpeg' }, signal: ctl.signal })
      .finally(() => res.off('close', cancel));
    res.json(r.data);
  } catch (e) {
    if (e?.code === 'ERR_CANCELED') { if (!res.headersSent) res.status(499).end(); return; }
    res.status(502).end();
  }
});

(async () => {
  const s1 = upstream.listen(18321), s2 = app.listen(18322);
  const body = Buffer.alloc(200000, 1);
  const ok = await axios.post('http://127.0.0.1:18322/frame', body, { headers: { 'Content-Type': 'image/jpeg' } });
  console.log('normal upload status', ok.status, 'upstreamDone', upstreamDone);
  await new Promise((resolve) => {
    const req = http.request({ host: '127.0.0.1', port: 18322, path: '/frame', method: 'POST', headers: { 'Content-Type': 'image/jpeg' } });
    req.on('error', () => {});
    req.end(body);
    setTimeout(() => { req.destroy(); setTimeout(resolve, 500); }, 100);
  });
  console.log('after disconnect upstreamAborted', upstreamAborted, 'upstreamDone', upstreamDone);
  s1.close(); s2.close();
  process.exit(ok.status === 200 && upstreamAborted === 1 && upstreamDone === 1 ? 0 : 1);
})();
