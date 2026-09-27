import { useState } from 'react';
import { Copy, Check, Share2 } from 'lucide-react';
import { useT } from '../utils/i18n';
import { copyCardImage, shareCardImage, canShareFiles } from '../utils/cardImageShare';

// Copy image / Share pills for a card picture. Shared by the card details view
// and the full-screen viewer so both behave identically. `getSrc` returns the
// URL actually displayed (contributed art, Scryfall, or card back); falls back
// to the card's provider image.
export default function CardImageActions({ card, getSrc, compact = false }) {
  const { t } = useT();
  const [status, setStatus] = useState(null); // 'copied' | 'shared' | 'saved' | 'error'
  const [busy, setBusy] = useState(false);
  const showShare = canShareFiles();

  const run = (action) => async (e) => {
    e.stopPropagation();
    if (busy) return;
    setBusy(true);
    try {
      setStatus(await action((getSrc && getSrc()) || card?.image_url, card?.name));
    } catch (err) {
      if (err?.name !== 'AbortError') {
        console.error('Card image share failed:', err);
        setStatus('error');
      }
    } finally {
      setBusy(false);
      setTimeout(() => setStatus(null), 2200);
    }
  };

  const label = status === 'copied' ? t('zoom.copied')
    : status === 'saved' ? t('zoom.saved')
    : status === 'shared' ? t('zoom.shared')
    : status === 'error' ? t('zoom.copyFailed')
    : t('zoom.copyImage');

  return (
    <div className={`zoom-actions${compact ? ' zoom-actions-compact' : ''}`} onClick={(e) => e.stopPropagation()}>
      <button type="button" className="zoom-action" onClick={run(copyCardImage)} disabled={busy} aria-live="polite">
        {status === 'copied' || status === 'saved' ? <Check size={18} /> : <Copy size={18} />}
        <span>{label}</span>
      </button>
      {showShare && (
        <button type="button" className="zoom-action" onClick={run(shareCardImage)} disabled={busy}>
          <Share2 size={18} />
          <span>{t('zoom.share')}</span>
        </button>
      )}
    </div>
  );
}
