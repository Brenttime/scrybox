import test from 'node:test';
import assert from 'node:assert/strict';
import { fitContain, quadPath, FRAME_MAX, zoomPlan, nextFailStreak, serverAllowed, needsServer, nextEdgeRun, isEdgePartial, EDGE_ESCAPE, nextPresentation, dedupeFresh, notePresence, edgeDirection, DEDUPE_MS, pixelPrint, presentationInput } from './fastScan.js';

const fail = (title, box = [100, 100, 600, 840]) => ({
  frame: { width: 1080, height: 1920 },
  candidates: [{ number: 1, box, eligible: true }],
  results: [{ scene_number: 1, ok: false, title }],
});

test('repeat-failure backoff: same unresolved card spaces out, anything else resets', () => {
  let s = nextFailStreak(null, fail('Fblthp'), 0);
  // Backoff needs the phone to see the SAME card now; no local evidence -> allowed.
  const local = (title, box = [100, 100, 600, 840]) => ({ frame: { width: 1080, height: 1920 }, candidates: [{ box }], results: [{ title }] });
  assert.equal(s.count, 1); assert.equal(serverAllowed(s, 500, local('fblthp')), false); assert.equal(serverAllowed(s, 1000, local('Fblthp')), true);
  assert.equal(serverAllowed(s, 500), true, 'no on-device read: never block');
  assert.equal(serverAllowed(s, 500, local('Draconic Visitor')), true, 'different card: never block');
  assert.equal(serverAllowed(s, 500, local('Fblthp', [700, 1400, 300, 420])), true, 'moved: never block');
  assert.equal(serverAllowed(s, 500, { frame: { width: 1080, height: 1920 }, candidates: [{ box: [100, 100, 600, 840] }], results: [] }), true, 'no title read: never block');
  assert.equal(serverAllowed(s, 500, { frame: { width: 1080, height: 1920 }, candidates: [], results: [] }), true, 'card lifted: never block');
  s = nextFailStreak(s, fail('Fblthp', [110, 105, 600, 840]), 2000);
  assert.equal(s.count, 2); assert.equal(s.until, 4000);
  for (let i = 0; i < 6; i++) s = nextFailStreak(s, fail('Fblthp'), 10000);
  assert.equal(s.until, 13000, 'capped at 3 s: a delay, never a block');
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

test('backoff survives settling / no-result passes (Konstrari Charm: settle between fallbacks)', () => {
  const s1 = nextFailStreak(null, fail('Konstrari Charm'), 0);
  const settling = { frame: { width: 1080, height: 1920 }, candidates: [{ number: 1, box: [100, 100, 600, 840], eligible: false, status: 'settling' }], results: [] };
  const s2 = nextFailStreak(s1, settling, 100);
  assert.deepEqual(s2, s1, 'a settling pass proves nothing: streak kept');
  const local = { frame: { width: 1080, height: 1920 }, candidates: [{ box: [100, 100, 600, 840] }], results: [{ title: 'Konstrari Charm' }] };
  assert.equal(serverAllowed(s2, 200, local), false, 'next still frame of the same card stays backed off');
  const edge = { frame: { width: 1080, height: 1920 }, candidates: [{ number: 1, box: [100, 100, 600, 840], eligible: true }], results: [{ number: 1, ok: false, near_edge_partial: { stage: 'title1', off: [0] }, title: null }] };
  assert.deepEqual(nextFailStreak(s1, edge, 100), s1, 'title-less edge partial keeps it too');
  assert.equal(nextFailStreak(s1, { candidates: [], results: [] }, 0), null, 'lifted card ends it');
  assert.equal(nextFailStreak(s1, { candidates: [{ box: [1, 1, 2, 2] }], results: [{ ok: true, card: {} }] }, 0), null, 'a hit ends it');
});

test('near-edge partials take a typed path: no server until a bounded escape, reset on move', () => {
  const edge = (box = [100, 4, 600, 840], title = null) => ({ frame: { width: 1920, height: 1080 }, candidates: [{ number: 1, box, eligible: true }], results: [{ number: 1, ok: false, retry: true, error: 'card too close to the frame edge', near_edge_partial: { stage: 'title1', off: [0] }, title }] });
  assert.equal(isEdgePartial(edge()), true);
  let run = null;
  for (let i = 1; i < EDGE_ESCAPE; i++) { run = nextEdgeRun(run, edge()); assert.equal(needsServer(edge(), { autoPass: true, edgeRun: run.count }), false, `pass ${i} stays local`); }
  run = nextEdgeRun(run, edge());
  assert.equal(run.count, EDGE_ESCAPE);
  assert.equal(needsServer(edge(), { autoPass: true, edgeRun: run.count }), true, 'bounded server rescue');
  assert.equal(nextEdgeRun(run, edge([600, 4, 600, 840])).count, 1, 'moved card starts over');
  assert.equal(nextEdgeRun(run, fail('Grief')), null, 'any other outcome resets');
  assert.equal(needsServer(edge(undefined, 'solarium sentry'), { autoPass: true, edgeRun: 1 }), false, 'title read, footer off-frame: still local');
  assert.equal(needsServer(edge(), { autoPass: false }), true, 'shutter press still asks the server');
});

test('backoff: on-device outcomes never renew the deadline (review R1-B2)', () => {
  const s1 = nextFailStreak(null, fail('Konstrari Charm'), 0);
  const until = s1.until;
  let s = s1;
  for (let t = 100; t < 20000; t += 600) s = nextFailStreak(s, fail('Konstrari Charm'), t, { fromServer: false });
  assert.equal(s.until, until, 'local deferred/unresolved reads keep the original deadline');
  const local = { frame: { width: 1080, height: 1920 }, candidates: [{ box: [100, 100, 600, 840] }], results: [{ title: 'Konstrari Charm' }] };
  assert.equal(serverAllowed(s, until, local), true, 'server is asked again once the deadline passes');
  assert.equal(nextFailStreak(null, fail('Konstrari Charm'), 0, { fromServer: false }), null, 'a local miss alone never starts a streak');
});

test('edge run: a moving pass between partials resets it (review R1-S1)', () => {
  const edge = { frame: { width: 1920, height: 1080 }, candidates: [{ number: 1, box: [100, 4, 600, 840], eligible: true }], results: [{ number: 1, ok: false, near_edge_partial: { stage: 'title1', off: [0] } }] };
  let run = null;
  for (let i = 0; i < 7; i++) run = nextEdgeRun(run, edge);
  assert.equal(nextEdgeRun(run, { frame: edge.frame, candidates: [{ number: 1, box: [900, 200, 600, 840], eligible: false, status: 'moving' }], results: [] }), null);
});

test('backoff: a titleless server failure is signed with the same-frame client title (R2-10)', () => {
  // Helm of the Host: sidecar says "no confident card title", client read Helm.
  const srv = { frame: { width: 1920, height: 1080 }, candidates: [{ number: 1, box: [500, 130, 750, 920], eligible: true }], results: [{ number: 1, ok: false, error: 'no confident card title' }] };
  const local = { frame: { width: 1920, height: 1080 }, candidates: [{ number: 1, box: [502, 131, 750, 918], eligible: true }], results: [{ number: 1, ok: false, title: 'helm of the host', error: 'exact printing not resolved' }] };
  const s = nextFailStreak(null, srv, 0, { fromServer: true, sameFrameLocal: local });
  assert.ok(s, 'streak started from the same-frame local title');
  assert.equal(s.title, 'helm of the host');
  assert.equal(serverAllowed(s, 500, local), false, 'same card same place: backs off');
  const other = { ...local, results: [{ number: 1, ok: false, title: 'galactus' }] };
  assert.equal(serverAllowed(s, 500, other), true, 'a changed title is never held');
  assert.equal(serverAllowed(s, 1000, local), true, 'deadline releases');
  assert.equal(nextFailStreak(null, srv, 0, { fromServer: false, sameFrameLocal: local }), null, 'only a real server attempt');
  const multi = { ...srv, candidates: [...srv.candidates, { number: 2, box: [1200, 100, 600, 840] }] };
  assert.equal(nextFailStreak(null, multi, 0, { fromServer: true, sameFrameLocal: local }), null, 'multi-card server scene: not signed from the single-card client read');
});

const seen1 = (title, box = [506, 127, 752, 922], status = 'ready') => ({ frame: { width: 1920, height: 1080 }, candidates: [{ number: 1, box, status, eligible: status === 'ready' }], results: [{ number: 1, ok: false, title }] });

test('dedupe: a card that never left is not re-added after 4 s of failed reads (Masamune, R2-0)', () => {
  const seen = new Map();
  const id = 'mtg-masamune', hit = [{ card: { id, name: 'The Masamune' } }];
  assert.equal(dedupeFresh(seen, id, 0), true);
  notePresence(seen, seen1('the masamune'), hit, 0);
  for (let t = 500; t < 9000; t += 500) notePresence(seen, seen1('the masamune'), [], t);   // unresolved, same box
  assert.equal(dedupeFresh(seen, id, 9000), false, 'same card, same place, never lifted: repeat');
});

test('dedupe: a genuine second copy is added after a lift, a slide, or hand motion (R2-0)', () => {
  const id = 'mtg-bolt', hit = [{ card: { id, name: 'Lightning Bolt' } }];
  for (const between of [
    { frame: { width: 1920, height: 1080 }, candidates: [], results: [] },         // lifted
    seen1('lightning bolt', [100, 100, 600, 840]),                                    // moved elsewhere
    seen1('galactus'),                                                                // other card at the spot
  ]) {
    const seen = new Map();
    notePresence(seen, seen1('lightning bolt'), hit, 0);
    notePresence(seen, between, [], 1000);
    // The second copy is recognized again and again (suppressed repeats):
    // that must neither clear the break nor push the deadline (R1-B1).
    for (let tt = 1500; tt < DEDUPE_MS; tt += 500) notePresence(seen, seen1('lightning bolt'), hit, tt, new Set());
    assert.equal(dedupeFresh(seen, id, DEDUPE_MS - 1), false, 'still inside the 4 s window');
    assert.equal(dedupeFresh(seen, id, DEDUPE_MS + 1), true, JSON.stringify(between.candidates));
  }
});

test('presentation epoch: a different title or a lift starts a new one; waiting on the same card does not (R2-0)', () => {
  let p = nextPresentation(null, seen1('helm of the host'), 0);
  assert.equal(p.epoch, 1); assert.equal(p.titleAt, 0);
  p = nextPresentation(p, seen1('helm of the host'), 500);
  assert.equal(p.epoch, 1);
  const q = nextPresentation(p, seen1('galactus'), 3500);
  assert.equal(q.epoch, 2, 'Helm -> Galactus is a new presentation');
  assert.equal(q.since, 3500);
  assert.equal(nextPresentation(q, { frame: { width: 1, height: 1 }, candidates: [], results: [] }, 4000), null);
  const r = nextPresentation(q, seen1(null, [506, 127, 752, 922], 'moving'), 3600);
  assert.equal(r.epoch, 2, 'no title this pass: same presentation');
  let m = r; for (let i = 0; i < 3; i++) m = nextPresentation(m, seen1(null, [506, 127, 752, 922], 'moving'), 3700 + i);
  assert.equal(m.epoch, 2);
  const after = nextPresentation(m, seen1('galactus'), 3800);
  assert.equal(after.epoch, 3, 'sustained hand motion then a still card: new presentation (R2-S1)');
  assert.equal(nextPresentation(q, { error: 'x' }, 4000), q, 'errors carry no information');
});

test('edgeDirection: which way to move the card (R2-9)', () => {
  assert.equal(edgeDirection({ edge_sides: ['top'] }), 'down');
  assert.equal(edgeDirection({ edge_sides: ['bottom', 'right'] }), 'up-left');
  assert.equal(edgeDirection({ edge_sides: ['top', 'bottom'] }), 'back');
  assert.equal(edgeDirection({}), null);
});

test('dedupe: sustained hand motion breaks a presentation, one blurred pass does not (R2-0)', () => {
  const id = 'mtg-bolt', hit = [{ card: { id, name: 'Lightning Bolt' } }];
  const seen = new Map();
  notePresence(seen, seen1('lightning bolt'), hit, 0);
  notePresence(seen, seen1(null, [506, 127, 752, 922], 'too blurry'), [], 500);
  notePresence(seen, seen1('lightning bolt'), hit, 1000, new Set());
  assert.equal(dedupeFresh(seen, id, 9000), false, 'autofocus blip on a sitting card: no re-add');
  for (let i = 0; i < 3; i++) notePresence(seen, seen1(null, [506, 127, 752, 922], 'moving'), [], 9100 + i * 100);
  assert.equal(dedupeFresh(seen, id, 9500), true, 'hand swapped the card: second copy allowed');
});

test('presentation epochs are monotonic across lifts (R1-S1)', () => {
  let base = 0;
  const a = nextPresentation(null, seen1('grief'), 0, base); base = a.epoch;
  assert.equal(nextPresentation(a, { frame: { width: 1, height: 1 }, candidates: [], results: [] }, 10, base), null);
  const b = nextPresentation(null, seen1('grief'), 20, base);
  assert.equal(b.epoch, a.epoch + 1);
});

test('pixelPrint: identical frames match, a one-level change in any sampled channel does not (R1-B2)', () => {
  const a = new Uint8ClampedArray(384 * 384 * 4); for (let i = 0; i < a.length; i++) a[i] = (i * 7) & 255;
  const b = a.slice();
  assert.equal(pixelPrint(a.buffer), pixelPrint(b.buffer));
  for (const i of [0, 1, 2, 5, 12345, a.length - 5]) { const c = a.slice(); c[i - (i % 5)] ^= 1; assert.notEqual(pixelPrint(c.buffer), pixelPrint(a.buffer), String(i)); }
});

test('presentationInput: title from the source that read it, geometry from the local read (R4-S1)', () => {
  const local = seen1(null); const srv = { frame: local.frame, candidates: local.candidates, results: [{ ok: true, card: { name: 'Helm of the Host' } }] };
  const a = presentationInput(local, false, srv);
  assert.equal(a.candidates, local.candidates);
  assert.equal(nextPresentation(null, a, 100, 0, 1000).titleAt, 1000, 'server title carried with its time');
  const titled = seen1('galactus');
  assert.equal(presentationInput(titled, true, srv), titled);
  assert.equal(presentationInput(null, false, srv), srv);
  assert.equal(presentationInput(local, false, { results: [] }), local);
});
