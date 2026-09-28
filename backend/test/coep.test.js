const test = require('node:test');
const assert = require('node:assert/strict');
const { coepFor } = require('../src/coep');

const UA = {
  iosSafari: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Mobile/15E148 Safari/604.1',
  iosChrome: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/140.0 Mobile/15E148 Safari/604.1',
  ipadDesktop: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15',
  winChrome: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36',
  android: 'Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Mobile Safari/537.36',
  edge: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36 Edg/140.0',
  firefox: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0',
  fxios: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/140.0 Mobile/15E148 Safari/605.1.15',
  edgios: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 EdgiOS/140.0 Mobile/15E148 Safari/605.1.15',
  wkwebview: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148',
  ipod: 'Mozilla/5.0 (iPod touch; CPU iPhone OS 15_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0 Mobile/15E148 Safari/604.1',
  macSafari: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15',
  criosDesktop: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_13_5) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/85 Version/11.1.1 Safari/605.1.15',
  macChrome: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36',
  noWebKit: 'curl/8.5.0',
};

test('WebKit gets require-corp; Blink/Gecko keep credentialless', () => {
  for (const k of ['iosSafari', 'iosChrome', 'ipadDesktop', 'fxios', 'edgios', 'wkwebview', 'ipod', 'macSafari', 'criosDesktop']) assert.equal(coepFor(UA[k], {}), 'require-corp', k);
  for (const k of ['winChrome', 'android', 'edge', 'firefox', 'macChrome', 'noWebKit']) assert.equal(coepFor(UA[k], {}), 'credentialless', k);
  assert.equal(coepFor('', {}), 'credentialless');
});

test('switches: SCAN_ISOLATION=0 off everywhere, SCAN_ISOLATION_WEBKIT=0 off for WebKit only', () => {
  assert.equal(coepFor(UA.winChrome, { SCAN_ISOLATION: '0' }), null);
  assert.equal(coepFor(UA.iosSafari, { SCAN_ISOLATION: '0' }), null);
  assert.equal(coepFor(UA.iosSafari, { SCAN_ISOLATION_WEBKIT: '0' }), null);
  assert.equal(coepFor(UA.winChrome, { SCAN_ISOLATION_WEBKIT: '0' }), 'credentialless');
});
