// Search WOTC preconstructed decks by name and import one as a deck.
//
// This is the fast path inside the unified Add Deck flow. The modal lists ranked
// matches (name + set + type + release date); picking one imports the
// product's exact card list — mainboard plus commander — as a new deck, each
// card resolved to the printing the product actually ships, so the "export
// what's missing" math afterwards is honest.
import { useState, useRef } from 'react';
import { X, Search, PackageOpen, Loader2 } from 'lucide-react';
import { useT } from '../utils/i18n';
import { useBackGuard } from '../utils/useBackGuard';
import { useDialogFocus } from '../utils/useDialogFocus';

const TYPE_LABEL = {
  'Commander Deck': 'Commander',
  'MTGO Commander Deck': 'Commander',
  'Oathbreaker Deck': 'Oathbreaker',
  'Jumpstart': 'Jumpstart',
  'Intro Pack': 'Intro Pack',
  'Theme Deck': 'Theme Deck',
  'Welcome Deck': 'Welcome Deck',
  'Duel Deck': 'Duel Deck',
  'Event Deck': 'Event Deck',
  'Challenger Deck': 'Challenger Deck',
  'Pioneer Challenger Deck': 'Challenger',
  'World Championship Deck': 'Pro Tour',
  'Pro Tour Deck': 'Pro Tour',
  'Box Set': 'Box Set',
  'Planeswalker Deck': 'Planeswalker',
  'Secret Lair Drop': 'Secret Lair',
  'Arena Starter Deck': 'Arena Starter',
  'MTGO Redemption': 'MTGO',
  "Deck Builder's Toolkit": 'Toolkit',
  'Sample Deck': 'Sample Deck',
  'Starter Deck': 'Starter Deck',
  'Enhanced Deck': 'Enhanced',
  'Advanced Deck': 'Advanced',
  'Clash Pack': 'Clash',
  'Halfdeck': 'Halfdeck',
};

export default function PreconSearchModal({ open, onClose, onImported, showToast, returnFocusRef }) {
  const { t } = useT();
  const [query, setQuery] = useState('');
  const [results, setResults] = useState([]);
  const [searching, setSearching] = useState(false);
  const [stale, setStale] = useState(false);
  const [error, setError] = useState('');
  const [importing, setImporting] = useState(null); // the fileName being imported
  const inputRef = useRef(null);
  const dialogRef = useRef(null);

  useBackGuard(open, () => { if (!importing) onClose(); });
  useDialogFocus({
    isOpen: open,
    containerRef: dialogRef,
    initialFocusRef: inputRef,
    returnFocusRef,
    onEscape: () => { if (!importing) onClose(); },
  });

  if (!open) return null;

  const runSearch = async (q) => {
    const term = (q || '').trim();
    if (term.length < 2) { setResults([]); return; }
    setSearching(true);
    setError('');
    try {
      const res = await fetch(`/api/precons?q=${encodeURIComponent(term)}`);
      const body = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(body.error || t('precon.searchFailed'));
      setResults(body.results || []);
      setStale(Boolean(body.stale));
    } catch (err) {
      setError(err.message);
      setResults([]);
    } finally {
      setSearching(false);
    }
  };

  const handleImport = async (deck) => {
    if (importing) return;
    setImporting(deck.fileName);
    try {
      const res = await fetch('/api/precons/import', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ fileName: deck.fileName, name: deck.name }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(body.error || t('precon.importFailed'));
      showToast(body.notFound
        ? t('precon.importedSome', { count: body.notFound })
        : t('precon.added', { name: deck.name }));
      onImported(body.id);
    } catch (err) {
      setError(err.message);
    } finally {
      setImporting(null);
    }
  };

  const yearOf = (d) => (d ? String(d).slice(0, 4) : '');

  return (
    <div
      className="modal-overlay"
      style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.85)', backdropFilter: 'blur(5px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}
      onClick={(e) => { if (e.target === e.currentTarget && !importing) onClose(); }}
    >
      <div ref={dialogRef} className="glass-panel" tabIndex={-1} role="dialog" aria-modal="true" aria-labelledby="precon-search-title" aria-describedby="precon-search-subtitle precon-fast-path-body" style={{ width: '100%', maxWidth: '680px', maxHeight: '88vh', display: 'flex', flexDirection: 'column', padding: '1.5rem', position: 'relative' }}>
        <button
          className="btn btn-secondary btn-icon-only modal-close"
          onClick={onClose}
          disabled={Boolean(importing)}
          style={{ position: 'absolute', top: '0.9rem', right: '0.9rem', borderRadius: '50%', opacity: importing ? 0.4 : 1 }}
          aria-label={t('common.cancel')}
        >
          <X size={16} />
        </button>

        <h3 id="precon-search-title" style={{ fontSize: '1.15rem', color: 'var(--text-strong)', margin: 0, display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
          <PackageOpen size={18} style={{ color: 'var(--accent-yellow)' }} /> {t('precon.title')}
        </h3>
        <p id="precon-search-subtitle" style={{ color: 'var(--text-secondary)', fontSize: '0.8rem', margin: '0.3rem 0 0.9rem 0' }}>
          {t('precon.subtitle')}
        </p>

        <div className="precon-fast-path-note">
          <PackageOpen size={16} />
          <div>
            <strong>{t('precon.fastPathTitle')}</strong>
            <span id="precon-fast-path-body">{t('precon.fastPathBody')}</span>
          </div>
        </div>

        <div style={{ display: 'flex', gap: '0.5rem', marginBottom: '0.75rem' }}>
          <div style={{ position: 'relative', flex: 1 }}>
            <Search size={15} style={{ position: 'absolute', left: '0.7rem', top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
            <input
              ref={inputRef}
              type="text"
              className="input-control"
              placeholder={t('precon.searchPlaceholder')}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') runSearch(query); }}
              style={{ paddingLeft: '2.1rem', width: '100%' }}
            />
          </div>
          <button
            className="btn btn-secondary"
            style={{ padding: '0.5rem 0.9rem', fontWeight: 600, opacity: importing ? 0.5 : 1 }}
            disabled={importing !== null || query.trim().length < 2}
            onClick={() => runSearch(query)}
          >
            {t('precon.search')}
          </button>
        </div>

        <div style={{ flex: 1, overflowY: 'auto', minHeight: '240px', display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
          {searching && (
            <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', color: 'var(--text-secondary)', fontSize: '0.85rem', padding: '1rem 0' }}>
              <Loader2 size={16} style={{ animation: 'spin 1s linear infinite' }} /> {t('precon.searching')}
            </div>
          )}
          {error && !searching && (
            <div style={{ color: '#f87171', fontSize: '0.85rem', padding: '0.75rem', background: 'rgba(248,113,113,0.08)', border: '1px solid rgba(248,113,113,0.25)', borderRadius: '8px' }}>
              {error}
            </div>
          )}
          {!searching && !error && results.length === 0 && (
            <div style={{ color: 'var(--text-muted)', fontSize: '0.85rem', textAlign: 'center', padding: '2rem 0' }}>
              {t('precon.noResults')}
            </div>
          )}
          {!searching && !error && results.map((d) => (
            <div
              key={d.fileName}
              style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: '0.75rem', padding: '0.7rem 0.85rem', border: '1px solid var(--border-glass)', borderRadius: '10px', background: 'rgba(255,255,255,0.02)' }}
            >
              <div style={{ minWidth: 0, flex: 1 }}>
                <div title={d.name} style={{ fontSize: '0.95rem', fontWeight: 700, color: 'var(--text-strong)', lineHeight: 1.3, overflowWrap: 'anywhere' }}>
                  {d.name}
                </div>
                <div style={{ fontSize: '0.72rem', color: 'var(--text-secondary)', marginTop: '2px' }}>
                  {[
                    TYPE_LABEL[d.type] || d.type,
                    d.setName ? `${d.setName} (${d.code})` : d.code,
                    yearOf(d.releaseDate),
                  ].filter(Boolean).join(' · ')}
                </div>
              </div>
              <button
                className="btn btn-primary"
                style={{ padding: '0.35rem 0.8rem', fontSize: '0.78rem', fontWeight: 700, flexShrink: 0, display: 'flex', alignItems: 'center', gap: '0.35rem', opacity: importing ? 0.5 : 1 }}
                disabled={importing !== null}
                onClick={() => handleImport(d)}
              >
                {importing === d.fileName
                  ? <Loader2 size={13} style={{ animation: 'spin 1s linear infinite' }} />
                  : <PackageOpen size={13} />}
                {t('precon.import')}
              </button>
            </div>
          ))}
        </div>

        {stale && !searching && (
          <div style={{ fontSize: '0.7rem', color: 'var(--text-muted)', marginTop: '0.6rem', textAlign: 'center' }}>
            {t('precon.staleNote')}
          </div>
        )}
      </div>
    </div>
  );
}
