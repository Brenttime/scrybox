// Add Cards > Search filter panel -> Scryfall syntax.
//
// The panel mirrors Collection's Filters drawer, but searches ALL cards, so
// every facet compiles to a Scryfall clause appended to the typed query and
// runs through the normal /api/search path. Values within one facet are ORed
// (like the Collection multi-selects); facets are ANDed.

export const EMPTY_SEARCH_FILTERS = Object.freeze({
  colors: [], colorMode: 'any', identity: [], types: [], subtypes: [], mv: [],
  rarities: [], sets: [], finishes: [], languages: [], minUsd: '', maxUsd: '',
});

export const SEARCH_RARITIES = ['common', 'uncommon', 'rare', 'mythic', 'special', 'bonus'];
export const SEARCH_FINISHES = ['nonfoil', 'foil', 'etched'];

// Common subtypes to pick from (the dropdown is searchable).
export const COMMON_SUBTYPES = [
  'Advisor', 'Angel', 'Assassin', 'Aura', 'Avatar', 'Beast', 'Bird', 'Cat', 'Cleric', 'Construct',
  'Demon', 'Dinosaur', 'Dog', 'Dragon', 'Drake', 'Druid', 'Dwarf', 'Elder', 'Eldrazi', 'Elemental',
  'Elf', 'Equipment', 'Faerie', 'Fox', 'Fungus', 'Giant', 'Gnome', 'Goblin', 'God', 'Golem', 'Horror',
  'Human', 'Hydra', 'Illusion', 'Insect', 'Knight', 'Kor', 'Merfolk', 'Minotaur', 'Monk', 'Ninja',
  'Ogre', 'Ooze', 'Orc', 'Phyrexian', 'Pirate', 'Plant', 'Rat', 'Rogue', 'Saga', 'Samurai', 'Saproling',
  'Scout', 'Serpent', 'Shaman', 'Shapeshifter', 'Skeleton', 'Sliver', 'Soldier', 'Sphinx', 'Spider',
  'Spirit', 'Squirrel', 'Treefolk', 'Troll', 'Vampire', 'Vehicle', 'Wall', 'Warrior', 'Wizard', 'Wolf',
  'Wurm', 'Zombie',
  'Forest', 'Island', 'Mountain', 'Plains', 'Swamp', 'Gate', 'Desert', 'Cave', 'Town',
  'Adventure', 'Arcane', 'Lesson', 'Trap', 'Food', 'Treasure', 'Clue', 'Class', 'Case', 'Room',
];

const WUBRG = 'wubrg';
const codes = list => WUBRG.split('').filter(c => list.map(v => String(v).toLowerCase()).includes(c)).join('');
const clean = v => String(v).trim().toLowerCase().replace(/[^a-z0-9+\-'_]/g, '');
const anyOf = parts => (parts.length > 1 ? `(${parts.join(' or ')})` : parts[0]);
const num = v => {
  if (v === '' || v == null) return null;
  const n = Number(v);
  return Number.isFinite(n) && n >= 0 ? n : null;
};

export function compileSearchFilters(f = {}) {
  const x = { ...EMPTY_SEARCH_FILTERS, ...f };
  const out = [];

  // Colour: 'any' = has at least one picked colour; 'exact' = exactly these.
  if (x.colors.length) {
    const col = codes(x.colors);
    const colorless = x.colors.some(c => String(c).toUpperCase() === 'C');
    if (x.colorMode === 'exact') out.push(col ? `c=${col}` : 'c=c');
    else out.push(anyOf([...col.split('').map(c => `c>=${c}`), ...(colorless ? ['c=c'] : [])]));
  }
  // Commander identity: fits within the picked colours.
  if (x.identity.length) {
    const id = codes(x.identity);
    if (id) out.push(`id<=${id}`);
  }
  const words = (list, key) => {
    const parts = list.map(clean).filter(Boolean).map(v => `${key}:${v}`);
    if (parts.length) out.push(anyOf(parts));
  };
  words(x.types, 't');
  words(x.subtypes, 't');
  if (x.mv.length) {
    const parts = x.mv.map(v => (String(v) === '7+' ? 'mv>=7' : /^\d+$/.test(String(v)) ? `mv=${v}` : null)).filter(Boolean);
    if (parts.length) out.push(anyOf(parts));
  }
  words(x.rarities, 'r');
  words(x.sets, 's');
  words(x.finishes.filter(v => SEARCH_FINISHES.includes(v)), 'is');
  words(x.languages, 'lang');
  const lo = num(x.minUsd);
  const hi = num(x.maxUsd);
  if (lo != null) out.push(`usd>=${lo}`);
  if (hi != null) out.push(`usd<=${hi}`);
  return out.join(' ');
}

export function countSearchFilters(f = {}) {
  const x = { ...EMPTY_SEARCH_FILTERS, ...f };
  return ['colors', 'identity', 'types', 'subtypes', 'mv', 'rarities', 'sets', 'finishes', 'languages']
    .filter(k => x[k].length > 0).length
    + (num(x.minUsd) != null ? 1 : 0) + (num(x.maxUsd) != null ? 1 : 0);
}

// Typed text + filter clause -> the query sent to /api/search.
export function withFilters(text, clause) {
  const a = String(text || '').trim();
  const b = String(clause || '').trim();
  return [a, b].filter(Boolean).join(' ');
}
