// scanLoad.startLoad (review iOS-COEP S1): a threaded start that fails is
// retried ONCE in a fresh worker at one thread, sticky for the session; a
// failed start retires its worker; telemetry keeps the reason.
import test from 'node:test';
import assert from 'node:assert/strict';
import { startLoad } from './scanLoad.js';

function rig(replies) {
  const log = [];
  let n = 0;
  return {
    log,
    loadOnce: async (threads) => { log.push(['load', threads]); return replies[Math.min(n++, replies.length - 1)](threads); },
    retire: (why) => log.push(['retire', why]),
  };
}

test('threaded failure -> retire + one reload at 1 thread; reason in info; sticky', async () => {
  const state = { forceOneThread: false, threadFallback: null };
  const g = rig([() => ({ ready: false, error: 'pthread abort', threads: 2 }), (t) => ({ ready: true, info: { threads: Number(t) } })]);
  const r = await startLoad({ ...g, override: null, state });
  assert.equal(r.ok, true);
  assert.deepEqual(g.log, [['load', null], ['retire', 'threaded start failed'], ['load', '1']]);
  assert.equal(r.info.threads, 1);
  assert.equal(r.info.threadFallback, 'pthread abort');
  // Next session load (after a later failure/reload) starts at 1 and keeps the reason.
  const g2 = rig([(t) => ({ ready: true, info: { threads: Number(t) } })]);
  const r2 = await startLoad({ ...g2, override: null, state });
  assert.deepEqual(g2.log, [['load', '1']]);
  assert.equal(r2.info.threadFallback, 'pthread abort');
});

test('both starts fail: no loop, the failed worker is retired for the cooldown retry', async () => {
  const state = { forceOneThread: false, threadFallback: null };
  const g = rig([() => ({ ready: false, error: 'x', threads: 2 }), () => ({ ready: false, error: 'y', threads: 1 })]);
  const r = await startLoad({ ...g, override: null, state });
  assert.equal(r.ok, false);
  assert.deepEqual(g.log, [['load', null], ['retire', 'threaded start failed'], ['load', '1'], ['retire', 'load failed']]);
});

test('single-thread failure (no isolation) does not retry and is retired', async () => {
  const state = { forceOneThread: false, threadFallback: null };
  const g = rig([() => ({ ready: false, error: 'net', threads: 1 })]);
  const r = await startLoad({ ...g, override: null, state });
  assert.equal(r.ok, false);
  assert.deepEqual(g.log, [['load', null], ['retire', 'load failed']]);
  assert.equal(state.forceOneThread, false);
});

test('worker timeout (no threads in reply) counts as a threaded failure only when not already 1', async () => {
  const state = { forceOneThread: false, threadFallback: null };
  const g = rig([() => ({ error: 'scan worker timed out' }), (t) => ({ ready: true, info: { threads: Number(t) } })]);
  const r = await startLoad({ ...g, override: null, state });
  assert.equal(r.ok, true);
  assert.equal(r.info.threads, 1);
  assert.equal(r.info.threadFallback, 'scan worker timed out');
});

test('a successful threaded start is untouched', async () => {
  const state = { forceOneThread: false, threadFallback: null };
  const g = rig([() => ({ ready: true, info: { threads: 2 } })]);
  const r = await startLoad({ ...g, override: null, state });
  assert.deepEqual(g.log, [['load', null]]);
  assert.equal(r.info.threads, 2);
  assert.equal(r.info.threadFallback, undefined);
});
