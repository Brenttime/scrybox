import test from 'node:test';
import assert from 'node:assert/strict';
import { evalWidthQuery, sideBySide } from './dualScreen.js';

test('width queries evaluate against the left segment', () => {
  assert.equal(evalWidthQuery('(width <= 768px)', 540), true);
  assert.equal(evalWidthQuery('(max-width: 768px)', 540), true);
  assert.equal(evalWidthQuery('(width >= 769px)', 540), false);
  assert.equal(evalWidthQuery('(min-width: 1024px)', 540), false);
  assert.equal(evalWidthQuery('(width <= 520px)', 540), false);
  assert.equal(evalWidthQuery('(hover: hover)', 540), null);
  assert.equal(evalWidthQuery('(width <= 768px) and (hover: none)', 540), null);
});

test('only two side-by-side segments count as spanned', () => {
  const L = { left: 0, top: 0, width: 540, right: 540 };
  const R = { left: 574, top: 0, width: 540, right: 1114 };
  assert.deepEqual(sideBySide([R, L]), { left: 540, hinge: 34 });
  assert.equal(sideBySide([L]), null);
  assert.equal(sideBySide(null), null);
  assert.equal(sideBySide([{ left: 0, top: 0, width: 720, right: 720 }, { left: 0, top: 574, width: 720, right: 720 }]), null);
});
