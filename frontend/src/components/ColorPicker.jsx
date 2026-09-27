import { COLORS } from '../utils/mtgFilters';
import { useT } from '../utils/i18n';

// WUBRG + colourless toggles styled as mana pips. `value` is an array of codes.
// Optional `mode`/`onModeChange` add the "any / exactly" switch for colour.
export default function ColorPicker({ value, onChange, mode, onModeChange, label, withColorless = true }) {
  const { t } = useT();
  const list = withColorless ? COLORS : COLORS.filter(c => c.value !== 'C');
  const toggle = code => onChange(value.includes(code) ? value.filter(v => v !== code) : [...value, code]);
  return (
    <div className="mtg-color-picker" role="group" aria-label={label}>
      <div className="mtg-pips">
        {list.map(c => (
          <button
            key={c.value}
            type="button"
            className={`mtg-pip mtg-pip-${c.value}${value.includes(c.value) ? ' on' : ''}`}
            aria-pressed={value.includes(c.value)}
            aria-label={t(`mtg.color.${c.value}`)}
            title={t(`mtg.color.${c.value}`)}
            onClick={() => toggle(c.value)}
          >
            {c.value}
          </button>
        ))}
      </div>
      {onModeChange && value.length > 0 && (
        <div className="mtg-mode">
          {['any', 'exact'].map(m => (
            <button key={m} type="button" className={`kw-chip${mode === m ? ' active' : ''}`} onClick={() => onModeChange(m)}>
              {t(`mtg.colorMode.${m}`)}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
