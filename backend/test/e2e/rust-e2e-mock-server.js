// Lane B unit 7b — standalone fixture server for the e2e-on-Rust runner.
//
// The Node e2e suites get their Scryfall/MTGJSON fixtures by preloading
// scryfall-mock.js into the server process (it patches axios). The Rust front
// has no axios to patch, so this file runs that SAME module — verbatim, no
// duplicated fixtures — in its own process and answers plain HTTP with the
// fixture payloads. The scrybox-server boot pins SCRYFALL_BASE_URL,
// SCRYBOX_MTGJSON_BASE and MOXFIELD_API_BASE onto this server with the
// upstream host kept as the first path segment, which we re-materialise into
// the full URL the mock branches match on.
//
// Offline guarantee: axios's original get/post are stubbed BEFORE
// scryfall-mock.js loads, so its fallback branches (originalGet.call) can
// never reach the network — they answer 599 {error:'MOCK_NETWORK_BLOCKED'}.
'use strict';
const http = require('http');

const port = Number(process.argv[2]);
if (!port) {
  console.error('usage: rust-e2e-mock-server.js <port>');
  process.exit(2);
}

const axios = require('axios');
const block = (url) => {
  const err = new Error(`MOCK_NETWORK_BLOCKED: ${url}`);
  err.response = { status: 599, data: { error: 'MOCK_NETWORK_BLOCKED', url } };
  return Promise.reject(err);
};
axios.Axios.prototype.get = (url) => block(url);
axios.Axios.prototype.post = (url) => block(url);

// Patches Axios.prototype.get/post with the suite fixtures (scryfall +
// mtgjson branches) on top of the blocking stubs captured above. It also
// sets SCRYFALL_GAP_SCALE/HTTPS_PORT — inert in this process.
require('./scryfall-mock.js');
const patchedGet = axios.Axios.prototype.get;
const patchedPost = axios.Axios.prototype.post;

const SCRY_BASE = 'https://api.scryfall.com';
let lastReq = Date.now();

http.createServer((req, res) => {
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', async () => {
    lastReq = Date.now();
    // '/api.scryfall.com/cards/search?q=..' -> 'https://api.scryfall.com/cards/search?q=..'
    const full = 'https://' + String(req.url || '').replace(/^\/+/, '');
    try {
      let out;
      if (req.method === 'POST') {
        if (!full.startsWith(SCRY_BASE)) throw { response: { status: 599, data: { error: 'MOCK_NETWORK_BLOCKED', url: full } } };
        const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : {};
        out = await patchedPost.call({ defaults: { baseURL: SCRY_BASE } }, full.slice(SCRY_BASE.length), body);
      } else {
        out = await patchedGet.call({ defaults: { baseURL: '' } }, full);
      }
      const payload = JSON.stringify(out.data);
      res.writeHead(200, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) });
      res.end(payload);
    } catch (err) {
      const status = (err && err.response && err.response.status) || 500;
      const payload = JSON.stringify((err && err.response && err.response.data) || { error: String(err && err.message || err) });
      res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) });
      res.end(payload);
    }
  });
}).listen(port, '127.0.0.1', () => {
  // Self-reap when the owning suite process dies (orphaned mock), and after a
  // long idle, so a crashed suite can never leave fixture servers listening.
  const myPpid = process.ppid;
  const tick = setInterval(() => {
    if (process.ppid !== myPpid || Date.now() - lastReq > 30 * 60 * 1000) process.exit(0);
  }, 1000);
  tick.unref();
});
