// Runnable smoke test for the Moxfield payload helpers. No framework — plain
// node + assert. Run: `node test/mfxpayload.test.js`. No db, no network: the
// helpers are pure, so a fixture deck stands in for a live /v3/decks/all.
const assert = require('assert');
const {
  MIRROR_BOARDS,
  extractDeckCards,
  extractMaybeCards,
  boardCounts,
  scryboxCardId,
  mfxFormatLabel,
  targetSizeForFormat,
  synthesizeMoxfieldCard
} = require('../src/utils/mfxPayload');

// A miniature of the real /v3/decks/all shape (verified live 2026-08):
// boards are objects of { count, cards: { key: { quantity, card } } }.
const FIXTURE = {
  name: 'Test Commander',
  boards: {
    mainboard: {
      count: 4,
      cards: {
        k9vw1: { quantity: 2, card: { scryfall_id: 'uuid-fling', name: 'Fling' } },
        E0qOe: { quantity: 1, card: { scryfall_id: 'uuid-fury', name: 'Unleash Fury' } },
        ghost: { quantity: 1, card: { id: 'tok1' } },          // token: no scryfall_id → skipped
        zero: { quantity: 0, card: { scryfall_id: 'uuid-zero', name: 'Zero Card' } } // qty 0 → skipped
      }
    },
    commanders: {
      count: 1,
      cards: {
        cmd1: { quantity: 1, card: { scryfall_id: 'uuid-cmdr', name: 'The Commander' } }
      }
    },
    sideboard: {
      count: 2,
      cards: {
        sb1: { quantity: 2, card: { scryfall_id: 'uuid-sb', name: 'Sideboard Spell' } }
      }
    },
    maybeboard: {
      count: 3,
      cards: {
        mb1: { quantity: 3, card: { scryfall_id: 'uuid-mb', name: 'Maybe Card' } }
      }
    },
    stickers: {
      count: 9,
      cards: { st1: { quantity: 9, card: { scryfall_id: 'uuid-st', name: 'Sticker' } } }
    }
  }
};

function testExtract() {
  const entries = extractDeckCards(FIXTURE);
  // mainboard: 2 valid (token + zero-qty dropped); commanders: 1; sideboard: 1.
  // maybeboard and stickers are NOT mirrored.
  assert.strictEqual(entries.length, 4, 'mirrors only mainboard/commanders/sideboard entries');

  const byBoard = {};
  for (const e of entries) byBoard[e.board] = (byBoard[e.board] || 0) + 1;
  assert.strictEqual(byBoard.mainboard, 2, 'two mainboard cards survive filtering');
  assert.strictEqual(byBoard.commanders, 1, 'commander mirrors into the commanders board');
  assert.strictEqual(byBoard.sideboard, 1, 'sideboard card mirrored');
  assert.strictEqual(byBoard.maybeboard, undefined, 'maybeboard not mirrored');
  assert.strictEqual(byBoard.stickers, undefined, 'stickers not mirrored');

  const fling = entries.find(e => e.card.name === 'Fling');
  assert.strictEqual(fling.quantity, 2, 'quantity preserved');
  assert.ok(!entries.some(e => e.card.name === 'Zero Card'), 'zero-quantity card dropped');
  assert.ok(!entries.some(e => !e.card.scryfall_id), 'cards without a scryfall_id dropped');
}

function testBoardCounts() {
  const counts = boardCounts(extractDeckCards(FIXTURE));
  assert.strictEqual(counts.mainboard, 3, 'mainboard total is summed quantity (2+1)');
  assert.strictEqual(counts.commanders, 1, 'commander total');
  assert.strictEqual(counts.sideboard, 2, 'sideboard total');
  assert.strictEqual(counts.maybeboard, 0, 'unmirrored boards stay zero');
}

function testScryboxCardId() {
  // The mapping that makes the whole feature work: Moxfield scryfall_id →
  // card_cache id. Verified against the running collection (mtg-<uuid>).
  assert.strictEqual(scryboxCardId({ scryfall_id: 'cb28fe03-8269-41de-b766-42c3421aeaef' }),
    'mtg-cb28fe03-8269-41de-b766-42c3421aeaef');
}

function testFormatLabel() {
  assert.strictEqual(mfxFormatLabel('commander'), 'Commander / EDH');
  assert.strictEqual(mfxFormatLabel('oathbreaker'), 'Commander / EDH');
  assert.strictEqual(mfxFormatLabel('standard'), 'Standard');
  assert.strictEqual(mfxFormatLabel('modern'), 'Modern');
  assert.strictEqual(mfxFormatLabel('PIONEER'), 'Pioneer');
  assert.strictEqual(mfxFormatLabel(null), 'Commander / EDH');
  assert.strictEqual(mfxFormatLabel('some-weird-custom'), 'some-weird-custom');
}

function testTargetSize() {
  assert.strictEqual(targetSizeForFormat('commander'), 100);
  assert.strictEqual(targetSizeForFormat('standard'), 75);
  assert.strictEqual(targetSizeForFormat('modern'), 75);
  assert.strictEqual(targetSizeForFormat(null), 100);
  // Only the constructed-with-sideboard formats get 75.
  assert.strictEqual(targetSizeForFormat('pauper'), 100);
}

function testMirrorBoards() {
  assert.deepStrictEqual(MIRROR_BOARDS, ['mainboard', 'commanders', 'sideboard']);
}

function testSynthesizeMoxfieldCard() {
  // The shape has to match scryfallApi.normalizeCard's output closely enough to
  // be written straight through cacheNormalizedCards, and to be flagged as a
  // non-Scryfall card via price_source.
  const card = synthesizeMoxfieldCard('mtg-7e56f614-cba4-49d9-b5fc-7566abba4a10', {
    name: 'Sakashima the Impostor',
    set: 'mb1',
    set_name: 'Mystery Booster',
    cn: '477',
    cmc: 4.0,
    type_line: 'Legendary Creature — Shapeshifter',
    colors: ['U', 'B']
  });
  assert.strictEqual(card.id, 'mtg-7e56f614-cba4-49d9-b5fc-7566abba4a10');
  assert.strictEqual(card.name, 'Sakashima the Impostor');
  assert.strictEqual(card.supertype, 'MTG');
  assert.deepStrictEqual(card.types, ['Blue', 'Black'], 'color letters become color words');
  assert.deepStrictEqual(card.color_identity, ['Blue', 'Black']);
  assert.deepStrictEqual(card.subtypes, ['Legendary', 'Creature', 'Shapeshifter'], 'type line split into subtypes');
  assert.strictEqual(card.set_id, 'mb1');
  assert.strictEqual(card.set_name, 'Mystery Booster');
  assert.strictEqual(card.number, '477');
  assert.strictEqual(card.cmc, 4);
  assert.strictEqual(card.price_source, 'moxfield', 'flagged so Scryfall-keyed features know it is not a real card');
  assert.strictEqual(card.price_trend, 0);
  assert.strictEqual(card.image_url, '');

  // Missing card data degrades gracefully instead of throwing.
  const bare = synthesizeMoxfieldCard('mtg-x', {});
  assert.strictEqual(bare.name, 'Unknown');
  assert.deepStrictEqual(bare.types, []);
  assert.strictEqual(bare.cmc, null);
}

function testExtractMaybe() {
  const details = { boards: {
    mainboard: { cards: { a: { quantity: 1, card: { scryfall_id: 'uuid-main', name: 'Main' } } } },
    maybeboard: { count: 3, cards: {
      m1: { quantity: 2, card: { scryfall_id: 'uuid-maybe', name: 'Maybe' } },
      m2: { quantity: 1, card: { id: 'tok' } },
      m3: { quantity: 0, card: { scryfall_id: 'uuid-z', name: 'Z' } },
      m4: { quantity: 1, card: { scryfall_id: 'uuid-maybe2', name: 'Maybe 2' } }
    } }
  } };
  const maybe = extractMaybeCards(details);
  assert.deepStrictEqual(maybe.map(e => [e.board, e.card.scryfall_id, e.quantity]),
    [['maybeboard', 'uuid-maybe', 2], ['maybeboard', 'uuid-maybe2', 1]]);
  // Maybeboard never leaks into the mirrored deck boards.
  assert.ok(!extractDeckCards(details).some(e => e.board === 'maybeboard'));
  assert.deepStrictEqual(extractMaybeCards({}), []);
  assert.deepStrictEqual(extractMaybeCards(null), []);
}

function main() {
  testExtractMaybe();
  testExtract();
  testBoardCounts();
  testScryboxCardId();
  testFormatLabel();
  testTargetSize();
  testMirrorBoards();
  testSynthesizeMoxfieldCard();
  console.log('mfxpayload.test.js passed');
}

try { main(); } catch (err) { console.error(err); process.exit(1); }
