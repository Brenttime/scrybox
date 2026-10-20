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

const LETTER_OF = (c) => COLOR_LETTERS[c] || (ORDER.includes(c) ? c : null);

const oracleText = (card) => {
  const parts = [card?.oracle_text || ''];
  let faces = card?.oracle_faces;
  if (typeof faces === 'string') { try { faces = JSON.parse(faces); } catch { faces = null; } }
  if (Array.isArray(faces)) for (const f of faces) parts.push(typeof f === 'string' ? f : (f?.oracle_text || ''));
  return parts.join('\n');
};

// Colours a land can make for THIS deck. Reads basic land types, the mana
// symbols in its "Add ..." text, "any color" / commander-identity lands and
// fetch lands (basic land -> every deck colour; named types -> those).
// Falls back to colour identity when there is no rules text.
export function landProduces(card, deckColors = []) {
  const hit = new Set();
  const subs = card?.subtypes || [];
  for (const [type, letter] of Object.entries(BASIC_TYPES)) {
    if (subs.includes(type) || (isBasicLand(card) && (card.name || '').endsWith(type))) hit.add(letter);
  }
  const text = oracleText(card);
  if (text.trim()) {
    for (const line of text.split('\n')) {
      if (/\badd\b/i.test(line)) {
        for (const m of line.matchAll(/\{([WUBRG])\}/g)) hit.add(m[1]);
        if (/mana of any (color|type)|any color in your commander|commander's color identity/i.test(line)) deckColors.forEach(l => hit.add(l));
      }
      const fetch = line.match(/search your library for (?:an? |up to \w+ )?([^.]*?)card/i);
      if (fetch) {
        const named = Object.entries(BASIC_TYPES).filter(([type]) => new RegExp(`\\b${type}\\b`).test(fetch[1])).map(([, l]) => l);
        if (named.length) named.forEach(l => hit.add(l));
        else if (/basic land|\bland\b/i.test(fetch[1])) deckColors.forEach(l => hit.add(l));
      }
    }
  } else {
    for (const c of card?.color_identity || []) { const l = LETTER_OF(c); if (l) hit.add(l); }
  }
  return hit;
}

export function deckSidebarStats(cards, commanderId = null) {
  // Deck colours: the commander's identity when known, else every colour a
  // spell in the deck uses.
  const commander = (cards || []).find(c => c.id === commanderId);
  let deckColors = (commander?.color_identity || []).map(LETTER_OF).filter(Boolean);
  if (!deckColors.length) {
    const seen = new Set();
    for (const c of cards || []) if (!isLandCard(c)) for (const l of [...cardColorLetters(c), ...(c.color_identity || []).map(LETTER_OF)]) if (l) seen.add(l);
    deckColors = ORDER.filter(l => seen.has(l));
  }
  const curve = [0, 0, 0, 0, 0, 0, 0, 0];
  let cmcSum = 0;
  let spellCount = 0;
  const pips = {};
  const sources = {};
  for (const card of cards || []) {
    const q = Number(card.quantity) || 0;
    if (isLandCard(card)) {
      const hit = landProduces(card, deckColors);
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
