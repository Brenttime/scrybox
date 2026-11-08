import assert from 'node:assert/strict';
import { lockGesture, unlockGesture, gestureLocked } from './gestureLock.js';

assert.equal(gestureLocked(), false);
lockGesture('a');
assert.equal(gestureLocked(), true);
unlockGesture('b');
assert.equal(gestureLocked(), true, 'another owner cannot release the lock');
unlockGesture('a');
assert.equal(gestureLocked(), false);
console.log('# gestureLock.test.js: all assertions passed');
