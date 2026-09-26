import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { getPrintingBadgeLabel, getPrintingLabel } from './cardPrinting.js';

const srcDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const localeDir = join(srcDir, 'locales');
const fixtureDir = join(srcDir, 'demo', 'fixtures');
const locales = readdirSync(localeDir).filter(file => file.endsWith('.json'));

const foilTerms = {
  de: 'Foil',
  en: 'Foil',
  es: 'foil',
  fr: 'foil',
  it: 'foil',
  ja: 'フォイル',
  ko: '포일',
  'pt-BR': 'foil',
  ru: 'фойл',
  'zh-Hans': '闪卡',
  'zh-Hant': '閃卡',
};
const staleGameTerms = {
  de: /Spiel/i,
  en: /\bgame\b/i,
  es: /juego/i,
  fr: /\bjeu\b/i,
  it: /gioco/i,
  ja: /ゲーム/,
  ko: /게임/,
  'pt-BR': /jogo/i,
  ru: /игр/i,
  'zh-Hans': /游戏/,
  'zh-Hant': /遊戲/,
};

for (const file of locales) {
  const locale = file.replace(/\.json$/, '');
  const dict = JSON.parse(readFileSync(join(localeDir, file), 'utf8'));
  const keys = Object.keys(dict);
  const values = Object.values(dict).join('\n');

  assert.ok(!keys.some(key => /(trainer|species|productMap)/i.test(key)), `${file} retains a trainer/species/product-map key`);
  const deadGameKeys = [
    'dash.emptyFilteredTitle', 'dash.emptyFilteredBody',
    'admin.globalHint', 'admin.globalDerivedHint', 'admin.indexEverySet',
    'admin.confirmGlobalBuild', 'admin.confirmStopGlobal', 'admin.confirmRemoveIndex',
  ];
  assert.ok(!keys.some(key => deadGameKeys.includes(key)), `${file} retains dead game-oriented copy`);
  assert.ok(!keys.some(key => key.startsWith('catalog.errStartProductMap')), `${file} retains dead product-map errors`);
  assert.ok(!keys.some(key => key.startsWith('setup.cards.')), `${file} retains dead game-selection setup copy`);
  assert.ok(!values.includes('PTCGO'), `${file} still advertises PTCGO`);

  assert.ok(dict['admin.totalUsers'] && dict['admin.filterUsers'] && dict['admin.noUserMatch'], `${file} is missing neutral user keys`);
  assert.ok(dict['deck.fullyOwnedCards'], `${file} is missing the neutral owned-card key`);
  assert.ok(keys.some(key => key.startsWith('deck.cardCount.')), `${file} is missing neutral card-count plurals`);
  assert.ok(dict['catalog.thLanguage'] && dict['catalog.notBuilt'], `${file} is missing neutral catalog keys`);
  // Retained on purpose: the UI no longer offers a "split by printing" toggle --
  // printing is always part of the stack key -- but this key is what proves each
  // locale labels the finish as Foil, so deleting it would only blind this gate.
  assert.ok(dict['collection.splitByPrinting'].includes(foilTerms[locale]), `${file} does not label the finish as Foil`);

  for (const key of [
    'deck.noMatchesHint', 'scan.catalogNotBuilt', 'scan.settingsHint',
    'setup.language.body', 'catalog.thLanguage',
    'catalog.buildWholeGame', 'catalog.buildYourOwnDesc', 'catalog.wholeGameHint',
  ]) {
    assert.ok(!staleGameTerms[locale].test(dict[key]), `${file} ${key} still mentions the removed game selection`);
  }
}

assert.equal(getPrintingBadgeLabel('Holofoil'), 'FOIL', 'legacy storage value must render as Foil');
assert.equal(getPrintingLabel('Holofoil'), 'Foil', 'details must not expose the legacy storage value');
assert.equal(getPrintingLabel('Normal'), 'Nonfoil', 'normal finish should use Magic terminology');

const sourcePolicies = [
  ['index.css', /Cyberpunk Pok[eé]dex/i],
  ['App.jsx', /reverse[- ]holo/i],
  [join('components', 'CardArtEditor.jsx'), /card\.game/],
  [join('components', 'CardImage.jsx'), /card\?\.game/],
  [join('components', 'CardInspectorModal.jsx'), />\{card\.printing\}</],
  [join('components', 'CollectionList.jsx'), /\{item\.printing\}\s*•/],
  [join('components', 'SharedCollection.jsx'), />\{activeCard\.printing\}</],
  // The deck edit pane searches; it does not browse. The "Browse Collection"
  // button dumped the whole library into the results pane and was removed by
  // design (2026-09-18) — searching is the only way to add a card to a deck.
  [join('components', 'DeckBuilder.jsx'), /\bbrowseCollection\b|\bforceBrowse\b/],
];
for (const [relative, forbidden] of sourcePolicies) {
  assert.doesNotMatch(readFileSync(join(srcDir, relative), 'utf8'), forbidden, `${relative} retains removed residue`);
}

function assertNoGameField(value, path = 'fixture') {
  if (Array.isArray(value)) return value.forEach((item, index) => assertNoGameField(item, `${path}[${index}]`));
  if (!value || typeof value !== 'object') return;
  assert.ok(!Object.hasOwn(value, 'game'), `${path} retains redundant game field`);
  for (const [key, child] of Object.entries(value)) assertNoGameField(child, `${path}.${key}`);
}
for (const file of readdirSync(fixtureDir).filter(name => name.endsWith('.json'))) {
  assertNoGameField(JSON.parse(readFileSync(join(fixtureDir, file), 'utf8')), file);
}

console.log('PASS: frontendResidue.test.js');
