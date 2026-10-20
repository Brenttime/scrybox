// Deck-page sidebar numbers: mana curve of spells, colour demand vs land
// sources, and the declared commander. Pure, so the redesign's stats are
// testable without rendering.
import { isBasicLand } from './deckRenderData.js';

export const COLOR_LETTERS = { White: 'W', Blue: 'U', Black: 'B', Red: 'R', Green: 'G' };
const BASIC_TYPES = { Plains: 'W', Island: 'U', Swamp: 'B', Mountain: 'R', Forest: 'G' };
const ORDER = ['W', 'U', 'B', 'R', 'G'];

export const isLandCard = (card) => {
  const subs = card?.subtypes || [];
  return subs.includes('Land') || card?.supertype === 'Land';
};

// card.types holds the card's colours as names ('Blue'); letters out.
export const cardColorLetters = (card) => (card?.types || [])
  .map(c => COLOR_LETTERS[c] || (ORDER.includes(c) ? c : null))
  .filter(Boolean);

// Declared commander first (deck_cards.is_commander); otherwise the card the
// deck list already uses for the art (commander_card_id / commander_name),
// but only for Commander-format decks.
export function findCommander(deck) {
  const cards = deck?.cards || [];
  const declared = cards.find(c => Number(c.is_commander) === 1);
  if (declared) return declared;
  if (!/commander|edh/i.test(deck?.format || '')) return null;
  if (deck?.commander_card_id) {
    const byId = cards.find(c => c.id === deck.commander_card_id);
    if (byId) return byId;
  }
  if (deck?.commander_name) return cards.find(c => c.name === deck.commander_name) || null;
  return null;
}

export function deckSidebarStats(cards, commanderId = null) {
  const curve = [0, 0, 0, 0, 0, 0, 0, 0];
  let cmcSum = 0;
  let spellCount = 0;
  const pips = {};
  const sources = {};
  for (const card of cards || []) {
    const q = Number(card.quantity) || 0;
    if (isLandCard(card)) {
      const hit = new Set();
      const subs = card.subtypes || [];
      for (const [type, letter] of Object.entries(BASIC_TYPES)) {
        if (subs.includes(type) || (isBasicLand(card) && card.name === type)) hit.add(letter);
      }
      for (const c of card.color_identity || []) {
        const l = COLOR_LETTERS[c] || c;
        if (ORDER.includes(l)) hit.add(l);
      }
      for (const l of hit) sources[l] = (sources[l] || 0) + q;
      continue;
    }
    for (const l of cardColorLetters(card)) pips[l] = (pips[l] || 0) + q;
    if (card.id === commanderId) continue;
    if (card.cmc === null || card.cmc === undefined) continue;
    const cmc = Number(card.cmc) || 0;
    curve[Math.min(Math.floor(cmc), 7)] += q;
    cmcSum += cmc * q;
    spellCount += q;
  }
  const pipTotal = Object.values(pips).reduce((a, b) => a + b, 0);
  const colors = ORDER.filter(l => pips[l] || sources[l]).map(l => ({
    color: l,
    spells: pips[l] || 0,
    share: pipTotal ? (pips[l] || 0) / pipTotal : 0,
    lands: sources[l] || 0,
  }));
  return {
    curve,
    avgCmc: spellCount ? cmcSum / spellCount : 0,
    colors,
    maxLands: Math.max(1, ...colors.map(c => c.lands)),
  };
}
