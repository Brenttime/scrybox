import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cardPriceText, estPrefix } from './formatPrice.js';

test('estimated card prices read with a leading ~', () => {
  assert.equal(cardPriceText({ price_trend: 0.4, price_estimated: 1 }), '~$0.40');
  assert.equal(cardPriceText({ price_trend: 0.4, price_estimated: 0 }), '$0.40');
  assert.equal(cardPriceText({ price_trend: 2, price_estimated: 1 }, 6), '~$6.00');
});

test('no ~ on a zero amount (proxies, unpriced)', () => {
  assert.equal(cardPriceText({ price_trend: 0, price_estimated: 1 }), '$0.00');
  assert.equal(estPrefix(true, 0), '');
  assert.equal(estPrefix(true, 12.5), '~');
  assert.equal(estPrefix(false, 12.5), '');
});
