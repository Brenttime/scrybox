import { useState, useEffect, lazy, Suspense } from 'react';
import { ChevronDown, ShieldAlert, Share2, Clipboard, RefreshCw, KeyRound, Check, Database, Download, Upload, SlidersHorizontal, Info, Bug, Lightbulb, MessagesSquare, ScrollText, Github, Languages, Globe, ShoppingCart, RefreshCcw } from 'lucide-react';
import { LOCALES, localeName, useT } from '../utils/i18n';
import { buildCardListText } from '../utils/cardList';
import { REPO_URL } from '../utils/repo';
import MoxfieldPanel from './MoxfieldPanel';
import MarketplaceAccountsPanel from './MarketplaceAccountsPanel';
import ManaboxSyncPanel from './ManaboxSyncPanel';

// Admin-only surface, code-split like the view components so its heavy deps
// (catalog management, backups) only load for admins on the Settings tab.
const AdminPanel = lazy(() => import('./AdminPanel'));

// Collapsible settings section: the header is a real button (keyboard +
// aria-expanded), the body only mounts when open so collapsed sections cost
// nothing (their fetches run on first expand).
function SettingsSection({ id, icon, title, open, onToggle, children }) {
  const bodyId = `${id}-body`;
  return (
    <div id={id} className="glass-panel settings-section" style={{ display: 'flex', flexDirection: 'column', gap: open ? '1.25rem' : 0, scrollMarginTop: '1rem', padding: open ? undefined : '0.85rem 1.25rem' }}>
      <button type="button" className="settings-section-toggle" onClick={onToggle}
        aria-expanded={open ? 'true' : 'false'} aria-controls={bodyId}
        style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', width: '100%', background: 'none', border: 'none', padding: 0, paddingBottom: open ? '0.75rem' : 0, borderBottom: open ? '1px solid var(--border-glass)' : 'none', cursor: 'pointer', textAlign: 'left', color: 'inherit', font: 'inherit' }}>
        {icon}
        <h3 style={{ color: 'var(--text-strong)', fontSize: '1.1rem', margin: 0, flex: 1 }}>{title}</h3>
        <ChevronDown size={18} aria-hidden="true" style={{ color: 'var(--text-muted)', transition: 'transform 0.2s ease', transform: open ? 'rotate(180deg)' : 'none' }} />
      </button>
      {open && <div id={bodyId} style={{ display: 'flex', flexDirection: 'column', gap: '1.25rem' }}>{children}</div>}
    </div>
  );
}

function Settings({ user, onUpdateUser, showToast, target }) {
  const { locale, setLocale, t } = useT();
  const [currentPassword, setCurrentPassword] = useState('');
  const [password, setPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [passwordLoading, setPasswordLoading] = useState(false);
  
  const [shareEnabled, setShareEnabled] = useState(user?.share_enabled === 1);
  const [shareLoading, setShareLoading] = useState(false);

  const [publicBaseUrl, setPublicBaseUrl] = useState('');


  const [versionInfo, setVersionInfo] = useState(null);
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [backendReachable, setBackendReachable] = useState(true);

  // Every section starts collapsed so the page reads as a short index; a deep
  // link from another tab (Moxfield Sync, marketplace) opens its section first.
  const targetSection = target === 'moxfield' ? 'moxfield-panel'
    : target === 'marketplace' ? 'marketplace-panel' : null;
  const [openSections, setOpenSections] = useState(() => (targetSection ? { [targetSection]: true } : {}));
  const toggleSection = (id) => setOpenSections(prev => ({ ...prev, [id]: !prev[id] }));

  useEffect(() => {
    if (!targetSection) return;
    setOpenSections(prev => ({ ...prev, [targetSection]: true }));
    const timer = setTimeout(() => {
      document.getElementById(targetSection)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    }, 80);
    return () => clearTimeout(timer);
  }, [targetSection]);

  useEffect(() => {
    fetch('/api/settings')
      .then(res => res.ok ? res.json() : null)
      .then(data => {
        if (data) setPublicBaseUrl(data.public_base_url || '');
      })
      .catch(() => {});
  }, []);

  // The build stamps its own version in, so Settings can always state what it
  // is even with the backend down. The call below only adds the SERVER's
  // version (to catch a stale backend behind a fresh frontend) and powers the
  // update check — it is never what makes the version appear.
  const appVersion = import.meta.env.VITE_APP_VERSION || null;
  const isDemo = !!import.meta.env.VITE_DEMO;

  useEffect(() => {
    fetch('/api/settings/version')
      .then(res => (res.ok ? res.json() : Promise.reject(new Error(`HTTP ${res.status}`))))
      .then(data => { setVersionInfo(data); setBackendReachable(true); })
      .catch(() => setBackendReachable(false));
  }, []);

  const handleCheckUpdate = async () => {
    setCheckingUpdate(true);
    try {
      const res = await fetch('/api/settings/version?check=1');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      setVersionInfo(data);
      setBackendReachable(true);
      if (data.check_failed) showToast(t('settings.updateNoGithub'));
      else if (data.update_available) showToast(t('settings.updateAvailable', { version: data.latest }));
      else showToast(t('settings.updateLatest'));
    } catch (err) {
      console.error(err);
      setBackendReachable(false);
      showToast(t('settings.updateNoServer'));
    } finally {
      setCheckingUpdate(false);
    }
  };

  // The version shown: the build's own stamp, falling back to whatever the
  // server reports if an older bundle has no stamp baked in.
  const shownVersion = appVersion || versionInfo?.version || null;
  // A frontend newer than the backend usually means a half-finished update —
  // worth surfacing, since it produces confusing bugs that look like app bugs.
  const versionSkew = appVersion && versionInfo?.version && appVersion !== versionInfo.version
    ? versionInfo.version
    : null;

  // Prefill a bug report with the details that otherwise take three round trips
  // to obtain. Environment only — nothing about the user's collection.
  const bugReportUrl = () => {
    const body = [
      '### What happened?',
      '',
      '',
      '### What did you expect?',
      '',
      '',
      '### Steps to reproduce',
      '1. ',
      '2. ',
      '',
      '### Environment',
      `- Scrybox (app): ${shownVersion || 'unknown'}`,
      `- Scrybox (server): ${versionInfo?.version || (backendReachable ? 'unknown' : 'unreachable')}`,
      `- Platform: ${navigator.platform || 'unknown'}`,
      `- Browser: ${navigator.userAgent}`,
      `- Screen: ${window.screen?.width}x${window.screen?.height}`,
      '',
      '<!-- Screenshots help a lot. Please remove anything you would rather not share. -->',
    ].join('\n');
    return `${REPO_URL}/issues/new?labels=bug&title=${encodeURIComponent('[Bug] ')}&body=${encodeURIComponent(body)}`;
  };

  const featureRequestUrl = () => {
    const body = [
      '### What would you like Scrybox to do?',
      '',
      '',
      '### Why would that help?',
      '',
      '',
      `<!-- Scrybox ${shownVersion || 'unknown'} -->`,
    ].join('\n');
    return `${REPO_URL}/issues/new?labels=enhancement&title=${encodeURIComponent('[Feature] ')}&body=${encodeURIComponent(body)}`;
  };

  const handleImportFile = async (e) => {
    const file = e.target.files[0];
    if (!file) return;

    if (!window.confirm(t('settings.confirmImport', { file: file.name }))) {
      e.target.value = '';
      return;
    }

    const reader = new FileReader();
    const isJson = file.name.endsWith('.json');
    const format = isJson ? 'json' : 'csv';

    reader.onload = async (event) => {
      try {
        const fileData = event.target.result;
        showToast(t('settings.importing'));
        const response = await fetch('/api/import', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            format,
            data: fileData
          })
        });

        const result = await response.json();
        if (response.ok) {
          showToast(result.message || t('settings.importOk'));
        } else {
          showToast(t('settings.importFailed', { error: result.error || t('settings.unknownError') }));
        }
      } catch (err) {
        console.error(err);
        showToast(t('settings.importFailed', { error: err.message }));
      }
    };

    reader.onerror = () => {
      showToast(t('settings.errReadFile'));
    };

    reader.readAsText(file);
    e.target.value = null;
  };

  useEffect(() => {
    if (user) {
      setShareEnabled(user.share_enabled === 1 || user.share_enabled === true);
    }
  }, [user]);

  const handlePasswordChange = async (e) => {
    e.preventDefault();
    if (!currentPassword) {
      showToast(t('settings.errCurrentPassword'));
      return;
    }
    if (password.length < 8) {
      showToast(t('login.errPasswordShort', { count: 8 }));
      return;
    }
    if (password !== confirmPassword) {
      showToast(t('login.errPasswordMismatch'));
      return;
    }

    setPasswordLoading(true);
    try {
      const response = await fetch('/api/auth/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ current_password: currentPassword, password })
      });

      if (response.ok) {
        showToast(t('settings.passwordUpdated'));
        setCurrentPassword('');
        setPassword('');
        setConfirmPassword('');
      } else {
        const data = await response.json();
        showToast(data.error || t('settings.errPasswordUpdate'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('settings.errPasswordUpdateGeneric'));
    } finally {
      setPasswordLoading(false);
    }
  };

  const handleExport = async (format) => {
    try {
      const response = await fetch(`/api/export?format=${format}`);
      if (!response.ok) {
        showToast(t('settings.errExport'));
        return;
      }
      const blob = await response.blob();
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `scrybox_collection.${format === 'json' ? 'json' : 'csv'}`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (err) {
      console.error(err);
      showToast(t('settings.errExportGeneric'));
    }
  };

  // Text card list of the whole collection, "qty Name" or "qty Name (SET) num".
  // Formatted in the browser from the same /api/collection data the UI shows —
  // the server keeps an identical endpoint (/api/collection/cardlist) for
  // scripts, and cardList.test.js proves the two copies agree.
  const handleExportCardList = async (style) => {
    try {
      const response = await fetch('/api/collection');
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const cards = await response.json();
      const text = buildCardListText(cards, style);
      if (!text) { showToast(t('settings.cardlistEmpty')); return; }
      await navigator.clipboard.writeText(text);
      showToast(t('settings.cardlistCopied'));
    } catch (err) {
      console.error(err);
      showToast(t('settings.errCardlist'));
    }
  };

  const handleShareToggle = async (checked) => {
    setShareEnabled(checked);
    setShareLoading(true);
    try {
      const response = await fetch('/api/auth/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ share_enabled: checked })
      });

      if (response.ok) {
        const data = await response.json();
        onUpdateUser(data.user);
        showToast(t(checked ? 'settings.sharingOn' : 'settings.sharingOff'));
      } else {
        setShareEnabled(!checked); // Revert
        showToast(t('settings.errSharing'));
      }
    } catch (err) {
      console.error(err);
      setShareEnabled(!checked);
      showToast(t('settings.errSharingGeneric'));
    } finally {
      setShareLoading(false);
    }
  };

  const handleRegenerateToken = async () => {
    if (!window.confirm(t('settings.confirmRegenerate'))) {
      return;
    }

    setShareLoading(true);
    try {
      const response = await fetch('/api/auth/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ regenerate_share_token: true })
      });

      if (response.ok) {
        const data = await response.json();
        onUpdateUser(data.user);
        showToast(t('settings.tokenRegenerated'));
      } else {
        showToast(t('settings.errRegenerate'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('settings.errRegenerateGeneric'));
    } finally {
      setShareLoading(false);
    }
  };

  const origin = publicBaseUrl || `${window.location.protocol}//${window.location.host}`;
  const shareUrl = `${origin}/share/${user?.share_token}`;

  const [copiedType, setCopiedType] = useState(''); // 'collection', 'trade'

  const copyToClipboard = (url, type) => {
    navigator.clipboard.writeText(url).then(() => {
      setCopiedType(type);
      showToast(t(`settings.copied.${type}`));
      setTimeout(() => setCopiedType(''), 2000);
    }).catch(() => {
      showToast(t('settings.errCopy'));
    });
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
      {/* Title Panel */}
      <div className="glass-panel">
        <h2 style={{ fontSize: '1.25rem', color: 'var(--text-strong)' }}>{t('settings.title')}</h2>
        <p style={{ color: 'var(--text-secondary)', fontSize: '0.85rem' }}>{t('settings.subtitle')}</p>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: '1fr', gap: '1.5rem' }} className="settings-grid">
        {/* Sharing Panel */}
        <SettingsSection id="settings.sharingTitle" icon={<Share2 size={20} style={{ color: 'var(--accent-red)' }} />} title={t('settings.sharingTitle')} open={!!openSections['settings.sharingTitle']} onToggle={() => toggleSection('settings.sharingTitle')}>

          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', background: 'rgba(255,255,255,0.01)', padding: '0.75rem 1rem', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
            <div>
              <div style={{ fontWeight: 700, color: 'var(--text-strong)', fontSize: '0.95rem' }}>{t('settings.shareLibrary')}</div>
              <div style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>{t('settings.shareLibraryHint')}</div>
            </div>
            <label className="switch-control" style={{ position: 'relative', display: 'inline-block', width: '46px', height: '24px' }}>
              <input 
                type="checkbox" 
                checked={shareEnabled} 
                onChange={(e) => handleShareToggle(e.target.checked)}
                disabled={shareLoading}
                style={{ opacity: 0, width: 0, height: 0 }}
              />
              <span className={`switch-slider ${shareEnabled ? 'active' : ''}`} style={{
                position: 'absolute',
                cursor: 'pointer',
                top: 0, left: 0, right: 0, bottom: 0,
                backgroundColor: shareEnabled ? 'var(--success)' : '#334155',
                transition: '0.3s',
                borderRadius: '24px'
              }}>
                <span style={{
                  position: 'absolute',
                  height: '18px', width: '18px',
                  left: shareEnabled ? '24px' : '4px',
                  bottom: '3px',
                  backgroundColor: '#fff',
                  transition: '0.3s',
                  borderRadius: '50%',
                  boxShadow: '0 2px 4px rgba(0,0,0,0.3)'
                }}></span>
              </span>
            </label>
          </div>

          {shareEnabled && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '1rem', marginTop: '0.5rem' }}>
              
              <div className="form-group" style={{ marginBottom: 0 }}>
                <label>{t('settings.linkCollection')}</label>
                <div style={{ display: 'flex', gap: '0.5rem' }}>
                  <input 
                    type="text" 
                    className="input-control" 
                    value={shareUrl} 
                    readOnly 
                    style={{ flex: 1, backgroundColor: 'rgba(0,0,0,0.2)', color: 'var(--text-secondary)', cursor: 'default' }}
                  />
                  <button className="btn btn-secondary" onClick={() => copyToClipboard(shareUrl, 'collection')} style={{ display: 'flex', alignItems: 'center', gap: '0.25rem', whiteSpace: 'nowrap' }}>
                    {copiedType === 'collection' ? <Check size={14} style={{ color: 'var(--success)' }} /> : <Clipboard size={14} />}
                    <span>{t('settings.copy')}</span>
                  </button>
                </div>
              </div>


              <div style={{ display: 'flex', justifyContent: 'flex-end', marginTop: '0.5rem' }}>
                <button
                  className="btn btn-secondary"
                  onClick={handleRegenerateToken}
                  disabled={shareLoading}
                  style={{ display: 'flex', alignItems: 'center', gap: '0.35rem', fontSize: '0.8rem', padding: '0.4rem 0.8rem' }}
                >
                  <RefreshCw size={12} className={shareLoading ? 'spin-animation' : ''} />
                  <span>{t('settings.regenerateLink')}</span>
                </button>
              </div>
            </div>
          )}

          {!shareEnabled && (
            <div style={{ display: 'flex', gap: '0.5rem', background: 'rgba(255, 71, 71, 0.05)', border: '1px solid rgba(255,71,71,0.1)', padding: '0.75rem', borderRadius: 'var(--radius-sm)', color: 'var(--text-secondary)', fontSize: '0.8rem' }}>
              <ShieldAlert size={16} style={{ color: 'var(--accent-red)', flexShrink: 0 }} />
              <span>{t('settings.privateNotice')}</span>
            </div>
          )}
        </SettingsSection>

        {/* Change Password Panel */}
        <SettingsSection id="settings.securityTitle" icon={<KeyRound size={20} style={{ color: 'var(--accent-yellow)' }} />} title={t('settings.securityTitle')} open={!!openSections['settings.securityTitle']} onToggle={() => toggleSection('settings.securityTitle')}>

          <form onSubmit={handlePasswordChange} style={{ display: 'flex', flexDirection: 'column', gap: '1rem' }}>
            <div className="form-group" style={{ marginBottom: 0 }}>
              <label htmlFor="current-password">{t('settings.currentPassword')}</label>
              <input
                id="current-password"
                type="password"
                name="current-password"
                autoComplete="current-password"
                className="input-control"
                placeholder={t('settings.currentPasswordPlaceholder')}
                value={currentPassword}
                onChange={(e) => setCurrentPassword(e.target.value)}
                required
                disabled={passwordLoading}
              />
            </div>

            <div className="form-group" style={{ marginBottom: 0 }}>
              <label htmlFor="settings-new-password">{t('settings.newPassword')}</label>
              <input
                id="settings-new-password"
                type="password"
                name="new-password"
                autoComplete="new-password"
                className="input-control"
                placeholder={t('settings.newPasswordPlaceholder', { count: 8 })}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
                disabled={passwordLoading}
              />
            </div>

            <div className="form-group" style={{ marginBottom: 0 }}>
              <label htmlFor="settings-confirm-password">{t('login.confirmPassword')}</label>
              <input
                id="settings-confirm-password"
                type="password"
                name="confirm-password"
                autoComplete="new-password"
                className="input-control"
                placeholder={t('login.confirmPasswordPlaceholder')}
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
                required
                disabled={passwordLoading}
              />
            </div>

            <button 
              type="submit" 
              className="btn btn-primary" 
              disabled={passwordLoading}
              style={{ padding: '0.6rem 1.2rem', alignSelf: 'flex-start', display: 'flex', alignItems: 'center', gap: '0.5rem' }}
            >
              {passwordLoading ? (
                <div className="spinner" style={{ width: '14px', height: '14px', margin: 0, borderWidth: '2px' }}></div>
              ) : t('settings.updatePassword')}
            </button>
          </form>
        </SettingsSection>

        {/* Moxfield Sync Panel */}
        <SettingsSection id="moxfield-panel" icon={<Globe size={20} style={{ color: 'var(--success, #4ade80)' }} />} title={t('mfx.title')} open={!!openSections['moxfield-panel']} onToggle={() => toggleSection('moxfield-panel')}>

          <MoxfieldPanel user={user} showToast={showToast} />
        </SettingsSection>

        {/* Marketplace Accounts: credentials for importing cards from a ManaPool
            or TCGplayer order you already placed (the Add Cards -> From order tab).
            Per-user, unlike the instance settings above: a member imports against
            their own marketplace account, and a TCGplayer cookie jar is a
            session-grade secret that must never be echoed back to another user. */}
        <SettingsSection id="marketplace-panel" icon={<ShoppingCart size={20} style={{ color: 'var(--accent-yellow)' }} />} title={t('marketplace.title')} open={!!openSections['marketplace-panel']} onToggle={() => toggleSection('marketplace-panel')}>
          <MarketplaceAccountsPanel showToast={showToast} />
        </SettingsSection>

        {/* Sync from ManaBox: reconcile the collection (and ManaBox lists) with a
            ManaBox CSV export. Preview first, apply second, record kept. */}
        <SettingsSection id="manabox-sync" icon={<RefreshCcw size={20} style={{ color: 'var(--accent-yellow)' }} />} title={t('mbsync.title')} open={!!openSections['manabox-sync']} onToggle={() => toggleSection('manabox-sync')}>
          <ManaboxSyncPanel showToast={showToast} />
        </SettingsSection>

        {/* Collection Backup & Data Options Panel */}
        <SettingsSection id="settings.backupTitle" icon={<Database size={20} style={{ color: 'var(--accent-red)' }} />} title={t('settings.backupTitle')} open={!!openSections['settings.backupTitle']} onToggle={() => toggleSection('settings.backupTitle')}>

          <div style={{ background: 'rgba(255, 255, 255, 0.02)', border: '1px solid var(--border-glass)', padding: '0.75rem 1rem', borderRadius: 'var(--radius-sm)', fontSize: '0.8rem', color: 'var(--text-secondary)', lineHeight: '1.4' }}>
            {t('settings.backupHint')}
          </div>

          <div style={{ display: 'flex', flexWrap: 'wrap', gap: '0.75rem' }}>
            <button
              type="button"
              onClick={() => handleExport('csv')}
              className="btn btn-secondary"
              style={{ display: 'inline-flex', alignItems: 'center', gap: '0.4rem', fontSize: '0.85rem' }}
            >
              <Download size={14} />
              <span>{t('settings.exportCsv')}</span>
            </button>
            <button
              type="button"
              onClick={() => handleExport('json')}
              className="btn btn-secondary"
              style={{ display: 'inline-flex', alignItems: 'center', gap: '0.4rem', fontSize: '0.85rem' }}
            >
              <Download size={14} />
              <span>{t('settings.exportJson')}</span>
            </button>

            <div style={{ width: '100%', height: '0.75rem' }} />

            <button
              type="button"
              onClick={() => handleExportCardList('plain')}
              className="btn btn-secondary"
              style={{ display: 'inline-flex', alignItems: 'center', gap: '0.4rem', fontSize: '0.85rem' }}
              title={t('settings.cardlistHint')}
            >
              <Clipboard size={14} />
              <span>{t('settings.exportCardlistPlain')}</span>
            </button>
            <button
              type="button"
              onClick={() => handleExportCardList('detailed')}
              className="btn btn-secondary"
              style={{ display: 'inline-flex', alignItems: 'center', gap: '0.4rem', fontSize: '0.85rem' }}
              title={t('settings.cardlistHint')}
            >
              <Clipboard size={14} />
              <span>{t('settings.exportCardlistDetailed')}</span>
            </button>

            <label 
              className="btn btn-primary" 
              style={{ display: 'inline-flex', alignItems: 'center', gap: '0.4rem', fontSize: '0.85rem', cursor: 'pointer', margin: 0 }}
            >
              <Upload size={14} />
              <span>{t('settings.importBackup')}</span>
              <input
                type="file"
                accept=".json,.csv"
                onChange={handleImportFile}
                style={{ display: 'none' }}
              />
            </label>
          </div>
        </SettingsSection>

        {/* Preferences Panel */}
        <SettingsSection id="prefs.title" icon={<SlidersHorizontal size={20} style={{ color: 'var(--accent-yellow)' }} />} title={t('prefs.title')} open={!!openSections['prefs.title']} onToggle={() => toggleSection('prefs.title')}>

          {/* Interface language. The picker only appears once a second locale file
              exists to switch to — dropping one into src/locales is what makes it
              appear — but the call for translators shows either way, since with
              English alone there is nothing else to advertise it.
              This is not the card language: that is picked per card on entry. */}
          <div className="form-group" style={{ marginBottom: 0 }}>
            <label htmlFor={LOCALES.length > 1 ? 'settings-ui-lang' : undefined}>{t('prefs.language')}</label>
            {LOCALES.length > 1 ? (
              <>
                <select
                  id="settings-ui-lang"
                  className="select-control"
                  value={locale}
                  onChange={(e) => setLocale(e.target.value)}
                >
                  {LOCALES.map(code => (
                    <option key={code} value={code}>{localeName(code)}</option>
                  ))}
                </select>
                <div style={{ fontSize: '0.75rem', color: 'var(--text-secondary)', marginTop: '0.4rem' }}>
                  {t('prefs.languageHint')}
                </div>
              </>
            ) : (
              <div style={{ fontSize: '0.75rem', color: 'var(--text-secondary)' }}>
                {t('prefs.languageOnlyEnglish')}
              </div>
            )}
            <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', fontSize: '0.75rem', marginTop: '0.5rem' }}>
              <Languages size={13} style={{ color: 'var(--accent-yellow)', flexShrink: 0 }} />
              <span style={{ color: 'var(--text-secondary)' }}>
                {t('prefs.translateCta')}{' '}
                <a
                  href={`${REPO_URL}/blob/main/docs/TRANSLATING.md`}
                  target="_blank"
                  rel="noopener noreferrer"
                  style={{ color: 'var(--accent-yellow)', fontWeight: 600 }}
                >
                  {t('prefs.translateCtaLink')}
                </a>
              </span>
            </div>
          </div>

        </SettingsSection>

        {/* About / version */}
        <SettingsSection id="settings.aboutTitle" icon={<Info size={20} style={{ color: 'var(--accent-yellow)' }} />} title={t('settings.aboutTitle')} open={!!openSections['settings.aboutTitle']} onToggle={() => toggleSection('settings.aboutTitle')}>

          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', flexWrap: 'wrap', gap: '0.75rem', background: 'rgba(255,255,255,0.01)', padding: '0.75rem 1rem', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
            <div>
              <div style={{ fontWeight: 700, color: 'var(--text-strong)', fontSize: '0.95rem', display: 'flex', alignItems: 'center', gap: '0.5rem', flexWrap: 'wrap' }}>
                <span>{shownVersion ? `Scrybox v${shownVersion}` : t('settings.versionUnknown')}</span>
                <button
                  type="button"
                  className="btn btn-secondary"
                  title={t('settings.copyVersionHint')}
                  onClick={() => {
                    const text = `Scrybox app v${shownVersion || 'unknown'} | server v${versionInfo?.version || (backendReachable ? 'unknown' : 'unreachable')} | ${navigator.platform || 'unknown'} | ${navigator.userAgent}`;
                    navigator.clipboard?.writeText(text)
                      .then(() => showToast(t('settings.versionCopied')))
                      .catch(() => showToast(t('settings.errCopyShort')));
                  }}
                  style={{ padding: '0.15rem 0.45rem', fontSize: '0.7rem' }}
                >
                  <Clipboard size={12} /> {t('settings.copy')}
                </button>
              </div>
              <div style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
                {isDemo
                  ? t('settings.updateDemo')
                  : !backendReachable
                  ? t('settings.updateUnreachable')
                  : versionInfo?.check_failed
                    ? t('settings.updateGithubFailed')
                    : versionInfo?.update_available
                      ? t('settings.updateAvailable', { version: versionInfo.latest })
                      : versionInfo?.latest
                        ? t('settings.updateRunningLatest')
                        : t('settings.updateOnDemand')}
              </div>
              {versionSkew && (
                <div style={{ fontSize: '0.75rem', color: 'var(--accent-yellow)', marginTop: '0.25rem' }}>
                  {t('settings.versionSkew', { version: versionSkew })}
                </div>
              )}
            </div>
            <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap' }}>
              {/* The demo answers /api from static fixtures, so a "check" would
                  report "you're up to date" without having checked anything. */}
              <button type="button" className="btn btn-secondary" onClick={handleCheckUpdate} disabled={checkingUpdate || isDemo}>
                <RefreshCw size={16} className={checkingUpdate ? 'spin-animation' : ''} />
                {t(checkingUpdate ? 'settings.checking' : 'settings.checkForUpdates')}
              </button>
              {versionInfo?.update_available && (
                <a
                  className="btn btn-primary"
                  href={versionInfo.release_url}
                  target="_blank"
                  rel="noopener noreferrer"
                  style={{ textDecoration: 'none' }}
                >
                  <Download size={16} />
                  {t('settings.getVersion', { version: versionInfo.latest })}
                </a>
              )}
            </div>
          </div>

          {/* Support links. Each opens GitHub's own compose page in a new tab —
              prefilled, never submitted, so nothing is posted without the user
              reading it and pressing the button on GitHub. */}
          <div>
            <div style={{ fontSize: '0.75rem', fontWeight: 700, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: '0.03em', marginBottom: '0.5rem' }}>
              {t('settings.getInvolvedTitle')}
            </div>
            <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap' }}>
              <a className="btn btn-secondary" href={bugReportUrl()} target="_blank" rel="noopener noreferrer" style={{ textDecoration: 'none' }}>
                <Bug size={16} /> {t('settings.reportBug')}
              </a>
              <a className="btn btn-secondary" href={featureRequestUrl()} target="_blank" rel="noopener noreferrer" style={{ textDecoration: 'none' }}>
                <Lightbulb size={16} /> {t('settings.requestFeature')}
              </a>
              <a className="btn btn-secondary" href={`${REPO_URL}/blob/main/docs/TRANSLATING.md`} target="_blank" rel="noopener noreferrer" style={{ textDecoration: 'none' }}>
                <Languages size={16} /> {t('settings.helpTranslate')}
              </a>
              <a className="btn btn-secondary" href={`${REPO_URL}/issues`} target="_blank" rel="noopener noreferrer" style={{ textDecoration: 'none' }}>
                <MessagesSquare size={16} /> {t('settings.browseIssues')}
              </a>
              <a className="btn btn-secondary" href={`${REPO_URL}/releases`} target="_blank" rel="noopener noreferrer" style={{ textDecoration: 'none' }}>
                <ScrollText size={16} /> {t('settings.changelog')}
              </a>
              <a className="btn btn-secondary" href={REPO_URL} target="_blank" rel="noopener noreferrer" style={{ textDecoration: 'none' }}>
                <Github size={16} /> {t('settings.source')}
              </a>
            </div>
            <div style={{ fontSize: '0.75rem', color: 'var(--text-secondary)', marginTop: '0.5rem', lineHeight: 1.4 }}>
              {t('settings.reportNote')}
            </div>
          </div>

          <div style={{ fontSize: '0.75rem', color: 'var(--text-secondary)' }}>
            {t('settings.updateChecksNote')}{' '}
            <a href={versionInfo?.releases_url || `${REPO_URL}/releases`} target="_blank" rel="noopener noreferrer" style={{ color: 'var(--accent-yellow)' }}>
              Brenttime/scrybox
            </a>
          </div>
        </SettingsSection>

        {/* Admin section: lives inside Settings (no separate tab). Rendered only
            for admin users; the panel itself is code-split behind Suspense. */}
        {user?.role === 'admin' && (
          <SettingsSection id="admin" icon={<ShieldAlert size={20} style={{ color: 'var(--accent-red)' }} />} title={t('admin.title')} open={!!openSections.admin} onToggle={() => toggleSection('admin')}>
            <Suspense fallback={null}>
              <AdminPanel showToast={showToast} />
            </Suspense>
          </SettingsSection>
        )}
      </div>
    </div>
  );
}

export default Settings;
