import test from 'node:test';
import assert from 'node:assert/strict';
import { fitContain, quadPath, FRAME_MAX, zoomPlan, nextFailStreak, serverAllowed } from './fastScan.js';

const fail = (title, box = [100, 100, 600, 840]) => ({
  frame: { width: 1080, height: 1920 },
  candidates: [{ number: 1, box, eligible: true }],
  results: [{ scene_number: 1, ok: false, title }],
});

test('repeat-failure backoff: same unresolved card spaces out, anything else resets', () => {
  let s = nextFailStreak(null, fail('Fblthp'), 0);
  assert.equal(s.count, 1); assert.equal(serverAllowed(s, 1000), false); assert.equal(serverAllowed(s, 1500), true);
  s = nextFailStreak(s, fail('Fblthp', [110, 105, 600, 840]), 2000);
  assert.equal(s.count, 2); assert.equal(s.until, 5000);
  for (let i = 0; i < 6; i++) s = nextFailStreak(s, fail('Fblthp'), 10000);
  assert.equal(s.until, 18000, 'capped at 8 s');
  // different card, moved card, or a success resets
  assert.equal(nextFailStreak(s, fail('Draconic Visitor'), 0).count, 1);
  assert.equal(nextFailStreak(s, fail('Fblthp', [700, 1400, 300, 420]), 0).count, 1);
  assert.equal(nextFailStreak(s, { candidates: [], results: [{ ok: true, card: {} }] }, 0), null);
  assert.equal(nextFailStreak(s, { candidates: [], results: [] }, 0), null);
  assert.equal(serverAllowed(null, 0), true);
});

test('FRAME_MAX is the measured 1920 ceiling', () => { assert.equal(FRAME_MAX, 1920); });

test('fitContain letterboxes and cover crops', () => {
  const f = { width: 1920, height: 1080 };
  const c = fitContain(f, 960, 960);
  assert.equal(c.s, 0.5); assert.equal(c.ox, 0); assert.equal(c.oy, 210);
  const v = fitContain(f, 960, 960, 'cover');
  assert.ok(Math.abs(v.s - 960 / 1080) < 1e-9); assert.ok(v.ox < 0); assert.equal(v.oy, 0);
});

test('quadPath maps quad or box corners', () => {
  const fit = { s: 0.5, ox: 10, oy: 20 };
  assert.deepEqual(quadPath({ box: [0, 0, 100, 200] }, fit), [[10, 20], [60, 20], [60, 120], [10, 120]]);
  assert.deepEqual(quadPath({ quad: [[2, 2], [4, 2], [4, 4], [2, 4]] }, fit)[2], [12, 22]);
});

test('zoomPlan crops small failed cards from the native frame', () => {
  const frame = { width: 1920, height: 1080 };
  const candidates = [
    { number: 1, eligible: true, box: [400, 100, 220, 308] },   // small, failed
    { number: 2, eligible: true, box: [900, 100, 600, 840] },   // big
    { number: 3, eligible: true, box: [100, 600, 200, 280] },   // small but read ok
    { number: 4, eligible: false, box: [50, 50, 100, 140] },
  ];
  const results = [{ number: 1, ok: false }, { number: 3, ok: true }];
  const native = zoomPlan({ candidates, results, frame, sw: 3840, sh: 2160 });
  assert.equal(native.crops.length, 1);
  assert.deepEqual(native.tooSmall, []);
  const c = native.crops[0];
  assert.equal(c.number, 1);
  assert.equal(c.sx, Math.round((400 - 66) * 2));
  assert.ok(c.scale <= 1 && Math.abs(c.scale - 620 / 440) > 0, 'never upscales');
  const capped = zoomPlan({ candidates, results, frame, sw: 1920, sh: 1080 });
  assert.equal(capped.crops.length, 0, 'no native gain: no zoom');
  assert.deepEqual(capped.tooSmall, [1], 'reported for a move-closer hint');
});
