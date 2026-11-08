import { useCallback, useEffect, useRef, useState } from 'react';
import { X, Search, Highlighter, Check } from 'lucide-react';
import CardImage from './CardImage';
import CardImageZoom from './CardImageZoom';
import LoadMore from './LoadMore';
import { useT } from '../utils/i18n';

// Oracle tag explorer (deck sidebar -> tap a tag): every Commander-legal card
// with that Tagger tag inside the commander's color identity, from local
// Scryfall, plus a filter box that narrows it with more Scryfall syntax.
const PAGE = 60;
const LETTER = { white: 'w', blue: 'u', black: 'b', red: 'r', green: 'g', w: 'w', u: 'u', b: 'b', r: 'r', g: 'g' };
const PIP = { w: 'W', u: 'U', b: 'B', r: 'R', g: 'G' };

const identityOf = (commander) => {
  const set = new Set((commander?.color_identity || []).map(c => LETTER[String(c).toLowerCase()]).filter(Boolean));
  return ['w', 'u', 'b', 'r', 'g'].filter(l => set.has(l)).join('');
};

export default function OtagExplorer({ tag, commander, deckNames, onHighlight, onClose }) {
  const { t } = useT();
  const ident = identityOf(commander);
  const [filter, setFilter] = useState('');
  const [applied, setApplied] = useState('');
  const [cards, setCards] = useState([]);
  const [total, setTotal] = useState(null);
  const [page, setPage] = useState(1);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [zoom, setZoom] = useState(null);
  const req = useRef(0);

  const base = `otag:${tag.slug} f:commander -is:funny${commander ? ` id<=${ident || 'c'}` : ''}`;
  const query = applied ? `${base} (${applied})` : base;

  const load = useCallback(async (pageNum) => {
    const id = ++req.current;
    setBusy(true);
    setError('');
    try {
      const params = new URLSearchParams({ q: query, scope: 'internet', lang: 'en', page: String(pageNum), limit: String(PAGE) });
      const res = await fetch(`/api/search?${params.toString()}`);
      const body = await res.json().catch(() => []);
      if (id !== req.current) return;
      if (!res.ok) { setError(body.error === 'INVALID_QUERY' ? t('otagx.badQuery') : (body.error || t('otagx.error'))); return; }
      const rows = Array.isArray(body) ? body : [];
      const reported = parseInt(res.headers.get('X-Total-Count'), 10);
      setTotal(Number.isFinite(reported) ? reported : null);
      setCards(prev => (pageNum === 1 ? rows : [...prev, ...rows.filter(r => !prev.some(p => p.id === r.id))]));
      setPage(pageNum);
    } catch {
      if (id === req.current) setError(t('otagx.error'));
    } finally {
      if (id === req.current) setBusy(false);
    }
  }, [query, t]);

  useEffect(() => { setCards([]); load(1); }, [load]);
  // Filter as you type (debounced); Enter applies at once.
  useEffect(() => {
    const v = filter.trim();
    if (v === applied) return undefined;
    const timer = setTimeout(() => setApplied(v), 450);
    return () => clearTimeout(timer);
  }, [filter, applied]);

  const hasMore = total != null ? cards.length < total : cards.length >= page * PAGE;
  const inDeck = (c) => deckNames.has(String(c.name || '').toLowerCase());

  return (
    <div className="modal-overlay otx-overlay" onClick={e => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="glass-panel otx-modal" role="dialog" aria-modal="true" aria-label={tag.label}>
        <button className="modal-close btn btn-secondary btn-icon-only" onClick={onClose} aria-label={t('common.close')}><X size={18} /></button>
        <div className="otx-head">
          <h2 className="otx-title">{tag.label}</h2>
          <div className="otx-sub">
            <code>otag:{tag.slug}</code>
            {commander && (
              <span className="otx-ident" title={t('otagx.identity')}>
                {(ident || 'c').split('').map(l => (l === 'c'
                  ? <span key="c" className="dk-pip dk-pip-C" />
                  : <span key={l} className={`dk-pip dk-pip-${PIP[l]}`} />))}
                <span>{commander.name}</span>
              </span>
            )}
          </div>
          <div className="otx-stats">
            {t('otagx.counts', { inDeck: tag.count, total: total ?? cards.length })}
            <button type="button" className="coll-textbtn" onClick={() => { onHighlight(); onClose(); }}>
              <Highlighter size={13} /> {t('otagx.highlight')}
            </button>
          </div>
          <form className="otx-filter" onSubmit={e => { e.preventDefault(); setApplied(filter.trim()); }}>
            <Search size={15} aria-hidden="true" />
            <input type="text" enterKeyHint="search" className="input-control" value={filter} onChange={e => setFilter(e.target.value)}
              placeholder={t('otagx.filterPlaceholder')} aria-label={t('otagx.filterPlaceholder')}
              autoCapitalize="off" autoCorrect="off" spellCheck={false} />
            {filter && <button type="button" className="deck-toolbar-clear" aria-label={t('addsearch.clear')} onClick={() => { setFilter(''); setApplied(''); }}><X size={13} /></button>}
          </form>
        </div>
        {error && <div className="cmb-msg cmb-err">{error}</div>}
        {!error && !busy && cards.length === 0 && <div className="cmb-msg">{t('otagx.none')}</div>}
        <div className="otx-grid">
          {cards.map(c => (
            <button key={c.id} type="button" className={`otx-card${inDeck(c) ? ' in-deck' : ''}`} onClick={() => setZoom(c)} aria-label={c.name}>
              <CardImage card={c} loading="lazy" draggable={false} />
              {inDeck(c) && <span className="otx-badge"><Check size={11} /> {t('otagx.inDeck')}</span>}
            </button>
          ))}
        </div>
        {busy && <div className="cmb-msg"><div className="spinner" /></div>}
        {!busy && !error && hasMore && cards.length > 0 && (
          <LoadMore busy={busy} onLoad={() => load(page + 1)}>{t('addsearch.loadMore')}</LoadMore>
        )}
      </div>
      {zoom && <CardImageZoom card={zoom} onClose={() => setZoom(null)} />}
    </div>
  );
}
