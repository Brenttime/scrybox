import test from 'node:test';
import assert from 'node:assert/strict';
import { scryfallDragData, setScryfallDrag } from './scryfallDrag.js';

const sol = {
  id: 'mtg-772edef7-2f17-4244-b1bb-4e7a53c253e1', name: 'Sol Ring', set_id: 'brc', number: '160',
  image_url: 'https://cards.scryfall.io/normal/front/7/7/772edef7-2f17-4244-b1bb-4e7a53c253e1.jpg?1783919658',
};

test('drag data looks like a Scryfall search result', () => {
  const d = scryfallDragData(sol);
  assert.equal(d.page, 'https://scryfall.com/card/brc/160/sol-ring');
  assert.equal(d.image, sol.image_url);
  assert.match(d.html, /^<a href="https:\/\/scryfall\.com\/card\/brc\/160\/sol-ring"><img src="https:\/\/cards\.scryfall\.io\//);
});

test('collection rows use card_id; non-Scryfall art falls back to the CDN image', () => {
  const d = scryfallDragData({ id: 42, card_id: sol.id, name: 'Sol Ring', set_id: 'brc', number: '160', image_url: '/api/card-art/x' });
  assert.equal(d.image, 'https://cards.scryfall.io/normal/front/7/7/772edef7-2f17-4244-b1bb-4e7a53c253e1.jpg');
});

test('no Scryfall id means no drag payload', () => {
  assert.equal(scryfallDragData({ id: 'custom-1', name: 'X' }), null);
  const set = {};
  assert.equal(setScryfallDrag({ dataTransfer: { setData: (k, v) => { set[k] = v; } } }, sol), true);
  assert.equal(set['text/uri-list'], 'https://scryfall.com/card/brc/160/sol-ring');
});
