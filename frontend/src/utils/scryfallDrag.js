// Drag a card out of Scrybox into Moxfield (or any site that accepts cards
// dragged from Scryfall). The drag carries exactly what a Scryfall search
// result carries: the card page link plus the card image on cards.scryfall.io.

const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;

const escapeHtml = (s) => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

export function scryfallDragData(card) {
  if (!card) return null;
  const rawId = String(card.card_id || card.id || '');
  const fromImage = String(card.image_url || '').match(UUID);
  const idMatch = rawId.match(UUID) || fromImage;
  if (!idMatch) return null;
  const id = idMatch[0].toLowerCase();
  const image = /^https:\/\/cards\.scryfall\.io\//.test(card.image_url || '')
    ? card.image_url
    : `https://cards.scryfall.io/normal/front/${id[0]}/${id[1]}/${id}.jpg`;
  const set = String(card.set_id || card.set_code || '').toLowerCase();
  const slug = String(card.name || '').toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
  const page = set && card.number
    ? `https://scryfall.com/card/${encodeURIComponent(set)}/${encodeURIComponent(card.number)}/${slug}`
    : `https://scryfall.com/card/${id}`;
  const name = card.name || '';
  return {
    page,
    image,
    html: `<a href="${escapeHtml(page)}"><img src="${escapeHtml(image)}" alt="${escapeHtml(name)}"></a>`,
  };
}

// Mouse/trackpad only: on touch, a long press is the multi-select gesture.
export const canDragCards = () =>
  typeof window !== 'undefined' && !!window.matchMedia && !window.matchMedia('(pointer: coarse)').matches;

export function setScryfallDrag(e, card) {
  const data = scryfallDragData(card);
  if (!data || !e.dataTransfer) return false;
  e.dataTransfer.effectAllowed = 'copy';
  e.dataTransfer.setData('text/uri-list', data.page);
  e.dataTransfer.setData('text/plain', data.page);
  e.dataTransfer.setData('text/html', data.html);
  return true;
}
