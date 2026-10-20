import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { deckSidebarStats, findCommander, cardColorLetters, landProduces } from './deckStats.js';

const srcDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const deckBuilder = readFileSync(join(srcDir, 'components', 'DeckBuilder.jsx'), 'utf8');
const en = JSON.parse(readFileSync(join(srcDir, 'locales', 'en.json'), 'utf8'));

// --- stats ---------------------------------------------------------------
const cards = [
  { id: 'cmd', name: 'Barrin', quantity: 1, cmc: 5, subtypes: ['Legendary', 'Creature'], types: ['Blue'], is_commander: 1 },
  { id: 'a', name: 'Opt', quantity: 1, cmc: 1, subtypes: ['Instant'], types: ['Blue'] },
  { id: 'b', name: 'Swords', quantity: 1, cmc: 1, subtypes: ['Instant'], types: ['White'] },
  { id: 'c', name: 'Big', quantity: 2, cmc: 9, subtypes: ['Creature'], types: [] },
  { id: 'i', name: 'Island', quantity: 10, cmc: 0, subtypes: ['Basic', 'Land', 'Island'], types: [] },
  { id: 'h', name: 'Hallowed Fountain', quantity: 1, cmc: 0, subtypes: ['Land', 'Plains', 'Island'], types: [], color_identity: ['White', 'Blue'] },
];
const s = deckSidebarStats(cards, 'cmd');
assert.deepEqual(s.curve, [0, 2, 0, 0, 0, 0, 0, 2], 'lands and the commander stay off the curve; 7+ bucket');
assert.equal(s.avgCmc, (1 + 1 + 18) / 4);
assert.deepEqual(s.colors.map(c => [c.color, c.spells, c.lands]), [['W', 1, 1], ['U', 2, 11]]);
// Utility lands count toward the colours they can actually make.
const sorted = (set) => [...set].sort();
const deckWU = ['W', 'U'];
assert.deepEqual(sorted(landProduces({ name: 'Command Tower', subtypes: ['Land'], oracle_text: "{T}: Add one mana of any color in your commander's color identity." }, deckWU)), ['U', 'W']);
assert.deepEqual(sorted(landProduces({ name: 'Unclaimed Territory', subtypes: ['Land'], oracle_text: '{T}: Add {C}.\n{T}: Add one mana of any color. Spend this mana only to cast a creature spell of the chosen type.' }, deckWU)), ['U', 'W']);
assert.deepEqual(sorted(landProduces({ name: 'Fabled Passage', subtypes: ['Land'], oracle_text: '{T}, Sacrifice this land: Search your library for a basic land card, put it onto the battlefield tapped, then shuffle.' }, deckWU)), ['U', 'W']);
assert.deepEqual(sorted(landProduces({ name: 'Flooded Strand', subtypes: ['Land'], oracle_text: '{T}, Pay 1 life, Sacrifice this land: Search your library for a Plains or Island card, put it onto the battlefield, then shuffle.' }, ['W', 'U', 'B'])), ['U', 'W']);
assert.deepEqual(sorted(landProduces({ name: 'Swarmyard', subtypes: ['Land'], oracle_text: '{T}: Add {C}.\n{T}: Regenerate target Insect.' }, deckWU)), []);
assert.deepEqual(sorted(landProduces({ name: 'Breeding Pool', subtypes: ['Land', 'Forest', 'Island'], oracle_text: '({T}: Add {G} or {U}.)' }, deckWU)), ['G', 'U']);
assert.deepEqual(sorted(landProduces({ name: 'Vault of the Archangel', subtypes: ['Land'], oracle_text: '{T}: Add {C}.\n{2}{W}{B}, {T}: Creatures you control gain deathtouch and lifelink until end of turn.' }, deckWU)), [], 'activation costs are not mana production');
assert.deepEqual(sorted(landProduces({ name: 'Snow-Covered Island', subtypes: ['Basic', 'Snow', 'Land', 'Island'] }, deckWU)), ['U']);

assert.deepEqual(cardColorLetters({ types: ['Blue', 'Red'] }), ['U', 'R']);

assert.equal(findCommander({ cards }).id, 'cmd', 'declared commander wins');
const undeclared = cards.map(c => ({ ...c, is_commander: 0 }));
assert.equal(findCommander({ cards: undeclared, format: 'Commander / EDH', commander_name: 'Barrin' }).id, 'cmd');
assert.equal(findCommander({ cards: undeclared, format: 'Modern', commander_name: 'Barrin' }), null);

// --- page structure --------------------------------------------------------
const editor = deckBuilder.slice(
  deckBuilder.indexOf('{/* 2. DECK EDITOR / DETAIL VIEW */}'),
  deckBuilder.indexOf('{/* --- POPUPS & MODALS --- */}'));
assert.doesNotMatch(editor, /PieChart|Supertype Breakdown/, 'supertype donut is gone');
assert.doesNotMatch(editor, /deck\.moxfieldTitle/, 'no separate Moxfield panel');
assert.doesNotMatch(editor, /\{supertype\}s/, 'no naive pluralisation ("Sorcerys")');
assert.doesNotMatch(editor, /x\{card\.quantity\}/, 'no x1 badges');
assert.match(editor, /card\.quantity > 1 &&/, 'quantity badge only above one');
assert.doesNotMatch(editor, /Trash2 size=\{1[01]\}/, 'no per-card trash buttons');
assert.equal((editor.match(/className="btn btn-primary"/g) || []).length, 3, 'three mutually exclusive primaries');
assert.match(deckBuilder, /localStorage\.setItem\('deckShowPrices'/, '$ toggle persists');
assert.match(deckBuilder, /localStorage\.getItem\('deckShowPrices'\) === '1'/, 'prices default off');
assert.equal(en['deck.group.Sorcery'], 'Sorceries');
for (const k of ['Creature', 'Planeswalker', 'Instant', 'Enchantment', 'Artifact', 'Battle', 'Land', 'Other']) {
  assert.ok(en[`deck.group.${k}`], `missing deck.group.${k}`);
}

console.log('PASS: deckPageRedesign.test.js');
