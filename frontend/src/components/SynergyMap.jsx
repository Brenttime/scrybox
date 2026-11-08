import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import cytoscape from 'cytoscape';
import { X, ZoomIn, ZoomOut, Maximize, Shuffle, Link2 } from 'lucide-react';
import { useT } from '../utils/i18n';
import { buildSynergyElements, linksStorageKey, toggleLink } from '../utils/synergyGraph';
import { DECK_ROLES, ROLE_SLUGS, roleOf, cardTagId } from '../utils/deckRoles';
import { isLandCard } from '../utils/deckStats';

// Deck Synergy map: the deck as an interactive cytoscape graph, ported from
// Omenpath. Cards are nodes (basics skipped, commander glows gold); layers add
// EDHREC synergy edges, Commander Spellbook combo nodes and shared Oracle-tag
// nodes. Tapping a card opens the shared CardInspectorModal via onOpenCard.

const css = (name, fb) => (getComputedStyle(document.documentElement).getPropertyValue(name) || '').trim() || fb;

function graphStyle() {
  const accent = css('--mb-accent', '#ff7a1a');
  const text = css('--text-primary', '#e6e6e6');
  const card = css('--mb-card', '#1f2329');
  const line = css('--mb-line-strong', '#3a3f48');
  const bg = css('--mb-bg', '#121417');
  const reduce = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
  const fade = reduce ? 0 : 160;
  const font = "Outfit, ui-sans-serif, system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif";
  return [
    { selector: 'node', style: {
      shape: 'round-rectangle', width: 110, height: 154,
      'background-color': card, 'background-fit': 'cover', 'background-clip': 'node',
      'border-color': line, 'border-width': 1,
      label: 'data(label)', 'text-valign': 'bottom', 'text-margin-y': 6, 'text-wrap': 'ellipsis', 'text-max-width': 150,
      'font-family': font, 'font-size': 12, 'font-weight': 500, color: text,
      'text-background-color': card, 'text-background-opacity': 0.92, 'text-background-padding': 4, 'text-background-shape': 'round-rectangle',
      'min-zoomed-font-size': 7, 'underlay-color': accent, 'underlay-padding': 7, 'underlay-opacity': 0, 'underlay-shape': 'round-rectangle',
      'overlay-opacity': 0, 'transition-property': 'opacity, underlay-opacity', 'transition-duration': fade,
    } },
    { selector: 'node[image != ""]', style: { 'background-image': 'data(image)', 'background-image-crossorigin': 'anonymous' } },
    { selector: 'node[kind = "tag"], node[kind = "combo"]', style: {
      width: 140, height: 60, 'background-image': 'none', 'text-valign': 'center', 'text-halign': 'center', 'text-margin-y': 0,
      'text-wrap': 'wrap', 'text-max-width': 124, 'font-size': 14, 'font-weight': 700, 'text-background-opacity': 0, 'border-opacity': 0.8,
    } },
    { selector: 'node[kind = "tag"]', style: { 'background-color': '#ff7a1a', 'border-color': '#ffb27a', color: '#1a0e04' } },
    { selector: 'node[kind = "combo"]', style: { width: 160, height: 68, 'background-color': '#8e44ad', 'border-color': '#d2b0ff', color: '#fff', 'underlay-color': '#a66bff' } },
    { selector: 'node[?isCommander]', style: { 'underlay-color': '#f5c542', 'underlay-opacity': 0.42, 'underlay-padding': 10, 'border-color': '#f5c542', 'border-width': 2, 'font-weight': 700, 'z-index': 20 } },
    { selector: 'edge', style: {
      width: 2, 'line-color': line, 'line-opacity': 0.75, 'line-cap': 'round', 'curve-style': 'bezier',
      'overlay-opacity': 0, 'transition-property': 'opacity, line-opacity, width', 'transition-duration': fade,
      'font-family': font, 'font-size': 10, color: text, 'text-background-color': bg, 'text-background-opacity': 0.85,
      'text-background-padding': 2, 'edge-text-rotation': 'autorotate', 'min-zoomed-font-size': 9,
    } },
    { selector: 'edge[kind = "edhrec"]', style: { 'line-color': '#35c47a', label: 'data(synergy)' } },
    { selector: 'edge[kind = "combo"]', style: { 'line-color': '#a66bff' } },
    { selector: 'edge[kind = "tag"]', style: { 'line-color': '#ff9a4d', 'line-opacity': 0.55 } },
    { selector: 'edge[kind = "link"]', style: { 'line-color': '#3d9cff', 'line-style': 'dashed', 'line-dash-pattern': [7, 5] } },
    { selector: 'node.hl-hover', style: { 'underlay-opacity': 0.4, 'z-index': 30 } },
    { selector: 'node.hl-neighbor', style: { 'underlay-opacity': 0.18, 'z-index': 25 } },
    { selector: 'edge.hl-edge', style: { 'line-opacity': 1, width: 3.5, 'z-index': 24 } },
    { selector: '.hl-faded', style: { opacity: 0.16 } },
    { selector: 'edge.hl-faded', style: { 'text-opacity': 0 } },
    { selector: 'node.link-src', style: { 'underlay-color': '#3d9cff', 'underlay-opacity': 0.6, 'border-color': '#3d9cff', 'border-width': 2 } },
    { selector: '.hide-node', style: { display: 'none' } },
  ];
}

// Omenpath's shared force layout (randomized start so stacked nodes don't pile
// up) for the linked part of the graph; unlinked cards go in a grid below it,
// shaped to the viewport, so they don't stretch the fit on a phone.
function runLayout(cy) {
  const nodes = cy.nodes(':visible');
  const lonely = nodes.filter(n => n.degree() === 0);
  const linked = cy.elements(':visible').not(lonely);
  const big = nodes.length > 200;
  let bb = { x1: 0, y1: 0, x2: 0, y2: 0, w: 0, h: 0 };
  if (linked.nodes().length) {
    linked.layout({
      name: 'cose', randomize: true, animate: false, fit: false,
      nodeDimensionsIncludeLabels: !big, nodeOverlap: 24, idealEdgeLength: () => 150, edgeElasticity: () => 80,
      nodeRepulsion: () => 1400000, gravity: 0.6, componentSpacing: 60, numIter: big ? 450 : 1000, coolingFactor: big ? 0.95 : 0.99,
    }).run();
    bb = linked.boundingBox();
  }
  if (lonely.length) {
    const aspect = Math.max(0.3, cy.width() / Math.max(1, cy.height()));
    const cellW = 140; const cellH = 200;
    const cols = Math.max(2, Math.min(lonely.length, Math.round(Math.max(bb.w / cellW, Math.sqrt(lonely.length * aspect * (cellH / cellW))))));
    const top = linked.nodes().length ? bb.y2 + 160 : 0;
    const left = linked.nodes().length ? bb.x1 + bb.w / 2 - (cols * cellW) / 2 : 0;
    lonely.sort((x, y) => String(x.data('label')).localeCompare(String(y.data('label')))).forEach((n, i) => {
      n.position({ x: left + (i % cols) * cellW + cellW / 2, y: top + Math.floor(i / cols) * cellH + cellH / 2 });
    });
  }
  cy.resize();
  cy.fit(undefined, 40);
}

const readLinks = (deckId) => {
  try { const v = JSON.parse(localStorage.getItem(linksStorageKey(deckId)) || '[]'); return Array.isArray(v) ? v : []; } catch { return []; }
};

export default function SynergyMap({ deck, commander, onClose, onOpenCard }) {
  const { t } = useT();
  const boxRef = useRef(null);
  const cyRef = useRef(null);
  const [show, setShow] = useState({ edhrec: true, combos: true, tags: false });
  const [data, setData] = useState({ synergy: null, combos: null, tags: null, roleTags: null });
  const [errors, setErrors] = useState({});
  const [linkMode, setLinkMode] = useState(false);
  const [linkedOnly, setLinkedOnly] = useState(false);
  const [links, setLinks] = useState(() => readLinks(deck.id));
  const linkSrc = useRef(null);
  const cardsById = useMemo(() => new Map((deck.cards || []).map(c => [c.id, c])), [deck.cards]);
  const handlers = useRef({});

  // Lazy layer fetches: each only when its chip is first switched on.
  useEffect(() => {
    let alive = true;
    const load = (key, url, init, pick) => {
      fetch(url, init)
        .then(async r => { const b = await r.json().catch(() => ({})); if (!r.ok) throw new Error(b.error || String(r.status)); return b; })
        .then(b => { if (alive) setData(d => ({ ...d, [key]: pick(b) })); })
        .catch(e => { if (alive) { setErrors(x => ({ ...x, [key]: e.message })); setData(d => ({ ...d, [key]: [] })); } });
    };
    const id = encodeURIComponent(deck.id);
    if (show.edhrec && data.synergy === null && commander) {
      load('synergy', `/api/decks/${id}/synergy?commander=${encodeURIComponent(commander.name)}`, undefined, b => b.cards || []);
    }
    if (show.combos && data.combos === null) load('combos', `/api/decks/${id}/combos`, undefined, b => b.included || []);
    if (show.tags && data.tags === null) {
      const ids = [...new Set((deck.cards || []).map(c => c.card_id || c.id).filter(Boolean))];
      if (ids.length) load('tags', '/api/cards/tag-stats', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ ids }) }, b => b.tags || []);
    }
    if (show.tags && data.roleTags === null) {
      const ids = [...new Set((deck.cards || []).map(cardTagId).filter(Boolean))];
      if (ids.length) load('roleTags', '/api/cards/role-tags', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ ids, slugs: ROLE_SLUGS }) }, b => b.tags || {});
    }
    return () => { alive = false; };
  }, [show, data, deck.id, deck.cards, commander]);

  // Same roles as the deck's Tag view (first matching role per card); lands stay out.
  const roles = useMemo(() => {
    if (!data.roleTags) return null;
    return DECK_ROLES.map(r => ({
      key: r.key, slugs: r.slugs, label: t(`deck.group.${r.key}`),
      ids: (deck.cards || []).filter(c => roleOf(c, data.roleTags, isLandCard) === r.key).map(c => c.id),
    }));
  }, [data.roleTags, deck.cards, t]);
  const { elements, counts } = useMemo(() => buildSynergyElements({
    cards: deck.cards, commander,
    synergy: data.synergy, combos: data.combos, tags: data.tags, roles, show, links,
  }), [deck.cards, commander, data, roles, show, links]);

  handlers.current.tapCard = (node) => {
    if (linkMode) {
      const key = node.id().slice(2);
      const src = linkSrc.current;
      if (!src) { linkSrc.current = key; node.addClass('link-src'); return; }
      cyRef.current?.nodes('.link-src').removeClass('link-src');
      linkSrc.current = null;
      if (src !== key) setLinks(l => toggleLink(l, src, key));
      return;
    }
    const card = cardsById.get(node.data('cardId'));
    if (card) onOpenCard(card);
  };

  // Mount cytoscape once.
  useEffect(() => {
    const cy = cytoscape({ container: boxRef.current, elements: [], style: graphStyle(), minZoom: 0.08, maxZoom: 3.5, motionBlur: false, textureOnViewport: false, pixelRatio: 'auto' });
    cyRef.current = cy;
    window.__synergyCy = cy;
    const clear = () => cy.batch(() => cy.elements().removeClass('hl-faded hl-hover hl-neighbor hl-edge'));
    const focus = (n) => {
      clear();
      const hood = n.closedNeighborhood();
      cy.batch(() => {
        n.addClass('hl-hover');
        hood.nodes().not(n).addClass('hl-neighbor');
        hood.edges().addClass('hl-edge');
        cy.elements().not(hood).addClass('hl-faded');
      });
    };
    cy.on('mouseover', 'node', e => focus(e.target));
    cy.on('mouseout', 'node', clear);
    cy.on('tap', 'node', e => {
      if (e.target.data('kind') === 'card') handlers.current.tapCard(e.target);
      else focus(e.target);
    });
    cy.on('tap', e => { if (e.target === cy) clear(); });
    // The panel can still be settling (lazy chunk, phone sheet) when cytoscape
    // first measures it; track the real size and refit until the user moves the view.
    let touched = false;
    cy.on('scrollzoom pinchzoom dragpan', () => { touched = true; });
    const ro = new ResizeObserver(() => { cy.resize(); if (!touched) cy.fit(undefined, 40); });
    ro.observe(boxRef.current);
    return () => { ro.disconnect(); cy.destroy(); cyRef.current = null; if (window.__synergyCy === cy) delete window.__synergyCy; };
  }, []);

  // Swap elements + re-layout whenever the layers change.
  useEffect(() => {
    const cy = cyRef.current;
    if (!cy) return;
    cy.batch(() => { cy.elements().remove(); cy.add(elements); });
    cy.nodes('[kind = "card"]').forEach(n => { if (linkedOnly && n.degree() === 0 && !n.data('isCommander')) n.addClass('hide-node'); });
    runLayout(cy);
  }, [elements, linkedOnly]);

  useEffect(() => { try { localStorage.setItem(linksStorageKey(deck.id), JSON.stringify(links)); } catch { /* quota */ } }, [links, deck.id]);
  useEffect(() => { if (!linkMode) { linkSrc.current = null; cyRef.current?.nodes('.link-src').removeClass('link-src'); } }, [linkMode]);

  useEffect(() => {
    const onKey = (e) => { if (e.key === 'Escape') onClose(); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const zoom = useCallback((f) => {
    const cy = cyRef.current; if (!cy) return;
    cy.animate({ zoom: { level: cy.zoom() * f, position: { x: cy.width() / 2, y: cy.height() / 2 } } }, { duration: 180 });
  }, []);
  const fit = () => cyRef.current?.animate({ fit: { padding: 48 } }, { duration: 250 });
  const relayout = () => cyRef.current && runLayout(cyRef.current);
  const toggle = (k) => setShow(s => ({ ...s, [k]: !s[k] }));
  const loading = (show.edhrec && commander && data.synergy === null) || (show.combos && data.combos === null) || (show.tags && (data.tags === null || data.roleTags === null));
  const errText = Object.entries(errors).filter(([k]) => (k === 'synergy' ? show.edhrec : show[k])).map(([, v]) => v).join(' · ');

  return (
    <div className="modal-overlay syn-overlay" onClick={onClose}>
      <div className="glass-panel syn-panel" role="dialog" aria-modal="true" aria-label={t('deck.synergyMap')} onClick={e => e.stopPropagation()}>
        <button type="button" className="modal-close btn btn-secondary btn-icon-only syn-close" onClick={onClose} aria-label={t('common.close')}>
          <X size={16} />
        </button>
        <div className="syn-head">
          <h3>{t('deck.synergyMap')}</h3>
          <p className="syn-sub">
            {t('deck.synergyCounts', { cards: counts.cards, edhrec: counts.edhrec, combos: counts.combos, tags: counts.tags })}
            {!commander && <> · {t('deck.synergyNoCommander')}</>}
            {loading && <> · {t('common.loading')}</>}
            {errText && <span className="syn-err"> · {errText}</span>}
          </p>
          <div className="deck-toolbar-chips syn-chips">
            <button type="button" className={`deck-chip deck-chip-btn${show.edhrec ? ' on' : ''}`} aria-pressed={show.edhrec} onClick={() => toggle('edhrec')} disabled={!commander}>
              <span className="syn-dot syn-dot-edhrec" /> {t('deck.synergyEdhrec')}
            </button>
            <button type="button" className={`deck-chip deck-chip-btn${show.combos ? ' on' : ''}`} aria-pressed={show.combos} onClick={() => toggle('combos')}>
              <span className="syn-dot syn-dot-combo" /> {t('deck.synergyCombos')}
            </button>
            <button type="button" className={`deck-chip deck-chip-btn${show.tags ? ' on' : ''}`} aria-pressed={show.tags} onClick={() => toggle('tags')}>
              <span className="syn-dot syn-dot-tag" /> {t('deck.synergyTags')}
            </button>
            <button type="button" className={`deck-chip deck-chip-btn${linkMode ? ' on' : ''}`} aria-pressed={linkMode} onClick={() => setLinkMode(v => !v)}>
              <Link2 size={13} /> {t('deck.synergyLinkMode')}{counts.links ? ` (${counts.links})` : ''}
            </button>
            <button type="button" className={`deck-chip deck-chip-btn${linkedOnly ? ' on' : ''}`} aria-pressed={linkedOnly} onClick={() => setLinkedOnly(v => !v)}>
              {t('deck.synergyLinkedOnly')}
            </button>
          </div>
          {linkMode && <p className="syn-hint">{t('deck.synergyLinkHint')}</p>}
        </div>
        <div className="syn-stage">
          <div ref={boxRef} className="syn-cy" data-testid="synergy-cy" />
          <div className="syn-zoom" role="toolbar" aria-label={t('deck.synergyZoom')}>
            <button type="button" onClick={() => zoom(1.25)} aria-label={t('deck.synergyZoomIn')}><ZoomIn size={16} /></button>
            <button type="button" onClick={() => zoom(0.8)} aria-label={t('deck.synergyZoomOut')}><ZoomOut size={16} /></button>
            <button type="button" onClick={fit} aria-label={t('deck.synergyFit')}><Maximize size={16} /></button>
            <button type="button" onClick={relayout} aria-label={t('deck.synergyRelayout')}><Shuffle size={16} /></button>
          </div>
          <ul className="syn-legend">
            <li><span className="syn-ln syn-ln-cmd" />{t('deck.synergyLegendCommander')}</li>
            {show.edhrec && <li><span className="syn-ln syn-ln-edhrec" />{t('deck.synergyEdhrec')}</li>}
            {show.combos && <li><span className="syn-ln syn-ln-combo" />{t('deck.synergyCombos')}</li>}
            {show.tags && <li><span className="syn-ln syn-ln-tag" />{t('deck.synergyTags')}</li>}
            {counts.links > 0 && <li><span className="syn-ln syn-ln-link" />{t('deck.synergyLinkMode')}</li>}
          </ul>
        </div>
      </div>
    </div>
  );
}
