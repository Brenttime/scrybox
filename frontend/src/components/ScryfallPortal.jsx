import { ChevronLeft, Database } from 'lucide-react';
import { useT } from '../utils/i18n';
import { useSwipeBack } from '../utils/useSwipeBack';

// Local Scryfall portal (LOCAL_WEB_PORTAL_ENABLED) as an in-app tool: the
// sidecar's read-only portal, proxied same-origin at /scryfall-portal/, in an
// iframe under a Scrybox back header. Touches inside an iframe never reach
// this document, so a thin transparent strip along the left edge catches the
// swipe-back gesture (not position:fixed, or useSwipeBack would skip it).
export default function ScryfallPortal({ onNavigate }) {
  const { t } = useT();
  const back = () => onNavigate?.('dashboard');
  useSwipeBack(true, back);

  return (
    <div className="sfp-view">
      <div className="sfp-head">
        <button type="button" className="btn btn-secondary" onClick={back} aria-label={t('common.back')} style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
          <ChevronLeft size={16} /> {t('common.back')}
        </button>
        <h2 className="sfp-title">
          <Database size={20} style={{ color: 'var(--accent-yellow)' }} />
          <span>{t('portal.title')}</span>
        </h2>
      </div>
      <div className="sfp-frame-wrap">
        <iframe className="sfp-frame" src="/scryfall-portal/" title={t('portal.title')} />
        <div className="sfp-swipe-edge" aria-hidden="true" />
      </div>
    </div>
  );
}
