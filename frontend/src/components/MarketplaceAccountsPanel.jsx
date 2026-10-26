import { useEffect, useState } from 'react';
import { Loader2, Check, Trash2, ExternalLink } from 'lucide-react';
import { useT } from '../utils/i18n';
import { startMarketRelay } from '../utils/marketRelay';

// Marketplace account credentials for the "import from order" feature.
//
// ManaPool has a real buyer-order API. The user creates an access token in
// their ManaPool dashboard (Settings -> Integrations) and pastes it here with
// the account email.
//
// The secret is never rendered: GET hands back masked presence booleans and
// a saved-at stamp. The password-style inputs are `type=password` +
// autoComplete=off + spellCheck off so a password manager or browser translation
// does not try to hold them.
export default function MarketplaceAccountsPanel({ showToast }) {
  const { t } = useT();
  const [status, setStatus] = useState(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');

  const [mpEmail, setMpEmail] = useState('');
  const [mpToken, setMpToken] = useState('');
  const [mpEnabled, setMpEnabled] = useState(true);
  // 'manapool' | '' while a Connect popup is mid-open (the open
  // itself is synchronous; this only tracks the blocked-vs-opened outcome flash).
  const [relaying, setRelaying] = useState('');

  // Connect: open the provider's own credentials page in a popup, synchronously
  // in the click so popup blockers see real user activation. There is NO
  // background credential handoff: cross-origin popups cannot script the
  // provider page and localStorage is not shared across sites, so any "automatic
  // capture" would silently time out at best. The page shows the credential to
  // the signed-in owner; they copy it into the field beside this button.
  const connect = (source) => {
    setError('');
    setRelaying(source);
    // The open happens synchronously inside the call (user activation is kept);
    // only the outcome arrives async, so a rejection must be caught on the
    // promise, not in a try block around the call.
    startMarketRelay(source)
      .then(() => setRelaying(''))
      .catch((err) => {
        setRelaying('');
        if (String(err?.message || '') === 'POPUP_BLOCKED') setError(t('marketplace.errPopupBlocked'));
      });
  };

  const load = async () => {
    setLoading(true);
    try {
      const res = await fetch('/api/marketplace/accounts');
      const data = await res.json().catch(() => ({}));
      if (!res.ok) { setError(data.error || t('marketplace.errLoad')); return; }
      setStatus(data);
      // Echo the non-secret state back into the fields so the panel shows what
      // is configured without ever leaking the value.
      setMpEnabled(data.manapool?.enabled !== false);
    } catch {
      setError(t('marketplace.errLoad'));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const save = async (patch) => {
    setSaving(true);
    setError('');
    try {
      const res = await fetch('/api/marketplace/accounts', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(patch),
      });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) { setError(data.error || t('marketplace.errSave')); return; }
      showToast(t('marketplace.saved'));
      setMpToken('');
      await load();
    } catch {
      setError(t('marketplace.errSave'));
    } finally {
      setSaving(false);
    }
  };

  const rowStyle = { display: 'flex', flexDirection: 'column', gap: '0.4rem' };
  const labelStyle = { fontSize: '0.78rem', fontWeight: 700, color: 'var(--text-secondary)' };
  const noteStyle = { fontSize: '0.72rem', color: 'var(--text-secondary)', lineHeight: 1.5 };
  const badge = (ok) => (
    <span style={{
      fontSize: '0.68rem', fontWeight: 800, padding: '0.1rem 0.45rem', borderRadius: 999,
      background: ok ? 'rgba(52,199,124,0.16)' : 'rgba(255,255,255,0.06)',
      color: ok ? 'var(--success)' : 'var(--text-secondary)',
      border: `1px solid ${ok ? 'rgba(52,199,124,0.4)' : 'var(--border-glass)'}`,
    }}>
      {ok ? t('marketplace.configured') : t('marketplace.notConfigured')}
    </span>
  );

  if (loading) return <div style={noteStyle}>{t('common.loading')}</div>;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '1.25rem' }}>
      <p style={noteStyle}>{t('marketplace.hint')}</p>
      {error && <div style={{ color: 'var(--accent-red)', fontSize: '0.8rem' }}>{error}</div>}

      {/* --- ManaPool --- */}
      <div style={{ display: 'flex', flexDirection: 'column', gap: '0.6rem', borderTop: '1px solid var(--border-glass)', paddingTop: '0.9rem' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
          <strong style={{ color: 'var(--text-strong)' }}>{t('marketplace.manapool')}</strong>
          {badge(status?.manapool?.configured)}
        </div>
        <div style={rowStyle}>
          <label style={labelStyle} htmlFor="mp-email">{t('marketplace.email')}</label>
          <input id="mp-email" type="email" className="input-control" value={mpEmail}
            onChange={(e) => setMpEmail(e.target.value)} placeholder={status?.manapool?.email || 'you@example.com'} />
        </div>
        <div style={rowStyle}>
          <label style={labelStyle} htmlFor="mp-token">{t('marketplace.token')}</label>
          <input id="mp-token" type="password" className="input-control" value={mpToken}
            autoComplete="off" spellCheck={false}
            onChange={(e) => setMpToken(e.target.value)} placeholder={status?.manapool?.token || 'mp…'} />
          {status?.manapool?.configured && (
            <div style={noteStyle}>{t('marketplace.tokenSavedHint')}{status?.manapool?.savedAt ? ` · ${status.manapool.savedAt}` : ''}</div>
          )}
        </div>
        <label style={{ display: 'flex', alignItems: 'center', gap: '0.45rem', fontSize: '0.8rem' }}>
          <input type="checkbox" checked={mpEnabled} onChange={(e) => setMpEnabled(e.target.checked)} />
          {t('marketplace.enable')}
        </label>
        <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap' }}>
          <button type="button" className="btn btn-primary" disabled={saving || !mpEmail.trim() || !mpToken.trim()}
            onClick={() => save({ manapool: { email: mpEmail.trim(), token: mpToken.trim(), enabled: mpEnabled } })}>
            {saving ? <Loader2 size={14} style={{ animation: 'spin 1s linear infinite' }} /> : <Check size={14} />} {t('marketplace.save')}
          </button>
          <button type="button" className="btn btn-secondary" disabled={saving || relaying === 'manapool'}
            onClick={() => connect('manapool')}>
            {relaying === 'manapool' ? <Loader2 size={14} style={{ animation: 'spin 1s linear infinite' }} /> : <ExternalLink size={14} />} {relaying === 'manapool' ? t('marketplace.connectWaiting') : t('marketplace.connect')}
          </button>
          <button type="button" className="btn btn-secondary" disabled={saving}
            onClick={() => save({ manapool: { clear: true } })}>
            <Trash2 size={14} /> {t('marketplace.clear')}
          </button>
        </div>
        <div style={noteStyle}>{t('marketplace.connectHelp')}</div>
      </div>
    </div>
  );
}
