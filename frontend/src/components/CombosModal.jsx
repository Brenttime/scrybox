import { useEffect, useState } from 'react';
import { X, ExternalLink, Infinity as InfinityIcon } from 'lucide-react';
import { useT } from '../utils/i18n';

// Find My Combos results (Commander Spellbook, proxied by /api/decks/:id/combos):
// combos already in the deck, then the ones a single card away.
function ComboCard({ combo, t }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="cmb-combo">
      <div className="cmb-cards">
        {combo.cards.map(c => (
          <span key={c.name} className={`cmb-card${c.missing ? ' is-missing' : ''}`} title={c.missing ? t('combos.missingCard') : c.name}>
            {c.image && <img src={c.image} alt="" loading="lazy" crossOrigin="anonymous" />}
            <span>{c.name}</span>
          </span>
        ))}
        {combo.templates.map(n => <span key={n} className="cmb-card cmb-template"><span>{n}</span></span>)}
      </div>
      {combo.produces.length > 0 && <div className="cmb-produces">{combo.produces.join(' · ')}</div>}
      <div className="cmb-meta">
        {combo.manaNeeded && <span>{t('combos.mana', { mana: combo.manaNeeded })}</span>}
        {combo.bracketTag && <span>{t('combos.bracket', { tag: combo.bracketTag })}</span>}
        <button type="button" className="coll-textbtn" onClick={() => setOpen(v => !v)}>{open ? t('combos.hideSteps') : t('combos.showSteps')}</button>
        <a href={`https://commanderspellbook.com/combo/${encodeURIComponent(combo.id)}/`} target="_blank" rel="noreferrer">
          {t('combos.open')} <ExternalLink size={11} />
        </a>
      </div>
      {open && (
        <div className="cmb-steps">
          {combo.prerequisites && <p className="cmb-pre">{combo.prerequisites}</p>}
          <p>{combo.description}</p>
        </div>
      )}
    </div>
  );
}

export default function CombosModal({ deckId, deckName, onClose }) {
  const { t } = useT();
  const [state, setState] = useState({ loading: true, error: '', data: null });

  useEffect(() => {
    let alive = true;
    fetch(`/api/decks/${encodeURIComponent(deckId)}/combos`)
      .then(async r => ({ ok: r.ok, body: await r.json().catch(() => ({})) }))
      .then(({ ok, body }) => {
        if (!alive) return;
        setState(ok ? { loading: false, error: '', data: body } : { loading: false, error: body.error || t('combos.error'), data: null });
      })
      .catch(() => { if (alive) setState({ loading: false, error: t('combos.error'), data: null }); });
    return () => { alive = false; };
  }, [deckId, t]);

  useEffect(() => {
    const onKey = (e) => { if (e.key === 'Escape') onClose(); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const { data } = state;
  return (
    <div className="modal-overlay cmb-overlay" onClick={e => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="glass-panel cmb-modal" role="dialog" aria-modal="true" aria-label={t('combos.title')}>
        <button className="modal-close btn btn-secondary btn-icon-only" onClick={onClose} aria-label={t('common.close')}><X size={18} /></button>
        <h2 className="cmb-title"><InfinityIcon size={20} /> {t('combos.title')}</h2>
        <div className="cmb-sub">{deckName} · {t('combos.via')}</div>
        {state.loading && <div className="cmb-msg"><div className="spinner" /> {t('combos.loading')}</div>}
        {state.error && <div className="cmb-msg cmb-err">{state.error}</div>}
        {data && (
          <>
            <h3 className="cmb-h">{t('combos.included', { count: data.included.length })}</h3>
            {data.included.length === 0 ? <div className="cmb-msg">{t('combos.none')}</div>
              : data.included.map(c => <ComboCard key={c.id} combo={c} t={t} />)}
            <h3 className="cmb-h">{t('combos.almost', { count: data.almostIncluded.length })}</h3>
            {data.almostIncluded.length === 0 ? <div className="cmb-msg">{t('combos.noneAlmost')}</div>
              : data.almostIncluded.map(c => <ComboCard key={c.id} combo={c} t={t} />)}
          </>
        )}
      </div>
    </div>
  );
}
