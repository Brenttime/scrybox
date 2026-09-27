import React, { useState, useEffect, useRef, lazy, Suspense } from 'react';
import { LayoutDashboard, Database, Sparkles, Settings as SettingsIcon, LogOut, Plus, Swords, ListChecks } from 'lucide-react';
import Login from './components/Login';
import Logo from './components/Logo';
import PullToRefresh from './components/PullToRefresh';
import { scrollToTop } from './utils/scrollRoot';
import { pushBackGuard } from './utils/useBackGuard';
import { getRememberedTab, rememberView, clearRememberedView, forgetOpenSubviews } from './utils/viewMemory';
import { useT } from './utils/i18n';
import { readTcgImportHash, TCG_IMPORT_KEY } from './utils/tcgBookmarklet';

// View components are code-split so heavy deps (recharts in the chart views)
// load on demand instead of in the initial bundle.
// A deploy replaces the hashed chunk files. A tab opened before the deploy
// still asks for the old names and gets "Failed to fetch dynamically imported
// module". Reload once to pick up the new index.html (guarded so a real
// outage can't loop), and retry once first for a flaky network.
const CHUNK_RELOAD_KEY = 'scrybox.chunkReloadAt';
function isChunkError(err) {
  return /dynamically imported module|Importing a module script failed|error loading dynamically imported|Unable to preload CSS/i.test(String(err?.message || err));
}
function reloadForNewDeploy() {
  const last = Number(sessionStorage.getItem(CHUNK_RELOAD_KEY) || 0);
  if (Date.now() - last < 30000) return false;
  sessionStorage.setItem(CHUNK_RELOAD_KEY, String(Date.now()));
  window.location.reload();
  return true;
}
if (typeof window !== 'undefined') {
  window.addEventListener('vite:preloadError', (event) => {
    if (reloadForNewDeploy()) event.preventDefault();
  });
}
const lazyView = (load) => lazy(() => load().catch(err => {
  if (!isChunkError(err)) throw err;
  return new Promise(r => setTimeout(r, 400)).then(load).catch(err2 => {
    if (isChunkError(err2) && reloadForNewDeploy()) return new Promise(() => {});
    throw err2;
  });
}));

const Dashboard = lazyView(() => import('./components/Dashboard'));
const AddCards = lazyView(() => import('./components/AddCards'));
const CollectionList = lazyView(() => import('./components/CollectionList'));
const Settings = lazyView(() => import('./components/Settings'));
const SetupWizard = lazyView(() => import('./components/SetupWizard'));
const SharedCollection = lazyView(() => import('./components/SharedCollection'));
const DeckBuilder = lazyView(() => import('./components/DeckBuilder'));
const Lists = lazyView(() => import('./components/Lists'));
const LimitedLands = lazyView(() => import('./components/LimitedLands'));
const Rules = lazyView(() => import('./components/Rules'));
const Keywords = lazyView(() => import('./components/Keywords'));

class ErrorBoundary extends React.Component {
  constructor(props) {
    super(props);
    this.state = { hasError: false, error: null };
  }
  static getDerivedStateFromError(error) {
    return { hasError: true, error };
  }
  componentDidCatch(error, errorInfo) {
    // Nested lazy chunks (charts, card inspector, admin) land here too.
    if (isChunkError(error) && reloadForNewDeploy()) return;
    console.error("ErrorBoundary caught an error", error, errorInfo);
  }
  render() {
    if (this.state.hasError) {
      // Class component, so no hook: App hands t down as a prop.
      const t = this.props.t;
      return (
        <div style={{ padding: '2rem', color: 'var(--text-strong)', background: 'rgba(255,0,0,0.1)', border: '1px solid red', borderRadius: '8px', margin: '2rem' }}>
          <h2 style={{ fontSize: '1.2rem', marginBottom: '1rem', color: 'var(--accent-red)' }}>{t('error.crashed')}</h2>
          <pre style={{ whiteSpace: 'pre-wrap', color: '#ff8888', background: 'rgba(0,0,0,0.2)', padding: '1rem', borderRadius: '4px', fontSize: '0.85rem' }}>{this.state.error && this.state.error.toString()}</pre>
          <pre style={{ whiteSpace: 'pre-wrap', fontSize: '0.75rem', marginTop: '1rem', color: 'var(--text-secondary)' }}>{this.state.error && this.state.error.stack}</pre>
          <button className="btn btn-primary" style={{ marginTop: '1.5rem' }} onClick={() => window.location.reload()}>{t('error.reload')}</button>
        </div>
      );
    }
    return this.props.children;
  }
}

// Fallback shown while a lazily-loaded view chunk is fetched.
function ChunkFallback() {
  const { t } = useT();
  return (
    <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', padding: '4rem', color: 'var(--text-secondary)' }}>
      <div className="spinner" aria-label={t('common.loading')} />
    </div>
  );
}

// Global fetch interceptor to append authorization headers and handle 401s
const originalFetch = window.fetch;
window.fetch = function (input, options = {}) {
  // `input` may be a string, a URL, or a Request object — normalize before using string methods.
  const url = typeof input === 'string' ? input : (input && input.url) || '';
  const isPublicOrAuthRoute = url.includes('/api/shared/') || url.includes('/api/auth/login') || url.includes('/api/auth/register') || url.includes('/api/auth/bootstrap');

  const token = localStorage.getItem('bindarr_token');
  const finalOptions = { ...options };
  if (token && url.startsWith('/api/') && !isPublicOrAuthRoute) {
    finalOptions.headers = {
      ...finalOptions.headers,
      'Authorization': `Bearer ${token}`
    };
  }
  return originalFetch(input, finalOptions).then(response => {
    if (response.status === 401 && !isPublicOrAuthRoute) {
      // Dispatch custom event to trigger logout without page refresh
      window.dispatchEvent(new Event('bindarr_logout'));
    }
    return response;
  });
};

function App() {
  const { t } = useT();
  const [token, setToken] = useState(localStorage.getItem('bindarr_token'));
  const [user, setUser] = useState(() => {
    try {
      const u = localStorage.getItem('bindarr_user');
      return u ? JSON.parse(u) : null;
    } catch {
      return null;
    }
  });

  // Boot where the refresh found us, not on Dashboard. Only meaningful once
  // we know we are logged in and not on a share route — see the effect below
  // that reconciles this with token/user/share state.
  const [activeTab, setActiveTab] = useState(() => {
    // TCGplayer bookmarklet handoff: stash the order (it may need to survive a
    // login first), scrub it from the URL, and boot straight into Add Cards.
    const handoff = readTcgImportHash(window.location.hash);
    if (handoff) {
      try { sessionStorage.setItem(TCG_IMPORT_KEY, JSON.stringify(handoff)); } catch { /* storage blocked */ }
      window.history.replaceState({}, document.title, window.location.pathname + window.location.search);
      rememberView('add-cards');
      return 'add-cards';
    }
    return getRememberedTab();
  });
  // First-run scanning setup. Asked once per session, only for an admin, and only
  // while it is genuinely incomplete — setupNeeded() reads the same endpoints the
  // wizard does so there is one definition of 'set up'.
  const [showSetup, setShowSetup] = useState(false);
  const [selectedCardFilter, setSelectedCardFilter] = useState('');
  const [toast, setToast] = useState(null);
  const [statsTrigger, setStatsTrigger] = useState(0);
  // Deep-link into a Settings panel when another tab jumps us there
  // (e.g. the deck list's "Moxfield Sync" button). Consumed on the next
  // Settings mount. Navigating anywhere else clears it.
  const [settingsTarget, setSettingsTarget] = useState(null);
  const [listsHandoff, setListsHandoff] = useState(null);

  const tabGuardRef = useRef(null);

  // Every tab is a fresh page: start it at the top. Without this the window
  // keeps the previous tab's scrollY (clamped to the new, shorter document),
  // so on mobile a page often opened partway down. The browser's own restore
  // is off too: our pushState guards share one URL, so 'auto' re-applied
  // stale offsets on reload/back.
  useEffect(() => {
    if ('scrollRestoration' in window.history) window.history.scrollRestoration = 'manual';
  }, []);
  useEffect(() => {
    scrollToTop(0);
  }, [activeTab]);

  // Navigate tabs through here so each change pushes a history entry: a back
  // gesture then returns to the PREVIOUS tab (not always dashboard), and modals
  // stack their own guards on top. We never dispose the old tab guard — each
  // switch pushes a fresh entry so the back button walks through tab history.
  // Disposing with history.back() would race during rapid switches and navigate
  // the browser past the app origin into about:blank.
  const goTab = (tab, target) => {
    if (tab === activeTab) return;
    const prev = activeTab;
    tabGuardRef.current = pushBackGuard(() => {
      tabGuardRef.current = null;
      forgetOpenSubviews();
      setActiveTab(prev);
      rememberView(prev); // back is a real navigation; the memory follows it
    });
    forgetOpenSubviews(); // deep reopen is for refresh only, not tab hops
    setActiveTab(tab);
    rememberView(tab);
    setSettingsTarget(tab === 'settings' ? target : null);
    if (tab === 'lists') setListsHandoff(target || null);
  };

  // Detect public share route on load
  const [shareToken] = useState(() => {
    const path = window.location.pathname;
    const match = path.match(/^\/share\/([a-zA-Z0-9_-]+)$/);
    return match ? match[1] : null;
  });

  // Keep the remembered place honest with who is looking. Logged out or on a
  // share page there is no place to keep (and a share visitor must never be
  // teleported into someone's saved view), so drop the key. Logged in, the key
  // wins — that is the case where the account changed under the app (logout ->
  // login as someone else) and the booted tab belongs to the previous user.
  useEffect(() => {
    if (shareToken || !token || !user) {
      clearRememberedView();
      return;
    }
    const remembered = getRememberedTab();
    if (remembered !== activeTab) setActiveTab(remembered);
    // Only the login/logout/share transitions matter; a deliberate tab click
    // already wrote the key before it changed activeTab.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shareToken, token, user]);

  const showToast = (message) => {
    setToast(message);
  };

  // Handle OIDC / SSO token in URL redirect
  useEffect(() => {
    try {
      const params = new URLSearchParams(window.location.search);
      const oidcToken = params.get('oidc_token') || params.get('token');
      if (oidcToken && !token) {
        // Clean URL parameters immediately
        const newUrl = window.location.pathname + (window.location.hash || '');
        window.history.replaceState({}, document.title, newUrl);

        // Fetch user profile with the token to complete login
        fetch('/api/auth/me', {
          headers: { 'Authorization': `Bearer ${oidcToken}` }
        })
          .then(res => res.ok ? res.json() : Promise.reject(new Error('Failed to fetch profile')))
          .then(data => {
            if (data.user) {
              setToken(oidcToken);
              setUser(data.user);
              localStorage.setItem('bindarr_token', oidcToken);
              localStorage.setItem('bindarr_user', JSON.stringify(data.user));
              showToast(t('toast.welcomeBack', { name: data.user.username }));
              setActiveTab('dashboard');
            }
          })
          .catch(err => {
            console.error('OIDC token verification failed:', err);
            showToast(t('login.errOidcFailed'));
          });
      }
    } catch { /* ignore URL parse error */ }
  }, [t, token]);

  useEffect(() => {
    if (toast) {
      const timer = setTimeout(() => {
        setToast(null);
      }, 2500);
      return () => clearTimeout(timer);
    }
  }, [toast]);

  // Offer first-run setup to an admin until it is finished or skipped. The flag
  // lives on the server, so closing the wizard halfway resumes it at the next
  // login on any device; everything it configures is also reachable from Settings
  // and Admin.
  useEffect(() => {
    if (!user || user.role !== 'admin') return;
    let cancelled = false;
    import('./components/SetupWizard')
      .then(m => m.setupNeeded())
      .then(needed => { if (needed && !cancelled) setShowSetup(true); })
      .catch(() => { /* never block the app on the wizard's own probe */ });
    return () => { cancelled = true; };
  }, [user]);

  // Handle automatic logout on 401
  useEffect(() => {
    const handleAutoLogout = () => {
      // Clear the place-memory while the user is still in state — after
      // removeItem below, viewMemory could not tell whose entry to drop.
      if (user && user.username) clearRememberedView(user.username);
      setToken(null);
      setUser(null);
      localStorage.removeItem('bindarr_token');
      localStorage.removeItem('bindarr_user');
      showToast(t('toast.sessionExpired'));
    };
    window.addEventListener('bindarr_logout', handleAutoLogout);
    return () => window.removeEventListener('bindarr_logout', handleAutoLogout);
  }, [t, user]);

  // Pointer-reactive foil: one delegated listener drives --px/--py (0-100%) on
  // whichever card the pointer is over, so the foil rainbow tracks the cursor.
  // CSS custom props inherit down to the overlay div.
  useEffect(() => {
    const onMove = (e) => {
      const card = e.target.closest && e.target.closest('.tilt-card-wrapper');
      if (!card) return;
      const r = card.getBoundingClientRect();
      card.style.setProperty('--px', `${((e.clientX - r.left) / r.width) * 100}%`);
      card.style.setProperty('--py', `${((e.clientY - r.top) / r.height) * 100}%`);
      card.classList.add('foil-active');
    };
    const onLeave = (e) => {
      const card = e.target.closest && e.target.closest('.tilt-card-wrapper');
      if (card) card.classList.remove('foil-active');
    };
    document.addEventListener('pointermove', onMove, { passive: true });
    document.addEventListener('pointerout', onLeave, { passive: true });
    return () => {
      document.removeEventListener('pointermove', onMove);
      document.removeEventListener('pointerout', onLeave);
    };
  }, []);

  const handleLoginSuccess = (newToken, newUser) => {
    setToken(newToken);
    setUser(newUser);
    localStorage.setItem('bindarr_token', newToken);
    localStorage.setItem('bindarr_user', JSON.stringify(newUser));
    showToast(t('toast.welcomeBack', { name: newUser.username }));
    // Welcome starts at Dashboard. Overwrite the remembered entry with it:
    // logout only *tries* to clear the key (private mode can refuse
    // removeItem), so a stale entry must not drag a fresh login into the last
    // session's tab.
    setActiveTab('dashboard');
    rememberView('dashboard');
  };

  const handleLogout = () => {
    // Revoke token on server asynchronously
    fetch('/api/auth/logout', { method: 'POST' }).catch(err => console.error(err));

    // Same ordering rule as the auto-logout: forget the place while we still
    // know whose place it is.
    if (user && user.username) clearRememberedView(user.username);
    setToken(null);
    setUser(null);
    localStorage.removeItem('bindarr_token');
    localStorage.removeItem('bindarr_user');
    showToast(t('toast.loggedOut'));
  };

  const handleUpdateUser = (updatedUser) => {
    setUser(updatedUser);
    localStorage.setItem('bindarr_user', JSON.stringify(updatedUser));
  };

  const triggerRefresh = () => {
    setStatsTrigger(prev => prev + 1);
  };

  // Render shared collection view if URL matches /share/:token
  if (shareToken) {
    return (
      <Suspense fallback={<ChunkFallback />}>
        <SharedCollection shareToken={shareToken} />
      </Suspense>
    );
  }

  // Render login screen if unauthenticated
  if (!token || !user) {
    return <Login onLoginSuccess={handleLoginSuccess} />;
  }

  const renderContent = () => {
    switch (activeTab) {
      case 'dashboard':
        return <Dashboard statsTrigger={statsTrigger} onNavigate={goTab} onUpdate={triggerRefresh} showToast={showToast} />;
      case 'add-cards':
        return <AddCards onAddSuccess={triggerRefresh} showToast={showToast} setActiveTab={goTab} />;
      case 'collection':
        return (
          <CollectionList 
            statsTrigger={statsTrigger} 
            onUpdate={triggerRefresh} 
            showToast={showToast} 
            token={token} 
            selectedCardFilter={selectedCardFilter}
            setSelectedCardFilter={setSelectedCardFilter}
          />
        );
      case 'deckbuilder':
        return <DeckBuilder showToast={showToast} onNavigate={goTab} />;
      case 'lists':
        return <Lists showToast={showToast} handoff={listsHandoff} onHandoffDone={() => setListsHandoff(null)} />;
      case 'limited':
        return <LimitedLands showToast={showToast} onNavigate={goTab} />;
      case 'rules':
        return <Rules onNavigate={goTab} />;
      case 'keywords':
        return <Keywords onNavigate={goTab} />;
      case 'settings':
        return <Settings user={user} onUpdateUser={handleUpdateUser} showToast={showToast} target={settingsTarget} />;
      default:
        return <Dashboard statsTrigger={statsTrigger} onNavigate={goTab} onUpdate={triggerRefresh} showToast={showToast} />;
    }
  };

  return (
    <div className="app-container">
      <PullToRefresh />
      {showSetup && (
        <Suspense fallback={null}>
          <SetupWizard user={user} showToast={showToast} onClose={() => setShowSetup(false)} />
        </Suspense>
      )}
      {/* Premium Header */}
      <header className="app-header">
        <div className="logo-section">
          <div className="logo-icon">
            <Logo />
          </div>
          <h1 className="logo-text">Scry<span>box</span></h1>
        </div>

        {/* Navigation Tabs (Nested inside header for unified layout) */}
        <nav className="nav-tabs" style={{ margin: 0 }}>
          <button 
            className={`nav-tab ${activeTab === 'dashboard' ? 'active' : ''}`}
            onClick={() => goTab('dashboard')}
          >
            <LayoutDashboard size={18} />
            <span>{t('nav.dashboard')}</span>
          </button>
          <button
            className={`nav-tab ${activeTab === 'add-cards' ? 'active' : ''}`}
            onClick={() => goTab('add-cards')}
          >
            <Plus size={18} />
            <span>{t('nav.addCards')}</span>
          </button>
          <button
            className={`nav-tab ${activeTab === 'collection' ? 'active' : ''}`}
            onClick={() => goTab('collection')}
          >
            <Database size={18} />
            <span>{t('nav.collection')}</span>
          </button>
          <button
            className={`nav-tab ${activeTab === 'deckbuilder' ? 'active' : ''}`}
            onClick={() => goTab('deckbuilder')}
          >
            <Swords size={18} />
            <span>{t('nav.deckBuilder')}</span>
          </button>

          <button
            className={`nav-tab ${activeTab === 'lists' ? 'active' : ''}`}
            onClick={() => goTab('lists')}
          >
            <ListChecks size={18} />
            <span>{t('nav.lists')}</span>
          </button>

        </nav>

        <div style={{ display: 'flex', alignItems: 'center', gap: '0.6rem' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', color: 'var(--text-secondary)', fontSize: '0.85rem' }}>
            <Sparkles size={14} style={{ color: 'var(--accent-yellow)' }} />
            <span><strong style={{ color: 'var(--text-strong)' }}>{user.username}</strong></span>
          </div>
          <button
            onClick={() => goTab('settings')}
            className={`btn ${activeTab === 'settings' ? 'btn-primary' : 'btn-secondary'} btn-icon-only`}
            title={t('nav.settings')}
            aria-label={t('nav.settings')}
            aria-current={activeTab === 'settings' ? 'page' : undefined}
            style={{ padding: '0.4rem 0.5rem', borderRadius: 'var(--radius-sm)' }}
          >
            <SettingsIcon size={14} />
          </button>
          <button
            onClick={handleLogout}
            className="btn btn-secondary btn-icon-only"
            title={t('header.logOut')}
            aria-label={t('header.logOut')}
            style={{ padding: '0.4rem 0.5rem', borderRadius: 'var(--radius-sm)' }}
          >
            <LogOut size={14} />
          </button>
        </div>
      </header>

      {/* Main Content Area */}
      <main style={{ flex: 1, marginTop: '1rem' }}>
        {/* key on activeTab remounts the boundary per tab, so a crash in one
            view clears when you navigate away instead of persisting until a
            manual reload. */}
        <ErrorBoundary key={activeTab} t={t}>
          <div className="view-transition">
            <Suspense fallback={<ChunkFallback />}>
              {renderContent()}
            </Suspense>
          </div>
        </ErrorBoundary>
      </main>

      {/* Toast Notification */}
      {toast && (
        <div className="toast">
          {toast}
        </div>
      )}
    </div>
  );
}

export default App;
