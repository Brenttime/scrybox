import { Suspense, lazy, useEffect, useMemo, useRef, useState } from 'react';
import { ChevronLeft, Dices, Search, Swords, RotateCw, Info, X } from 'lucide-react';
import { useT } from '../utils/i18n';
import { rememberOpen } from '../utils/viewMemory';
import { cardPriceText } from '../utils/formatPrice';
import CardImage from './CardImage';
import {
  masterSets, filterMasterSets, commanderQuery, dedupeCommanders, pickWinner,
  buildSlices, landingRotation, sliceColor, deckNameFor,
} from '../utils/commanderWheel';

const CardInspectorModal = lazy(() => import('./CardInspectorModal'));

// Commander Wheel: pick one or more master sets (a set plus its children,
// e.g. every Final Fantasy product), spin, and start a deck from the result.
// Cards come from the local Scryfall engine (`e:` includes reprints).

const PAGE = 250;
const MAX_PAGES = 12;
const SPIN_MS = 5200;
const STORE = 'scrybox.commanderWheel.sets';

function loadPicked() {
  try { const a = JSON.parse(localStorage.getItem(STORE) || '[]'); return Array.isArray(a) ? a.filter(x => typeof x === 'string') : []; } catch { return []; }
}

async function fetchQuery(q, signal) {
  const rows = [];
  for (let page = 1; page <= MAX_PAGES; page++) {
    const params = new URLSearchParams({ q, scope: 'internet', lang: 'en', page: String(page), limit: String(PAGE) });
    const res = await fetch(`/api/search?${params}`, { signal });
    if (!res.ok) throw new Error(String(res.status));
    const body = await res.json();
    const got = Array.isArray(body) ? body : [];
    rows.push(...got);
    const total = parseInt(res.headers.get('X-Total-Count'), 10);
    if (got.length < PAGE || (Number.isFinite(total) && rows.length >= total)) break;
  }
  return rows;
}

// Two passes: the family's own printings, then The List cards filed with the
// same release date as the family's main set. The date comes from the cards
// themselves (the set index can disagree with Scryfall's card dates).
async function fetchPool(codes, rootCodes, fallbackDates, signal) {
  const own = await fetchQuery(commanderQuery(codes), signal);
  const roots = new Set(rootCodes);
  const dates = new Set(own.filter(r => roots.has(String(r.set_id || '').toLowerCase())).map(r => r.released_at).filter(Boolean));
  if (!dates.size) fallbackDates.forEach(d => dates.add(d));
  const listQ = `is:commander -is:funny e:plst (${[...dates].map(d => `date=${d}`).join(' or ')}) unique:prints`;
  const list = dates.size ? await fetchQuery(listQ, signal) : [];
  return dedupeCommanders([...own, ...list], [...codes, 'plst']);
}

const R = 160;
function slicePath(i, n) {
  if (n === 1) return `M ${R} 0 A ${R} ${R} 0 1 1 ${R - 0.01} 0 Z`;
  const a0 = (i / n) * 2 * Math.PI;
  const a1 = ((i + 1) / n) * 2 * Math.PI;
  const p = a => [R + R * Math.sin(a), R - R * Math.cos(a)];
  const [x0, y0] = p(a0);
  const [x1, y1] = p(a1);
  return `M ${R} ${R} L ${x0} ${y0} A ${R} ${R} 0 ${a1 - a0 > Math.PI ? 1 : 0} 1 ${x1} ${y1} Z`;
}
const shortName = s => { const n = String(s || '').split(' // ')[0].split(',')[0]; return n.length > 18 ? `${n.slice(0, 17)}…` : n; };

function Wheel({ slices, rotation, spinning }) {
  const n = slices.length;
  return (
    <div className="cw-wheel-wrap">
      <div className="cw-pointer" aria-hidden="true" />
      <svg className="cw-wheel" viewBox={`0 0 ${R * 2} ${R * 2}`}
        style={{ transform: `rotate(${rotation}deg)`, transition: spinning ? `transform ${SPIN_MS}ms cubic-bezier(0.12, 0.7, 0.08, 1)` : 'none' }}>
        {n === 0 && <circle cx={R} cy={R} r={R} className="cw-empty" />}
        {slices.map((c, i) => {
          const mid = ((i + 0.5) / n) * 360;
          return (
            <g key={`${c.name}-${i}`}>
              <path d={slicePath(i, n)} fill={sliceColor(c.color_identity, i)} className="cw-slice" />
              <text x={R} y={R} className="cw-label" fontSize={n > 16 ? 9 : 11}
                transform={`rotate(${mid - 90} ${R} ${R}) translate(${R * 0.32} 3)`}>{shortName(c.name)}</text>
            </g>
          );
        })}
        <circle cx={R} cy={R} r={22} className="cw-hub" />
      </svg>
    </div>
  );
}

export default function CommanderWheel({ onNavigate, showToast }) {
  const { t } = useT();
  const [families, setFamilies] = useState([]);
  const [filter, setFilter] = useState('');
  const [picked, setPicked] = useState(loadPicked);
  const [pool, setPool] = useState([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [slices, setSlices] = useState([]);
  const [rotation, setRotation] = useState(0);
  const [spinning, setSpinning] = useState(false);
  const [winner, setWinner] = useState(null);
  const [inspect, setInspect] = useState(false);
  const [creating, setCreating] = useState(false);
  const timer = useRef(null);

  useEffect(() => {
    fetch('/api/sets?game=mtg&lang=en&tree=1').then(r => (r.ok ? r.json() : [])).then(d => setFamilies(masterSets(d))).catch(() => setFamilies([]));
    return () => clearTimeout(timer.current);
  }, []);

  const pickedFamilies = useMemo(() => families.filter(f => picked.includes(f.code)), [families, picked]);
  const codes = useMemo(() => [...new Set(pickedFamilies.flatMap(f => f.codes))], [pickedFamilies]);
  const dates = useMemo(() => pickedFamilies.map(f => f.released).filter(Boolean), [pickedFamilies]);
  const roots = useMemo(() => pickedFamilies.map(f => f.code), [pickedFamilies]);
  const codesKey = `${codes.join(',')}|${dates.join(',')}`;

  useEffect(() => {
    try { localStorage.setItem(STORE, JSON.stringify(picked)); } catch { /* best effort */ }
  }, [picked]);

  useEffect(() => {
    setWinner(null);
    if (!codes.length) { setPool([]); setSlices([]); return undefined; }
    const ctl = new AbortController();
    setLoading(true);
    setError('');
    fetchPool(codes, roots, dates, ctl.signal)
      .then(p => { setPool(p); setSlices(buildSlices(p, pickWinner(p)).slices); })
      .catch(e => { if (e.name !== 'AbortError') { setError(t('cw.error')); setPool([]); setSlices([]); } })
      .finally(() => { if (!ctl.signal.aborted) setLoading(false); });
    return () => ctl.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [codesKey]);

  const visible = useMemo(() => filterMasterSets(families, filter).slice(0, 60), [families, filter]);
  const toggle = code => setPicked(p => (p.includes(code) ? p.filter(c => c !== code) : [...p, code]));

  const spin = () => {
    if (spinning || !pool.length) return;
    const w = pickWinner(pool);
    const { slices: s, winnerSlice } = buildSlices(pool, w);
    setWinner(null);
    setSlices(s);
    setSpinning(true);
    requestAnimationFrame(() => setRotation(r => landingRotation(r, winnerSlice, s.length, 6, Math.random())));
    clearTimeout(timer.current);
    timer.current = setTimeout(() => { setSpinning(false); setWinner(pool[w]); }, SPIN_MS + 80);
  };

  const startDeck = async () => {
    if (!winner || creating) return;
    setCreating(true);
    try {
      // decklist_text is the create-time import path: it adds the card even
      // when it isn't owned yet (the single-card add route enforces
      // ownership), so a rolled commander always lands in the new deck.
      const from = pickedFamilies.map(f => f.name).join(', ');
      const res = await fetch('/api/decks', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name: deckNameFor(winner), description: t('cw.deckDesc', { sets: from }),
          format: 'Commander / EDH', category: 'Casual', target_size: 100,
          decklist_text: `1 ${winner.name}`,
        }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok || !body.id) throw new Error('create');
      const check = await fetch(`/api/decks/${body.id}`).then(r => (r.ok ? r.json() : null)).catch(() => null);
      const landed = Array.isArray(check?.cards) && check.cards.length > 0;
      showToast && showToast(landed ? t('cw.deckCreated', { name: winner.name }) : t('cw.addFailed'));
      onNavigate && onNavigate('deckbuilder');
      rememberOpen('deckbuilder', body.id);
    } catch {
      showToast && showToast(t('cw.createFailed'));
    } finally {
      setCreating(false);
    }
  };

  const price = winner ? cardPriceText(winner) : '';

  return (
    <div className="cw-view">
      <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', marginBottom: '1rem' }}>
        <button type="button" className="btn btn-secondary btn-icon-only" onClick={() => onNavigate && onNavigate('dashboard')} aria-label={t('common.back')}>
          <ChevronLeft size={18} />
        </button>
        <h2 style={{ margin: 0, color: 'var(--text-strong)', display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
          <Dices size={20} /> {t('cw.title')}
        </h2>
      </div>

      <div className="cw-layout">
        <section className="glass-panel cw-sets">
          <h3 className="cw-h">{t('cw.pickSets')}</h3>
          <div style={{ position: 'relative', marginBottom: '0.5rem' }}>
            <Search size={15} style={{ position: 'absolute', left: 10, top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
            <input className="form-input" style={{ paddingLeft: 32, width: '100%' }} value={filter} onChange={e => setFilter(e.target.value)} placeholder={t('cw.searchSets')} aria-label={t('cw.searchSets')} />
          </div>
          {pickedFamilies.length > 0 && (
            <div className="cw-chips">
              {pickedFamilies.map(f => (
                <button key={f.code} type="button" className="cw-chip on" onClick={() => toggle(f.code)} aria-label={t('cw.remove', { name: f.name })}>
                  {f.name} <X size={12} />
                </button>
              ))}
            </div>
          )}
          <div className="cw-setlist">
            {visible.map(f => (
              <button key={f.code} type="button" className={`cw-setrow${picked.includes(f.code) ? ' on' : ''}`} onClick={() => toggle(f.code)} aria-pressed={picked.includes(f.code)}>
                {f.symbol && <img crossOrigin="anonymous" src={f.symbol} alt="" className="cw-sym" loading="lazy" />}
                <span className="cw-setname">{f.name}
                  {f.childNames.length > 0 && <span className="cw-setkids">{t('cw.includes', { n: f.codes.length })}</span>}
                </span>
                <span className="cw-setdate">{(f.released || '').slice(0, 4)}</span>
              </button>
            ))}
          </div>
        </section>

        <section className="glass-panel cw-stage">
          <Wheel slices={slices} rotation={rotation} spinning={spinning} />
          <div className="cw-status">
            {loading ? t('cw.loading') : error || (codes.length ? t('cw.poolCount', { n: pool.length }) : t('cw.pickFirst'))}
          </div>
          <button type="button" className="btn btn-primary cw-spin" onClick={spin} disabled={spinning || loading || !pool.length}>
            <RotateCw size={18} /> {spinning ? t('cw.spinning') : t('cw.spin')}
          </button>

          {winner && (
            <div className="cw-result" role="status">
              <CardImage card={winner} alt={winner.name} className="cw-art" onClick={() => setInspect(true)} />
              <div className="cw-info">
                <div className="cw-eyebrow">{t('cw.yourCommander')}</div>
                <h3 className="cw-name">{winner.name}</h3>
                <div className="cw-meta">
                  {String(winner.color_identity || '').replace(/[^WUBRG]/gi, '').split('').map(c => <img key={c} src={`/mana/${c.toUpperCase()}.svg`} alt={c} className="cw-pip" />)}
                  <span>{winner.set_name}{winner.number ? ` #${winner.number}` : ''}</span>
                  {price && <span className="cw-price">{price}</span>}
                </div>
                {winner.oracle_text && <p className="cw-oracle">{winner.oracle_text}</p>}
                <div className="cw-actions">
                  <button type="button" className="btn btn-primary" onClick={startDeck} disabled={creating}><Swords size={16} /> {creating ? t('cw.creating') : t('cw.startDeck')}</button>
                  <button type="button" className="btn btn-secondary" onClick={() => setInspect(true)}><Info size={16} /> {t('cw.details')}</button>
                  <button type="button" className="btn btn-secondary" onClick={spin} disabled={spinning}><RotateCw size={16} /> {t('cw.again')}</button>
                </div>
              </div>
            </div>
          )}
        </section>
      </div>

      {inspect && winner && (
        <Suspense fallback={null}>
          <CardInspectorModal card={{ ...winner, card_id: winner.id, quantity: winner.owned_qty || 0 }} readOnly onClose={() => setInspect(false)} showToast={showToast} />
        </Suspense>
      )}
    </div>
  );
}
