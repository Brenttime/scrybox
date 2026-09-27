import { useEffect, useMemo, useRef, useState } from 'react';
import { BookOpen, ChevronLeft, ChevronRight, Search, X } from 'lucide-react';
import { useT } from '../utils/i18n';
import LoadMore from './LoadMore';

// ManaBox-style rules book: browse a table of contents
// (chapter -> section -> rules), or search across everything. Rule references
// inside the text are links that open the section and scroll to the rule.

const CHAPTERS = {
  1: 'Game Concepts', 2: 'Parts of a Card', 3: 'Card Types', 4: 'Zones', 5: 'Turn Structure',
  6: 'Spells, Abilities, and Effects', 7: 'Additional Rules', 8: 'Multiplayer Rules', 9: 'Casual Variants',
};
const PAGE = 200;
const REF = /\b(\d{3})(?:\.(\d+)([a-z])?)?\b/;
const idQuery = q => /^\d{3}(\.\d*[a-z]?)?$/.test(q);
const escRe = s => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const depth = id => (/\d[a-z]$/.test(id) ? 2 : 1);

function Highlight({ text, terms, onRule }) {
  const words = terms.filter(w => w.length > 1).map(escRe);
  const re = new RegExp(`(\\b\\d{3}\\.\\d+[a-z]?\\b${words.length ? '|' + words.join('|') : ''})`, 'gi');
  return text.split(re).map((part, i) => {
    if (i % 2 === 0) return part;
    if (/^\d{3}\.\d+[a-z]?$/i.test(part)) {
      return <a key={i} href="#" className="rules-ref" onClick={e => { e.preventDefault(); onRule(part); }} style={{ color: 'var(--accent-blue, #7fb3ff)' }}>{part}</a>;
    }
    return <mark key={i} style={{ background: 'rgba(245,185,66,0.35)', color: 'inherit', borderRadius: 3 }}>{part}</mark>;
  });
}

function Row({ title, sub, onClick }) {
  return (
    <button type="button" className="rules-row" onClick={onClick}
      style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', width: '100%', textAlign: 'left', padding: '0.8rem 1rem', background: 'none', border: 0, borderBottom: '1px solid var(--border-glass)', color: 'var(--text-primary)', cursor: 'pointer', font: 'inherit' }}>
      <span style={{ flex: 1, minWidth: 0 }}>
        <span style={{ display: 'block', fontWeight: 600 }}>{title}</span>
        {sub && <span style={{ display: 'block', color: 'var(--text-muted)', fontSize: '0.75rem' }}>{sub}</span>}
      </span>
      <ChevronRight size={18} style={{ color: 'var(--text-muted)', flexShrink: 0 }} />
    </button>
  );
}

function RuleLine({ r, terms, onRule, target, showSection }) {
  const d = r.src === 'Comprehensive' ? depth(r.id) : 1;
  return (
    <div id={`rule-${r.id}`} className="rules-rule" data-target={target || undefined}
      style={{ padding: '0.55rem 1rem', paddingLeft: d === 2 ? '2.2rem' : '1rem', borderBottom: '1px solid var(--border-glass)', whiteSpace: 'pre-wrap', lineHeight: 1.5, background: target ? 'rgba(245,185,66,0.15)' : 'none', transition: 'background 1s' }}>
      {showSection && <div style={{ color: 'var(--text-muted)', fontSize: '0.72rem' }}>{r.section}</div>}
      <span style={{ fontWeight: 800, color: 'var(--accent-yellow)', marginRight: 8 }}>{r.src === 'Commander' ? '' : r.id}</span>
      <span style={{ color: 'var(--text-primary)' }}><Highlight text={r.text} terms={terms} onRule={onRule} /></span>
    </div>
  );
}

export default function Rules({ onNavigate }) {
  const { t } = useT();
  const [data, setData] = useState(null);
  const [error, setError] = useState('');
  const [q, setQ] = useState('');
  const [debounced, setDebounced] = useState('');
  const [shown, setShown] = useState(PAGE);
  // Navigation stack: [] = contents, [{chapter}], [{chapter},{section}]
  const [path, setPath] = useState([]);
  const [target, setTarget] = useState('');
  const topRef = useRef(null);

  useEffect(() => {
    fetch('/api/rules').then(async r => {
      const d = await r.json();
      if (!r.ok) throw new Error(d.error || r.statusText);
      setData({ ...d, rules: d.rules.map(x => ({ ...x, _s: `${x.id} ${x.section} ${x.text}`.toLowerCase() })) });
    }).catch(e => setError(e.message));
  }, []);

  useEffect(() => { const h = setTimeout(() => setDebounced(q), 120); return () => clearTimeout(h); }, [q]);
  useEffect(() => setShown(PAGE), [debounced]);

  // Index: chapters -> sections -> rules
  const book = useMemo(() => {
    if (!data) return null;
    const sections = new Map(); // key -> {key,title,chapter,rules[]}
    const add = (key, title, chapter, r) => {
      if (!sections.has(key)) sections.set(key, { key, title, chapter, rules: [] });
      if (r) sections.get(key).rules.push(r);
    };
    for (const r of data.rules) {
      if (r.src === 'Commander') add(`cmd:${r.section}`, r.section, 'commander', r);
      else if (r.src === 'Glossary') add(`gl:${r.id[0].toUpperCase()}`, r.id[0].toUpperCase(), 'glossary', r);
      else if (r.header) add(r.id, r.text, r.id[0]);
      else add(r.id.split('.')[0], r.section, r.id[0], r);
    }
    const chapters = [
      { key: 'commander', title: t('rules.src.commander') },
      ...Object.entries(CHAPTERS).map(([k, v]) => ({ key: k, title: `${k}. ${v}` })),
      { key: 'glossary', title: t('rules.src.glossary') },
    ].map(c => ({ ...c, sections: [...sections.values()].filter(s => s.chapter === c.key) }))
      .filter(c => c.sections.length);
    return { chapters, sections };
  }, [data, t]);

  const raw = debounced.trim().toLowerCase();
  const terms = useMemo(() => {
    const phrase = (raw.match(/"([^"]+)"/) || [])[1];
    return phrase ? [phrase] : raw.split(/\s+/).filter(Boolean);
  }, [raw]);

  const results = useMemo(() => {
    if (!data || !raw) return [];
    let res = data.rules.filter(r => !r.header);
    if (idQuery(raw)) {
      const pre = raw.includes('.') ? raw : raw + '.';
      return res.filter(r => { const id = r.id.toLowerCase(); return id === raw || id.startsWith(pre); });
    }
    res = res.filter(r => terms.every(w => r._s.includes(w)));
    const score = r => (r.src === 'Glossary' && r.id.toLowerCase() === raw ? 5 : 0) + (r.id.toLowerCase().includes(raw) ? 2 : 0);
    return [...res].sort((a, b) => score(b) - score(a));
  }, [data, raw, terms]);

  // Open the section containing a rule id and scroll to it.
  const openRule = id => {
    if (!book) return;
    const m = id.match(REF);
    const sec = m && book.sections.get(m[1]);
    if (!sec) { setQ(id); return; }
    const ch = book.chapters.find(c => c.key === sec.chapter);
    setQ(''); setDebounced('');
    setPath([ch.key, sec.key]);
    setTarget(sec.rules.some(r => r.id === id) ? id : '');
  };
  const openHit = r => {
    if (r.src === 'Comprehensive') return openRule(r.id);
    const key = r.src === 'Commander' ? `cmd:${r.section}` : `gl:${r.id[0].toUpperCase()}`;
    const sec = book.sections.get(key);
    setQ(''); setDebounced(''); setPath([sec.chapter, key]); setTarget(r.id);
  };

  useEffect(() => {
    if (target) {
      requestAnimationFrame(() => document.getElementById(`rule-${target}`)?.scrollIntoView({ block: 'center' }));
      const h = setTimeout(() => setTarget(''), 2500);
      return () => clearTimeout(h);
    }
    topRef.current?.scrollIntoView({ block: 'start' });
    return undefined;
  }, [path, target]);

  const chapter = book && path[0] && book.chapters.find(c => c.key === path[0]);
  const section = book && path[1] && book.sections.get(path[1]);
  const title = section ? section.title : chapter ? chapter.title : t('rules.title');
  const back = () => (path.length ? setPath(p => p.slice(0, -1)) : onNavigate?.('dashboard'));
  const list = { padding: 0, overflow: 'hidden' };

  return (
    <div className="rules-view" ref={topRef} style={{ display: 'flex', flexDirection: 'column', gap: '1rem', scrollMarginTop: 80 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: '0.6rem' }}>
        <button type="button" className="btn btn-secondary rules-back" onClick={back} aria-label={t('common.back')} style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
          <ChevronLeft size={16} /> {t('common.back')}
        </button>
        <h2 style={{ margin: 0, fontSize: '1.4rem', fontWeight: 800, color: 'var(--text-strong)', display: 'flex', alignItems: 'center', gap: '0.5rem', minWidth: 0 }}>
          {!path.length && <BookOpen size={22} style={{ color: 'var(--accent-yellow)' }} />}
          <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{title}</span>
        </h2>
      </div>

      <div className="glass-panel" style={{ padding: '0.7rem 0.9rem', position: 'sticky', top: 0, zIndex: 5 }}>
        <div style={{ position: 'relative' }}>
          <Search size={16} style={{ position: 'absolute', left: 12, top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
          <input className="form-input rules-search" type="search" value={q} onChange={e => setQ(e.target.value)}
            placeholder={t('rules.placeholder')} aria-label={t('rules.placeholder')}
            style={{ width: '100%', boxSizing: 'border-box', padding: '0.6rem 2.2rem', fontSize: 16, background: 'var(--input-bg)', color: 'var(--text-primary)', border: '1px solid var(--border-glass)', borderRadius: 'var(--radius-sm)' }} />
          {q && <button type="button" aria-label={t('rules.clear')} onClick={() => { setQ(''); setDebounced(''); }}
            style={{ position: 'absolute', right: 8, top: '50%', transform: 'translateY(-50%)', background: 'none', border: 0, color: 'var(--text-muted)', cursor: 'pointer' }}><X size={16} /></button>}
        </div>
        <div className="rules-meta" style={{ color: 'var(--text-muted)', fontSize: '0.72rem', marginTop: '0.45rem' }}>
          {error ? error : !data ? t('rules.loading')
            : raw ? t('rules.hits', { count: results.length })
              : t('rules.meta', { shown: data.rules.length, total: data.rules.length, version: data.crVersion, date: new Date(data.fetched).toLocaleDateString() })
                + (data.stale ? ` · ${t('rules.stale')}` : '')}
        </div>
      </div>

      {book && raw && (
        <div className="glass-panel rules-results" style={list}>
          {results.slice(0, shown).map((r, i) => (
            <div key={r.src + r.id + i} role="button" tabIndex={0} style={{ cursor: 'pointer' }}
              onClick={e => { if (!e.target.closest('a')) openHit(r); }}
              onKeyDown={e => { if (e.key === 'Enter') openHit(r); }}>
              <RuleLine r={r} terms={terms} onRule={openRule} showSection />
            </div>
          ))}
          {!results.length && <p style={{ color: 'var(--text-muted)', padding: '0 1rem' }}>{t('rules.none')}</p>}
          {results.length > shown && (
            <LoadMore style={{ margin: '1rem 0' }} onLoad={() => setShown(s => s + 300)}>
              {t('rules.more', { count: results.length - shown })}
            </LoadMore>
          )}
        </div>
      )}

      {book && !raw && !chapter && (
        <div className="glass-panel rules-toc" style={list}>
          {book.chapters.map(c => <Row key={c.key} title={c.title} sub={t('rules.sections', { count: c.sections.length })} onClick={() => setPath([c.key])} />)}
        </div>
      )}

      {book && !raw && chapter && !section && (
        <div className="glass-panel rules-toc" style={list}>
          {chapter.sections.map(s => <Row key={s.key} title={s.title} onClick={() => setPath([chapter.key, s.key])} />)}
        </div>
      )}

      {book && !raw && section && (
        <div className="glass-panel rules-section" style={list}>
          {section.rules.map((r, i) => (
            r.src === 'Glossary'
              ? <div key={r.id + i} id={`rule-${r.id}`} className="rules-rule" style={{ padding: '0.6rem 1rem', borderBottom: '1px solid var(--border-glass)', lineHeight: 1.5, background: target === r.id ? 'rgba(245,185,66,0.15)' : 'none' }}>
                  <div style={{ fontWeight: 800, color: 'var(--accent-yellow)' }}>{r.id}</div>
                  <div style={{ color: 'var(--text-primary)', whiteSpace: 'pre-wrap' }}><Highlight text={r.text} terms={[]} onRule={openRule} /></div>
                </div>
              : <RuleLine key={r.id + i} r={r} terms={[]} onRule={openRule} target={target === r.id} />
          ))}
        </div>
      )}
    </div>
  );
}
