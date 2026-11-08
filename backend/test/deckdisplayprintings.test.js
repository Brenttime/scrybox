const test = require('node:test');
const assert = require('node:assert');
const { choosePrintings } = require('../src/utils/deckDisplayPrintings');

test('owned copy replaces the deck list printing', () => {
  const picks = choosePrintings(
    [{ id: 'list-print', card_key: 'sol ring' }],
    [{ card_key: 'sol ring', card_id: 'my-print', qty: 1 }]
  );
  assert.strictEqual(picks.get('sol ring'), 'my-print');
});

test('deck list printing stays when the user owns that exact printing', () => {
  const picks = choosePrintings(
    [{ id: 'list-print', card_key: 'sol ring' }],
    [{ card_key: 'sol ring', card_id: 'other', qty: 5 }, { card_key: 'sol ring', card_id: 'list-print', qty: 1 }]
  );
  assert.strictEqual(picks.has('sol ring'), false);
});

test('unowned cards keep the deck list printing; most-owned printing wins', () => {
  const picks = choosePrintings(
    [{ id: 'a', card_key: 'x' }, { id: 'b', card_key: 'y' }],
    [{ card_key: 'y', card_id: 'y1', qty: 1 }, { card_key: 'y', card_id: 'y2', qty: 3 }]
  );
  assert.strictEqual(picks.has('x'), false);
  assert.strictEqual(picks.get('y'), 'y2');
});
