import assert from 'node:assert/strict';
import { moveItem, nearestSlot } from './useHoldReorder.js';

assert.deepEqual(moveItem(['a', 'b', 'c', 'd'], 0, 2), ['b', 'c', 'a', 'd']);
assert.deepEqual(moveItem(['a', 'b', 'c', 'd'], 3, 0), ['d', 'a', 'b', 'c']);
const same = ['a', 'b'];
assert.equal(moveItem(same, 1, 1), same);
const col = [0, 1, 2].map(i => ({ left: 0, top: i * 100, width: 300, height: 90 }));
assert.equal(nearestSlot(col, 150, 10), 0);
assert.equal(nearestSlot(col, 150, 240), 2);
const grid = [{ left: 0, top: 0, width: 100, height: 100 }, { left: 120, top: 0, width: 100, height: 100 }];
assert.equal(nearestSlot(grid, 200, 50), 1);
console.log('# useHoldReorder.test.js: all assertions passed');
