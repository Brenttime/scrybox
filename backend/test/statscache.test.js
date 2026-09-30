// statsCache: hit after first GET, invalidated by any write from that user,
// isolated between users.
const assert = require('assert');
delete process.env.STATS_CACHE;
const { cached, invalidateOnWrite } = require('../src/utils/statsCache');

function mockRes() { const r = { statusCode: 200, body: null, headers: {} }; r.set = (k, v) => { r.headers[k] = v; }; r.json = (b) => { r.body = b; return r; }; return r; }
(async () => {
  let n = 0;
  const h = cached(() => 'stats', async (req, res) => res.json({ n: ++n }));
  const call = async (uid) => { const res = mockRes(); await h({ user: { id: uid }, query: {} }, res); return res; };
  const a = await call(1); const b = await call(1);
  assert.strictEqual(a.body.n, 1); assert.strictEqual(b.body.n, 1, 'second GET is a cache hit');
  assert.strictEqual(b.headers['X-Stats-Cache'], 'hit');
  assert.strictEqual((await call(2)).body.n, 2, 'users do not share cache');
  invalidateOnWrite({ method: 'POST', user: { id: 1 } }, {}, () => {});
  assert.strictEqual((await call(1)).body.n, 3, 'a write by the user invalidates');
  invalidateOnWrite({ method: 'GET', user: { id: 1 } }, {}, () => {});
  assert.strictEqual((await call(1)).body.n, 3, 'a GET does not invalidate');
  console.log('statscache.test.js: all assertions passed');
})().catch((e) => { console.error(e); process.exit(1); });
