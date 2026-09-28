import { useState, useEffect, useCallback } from 'react';
import { Plus, Trash2, RefreshCw, CheckCircle2, AlertTriangle, Clock, ExternalLink, Save } from 'lucide-react';
import { useT } from '../utils/i18n';

// Moxfield sync, embedded in the Settings panel (see Settings.jsx, which
// provides the glass-panel wrapper and header). The server runs both jobs on
// its own clocks (see backend moxfieldScheduler); this view just shows where
// things stand, lets you add/remove authors, tune the two intervals, and force
// a re-sync when something looks wrong. It polls every 15s so the "Up to date
// / Stale" badges flip on their own.

const POLL_MS = 15000;

function timeAgo(iso) {
  if (!iso) return null;
  const then = Date.parse(iso.includes('T') ? iso : iso.replace(' ', 'T') + 'Z');
  if (!Number.isFinite(then)) return null;
  const s = Math.floor((Date.now() - then) / 1000);
  if (s < 45) return 'just now';
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

// Compact progress bar of how many cards are in the deck's main slot against
// the format's target (commander 100, constructed 75). Green only when the
// deck hits its target — anything short of that reads red, so a not-quite-
// full deck is obvious at a glance. Paused decks show a muted gray fill
// instead. Rendered at the far right of each deck row.
function CardProgress({ deck }) {
  const { t } = useT();
  const count = deck.card_count;
  const target = deck.card_target;
  if (count == null || !target) return null;
  const pct = Math.max(0, Math.min(100, Math.round((count / target) * 100)));
  const complete = count >= target;
  const activeColor = complete ? 'var(--success, #4ade80)' : 'var(--accent-red, #ff4747)';
  const color = deck.enabled ? activeColor : 'var(--text-muted)';
  const hint = deck.enabled
    ? complete ? t('mfx.completeHint', { target }) : t('mfx.shortHint', { missing: target - count, target })
    : t('mfx.deckPaused');
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: '0.35rem', flexShrink: 0 }} title={hint}>
      <div style={{ width: '3.2rem', height: '0.28rem', borderRadius: '999px', background: 'rgba(255,255,255,0.08)', overflow: 'hidden' }}>
        <div style={{ width: `${pct}%`, height: '100%', borderRadius: '999px', background: color, transition: 'width 0.3s ease' }} />
      </div>
      <span style={{ fontSize: '0.64rem', color: color, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap' }}>
        {count}/{target}
      </span>
    </div>
  );
}

function StatusBadge({ deck }) {
  const { t } = useT();
  if (!deck.enabled) {
    return (
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: '0.3rem', color: 'var(--text-muted)', fontSize: '0.78rem', fontWeight: 600 }}>
        <Clock size={13} /> {t('mfx.statusNotSyncing')}
      </span>
    );
  }
  if (deck.last_error) {
    return (
      <span title={deck.last_error} style={{ display: 'inline-flex', alignItems: 'center', gap: '0.3rem', color: 'var(--accent-red)', fontSize: '0.78rem', fontWeight: 600 }}>
        <AlertTriangle size={13} /> {t('mfx.statusError')}
      </span>
    );
  }
  if (deck.current) {
    return (
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: '0.3rem', color: 'var(--success, #4ade80)', fontSize: '0.78rem', fontWeight: 600 }}>
        <CheckCircle2 size={13} /> {t('mfx.statusCurrent')}
      </span>
    );
  }
  return (
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: '0.3rem', color: 'var(--accent-yellow)', fontSize: '0.78rem', fontWeight: 600 }}>
      <Clock size={13} /> {deck.last_synced_updated_at ? t('mfx.statusStale') : t('mfx.statusNever')}
    </span>
  );
}

function AuthorCard({ author, onRemove, onSyncDecklist, onSyncContents, onSyncDeck, onToggleDeck, busy }) {
  const { t } = useT();
  const lastDecklist = timeAgo(author.last_decklist_sync_at);
  const lastContentCheck = timeAgo(author.last_content_check_at);
  const lastContent = timeAgo(author.last_content_sync);
  // Moxfield avatars send no CORS/CORP headers, so an iPhone (COEP
  // require-corp, for the scanner's threads) blocks them: fall back to the
  // initials. Keyed on the URL so a new avatar is tried again.
  const [failedAvatar, setFailedAvatar] = useState(null);
  const avatar = author.profile_image_url && failedAvatar !== author.profile_image_url ? author.profile_image_url : null;

  return (
    <div className="glass-panel" style={{ padding: '1rem', display: 'flex', flexDirection: 'column', gap: '0.75rem' }}>
      {/* Author header */}
      <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', flexWrap: 'wrap' }}>
        {avatar ? (
          <img src={avatar} alt="" onError={() => setFailedAvatar(avatar)} style={{ width: '2.25rem', height: '2.25rem', borderRadius: '50%', objectFit: 'cover', border: '1px solid var(--border-glass)' }} />
        ) : (
          <div style={{ width: '2.25rem', height: '2.25rem', borderRadius: '50%', background: 'var(--surface-3)', display: 'flex', alignItems: 'center', justifyContent: 'center', color: 'var(--text-secondary)', fontSize: '0.9rem', fontWeight: 700 }}>
            {(author.display_name || author.moxfield_user || '?').slice(0, 2).toUpperCase()}
          </div>
        )}
        <div style={{ flex: 1, minWidth: '8rem' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem' }}>
            <strong style={{ color: 'var(--text-strong)', fontSize: '1rem' }}>{author.display_name || author.moxfield_user}</strong>
            <a
              href={`https://moxfield.com/mage/${encodeURIComponent(author.moxfield_user)}`}
              target="_blank"
              rel="noreferrer"
              title={`moxfield.com/mage/${author.moxfield_user}`}
              style={{ color: 'var(--text-muted)', display: 'inline-flex' }}
            >
              <ExternalLink size={13} />
            </a>
          </div>
          <div style={{ color: 'var(--text-secondary)', fontSize: '0.78rem' }}>
            @{author.moxfield_user} · {t('mfx.deckCount', { count: author.tracked_decks })}
            {author.tracked_decks > author.total_decks && (
              <span title={t('mfx.pausedHint')} style={{ color: 'var(--text-muted)' }}>
                {' '}· <span style={{ color: 'var(--success, #4ade80)', fontWeight: 600 }}>{t('mfx.syncingCount', { count: author.total_decks })}</span>
              </span>
            )}
          </div>
        </div>
        <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap' }}>
          <button
            className="btn btn-secondary"
            disabled={busy !== null}
            onClick={() => onSyncDecklist(author)}
            style={{ padding: '0.4rem 0.7rem', fontSize: '0.8rem' }}
            title={t('mfx.decklistRefreshHint')}
          >
            <RefreshCw size={14} /> {t('mfx.decklistRefresh')}
          </button>
          <button
            className="btn btn-secondary"
            disabled={busy !== null}
            onClick={() => onSyncContents(author)}
            style={{ padding: '0.4rem 0.7rem', fontSize: '0.8rem' }}
            title={t('mfx.syncNowHint')}
          >
            <RefreshCw size={14} /> {t('mfx.syncNow')}
          </button>
          <button
            className="btn btn-danger btn-icon-only"
            disabled={busy !== null}
            onClick={() => onRemove(author)}
            style={{ padding: '0.4rem', color: 'var(--accent-red)' }}
            title={t('mfx.removeAuthor')}
            aria-label={t('mfx.removeAuthor')}
          >
            <Trash2 size={14} />
          </button>
        </div>
      </div>

      {/* Sync state line */}
      <div style={{ display: 'flex', gap: '1rem', flexWrap: 'wrap', color: 'var(--text-secondary)', fontSize: '0.78rem' }}>
        <span>{t('mfx.lastDecklist')}: <strong style={{ color: 'var(--text-primary)' }}>{lastDecklist || t('mfx.never')}</strong></span>
        <span title={t('mfx.contentCheckHint')}>{t('mfx.lastContentCheck')}: <strong style={{ color: 'var(--success, #4ade80)' }}>{lastContentCheck || t('mfx.never')}</strong></span>
        <span title={t('mfx.lastPullHint')}>{t('mfx.lastContent')}: <strong style={{ color: 'var(--text-primary)' }}>{lastContent || t('mfx.never')}</strong></span>
        {author.last_error && (
          <span style={{ color: 'var(--accent-red)', display: 'inline-flex', alignItems: 'center', gap: '0.3rem' }}>
            <AlertTriangle size={12} /> {author.last_error}
          </span>
        )}
      </div>

      {/* Deck rows */}
      {author.decks.length > 0 && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem' }}>
          {author.decks.map(deck => (
            <div key={deck.public_id} style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', padding: '0.45rem 0.6rem', background: 'var(--surface-1)', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)', opacity: deck.enabled ? 1 : 0.62 }}>
              <label style={{ display: 'inline-flex', alignItems: 'center', gap: '0.4rem', cursor: 'pointer', flexShrink: 0, userSelect: 'none' }} title={deck.enabled ? t('mfx.pauseDeckHint') : t('mfx.resumeDeckHint')}>
                <input
                  type="checkbox"
                  checked={deck.enabled}
                  disabled={busy !== null}
                  onChange={() => onToggleDeck(deck)}
                  style={{ accentColor: 'var(--accent-blue, #3b82f6)', width: '0.95rem', height: '0.95rem', cursor: 'pointer' }}
                />
                <span style={{ fontSize: '0.72rem', color: 'var(--text-secondary)' }}>{t('mfx.syncThisDeck')}</span>
              </label>
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', flexWrap: 'wrap' }}>
                  <span style={{ color: 'var(--text-strong)', fontWeight: 600, fontSize: '0.88rem', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {deck.name}
                  </span>
                  <StatusBadge deck={deck} />
                </div>
                <div style={{ color: 'var(--text-muted)', fontSize: '0.72rem', marginTop: '0.15rem' }}>
                  {deck.format || '—'}
                  {deck.mainboard_count != null && <> · {t('mfx.mainboard')}: {deck.mainboard_count}</>}
                  {deck.sideboard_count != null && <> · {t('mfx.sideboard')}: {deck.sideboard_count}</>}
                  {deck.last_content_sync_at && <> · {t('mfx.lastContent')}: {timeAgo(deck.last_content_sync_at)}</>}
                </div>
              </div>
              <CardProgress deck={deck} />
              <a
                href={`https://moxfield.com/decks/${deck.public_id}`}
                target="_blank"
                rel="noreferrer"
                style={{ color: 'var(--text-muted)', display: 'inline-flex', flexShrink: 0 }}
                title={t('mfx.openOnMoxfield')}
              >
                <ExternalLink size={13} />
              </a>
              <button
                className="btn btn-secondary"
                disabled={busy !== null || !deck.enabled}
                onClick={() => onSyncDeck(deck)}
                style={{ padding: '0.3rem 0.55rem', fontSize: '0.75rem', flexShrink: 0 }}
                title={deck.enabled ? t('mfx.resyncHint') : t('mfx.resumeDeckHint')}
              >
                <RefreshCw size={12} /> {t('mfx.resync')}
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export default function MoxfieldPanel({ showToast, user }) {
  const { t } = useT();
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(null); // 'decklist' | 'contents' | 'deck' | null
  const [newUsername, setNewUsername] = useState('');
  const [adding, setAdding] = useState(false);
  const [decklistMin, setDecklistMin] = useState(60);
  const [contentMin, setContentMin] = useState(1);
  const [savingIntervals, setSavingIntervals] = useState(false);

  const load = useCallback(async () => {
    try {
      const r = await fetch('/api/moxfield');
      if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || 'load failed');
      const d = await r.json();
      setData(d);
      if (d.intervals) {
        setDecklistMin(d.intervals.decklist_min);
        setContentMin(d.intervals.content_min);
      }
      setError(null);
    } catch (err) {
      setError(err.message || t('mfx.errLoad'));
    }
  }, [t]);

  useEffect(() => {
    load();
    const timer = setInterval(load, POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  const addAuthor = async () => {
    const username = newUsername.trim();
    if (!username) return;
    setAdding(true);
    try {
      const r = await fetch('/api/moxfield/authors', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ username })
      });
      const d = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(d.error || t('mfx.errAdd'));
      setNewUsername('');
      showToast?.(t('mfx.added', { name: d.author || username, count: d.decks_created || 0 }));
      await load();
    } catch (err) {
      showToast?.(err.message || t('mfx.errAdd'));
    } finally {
      setAdding(false);
    }
  };

  const removeAuthor = async (author) => {
    if (!window.confirm(t('mfx.confirmRemove', { name: author.display_name || author.moxfield_user }))) return;
    setBusy('remove');
    try {
      const r = await fetch(`/api/moxfield/authors/${author.id}`, { method: 'DELETE' });
      const d = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(d.error || t('mfx.errGeneric'));
      showToast?.(t('mfx.removed'));
      await load();
    } catch (err) {
      showToast?.(err.message || t('mfx.errGeneric'));
    } finally {
      setBusy(null);
    }
  };

  const syncDecklist = async (author) => {
    setBusy('decklist');
    try {
      const r = await fetch(`/api/moxfield/authors/${author.id}/sync-decklist`, { method: 'POST' });
      const d = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(d.error || t('mfx.errGeneric'));
      showToast?.(t('mfx.decklistDone', { created: d.decks_created || 0, removed: d.decks_removed || 0 }));
      await load();
    } catch (err) {
      showToast?.(err.message || t('mfx.errGeneric'));
    } finally {
      setBusy(null);
    }
  };

  const syncContents = async (author) => {
    setBusy('contents');
    try {
      const r = await fetch(`/api/moxfield/authors/${author.id}/sync-contents`, { method: 'POST' });
      const d = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(d.error || t('mfx.errGeneric'));
      showToast?.(t('mfx.contentsDone', { updated: d.updated || 0, checked: d.checked || 0 }));
      await load();
    } catch (err) {
      showToast?.(err.message || t('mfx.errGeneric'));
    } finally {
      setBusy(null);
    }
  };

  const syncDeck = async (deck) => {
    setBusy('deck');
    try {
      const r = await fetch(`/api/moxfield/decks/${deck.public_id}/sync`, { method: 'POST' });
      const d = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(d.error || t('mfx.errGeneric'));
      showToast?.(t('mfx.deckSynced', { name: d.deck || deck.name }));
      await load();
    } catch (err) {
      showToast?.(err.message || t('mfx.errGeneric'));
    } finally {
      setBusy(null);
    }
  };

  const toggleDeck = async (deck) => {
    setBusy('deck');
    try {
      const r = await fetch(`/api/moxfield/decks/${deck.public_id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled: !deck.enabled })
      });
      const d = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(d.error || t('mfx.errGeneric'));
      showToast?.(deck.enabled ? t('mfx.deckPaused', { name: deck.name }) : t('mfx.deckResumed', { name: deck.name }));
      await load();
    } catch (err) {
      showToast?.(err.message || t('mfx.errGeneric'));
    } finally {
      setBusy(null);
    }
  };

  const saveIntervals = async () => {
    setSavingIntervals(true);
    try {
      const r = await fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          moxfield_decklist_interval_min: parseInt(decklistMin, 10) || 60,
          moxfield_content_interval_min: parseInt(contentMin, 10) || 1
        })
      });
      const d = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(d.error || t('mfx.errGeneric'));
      setDecklistMin(d.moxfield_decklist_interval_min || decklistMin);
      setContentMin(d.moxfield_content_interval_min || contentMin);
      showToast?.(t('mfx.intervalsSaved'));
    } catch (err) {
      showToast?.(err.message || t('mfx.errGeneric'));
    } finally {
      setSavingIntervals(false);
    }
  };

  const isAdmin = user && user.role === 'admin';

  return (
    <>
      {/* Intro + add author */}
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: '1rem', flexWrap: 'wrap' }}>
        <div style={{ color: 'var(--text-secondary)', fontSize: '0.8rem', maxWidth: '34rem', lineHeight: 1.45 }}>{t('mfx.subtitle')}</div>
        <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap' }}>
          <input
            className="input-control"
            value={newUsername}
            onChange={e => setNewUsername(e.target.value)}
            onKeyDown={e => e.key === 'Enter' && addAuthor()}
            placeholder={t('mfx.usernamePlaceholder')}
            style={{ width: '14rem' }}
          />
          <button className="btn btn-primary" onClick={addAuthor} disabled={adding || !newUsername.trim()}>
            <Plus size={16} /> {adding ? t('mfx.adding') : t('mfx.addAuthor')}
          </button>
        </div>
      </div>

      {/* Intervals */}
      <div>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: '1rem', flexWrap: 'wrap' }}>
          <div style={{ flex: 1, minWidth: '20rem' }}>
            <div style={{ fontWeight: 600, color: 'var(--text-strong)', fontSize: '0.9rem' }}>{t('mfx.intervals')}</div>
            <div style={{ color: 'var(--text-secondary)', fontSize: '0.78rem', marginTop: '0.2rem' }}>{t('mfx.intervalsHint')}</div>
          </div>
          <div style={{ display: 'flex', alignItems: 'flex-end', gap: '0.75rem', flexWrap: 'wrap' }}>
            <div className="form-group" style={{ marginBottom: 0 }}>
              <label>{t('mfx.decklistInterval')}</label>
              <input
                type="number" min="1" max="1440" className="input-control"
                value={decklistMin}
                onChange={e => setDecklistMin(e.target.value)}
                style={{ width: '6.5rem' }}
              />
            </div>
            <div className="form-group" style={{ marginBottom: 0 }}>
              <label>{t('mfx.contentInterval')}</label>
              <input
                type="number" min="1" max="1440" className="input-control"
                value={contentMin}
                onChange={e => setContentMin(e.target.value)}
                style={{ width: '6.5rem' }}
              />
            </div>
            <button className="btn btn-secondary" onClick={saveIntervals} disabled={savingIntervals || !isAdmin} title={!isAdmin ? t('mfx.intervalsAdminOnly') : ''}>
              <Save size={15} /> {t('common.save')}
            </button>
          </div>
        </div>
        {!isAdmin && <div style={{ color: 'var(--text-muted)', fontSize: '0.75rem', marginTop: '0.5rem' }}>{t('mfx.intervalsAdminOnly')}</div>}
      </div>

      {/* Errors */}
      {error && (
        <div style={{ padding: '0.75rem 1rem', borderRadius: 'var(--radius-sm)', background: 'rgba(255,0,0,0.08)', border: '1px solid rgba(255,0,0,0.25)', color: 'var(--accent-red)', fontSize: '0.85rem' }}>
          {error}
        </div>
      )}

      {/* Authors */}
      {!data && !error ? (
        <div style={{ padding: '2rem', textAlign: 'center', color: 'var(--text-secondary)' }}><div className="spinner" aria-label={t('common.loading')} /></div>
      ) : (data && data.authors.length === 0 && !error) ? (
        <div style={{ padding: '2rem', textAlign: 'center', color: 'var(--text-secondary)', background: 'rgba(255,255,255,0.01)', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
          <div style={{ fontSize: '1rem', fontWeight: 600, color: 'var(--text-primary)' }}>{t('mfx.noAuthors')}</div>
          <div style={{ fontSize: '0.85rem', marginTop: '0.4rem' }}>{t('mfx.noAuthorsHint')}</div>
        </div>
      ) : null}

      {(data?.authors || []).map(author => (
        <AuthorCard
          key={author.id}
          author={author}
          busy={busy}
          onRemove={removeAuthor}
          onSyncDecklist={syncDecklist}
          onSyncContents={syncContents}
          onSyncDeck={syncDeck}
          onToggleDeck={toggleDeck}
        />
      ))}
    </>
  );
}
