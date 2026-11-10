import test from 'node:test';
import assert from 'node:assert/strict';
import { createBundles, assetsKey } from './scanBundles.js';

const M = (v) => ({ index: `scan-index.${v}.json.gz`, rec: 'rec.onnx', dict: 'dict.txt' });
const bundle = (v) => ({ manifest: M(v), info: { index: M(v).index }, reader: { v }, busy: 0, retired: false, loadMs: 5 });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };

const tick = () => new Promise(r => setImmediate(r));
let harnessLoads = {};

function harness({ server = 'A', loads = {}, manifests = [] } = {}) {
  harnessLoads = loads;
  const state = { server, released: [], fetches: 0, loads: [], clock: 1000, timers: [], manifestSignals: [] };
  const b = createBundles({
    fetchManifest: async (signal) => { state.fetches++; state.manifestSignals.push(signal); return manifests.length ? manifests.shift().promise : M(state.server); },
    loadBundle: (m, signal) => { const v = m.index.split('.')[1]; state.loads.push({ v, signal }); const l = harnessLoads[v]; delete harnessLoads[v]; return l ? l.promise : Promise.resolve(bundle(v)); },
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
  first.catch(() => {});
  await new Promise(r => setTimeout(r, 0));
  state.server = 'C';
  const second = b.check();
  await tick();                            // C's manifest arrives while B builds
  slowB.resolve(bundle('B'));
  await assert.rejects(first, /superseded/);
  const r2 = await second;
  assert.equal(r2.updated, true); assert.equal(r2.info.index, M('C').index);
  assert.equal(state.loads[0].signal.aborted, true, 'B fetches aborted');
  assert.ok(state.released.includes('B'));
  assert.equal(b.begin().reader.v, 'C');
});

test('scanBundles: the refresh deadline settles the check and frees the slot WITHOUT the hung load ever resolving (R2-B2)', async () => {
  const hung = deferred();                 // never resolved in this test
  const { b, state } = harness({ server: 'B', loads: { B: hung } });
  const p = b.check();
  await tick();
  assert.equal(state.loads.length, 1);
  for (const t of state.timers.splice(0)) t();   // every deadline due
  await assert.rejects(p, /timed out|superseded/);
  assert.equal(state.loads[0].signal.aborted, true);
  assert.equal(b.begin().reader.v, 'A', 'current bundle kept');
  state.loads = [];
  const ok = deferred(); harnessLoads.B = ok;
  const retry = b.check();
  await tick();
  assert.equal(state.loads.length, 1, 'a same-key retry starts its own build, not joining the hung one');
  ok.resolve(bundle('B'));
  assert.equal((await retry).updated, true);
});

test('scanBundles: a late result of an abandoned build is released, never installed (R2-B2)', async () => {
  const hung = deferred();
  const { b, state } = harness({ server: 'B', loads: { B: hung } });
  const p = b.check();
  await tick();
  for (const t of state.timers.splice(0)) t();
  await assert.rejects(p);
  hung.resolve(bundle('B'));
  await tick();
  assert.deepEqual(state.released, ['B']);
  assert.equal(b.pending, null);
  assert.equal(b.begin().reader.v, 'A');
});

test('scanBundles: a stalled manifest request is bounded by the deadline and its late reply ignored (R2-B2)', async () => {
  const man = deferred();
  const { b, state } = harness({ server: 'B', manifests: [man] });
  const p = b.check();
  await tick();
  assert.ok(state.manifestSignals[0], 'manifest fetch gets an AbortSignal');
  for (const t of state.timers.splice(0)) t();
  await assert.rejects(p, /timed out/);
  assert.equal(state.manifestSignals[0].aborted, true);
  man.resolve(M('B'));
  await tick(); await tick();
  assert.equal(state.loads.length, 0, 'late manifest never starts a build');
  assert.equal(b.pending, null);
});

test('scanBundles: out-of-order manifests (newer C first, older B late) end on C (R2-B3)', async () => {
  const older = deferred(), newer = deferred();
  const { b, state } = harness({ manifests: [older, newer] });
  const first = b.check().catch(e => ({ error: e.message }));
  const second = b.check();
  newer.resolve(M('C'));
  assert.equal((await second).info.index, M('C').index);
  older.resolve(M('B'));
  const r1 = await first;
  assert.ok(r1.error, 'stale reply reports superseded');
  assert.equal(state.loads.filter(l => l.v === 'B').length, 0, 'stale reply never selects a target');
  assert.equal(b.begin().reader.v, 'C');
  assert.ok(!state.released.includes('C'));
});

test('scanBundles: a newer check matching the active bundle (rollback) cancels the older build (R2-B3)', async () => {
  const slowB = deferred();
  const { b, state } = harness({ server: 'B', loads: { B: slowB } });
  const first = b.check().catch(e => ({ error: e.message }));
  await tick();
  state.server = 'A';
  const second = await b.check();
  assert.equal(second.fresh, true); assert.equal(second.info.index, M('A').index);
  assert.equal(state.loads[0].signal.aborted, true, 'older B build aborted');
  slowB.resolve(bundle('B'));
  assert.ok((await first).error);
  assert.equal(b.pending, null);
  assert.equal(b.begin().reader.v, 'A', 'B never activates');
  assert.deepEqual(state.released, ['B']);
});

test('scanBundles: rollback after B is already pending drops B (R2-B3)', async () => {
  const { b, state } = harness({ server: 'B' });
  assert.equal((await b.check()).updated, true);
  state.clock += 60000; state.server = 'A';
  assert.equal((await b.check()).fresh, true);
  assert.equal(b.begin().reader.v, 'A');
  assert.deepEqual(state.released, ['B']);
});

test('scanBundles: a swap waits while the current reader owes a guess verification, bounded (R2-S3)', async () => {
  const { b } = harness({ server: 'B' });
  const tx = b.begin();
  b.noteRead(tx, { results: [{ guess_first: true }] });
  await b.check();
  assert.equal(b.begin().reader.v, 'A', 'verify read stays on the guessing reader');
  b.noteRead(b.current(), { results: [{ num: '403' }] });
  assert.equal(b.begin().reader.v, 'B', 'swap once the obligation is met');
});

test('scanBundles: the verify deferral is bounded and cleared by a no-card probe (R2-S3)', async () => {
  const h2 = harness({ server: 'B' });
  const tx = h2.b.begin();
  h2.b.noteRead(tx, { results: [{ guess_first: true }] });
  await h2.b.check();
  const v = [];
  for (let i = 0; i < 5; i++) { const x = h2.b.begin(); v.push(x.reader.v); h2.b.noteRead(x, { results: [{ guess_first: true }] }); }
  assert.ok(v.includes('B'), 'never deferred forever');
  const h3 = harness({ server: 'B' });
  const t3 = h3.b.begin();
  h3.b.noteRead(t3, { results: [{ guess_first: true }] });
  await h3.b.check();
  h3.b.noteProbe(t3, null);
  assert.equal(h3.b.begin().reader.v, 'B', 'no card -> no obligation');
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
