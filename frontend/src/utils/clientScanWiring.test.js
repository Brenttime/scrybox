// clientScan.js worker wiring (review iOS-r3): a worker error DURING a load
// must not split the shared load promise or skip the failure cooldown.
// Loads the real clientScan.js with fake Worker/browser globals.
import test from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// Vite resolves extensionless relative imports; Node does not.
register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  try { return await next(spec, ctx); } catch (e) {
    if (/^\\.\\.?\\//.test(spec) && !/\\.[mc]?js$/.test(spec)) return next(spec + '.js', ctx);
    throw e;
  }
}`));

const workers = [];
let behave = [];   // per worker: (msg, w) => void (reply via w.reply / w.crash)
globalThis.localStorage = { getItem: () => null };
globalThis.DecompressionStream = class {};
globalThis.window = globalThis;
globalThis.Worker = class {
  constructor() { this.n = workers.length; this.dead = false; this.loads = []; workers.push(this); }
  terminate() { this.dead = true; }
  reply(data) { queueMicrotask(() => { if (!this.dead) this.onmessage({ data }); }); }
  crash(message) { queueMicrotask(() => { if (!this.dead) this.onerror({ message }); }); }
  postMessage(msg) {
    if (msg.type !== 'load') return;
    this.loads.push(msg.threads);
    (behave[this.n] || behave[behave.length - 1])(msg, this);
  }
};
let now = 1_000_000;
Date.now = () => now;
const cs = await import('./clientScan.js');
const flush = () => new Promise(r => setTimeout(r, 5));

test('worker.onerror during the threaded start: same promise, one 1-thread fallback, no third load', async () => {
  workers.length = 0;
  let release;
  const held = new Promise(r => { release = r; });
  behave = [
    (msg, w) => w.crash('pthread spawn failed'),
    (msg, w) => held.then(() => w.reply({ id: msg.id, ready: true, loadMs: 1, info: { threads: Number(msg.threads) } })),
  ];
  const a = cs.loadClientScan();
  await flush();                                   // crash handled, fallback held in flight
  assert.equal(workers.length, 2, 'fallback worker started');
  const b = cs.loadClientScan();
  release();
  assert.equal(b, a, 'a caller during the fallback joins the same promise');
  const r = await a;
  assert.equal(r.ok, true);
  assert.equal(workers.length, 2);
  assert.deepEqual(workers[1].loads, ['1']);
  assert.equal(r.info.threadFallback, 'pthread spawn failed');
  assert.equal(cs.loadClientScan(), a);
});

test('onerror then a failed 1-thread fallback: cooldown holds, then a fresh worker', async () => {
  // Reset module state: the previous test left a ready promise; force expiry via a failed one.
  workers.length = 0;
  behave = [(msg, w) => w.crash('boom'), (msg, w) => w.reply({ id: msg.id, ready: false, error: 'still bad', threads: 1 })];
  const cs2 = await import('./clientScan.js?cooldown');
  const a = cs2.loadClientScan();
  const r = await a;
  assert.equal(r.ok, false);
  await flush();
  const base = workers.length;
  assert.equal(cs2.loadClientScan(), a, 'within LOAD_RETRY_MS: same failed promise, no new worker');
  now += 29_999;
  assert.equal(cs2.loadClientScan(), a);
  assert.equal(workers.length, base);
  now += 2;
  behave = [(msg, w) => w.reply({ id: msg.id, ready: true, info: { threads: Number(msg.threads) } })];
  const b = cs2.loadClientScan();
  assert.notEqual(b, a, 'after the cooldown: a new load');
  const rb = await b;
  assert.equal(rb.ok, true);
  assert.equal(workers.length, base + 1, 'on a fresh worker');
  assert.equal(workers[workers.length - 1].loads[0], '1', 'still one thread (sticky)');
  assert.ok(workers.slice(0, base).every(w => w.dead), 'failed workers retired');
});

// Review R3-S2: one shared /choices request per twin set; each caller's abort
// ends only its own wait, never the request or another caller's.
test('fetchChoices: shared request, per-caller cancellation', async () => {
  let calls = 0, release;
  globalThis.fetch = async (url) => {
    assert.equal(url, '/api/cardscan/choices'); calls++;
    await new Promise(r => { release = r; });
    return { ok: true, json: async () => ({ ok: true, cards: [{ id: 'mtg-a' }, { id: 'mtg-b' }] }) };
  };
  const ids = [{ scryfallId: 'a' }, { scryfallId: 'b' }];
  const first = new AbortController(), joiner = new AbortController();
  const p1 = cs.fetchChoices(ids, first.signal);
  const p2 = cs.fetchChoices([...ids].reverse(), joiner.signal);
  const pre = new AbortController(); pre.abort();
  await assert.rejects(cs.fetchChoices(ids, pre.signal), { name: 'AbortError' });
  joiner.abort();
  await assert.rejects(p2, { name: 'AbortError' });
  first.abort();
  await assert.rejects(p1, { name: 'AbortError' });
  const p3 = cs.fetchChoices(ids, new AbortController().signal);
  release();
  assert.equal((await p3).length, 2, 'the shared request survives every caller abort');
  assert.equal(calls, 1);
  assert.equal((await cs.fetchChoices(ids)).length, 2);
  assert.equal(calls, 1, 'cached');
});
