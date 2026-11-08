import test from 'node:test';
import assert from 'node:assert/strict';
import { waitForFirstScreenImages } from './collectionSessionCache.js';

function fakeImg({ complete = false, top = 0, bottom = 100 } = {}) {
  const listeners = {};
  return {
    complete,
    getBoundingClientRect: () => ({ top, bottom }),
    addEventListener: (type, fn) => { listeners[type] = fn; },
    removeEventListener: (type) => { delete listeners[type]; },
    fire: (type) => listeners[type]?.(),
    listeners,
  };
}

test('resolves immediately when every visible image is already loaded', async () => {
  assert.equal(await waitForFirstScreenImages({ images: [fakeImg({ complete: true })], viewportHeight: 800 }), true);
});

test('ignores off-screen images and waits for on-screen ones', async () => {
  const on = fakeImg();
  const off = fakeImg({ top: 2000, bottom: 2300 });
  let resolved = false;
  const p = waitForFirstScreenImages({ images: [on, off], viewportHeight: 800, timeout: 5000 }).then(v => { resolved = true; return v; });
  await new Promise(r => setTimeout(r, 10));
  assert.equal(resolved, false);
  assert.equal(off.listeners.load, undefined, 'off-screen image is not awaited');
  on.fire('error');
  assert.equal(await p, true);
  assert.deepEqual(Object.keys(on.listeners), [], 'listeners are removed');
});

test('times out instead of blocking hydration forever', async () => {
  const t0 = Date.now();
  assert.equal(await waitForFirstScreenImages({ images: [fakeImg()], viewportHeight: 800, timeout: 30 }), true);
  assert.ok(Date.now() - t0 < 1000);
});

test('abort resolves false', async () => {
  const ctrl = new AbortController();
  const p = waitForFirstScreenImages({ images: [fakeImg()], viewportHeight: 800, timeout: 5000, signal: ctrl.signal });
  ctrl.abort();
  assert.equal(await p, false);
});
