import { X, Check, AlertTriangle } from 'lucide-react';
import CardImage from './CardImage';
import { useBackGuard } from '../utils/useBackGuard';
import { useT } from '../utils/i18n';

// Deck checkout / check-in coverage guide. Backed by the backend's
// /api/decks/:id/locations payload, which reports for every card in the deck
// how many copies are owned, how many are locked by other checked-out decks,
// and how many are available. Nothing physical here — this answers "do I have
// enough of everything for this deck?", not "where is each card in the
// binder". Missing cards float to the top (the backend sorts them there).
const CheckoutWizardModal = ({ locationsData, mode = 'checkout', onClose, onCancel }) => {
  const { t } = useT();
  const kind = mode === 'checkin' ? 'checkin' : 'checkout';
  const cancel = onCancel || onClose;

  const cards = Array.isArray(locationsData) ? locationsData : (locationsData?.cards || []);
  // Basic lands are ignored for coverage: anyone who plays the format owns a
  // set of them, so a short basic land is never a reason a deck can't go out
  // the door. They are listed (dimmed) for completeness but never count
  // against the progress bar or the covered check.
  const counted = cards.filter(c => !c.is_basic_land);
  const ignoredBasicLands = cards.filter(c => c.is_basic_land);
  const missingCards = counted.filter(c => c.missing > 0);
  const coveredCards = counted.filter(c => c.missing === 0);
  const totalCards = counted.length;
  const coveredCount = coveredCards.length;
  const allCovered = totalCards > 0 && coveredCount === totalCards;

  useBackGuard(true, cancel);

  const renderRow = (c) => {
    const short = c.missing > 0;
    return (
      <div
        key={c.card_id}
        style={{
          display: 'flex', alignItems: 'center', gap: '0.75rem', padding: '0.55rem 0.65rem',
          background: short ? 'rgba(239,68,68,0.08)' : 'rgba(255,255,255,0.03)',
          border: `1px solid ${short ? 'rgba(239,68,68,0.4)' : 'var(--border-glass)'}`,
          borderRadius: 'var(--radius-sm)'
        }}
      >
        <div className="cw-thumb" style={{ width: '40px', aspectRatio: '488 / 680', flexShrink: 0, borderRadius: '4px', overflow: 'hidden', background: 'rgba(0,0,0,0.3)' }}>
          {c.image_url && <CardImage card={{ image_url: c.image_url, name: c.card_name || c.name }} loading="lazy" decoding="async" style={{ display: 'block', width: '100%', height: '100%', objectFit: 'cover' }} />}
        </div>
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ color: 'var(--text-strong)', fontSize: '0.9rem', fontWeight: 600, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
            {c.card_name}
          </div>
          <div style={{ color: 'var(--text-secondary)', fontSize: '0.72rem' }}>
            {c.set_name}{c.number ? ` · #${c.number}` : ''}
            {c.in_use > 0 && <span style={{ color: 'var(--text-muted)' }}> · {t('wizard.inUse', { count: c.in_use })}</span>}
          </div>
        </div>
        <div style={{ flexShrink: 0, textAlign: 'right' }}>
          <div style={{ fontSize: '0.8rem', fontWeight: 700, color: short ? 'var(--accent-red)' : 'var(--accent-green)' }}>
            {short ? `×${c.required} / ${c.available}` : `×${c.required}`}
          </div>
          <div style={{ fontSize: '0.65rem', color: 'var(--text-muted)' }}>
            {t('wizard.owned')}: {c.owned} · {t('wizard.available')}: {c.available}
          </div>
        </div>
        <div style={{ width: '22px', height: '22px', flexShrink: 0, borderRadius: '50%', display: 'flex', alignItems: 'center', justifyContent: 'center', background: short ? 'rgba(239,68,68,0.25)' : 'var(--accent-green)' }}>
          {short ? <AlertTriangle size={13} color="#fecaca" /> : <Check size={14} color="#000" strokeWidth={3} />}
        </div>
      </div>
    );
  };

  return (
    <div
      className="modal-overlay"
      style={{ position: 'fixed', inset: 0, backgroundColor: 'rgba(0,0,0,0.85)', backdropFilter: 'blur(10px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000 }}
      onClick={cancel}
    >
      <div
        className="glass-panel"
        style={{ maxWidth: '640px', width: '100%', maxHeight: '90vh', display: 'flex', flexDirection: 'column' }}
        onClick={e => e.stopPropagation()}
      >
        {/* Header */}
        <div style={{ padding: '1.25rem 1.5rem', borderBottom: '1px solid var(--border-glass)', flexShrink: 0 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: '1rem' }}>
            <div>
              <h2 style={{ fontSize: '1.35rem', color: 'var(--text-strong)', fontWeight: 800, margin: '0 0 0.25rem 0' }}>{t(`wizard.${kind}.title`)}</h2>
              <p style={{ color: 'var(--text-secondary)', margin: 0, fontSize: '0.85rem' }}>{t(`wizard.${kind}.subtitle`)}</p>
            </div>
            <button className="btn btn-secondary btn-icon-only modal-close" onClick={cancel} aria-label={t('common.cancel')}>
              <X size={16} />
            </button>
          </div>

          {totalCards > 0 && (
            <div style={{ marginTop: '1rem' }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '0.4rem' }}>
                <span style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>{t('wizard.progress', { done: coveredCount, total: totalCards })}</span>
                {allCovered && <span style={{ fontSize: '0.75rem', color: 'var(--accent-green)', fontWeight: 700 }}>{t('wizard.allCovered')}</span>}
              </div>
              <div style={{ width: '100%', height: '6px', background: 'rgba(255,255,255,0.1)', borderRadius: '3px', overflow: 'hidden' }}>
                <div style={{ width: `${totalCards ? Math.round((coveredCount / totalCards) * 100) : 0}%`, height: '100%', background: allCovered ? 'var(--accent-green)' : 'var(--accent-blue)', transition: 'width 0.3s ease' }} />
              </div>
            </div>
          )}
        </div>

        {/* Body */}
        <div style={{ padding: '1.25rem 1.5rem', overflowY: 'auto', display: 'flex', flexDirection: 'column', gap: '1rem' }}>
          {totalCards === 0 && (
            <div style={{ color: 'var(--text-muted)', textAlign: 'center', padding: '1.5rem 0', fontSize: '0.9rem' }}>{t('wizard.empty')}</div>
          )}

          {missingCards.length > 0 && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem' }}>
              <div style={{ color: 'var(--accent-red)', fontWeight: 700, fontSize: '0.85rem' }}>{t('wizard.missingSection')}</div>
              {missingCards.map(renderRow)}
            </div>
          )}

          {coveredCards.length > 0 && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem' }}>
              <div style={{ color: 'var(--text-secondary)', fontWeight: 700, fontSize: '0.85rem' }}>{t('wizard.coveredSection')}</div>
              {coveredCards.map(renderRow)}
            </div>
          )}

          {ignoredBasicLands.length > 0 && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem' }}>
              <div style={{ color: 'var(--text-muted)', fontWeight: 700, fontSize: '0.8rem' }}>{t('wizard.basicLandsIgnored')}</div>
              {ignoredBasicLands.map(c => (
                <div
                  key={c.card_id}
                  style={{
                    display: 'flex', alignItems: 'center', gap: '0.75rem', padding: '0.5rem 0.65rem',
                    background: 'rgba(255,255,255,0.02)',
                    border: '1px dashed var(--border-glass)',
                    borderRadius: 'var(--radius-sm)',
                    opacity: 0.75
                  }}
                >
                  <div className="cw-thumb" style={{ width: '40px', aspectRatio: '488 / 680', flexShrink: 0, borderRadius: '4px', overflow: 'hidden', background: 'rgba(0,0,0,0.3)' }}>
                    {c.image_url && <CardImage card={{ image_url: c.image_url, name: c.card_name || c.name }} loading="lazy" decoding="async" style={{ display: 'block', width: '100%', height: '100%', objectFit: 'cover' }} />}
                  </div>
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={{ color: 'var(--text-secondary)', fontSize: '0.85rem', fontWeight: 600, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
                      {c.card_name}
                    </div>
                    <div style={{ color: 'var(--text-muted)', fontSize: '0.7rem' }}>
                      {t('wizard.basicLand', { owned: c.owned, required: c.required })}
                    </div>
                  </div>
                  <span style={{ fontSize: '0.65rem', fontWeight: 700, textTransform: 'uppercase', color: 'var(--text-muted)' }}>{t('wizard.ignored')}</span>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* Footer */}
        <div style={{ padding: '1rem 1.5rem', borderTop: '1px solid var(--border-glass)', display: 'flex', justifyContent: 'flex-end', gap: '0.75rem', flexShrink: 0 }}>
          <button className="btn btn-primary" onClick={onClose}>{t('bulk.done')}</button>
        </div>
      </div>
    </div>
  );
};

export default CheckoutWizardModal;
