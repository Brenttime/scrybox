import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildSynergyElements, graphCards, normName, toggleLink } from './synergyGraph.js';

const card = (id, name, extra = {}) => ({ id, name, quantity: 1, subtypes: [], image_url: `https://x/${id}.jpg`, ...extra });
const deck = [
  card('cmd', 'Noctis, Prince of Lucis', { is_commander: 1 }),
  card('a', 'Sword of Fire and Ice'),
  card('b', 'Puresteel Paladin'),
  card('c', 'Sram, Senior Edificer'),
  card('a2', 'Sword of Fire and Ice'),
  card('isl', 'Island', { subtypes: ['Basic', 'Land', 'Island'] }),
  card('dfc', 'Esika, God of the Tree // The Prismatic Bridge'),
];

test('graphCards skips basics, merges names, puts commander first', () => {
  const g = graphCards(deck, null);
  assert.equal(g.length, 5);
  assert.equal(g[0].card.id, 'cmd');
  assert.equal(g[0].isCommander, true);
  assert.equal(g.find(x => x.card.id === 'a').quantity, 2);
});

test('normName matches front face, ignores punctuation', () => {
  assert.equal(normName('Esika, God of the Tree // The Prismatic Bridge'), normName('Esika, God of the Tree'));
});

test('layers add edges and concept nodes only when toggled', () => {
  const synergy = [{ name: 'Puresteel Paladin', synergy: 0.5 }, { name: 'Not In Deck', synergy: 0.9 }, { name: 'Noctis, Prince of Lucis', synergy: 1 }];
  const combos = [
    { id: '1-2', cards: [{ name: 'Puresteel Paladin' }, { name: 'Sram, Senior Edificer' }], produces: ['Infinite draw'] },
    { id: 'x', cards: [{ name: 'Sram, Senior Edificer' }, { name: 'Elsewhere' }] },
  ];
  const tags = [{ slug: 'equipment-matters', label: 'equipment matters', cards: ['b', 'c'] }, { slug: 'solo', cards: ['b'] }];
  const off = buildSynergyElements({ cards: deck, synergy, combos, tags, show: {} });
  assert.equal(off.elements.length, 5);
  const on = buildSynergyElements({ cards: deck, synergy, combos, tags, show: { edhrec: true, combos: true, tags: true } });
  assert.deepEqual(on.counts, { cards: 5, edhrec: 1, combos: 1, tags: 1, links: 0 });
  assert.ok(on.elements.some(e => e.data.id === 'combo:1-2' && e.data.label === 'Infinite draw'));
  assert.ok(on.elements.some(e => e.data.kind === 'edhrec' && e.data.source === `c:${normName('Noctis, Prince of Lucis')}`));
});

test('EDHREC edges need a commander', () => {
  const r = buildSynergyElements({ cards: deck.slice(1), synergy: [{ name: 'Puresteel Paladin', synergy: 0.5 }], show: { edhrec: true } });
  assert.equal(r.counts.edhrec, 0);
});

test('manual links toggle and render', () => {
  const a = normName('Puresteel Paladin'); const b = normName('Sram, Senior Edificer');
  let links = toggleLink([], a, b);
  assert.equal(links.length, 1);
  assert.equal(buildSynergyElements({ cards: deck, show: {}, links }).counts.links, 1);
  links = toggleLink(links, b, a);
  assert.equal(links.length, 0);
  assert.deepEqual(toggleLink([], a, a), []);
});
