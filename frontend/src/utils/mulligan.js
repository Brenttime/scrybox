// Hand simulator rules (London mulligan, CR 103.5).
// Every mulligan draws a fresh 7; the player then puts N cards on the bottom.
// Commander (CR 103.5c, multiplayer): the first mulligan is free, so N is one
// less, and the commander starts in the command zone, never in the library.
import { shuffleArray } from './shuffle.js';

export const OPENING_HAND = 7;

export const isCommanderDeck = (deck, commanders) =>
  (commanders && commanders.length > 0) || /commander|edh/i.test(String(deck?.format || ''));

export function bottomsOwed(mulligans, commander) {
  return Math.max(0, mulligans - (commander ? 1 : 0));
}

// cards: deck rows with quantity; commanderIds: ids that live in the command zone.
export function buildLibrary(cards, commanderIds = []) {
  const zone = new Set(commanderIds);
  const library = [];
  const commandZone = [];
  for (const c of cards || []) {
    const qty = Math.max(0, Number(c.quantity) || 0);
    for (let i = 0; i < qty; i++) (zone.has(c.id) ? commandZone : library).push({ ...c });
  }
  return { library, commandZone };
}

// Shuffle everything not in the command zone and deal a fresh 7.
export function deal(cards, shuffle = shuffleArray) {
  const lib = shuffle(cards);
  return { hand: lib.slice(0, OPENING_HAND), library: lib.slice(OPENING_HAND) };
}

// Move hand[idx] to the bottom of the library.
export function putOnBottom(hand, library, idx) {
  if (idx < 0 || idx >= hand.length) return { hand, library };
  return { hand: hand.filter((_, i) => i !== idx), library: [...library, hand[idx]] };
}
