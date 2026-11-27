import { useT } from '../utils/i18n';

const MTG_FORMATS = ['Commander / EDH', 'Standard', 'Modern', 'Pioneer', 'Legacy', 'Vintage', 'Pauper'];
const DECK_CATEGORIES = ['Competitive', 'Casual', 'Tournament', 'Theorycraft', 'Proxy', 'Trade'];
const DECK_ACCENT_COLORS = [
  { name: 'Gold', hex: '#eab308' },
  { name: 'Red', hex: '#ef4444' },
  { name: 'Blue', hex: '#3b82f6' },
  { name: 'Green', hex: '#10b981' },
  { name: 'Purple', hex: '#a855f7' },
  { name: 'Slate', hex: '#64748b' },
  { name: 'Pink', hex: '#ec4899' },
  { name: 'Orange', hex: '#f97316' },
];

const labelStyle = { fontSize: '0.8rem', fontWeight: 700, color: 'var(--text-strong)', marginBottom: '0.3rem', display: 'block' };

// Format, size, name, category, accent and description: the deck "profile"
// shared by Create Deck and Edit Deck. `value` holds those six fields;
// `onChange(patch)` merges a partial update.
export default function DeckProfileFields({ value, onChange }) {
  const { t } = useT();
  // Imported/legacy decks can carry a format outside the list (e.g. Moxfield's
  // "pauperEdh"); keep it selectable so the select shows the real value.
  const formats = value.format && !MTG_FORMATS.includes(value.format) ? [value.format, ...MTG_FORMATS] : MTG_FORMATS;

  const changeFormat = (format) => {
    if (format.includes('Commander')) onChange({ format, target_size: 100 });
    else if (/Standard|Modern|Pioneer/.test(format)) onChange({ format, target_size: 60 });
    else onChange({ format });
  };

  return (
    <>
      <div style={{ display: 'grid', gridTemplateColumns: '2fr 1fr', gap: '0.75rem' }}>
        <div className="form-group">
          <label style={labelStyle}>{t('deck.format')}</label>
          <select className="input-control" value={value.format} onChange={(e) => changeFormat(e.target.value)} style={{ fontSize: '0.85rem' }}>
            {formats.map(fmt => (
              <option key={fmt} value={fmt} style={{ background: '#1e293b', color: '#fff' }}>{fmt}</option>
            ))}
          </select>
        </div>
        <div className="form-group">
          <label style={labelStyle}>{t('deck.targetSize')}</label>
          <input
            type="number"
            min="1"
            max="300"
            className="input-control"
            value={value.target_size}
            onChange={(e) => onChange({ target_size: parseInt(e.target.value, 10) || 60 })}
            style={{ fontSize: '0.85rem' }}
          />
        </div>
      </div>

      <div className="form-group">
        <label style={labelStyle}>{t('deck.deckName')}</label>
        <input
          type="text"
          className="input-control"
          placeholder={t('deck.namePlaceholder')}
          value={value.name}
          onChange={(e) => onChange({ name: e.target.value })}
          required
          autoFocus
        />
      </div>

      <div className="form-group">
        <label style={{ ...labelStyle, marginBottom: '0.4rem' }}>{t('deck.category')}</label>
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: '0.4rem' }}>
          {DECK_CATEGORIES.map(cat => {
            const isSelected = value.category === cat;
            return (
              <button
                key={cat}
                type="button"
                onClick={() => onChange({ category: cat })}
                style={{
                  fontSize: '0.75rem',
                  fontWeight: 700,
                  padding: '0.3rem 0.65rem',
                  borderRadius: '12px',
                  border: isSelected ? '1px solid var(--accent-yellow)' : '1px solid var(--border-glass)',
                  background: isSelected ? 'rgba(234, 179, 8, 0.2)' : 'rgba(0,0,0,0.2)',
                  color: isSelected ? 'var(--accent-yellow)' : 'var(--text-secondary)',
                  cursor: 'pointer',
                  transition: 'all 0.15s'
                }}
              >
                {cat}
              </button>
            );
          })}
        </div>
      </div>

      <div className="form-group">
        <label style={{ ...labelStyle, marginBottom: '0.4rem' }}>{t('deck.accentColor')}</label>
        {/* The form scrolls (overflow-y), which also clips overflow-x, so the
            selected swatch's scale + glow needs room inside this row. */}
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', flexWrap: 'wrap', padding: '6px' }}>
          {DECK_ACCENT_COLORS.map(c => {
            const isSelected = value.accent_color === c.hex;
            return (
              <div
                key={c.hex}
                onClick={() => onChange({ accent_color: c.hex })}
                title={c.name}
                style={{
                  width: '26px',
                  height: '26px',
                  borderRadius: '50%',
                  backgroundColor: c.hex,
                  cursor: 'pointer',
                  border: isSelected ? '2px solid #ffffff' : '2px solid transparent',
                  boxShadow: isSelected ? `0 0 10px ${c.hex}` : 'none',
                  transform: isSelected ? 'scale(1.15)' : 'scale(1)',
                  transition: 'all 0.15s'
                }}
              />
            );
          })}
        </div>
      </div>

      <div className="form-group">
        <label style={labelStyle}>{t('deck.descriptionOptional')}</label>
        <textarea
          className="input-control"
          style={{ minHeight: '65px', resize: 'vertical', fontSize: '0.85rem' }}
          placeholder={t('deck.notesPlaceholder')}
          value={value.description}
          onChange={(e) => onChange({ description: e.target.value })}
        />
      </div>
    </>
  );
}
