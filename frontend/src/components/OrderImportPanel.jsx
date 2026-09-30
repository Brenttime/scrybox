import { useEffect, useRef, useState } from 'react';
import { ShoppingCart, Loader2, ShieldAlert, KeyRound, Bookmark, Copy, Smartphone, ChevronUp, ChevronDown } from 'lucide-react';
import CardImage from './CardImage';
import { LANGUAGES } from '../utils/cardOptions';
import { useT } from '../utils/i18n';
import { buildBookmarklet, takePendingTcgImport } from '../utils/tcgBookmarklet';

// Pull every card from a ManaPool or TCGplayer ORDER into the collection in one
// action. The flow mirrors SecretLairPanel (enter -> preview resolved lines ->
// commit): what you approve is exactly what gets filed, with per-line quantity,
// unit price and condition visible before anything is written.
//
// Credentials deliberately do NOT live here. A ManaPool token and a TCGplayer
// session cookie are account-level secrets, so they are stored per user under
// Settings > Marketplace Accounts and only ever read back masked; this panel
// just shows whether each source is ready and links there when it is not. The
// order number is the only thing this screen sends. (The app-wide fetch
// interceptor in App.jsx attaches the session Bearer automatically, so no
// Authorization header is built here.)
//
// The server owns the whole read path (/api/marketplace): provider HTTP, the
// cookie jar, card resolution through the shared bulkFetchByIdentifier pipeline,
// price sanitising, and the write via the same bulk-add core the tray and
// Secret Lair use. That means a preview line that resolved here cannot fail to
// resolve on commit — the two calls run the same resolver over the same lines.
//
// Printing follows the order: a line is foiled only when the order itself says
// so. There is no "force all foil" override, because the order, not the
// importer, is the authority on what arrived in the box. 'nonfoil' exists for
// the buyer who wants the stack counted as its non-foil equivalent and is
// explicit about it.
export default function OrderImportPanel({ onAddSuccess, showToast, setActiveTab }) {
  const { t } = useT();
  const JSON_HEADERS = { 'Content-Type': 'application/json' };

  const [source, setSource] = useState('manapool');
  const [orderNumber, setOrderNumber] = useState('');
  const [status, setStatus] = useState(null);            // {manapool:{configured},tcgplayer:{configured}}
  const [phase, setPhase] = useState('form');            // form | preview | done
  const [preview, setPreview] = useState(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState(null);
  const [language, setLanguage] = useState('English');
  const [printingMode, setPrintingMode] = useState('auto');
  const previewSeq = useRef(0);
  // TCGplayer runs through the bookmarklet: the order arrives already read off
  // the user's logged-in TCGplayer page, so no cookie is needed or used.
  const [pageImport, setPageImport] = useState(null);   // {orders, lines}
  const bookmarkRef = useRef(null);
  useEffect(() => {
    // React refuses javascript: hrefs in JSX; set it on the element directly.
    if (bookmarkRef.current) bookmarkRef.current.setAttribute('href', buildBookmarklet(window.location.origin));
  });

  // Which sources are usable right now. A missing credential is not an error —
  // the panel says so and points at Settings rather than showing a form that
  // could only fail.
  useEffect(() => {
    let cancelled = false;
    fetch('/api/marketplace/accounts')
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(String(r.status)))))
      .then((d) => { if (!cancelled) setStatus(d); })
      .catch(() => { if (!cancelled) setStatus(null); });
    return () => { cancelled = true; };
  }, []);

  const readyFor = (src) => src === 'tcgplayer' || (!!status && !!(status[src] && status[src].configured));
  const needsSetup = status && !readyFor(source);

  // Recent-order picker: fetch the source's list when the form opens (and when
  // the source tab changes). It is a convenience, never a blocker: any failure
  // just hides the list and the manual number field stays the path.
  const [recent, setRecent] = useState({ loading: false, orders: null, error: '', offset: 0, hasMore: false });
  const recentSeq = useRef(0);
  const loadRecent = async (src, offset = 0) => {
    if (src === 'tcgplayer' || !readyFor(src)) return;
    const seq = ++recentSeq.current;
    setRecent((r) => ({ ...r, loading: true }));
    try {
      const res = await fetch(`/api/marketplace/recent/${src}?offset=${offset}`);
      const data = await res.json().catch(() => ({}));
      if (seq !== recentSeq.current) return;
      if (!res.ok) { setRecent({ loading: false, orders: null, error: data.list_unavailable ? 'list' : 'error', offset: 0, hasMore: false }); return; }
      setRecent({ loading: false, orders: data.orders || [], error: '', offset: data.offset ?? offset, hasMore: !!data.hasMore });
    } catch {
      if (seq === recentSeq.current) setRecent({ loading: false, orders: null, error: 'error', offset: 0, hasMore: false });
    }
  };

  useEffect(() => {
    if (status && readyFor(source)) loadRecent(source);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status, source]);

  const doPreview = async () => {
    const num = orderNumber.trim();
    if (!num) return;
    const seq = ++previewSeq.current;
    setBusy(true);
    setError('');
    setPreview(null);
    try {
      const res = await fetch('/api/marketplace/preview', {
        method: 'POST',
        headers: JSON_HEADERS,
        body: JSON.stringify({ source, order_number: num }),
      });
      const data = await res.json();
      if (seq !== previewSeq.current) return;            // a newer preview has landed
      if (!res.ok) { setError(data.error || t('orderimport.errPreview')); return; }
      setPreview(data);
      setPhase('preview');
    } catch {
      if (seq === previewSeq.current) setError(t('orderimport.errPreview'));
    } finally {
      if (seq === previewSeq.current) setBusy(false);
    }
  };

  const doPagePreview = async (handoff) => {
    const seq = ++previewSeq.current;
    setBusy(true); setError(''); setPreview(null);
    try {
      const res = await fetch('/api/marketplace/tcg-page/preview', {
        method: 'POST', headers: JSON_HEADERS,
        body: JSON.stringify({ orders: handoff.orders, lines: handoff.lines }),
      });
      const data = await res.json().catch(() => ({}));
      if (seq !== previewSeq.current) return;
      if (!res.ok) { setError(data.error || t('orderimport.errPreview')); return; }
      setPageImport(handoff);
      setPreview(data);
      setPhase('preview');
    } catch {
      if (seq === previewSeq.current) setError(t('orderimport.errPreview'));
    } finally {
      if (seq === previewSeq.current) setBusy(false);
    }
  };

  useEffect(() => {
    const handoff = takePendingTcgImport();
    if (handoff) { setSource('tcgplayer'); doPagePreview(handoff); }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const doAdd = async () => {
    if (!preview) return;
    if (pageImport) {
      setBusy(true); setError('');
      try {
        const res = await fetch('/api/marketplace/tcg-page/add', {
          method: 'POST', headers: JSON_HEADERS,
          body: JSON.stringify({ lines: pageImport.lines, printing_mode: printingMode, language }),
        });
        const data = await res.json().catch(() => ({}));
        if (!res.ok) { setError(data.error || t('orderimport.errAdd')); return; }
        setResult(data); setPhase('done'); setPageImport(null);
        if (data.added) showToast?.(t('orderimport.added', { count: data.added }));
        onAddSuccess?.();
      } catch {
        setError(t('orderimport.errAdd'));
      } finally {
        setBusy(false);
      }
      return;
    }
    setBusy(true);
    setError('');
    try {
      const res = await fetch('/api/marketplace/add', {
        method: 'POST',
        headers: JSON_HEADERS,
        body: JSON.stringify({
          source,
          order_number: preview.number || orderNumber.trim(),
          printing_mode: printingMode,
          language,
        }),
      });
      const data = await res.json();
      if (!res.ok) { setError(data.error || t('orderimport.errAdd')); return; }
      setResult(data);
      setPhase('done');
      // Same handoff the scanner uses: a toast for the at-a-glance confirmation
      // and a stats refresh, while the in-panel summary carries the detail.
      if (data.added) showToast?.(t('orderimport.added', { count: data.added }));
      onAddSuccess?.();
    } catch {
      setError(t('orderimport.errAdd'));
    } finally {
      setBusy(false);
    }
  };

  const reset = () => {
    previewSeq.current += 1;
    setPreview(null); setResult(null); setError(''); setPhase('form'); setBusy(false); setPageImport(null);
  };

  const dollars = (cents) => cents == null ? null : `$${(cents / 100).toFixed(2)}`;

  // ---- form -------------------------------------------------------------
  if (phase === 'form') {
    return (
      <div className="glass-panel" style={{ maxWidth: 560, margin: '0 auto', display: 'flex', flexDirection: 'column', gap: '1rem' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '.5rem' }}>
          <ShoppingCart size={18} style={{ color: 'var(--accent-yellow)' }} />
          <h3 style={{ margin: 0, color: 'var(--text-strong)', fontSize: '1.05rem' }}>{t('orderimport.title')}</h3>
        </div>
        <p style={{ margin: 0, color: 'var(--text-secondary)', fontSize: '.85rem' }}>{t('orderimport.subtitle')}</p>

        <div className="sub-nav-tabs" role="tablist" aria-label={t('orderimport.sourceAria')}>
          {['manapool', 'tcgplayer'].map((k) => (
            <button key={k} type="button" role="tab" aria-selected={source === k}
              className={`sub-nav-tab${source === k ? ' active' : ''}`} onClick={() => setSource(k)}>
              {t(`orderimport.source.${k}`)}
            </button>
          ))}
        </div>

        {source === 'tcgplayer' && (
          <div className="tcg-bookmarklet" style={{ display: 'flex', flexDirection: 'column', gap: '.6rem', fontSize: '.82rem', color: 'var(--text-secondary)' }}>
            <div>{t('orderimport.tcgHow')}</div>
            <ol style={{ margin: 0, paddingLeft: '1.2rem', display: 'flex', flexDirection: 'column', gap: '.3rem' }}>
              <li>{t('orderimport.tcgStep1')}</li>
              <li>{t('orderimport.tcgStep2')}</li>
              <li>{t('orderimport.tcgStep3')}</li>
            </ol>
            <a ref={bookmarkRef} className="btn btn-primary tcg-bookmarklet-link" draggable="true"
              onClick={(e) => { e.preventDefault(); showToast?.(t('orderimport.tcgDragHint')); }}
              style={{ alignSelf: 'flex-start', display: 'inline-flex', alignItems: 'center', gap: '.4rem', cursor: 'grab' }}>
              <Bookmark size={14} /> {t('orderimport.tcgBookmarkName')}
            </a>
            <details className="tcg-phone-setup" style={{ fontSize: '.8rem' }}>
              <summary style={{ cursor: 'pointer', fontWeight: 700, color: 'var(--text-strong)', display: 'inline-flex', alignItems: 'center', gap: '.35rem' }}>
                <Smartphone size={14} /> {t('orderimport.tcgPhoneTitle')}
              </summary>
              <ol style={{ margin: '.5rem 0 0', paddingLeft: '1.2rem', display: 'flex', flexDirection: 'column', gap: '.3rem' }}>
                <li>
                  <button type="button" className="btn btn-small btn-secondary tcg-copy-bookmarklet" style={{ display: 'inline-flex', alignItems: 'center', gap: '.3rem' }}
                    onClick={async () => {
                      const code = buildBookmarklet(window.location.origin);
                      let ok = false;
                      try { await navigator.clipboard.writeText(code); ok = true; } catch {
                        const ta = document.createElement('textarea'); ta.value = code; document.body.append(ta); ta.select();
                        try { ok = document.execCommand('copy'); } catch { /* reported below */ } ta.remove();
                      }
                      showToast?.(ok ? t('orderimport.tcgCopied') : t('orderimport.tcgCopyFailed'));
                    }}>
                    <Copy size={12} /> {t('orderimport.tcgCopyCode')}
                  </button>
                </li>
                <li>{t('orderimport.tcgPhoneIos')}</li>
                <li>{t('orderimport.tcgPhoneAndroid')}</li>
                <li>{t('orderimport.tcgPhoneUse')}</li>
              </ol>
            </details>
            <div style={{ fontSize: '.72rem', color: 'var(--text-muted)' }}>{t('orderimport.tcgPrivacy')}</div>
            {busy && <div><Loader2 size={14} style={{ animation: 'spin 1s linear infinite' }} /> {t('orderimport.retrieving')}</div>}
            {error && <div style={{ color: 'var(--accent-red)' }}>{error}</div>}
          </div>
        )}

        {source !== 'tcgplayer' && status && (
          <div style={{ display: 'flex', alignItems: 'center', gap: '.5rem', fontSize: '.78rem', color: readyFor(source) ? 'var(--text-secondary)' : 'var(--accent-yellow)' }}>
            {readyFor(source)
              ? <>{t('orderimport.ready', { source: t(`orderimport.source.${source}`) })}</>
              : <><ShieldAlert size={14} /> {t('orderimport.notConfigured', { source: t(`orderimport.source.${source}`) })}
                   <button type="button" className="btn btn-small" onClick={() => setActiveTab?.('settings', 'marketplace')}>
                     <KeyRound size={12} /> {t('orderimport.goToSettings')}
                   </button></>}
          </div>
        )}

        {source !== 'tcgplayer' && (<>
        {/* Recent orders: a quick-pick list when the provider exposes one. */}
        {readyFor(source) && recent.loading && !Array.isArray(recent.orders) && (
          <div style={{ fontSize: '.78rem', color: 'var(--text-secondary)' }}>{t('orderimport.recentLoading')}</div>
        )}
        {readyFor(source) && Array.isArray(recent.orders) && recent.orders.length > 0 && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: '.35rem' }}>
            <div style={{ fontSize: '.72rem', fontWeight: 700, color: 'var(--text-secondary)' }}>{t('orderimport.recentTitle')}</div>
            <div style={{ display: 'flex', gap: '.5rem', alignItems: 'stretch' }}>
            <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: '.35rem', opacity: recent.loading ? .5 : 1 }}>
            {recent.orders.map((o) => (
              <button key={o.number} type="button"
                className={`btn btn-small ${orderNumber.trim() === o.number ? 'btn-primary' : 'btn-secondary'}`}
                style={{ justifyContent: 'flex-start', textAlign: 'left', whiteSpace: 'normal' }}
                disabled={busy}
                onClick={() => { setOrderNumber(o.number); setError(''); }}>
                <span style={{ fontWeight: 700 }}>#{o.number}</span>
                <span style={{ opacity: .75, marginLeft: '.5rem' }}>{String(o.placedAt || '').slice(0, 10)}</span>
                {o.status && <span style={{ opacity: .6, marginLeft: '.5rem' }}>{o.status}</span>}
                <span style={{ marginLeft: 'auto', opacity: .8 }}>{t('orderimport.recentCopies', { count: o.cardCount })}</span>
              </button>
            ))}
            </div>
            {source === 'manapool' && (
              <div style={{ display: 'flex', flexDirection: 'column', justifyContent: 'space-between', gap: '.35rem' }}>
                <button type="button" className="btn btn-small btn-secondary" aria-label="Newer orders"
                  style={{ flex: 1, padding: '0 .5rem' }}
                  disabled={busy || recent.loading || recent.offset <= 0}
                  onClick={() => loadRecent(source, Math.max(0, recent.offset - 3))}>
                  <ChevronUp size={18} />
                </button>
                <button type="button" className="btn btn-small btn-secondary" aria-label="Older orders"
                  style={{ flex: 1, padding: '0 .5rem' }}
                  disabled={busy || recent.loading || !recent.hasMore}
                  onClick={() => loadRecent(source, recent.offset + 3)}>
                  <ChevronDown size={18} />
                </button>
              </div>
            )}
            </div>
          </div>
        )}
        {readyFor(source) && !recent.loading && recent.error === 'list' && (
          <div style={{ fontSize: '.72rem', color: 'var(--text-secondary)' }}>{t('orderimport.recentUnavailable')}</div>
        )}

        <div className="form-group" style={{ marginBottom: 0 }}>
          <label htmlFor="order-number-input">{t('orderimport.orderNumber')}</label>
          <input id="order-number-input" className="input-control" type="text" inputMode="numeric"
            autoComplete="off" value={orderNumber} disabled={busy || needsSetup}
            onChange={(e) => setOrderNumber(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') doPreview(); }} />
        </div>

        {error && <div style={{ background: 'rgba(239,68,68,.1)', border: '1px solid var(--accent-red)', borderRadius: 'var(--radius-sm)', padding: '.5rem .75rem', color: 'var(--accent-red)', fontSize: '.82rem' }}>{error}</div>}

        <div style={{ display: 'flex', gap: '.5rem', justifyContent: 'flex-end' }}>
          {/* Clears the field. It read "Retrieve order" before, which made two
              buttons side by side say the same thing while only one of them
              retrieved — the other silently wiped what you had typed. */}
          <button type="button" className="btn btn-secondary" onClick={() => { setOrderNumber(''); setError(''); }} disabled={busy}>
            {t('orderimport.clear')}
          </button>
          <button type="button" className="btn btn-primary" onClick={doPreview}
            disabled={busy || needsSetup || !orderNumber.trim()}>
            {busy ? <><Loader2 size={14} style={{ animation: 'spin 1s linear infinite' }} /> {t('orderimport.retrieving')}</> : t('orderimport.retrieve')}
          </button>
        </div>
        </>)}
      </div>
    );
  }

  // ---- done -------------------------------------------------------------
  if (phase === 'done' && result) {
    return (
      <div className="glass-panel" style={{ maxWidth: 560, margin: '0 auto', textAlign: 'center', display: 'flex', flexDirection: 'column', gap: '.75rem' }}>
        <ShoppingCart size={26} style={{ color: 'var(--accent-green)', margin: '0 auto' }} />
        <p style={{ color: 'var(--text-strong)', fontWeight: 600, margin: 0 }}>{result.message || t('orderimport.added', { count: result.added || 0 })}</p>
        {result.unresolved > 0 && <p style={{ color: 'var(--text-secondary)', fontSize: '.8rem', margin: 0 }}>{t('orderimport.unresolvedNote', { count: result.unresolved })}</p>}
        {Array.isArray(result.failed) && result.failed.length > 0 && (
          <p style={{ color: 'var(--accent-red)', fontSize: '.8rem', margin: 0 }}>{t('orderimport.failedNote', { count: result.failed.length })}</p>
        )}
        <div style={{ display: 'flex', gap: '.5rem', justifyContent: 'center' }}>
          <button type="button" className="btn btn-secondary" onClick={reset}>{t('orderimport.again')}</button>
          <button type="button" className="btn btn-primary" onClick={() => setActiveTab?.('collection')}>{t('orderimport.goToCollection')}</button>
        </div>
      </div>
    );
  }

  // ---- preview ----------------------------------------------------------
  const cards = (preview && preview.cards) || [];
  return (
    <div className="glass-panel" style={{ maxWidth: 760, margin: '0 auto', display: 'flex', flexDirection: 'column', gap: '.85rem' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: '.5rem' }}>
        <button type="button" className="btn btn-small btn-secondary" onClick={reset} disabled={busy}>{t('orderimport.back')}</button>
        <h3 style={{ margin: 0, color: 'var(--text-strong)', fontSize: '1.02rem' }}>
          {t('orderimport.orderSummary', { number: preview.number || orderNumber, count: preview.totalListed ?? cards.length })}
        </h3>
      </div>

      <div style={{ display: 'flex', flexWrap: 'wrap', gap: '1rem', fontSize: '.78rem', color: 'var(--text-secondary)' }}>
        {preview.status && <span>{t('orderimport.status', { status: preview.status })}</span>}
        {preview.placedAt && <span>{t('orderimport.placed', { date: String(preview.placedAt).slice(0, 10) })}</span>}
        <span>{t('orderimport.totals', { copies: preview.totalCopies ?? cards.length, cards: preview.resolvedCount ?? cards.length })}</span>
        {preview.price_cents != null && <span style={{ color: 'var(--accent-green)' }}>{t('orderimport.totalPrice', { amount: dollars(preview.price_cents) })}</span>}
      </div>

      <div style={{ display: 'flex', gap: '.6rem', flexWrap: 'wrap', alignItems: 'center' }}>
        <label className="form-label" htmlFor="oi-language">{t('orderimport.language')}</label>
        <select id="oi-language" className="input-control" style={{ width: 'auto' }} value={language} disabled={busy}
          onChange={(e) => setLanguage(e.target.value)}>
          {LANGUAGES.map((l) => <option key={l} value={l}>{l}</option>)}
        </select>
        <label className="form-label" htmlFor="oi-printing">{t('orderimport.printingMode')}</label>
        <select id="oi-printing" className="input-control" style={{ width: 'auto' }} value={printingMode} disabled={busy}
          onChange={(e) => setPrintingMode(e.target.value)}>
          <option value="auto">{t('orderimport.printingAuto')}</option>
          <option value="nonfoil">{t('orderimport.printingNonfoil')}</option>
        </select>
      </div>

      {cards.length === 0 ? (
        <p style={{ color: 'var(--text-secondary)', margin: 0 }}>{t('orderimport.noCards')}</p>
      ) : (
        <div className="sl-cardlist" style={{ listStyle: 'none', margin: 0, padding: 0, display: 'flex', flexDirection: 'column', gap: '.5rem' }}>
          {cards.map((c, i) => (
            <div key={`${c.card_id || 'x'}-${i}`} className="oi-row" style={{ display: 'flex', alignItems: 'center', gap: '.6rem', border: '1px solid var(--border-glass)', borderRadius: 'var(--radius-sm)', padding: '.4rem .6rem' }}>
              <div style={{ width: 40, height: 56, flexShrink: 0, borderRadius: 4, overflow: 'hidden', background: 'var(--bg-secondary)' }}>
                <CardImage card={{ image_url: c.image_url }} alt={c.name} width={40} style={{ width: '100%', height: '100%', objectFit: 'cover' }} />
              </div>
              <div style={{ minWidth: 0, flex: 1 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: '.4rem', flexWrap: 'wrap' }}>
                  <span style={{ fontWeight: 700, color: 'var(--text-strong)', fontSize: '.9rem' }}>{c.name}</span>
                  {c.set_code && <span style={{ fontSize: '.7rem', color: 'var(--text-secondary)' }}>{String(c.set_code).toUpperCase()} {c.number}</span>}
                  {c.is_foil && <span style={{ fontSize: '.64rem', fontWeight: 800, color: 'var(--accent-yellow)' }}>FOIL</span>}
                  {c.condition && <span style={{ fontSize: '.68rem', color: 'var(--text-secondary)' }}>{c.condition}</span>}
                  {c.owned > 0 && <span style={{ fontSize: '.66rem', color: 'var(--accent-green)' }}>{t('orderimport.ownedBadge', { count: c.owned })}</span>}
                </div>
                <div style={{ fontSize: '.74rem', color: 'var(--text-secondary)' }}>
                  {t('orderimport.qty', { count: c.quantity })}
                  {c.price_cents != null && <> · {dollars(c.price_cents)}</>}
                </div>
              </div>
            </div>
          ))}
        </div>
      )}

      {(preview.unresolved > 0 || preview.extras > 0) && (
        <p style={{ color: 'var(--text-secondary)', fontSize: '.78rem', margin: 0 }}>
          {preview.unresolved > 0 && <span>{t('orderimport.unresolvedNote', { count: preview.unresolved })} </span>}
          {Array.isArray(preview.unmatchedNames) && preview.unmatchedNames.length > 0 && (
            <span style={{ display: 'block', opacity: .8 }}>{preview.unmatchedNames.slice(0, 8).join(', ')}{preview.unmatchedNames.length > 8 ? '…' : ''}</span>
          )}
          {preview.extras > 0 && (
            <span>{t('orderimport.extrasNote', { count: preview.extras })}</span>
          )}
        </p>
      )}

      {error && <div style={{ background: 'rgba(239,68,68,.1)', border: '1px solid var(--accent-red)', borderRadius: 'var(--radius-sm)', padding: '.5rem .75rem', color: 'var(--accent-red)', fontSize: '.82rem' }}>{error}</div>}

      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: '.5rem' }}>
        <span style={{ fontSize: '.78rem', color: 'var(--text-secondary)' }}>
          {t('orderimport.confirmNote', { count: preview.resolvedCount ?? cards.length })}
        </span>
        <div style={{ display: 'flex', gap: '.5rem' }}>
          <button type="button" className="btn btn-secondary" onClick={reset} disabled={busy}>{t('orderimport.cancel')}</button>
          <button type="button" className="btn btn-primary" onClick={doAdd} disabled={busy || cards.length === 0}>
            {busy ? <><Loader2 size={14} style={{ animation: 'spin 1s linear infinite' }} /> {t('orderimport.adding')}</> : t('orderimport.confirmAdd', { count: preview.resolvedCount ?? cards.length })}
          </button>
        </div>
      </div>
    </div>
  );
}
