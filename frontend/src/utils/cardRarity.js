// Scryfall's complete rarity vocabulary. Keep this aligned with rarityRank() in
// backend/src/utils/cardSort.js so client and server sorting never diverge.
const RARITY_RANK = Object.freeze({
  common: 1,
  uncommon: 2,
  rare: 3,
  mythic: 4,
  special: 5,
  bonus: 6,
});

const BADGE_LABEL = Object.freeze({
  common: 'COM',
  uncommon: 'UNC',
  rare: 'RARE',
  mythic: 'MYTHIC',
  special: 'SPECIAL',
  bonus: 'BONUS',
  proxy: 'PROXY',
});

// An owned copy marked as a proxy shows the rarity 'Proxy' in place of its
// printed one. The server already sends rarity 'Proxy' for such rows; this is
// the client-side mirror for a row toggled in place before the next refetch.
export const PROXY_RARITY = 'Proxy';
export function displayRarity(card) {
  if (!card) return '';
  if (card.is_proxy) return PROXY_RARITY;
  return card.base_rarity !== undefined ? card.base_rarity : card.rarity;
}

const normalizeRarity = (rarity) => String(rarity || '').trim().toLowerCase();

export function isPremiumRarity(rarity) {
  return ['mythic', 'special', 'bonus'].includes(normalizeRarity(rarity));
}

// Single source of truth for the rarity tiers used across card border glow,
// badge color, and badge label.
export function getRarityTier(rarity) {
  const value = normalizeRarity(rarity);
  if (isPremiumRarity(value)) return 'top';
  if (value === 'rare') return 'rare';
  if (value === 'uncommon') return 'uncommon';
  return 'common';
}

export function getRarityRank(rarity) {
  return RARITY_RANK[normalizeRarity(rarity)] || 0;
}

export function getCardRarityBorder(rarity) {
  switch (getRarityTier(rarity)) {
    case 'top':
      return {
        border: '2.5px solid #f59e0b',
        boxShadow: '0 0 12px rgba(245, 158, 11, 0.95), inset 0 0 6px rgba(245, 158, 11, 0.5)'
      };
    case 'rare':
      return {
        border: '2px solid #e2e8f0',
        boxShadow: '0 0 8px rgba(255, 255, 255, 0.85), inset 0 0 4px rgba(255, 255, 255, 0.4)'
      };
    case 'uncommon':
      return {
        border: '1.5px solid #3b82f6',
        boxShadow: '0 0 6px rgba(59, 130, 246, 0.8)'
      };
    default:
      return {
        border: '1px solid rgba(255, 255, 255, 0.3)',
        boxShadow: 'none'
      };
  }
}

export function getRarityBadgeStyle(rarity) {
  if (normalizeRarity(rarity) === 'proxy') return { background: 'rgba(124, 58, 237, 0.85)', color: '#fff' };
  const tier = getRarityTier(rarity);
  const background = tier === 'top' ? '#f59e0b'
    : tier === 'rare' ? '#e2e8f0'
    : tier === 'uncommon' ? '#3b82f6'
    : 'rgba(156, 163, 175, 0.75)';
  const color = tier === 'rare' ? '#000' : '#fff';
  return { background, color };
}

export function getRarityBadgeLabel(rarity) {
  const value = normalizeRarity(rarity);
  return BADGE_LABEL[value] || (value ? value.slice(0, 8).toUpperCase() : '—');
}
