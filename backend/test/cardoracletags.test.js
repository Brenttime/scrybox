const test = require('node:test');
const assert = require('node:assert');
const { cardOracleTags } = require('../src/utils/cardOracleTags');

// Minimal fake: answers the three queries the helper makes.
function fakeDb({ gen = { id: 1 }, oracleId = 'o1', tags = [], edges = [] }) {
  return {
    get: async (sql) => (/oracle_tag_generations/.test(sql) ? gen : { oracle_id: oracleId }),
    all: async (sql) => (/oracle_tag_closure/.test(sql) ? edges : tags),
  };
}

test('inherited = ancestors of direct tags, minus direct ones, with via', async () => {
  const r = await cardOracleTags('c', null, fakeDb({
    tags: [{ tag_id: 'a', slug: 'mana-rock' }, { tag_id: 'b', slug: 'ramp' }],
    edges: [
      { tag_id: 'b', slug: 'ramp', via: 'mana-rock' },
      { tag_id: 'p', slug: 'mana-producer', via: 'mana-rock' },
      { tag_id: 'p', slug: 'mana-producer', via: 'ramp' },
    ],
  }));
  assert.deepStrictEqual(r.tags.map(t => t.slug), ['mana-rock', 'ramp']);
  assert.deepStrictEqual(r.inherited.map(t => [t.slug, t.via]), [['mana-producer', ['mana-rock', 'ramp']]]);
});

test('no active tag generation reports not ready', async () => {
  const r = await cardOracleTags('c', null, fakeDb({ gen: null }));
  assert.strictEqual(r.ready, false);
});
