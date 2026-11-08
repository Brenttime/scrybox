import test from 'node:test';
import assert from 'node:assert/strict';
import { bottomsOwed, buildLibrary, deal, putOnBottom, isCommanderDeck } from './mulligan.js';

const noShuffle = a => [...a];

test('commander: first mulligan is free, then one card per mulligan', () => {
  assert.deepEqual([0, 1, 2, 3].map(m => bottomsOwed(m, true)), [0, 0, 1, 2]);
});

test('non-commander London mulligan: one card per mulligan', () => {
  assert.deepEqual([0, 1, 2, 3].map(m => bottomsOwed(m, false)), [0, 1, 2, 3]);
});

test('commander never shuffles into the library', () => {
  const cards = [{ id: 'cmd', quantity: 1 }, { id: 'a', quantity: 98 }, { id: 'b', quantity: 1 }];
  const { library, commandZone } = buildLibrary(cards, ['cmd']);
  assert.equal(library.length, 99);
  assert.ok(library.every(c => c.id !== 'cmd'));
  assert.deepEqual(commandZone.map(c => c.id), ['cmd']);
});

test('every deal is a full 7, and bottoming moves the card under the library', () => {
  const lib = Array.from({ length: 20 }, (_, i) => ({ id: String(i) }));
  const { hand, library } = deal(lib, noShuffle);
  assert.equal(hand.length, 7);
  assert.equal(library.length, 13);
  const r = putOnBottom(hand, library, 2);
  assert.equal(r.hand.length, 6);
  assert.equal(r.library.at(-1).id, '2');
});

test('commander decks are recognised by flag or format', () => {
  assert.equal(isCommanderDeck({ format: 'Commander / EDH' }, []), true);
  assert.equal(isCommanderDeck({ format: 'Modern' }, [{ id: 'x' }]), true);
  assert.equal(isCommanderDeck({ format: 'Modern' }, []), false);
});
