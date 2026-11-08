import { Suspense, useEffect, useState } from 'react';
import { Camera, PackageOpen, ScanText, Search, ShoppingCart } from 'lucide-react';
import { useT } from '../utils/i18n';
import { lazyView } from '../utils/lazyView';

// Each pane is its own chunk, so a mode only downloads what it renders: the
// scanner (and its client-scan pipeline) is the bulk of this tab and Search,
// Secret Lair and Order import never need it. App.jsx starts the scanner chunk
// in parallel with this one because it is the default pane.
const FastScanner = lazyView(() => import('./FastScanner'));
const loadCardSearch = () => import('./CardSearch');
const CardSearch = lazyView(loadCardSearch);
const SecretLairPanel = lazyView(() => import('./SecretLairPanel'));
const OrderImportPanel = lazyView(() => import('./OrderImportPanel'));

// Holds the pane's space while its chunk arrives so the page doesn't jump.
const PaneFallback = () => <div style={{ minHeight: '40vh' }} aria-busy="true" />;

// The "Add Cards" tab. It holds four independent ways to put cards in the
// collection, so it opens on a choice between them rather than assuming the
// camera: scan a physical card, search the catalogue and add what you find,
// bring in a whole Secret Lair drop, or pull every card from a ManaPool
// order you have already placed.
//
// The search mode speaks full Scryfall syntax (`is:land`, `color:g`,
// `set:m21`, `rarity:rare`, `otag:...`, quoted phrases, `-negation`,
// `(groups)`), resolved locally against the catalogue where it can be and
// live against the card API for catalogue-only operators — the backend
// `/api/search` route does that routing, the box just takes the text.
//
// The scanner is the default because it is the reason this tab exists and the
// path nearly everyone takes. The segmented control stays mounted across the
// switch so the search box does not lose what you typed, and each view is
// unmounted when hidden — a half-finished Secret Lair preview costs nothing to
// throw away, and neither view has state worth keeping alive across the other.
export default function AddCards({ onAddSuccess, showToast, setActiveTab }) {
  const { t } = useT();
  const [mode, setMode] = useState('fastscan');

  // Search is the next most used pane: warm its chunk once the browser is idle
  // so switching to it doesn't wait on the network.
  useEffect(() => {
    const idle = window.requestIdleCallback || ((cb) => setTimeout(cb, 1500));
    const cancel = window.cancelIdleCallback || clearTimeout;
    const id = idle(() => { loadCardSearch().catch(() => {}); });
    return () => cancel(id);
  }, []);

  // Demo build has no backend: neither the camera scanner nor the Secret Lair
  // importer can work, so show a notice instead of two controls that would fail.
  if (import.meta.env.VITE_DEMO) {
    return (
      <div className='glass-panel' style={{ maxWidth: '520px', margin: '2rem auto', padding: '2rem', textAlign: 'center' }}>
        <Camera size={40} style={{ color: 'var(--accent-yellow)', marginBottom: '1rem' }} />
        <h2 style={{ fontSize: '1.2rem', color: 'var(--text-strong)', marginBottom: '0.75rem' }}>{t('demo.unavailableTitle')}</h2>
        <p style={{ color: 'var(--text-secondary)', fontSize: '0.9rem', lineHeight: 1.5 }}>
          {t('demo.unavailableBody')}
        </p>
      </div>
    );
  }

  const MODES = [
    { key: 'fastscan', icon: ScanText, label: t('addcards.tabFastScan'), short: t('addcards.shortScan') },
    { key: 'search', icon: Search, label: t('addcards.tabSearch'), short: t('addcards.shortSearch') },
    { key: 'secretlair', icon: PackageOpen, label: t('addcards.tabSecretLair'), short: t('addcards.shortSecretLair') },
    { key: 'orderimport', icon: ShoppingCart, label: t('addcards.tabOrderImport'), short: t('addcards.shortOrder') },
  ];

  return (
    <div>
      {/* Segmented control: the house `.sub-nav-tabs` treatment (index.css),
          as on the public collection page. Reusing it rather than hand-rolling
          keeps it on-theme for free — the glass pill track, the gradient-filled
          active state, and the (hover:hover) touch guard all come from there.
          `.sl-mode-tabs` centres it; the width is set per mode so it matches the
          pane underneath it (each pane centres itself at its own measure). */}
      <div
        className="sub-nav-tabs sl-mode-tabs"
        role="tablist"
        aria-label={t('addcards.modePickerAria')}
      >
        {MODES.map(({ key, icon: Icon, label, short }, i) => (
          <button
            key={key}
            type="button"
            role="tab"
            aria-selected={mode === key}
            tabIndex={mode === key ? 0 : -1}
            className={`sub-nav-tab ${mode === key ? 'active' : ''}`}
            onClick={() => setMode(key)}
            onKeyDown={(e) => {
              const step = e.key === 'ArrowRight' ? 1 : e.key === 'ArrowLeft' ? -1 : null;
              if (step === null) return;
              e.preventDefault();
              const nextIdx = (i + step + MODES.length) % MODES.length;
              setMode(MODES[nextIdx].key);
              e.currentTarget.parentElement?.querySelectorAll('[role="tab"]')[nextIdx]?.focus();
            }}
            aria-label={label}
            title={label}
          >
            <Icon size={16} aria-hidden="true" />
            <span className="sl-mode-long" aria-hidden="true">{label}</span>
            <span className="sl-mode-short" aria-hidden="true">{short}</span>
          </button>
        ))}
      </div>

      <Suspense fallback={<PaneFallback />}>
      {mode === 'fastscan' && <FastScanner onAddSuccess={onAddSuccess} showToast={showToast} />}
      {/* CardSearch takes just the two hooks it uses: it reports an add so the
          stats refresh, and it toasts. It never navigates, so unlike the other
          panes it gets no setActiveTab. */}
      {mode === 'search' && <CardSearch onAddSuccess={onAddSuccess} showToast={showToast} />}
      {mode === 'secretlair' && <SecretLairPanel onAddSuccess={onAddSuccess} showToast={showToast} setActiveTab={setActiveTab} />}
      {mode === 'orderimport' && <OrderImportPanel onAddSuccess={onAddSuccess} showToast={showToast} setActiveTab={setActiveTab} />}
      </Suspense>
    </div>
  );
}
