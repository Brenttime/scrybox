// Card-list text export for the UI. The canonical builder lives in
// shared/cardListText.js — the server's cardlist route ports it with
// byte-parity (GET /api/collection/cardlist, collection_writes.rs), and this
// file keeps the exact same behaviour so the button and
// the API never disagree. cardList.test.js imports the shared copy and asserts
// the two line up byte for byte; if you change one, change both.

const norm = (v) => String(v == null ? '' : v).trim();

// One card line. plain: "4 Lightning Bolt" — detailed: "4 Lightning Bolt (JUD) 124".
// Set code uppercased; split cards stay one line, name verbatim.
export function cardListLine(card, style = 'plain') {
  const parsedQty = parseInt(card.quantity, 10);
  const qty = Number.isFinite(parsedQty) ? parsedQty : 1;
  if (qty <= 0) return '';
  const name = norm(card.name);
  if (style === 'detailed') {
    const set = norm(card.set_id || card.set_code).toUpperCase();
    const num = norm(card.number || card.collector_number);
    let line = name;
    if (set) line += ` (${set})`;
    if (num) line += ` ${num}`;
    return `${qty} ${line}`;
  }
  return `${qty} ${name}`;
}

export function buildCardListText(cards, style = 'plain') {
  if (!Array.isArray(cards) || cards.length === 0) return '';
  return cards.map(c => cardListLine(c, style)).filter(Boolean).join('\n');
}
