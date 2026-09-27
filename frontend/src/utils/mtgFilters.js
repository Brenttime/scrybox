// Magic-shaped filter facets shared by Add Cards and Collection.
//
// Card rows carry `types` = colour NAMES ("Blue") and `subtypes` = every word of
// the type line (["Legendary", "Creature", "Elf", "Druid"]), and `supertype` is
// always "MTG". These helpers turn that into the filters a Magic player expects:
// colour, colour identity, card type, supertype, creature/subtype, mana value.

export const COLORS = [
  { value: 'W', name: 'White' },
  { value: 'U', name: 'Blue' },
  { value: 'B', name: 'Black' },
  { value: 'R', name: 'Red' },
  { value: 'G', name: 'Green' },
  { value: 'C', name: 'Colorless' },
];
const NAME_TO_CODE = Object.fromEntries(COLORS.map(c => [c.name, c.value]));

export const CARD_TYPES = ['Creature', 'Instant', 'Sorcery', 'Artifact', 'Enchantment', 'Planeswalker', 'Land', 'Battle', 'Kindred', 'Tribal'];
export const SUPERTYPES = ['Legendary', 'Basic', 'Snow', 'World', 'Ongoing', 'Host'];
const NOT_SUBTYPE = new Set([...CARD_TYPES, ...SUPERTYPES, 'Token', 'Emblem', 'Card', 'Dungeon', 'Plane', 'Phenomenon', 'Scheme', 'Vanguard', 'Conspiracy']);

export const RARITY_ORDER = ['Common', 'Uncommon', 'Rare', 'Mythic', 'Special', 'Bonus'];
export const sortRarities = list => [...list].sort((a, b) => {
  const ia = RARITY_ORDER.indexOf(a); const ib = RARITY_ORDER.indexOf(b);
  return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib) || a.localeCompare(b);
});

// Mana value buckets: 0..6 and 7+.
export const MANA_VALUES = ['0', '1', '2', '3', '4', '5', '6', '7+'];
export const mvBucket = cmc => (cmc == null || Number.isNaN(Number(cmc)) ? null : Number(cmc) >= 7 ? '7+' : String(Math.floor(Number(cmc))));

const arr = v => {
  if (Array.isArray(v)) return v;
  if (typeof v === 'string' && v.startsWith('[')) { try { return JSON.parse(v); } catch { return []; } }
  return [];
};

export function cardColors(card) {
  const codes = arr(card.types).map(n => NAME_TO_CODE[n]).filter(Boolean);
  return codes.length ? codes : ['C'];
}
export function cardIdentity(card) {
  const codes = arr(card.color_identity).map(n => NAME_TO_CODE[n] || n).filter(c => 'WUBRG'.includes(c));
  return codes.length ? codes : ['C'];
}
export const typeWords = card => arr(card.subtypes);
export const cardTypes = card => typeWords(card).filter(w => CARD_TYPES.includes(w));
export const cardSupertypes = card => typeWords(card).filter(w => SUPERTYPES.includes(w));
export const cardSubtypes = card => typeWords(card).filter(w => !NOT_SUBTYPE.has(w));

// Colour: `mode` is 'any' (has at least one picked colour), 'exact' (exactly
// the picked colours) or 'within' (every colour of the card is picked — the
// Commander "fits in my deck" test; used for identity).
export function matchColors(cardCodes, picked, mode = 'any') {
  if (!picked.length) return true;
  const set = new Set(cardCodes);
  if (mode === 'exact') return set.size === picked.length && picked.every(c => set.has(c));
  // Colourless fits in every deck, so C never has to be picked for 'within'.
  if (mode === 'within') return [...set].every(c => c === 'C' || picked.includes(c));
  return picked.some(c => set.has(c));
}

// One place that applies every Magic facet, so both screens agree.
export function matchMtgFilters(card, f) {
  if (f.colors?.length && !matchColors(cardColors(card), f.colors, f.colorMode || 'any')) return false;
  if (f.identity?.length && !matchColors(cardIdentity(card), f.identity, 'within')) return false;
  if (f.cardTypes?.length) { const t = cardTypes(card); if (!f.cardTypes.every(x => t.includes(x))) return false; }
  if (f.supertypes?.length) { const s = cardSupertypes(card); if (!f.supertypes.some(x => s.includes(x))) return false; }
  if (f.subtypes?.length) { const s = cardSubtypes(card); if (!f.subtypes.some(x => s.includes(x))) return false; }
  if (f.manaValues?.length && !f.manaValues.includes(mvBucket(card.cmc))) return false;
  if (f.rarities?.length && !f.rarities.includes(card.rarity)) return false;
  return true;
}

export const uniqueSorted = (cards, fn) => Array.from(new Set(cards.flatMap(fn).filter(Boolean))).sort((a, b) => a.localeCompare(b));
