const test = require('node:test');
const assert = require('node:assert');
const { _codec } = require('../src/routes/searchHistory');

test('history round-trips through the compressed codec', () => {
  const list = [['otag:ramp', 1793577126], ['t:goblin', 1793577000]];
  const blob = _codec.encode(list);
  assert.ok(Buffer.isBuffer(blob));
  assert.deepStrictEqual(_codec.decode(blob), list);
});

test('compressed form is smaller than raw JSON for a full history', () => {
  const list = Array.from({ length: 100 }, (_, i) => [`t:creature c:r cmc<=${i % 7} otag:removal`, 1793500000 + i]);
  assert.ok(_codec.encode(list).length < Buffer.byteLength(JSON.stringify(list)) / 3);
});

test('corrupt or empty blobs decode to an empty list', () => {
  assert.deepStrictEqual(_codec.decode(null), []);
  assert.deepStrictEqual(_codec.decode(Buffer.from('not deflate')), []);
});
