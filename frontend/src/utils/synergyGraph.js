// Pure graph building for the deck Synergy map (components/SynergyMap.jsx).
// Ported from Omenpath: one node per non-basic deck card, the commander marked,
// plus optional layers - EDHREC synergy edges (card <-> commander), Commander
// Spellbook combo concept nodes, and shared Oracle-tag concept nodes.
import { isBasicLand } from './deckRenderData.js';

// Omenpath's matcher: case/punctuation-insensitive, front face only.
export const normName = (s) => String(s || '').split(' // ')[0].toLowerCase().replace(/[^a-z0-9]+/g, '');

const cardNodeId = (card) => `c:${normName(card.name)}`;

// Deck cards -> unique card list (one per name), basics skipped, commander first.
export function graphCards(cards, commander) {
  const cmdKeys = new Set();
  for (const c of cards || []) if (Number(c.is_commander) === 1) cmdKeys.add(normName(c.name));
  if (commander) cmdKeys.add(normName(commander.name));
  const byKey = new Map();
  for (const c of cards || []) {
    if (!c || !c.name || isBasicLand(c)) continue;
    const k = normName(c.name);
    const prev = byKey.get(k);
    if (prev) { prev.quantity += Number(c.quantity) || 0; continue; }
    byKey.set(k, { card: c, key: k, quantity: Number(c.quantity) || 0, isCommander: cmdKeys.has(k) });
  }
  return [...byKey.values()].sort((a, b) => Number(b.isCommander) - Number(a.isCommander));
}

/**
 * Build cytoscape elements.
 * @param {object} p
 * @param {Array} p.cards        deck cards (activeDeck.cards)
 * @param {object} [p.commander] findCommander() result
 * @param {Array} [p.synergy]    [{name, synergy}] from /api/decks/:id/synergy
 * @param {Array} [p.combos]     `included` combos from /api/decks/:id/combos
 * @param {Array} [p.tags]       [{slug, label, cards:[cardId]}] from /api/cards/tag-stats
 * @param {object} p.show        { edhrec, combos, tags } layer toggles
 * @param {Array} [p.roles]      [{key, label, ids:[cardId]}] deck roles (Ramp, Draw, ...), same as the deck Tag view
 * @param {number} [p.maxTags]   cap for extra Oracle-tag nodes beyond the roles (default 4)
 * @param {Array} [p.links]      manual links [[keyA, keyB]]
 * @returns {{ elements: Array, counts: {cards, edhrec, combos, tags, links} }}
 */
export function buildSynergyElements({ cards, commander, synergy, combos, tags, roles, show = {}, maxTags = 4, links = [] }) {
  const list = graphCards(cards, commander);
  const elements = [];
  const known = new Map();
  const byCardId = new Map();
  for (const g of list) {
    const c = g.card;
    const id = cardNodeId(c);
    known.set(g.key, id);
    if (c.id) byCardId.set(c.id, id);
    if (c.card_id) byCardId.set(c.card_id, id);
    elements.push({
      group: 'nodes',
      data: {
        id, kind: 'card', label: c.name, cardId: c.id, quantity: g.quantity,
        image: c.image_url || '', isCommander: g.isCommander || undefined,
      },
    });
  }
  const counts = { cards: list.length, edhrec: 0, combos: 0, tags: 0, links: 0 };
  const cmdIds = list.filter(g => g.isCommander).map(g => known.get(g.key));

  if (show.edhrec && cmdIds.length && Array.isArray(synergy)) {
    const seen = new Set();
    for (const s of synergy) {
      const id = known.get(normName(s.name));
      if (!id || cmdIds.includes(id) || seen.has(id)) continue;
      seen.add(id);
      elements.push({ group: 'edges', data: { id: `edhrec:${id}`, source: cmdIds[0], target: id, kind: 'edhrec', synergy: s.synergy } });
      counts.edhrec++;
    }
  }

  if (show.combos && Array.isArray(combos)) {
    combos.forEach((combo, i) => {
      const pieces = [...new Set((combo.cards || []).map(c => known.get(normName(c.name))).filter(Boolean))];
      if (pieces.length < 2) return;
      const cid = `combo:${combo.id || i}`;
      const label = (combo.produces || []).slice(0, 2).join(' · ') || 'Combo';
      elements.push({ group: 'nodes', data: { id: cid, kind: 'combo', label, comboId: combo.id } });
      for (const p of pieces) elements.push({ group: 'edges', data: { id: `${cid}>${p}`, source: cid, target: p, kind: 'combo' } });
      counts.combos++;
    });
  }

  const roleSlugs = new Set();
  if (show.tags && Array.isArray(roles)) {
    for (const r of roles) {
      for (const s of r.slugs || []) roleSlugs.add(s);
      const ids = [...new Set((r.ids || []).map(x => byCardId.get(x)).filter(Boolean))];
      if (!ids.length) continue;
      const rid = `role:${r.key}`;
      elements.push({ group: 'nodes', data: { id: rid, kind: 'tag', role: true, label: r.label || r.key, slug: r.key } });
      for (const p of ids) elements.push({ group: 'edges', data: { id: `${rid}>${p}`, source: rid, target: p, kind: 'tag' } });
      counts.tags++;
    }
  }
  if (show.tags && Array.isArray(tags)) {
    const usable = tags
      .filter(t => !roleSlugs.has(t.slug))
      .map(t => ({ ...t, ids: [...new Set((t.cards || []).map(x => byCardId.get(x)).filter(Boolean))] }))
      .filter(t => t.ids.length >= 2)
      .slice(0, maxTags);
    for (const t of usable) {
      const tid = `tag:${t.slug}`;
      elements.push({ group: 'nodes', data: { id: tid, kind: 'tag', label: t.label || t.slug, slug: t.slug } });
      for (const p of t.ids) elements.push({ group: 'edges', data: { id: `${tid}>${p}`, source: tid, target: p, kind: 'tag' } });
      counts.tags++;
    }
  }

  const ids = new Set(known.values());
  for (const [a, b] of links || []) {
    const s = `c:${a}`; const t = `c:${b}`;
    if (a === b || !ids.has(s) || !ids.has(t)) continue;
    elements.push({ group: 'edges', data: { id: `link:${[a, b].sort().join('|')}`, source: s, target: t, kind: 'link' } });
    counts.links++;
  }
  return { elements, counts };
}

// Manual links persisted per deck in localStorage ([[keyA,keyB]], keys = normName).
export const linksStorageKey = (deckId) => `scrybox.synergyLinks.${deckId}`;
export function toggleLink(links, a, b) {
  if (!a || !b || a === b) return links;
  const k = [a, b].sort().join('|');
  const has = links.some(([x, y]) => [x, y].sort().join('|') === k);
  return has ? links.filter(([x, y]) => [x, y].sort().join('|') !== k) : [...links, [a, b].sort()];
}
