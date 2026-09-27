import { useEffect, useMemo, useState } from 'react';
import { ChevronDown, ChevronLeft, Search, Sparkles, X } from 'lucide-react';
import { useT } from '../utils/i18n';

// Keyword glossary: every keyword ability (CR 702) and keyword action (CR 701)
// with the reminder text printed on cards and the full rules underneath.
// Data: GET /api/rules/keywords (reminders mined from the local card cache).
export default function Keywords({ onNavigate }) {
  const { t } = useT();
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  const [q, setQ] = useState('');
  const [kind, setKind] = useState('all');
  const [open, setOpen] = useState(null);

  useEffect(() => {
    fetch('/api/rules/keywords').then(async r => {
      const j = await r.json();
      if (!r.ok) throw new Error(j.error || r.statusText);
      setData(j);
    }).catch(e => setError(e.message));
  }, []);

  const list = useMemo(() => {
    if (!data) return [];
    const needle = q.trim().toLowerCase();
    return data.keywords.filter(k => (kind === 'all' || k.kind === kind) && (!needle
      || k.name.toLowerCase().includes(needle)
      || (k.reminder || '').toLowerCase().includes(needle)
      || k.summary.toLowerCase().includes(needle)));
  }, [data, q, kind]);

  // Name matches first, then text matches.
  const sorted = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return list;
    const rank = k => (k.name.toLowerCase().startsWith(needle) ? 0 : k.name.toLowerCase().includes(needle) ? 1 : 2);
    return [...list].sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
  }, [list, q]);

  return (
    <div className="keywords-view">
      <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', marginBottom: '1rem' }}>
        <button type="button" className="btn btn-secondary btn-icon-only" onClick={() => onNavigate && onNavigate('dashboard')} aria-label={t('common.back')}>
          <ChevronLeft size={18} />
        </button>
        <h2 style={{ margin: 0, color: 'var(--text-strong)', display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
          <Sparkles size={20} /> {t('kw.title')}
        </h2>
      </div>

      <div className="glass-panel" style={{ padding: '0.75rem', marginBottom: '0.75rem' }}>
        <div style={{ position: 'relative' }}>
          <Search size={16} style={{ position: 'absolute', left: 12, top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
          <input
            className="form-input"
            type="search"
            value={q}
            onChange={e => setQ(e.target.value)}
            placeholder={t('kw.placeholder')}
            style={{ paddingLeft: 36, paddingRight: q ? 36 : undefined, width: '100%' }}
            autoCapitalize="none"
            autoCorrect="off"
          />
          {q && (
            <button type="button" onClick={() => setQ('')} aria-label={t('rules.clear')}
              style={{ position: 'absolute', right: 8, top: '50%', transform: 'translateY(-50%)', background: 'none', border: 0, color: 'var(--text-muted)', cursor: 'pointer', padding: 4 }}>
              <X size={16} />
            </button>
          )}
        </div>
        <div className="kw-chips">
          {['all', 'ability', 'action'].map(k => (
            <button key={k} type="button" className={`kw-chip${kind === k ? ' active' : ''}`} onClick={() => setKind(k)}>
              {t(`kw.kind.${k}`)}
            </button>
          ))}
          {data && <span style={{ marginLeft: 'auto', color: 'var(--text-muted)', fontSize: '0.75rem', alignSelf: 'center' }}>
            {t('kw.count', { shown: sorted.length, total: data.keywords.length })}
          </span>}
        </div>
      </div>

      {error && <div className="glass-panel" style={{ padding: '1rem', color: 'var(--accent-red, #ff4747)' }}>{error}</div>}
      {!data && !error && <div className="glass-panel" style={{ padding: '1rem', color: 'var(--text-muted)' }}>{t('kw.loading')}</div>}

      {data && (
        <div className="glass-panel" style={{ padding: 0, overflow: 'hidden' }}>
          {sorted.length === 0 && <div style={{ padding: '1rem', color: 'var(--text-muted)' }}>{t('kw.none')}</div>}
          {sorted.map(k => {
            const isOpen = open === k.id;
            return (
              <div key={k.id} className={`kw-row${isOpen ? ' open' : ''}`}>
                <button type="button" className="kw-head" onClick={() => setOpen(isOpen ? null : k.id)} aria-expanded={isOpen}>
                  <span style={{ flex: 1, minWidth: 0 }}>
                    <span className="kw-name">
                      {k.name}
                      <span className="kw-tag">{t(`kw.kind.${k.kind}`)}</span>
                    </span>
                    <span className="kw-reminder">
                      {k.reminder ? <>({k.reminder})</> : k.summary}
                    </span>
                  </span>
                  <ChevronDown size={18} className="kw-caret" />
                </button>
                {isOpen && (
                  <div className="kw-body">
                    {k.reminder && k.summary && <p className="kw-summary">{k.summary}</p>}
                    <ol className="kw-rules">
                      {k.rules.map(r => (
                        <li key={r.id}><span className="kw-rule-id">{r.id}</span> {r.text}</li>
                      ))}
                    </ol>
                    <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap', alignItems: 'center' }}>
                      <span style={{ color: 'var(--text-muted)', fontSize: '0.75rem' }}>
                        CR {k.id}{k.cards ? ` · ${t('kw.cards', { count: k.cards })}` : ''}
                      </span>
                      <a className="kw-link" href={`https://scryfall.com/search?q=${encodeURIComponent(`keyword:"${k.name.toLowerCase()}"`)}`} target="_blank" rel="noopener noreferrer">
                        {t('kw.scryfall')}
                      </a>
                    </div>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
