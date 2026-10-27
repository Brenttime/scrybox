import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ChevronLeft, Database, Search, X } from 'lucide-react';
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

export default function ScryfallPortal({ onNavigate, showToast }) {
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
            <form onSubmit={submit} className="sfp-searchbox">
              <div className="deck-toolbar-search" style={{ position: 'relative' }}>
                <Search size={16} className="sfp-search-ic" />
                <input className="form-input sfp-input" type="search" enterKeyHint="search" value={q}
                  onChange={e => setQ(e.target.value)} placeholder={t('portal.placeholder')} aria-label={t('portal.search')} />
                {q && <button type="button" className="sfp-clear" aria-label={t('common.close')} onClick={() => { setQ(''); setSuggestions([]); }}><X size={14} /></button>}
                {suggestions.length > 0 && (
                  <div className="sfp-suggest glass-panel">
                    {suggestions.map(s => (
                      <button key={s} type="button" onClick={() => { setQ(s); run(`!"${s}" unique:prints`); }}>{s}</button>
                    ))}
                  </div>
                )}
              </div>
              <div className="deck-toolbar-chips sfp-chips">
                {CHIPS.map(c => (
                  <button key={c} type="button" className="deck-chip-btn" onClick={() => { setQ(c); run(c); }}>{c}</button>
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
          <div className="deck-toolbar-search" style={{ position: 'relative' }}>
            <Search size={16} className="sfp-search-ic" />
            <input className="form-input sfp-input" type="search" value={setFilter} onChange={e => setSetFilter(e.target.value)} placeholder={t('portal.filterSets')} aria-label={t('portal.filterSets')} />
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
          <CardInspectorModal card={{ ...inspect, card_id: inspect.id, quantity: inspect.owned_qty || 0 }} readOnly onClose={() => setInspect(null)} showToast={showToast} />
        </Suspense>
      )}
    </div>
  );
}
