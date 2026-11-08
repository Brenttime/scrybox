import { ListChecks, Library } from 'lucide-react';
import { useT } from '../utils/i18n';
import { COLLECTION } from '../utils/addDestination';

// Compact "Add to: Collection / <list>" picker. A native select so phones get
// the system wheel/sheet.
export default function DestinationSelect({ dest, setDest, lists, className = '' }) {
  const { t } = useT();
  const Icon = dest === COLLECTION ? Library : ListChecks;
  return (
    <label className={`add-dest ${className}`}>
      <Icon size={14} aria-hidden="true" />
      <select value={dest} onChange={e => setDest(e.target.value)} aria-label={t('addDest.label')}>
        <option value={COLLECTION}>{t('addDest.collection')}</option>
        {lists.length > 0 && (
          <optgroup label={t('addDest.lists')}>
            {lists.map(l => <option key={l.id} value={String(l.id)}>{l.name}</option>)}
          </optgroup>
        )}
      </select>
    </label>
  );
}
