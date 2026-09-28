import { CONDITIONS, getPrintings, LANGUAGES } from '../utils/cardOptions';
import { useT } from '../utils/i18n';

// Shared quantity / purchase-price / condition / printing / language inputs for
// the add-card and edit-card flows. Presentational only: the parent owns the
// state and the submit/API logic, so the same fields serve create (POST) and
// edit (PUT) callers without this component knowing which.
//   variant 'grid'    - 2-col (qty/price) + 3-col (cond/print/lang); used in the
//                       CardSearch and CardInspector modals.
//   variant 'stacked' - single column for the scanner drawer's quick-add layout.
export default function CardEntryFields({
  quantity, purchasePrice, condition, printing, language,
  onQuantity, onPurchasePrice, onCondition, onPrinting, onLanguage,
  variant = 'grid',
  compact = false,   // card-details edit menu: no condition, foil as a toggle
}) {
  const { t } = useT();
  const stacked = variant === 'stacked';
  const printings = getPrintings();
  const groupStyle = stacked ? { marginBottom: 0 } : undefined;

  const stepQty = (delta) => onQuantity(String(Math.max(1, (parseInt(quantity, 10) || 1) + delta)));
  const Quantity = stacked ? (
    // Scanner quick-add: quantity is the most-changed field, so give it big
    // tap targets instead of a bare number input.
    <div className="form-group quick-add-full-width" style={groupStyle}>
      <label>{t('card.quantity')}</label>
      <div style={{ display: 'flex', gap: '0.4rem', alignItems: 'stretch' }}>
        <button type="button" className="btn btn-secondary" onClick={() => stepQty(-1)} aria-label={t('card.quantityDown')} style={{ padding: '0 1.1rem', fontSize: '1.3rem', flexShrink: 0 }}>&minus;</button>
        <input type="number" className="input-control" min="1" value={quantity} onChange={(e) => onQuantity(e.target.value)} required style={{ flex: 1, minWidth: 0, textAlign: 'center', fontWeight: 700 }} />
        <button type="button" className="btn btn-secondary" onClick={() => stepQty(1)} aria-label={t('card.quantityUp')} style={{ padding: '0 1.1rem', fontSize: '1.3rem', flexShrink: 0 }}>+</button>
      </div>
    </div>
  ) : (
    <div className="form-group" style={groupStyle}>
      <label>{t('card.quantity')}</label>
      <input type="number" className="input-control" min="1" value={quantity} onChange={(e) => onQuantity(e.target.value)} required />
    </div>
  );
  const Price = (
    <div className="form-group" style={groupStyle}>
      <label>{t('card.purchasePrice')}</label>
      <input type="number" step="0.01" className="input-control" value={purchasePrice} onChange={(e) => onPurchasePrice(e.target.value)} placeholder="0.00" />
    </div>
  );
  const Condition = (
    <div className="form-group" style={groupStyle}>
      <label>{t('card.condition')}</label>
      <select className="select-control" value={condition} onChange={(e) => onCondition(e.target.value)}>
        {CONDITIONS.map(c => <option key={c} value={c}>{c}</option>)}
      </select>
    </div>
  );
  const Printing = (
    <div className="form-group" style={groupStyle}>
      <label>{t('card.printing')}</label>
      <select className="select-control" value={printing} onChange={(e) => onPrinting(e.target.value)}>
        {printings.map(p => <option key={p.value} value={p.value}>{p.label}</option>)}
      </select>
    </div>
  );
  const Language = (
    <div className={stacked ? 'form-group quick-add-full-width' : 'form-group'} style={groupStyle}>
      {/* The language the card was printed in — not the app's language. */}
      <label>{t('card.language')}</label>
      <select className="select-control" value={language} onChange={(e) => onLanguage(e.target.value)}>
        {LANGUAGES.map(l => <option key={l} value={l}>{l}</option>)}
      </select>
    </div>
  );

  if (compact) {
    const foil = printing === 'Holofoil';
    return (
      <>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, 1fr)', gap: '0.75rem' }}>{Quantity}{Price}</div>
        <div style={{ display: 'grid', gridTemplateColumns: 'auto 1fr', gap: '0.75rem', alignItems: 'end' }}>
          <div className="form-group">
            <label>{t('card.foil')}</label>
            <button
              type="button"
              role="switch"
              aria-checked={foil}
              className="ci-foil-toggle"
              onClick={() => onPrinting(foil ? 'Normal' : 'Holofoil')}
              style={{ width: '52px', height: '30px', borderRadius: '999px', border: '1px solid var(--border-glass)', background: foil ? 'var(--accent-yellow)' : 'rgba(255,255,255,0.08)', position: 'relative', cursor: 'pointer', padding: 0, transition: 'background 0.15s' }}
            >
              <span style={{ position: 'absolute', top: '3px', left: foil ? '25px' : '3px', width: '22px', height: '22px', borderRadius: '50%', background: '#fff', transition: 'left 0.15s' }} />
            </button>
          </div>
          {Language}
        </div>
      </>
    );
  }
  if (stacked) {
    // Language omitted here: scanner defaults to English and it's rarely changed
    // on a quick add. Still editable later in the card inspector.
    return (
      <div className="quick-add-fields-group">
        {Quantity}{Price}{Condition}{Printing}
      </div>
    );
  }
  return (
    <>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, 1fr)', gap: '0.75rem' }}>{Quantity}{Price}</div>
      <div className="card-entry-fields-row-3" style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: '0.75rem' }}>{Condition}{Printing}{Language}</div>
    </>
  );
}
