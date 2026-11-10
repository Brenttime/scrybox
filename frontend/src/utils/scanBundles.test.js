import test from 'node:test';
import assert from 'node:assert/strict';
import { createBundles, assetsKey } from './scanBundles.js';

const M = (v) => ({ index: `scan-index.${v}.json.gz`, rec: 'rec.onnx', dict: 'dict.txt' });
const bundle = (v) => ({ manifest: M(v), info: { index: M(v).index }, reader: { v }, busy: 0, retired: false, loadMs: 5 });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };

function harness({ server = 'A', loads = {} } = {}) {
  const state = { server, released: [], fetches: 0, loads: [], clock: 1000, timers: [] };
  const b = createBundles({
    fetchManifest: async () => { state.fetches++; return M(state.server); },
    loadBundle: (m, signal) => { const v = m.index.split('.')[1]; state.loads.push({ v, signal }); return loads[v] ? loads[v].promise : Promise.resolve(bundle(v)); },
    release: (x) => state.released.push(x.manifest.index.split('.')[1]),
    now: () => state.clock,
    setTimer: (fn) => { state.timers.push(fn); return state.timers.length; },
    clearTimer: () => {},
  });
  b.install(bundle('A'));
  state.clock += 60000;
  return { b, state };
}

test('scanBundles: same hash -> fresh with one manifest request; right after load, none', async () => {
  const { b, state } = harness();
  assert.deepEqual(await b.check(), { fresh: true, info: { index: M('A').index } });
  assert.equal(state.fetches, 1);
  assert.equal((await b.check()).fresh, true);
  assert.equal(state.fetches, 1, 'within freshMs of a match: no request');
  assert.equal(assetsKey(M('A')), assetsKey(M('A')));
});

test('scanBundles: a new hash loads in the background and swaps in only at the next transaction', async () => {
  const { b, state } = harness({ server: 'B' });
  const tx = b.begin();
  assert.equal(tx.reader.v, 'A');
  const inflight = b.use(tx, () => new Promise(r => setTimeout(r, 5)));
  const res = await b.check();
  assert.equal(res.updated, true); assert.equal(res.from, M('A').index); assert.equal(res.info.index, M('B').index);
  assert.equal(b.current().reader.v, 'A', 'the read of the running transaction stays on A');
  assert.equal(b.begin().reader.v, 'B', 'the next probe uses B');
  assert.deepEqual(state.released, [], 'A is still busy');
  await inflight;
  assert.deepEqual(state.released, ['A'], 'A freed after its last read');
});

test('scanBundles: a newer manifest supersedes a slow refresh; the stale bundle is freed, never installed (R1-B3)', async () => {
  const slowB = deferred();
  const { b, state } = harness({ server: 'B', loads: { B: slowB } });
  const first = b.check();
  await new Promise(r => setTimeout(r, 0));
  state.server = 'C';
  const second = b.check();
  slowB.resolve(bundle('B'));
  await assert.rejects(first, /superseded/);
  const r2 = await second;
  assert.equal(r2.updated, true); assert.equal(r2.info.index, M('C').index);
  assert.equal(state.loads[0].signal.aborted, true, 'B fetches aborted');
  assert.ok(state.released.includes('B'));
  assert.equal(b.begin().reader.v, 'C');
});

test('scanBundles: a refresh that hits its deadline is abandoned; a late bundle is freed and the next check retries (R1-B2)', async () => {
  const hung = deferred();
  const { b, state } = harness({ server: 'B', loads: { B: hung } });
  const p = b.check();
  await new Promise(r => setTimeout(r, 0));
  state.timers[0]();                       // UPDATE deadline
  hung.resolve(bundle('B'));               // late completion
  await assert.rejects(p, /superseded/);
  assert.deepEqual(state.released, ['B']);
  assert.equal(b.begin().reader.v, 'A', 'current bundle kept');
  const retry = await b.check();
  assert.equal(retry.updated, true, 'a later check starts over');
});

test('scanBundles: a failed refresh keeps the current bundle', async () => {
  const bad = deferred();
  const { b, state } = harness({ server: 'B', loads: { B: bad } });
  const p = b.check();
  bad.reject(new Error('gzip broken'));
  await assert.rejects(p, /gzip/);
  assert.equal(b.begin().reader.v, 'A');
  assert.deepEqual(state.released, []);
});
