import assert from 'node:assert';
import { parseDirectAdd, directAddQuery, pickPrinting, sortByRelease } from './directAdd.js';

const sets = new Set(['ltr', 'cmm', 'c21']);

const a = parseDirectAdd('ltr 246', sets);
assert.deepStrictEqual(a, { kind: 'number', set: 'ltr', number: '246', qty: 1, printing: null, condition: null, language: null });
assert.strictEqual(directAddQuery(a), 'set:ltr cn:246');
assert.strictEqual(parseDirectAdd('ltr 246', new Set())?.kind, 'number', 'set+number parses without a set list');

const b = parseDirectAdd('2 sol ring foil', sets);
assert.strictEqual(b.kind, 'name');
assert.strictEqual(b.name, 'sol ring');
assert.strictEqual(b.qty, 2);
assert.strictEqual(b.printing, 'Holofoil');
assert.strictEqual(b.set, null);

const c = parseDirectAdd('sol ring cmm', sets);
assert.deepStrictEqual([c.kind, c.name, c.set, c.qty], ['name', 'sol ring', 'cmm', 1]);
assert.strictEqual(directAddQuery(c), '!"sol ring" set:cmm unique:prints -is:digital');

const d = parseDirectAdd('3 ltr 246 lp jp', sets);
assert.deepStrictEqual([d.kind, d.qty, d.condition, d.language], ['number', 3, 'Lightly Played', 'Japanese']);

// Not direct adds: syntax and plain names search.
for (const s of ['t:creature', 't:dragon c:r is:commander', 'set:ltr', 'cmc<=2', '-is:digital', 'sol ring', 'sol r', 'Lightning Bolt', '']) {
  assert.strictEqual(parseDirectAdd(s, sets), null, `${s} is a search`);
}
// "ring" is not a known set, so it stays part of the name.
assert.strictEqual(parseDirectAdd('one ring', sets), null);

assert.strictEqual(pickPrinting({ finishes: ['foil'] }, 'Normal'), 'Holofoil');
assert.strictEqual(pickPrinting({ finishes: ['etched'] }, 'Normal'), 'Holofoil');
assert.strictEqual(pickPrinting({ finishes: ['nonfoil'] }, 'Holofoil'), 'Normal');
assert.strictEqual(pickPrinting({ finishes: ['nonfoil', 'foil'] }, 'Holofoil'), 'Holofoil');
assert.strictEqual(pickPrinting({}, 'Normal'), 'Normal');

assert.deepStrictEqual(
  sortByRelease([{ id: 1, released_at: '1993-08-05' }, { id: 2, released_at: '2025-01-01' }, { id: 3 }]).map(x => x.id),
  [2, 1, 3]
);
