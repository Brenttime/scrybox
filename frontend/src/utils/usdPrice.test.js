import test from 'node:test';
import assert from 'node:assert/strict';
import { usdPrice } from './resolveCardPrice.js';

test('scanner price: USD only, otherwise no price', () => {
  assert.equal(usdPrice({ price_currency: 'USD', price_normal: 1.5, price_trend: 1.5 }, 'Normal'), 1.5);
  assert.equal(usdPrice({ price_normal: 2, price_trend: 2 }, 'Normal'), 2, 'legacy rows without currency are USD');
  assert.equal(usdPrice({ price_currency: 'USD', price_holofoil: 4, price_trend: 1 }, 'Holofoil'), 4);
  assert.equal(usdPrice({ price_currency: 'EUR', price_normal: 3, price_trend: 3 }, 'Normal'), null, 'EUR never shown');
  assert.equal(usdPrice({ price_currency: 'USD', price_trend: 0 }, 'Normal'), null, 'no quote -> no price, not $0.00');
  assert.equal(usdPrice(null, 'Normal'), null);
});
