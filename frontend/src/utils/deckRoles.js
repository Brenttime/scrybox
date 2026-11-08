// Deck "Tag view" (Archidekt-style): functional roles from local Scryfall
// Tagger oracle tags. Each role lists Tagger slugs; a card matches a role when
// it carries one of them directly or through an ancestor tag. Every card goes
// to its FIRST matching role, so section counts add up to the deck size.
export const DECK_ROLES = [
  { key: 'ramp', slugs: ['ramp'] },
  { key: 'draw', slugs: ['card-advantage', 'draw'] },
  { key: 'wipe', slugs: ['sweeper'] },
  { key: 'removal', slugs: ['removal'] },
  { key: 'counter', slugs: ['counterspell'] },
  { key: 'tutor', slugs: ['tutor'] },
  { key: 'protection', slugs: ['protection', 'gives-protection'] },
  { key: 'recursion', slugs: ['recursion', 'reanimate'] },
  { key: 'sac', slugs: ['sacrifice-outlet'] },
  { key: 'evasion', slugs: ['gives-evasion'] },
];

export const ROLE_SLUGS = [...new Set(DECK_ROLES.flatMap(r => r.slugs))];

export const cardTagId = (card) => card?.card_id || card?.id;

// tagMap: { [cardId]: [slug, ...] }. isLand(card) keeps lands in one section
// (most lands carry no role tags, and ramp lands would otherwise scatter).
export function roleOf(card, tagMap, isLand) {
  if (isLand(card)) return 'Land';
  const tags = new Set(tagMap[cardTagId(card)] || []);
  const role = DECK_ROLES.find(r => r.slugs.some(s => tags.has(s)));
  return role ? role.key : 'Other';
}

export function groupByRole(cards, tagMap, isLand) {
  const order = [...DECK_ROLES.map(r => r.key), 'Other', 'Land'];
  const byKey = new Map(order.map(k => [k, []]));
  for (const card of cards) byKey.get(roleOf(card, tagMap, isLand)).push(card);
  return order
    .map(name => {
      const list = byKey.get(name).slice().sort((a, b) => String(a.name).localeCompare(String(b.name)));
      return { name, cards: list, count: list.reduce((n, c) => n + (c.quantity || 0), 0) };
    })
    .filter(g => g.cards.length > 0);
}
