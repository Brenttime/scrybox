// Scan telemetry metadata (review R2-S1, R2-S4, R2-N1).
// S1: the FastScanner load effect's async completions (persist(), bundle
//     check) may land in any order; the batch device metadata must keep
//     persisted/threadFallback and never restore the shared load's bundle A
//     after the check reported B. Runs the REAL effect body against the real
//     telemetry module.
// S4: each record snapshots its stream's camera at pass start.
// N1: scan.res only accepts '720'/'1080'/'1440'.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const sent = [];
globalThis.localStorage = { getItem: () => null };
globalThis.fetch = (url, opts) => { sent.push(JSON.parse(opts.body)); return Promise.resolve({}); };
const { scanTelemetry } = await import('./scanTelemetry.js');

const comp = fs.readFileSync(new URL('../components/FastScanner.jsx', import.meta.url), 'utf8');
const effect = comp.slice(comp.indexOf('    const attempt = () => loadClientScan()'), comp.indexOf('    return () => { live = false; clearTimeout(timer); };'));
assert.ok(effect.length > 50, 'effect body found');
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
const tick = () => new Promise(r => setTimeout(r, 0));

async function runEffect(order) {
  const persist = deferred(), check = deferred();
  const c = vm.createContext({
    console, Promise, live: true, timer: null, onDeviceRef: {}, setOnDevice: () => {}, setTimeout: () => 0, scanTelemetry,
    loadClientScan: async () => ({ ok: true, loadMs: 7, info: { index: 'A', rec: 'old-rec', threads: 1, threadFallback: 'thread compile failed' } }),
    requestPersistentStorage: () => persist.promise, checkClientScan: () => check.promise,
  });
  vm.runInContext(effect, c);
  await tick();
  const cr = { updated: true, from: 'A', loadMs: 50, info: { index: 'B', rec: 'new-rec', threads: 1 } };
  if (order === 'persist-then-check') { persist.resolve(true); await tick(); check.resolve(cr); }
  else { check.resolve(cr); await tick(); persist.resolve(true); }
  await tick();
  return scanTelemetry.device();
}

for (const order of ['persist-then-check', 'check-then-persist']) {
  test(`device metadata survives ${order}`, async () => {
    const d = await runEffect(order);
    assert.equal(d.index, 'B');
    assert.equal(d.rec, 'new-rec');
    assert.equal(d.persisted, true);
    assert.equal(d.threadFallback, 'thread compile failed');
    assert.equal(d.loadMs, 7);
    assert.equal(d.updatedFrom, 'A');
    assert.equal(d.ok, true);
  });
}

test('a remount re-reading the shared ready (A) does not restore A', async () => {
  scanTelemetry.loaded({ ok: true, loadMs: 7, info: { index: 'A', rec: 'old-rec' } });
  assert.equal(scanTelemetry.device().index, 'B');
  assert.equal(scanTelemetry.device().persisted, true);
});

test('records snapshot bundle identity (empty probes too) and camera per pass', () => {
  sent.length = 0;
  scanTelemetry.camera({ w: 720, h: 1280, fps: 30, asked: [1920, 1080], max: [1920, 1080], deviceId: 'x' });
  const t1 = scanTelemetry.begin({ mode: 'auto', sw: 720, sh: 1280 });
  t1.local({ ok: true, index: 'B', rec: 'new-rec', candidates: [], results: [] });
  t1.end({ outcome: 'added' });
  // New stream before the flush: the queued row must keep its own stream.
  scanTelemetry.camera({ w: 1080, h: 1920, fps: 30, asked: [1920, 1080] });
  const t2 = scanTelemetry.begin({ mode: 'auto', sw: 1080, sh: 1920 });
  t2.end({ outcome: 'added' });
  scanTelemetry.flush();
  const rows = sent.flatMap(b => b.records.map(r => ({ ...r, batchCam: b.camera })));
  assert.equal(rows.length, 2);
  const [a, b] = rows;
  assert.deepEqual([a.camera.w, a.camera.h], a.video);
  assert.deepEqual([b.camera.w, b.camera.h], b.video);
  assert.equal(a.camera.deviceId, undefined);
  assert.notEqual(a.camera.gen, b.camera.gen);
  // The first batch was flushed with its own stream's camera.
  assert.deepEqual([a.batchCam.w, a.batchCam.h], a.video);
  assert.equal(a.local.index, 'B');
  assert.equal(a.local.rec, 'new-rec');
  assert.equal(a.local.status, 'no card');
});

test('scan.res accepts only the documented presets', () => {
  const src = comp.slice(comp.indexOf('const RES ='), comp.indexOf('// Some browsers still hand back VGA'));
  const size = (v, coarse = false) => [...vm.runInNewContext(`${src}; cameraSize()`, {
    localStorage: { getItem: () => v }, matchMedia: () => ({ matches: coarse }), String, Map,
  })];
  assert.deepEqual(size('720'), [1280, 720]);
  assert.deepEqual(size('1080'), [1920, 1080]);
  assert.deepEqual(size('1440'), [2560, 1440]);
  for (const bad of ['__proto__', 'constructor', 'garbage', 'toString', null]) {
    assert.deepEqual(size(bad), [2560, 1440], bad);
    assert.deepEqual(size(bad, true), [1920, 1080], bad);
  }
});
