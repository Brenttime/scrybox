import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { startMarketRelay, CONNECT_TARGETS } from './marketRelay.js';

// The "Connect" button is a visibility launcher: it opens the provider page
// that shows the credential, in the SAME tick as the click (a deferred
// window.open loses the user activation and gets blocked), and reports the
// outcome honestly. It must never do anything with a credential.

const calls = [];
const allowed = (url) => { calls.push(url); return { location: {} }; };

async function main() {
  // Synchronous open, correct target per source.
  const mp = startMarketRelay('manapool', allowed);
  assert.strictEqual(calls.length, 1, 'open fired synchronously, before any await');
  assert.strictEqual(calls[0], CONNECT_TARGETS.manapool);
  assert.deepStrictEqual(await mp, { opened: true });

  // TCGplayer order import was removed; its Connect target went with it.
  await assert.rejects(startMarketRelay('tcgplayer', allowed), /UNKNOWN_SOURCE/);

  // A popup object without a usable location (blocked/cross-origin edge) is a
  // failure, not a silent success.
  await assert.rejects(startMarketRelay('manapool', () => null), /POPUP_BLOCKED/);
  await assert.rejects(startMarketRelay('manapool', () => ({})), /POPUP_BLOCKED/);
  await assert.rejects(startMarketRelay('seattransfer', allowed), /UNKNOWN_SOURCE/);

  // The launcher's own CODE must carry no injection vectors and no
  // credential-capture vocabulary — the old relay's whole surface. Comments
  // discuss those mechanisms to explain why they are absent, so prose is
  // stripped before the scan; the line filter is crude but this file has
  // no string literals containing those tokens.
  const raw = readFileSync(new URL('./marketRelay.js', import.meta.url), 'utf8');
  const code = raw.split('\n').filter((l) => !l.trim().startsWith('//')).join('\n');
  assert.ok(!/javascript\s*:/.test(code), 'no javascript: URLs');
  assert.ok(!/localStorage|document\.cookie|XMLHttpRequest|fetch\(/.test(code),
    'module touches no credential channel');

  console.log('marketRelay: all assertions passed');
}

main().catch((err) => { console.error(err); process.exit(1); });