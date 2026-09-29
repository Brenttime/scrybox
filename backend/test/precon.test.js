// Preconstructed-deck import: ranking the index and the card-resolution core.
// No network — getPreconCardList's HTTP and bulkFetchByIdentifier are stubbed,
// and the DB writes are recorded, so the assertions are about what WOULD be
// written, not about a live database.
const assert = require('assert');
const os = require('os');
const path = require('path');
// preconData pulls in the db module; park it in a throwaway file so the test
// can never touch a real (dev) database.
process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-precon-${process.pid}.db`);
const { rankPrecons, importPreconCardsIntoDeck } = require('../src/utils/preconData');

const DECKS = [
  { name: 'Lorehold Legacies', code: 'C21', type: 'Commander Deck', releaseDate: '2021-04-23', fileName: 'LoreholdLegacies_C21' },
  { name: 'Silverquill Statement', code: 'C21', type: 'Commander Deck', releaseDate: '2021-04-23', fileName: 'SilverquillStatement_C21' },
  { name: 'Ruthless Regiment', code: 'C20', type: 'Commander Deck', releaseDate: '2020-07-03', fileName: 'RuthlessRegiment_C20' },
  { name: 'Black Deck', code: 'HOB', type: 'Welcome Deck', releaseDate: '2026-08-14', fileName: 'BlackDeck_HOB' },
  { name: 'Angelic Fury', code: 'SOI', type: 'Intro Pack', releaseDate: '2017-09-09', fileName: 'AngelicFury_SOI' },
  { name: 'Sligh', code: 'WC98', type: 'World Championship Deck', releaseDate: '1998-08-12', fileName: 'Sligh_WC98' },
];

(async () => {
  // --- rankPrecons ------------------------------------------------------
  let hits = rankPrecons(DECKS, 'lorehold').map((d) => d.name);
  assert.strictEqual(hits[0], 'Lorehold Legacies', 'exact name match ranks first');

  // Substring beats subsequence: "ruth" matches Ruthless Regiment, and it must
  // outrank a subsequence-only match for the same needle.
  hits = rankPrecons(DECKS, 'ruth').map((d) => d.name);
  assert.strictEqual(hits[0], 'Ruthless Regiment');

  // Subsequence: "ueg" is scattered across Ruthless Regiment (u, then e, then g)
  // but is not a substring, so only the subsequence branch should match it.
  hits = rankPrecons(DECKS, 'ueg').map((d) => d.name);
  assert.strictEqual(hits[0], 'Ruthless Regiment');

  // Set code in the search: "c21" should pull both C21 decks, name-ordered.
  hits = rankPrecons(DECKS, 'c21').map((d) => d.name);
  assert.deepStrictEqual(hits, ['Lorehold Legacies', 'Silverquill Statement']);

  // Set name in the search: a deck whose name never mentions its set is found
  // by the set it was made for.
  const withSets = [...DECKS, { name: 'Lorehold Spirit', code: 'SOC', setName: 'Secrets of Strixhaven Commander', type: 'Commander Deck', releaseDate: '2026-04-24', fileName: 'LoreholdSpirit_SOC' }];
  hits = rankPrecons(withSets, 'strixhaven').map((d) => d.name);
  assert.deepStrictEqual(hits, ['Lorehold Spirit']);
  assert.deepStrictEqual(require('../src/utils/preconData').setNameMap({ data: [{ code: 'soc', name: 'X' }] }), { SOC: 'X' });

  // Only real paper precons are searchable.
  const { isPrecon } = require('../src/routes/precons');
  assert.ok(isPrecon({ type: 'Commander Deck' }));
  for (const type of ['Secret Lair Drop', 'Jumpstart', 'MTGO Redemption', 'Shandalar Enemy Deck', 'Bundle Land Pack', 'World Championship Deck']) {
    assert.ok(!isPrecon({ type }), type);
  }

  // No match is an empty list, not an error.
  assert.deepStrictEqual(rankPrecons(DECKS, 'zzzqq'), []);

  // Case-insensitive.
  assert.strictEqual(rankPrecons(DECKS, 'LOREHOLD')[0].name, 'Lorehold Legacies');

  // --- importPreconCardsIntoDeck ----------------------------------------
  const run = (sql, params) => {
    runs.push({ sql, params });
    return { lastID: 99 };
  };
  const runs = [];
  // Resolver: set+number → authoritative card, like bulkFetchByIdentifier does
  // (pairs links each requested row to its resolved card; notFound counts the
  // rows the provider had no answer for).
  const cardById = async (rows) => {
    const pairs = [];
    const cards = [];
    let notFound = 0;
    for (const r of rows) {
      if (r.set_id === 'NOPE') { notFound++; continue; }
      const logicalName = r.number === '164' || r.number === '999a' ? 'Shared Reprint' : `${r.set_id}-${r.number}`;
      const card = {
        id: `mtg-${r.set_id}-${r.number}`,
        name: logicalName,
        set_id: r.set_id,
        number: String(r.number)
      };
      cards.push(card);
      pairs.push({ row: r, card });
    }
    return { cards, pairs, notFound };
  };

  // Basic: two land types + a spell, duplicate set+number rows merge.
  runs.length = 0;
  let out = await importPreconCardsIntoDeck({
    name: 'Test Precon',
    description: 'Preconstructed: Test (C21)',
    format: 'Standard',
    category: 'Casual',
    accentColor: '#eab308',
    targetSize: 60,
    userId: 7,
    rows: [
      { set_id: 'C21', number: '164', quantity: 1 },
      { set_id: 'C21', number: '367', quantity: 4 },
      { set_id: 'STX', number: '373', quantity: 6 },
      { set_id: 'STX', number: '373', quantity: 2 }, // duplicate printing
      { set_id: 'NOPE', number: '999', quantity: 1 }, // unresolvable
    ],
    cardById,
    cacheCards: async () => {},
    decksRun: run,
    cardsRun: run,
  });
  assert.strictEqual(out.cards, 3, 'resolvable printings land in the deck');
  assert.strictEqual(out.notFound, 1, 'unresolvable printing is reported, not faked');
  assert.strictEqual(runs.length, 4, 'one deck insert + one row per card type');
  assert.strictEqual(runs[0].params[0], 'Test Precon');
  assert.strictEqual(runs[0].params[7], 'precon', 'deck source is stamped precon');
  assert.deepStrictEqual(runs[3].params, [99, 'mtg-STX-373', 8, 0], 'duplicate rows merged to 8');

  // Alternate printings with one canonical name collapse to one logical deck row.
  runs.length = 0;
  out = await importPreconCardsIntoDeck({
    name: 'Reprint Precon', description: '', format: 'Standard', category: 'Casual',
    accentColor: '#eab308', targetSize: 60, userId: 7,
    rows: [
      { set_id: 'C21', number: '164', quantity: 1 },
      { set_id: 'ALT', number: '999a', quantity: 2 },
    ],
    cardById, cacheCards: async () => {}, decksRun: run, cardsRun: run,
  });
  assert.strictEqual(out.cards, 1, 'reprints become one logical card type');
  assert.strictEqual(runs.length, 2, 'one deck insert + one logical card insert');
  assert.deepStrictEqual(runs[1].params, [99, 'mtg-C21-164', 3, 0]);

  // Everything unresolvable → 422, no deck written.
  runs.length = 0;
  try {
    await importPreconCardsIntoDeck({
      name: 'Bad Precon', description: '', format: 'Standard', category: 'Casual',
      accentColor: '#eab308', targetSize: 60, userId: 7,
      rows: [{ set_id: 'NOPE', number: '1', quantity: 1 }],
      cardById, cacheCards: async () => {}, decksRun: run, cardsRun: run,
    });
    assert.fail('should have thrown');
  } catch (err) {
    assert.strictEqual(err.status, 422);
    assert.strictEqual(runs.length, 0, 'no deck is written when nothing resolves');
  }

  // Commander shape: a 100-card deck with the commander counted as 1.
  runs.length = 0;
  out = await importPreconCardsIntoDeck({
    name: 'Lorehold Legacies', description: 'Preconstructed: Lorehold Legacies (C21)',
    format: 'Commander / EDH', category: 'Competitive', accentColor: '#8b5cf6', targetSize: 100,
    userId: 7,
    rows: [{ set_id: 'C21', number: '8', quantity: 1 }, { set_id: 'C21', number: '164', quantity: 1 }],
    cardById, cacheCards: async () => {}, decksRun: run, cardsRun: run,
  });
  assert.strictEqual(out.cards, 2);

  console.log('PASS: precon ranking + import core');
  process.exit(0);
})().catch((err) => {
  console.error('FAIL: precon import —', err.message);
  process.exit(1);
});
