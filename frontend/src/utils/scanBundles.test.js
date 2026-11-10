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

// A chronological fake clock: timers fire only when the clock passes their
// due time, in order (review R3: firing every timer at once hid the skew
// between a check deadline and a build started later).
function clockHarness({ server = 'A' } = {}) {
  const s = { clock: 0, server, manifests: [], gates: new Map(), loads: [], released: [], timers: new Map(), seq: 0 };
  const b = createBundles({
    now: () => s.clock,
    fetchManifest: () => (s.manifests.length ? s.manifests.shift().promise : Promise.resolve(M(s.server))),
    loadBundle: (m, signal) => { const v = m.index.split('.')[1]; s.loads.push({ v, signal, at: s.clock }); const g = s.gates.get(v); s.gates.delete(v); return g ? g.promise : Promise.resolve(bundle(v)); },
    release: (x) => s.released.push(x.manifest.index.split('.')[1]),
    setTimer: (fn, ms) => { const id = ++s.seq; s.timers.set(id, { fn, at: s.clock + ms }); return id; },
    clearTimer: (id) => s.timers.delete(id),
  });
  b.install(bundle('A'));
  s.clock = 16000;
  const advance = async (t) => {
    for (;;) {
      const due = [...s.timers].filter(([, x]) => x.at <= t).sort((a, c) => a[1].at - c[1].at)[0];
      if (!due) break;
      s.timers.delete(due[0]); s.clock = due[1].at; due[1].fn(); await tick();
    }
    s.clock = t; await tick();
  };
  return { b, s, advance };
}
const settle = (p) => p.then(v => v, e => ({ error: e.message }));

test('scanBundles: one end-to-end deadline: a build started late by a slow manifest is cancelled when the check expires, never installed (R3-B1)', async () => {
  const { b, s, advance } = clockHarness();
  const man = deferred(), work = deferred();
  s.manifests.push(man); s.gates.set('B', work);
  const p = settle(b.check());
  await advance(16000 + 80000);                       // manifest after 80 s
  man.resolve(M('B')); await tick();
  assert.equal(s.loads.length, 1);
  await advance(16000 + 90000);                       // the check's 90 s deadline
  assert.match((await p).error, /timed out/);
  assert.equal(s.loads[0].signal.aborted, true, 'the build the check started is aborted with it');
  s.clock = 16000 + 91000;
  work.resolve(bundle('B')); await tick(); await tick();
  assert.equal(b.pending, null, 'expired build never becomes pending');
  assert.equal(b.begin().reader.v, 'A', 'last-known-good A stays');
  assert.deepEqual(s.released, ['B'], 'the late bundle is released by its build');
  assert.equal(s.timers.size, 0, 'no orphan timers');
});

test('scanBundles: a same-target retry after expiry starts a fresh build; the abandoned one cannot install (R3-B1)', async () => {
  const { b, s, advance } = clockHarness();
  const man = deferred(), old = deferred();
  s.manifests.push(man); s.gates.set('B', old);
  const p = settle(b.check());
  await advance(16000 + 80000); man.resolve(M('B')); await tick();
  await advance(16000 + 90000);
  assert.ok((await p).error);
  s.server = 'B';
  const fresh = deferred(); s.gates.set('B', fresh);
  const retry = settle(b.check()); await tick();
  assert.equal(s.loads.length, 2, 'retry did not join the expired build');
  old.resolve(bundle('B')); await tick();
  assert.equal(b.pending, null, 'the abandoned build does not install');
  fresh.resolve(bundle('B'));
  const r = await retry;
  assert.equal(r.updated, true, JSON.stringify(r));
  assert.equal(b.pending.reader.v, 'B');
  assert.notEqual(b.pending, null);
  assert.equal(s.released.length, 1, 'only the abandoned bundle released');
});

test('scanBundles: a live joiner keeps a shared build when an older waiter expires (R3-B1)', async () => {
  const { b, s, advance } = clockHarness({ server: 'B' });
  const work = deferred(); s.gates.set('B', work);
  const first = settle(b.check()); await tick();
  await advance(16000 + 60000);
  const second = settle(b.check()); await tick();     // same target, own deadline at +150 s
  assert.equal(s.loads.length, 1, 'joined, not rebuilt');
  await advance(16000 + 90000);                       // first expires; second is still live
  assert.ok((await first).error);
  assert.equal(s.loads[0].signal.aborted, false, 'still needed by the live check');
  s.clock = 16000 + 100000; work.resolve(bundle('B'));
  assert.equal((await second).updated, true);
  assert.equal(b.begin().reader.v, 'B');
});

test('scanBundles: a build finishing while a newer check awaits its manifest is held, then installed or dropped by that manifest (R3 generation caveat)', async () => {
  for (const next of ['B', 'C']) {
    const { b, s } = clockHarness({ server: 'B' });
    const work = deferred(); s.gates.set('B', work);
    const first = settle(b.check()); await tick();
    const man = deferred(); s.manifests.push(man);
    const second = settle(b.check()); await tick();
    work.resolve(bundle('B')); await tick(); await tick();
    assert.equal(b.pending, null, 'not installed while the newer manifest is outstanding');
    assert.equal(b.begin().reader.v, 'A');
    man.resolve(M(next));
    const r = await second;
    assert.equal(r.updated, true, JSON.stringify(r)); assert.equal(b.begin().reader.v, next);
    await first;
    if (next === 'C') assert.ok(s.released.includes('B'));
  }
});

test('scanBundles: a non-verifying read keeps the verify obligation; a different card or no card clears it (R3-S3)', async () => {
  const { b } = harness({ server: 'B' });
  const tx = b.begin();
  b.noteRead(tx, { results: [{ guess_first: true }], verify_pending: true });
  await b.check();
  const t2 = b.begin();
  assert.equal(t2.reader.v, 'A', 'deferred once');
  b.noteRead(t2, { results: [{ ok: false }], verify_pending: true });   // title miss
  assert.equal(t2.verifyPending, true, 'title miss keeps the obligation');
  const t3 = b.begin();
  assert.equal(t3.reader.v, 'A', 'still deferred (bounded by maxVerifyDefer = 2)');
  b.noteRead(t3, { results: [{ num: '403' }] });                        // verified
  assert.equal(b.begin().reader.v, 'B');
  const h = harness({ server: 'B' });
  const u = h.b.begin();
  h.b.noteRead(u, { results: [{ guess_first: true }], verify_pending: true });
  await h.b.check();
  h.b.begin(); h.b.noteRead(h.b.current(), { results: [{ ok: true }] });  // a different card, no pending verify
  assert.equal(h.b.begin().reader.v, 'B', 'a new card resets the obligation');
});
