import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ChevronLeft, Database, Minus, Plus, Search, Undo2, X } from 'lucide-react';
import { pickPrinting } from '../utils/directAdd';
import { useT } from '../utils/i18n';
import { useSwipeBack } from '../utils/useSwipeBack';
import CardImage from './CardImage';

const CardInspectorModal = lazy(() => import('./CardInspectorModal'));

// Local Scryfall as a native Scrybox tool (shown when LOCAL_WEB_PORTAL_ENABLED):
// Search (full Scryfall syntax against the local engine via /api/search
// scope=internet) and Sets (newest first; tap a set to browse its cards).
// Swipe back: set browse -> Sets, otherwise -> Dashboard.
const PAGE = 60;
const CHIPS = ['t:legendary t:creature', 'is:commander', 'r:mythic', 'otag:ramp', 'is:foil', 'year>=2025'];
const setCode = (s) => (s.set_code || String(s.id || '').replace(/^mtg-/, '')).toLowerCase();
const numKey = (n) => { const m = String(n || '').match(/^(\d+)(.*)$/); return m ? [Number(m[1]), m[2]] : [1e9, String(n || '')]; };

// Add-to-collection controls for the card details sheet: qty stepper, a
// Foil / Non-foil chip (only when the printing has both), Add, then Undo for
// the rows just created. NM / English, one stacked row like Add Cards' +.
function AddToCollection({ card, showToast, onAdded }) {
  const { t } = useT();
  const f = Array.isArray(card.finishes) ? card.finishes : [];
  const both = f.includes('nonfoil') && (f.includes('foil') || f.includes('etched'));
  const [qty, setQty] = useState(1);
  const [foil, setFoil] = useState(() => pickPrinting(card, 'Normal') === 'Holofoil');
  const [busy, setBusy] = useState(false);
  const [last, setLast] = useState(null);
  const printing = foil ? 'Holofoil' : 'Normal';

  const add = async () => {
    setBusy(true);
    try {
      const res = await fetch('/api/collection', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ card_id: card.id, quantity: qty, condition: 'Near Mint', printing, language: 'English', purchase_price: 0, stackable: true }),
      });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(data.error || t('search.errAddCard'));
      const ids = Array.isArray(data.ids) && data.ids.length ? data.ids : [data.id];
      setLast({ ids, qty });
      onAdded(qty);
      showToast?.(t('portal.added', { count: qty, name: card.name }));
    } catch (e) { showToast?.(e.message); } finally { setBusy(false); }
  };

  const undo = async () => {
    if (!last) return;
    setBusy(true);
    try {
      for (const id of last.ids) {
        const res = await fetch(`/api/collection/${id}`, { method: 'DELETE' });
        if (!res.ok) throw new Error(t('search.errUndo'));
      }
      onAdded(-last.qty);
      setLast(null);
    } catch (e) { showToast?.(e.message); } finally { setBusy(false); }
  };

  return (
    <div className="sfp-add">
      <div className="sfp-add-row">
        <div className="sfp-stepper">
          <button type="button" aria-label="-" disabled={qty <= 1 || busy} onClick={() => setQty(n => Math.max(1, n - 1))}><Minus size={14} /></button>
          <span>{qty}</span>
          <button type="button" aria-label="+" disabled={busy} onClick={() => setQty(n => Math.min(99, n + 1))}><Plus size={14} /></button>
        </div>
        {both && (
          <button type="button" className={`deck-chip deck-chip-btn${foil ? ' on' : ''}`} disabled={busy} onClick={() => setFoil(v => !v)}>
            {foil ? t('portal.foil') : t('portal.nonfoil')}
          </button>
        )}
        <button type="button" className="btn btn-primary sfp-add-btn" disabled={busy} onClick={add}>
          <Plus size={16} /> {t('portal.addToCollection')}
        </button>
      </div>
      {last && (
        <button type="button" className="sfp-undo" disabled={busy} onClick={undo}>
          <Undo2 size={13} /> {t('portal.undoAdd', { count: last.qty })}
        </button>
      )}
    </div>
  );
}

export default function ScryfallPortal({ onNavigate, showToast, onUpdate }) {
  const { t } = useT();
  const [mode, setMode] = useState('search');
  const [q, setQ] = useState('');
  const [suggestions, setSuggestions] = useState([]);
  const [results, setResults] = useState([]);
  const [total, setTotal] = useState(null);
  const [page, setPage] = useState(1);
  const [ran, setRan] = useState('');
  const [fromSet, setFromSet] = useState(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [sets, setSets] = useState(null);
  const [setFilter, setSetFilter] = useState('');
  const [inspect, setInspect] = useState(null);
  const reqRef = useRef(0);

  const back = useCallback(() => {
    if (fromSet) { setFromSet(null); setMode('sets'); return; }
    onNavigate?.('dashboard');
  }, [fromSet, onNavigate]);
  useSwipeBack(!inspect, back);

  const fetchPage = useCallback(async (query, pageNum) => {
    const params = new URLSearchParams({ q: query, scope: 'internet', lang: 'en', page: String(pageNum), limit: String(PAGE) });
    const res = await fetch(`/api/search?${params.toString()}`);
    if (!res.ok) {
      const err = await res.json().catch(() => ({}));
      throw new Error(err.error === 'INVALID_QUERY' ? t('portal.badQuery') : (err.error || t('portal.error')));
    }
    const data = await res.json();
    const reported = parseInt(res.headers.get('X-Total-Count'), 10);
    return { data: Array.isArray(data) ? data : [], total: Number.isFinite(reported) ? reported : null };
  }, [t]);

  const run = useCallback(async (raw, set = null) => {
    const query = String(raw || '').trim();
    if (!query) return;
    const id = ++reqRef.current;
    setMode('search'); setFromSet(set); setRan(query); setSuggestions([]);
    setLoading(true); setError(''); setResults([]); setTotal(null); setPage(1);
    try {
      const r = await fetchPage(query, 1);
      if (id !== reqRef.current) return;
      setResults(set ? [...r.data].sort((a, b) => { const x = numKey(a.number), y = numKey(b.number); return x[0] - y[0] || x[1].localeCompare(y[1]); }) : r.data);
      setTotal(r.total);
    } catch (e) {
      if (id === reqRef.current) setError(e.message);
    } finally {
      if (id === reqRef.current) setLoading(false);
    }
  }, [fetchPage]);

  const more = async () => {
    const id = reqRef.current;
    setLoading(true);
    try {
      const r = await fetchPage(ran, page + 1);
      if (id !== reqRef.current) return;
      setResults(prev => {
        const seen = new Set(prev.map(c => c.id));
        const next = [...prev, ...r.data.filter(c => !seen.has(c.id))];
        return fromSet ? next.sort((a, b) => { const x = numKey(a.number), y = numKey(b.number); return x[0] - y[0] || x[1].localeCompare(y[1]); }) : next;
      });
      setPage(p => p + 1);
    } catch (e) { showToast?.(e.message); } finally { setLoading(false); }
  };

  // Name suggestions as you type (plain words only, not syntax).
  const trimmed = q.trim();
  useEffect(() => {
    if (trimmed.length < 2 || /[:<>=!]/.test(trimmed) || trimmed === ran) { setSuggestions([]); return undefined; }
    const ctl = new AbortController();
    const timer = setTimeout(() => {
      fetch(`/api/suggest?q=${encodeURIComponent(trimmed)}`, { signal: ctl.signal })
        .then(r => (r.ok ? r.json() : { suggestions: [] }))
        .then(d => setSuggestions(Array.isArray(d.suggestions) ? d.suggestions.slice(0, 6) : []))
        .catch(() => {});
    }, 150);
    return () => { clearTimeout(timer); ctl.abort(); };
  }, [trimmed, ran]);

  useEffect(() => {
    if (mode !== 'sets' || sets) return;
    fetch('/api/sets?lang=en').then(r => (r.ok ? r.json() : [])).then(d => setSets(Array.isArray(d) ? d : [])).catch(() => setSets([]));
  }, [mode, sets]);

  const shownSets = useMemo(() => {
    if (!sets) return [];
    const f = setFilter.trim().toLowerCase();
    return [...sets]
      .filter(s => !f || s.name?.toLowerCase().includes(f) || setCode(s).includes(f))
      .sort((a, b) => String(b.release_date || '').localeCompare(String(a.release_date || '')));
  }, [sets, setFilter]);

  const openSet = (s) => { setQ(`s:${setCode(s)}`); run(`s:${setCode(s)} unique:prints`, s); };
  // Owned counts are per card name (as /api/search reports them): bump every
  // printing of that name in the grid and the open sheet.
  const bumpOwned = (n) => {
    const name = inspect?.name;
    const adj = c => (c.name === name ? { ...c, owned_qty: Math.max(0, (c.owned_qty || 0) + n) } : c);
    setResults(prev => prev.map(adj));
    setInspect(c => (c ? adj(c) : c));
    onUpdate?.();
  };
  const submit = (e) => { e.preventDefault(); run(q); };
  const title = fromSet ? fromSet.name : t('portal.title');

  return (
    <div className="sfp-view">
      <div className="sfp-head">
        <button type="button" className="btn btn-secondary" onClick={back} aria-label={t('common.back')} style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
          <ChevronLeft size={16} /> {t('common.back')}
        </button>
        <h2 className="sfp-title">
          {fromSet?.symbol_url ? <img src={fromSet.symbol_url} alt="" className="sfp-set-ic" /> : <Database size={20} style={{ color: 'var(--accent-yellow)' }} />}
          <span>{title}</span>
        </h2>
      </div>

      {!fromSet && (
        <div className="sfp-seg" role="tablist">
          {['search', 'sets'].map(m => (
            <button key={m} type="button" role="tab" aria-selected={mode === m} className={`sfp-seg-btn${mode === m ? ' on' : ''}`} onClick={() => setMode(m)}>
              {t(m === 'search' ? 'portal.search' : 'portal.sets')}
            </button>
          ))}
        </div>
      )}

      {mode === 'search' && (
        <>
          {!fromSet && (
            <form onSubmit={submit} className="addsearch-bar sfp-searchbox">
              <div className="deck-toolbar-search addsearch-input">
                <Search size={16} />
                <input className="input-control" type="search" enterKeyHint="search" value={q}
                  onChange={e => setQ(e.target.value)} placeholder={t('portal.placeholder')} aria-label={t('portal.search')}
                  autoCapitalize="off" autoCorrect="off" spellCheck={false}
                  style={/[:<>=]/.test(q) ? { fontFamily: 'var(--font-mono, monospace)' } : undefined} />
                {q && (
                  <button type="button" className="deck-toolbar-clear" aria-label={t('common.close')}
                    onMouseDown={e => e.preventDefault()} onClick={() => { setQ(''); setSuggestions([]); }}>
                    <X size={13} />
                  </button>
                )}
                {suggestions.length > 0 && (
                  <ul className="addsearch-suggest" role="listbox">
                    {suggestions.map(s => (
                      <li key={s.id || s.name} role="option" aria-selected="false"
                        onMouseDown={e => e.preventDefault()}
                        onClick={() => { setQ(s.name); run(`!"${s.name}" unique:prints`); }}>
                        {s.image_small ? <img src={s.image_small} alt="" loading="lazy" /> : <span className="addsearch-suggest-ph" />}
                        <span className="addsearch-suggest-name">{s.name}</span>
                        <span className="addsearch-suggest-type">{s.type_line}</span>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
              <div className="deck-toolbar-chips addsearch-chips">
                {CHIPS.map(c => (
                  <button key={c} type="button" className={`deck-chip deck-chip-btn${ran === c ? ' on' : ''}`} onClick={() => { setQ(c); run(c); }}>{c}</button>
                ))}
              </div>
            </form>
          )}

          {error && <div className="sfp-msg">{error}</div>}
          {!error && ran && !loading && results.length === 0 && <div className="sfp-msg">{t('portal.none')}</div>}
          {!ran && !fromSet && <div className="sfp-msg dim">{t('portal.hint')}</div>}
          {ran && total != null && results.length > 0 && <div className="sfp-count">{t('portal.count', { count: total })}</div>}

          {results.length > 0 && (
            <div className="card-grid sfp-grid">
              {results.map(c => (
                <div key={c.id} className="tcg-card sfp-tile">
                  <div className="tcg-card-inner" role="button" tabIndex={0}
                    onClick={() => setInspect(c)} onKeyDown={e => { if (e.key === 'Enter') setInspect(c); }}>
                    <CardImage card={c} className="tcg-card-image" loading="lazy" draggable={false} />
                    {c.owned_qty > 0 && <div className="addsearch-owned">{c.owned_qty}</div>}
                  </div>
                </div>
              ))}
            </div>
          )}
          {loading && <div className="sfp-msg dim">{t('portal.loading')}</div>}
          {!loading && total != null && results.length < total && (
            <button type="button" className="btn btn-secondary sfp-more" onClick={more}>{t('portal.more')}</button>
          )}
        </>
      )}

      {mode === 'sets' && (
        <>
          <div className="deck-toolbar-search addsearch-input">
            <Search size={16} />
            <input className="input-control" type="search" value={setFilter} onChange={e => setSetFilter(e.target.value)} placeholder={t('portal.filterSets')} aria-label={t('portal.filterSets')} />
          </div>
          {!sets && <div className="sfp-msg dim">{t('portal.loading')}</div>}
          <div className="sfp-sets">
            {shownSets.map(s => (
              <button key={s.id} type="button" className="sfp-set" onClick={() => openSet(s)}>
                {s.symbol_url ? <img src={s.symbol_url} alt="" className="sfp-set-ic" loading="lazy" /> : <span className="sfp-set-ic" />}
                <span className="sfp-set-name">{s.name}<span className="dim"> · {setCode(s).toUpperCase()}</span></span>
                <span className="sfp-set-meta">{String(s.release_date || '').slice(0, 4)}{s.total ? ` · ${s.total}` : ''}</span>
              </button>
            ))}
          </div>
        </>
      )}

      {inspect && (
        <Suspense fallback={null}>
          <CardInspectorModal card={{ ...inspect, card_id: inspect.id, quantity: inspect.owned_qty || 0 }} readOnly onClose={() => setInspect(null)} showToast={showToast}>
            <AddToCollection key={inspect.id} card={inspect} showToast={showToast} onAdded={bumpOwned} />
          </CardInspectorModal>
        </Suspense>
      )}
    </div>
  );
}
