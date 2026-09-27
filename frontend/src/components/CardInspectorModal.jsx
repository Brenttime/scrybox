import { useState, useEffect } from 'react';
import { X, Trash2, Maximize2, ExternalLink, Search } from 'lucide-react';
import { getCardDisplayName } from '../utils/langHelper';
import { translatedName, setCode, isEnglish } from '../utils/languages';
import { formatPrice, priceText } from '../utils/formatPrice';
import { resolveCardPrice } from '../utils/resolveCardPrice';
import { getPrintingLabel } from '../utils/cardPrinting';
import { tcgplayerUrl, cardmarketUrl, searchUrl, priceSource, noLinkReason } from '../utils/marketplaceLinks';
import CardImage from './CardImage';
import CardImageZoom from './CardImageZoom';
import CardImageActions from './CardImageActions';
import CardEntryFields from './CardEntryFields';
import PriceHistoryChart from './PriceHistoryChart';
import CardArtEditor from './CardArtEditor';
import { useBackGuard } from '../utils/useBackGuard';
import { useT } from '../utils/i18n';

// MTG color identity pip colors (WUBRG), approximating the printed mana colors.
const MTG_COLOR_BG = {
  White: '#f8f6d8', Blue: '#0e68ab', Black: '#2b2422', Red: '#d3202a', Green: '#00733e'
};
const MTG_COLOR_FG = {
  White: '#3a3520', Blue: '#fff', Black: '#fff', Red: '#fff', Green: '#fff'
};

// Shared card detail popup used by Dashboard and CollectionList.
// Self-contained: owns its edit form (PUT) and delete (DELETE) so every screen
// gets the same rich view + edit without duplicating the form. onUpdate() lets
// the parent refetch after a change.
function CardInspectorModal({ card, onClose, onUpdate, onDeleted, showToast, startInEdit = false }) {
  const { t } = useT();
  const [mode, setMode] = useState('view');
  const [q, setQ] = useState(1);
  const [condition, setCondition] = useState('Near Mint');
  const [printing, setPrinting] = useState('Normal');
  const [language, setLanguage] = useState('English');
  const [purchasePrice, setPurchasePrice] = useState(0);
  const [isTrade, setIsTrade] = useState(0);
  const [notes, setNotes] = useState('');
  const [isFullScreen, setIsFullScreen] = useState(false);
  // Rules text, fetched per card (not per collection row) so the list payload
  // stays lean. null = not loaded / none.
  const [oracle, setOracle] = useState(null);
  const oracleCardId = card?.card_id || null;

  useEffect(() => {
    setOracle(null);
    if (!oracleCardId) return undefined;
    let cancelled = false;
    fetch(`/api/cards/${encodeURIComponent(oracleCardId)}/oracle`)
      .then(r => (r.ok ? r.json() : null))
      .then(d => { if (!cancelled) setOracle(d); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [oracleCardId]);

  useBackGuard(isFullScreen, () => setIsFullScreen(false));

  const targetEntryId = card?.entry_id || card?.id;

  useEffect(() => {
    if (!card) return;
    setMode(startInEdit ? 'edit' : 'view');
    setQ(card.quantity ?? 1);
    setCondition(card.condition || 'Near Mint');
    setPrinting(card.printing || 'Normal');
    setLanguage(card.language || 'English');
    setPurchasePrice(card.purchase_price || 0);
    setIsTrade(card.is_trade ? 1 : 0);
    setNotes(card.notes || '');
    // eslint-disable-next-line react-hooks/exhaustive-deps -- reset form only when the entry changes, not on every card mutation
  }, [targetEntryId, startInEdit]);

  const handleClose = () => {
    onClose && onClose();
  };

  useBackGuard(!!card, handleClose);

  if (!card) return null;

  const handleSave = async (e) => {
    e.preventDefault();
    if (!targetEntryId) return;
    const qNum = Math.max(1, parseInt(q, 10) || 1);
    try {
      const res = await fetch(`/api/collection/${targetEntryId}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          // Sent only when actually changed. The server reads quantity as the
          // absolute number of copies owned and adds or removes rows to match,
          // and some screens open this popup on a single row whose quantity is
          // not the whole stack — so an untouched field must not be able to
          // trim copies the user never asked to lose.
          ...(qNum !== (card.quantity ?? 1) ? { quantity: qNum } : {}),
          condition,
          printing,
          language,
          purchase_price: parseFloat(purchasePrice) || 0,
          is_trade: isTrade ? 1 : 0,
          notes
        })
      });
      if (res.ok) {
        card.quantity = qNum;
        card.condition = condition;
        card.printing = printing;
        card.language = language;
        card.purchase_price = parseFloat(purchasePrice) || 0;
        card.is_trade = isTrade ? 1 : 0;
        card.notes = notes;
        // The server resolves this per printing on the next fetch; mirror it here so
        // a screen still holding this object does not show the old printing's price.
        card.price_trend = resolveCardPrice(card, printing);
        showToast && showToast(t('inspector.entryUpdated'));
        onUpdate && onUpdate();
        onClose();
      } else {
        const body = await res.json().catch(() => null);
        showToast && showToast(body?.error || t('inspector.errUpdate'));
      }
    } catch (err) {
      console.error(err);
      showToast && showToast(t('inspector.errEdit'));
    }
  };

  const handleDelete = async () => {
    if (!targetEntryId) return;
    if (!window.confirm(t('collection.confirmDeleteCard', { name: card.name }))) return;
    try {
      const res = await fetch(`/api/collection/${targetEntryId}`, { method: 'DELETE' });
      if (res.ok) {
        showToast && showToast(t('collection.cardRemoved', { name: card.name }));
        onDeleted && onDeleted(targetEntryId);
        onUpdate && onUpdate();
        onClose();
      } else {
        showToast && showToast(t('collection.errDelete'));
      }
    } catch (err) {
      console.error(err);
      showToast && showToast(t('common.errBackend'));
    }
  };

  const cardNumber = card.number || card.collector_number || card.card_number || '';

  // Resolved against the printing selected RIGHT NOW, not the one that was saved
  // when this row was fetched. `card.price_trend` arrives from the server already
  // resolved for the stored printing, so rendering it directly meant switching
  // from one finish to another in the form changed nothing on screen — the number
  // only caught up after a save and a refetch, which reads as "prices don't
  // respond to the foil type". Same resolution order as the server.
  const displayPrice = resolveCardPrice(card, printing);

  return (
    <div className="modal-overlay" style={{
      position: 'fixed',
      top: 0, left: 0, right: 0, bottom: 0,
      backgroundColor: 'rgba(0,0,0,0.75)',
      backdropFilter: 'blur(8px)',
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'center',
      zIndex: 999
    }} onClick={handleClose}>
      <div className="glass-panel card-inspector" onClick={(e) => e.stopPropagation()}>
        <button type="button" className="btn btn-secondary btn-icon-only modal-close" onClick={handleClose} aria-label={t('common.close')} title={t('common.close')} style={{
          position: 'absolute',
          top: '1rem',
          right: '1rem',
          borderRadius: '50%',
          zIndex: 10
        }}>
          <X size={16} />
        </button>

        {/* Left side: Main Card Image Focus */}
        <div className="ci-image-col" style={{ flex: '1 1 260px', display: 'flex', flexDirection: 'column', alignItems: 'center' }}>
          <div
            className="ci-image-wrap"
            onClick={() => setIsFullScreen(true)}
            title={t('inspector.zoomHint')}
            style={{ position: 'relative', width: '100%', maxWidth: '300px', cursor: 'pointer' }}
          >
            {/* CardImage, not a bare <img>: this was the last call site still
                reading card.image_url directly, so contributed art uploaded
                through the editor below was never shown in the very view that
                uploads it, and a card with no provider art rendered as a broken
                image icon here alone. */}
            <CardImage
              card={card}
              style={{
                width: '100%',
                aspectRatio: 0.718,
                objectFit: 'cover',
                borderRadius: 'var(--radius-md)',
                boxShadow: '0 12px 36px rgba(0,0,0,0.6), 0 0 20px rgba(255,255,255,0.05)',
                transition: 'transform 0.2s ease'
              }}
            />
            <div style={{
              position: 'absolute',
              bottom: '0.6rem',
              right: '0.6rem',
              background: 'rgba(0,0,0,0.65)',
              backdropFilter: 'blur(6px)',
              padding: '0.25rem 0.5rem',
              borderRadius: 'var(--radius-sm)',
              color: '#fff',
              fontSize: '0.65rem',
              fontWeight: 700,
              display: 'flex',
              alignItems: 'center',
              gap: '0.3rem',
              pointerEvents: 'none',
              border: '1px solid rgba(255,255,255,0.15)'
            }}>
              <Maximize2 size={12} />
              <span>{t('inspector.fullScreen')}</span>
            </div>
          </div>
          <CardImageActions card={card} compact getSrc={() => document.querySelector('.card-inspector .ci-image-wrap img')?.currentSrc} />
          <CardArtEditor
            card={card}
            hasProviderArt={!!card.image_url}
            showToast={showToast}
            onChanged={onUpdate}
          />
        </div>

        {/* Right side: Information / Edit */}
        <div className="ci-info-col" style={{ flex: '1 1 320px', display: 'flex', flexDirection: 'column', gap: '1.25rem', justifyContent: 'space-between' }}>
          <div>
            <div style={{ display: 'flex', gap: '0.4rem', flexWrap: 'wrap', marginBottom: '0.5rem' }}>
              {card.is_trade === 1 && (
                <span style={{ fontSize: '0.65rem', fontWeight: 800, textTransform: 'uppercase', padding: '0.2rem 0.5rem', borderRadius: '4px', backgroundColor: 'rgba(74, 222, 128, 0.15)', color: 'var(--success)', border: '1px solid rgba(74, 222, 128, 0.3)' }}>
                  {t('inspector.forTrade')}
                </span>
              )}
            </div>

            <h3 style={{ fontSize: '1.65rem', color: 'var(--text-strong)', fontWeight: 800, lineHeight: 1.15, marginBottom: '0.25rem' }}>
              {getCardDisplayName(card.name, card.language, card.printed_name)}
            </h3>
            {/* The English name when the provider has one for this printing
                (Scryfall has it for every Magic printing). Shown only when the
                card's own language differs from the UI's. */}
            {translatedName(card) && (
              <p style={{ color: 'var(--text-muted)', fontSize: '0.9rem', fontWeight: 500, marginBottom: '0.25rem' }}>
                {translatedName(card)}
              </p>
            )}
            <p style={{ color: 'var(--text-secondary)', fontSize: '0.85rem', fontWeight: 500 }}>
              {card.set_name}
              {/* Set code alongside the native set name: it reads the same in every
                  language, so it is the part you can search or quote. The collector
                  number is already spelled out just after, so only the code here. */}
              {!isEnglish(card.language) && setCode(card) && (
                <span style={{ fontFamily: 'monospace', color: 'var(--text-muted)' }}> ({setCode(card)})</span>
              )}
              {cardNumber ? ` • #${cardNumber}` : ''}{card.rarity ? ` • ${card.rarity}` : ''} • {t('inspector.owned', { count: card.quantity ?? 1 })}
            </p>

            {/* MTG cards: show color pips + type line. */}
            {card.supertype === 'MTG' && (
              <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', flexWrap: 'wrap', marginTop: '0.5rem' }}>
                {(Array.isArray(card.types) ? card.types : []).map(color => (
                  <span key={color} className={`mtg-color-pip mtg-color-${color.toLowerCase()}`} style={{
                    fontSize: '0.6rem', fontWeight: 800, textTransform: 'uppercase', letterSpacing: '0.03em',
                    padding: '0.15rem 0.45rem', borderRadius: '999px',
                    background: MTG_COLOR_BG[color] || 'rgba(255,255,255,0.1)',
                    color: MTG_COLOR_FG[color] || '#fff', border: '1px solid rgba(0,0,0,0.2)'
                  }}>{color}</span>
                ))}
                {(!card.types || card.types.length === 0) && (
                  <span style={{ fontSize: '0.6rem', fontWeight: 800, textTransform: 'uppercase', padding: '0.15rem 0.45rem', borderRadius: '999px', background: 'rgba(180,180,180,0.25)', color: '#eee' }}>{t('inspector.colorless')}</span>
                )}
                {Array.isArray(card.subtypes) && card.subtypes.length > 0 && (
                  <span style={{ fontSize: '0.75rem', color: 'var(--text-secondary)' }}>{card.subtypes.join(' ')}</span>
                )}
              </div>
            )}
          </div>

          {mode !== 'edit' && oracle && (oracle.faces?.length || oracle.oracle_text) ? (
            <div className="ci-oracle" style={{ display: 'flex', flexDirection: 'column', gap: '0.6rem', background: 'rgba(255,255,255,0.02)', border: '1px solid var(--border-glass)', borderRadius: 'var(--radius-sm)', padding: '0.7rem 0.85rem' }}>
              {(oracle.faces?.length ? oracle.faces : [{ oracle_text: oracle.oracle_text }]).map((face, i) => (
                <div key={i}>
                  {face.name && (
                    <div style={{ fontSize: '0.78rem', fontWeight: 700, color: 'var(--text-strong)', marginBottom: '0.15rem' }}>
                      {face.name}{face.mana_cost ? <span style={{ color: 'var(--text-muted)', fontWeight: 500 }}> {face.mana_cost}</span> : null}
                      {face.type_line ? <span style={{ display: 'block', color: 'var(--text-muted)', fontWeight: 500, fontSize: '0.72rem' }}>{face.type_line}</span> : null}
                    </div>
                  )}
                  {face.oracle_text && (
                    <p style={{ margin: 0, fontSize: '0.82rem', lineHeight: 1.45, color: 'var(--text-secondary)', whiteSpace: 'pre-line' }}>{face.oracle_text}</p>
                  )}
                </div>
              ))}
            </div>
          ) : null}

          {mode === 'edit' ? (
            <form onSubmit={handleSave} style={{ display: 'flex', flexDirection: 'column', gap: '1rem' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', background: 'rgba(255,255,255,0.02)', padding: '0.6rem 0.9rem', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
                <input type="checkbox" checked={isTrade === 1} onChange={(e) => setIsTrade(e.target.checked ? 1 : 0)} id="isTrade" style={{ width: '16px', height: '16px', cursor: 'pointer' }} />
                <label htmlFor="isTrade" style={{ cursor: 'pointer', margin: 0, fontWeight: 700, color: 'var(--text-strong)', fontSize: '0.85rem' }}>
                  {t('inspector.listedInTrade')}
                </label>
              </div>

              <CardEntryFields
                quantity={q} purchasePrice={purchasePrice} condition={condition} printing={printing} language={language}
                onQuantity={setQ} onPurchasePrice={setPurchasePrice} onCondition={setCondition} onPrinting={setPrinting} onLanguage={setLanguage}
              />

              <div className="form-group">
                <label>{t('inspector.notes')}</label>
                <textarea
                  className="input-control"
                  value={notes}
                  onChange={(e) => setNotes(e.target.value)}
                  placeholder={t('inspector.notesPlaceholder')}
                  rows={3}
                  style={{ resize: 'vertical', fontFamily: 'inherit' }}
                />
              </div>

              <div style={{ display: 'flex', gap: '0.75rem', marginTop: '0.25rem' }}>
                <button type="button" className="btn btn-secondary" onClick={() => setMode('view')} style={{ flex: 1 }}>{t('common.cancel')}</button>
                <button type="submit" className="btn btn-primary" style={{ flex: 2 }}>{t('inspector.saveChanges')}</button>
              </div>
            </form>
          ) : (
            <>
              {/* Price Panel */}
              <div style={{ borderTop: '1px solid var(--border-glass)', borderBottom: '1px solid var(--border-glass)', padding: '0.75rem 0', display: 'grid', gridTemplateColumns: 'repeat(2, 1fr)', gap: '1rem' }}>
                <div>
                  <div style={{ fontSize: '0.65rem', color: 'var(--text-muted)', fontWeight: 700 }}>{t('inspector.marketPrice')}</div>
                  <div style={{ fontSize: '1.25rem', fontWeight: 800, color: 'var(--accent-yellow)', marginTop: '0.15rem' }}>
                    {priceText(displayPrice, card.price_currency)}
                  </div>
                  {/* Say where a non-English price came from and in what currency —
                      it is Cardmarket's EUR figure rendered with the app's $. */}
                  {priceSource(card) && (
                    <div style={{ fontSize: '0.62rem', color: 'var(--text-muted)', marginTop: '0.1rem' }}>
                      {t('inspector.priceVia', { source: priceSource(card).name, currency: priceSource(card).currency })}
                    </div>
                  )}
                  {/* Printings are priced separately; conditions are not, by anyone
                      Scrybox talks to — TCGplayer, Scryfall and Cardmarket all quote
                      a Near Mint copy. Saying so beats letting a played card show a
                      NM price with nothing to explain it, and beats inventing a
                      condition multiplier, which would be a made-up number wearing
                      the same styling as a real one. */}
                  {condition && condition !== 'Near Mint' && (
                    <div style={{ fontSize: '0.62rem', color: 'var(--text-muted)', marginTop: '0.1rem', lineHeight: 1.35 }}>
                      {t('inspector.priceNearMintOnly', { condition })}
                    </div>
                  )}
                </div>
                <div>
                  <div style={{ fontSize: '0.65rem', color: 'var(--text-muted)', fontWeight: 700 }}>{t('inspector.purchaseValue')}</div>
                  <div style={{ fontSize: '1.25rem', fontWeight: 800, color: 'var(--text-strong)', marginTop: '0.15rem' }}>
                    ${formatPrice(card.purchase_price)}
                  </div>
                </div>
              </div>

              {/* Marketplace links. "View on TCGplayer" now means the card's own
                  product page and nothing else — it used to fall back to a name
                  search wearing the same label, which for a Japanese printing
                  reliably found nothing.

                  A search is still offered, as its own action with its own words, so
                  the reader can tell which of the two they are about to get. */}
              <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap' }}>
                {tcgplayerUrl(card) && (
                  <a
                    href={tcgplayerUrl(card)} target="_blank" rel="noopener noreferrer"
                    className="btn btn-secondary"
                    style={{ flex: 1, minWidth: '140px', display: 'flex', alignItems: 'center', justifyContent: 'center', gap: '0.35rem', fontSize: '0.75rem' }}
                  >
                    <ExternalLink size={13} /> {t('inspector.viewOnTcgplayer')}
                  </a>
                )}
                {cardmarketUrl(card) && (
                  <a
                    href={cardmarketUrl(card)} target="_blank" rel="noopener noreferrer"
                    className="btn btn-secondary"
                    style={{ flex: 1, minWidth: '140px', display: 'flex', alignItems: 'center', justifyContent: 'center', gap: '0.35rem', fontSize: '0.75rem' }}
                  >
                    <ExternalLink size={13} /> Cardmarket
                  </a>
                )}
                {/* Only shown when there is no direct link — as a fallback the reader
                    chooses, not a substitute presented as the real thing. */}
                {!tcgplayerUrl(card) && !cardmarketUrl(card) && searchUrl(card) && (
                  <a
                    href={searchUrl(card)} target="_blank" rel="noopener noreferrer"
                    className="btn btn-secondary"
                    style={{ flex: 1, minWidth: '140px', display: 'flex', alignItems: 'center', justifyContent: 'center', gap: '0.35rem', fontSize: '0.75rem' }}
                  >
                    <Search size={13} /> {t('inspector.searchTcgplayer')}
                  </a>
                )}
              </div>
              {noLinkReason(card) && (
                <div style={{ fontSize: '0.7rem', color: 'var(--text-muted)', lineHeight: 1.4 }}>
                  {noLinkReason(card)}
                </div>
              )}

              {/* Price History Area Chart */}
              <PriceHistoryChart cardId={card.card_id} currency={card.price_currency} height={100} defaultRange="30d" />

              {/* Specifications Details Grid */}
              <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '0.6rem 1rem', background: 'rgba(255,255,255,0.01)', border: '1px solid var(--border-glass)', padding: '0.75rem', borderRadius: 'var(--radius-sm)', fontSize: '0.75rem' }}>
                <div><span style={{ color: 'var(--text-muted)' }}>{t('inspector.specCondition')}</span> <span style={{ color: 'var(--text-strong)', fontWeight: 600 }}>{card.condition}</span></div>
                <div><span style={{ color: 'var(--text-muted)' }}>{t('inspector.specPrinting')}</span> <span style={{ color: 'var(--text-strong)', fontWeight: 600 }}>{getPrintingLabel(card.printing)}</span></div>
                <div><span style={{ color: 'var(--text-muted)' }}>{t('inspector.specLanguage')}</span> <span style={{ color: 'var(--text-strong)', fontWeight: 600 }}>{card.language}</span></div>
                <div><span style={{ color: 'var(--text-muted)' }}>{t('inspector.specSupertype')}</span> <span style={{ color: 'var(--text-strong)', fontWeight: 600 }}>{card.supertype}</span></div>
              </div>

              {card.notes && (
                <div style={{ background: 'rgba(0,0,0,0.15)', border: '1px solid var(--border-subtle)', borderRadius: 'var(--radius-sm)', padding: '0.6rem 0.75rem', fontSize: '0.8rem', color: 'var(--text-secondary)', whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}>
                  {card.notes}
                </div>
              )}

              {/* Main Actions Row: a compact Edit button + Delete. */}
              <div style={{ display: 'flex', gap: '0.5rem', marginTop: '0.25rem', alignItems: 'center', flexWrap: 'wrap' }}>
                <button className="btn btn-primary ci-edit-btn" style={{ padding: '0.4rem 0.9rem', fontSize: '0.8rem' }} onClick={() => setMode('edit')}>
                  {t('inspector.editCard')}
                </button>

                <button
                  type="button"
                  className="btn btn-danger btn-icon-only"
                  style={{ borderRadius: 'var(--radius-sm)', padding: '0.6rem' }}
                  onClick={handleDelete}
                  title={t('inspector.deleteCard')}
                >
                  <Trash2 size={16} />
                </button>
              </div>
            </>
          )}
        </div>
      </div>

      {isFullScreen && (
        <CardImageZoom card={card} onClose={() => setIsFullScreen(false)} />
      )}
    </div>
  );
}

export default CardInspectorModal;
