// POST /api/cardscan/telemetry (review R2-S4): each record keeps its own
// camera snapshot (the stream it was read from), sanitized numbers-only like
// the batch camera; arbitrary fields such as deviceId are never stored.
// Runs the real handler (sliced from routes/cardscan.js) against a temp dir.
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const vm = require('vm');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scrybox-scantel-'));
const source = fs.readFileSync(path.join(__dirname, '..', 'src', 'routes', 'cardscan.js'), 'utf8');
const code = source.slice(source.indexOf("const fs = require('fs');"), source.indexOf("router.post('/scan',"));
let handler;
const ctx = vm.createContext({
  require, __dirname, process: { env: { SCAN_TELEMETRY_DIR: dir } },
  express: { json: () => () => {} },
  router: { post: (route, ...args) => { if (route === '/telemetry') handler = args.at(-1); } },
  Date, Number, Array, String, JSON,
});
vm.runInContext(code, ctx);
assert.ok(handler, 'telemetry handler found');

const body = {
  session: 's', platform: { os: 'ios' }, device: { index: 'B' },
  camera: { w: 1080, h: 1920, fps: 29.97, asked: [1920, 1080, 9], gen: 2, deviceId: 'secret' },
  records: [
    { id: 'old', video: [720, 1280], camera: { w: 720, h: 1280, fps: 30, gen: 1, deviceId: 'secret', label: 'x' } },
    { id: 'none', video: [1080, 1920] },
  ],
};
(async () => {
  let status;
  await new Promise((resolve, reject) => {
    handler({ body, user: { id: 1 }, get: () => '' }, { status(n) { status = n; return this; }, end: resolve, json: (x) => reject(new Error(JSON.stringify(x))) });
  });
  assert.strictEqual(status, 204);
  const [file] = fs.readdirSync(dir);
  const rows = fs.readFileSync(path.join(dir, file), 'utf8').trim().split('\n').map(JSON.parse);
  assert.deepStrictEqual(rows[0].camera, { w: 720, h: 1280, fps: 30, gen: 1 });
  assert.deepStrictEqual(rows[0].video, [720, 1280]);
  assert.strictEqual(rows[1].camera.w, 1080);
  assert.strictEqual(rows[1].camera.gen, 2);
  for (const r of rows) assert.strictEqual(r.camera.deviceId, undefined);
  assert.deepStrictEqual(rows[1].camera.asked, [1920, 1080]);
  fs.rmSync(dir, { recursive: true, force: true });
  console.log('scantelemetry: ok');
})().catch((e) => { console.error(e); process.exit(1); });
