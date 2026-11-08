// statsCache: hit after first GET, invalidated by any write from that user,
// isolated between users.
const assert = require('assert');
delete process.env.STATS_CACHE;
const { cached, invalidateOnWrite, setRevisionSource } = require('../src/utils/statsCache');
let rev = 1;
setRevisionSource(async () => String(rev));

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
  rev++; // a background write (price sweep, Moxfield sync) bumps the DB revision
  assert.strictEqual((await call(1)).body.n, 4, 'a DB revision change invalidates');
  assert.strictEqual((await call(1)).body.n, 4, 'same revision hits again');
  const onHit = cached(() => 'nw', async (req, res) => res.json({ n: ++n, asOf: 'old' }), { onHit: (b) => ({ ...b, asOf: 'fresh' }) });
  const r1 = mockRes(); await onHit({ user: { id: 1 }, query: {} }, r1);
  const r2 = mockRes(); await onHit({ user: { id: 1 }, query: {} }, r2);
  assert.strictEqual(r2.body.n, r1.body.n); assert.strictEqual(r2.body.asOf, 'fresh');
  console.log('statscache.test.js: all assertions passed');
})().catch((e) => { console.error(e); process.exit(1); });
