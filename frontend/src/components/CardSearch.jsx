import { useState, useEffect, useMemo, useRef, useCallback } from 'react';
import { Search, Plus, Minus, X, ShieldAlert, Check, Undo2, Braces, Zap, SlidersHorizontal } from 'lucide-react';
import { usdPrice } from '../utils/resolveCardPrice';
import { CONDITIONS, LANGUAGES } from '../utils/cardOptions';
import { displayName } from '../utils/languages';
import CardImage from './CardImage';
import CardImageZoom from './CardImageZoom';
import LoadMore from './LoadMore';
import { useT } from '../utils/i18n';
import { adjustOwnedQuantityByName } from '../utils/cardIdentity';
import { looksLikeSyntax } from '../utils/scryfallSyntax';
import ColorPicker from './ColorPicker';
import MultiSelectDropdown from './MultiSelectDropdown';
import Field from './FilterField';
import { CARD_TYPES, MANA_VALUES } from '../utils/mtgFilters';
import { LANGUAGES as LANGUAGE_TABLE } from '../utils/languages';
import { EMPTY_SEARCH_FILTERS, SEARCH_RARITIES, SEARCH_FINISHES, COMMON_SUBTYPES, compileSearchFilters, countSearchFilters, withFilters } from '../utils/searchFilters';
import { parseDirectAdd, directAddQuery, pickPrinting, sortByRelease } from '../utils/directAdd';

// Add Cards > Search, grid-first (Brent's "C grid" design):
//   one box (as-you-type suggestions, Scryfall syntax, typed direct-add
//   shortcuts on Enter) -> one sideways chip row + Nonfoil/Foil switch ->
//   the result grid right below. Every tile's round + adds one copy at once
//   (Undo toast); tapping the art opens a compact sheet for qty/printing/etc.
//
// A plain name lists EVERY printing (`!"Name" unique:prints`, newest first);
// anything with syntax lists matching cards and pages through has_more. Prices
// come merged server-side (the sidecar has none); a missing quote shows "—".
const PAGE = 60;
const PRINTS_PAGE = 250;

const SYNTAX_CHIPS = [
  { key: 'creatures', q: 't:creature' },
  { key: 'commanders', q: 'is:commander' },
  { key: 'mythic', q: 'r:mythic' },
  { key: 'ramp', q: 'otag:ramp' },
];

const priceLabel = (card, printing) => {
  const p = usdPrice(card, printing);
  return p == null ? '—' : `$${p.toFixed(2)}`;
};
const hasFoil = (c) => (Array.isArray(c.finishes) ? c.finishes.some(f => f === 'foil' || f === 'etched') : c.price_holofoil > 0);

function namesQuery(name) {
  return `!"${String(name).replace(/"/g, '')}" unique:prints`;
}

function CardSearch({ onAddSuccess, showToast }) {
  const { t } = useT();
  const [text, setText] = useState('');
  const [suggestions, setSuggestions] = useState([]);
  const [suggestOpen, setSuggestOpen] = useState(false);
  const [cards, setCards] = useState([]);
  const [query, setQuery] = useState(null); // { q, prints }
  const [page, setPage] = useState(1);
  const [hasMore, setHasMore] = useState(false);
  const [total, setTotal] = useState(null);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [searchError, setSearchError] = useState(null);
  const [foil, setFoil] = useState(false);
  const [chip, setChip] = useState('all');
  const [added, setAdded] = useState({}); // card id -> copies added this session
  const [undo, setUndo] = useState(null); // { ids, card, qty }
  const [busyId, setBusyId] = useState(null);
  const [knownSets, setKnownSets] = useState(() => new Set());
  const [setOptions, setSetOptions] = useState([]);
  const [filters, setFilters] = useState(EMPTY_SEARCH_FILTERS);
  const [showFilters, setShowFilters] = useState(false);
  const filterClause = useMemo(() => compileSearchFilters(filters), [filters]);
  const activeFilterCount = countSearchFilters(filters);
  const setF = (key) => (v) => setFilters(f => ({ ...f, [key]: v }));
  const [sheet, setSheet] = useState(null);
  const [zoom, setZoom] = useState(null);
  const inputRef = useRef(null);
  const reqRef = useRef(0);
  const undoTimer = useRef(null);

  useEffect(() => {
    let cancelled = false;
    fetch('/api/sets?lang=en')
      .then(r => (r.ok ? r.json() : []))
      .then(rows => {
        if (cancelled) return;
        setKnownSets(new Set(rows.map(s => String(s.id || '').replace(/^mtg-/, '').toLowerCase()).filter(Boolean)));
        setSetOptions([...rows]
          .sort((a, b) => String(b.release_date || '').localeCompare(String(a.release_date || '')))
          .map(s => {
            const code = String(s.set_code || s.id || '').replace(/^mtg-/, '').toLowerCase();
            return { value: code, label: `${s.name} (${code.toUpperCase()})` };
          })
          .filter(o => o.value));
      })
      .catch(() => {});
    return () => { cancelled = true; };
  }, []);

  const trimmed = text.trim();
  const direct = useMemo(() => parseDirectAdd(trimmed, knownSets), [trimmed, knownSets]);
  const isSyntax = looksLikeSyntax(trimmed);

  // As-you-type suggestions (plain names only).
  useEffect(() => {
    if (trimmed.length < 2 || isSyntax || direct) { setSuggestions([]); return undefined; }
    const ctl = new AbortController();
    const timer = setTimeout(() => {
      fetch(`/api/suggest?q=${encodeURIComponent(trimmed)}`, { signal: ctl.signal })
        .then(r => (r.ok ? r.json() : { suggestions: [] }))
        .then(d => setSuggestions(Array.isArray(d.suggestions) ? d.suggestions : []))
        .catch(() => {});
    }, 150);
    return () => { clearTimeout(timer); ctl.abort(); };
  }, [trimmed, isSyntax, direct]);

  const fetchPage = useCallback(async (q, prints, pageNum) => {
    const limit = prints ? PRINTS_PAGE : PAGE;
    const params = new URLSearchParams({ q, scope: 'internet', lang: 'en', page: String(pageNum), limit: String(limit) });
    const res = await fetch(`/api/search?${params.toString()}`);
    if (!res.ok) {
      const err = await res.json().catch(() => ({}));
      const e = new Error(err.error || t('search.errRequest'));
      e.code = res.status === 400 && err.error === 'INVALID_QUERY' ? 'invalid-query'
        : res.status === 429 ? 'rate-limit' : res.status === 503 ? 'upstream' : null;
      throw e;
    }
    const data = await res.json();
    const reported = parseInt(res.headers.get('X-Total-Count'), 10);
    return { data, total: Number.isFinite(reported) ? reported : null, limit };
  }, [t]);

  const runSearch = useCallback(async (raw, clause = '') => {
    const value = String(raw || '').trim();
    if (!value && !clause) return;
    const reqId = ++reqRef.current;
    setSuggestOpen(false);
    setLoading(true);
    setSearchError(null);
    setChip(c => (c === 'added' ? 'all' : c));
    try {
      let prints = !!value && !looksLikeSyntax(value);
      let q = withFilters(prints ? namesQuery(value) : value, clause);
      let r = await fetchPage(q, prints, 1);
      // Not an exact card name: fall back to a plain name search.
      if (prints && r.data.length === 0) { prints = false; q = withFilters(value, clause); r = await fetchPage(q, false, 1); }
      if (reqId !== reqRef.current) return;
      setQuery({ q, prints });
      setCards(prints ? sortByRelease(r.data) : r.data);
      setTotal(r.total);
      setPage(1);
      setHasMore(r.total != null ? r.data.length < r.total : r.data.length >= r.limit);
    } catch (err) {
      if (reqId !== reqRef.current) return;
      setCards([]);
      setQuery({ q: withFilters(value, clause), prints: false });
      setSearchError(err.code || null);
      if (err.code !== 'invalid-query') showToast(err.message || t('search.errApi'));
    } finally {
      if (reqId === reqRef.current) setLoading(false);
    }
  }, [fetchPage, showToast, t]);

  // Changing a filter re-runs the search (filters alone are a valid query).
  const textRef = useRef('');
  textRef.current = text;
  const firstClause = useRef(true);
  useEffect(() => {
    if (firstClause.current) { firstClause.current = false; return; }
    const typed = textRef.current.trim();
    if (!typed && !filterClause) {
      reqRef.current += 1;
      setQuery(null); setCards([]); setTotal(null); setHasMore(false); setLoading(false);
      return;
    }
    runSearch(typed, filterClause);
  }, [filterClause, runSearch]);

  const loadMore = async () => {
    if (!query || loadingMore) return;
    setLoadingMore(true);
    try {
      const r = await fetchPage(query.q, query.prints, page + 1);
      setCards(prev => {
        const seen = new Set(prev.map(c => c.id));
        const merged = [...prev, ...r.data.filter(c => !seen.has(c.id))];
        if (r.total != null) setHasMore(merged.length < r.total);
        else setHasMore(r.data.length >= r.limit);
        return query.prints ? sortByRelease(merged) : merged;
      });
      if (r.total != null) setTotal(r.total);
      setPage(p => p + 1);
    } catch (err) {
      showToast(err.message || t('search.errApi'));
    } finally {
      setLoadingMore(false);
    }
  };

  const showUndo = (entry) => {
    clearTimeout(undoTimer.current);
    setUndo(entry);
    undoTimer.current = setTimeout(() => setUndo(null), 6000);
  };
  useEffect(() => () => clearTimeout(undoTimer.current), []);

  // POST one stacked row; returns the created row ids (undo deletes exactly these).
  const addCopies = async (card, { qty = 1, printing, condition = 'Near Mint', language = 'English' }) => {
    const res = await fetch('/api/collection', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        card_id: card.id, quantity: qty, condition, printing, language, purchase_price: 0, stackable: true,
      }),
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(data.error || t('search.errAddCard'));
    const ids = Array.isArray(data.ids) && data.ids.length ? data.ids : [data.id];
    setAdded(prev => ({ ...prev, [card.id]: (prev[card.id] || 0) + qty }));
    setCards(prev => adjustOwnedQuantityByName(prev, card, qty));
    showUndo({ ids, card, qty, printing });
    onAddSuccess();
    return ids;
  };

  const quickAdd = async (card) => {
    if (busyId) return;
    setBusyId(card.id);
    try {
      await addCopies(card, { qty: 1, printing: pickPrinting(card, foil ? 'Holofoil' : 'Normal') });
    } catch (err) {
      showToast(err.message || t('search.errAddCardGeneric'));
    } finally {
      setBusyId(null);
    }
  };

  const undoLast = async () => {
    const entry = undo;
    if (!entry) return;
    setUndo(null);
    clearTimeout(undoTimer.current);
    try {
      for (const id of entry.ids) {
        const res = await fetch(`/api/collection/${id}`, { method: 'DELETE' });
        if (!res.ok) throw new Error(t('search.errUndo'));
      }
      setAdded(prev => {
        const n = (prev[entry.card.id] || 0) - entry.qty;
        const next = { ...prev };
        if (n > 0) next[entry.card.id] = n; else delete next[entry.card.id];
        return next;
      });
      setCards(prev => adjustOwnedQuantityByName(prev, entry.card, -entry.qty));
      showToast(t('search.removed', { name: displayName(entry.card) }));
      onAddSuccess();
    } catch (err) {
      showToast(err.message || t('search.errUndoGeneric'));
    }
  };

  // Enter: a direct-add form adds; anything else searches.
  const submit = async (e) => {
    e?.preventDefault();
    if (!trimmed && !filterClause) return;
    if (!direct) { runSearch(trimmed, filterClause); return; }
    setLoading(true);
    try {
      const params = new URLSearchParams({ q: directAddQuery(direct), scope: 'internet', lang: 'en', page: '1', limit: '250' });
      const res = await fetch(`/api/search?${params.toString()}`);
      const matches = res.ok ? await res.json() : [];
      let hit = null;
      if (direct.kind === 'number') {
        const exact = matches.filter(c => String(c.number).toLowerCase() === direct.number);
        hit = exact[0] || (matches.length === 1 ? matches[0] : null);
      } else {
        const wanted = direct.printing || (foil ? 'Holofoil' : 'Normal');
        const sorted = sortByRelease(matches);
        hit = sorted.find(c => pickPrinting(c, wanted) === wanted) || sorted[0] || null;
      }
      if (!hit) {
        showToast(t('addsearch.noDirectMatch', { text: trimmed }));
        setLoading(false);
        return;
      }
      await addCopies(hit, {
        qty: direct.qty,
        printing: pickPrinting(hit, direct.printing || (foil ? 'Holofoil' : 'Normal')),
        condition: direct.condition || 'Near Mint',
        language: direct.language || 'English',
      });
      showToast(t('addsearch.addedDirect', { qty: direct.qty, name: displayName(hit), set: String(hit.set_id || '').toUpperCase(), number: hit.number }));
      setText('');
      // Show what was just added (and its siblings) so the badge is visible.
      setCards(prev => (prev.some(c => c.id === hit.id) ? prev : [hit, ...prev]));
      setQuery(q => q || { q: directAddQuery(direct), prints: true });
    } catch (err) {
      showToast(err.message || t('search.errAddCardGeneric'));
    } finally {
      setLoading(false);
      inputRef.current?.focus();
    }
  };

  const pickSuggestion = (name) => {
    setText(name);
    setSuggestions([]);
    setSuggestOpen(false);
    runSearch(name, filterClause);
  };

  const applySyntaxChip = (q) => {
    const base = text.trim();
    const next = base && !looksLikeSyntax(base) ? q : `${base ? `${base} ` : ''}${q}`;
    setText(next);
    runSearch(next, filterClause);
  };

  const shown = useMemo(() => {
    let list = cards;
    if (chip === 'hasfoil') list = list.filter(hasFoil);
    if (chip === 'added') list = list.filter(c => added[c.id] > 0);
    if (chip === 'cheapest') {
      const pr = (c) => usdPrice(c, foil ? 'Holofoil' : 'Normal') ?? Infinity;
      list = [...list].sort((a, b) => pr(a) - pr(b));
    }
    return list;
  }, [cards, chip, added, foil]);

  const previewText = direct ? (direct.kind === 'number'
    ? t('addsearch.previewNumber', { qty: direct.qty, set: direct.set.toUpperCase(), number: direct.number })
    : t('addsearch.previewName', { qty: direct.qty, name: direct.name, set: direct.set ? ` (${direct.set.toUpperCase()})` : '' }))
    + [direct.printing === 'Holofoil' ? ' · Foil' : '', direct.condition ? ` · ${direct.condition}` : '', direct.language ? ` · ${direct.language}` : ''].join('')
    : null;

  const chips = [
    { key: 'all', label: t('addsearch.chipAll') },
    { key: 'cheapest', label: t('addsearch.chipCheapest') },
    { key: 'hasfoil', label: t('addsearch.chipHasFoil') },
    { key: 'added', label: t('addsearch.chipAdded') },
  ];

  return (
    <div className="addsearch">
      <form className="addsearch-bar" onSubmit={submit}>
        <div className="deck-toolbar-search addsearch-input">
          {isSyntax ? <Braces size={16} /> : direct ? <Zap size={16} /> : <Search size={16} />}
          <input
            ref={inputRef}
            type="search"
            enterKeyHint="search"
            className="input-control"
            placeholder={t('addsearch.placeholder')}
            value={text}
            onChange={(e) => { setText(e.target.value); setSuggestOpen(true); }}
            onFocus={() => setSuggestOpen(true)}
            onBlur={() => setTimeout(() => setSuggestOpen(false), 150)}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            aria-label={t('addsearch.placeholder')}
            style={isSyntax ? { fontFamily: 'var(--font-mono, monospace)' } : undefined}
          />
          {text && (
            <button type="button" className="deck-toolbar-clear" aria-label={t('addsearch.clear')}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => { setText(''); setSuggestions([]); inputRef.current?.focus(); }}>
              <X size={13} />
            </button>
          )}
          {suggestOpen && suggestions.length > 0 && (
            <ul className="addsearch-suggest" role="listbox">
              {suggestions.map(s => (
                <li key={s.id || s.name} role="option" aria-selected="false"
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => pickSuggestion(s.name)}>
                  {s.image_small ? <img src={s.image_small} alt="" loading="lazy" /> : <span className="addsearch-suggest-ph" />}
                  <span className="addsearch-suggest-name">{s.name}</span>
                  <span className="addsearch-suggest-type">{s.type_line}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
        {previewText && (
          <div className="addsearch-preview"><Zap size={12} /> {t('addsearch.enterAdds')} {previewText}</div>
        )}
      </form>

      <div className="deck-toolbar-chips addsearch-chips">
        <div className="addsearch-finish" role="group" aria-label={t('addsearch.finish')}>
          <button type="button" className={!foil ? 'on' : ''} aria-pressed={!foil} onClick={() => setFoil(false)}>{t('addsearch.nonfoil')}</button>
          <button type="button" className={foil ? 'on' : ''} aria-pressed={foil} onClick={() => setFoil(true)}>{t('addsearch.foil')}</button>
        </div>
        {chips.map(c => (
          <button key={c.key} type="button" className={`deck-chip deck-chip-btn ${chip === c.key ? 'on' : ''}`} onClick={() => setChip(c.key)}>
            {c.label}{c.key === 'added' && Object.keys(added).length > 0 ? ` ${Object.values(added).reduce((a, b) => a + b, 0)}` : ''}
          </button>
        ))}
        <button type="button" className={`deck-chip deck-chip-btn ${showFilters || activeFilterCount ? 'on' : ''}`} onClick={() => setShowFilters(s => !s)} aria-expanded={showFilters}>
          <SlidersHorizontal size={13} />
          {t('collection.filters')}
          {activeFilterCount > 0 && <span className="deck-chip-count">{activeFilterCount}</span>}
        </button>
        {SYNTAX_CHIPS.map(c => (
          <button key={c.key} type="button" className="deck-chip deck-chip-btn addsearch-syntax-chip" onClick={() => applySyntaxChip(c.q)} title={c.q}>
            {t(`addsearch.chip.${c.key}`)}
          </button>
        ))}
      </div>

      {showFilters && (
        <div className="glass-panel addsearch-filters" style={{ display: 'flex', flexDirection: 'column', gap: '1rem', margin: '0.5rem 0 0.75rem', padding: '0.75rem 0.9rem' }}>
          <div className="filter-section-title">{t('mtg.sectionCard')}</div>
          <div className="mtg-color-row">
            <Field label={t('mtg.color')}>
              <ColorPicker label={t('mtg.color')} value={filters.colors} onChange={setF('colors')} mode={filters.colorMode} onModeChange={setF('colorMode')} />
            </Field>
            <Field label={t('mtg.identity')}>
              <ColorPicker label={t('mtg.identity')} value={filters.identity} onChange={setF('identity')} withColorless={false} />
            </Field>
          </div>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))', gap: '0.75rem' }}>
            <Field label={t('mtg.cardType')}>
              <MultiSelectDropdown label={t('mtg.cardType')} allLabel={t('collection.allTypes')} value={filters.types} onChange={setF('types')}
                options={CARD_TYPES.filter(v => v !== 'Tribal').map(v => ({ value: v, label: v }))} />
            </Field>
            <Field label={t('mtg.subtype')}>
              <MultiSelectDropdown label={t('mtg.subtype')} allLabel={t('mtg.allSubtypes')} value={filters.subtypes} onChange={setF('subtypes')}
                options={COMMON_SUBTYPES.map(v => ({ value: v, label: v }))} />
            </Field>
            <Field label={t('collection.fManaValue')}>
              <MultiSelectDropdown label={t('collection.fManaValue')} allLabel={t('collection.allManaValues')} value={filters.mv} onChange={setF('mv')}
                options={MANA_VALUES.map(v => ({ value: v, label: v }))} />
            </Field>
            <Field label={t('collection.fRarity')}>
              <MultiSelectDropdown label={t('collection.fRarity')} allLabel={t('collection.allRarities')} value={filters.rarities} onChange={setF('rarities')}
                options={SEARCH_RARITIES.map(v => ({ value: v, label: v[0].toUpperCase() + v.slice(1) }))} />
            </Field>
            <Field label={t('collection.fSet')}>
              <MultiSelectDropdown label={t('collection.fSet')} allLabel={t('collection.allSets')} value={filters.sets} onChange={setF('sets')}
                options={setOptions} />
            </Field>
          </div>
          <div className="filter-section-title">{t('addsearch.sectionPrinting')}</div>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))', gap: '0.75rem' }}>
            <Field label={t('addsearch.finish')}>
              <MultiSelectDropdown label={t('addsearch.finish')} allLabel={t('addsearch.allFinishes')} value={filters.finishes} onChange={setF('finishes')}
                options={SEARCH_FINISHES.map(v => ({ value: v, label: t(`addsearch.finishOpt.${v}`) }))} />
            </Field>
            <Field label={t('card.language')}>
              <MultiSelectDropdown label={t('card.language')} allLabel={t('collection.allLanguages')} value={filters.languages} onChange={setF('languages')}
                options={LANGUAGE_TABLE.map(l => ({ value: l.scryfall || l.code, label: l.name }))} />
            </Field>
            <Field label={t('collection.fMinPrice')}>
              <input type="number" min="0" inputMode="decimal" className="input-control" placeholder={t('collection.minPricePlaceholder')} value={filters.minUsd} onChange={(e) => setF('minUsd')(e.target.value)} />
            </Field>
            <Field label={t('collection.fMaxPrice')}>
              <input type="number" min="0" inputMode="decimal" className="input-control" placeholder={t('collection.maxPricePlaceholder')} value={filters.maxUsd} onChange={(e) => setF('maxUsd')(e.target.value)} />
            </Field>
          </div>
          {activeFilterCount > 0 && (
            <div style={{ display: 'flex', borderTop: '1px solid var(--border-glass)', paddingTop: '0.75rem' }}>
              <button type="button" className="btn btn-secondary" onClick={() => setFilters(EMPTY_SEARCH_FILTERS)} style={{ marginLeft: 'auto', fontSize: '0.72rem', padding: '0.3rem 0.7rem', display: 'inline-flex', alignItems: 'center', gap: '0.3rem' }}>
                <X size={13} /> {t('collection.clearFilters')}
              </button>
            </div>
          )}
        </div>
      )}

      {searchError && (
        <div className="addsearch-error">
          <ShieldAlert size={16} />
          <div><strong>{t(`searchErr.${searchError}.title`)}</strong> {t(`searchErr.${searchError}.body`)}</div>
        </div>
      )}

      {query && !loading && (
        <div className="addsearch-count">
          {t('addsearch.count', { shown: shown.length, total: total != null ? total : cards.length })}
        </div>
      )}

      {loading && <div className="spinner"></div>}

      {!loading && shown.length > 0 && (
        <div className="card-grid addsearch-grid">
          {shown.map(card => {
            const n = added[card.id] || 0;
            const wanted = foil ? 'Holofoil' : 'Normal';
            return (
              <div key={card.id} className="tcg-card addsearch-tile">
                <div className="tcg-card-inner" role="button" tabIndex={0}
                  onClick={() => setSheet({ card, qty: 1, printing: pickPrinting(card, wanted), condition: 'Near Mint', language: 'English', more: false })}
                  onKeyDown={(e) => { if (e.key === 'Enter') setSheet({ card, qty: 1, printing: pickPrinting(card, wanted), condition: 'Near Mint', language: 'English', more: false }); }}>
                  <CardImage card={card} className="tcg-card-image" loading="lazy" draggable={false} />
                  {card.owned_qty > 0 && (
                    <div className="addsearch-owned"><Check size={10} /> {card.owned_qty}</div>
                  )}
                  {n > 0 && <div className="addsearch-added">+{n}</div>}
                  <button type="button" className="addsearch-plus" aria-label={t('addsearch.addOne', { name: displayName(card) })}
                    disabled={busyId === card.id} onClick={(e) => { e.stopPropagation(); quickAdd(card); }}
                    onKeyDown={(e) => e.stopPropagation()}>
                    <Plus size={20} />
                  </button>
                </div>
                <div className="tcg-card-info">
                  <div className="tcg-card-name">{displayName(card)}</div>
                  <div className="tcg-card-meta">
                    <span className="addsearch-set">{String(card.set_id || '').toUpperCase()} #{card.number}</span>
                    <span className="tcg-card-price">{priceLabel(card, pickPrinting(card, wanted))}</span>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {!loading && hasMore && cards.length > 0 && chip !== 'added' && (
        <LoadMore busy={loadingMore} onLoad={loadMore}>
          {loadingMore ? t('common.loading') : t('addsearch.loadMore')}
        </LoadMore>
      )}

      {!loading && query && !searchError && shown.length === 0 && (
        <div className="addsearch-empty">{chip === 'all' ? t('search.noQueryMatches') : t('search.noFilterMatches')}</div>
      )}

      {sheet && (
        <AddSheet
          sheet={sheet}
          setSheet={setSheet}
          onZoom={setZoom}
          onAdd={async () => {
            try {
              await addCopies(sheet.card, { qty: sheet.qty, printing: sheet.printing, condition: sheet.condition, language: sheet.language });
              showToast(t('search.addedToCollection', { name: displayName(sheet.card) }));
              setSheet(null);
            } catch (err) {
              showToast(err.message || t('search.errAddCardGeneric'));
            }
          }}
        />
      )}

      {zoom && <CardImageZoom card={zoom} onClose={() => setZoom(null)} />}

      {undo && (
        <div className="addsearch-undo" role="status">
          <span>{t('addsearch.addedToast', { qty: undo.qty, name: displayName(undo.card) })}</span>
          <button type="button" onClick={undoLast}><Undo2 size={14} /> {t('search.undo')}</button>
        </div>
      )}
    </div>
  );
}

// Compact add sheet: big card, printing strip with prices, qty stepper,
// Nonfoil/Foil, condition/language behind a small "change" disclosure.
function AddSheet({ sheet, setSheet, onAdd, onZoom }) {
  const { t } = useT();
  const [printings, setPrintings] = useState([sheet.card]);
  const [adding, setAdding] = useState(false);
  const name = sheet.card.name;
  useEffect(() => {
    let cancelled = false;
    const params = new URLSearchParams({ q: `${namesQuery(name)} -is:digital`, scope: 'internet', lang: 'en', page: '1', limit: '250' });
    fetch(`/api/search?${params.toString()}`)
      .then(r => (r.ok ? r.json() : []))
      .then(rows => { if (!cancelled && rows.length) setPrintings(sortByRelease(rows)); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [name]);
  const card = sheet.card;
  const update = (patch) => setSheet(s => ({ ...s, ...patch }));
  const finishes = Array.isArray(card.finishes) ? card.finishes : ['nonfoil', 'foil'];
  const canNonfoil = finishes.includes('nonfoil');
  const canFoil = finishes.includes('foil') || finishes.includes('etched');

  return (
    <>
      <div className="drawer-backdrop open" onClick={() => setSheet(null)}></div>
      <div className="quick-add-drawer open addsheet" role="dialog" aria-label={t('search.addCardTitle')}>
        <div className="addsheet-head">
          <div className="addsheet-title">
            <strong>{displayName(card)}</strong>
            <span>{card.set_name} · {String(card.set_id || '').toUpperCase()} #{card.number}</span>
          </div>
          <button className="btn btn-secondary btn-icon-only" onClick={() => setSheet(null)} aria-label={t('common.cancel')} style={{ borderRadius: '50%' }}><X size={18} /></button>
        </div>
        <div className="addsheet-card" onClick={() => onZoom(card)}>
          <CardImage card={card} draggable={false} />
        </div>
        <div className="addsheet-strip">
          {printings.map(p => (
            <button key={p.id} type="button" className={`addsheet-print ${p.id === card.id ? 'on' : ''}`}
              onClick={() => update({ card: p, printing: pickPrinting(p, sheet.printing) })}>
              <CardImage card={p} loading="lazy" draggable={false} />
              <span>{String(p.set_id || '').toUpperCase()}</span>
              <span className="addsheet-print-price">{priceLabel(p, pickPrinting(p, sheet.printing))}</span>
            </button>
          ))}
        </div>
        <div className="addsheet-row">
          <div className="addsheet-stepper">
            <button type="button" aria-label="-" onClick={() => update({ qty: Math.max(1, sheet.qty - 1) })}><Minus size={16} /></button>
            <span>{sheet.qty}</span>
            <button type="button" aria-label="+" onClick={() => update({ qty: Math.min(99, sheet.qty + 1) })}><Plus size={16} /></button>
          </div>
          <div className="addsearch-finish">
            <button type="button" disabled={!canNonfoil} className={sheet.printing === 'Normal' ? 'on' : ''} onClick={() => update({ printing: 'Normal' })}>{t('addsearch.nonfoil')}</button>
            <button type="button" disabled={!canFoil} className={sheet.printing === 'Holofoil' ? 'on' : ''} onClick={() => update({ printing: 'Holofoil' })}>{t('addsearch.foil')}</button>
          </div>
          <span className="addsheet-price">{priceLabel(card, sheet.printing)}</span>
        </div>
        <div className="addsheet-more">
          <span>{sheet.condition} · {sheet.language}</span>
          <button type="button" className="addsheet-change" onClick={() => update({ more: !sheet.more })}>{t('addsearch.change')}</button>
        </div>
        {sheet.more && (
          <div className="addsheet-fields">
            <select className="select-control" value={sheet.condition} onChange={(e) => update({ condition: e.target.value })} aria-label={t('addsearch.condition')}>
              {CONDITIONS.map(c => <option key={c} value={c}>{c}</option>)}
            </select>
            <select className="select-control" value={sheet.language} onChange={(e) => update({ language: e.target.value })} aria-label={t('search.language')}>
              {LANGUAGES.map(l => <option key={l} value={l}>{l}</option>)}
            </select>
          </div>
        )}
        <button type="button" className="btn btn-primary addsheet-add" disabled={adding}
          onClick={async () => { setAdding(true); await onAdd(); setAdding(false); }}>
          {t('addsearch.addN', { qty: sheet.qty })}
        </button>
      </div>
    </>
  );
}

export default CardSearch;
