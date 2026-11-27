// The deck list page is a browsing surface, not an operations console (Brent,
// 2026-09-17). Three decisions were made deliberately and are cheap to undo by
// accident, so they are pinned here:
//
//   1. no Actions column — rows/cards are clicked to open, never operated on;
//   2. no Format column at all — header and data alike. The table shows a deck's
//      format nowhere by design; the Grid view's cards keep their format badge,
//      which is why that assertion is scoped to the table region.
//   3. a precon never shows a play-style category tag, and the import that
//      creates one never stamps a category in the first place.
//
// Deck checkout / return / delete therefore live only inside the deck editor.
// This test is the guard that keeps them from being dropped on the floor while
// the selection view is trimmed.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const srcDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const read = (...parts) => readFileSync(join(srcDir, ...parts), 'utf8');
const deckBuilder = read('components', 'DeckBuilder.jsx');
// The precon import lives in the Rust server since the Node backend removal
// (was backend/src/routes/precons.js).
const preconsRoute = read('..', '..', 'server', 'crates', 'scrybox-server', 'src', 'precons.rs');

const section = (start, end) => {
  const startIndex = deckBuilder.indexOf(start);
  const endIndex = deckBuilder.indexOf(end, startIndex + start.length);
  assert.notEqual(startIndex, -1, `missing section marker: ${start}`);
  assert.notEqual(endIndex, -1, `missing section marker: ${end}`);
  return deckBuilder.slice(startIndex, endIndex);
};

// --- 1 + 2: the selection view offers no row actions and no column labels -----
const selection = section(
  '{/* 1. SELECTION MENU VIEW OF ALL DECKS */}',
  '{/* 2. DECK EDITOR / DETAIL VIEW */}'
);

assert.doesNotMatch(selection, /handleDeleteDeck\s*\(/,
  'the deck list must not offer deck deletion — it belongs to the deck editor');
assert.doesNotMatch(selection, /handle(?:Checkout|Return)\(\s*deck\s*[,)]/,
  'checkout/return must not be offered per row/card — the editor owns those actions');
assert.doesNotMatch(selection, /t\('admin\.colActions'\)/,
  'the Actions column header must stay off the deck list');
// --- 2: the table has no Format column, header or body -----------------------
// Scoped to the table view on purpose: the grid cards are not a column and still
// carry their format badge, so asserting this over the whole selection view would
// either pass vacuously or fight the grid design.
const tableView = section(
  '/* --- TABLE VIEW --- */',
  '{/* 2. DECK EDITOR / DETAIL VIEW */}'
);
assert.doesNotMatch(tableView, /deck\.format\b/,
  'the deck table must have no Format column — neither the header label nor the cell content');
assert.doesNotMatch(tableView, /t\('deck\.format'\)/,
  'the Format header label must stay off the deck table');
// Two-sided: the row must not lose its accent swatch while losing the format cell.
assert.equal((tableView.match(/backgroundColor:\s*accentColor/g) || []).length, 1,
  'the table row must keep exactly one accent swatch (now beside the deck name)');
// And the removal stops at the table: the grid cards are a different surface and
// still badge their format, so a sweep that deletes those too is a regression.
const cardGrid = section(
  '/* --- GRID VIEW --- */',
  '/* --- TABLE VIEW --- */'
);
assert.match(cardGrid, /\{deck\.format &&\s*\(/,
  'the grid cards must keep their format badge — only the table lost the Format column');
assert.doesNotMatch(selection, /<th[^>]*textAlign:\s*'right'/,
  'no right-aligned action header may reappear in the deck list table');

// The editor must still carry what it took over from the list. Scoped to the
// editor header region so these cannot be satisfied by the selection view.
const deckEditor = section(
  '{/* 2. DECK EDITOR / DETAIL VIEW */}',
  '{/* Checked out info banner */}',
  'deck editor header'
);
assert.match(deckEditor, /handleDeleteDeck\(\s*activeDeck\.id/,
  'the deck editor must expose deck delete now that the list rows do not');
assert.match(deckEditor, /handleReturn\(\s*activeDeck\s*\)/,
  'the deck editor must keep the Return action');
assert.match(deckEditor, /handleCheckout\(\s*activeDeck\s*\)/,
  'the deck editor must keep the Checkout action');

// --- 3: precons carry no category tag ---------------------------------------
// Every rendered category badge is gated on the deck not being a precon, so a
// row imported before the import stopped stamping categories still reads clean.
const badgeSites = deckBuilder.match(/\{deck\.category &&[^)]*\)/g) || [];
assert.ok(badgeSites.length >= 2,
  'both category badges (list and grid cards) must stay rendered from deck.category');
for (const site of badgeSites) {
  assert.match(site, /deck\.source\s*!==\s*'precon'/,
    `a category badge must stay gated on the deck not being a precon: ${site}`);
}

// The create path itself must not invent a play-style category for a printed
// product. The DB column default is the last-line fallback, so a null passed
// through must not fall back to a string either — assert the shape, not the SQL.
assert.match(preconsRoute, /fn default_shape\(/,
  'precon import must derive its deck shape from the product type');
assert.match(preconsRoute, /COMMANDER_TYPES\.contains\(&type_\)/,
  'precon import must derive its deck shape from the product type (COMMANDER_TYPES branch)');
assert.doesNotMatch(preconsRoute, /"(?:Competitive|Casual|Tournament|Theorycraft|Proxy|Trade)"/,
  'precon import must not stamp a play-style category — a printed deck is not a build style');
assert.match(preconsRoute, /let category: Option<String> = if truthy\(&category_v\)/,
  'precon import must default the category to None (SQL NULL), not a guess');

console.log('PASS: deckListPresentation.test.js');
