import { useEffect, useRef, useState, useCallback } from 'react';
import { X, ChevronLeft, ChevronRight } from 'lucide-react';
import { cardPriceText } from '../utils/formatPrice';
import { useBackGuard } from '../utils/useBackGuard';
import { useT } from '../utils/i18n';

// Full-screen swipeable carousel of every printing of one card. Snaps one
// card to the centre; the centred card is the one "Use this printing" picks.
// Opens on the current printing. prints: null|'loading'|array.
// Only slides near the centre get an <img>: a 140-printing card would
// otherwise decode 140 images while you swipe, which is what made it janky.
const WINDOW = 3;

function PrintingCarousel({ prints, current, busy, onPick, onClose }) {
  const currentId = current?.card_id;
  const { t } = useT();
  const trackRef = useRef(null);
  const [active, setActive] = useState(0);
  // While loading, show the current card alone so the sheet opens instantly.
  const loading = prints === 'loading';
  const list = Array.isArray(prints) ? prints
    : loading && current ? [{ ...current, id: current.card_id, set_id: current.set_id || current.set_code, number: current.number || current.collector_number }]
      : [];
  const rafRef = useRef(0);

  useBackGuard(true, onClose);

  // Land on the current printing once the list arrives.
  useEffect(() => {
    if (!list.length) return;
    const i = Math.max(0, list.findIndex(c => c.id === currentId));
    setActive(i);
    const el = trackRef.current?.children[i];
    if (el) el.scrollIntoView({ inline: 'center', block: 'nearest', behavior: 'instant' });
    // eslint-disable-next-line react-hooks/exhaustive-deps -- only when the list loads
  }, [prints]);

  const onScroll = useCallback(() => {
    if (rafRef.current) return;
    rafRef.current = requestAnimationFrame(() => {
    rafRef.current = 0;
    const track = trackRef.current;
    if (!track) return;
    const mid = track.scrollLeft + track.clientWidth / 2;
    let best = 0, bestD = Infinity;
    Array.from(track.children).forEach((el, i) => {
      const d = Math.abs(el.offsetLeft + el.offsetWidth / 2 - mid);
      if (d < bestD) { bestD = d; best = i; }
    });
    setActive(best);
    });
  }, []);
  useEffect(() => () => cancelAnimationFrame(rafRef.current), []);

  const go = useCallback((i) => {
    const el = trackRef.current?.children[i];
    if (el) el.scrollIntoView({ inline: 'center', block: 'nearest', behavior: 'smooth' });
  }, []);

  useEffect(() => {
    const onKey = (e) => {
      if (e.key === 'ArrowLeft') go(Math.max(0, active - 1));
      else if (e.key === 'ArrowRight') go(Math.min(list.length - 1, active + 1));
      else if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [active, list.length, go, onClose]);

  const cur = list[active];
  const isCurrent = cur && cur.id === currentId;

  return (
    <div className="pc-overlay" role="dialog" aria-modal="true" aria-label={t('scan.changePrinting')}
      onClick={(e) => { e.stopPropagation(); onClose(); }}>
      <div className="pc-head" onClick={e => e.stopPropagation()}>
        <span>{t('scan.changePrinting')}{loading ? ` · ${t('fastscan.loadingPrintings')}` : list.length ? ` · ${active + 1}/${list.length}` : ''}</span>
        <button type="button" className="btn btn-secondary btn-icon-only modal-close" onClick={onClose} aria-label={t('common.close')} title={t('common.close')}>
          <X size={16} />
        </button>
      </div>

      {list.length === 0 && loading ? (
        <div className="pc-msg">{t('fastscan.loadingPrintings')}</div>
      ) : list.length === 0 ? (
        <div className="pc-msg">{t('scan.noOtherPrintings')}</div>
      ) : (
        <>
          <div className="pc-stage" onClick={e => e.stopPropagation()}>
            <button type="button" className="pc-nav pc-prev" onClick={() => go(Math.max(0, active - 1))} disabled={active === 0} aria-label={t('printings.prev')}><ChevronLeft size={22} /></button>
            <div className="pc-track" ref={trackRef} onScroll={onScroll}>
              {list.map((c, i) => (
                <button key={c.id} type="button" className={`pc-slide${i === active ? ' is-active' : ''}${c.id === currentId ? ' is-current' : ''}`}
                  onClick={() => (i === active ? (!busy && c.id !== currentId && onPick(c)) : go(i))}
                  aria-label={`${String(c.set_id || '').toUpperCase()} #${c.number || ''}`}>
                  {Math.abs(i - active) > WINDOW ? <div className="pc-ph" /> : c.image_url ? <img src={c.image_url} alt="" decoding="async" crossOrigin="anonymous" draggable={false} /> : <div className="pc-ph">{c.name}</div>}
                </button>
              ))}
            </div>
            <button type="button" className="pc-nav pc-next" onClick={() => go(Math.min(list.length - 1, active + 1))} disabled={active >= list.length - 1} aria-label={t('printings.next')}><ChevronRight size={22} /></button>
          </div>

          {cur && (
            <div className="pc-info" onClick={e => e.stopPropagation()}>
              <div className="pc-set">{cur.set_name}</div>
              <div className="pc-meta">
                {String(cur.set_id || '').toUpperCase()}{cur.number ? ` · #${cur.number}` : ''}
                {cur.price_trend != null ? ` · ${cardPriceText(cur)}` : ''}
              </div>
              <button type="button" className="btn btn-primary pc-use" disabled={busy || isCurrent || loading} onClick={() => onPick(cur)}>
                {isCurrent ? t('printings.current') : t('printings.use')}
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

export default PrintingCarousel;
