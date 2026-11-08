import { useEffect, useState } from 'react';
import { Tag } from 'lucide-react';
import { useT } from '../utils/i18n';

// Tagger oracle tags for the card details sheet, like tagger.scryfall.com:
// tags on the card itself, then the ancestor tags they imply ("inherited",
// each saying which direct tag it comes through). Tapping a tag searches it.
export default function OracleTags({ cardId, oracleId, onTagClick }) {
  const { t } = useT();
  const [data, setData] = useState(null);

  useEffect(() => {
    setData(null);
    if (!cardId) return undefined;
    let cancelled = false;
    const qs = oracleId ? `?oracle_id=${encodeURIComponent(oracleId)}` : '';
    fetch(`/api/cards/${encodeURIComponent(cardId)}/oracle-tags${qs}`)
      .then(r => (r.ok ? r.json() : null))
      .then(d => { if (!cancelled) setData(d); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [cardId, oracleId]);

  if (!data || (!data.tags?.length && !data.inherited?.length)) return null;

  const chip = (tag, inherited) => (
    <button
      key={tag.slug}
      type="button"
      className={`ci-otag${inherited ? ' inherited' : ''}`}
      title={tag.description || undefined}
      onClick={() => onTagClick?.(tag.slug)}
    >
      {tag.label || tag.slug}
      {inherited && tag.via?.length > 0 && <span className="ci-otag-via">← {tag.via.join(', ')}</span>}
    </button>
  );

  return (
    <section className="ci-otags">
      <div className="ci-otags-h"><Tag size={12} /> {t('inspector.otags')}</div>
      <div className="ci-otags-row">{data.tags.map(tag => chip(tag, false))}</div>
      {data.inherited.length > 0 && (
        <>
          <div className="ci-otags-h sub">{t('inspector.otagsInherited')}</div>
          <div className="ci-otags-row">{data.inherited.map(tag => chip(tag, true))}</div>
        </>
      )}
    </section>
  );
}
