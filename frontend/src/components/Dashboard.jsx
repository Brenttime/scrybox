import { lazy, Suspense, useEffect, useRef, useState } from 'react';
import { TrendingUp, Mountain, BookOpen, Sparkles } from 'lucide-react';
import { getCardDisplayName } from '../utils/langHelper';
import { priceText } from '../utils/formatPrice';
import { useT } from '../utils/i18n';
import CardImage from './CardImage';

// Dashboard = Brent's approved "Bento + Wallet" layout: a bento grid of
// summary tiles (net worth sparkline, gain, cards, decks, colour donut, top
// card art, rarity), then Wallet-style carousels and ranked set rows. Charts
// are inline SVG, so the dashboard ships no Recharts at all.
const CardInspectorModal = lazy(() => import('./CardInspectorModal'));

const PIP = { White: 'W', Blue: 'U', Black: 'B', Red: 'R', Green: 'G', Multicolor: 'M', Colorless: 'C', Land: 'L' };
const PIP_COLOR = { W: '#f3e7b8', U: '#3d9cff', B: '#9a7bd1', R: '#ef5a42', G: '#35b46b', M: '#e2b33c', C: '#a5adba', L: '#9a6b4a' };
const RARITY_ORDER = ['Mythic', 'Rare', 'Uncommon', 'Common'];
const RARITY_COLOR = { Mythic: '#e8743b', Rare: '#e2b33c', Uncommon: '#a9b8c9', Common: '#59616d' };
const RANGES = ['7d', '30d', '1y', '5y'];

const artCrop = (url) => (url || '').replace('/normal/', '/art_crop/');

function DashboardChunkFallback() {
  const { t } = useT();
  return (
    <div className="dash-overlay-fallback">
      <div className="glass-panel" role="status" aria-live="polite" style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', padding: '1.5rem' }}>
        <div className="spinner" aria-hidden="true" />
        <span>{t('common.loading')}</span>
      </div>
    </div>
  );
}

// Gradient area sparkline you can scrub with a finger or mouse.
function Sparkline({ points, money, locale }) {
  const ref = useRef(null);
  const [idx, setIdx] = useState(null);
  if (!points || points.length < 2) return null;
  const W = 400;
  const H = 120;
  const vs = points.map(p => Number(p.value) || 0);
  let lo = Math.min(...vs);
  let hi = Math.max(...vs);
  const r = (hi - lo) || 1;
  lo -= r * 0.15; hi += r * 0.1;
  const X = (i) => (W * i) / (points.length - 1);
  const Y = (v) => H - 4 - ((H - 12) * (v - lo)) / (hi - lo);
  const d = vs.map((v, i) => `${i ? 'L' : 'M'}${X(i).toFixed(1)},${Y(v).toFixed(1)}`).join('');
  const move = (e) => {
    const b = ref.current?.getBoundingClientRect();
    if (!b) return;
    const cx = e.touches ? e.touches[0].clientX : e.clientX;
    const i = Math.round(((cx - b.left) / b.width) * (points.length - 1));
    setIdx(Math.max(0, Math.min(points.length - 1, i)));
  };
  const i = idx !== null && idx < points.length ? idx : null;
  const sel = i !== null ? points[i] : null;
  const fmtDate = (s) => { const dt = new Date(s); return Number.isNaN(dt.getTime()) ? s : dt.toLocaleDateString(locale); };
  return (
    <div
      className="dash-spark"
      ref={ref}
      onMouseMove={move}
      onMouseLeave={() => setIdx(null)}
      onTouchStart={move}
      onTouchMove={move}
      onTouchEnd={() => setIdx(null)}
    >
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden="true">
        <defs>
          <linearGradient id="dashSparkFill" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0" stopColor="var(--accent-red)" stopOpacity="0.35" />
            <stop offset="1" stopColor="var(--accent-red)" stopOpacity="0" />
          </linearGradient>
        </defs>
        <path d={`${d}L${W},${H}L0,${H}Z`} fill="url(#dashSparkFill)" />
        <path d={d} fill="none" stroke="var(--accent-red)" strokeWidth="2.4" strokeLinejoin="round" strokeLinecap="round" vectorEffect="non-scaling-stroke" />
        {sel && <line x1={X(i)} x2={X(i)} y1="0" y2={H} stroke="var(--text-muted)" strokeWidth="1" vectorEffect="non-scaling-stroke" />}
      </svg>
      {sel && (
        <>
          <span className="dash-spark-dot" style={{ left: `${(X(i) / W) * 100}%`, top: `${(Y(vs[i]) / H) * 100}%` }} />
          <span className="dash-spark-tip" style={{ left: `${Math.min(78, Math.max(22, (X(i) / W) * 100))}%` }}>
            <b>${money(sel.value)}</b> · {fmtDate(sel.date)}
          </span>
        </>
      )}
    </div>
  );
}

function Donut({ data, size, thickness, active, onPick, center }) {
  const total = data.reduce((s, x) => s + x.value, 0) || 1;
  const R = size / 2 - 6;
  const rIn = R - thickness;
  const gap = data.length > 1 ? 0.025 : 0;
  let a = -Math.PI / 2;
  const P = (ang, rr) => [size / 2 + rr * Math.cos(ang), size / 2 + rr * Math.sin(ang)];
  const arcs = data.map((x, i) => {
    const sweep = Math.min((x.value / total) * Math.PI * 2, Math.PI * 2 - 0.001);
    const a1 = a;
    const a2 = a + Math.max(sweep - gap, 0.001);
    a += sweep;
    const ro = R + (active === i ? 5 : 0);
    const [x1, y1] = P(a1, ro); const [x2, y2] = P(a2, ro);
    const [x3, y3] = P(a2, rIn); const [x4, y4] = P(a1, rIn);
    const big = a2 - a1 > Math.PI ? 1 : 0;
    return (
      <path
        key={x.name}
        d={`M${x1},${y1}A${ro},${ro} 0 ${big} 1 ${x2},${y2}L${x3},${y3}A${rIn},${rIn} 0 ${big} 0 ${x4},${y4}Z`}
        fill={PIP_COLOR[PIP[x.name]] || '#6b7280'}
        fillOpacity={active === null || active === i ? 1 : 0.22}
        style={{ cursor: 'pointer', transition: 'fill-opacity .2s' }}
        onClick={(e) => { e.stopPropagation(); onPick(i); }}
      />
    );
  });
  return (
    <div className="dash-donut" style={{ width: size, height: size }}>
      <svg width={size} height={size} viewBox={`0 0 ${size} ${size}`}>{arcs}</svg>
      <div className="dash-donut-center">{center}</div>
    </div>
  );
}

function Pip({ name }) {
  const k = PIP[name] || '?';
  return <span className="dash-pip" style={{ background: PIP_COLOR[k] || '#6b7280' }}>{k}</span>;
}

function Dashboard({ statsTrigger, onNavigate, onUpdate, showToast }) {
  const { t, locale } = useT();
  // Money and dates follow the interface language, not the browser's.
  const money = (n) => (Number(n) || 0).toLocaleString(locale, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  const money0 = (n) => (Number(n) || 0).toLocaleString(locale, { maximumFractionDigits: 0 });
  const num = (n) => Number(n || 0).toLocaleString(locale);
  // Last dashboard, per user, painted instantly on open while a fresh copy
  // loads (stale-while-revalidate). Keyed by username so accounts never mix.
  const cacheKey = (name) => {
    try { return `scrybox_dash_${JSON.parse(localStorage.getItem('bindarr_user') || '{}').username || 'anon'}_${name}`; } catch { return `scrybox_dash_anon_${name}`; }
  };
  const readCache = (name) => { try { return JSON.parse(localStorage.getItem(cacheKey(name)) || 'null'); } catch { return null; } };
  const writeCache = (name, v) => { try { localStorage.setItem(cacheKey(name), JSON.stringify(v)); } catch { /* quota */ } };
  const [stats, setStats] = useState(() => readCache('stats'));
  const [loading, setLoading] = useState(() => !readCache('stats'));
  const [error, setError] = useState(null);
  const [timePeriod, setTimePeriod] = useState('30d');
  const [historyData, setHistoryData] = useState(() => readCache('history_30d') || []);
  const [decks, setDecks] = useState(() => readCache('decks'));
  const [activeColor, setActiveColor] = useState(null);
  const [inspectorCard, setInspectorCard] = useState(null);

  useEffect(() => {
    fetchStats();
    fetchDecks();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [statsTrigger]);

  useEffect(() => {
    if (stats && stats.summary.totalCards > 0) fetchTimelineHistory();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timePeriod, stats]);

  const fetchStats = async () => {
    try {
      if (!stats) setLoading(true);
      const response = await fetch('/api/stats');
      if (!response.ok) throw new Error(t('dash.errStats'));
      const data = await response.json();
      setStats(data);
      setError(null);
      writeCache('stats', data);
    } catch (err) {
      console.error(err);
      // Keep showing the cached dashboard if the refresh fails.
      if (!stats) setError(err.message);
    } finally {
      setLoading(false);
    }
  };

  // /api/decks is served from the trigger-revision deck-list cache, so the
  // counts cost nothing extra on the server.
  const fetchDecks = async () => {
    try {
      const r = await fetch('/api/decks');
      if (!r.ok) return;
      const list = await r.json();
      if (!Array.isArray(list)) return;
      const summary = {
        count: list.length,
        built: list.filter(d => d.checked_out).length,
        needCards: list.filter(d => !d.checked_out && Number(d.missing_copies) > 0).length,
      };
      setDecks(summary);
      writeCache('decks', summary);
    } catch (err) {
      console.error('Error loading decks:', err);
    }
  };

  const fetchTimelineHistory = async () => {
    const period = timePeriod;
    try {
      const cachedHistory = readCache(`history_${period}`);
      setHistoryData(cachedHistory || []);
      const response = await fetch(`/api/stats/history?period=${period}`);
      if (response.ok) {
        const data = await response.json();
        setHistoryData(data);
        writeCache(`history_${period}`, data);
      }
    } catch (err) {
      console.error('Error loading history timeline:', err);
    }
  };

  if (loading) return <div className="spinner"></div>;

  if (error) {
    return (
      <div className="glass-panel" style={{ textAlign: 'center', color: 'var(--text-secondary)' }}>
        <p>{t('dash.errLoad', { error })}</p>
        <button className="btn btn-primary" onClick={fetchStats} style={{ marginTop: '1rem' }}>{t('dash.retry')}</button>
      </div>
    );
  }

  if (!stats || stats.summary.totalCards === 0) {
    return (
      <div className="glass-panel" style={{ textAlign: 'center', padding: '3rem 1.5rem', color: 'var(--text-secondary)' }}>
        <TrendingUp size={48} style={{ color: 'var(--accent-red)', marginBottom: '1.5rem', opacity: 0.8 }} />
        <h2 style={{ color: 'var(--text-strong)', marginBottom: '0.5rem' }}>{t('dash.emptyTitle')}</h2>
        <p style={{ maxWidth: '400px', margin: '0 auto 1.5rem auto' }}>{t('dash.emptyBody')}</p>
        <button className="btn btn-primary" onClick={() => onNavigate && onNavigate('add-cards')}>{t('dash.goToAddCards')}</button>
      </div>
    );
  }

  const { summary, rarities = [], sets = [], topValuable = [], recentAdditions = [], colorValues = [] } = stats;
  const change = { '7d': summary.change7d, '30d': summary.change30d, '1y': summary.change1y, '5y': summary.change5y }[timePeriod];
  const history = (historyData || []).filter(p => Number(p.value) > 0);
  // 5Y only when the server actually has something for it.
  const ranges = RANGES.filter(p => p !== '5y' || summary.change5y?.available);
  const roi = summary.roi || { abs: 0, pct: null };
  const gainUp = (roi.abs || 0) >= 0;
  const colorData = colorValues.map(c => ({ ...c, value: Number(c.value) || 0 })).filter(c => c.value > 0);
  const colorTotal = colorData.reduce((s, c) => s + c.value, 0);
  const pct = (v) => (colorTotal ? Math.round((v / colorTotal) * 100) : 0);
  const selColor = activeColor !== null ? colorData[activeColor] : null;
  const rarityRows = RARITY_ORDER.map(n => rarities.find(r => String(r.name).toLowerCase() === n.toLowerCase())).filter(Boolean);
  const rarityMax = Math.max(1, ...rarityRows.map(r => Number(r.value) || 0));
  const topSets = sets.slice(0, 6);
  const setMax = Math.max(1, ...topSets.map(s => s.value || 0));
  const top = topValuable[0];
  const name = (c) => getCardDisplayName(c.name, c.language, c.printed_name);

  const cardTile = (c, cls) => (
    <button key={c.entry_id ?? c.card_id} type="button" className={`dash-cardtile ${cls}`} onClick={() => setInspectorCard(c)}>
      <CardImage card={c} loading="lazy" className="dash-cardtile-img" />
      <span className="dash-cardtile-name">{name(c)}</span>
      <span className="dash-cardtile-price">{priceText(c.price_trend, c.price_currency)}</span>
    </button>
  );

  return (
    <div className="dash">
      <div className="dash-head">
        <h2>{t('nav.dashboard')}</h2>
        <div className="dash-seg" role="tablist" aria-label={t('dash.range')}>
          {ranges.map(p => (
            <button key={p} type="button" role="tab" aria-selected={timePeriod === p} className={timePeriod === p ? 'on' : ''} onClick={() => setTimePeriod(p)}>
              {p.toUpperCase()}
            </button>
          ))}
        </div>
      </div>

      <div className="dash-bento">
        <div className="dash-tile dash-net">
          <div className="dash-k">{t('dash.netWorth')}</div>
          <div className="dash-val dash-net-val">${money(summary.totalValue)}</div>
          <div className="dash-change">
            {change && change.available ? (
              <span className={change.abs >= 0 ? 'up' : 'down'}>
                {change.abs >= 0 ? '▲' : '▼'} ${money(Math.abs(change.abs))} ({change.pct > 0 ? '+' : ''}{change.pct}%)
              </span>
            ) : (
              <span className="dim">{t('dash.noPriceHistory')}</span>
            )}
          </div>
          <Sparkline points={history} money={money} locale={locale} />
        </div>

        <div className="dash-tile">
          <div className="dash-k">{t('dash.gain')}</div>
          <div className={`dash-val ${gainUp ? 'up' : 'down'}`}>{gainUp ? '+' : '−'}${money0(Math.abs(roi.abs || 0))}</div>
          <div className={`dash-sub ${roi.pct === null ? 'dim' : (gainUp ? 'up' : 'down')}`}>
            {roi.pct === null ? t('dash.roiUnset') : `${gainUp ? '+' : ''}${roi.pct}%`}
          </div>
        </div>

        <div className="dash-tile">
          <div className="dash-k">{t('dash.cards')}</div>
          <div className="dash-val">{num(summary.totalCards)}</div>
          <div className="dash-sub dim">{t('dash.avgCard', { price: money(summary.avgCardValue) })}</div>
        </div>

        <button type="button" className="dash-tile dash-tile-btn" onClick={() => onNavigate && onNavigate('deckbuilder')}>
          <span className="dash-k">{t('dash.decks')}</span>
          <span className="dash-val">{decks ? num(decks.count) : '—'}</span>
          {decks && <span className="dash-sub dim">{t('dash.decksSub', { built: decks.built, need: decks.needCards })}</span>}
        </button>

        {top && (
          <button type="button" className="dash-tile dash-art" onClick={() => setInspectorCard(top)}>
            <CardImage card={top} src={artCrop(top.image_url)} fallbackSrc={top.image_url} className="dash-art-img" alt="" />
            <span className="dash-art-shade" />
            <span className="dash-art-cap">
              <span className="dash-k">{t('dash.topCard')}</span>
              <span className="dash-art-name">{name(top)}</span>
              <span className="dash-art-price">{priceText(top.price_trend, top.price_currency)}</span>
            </span>
          </button>
        )}

        <div className="dash-tile dash-wide dash-colors" onClick={() => setActiveColor(null)}>
          <div className="dash-k" style={{ marginBottom: 8 }}>{t('dash.valueByColor')}</div>
          {colorData.length === 0 ? (
            <div className="dim">{t('dash.noTypeData')}</div>
          ) : (
            <div className="dash-donut-wrap">
              <Donut
                data={colorData}
                size={132}
                thickness={17}
                active={activeColor}
                onPick={(i) => setActiveColor(a => (a === i ? null : i))}
                center={selColor ? (
                  <>
                    <b>${money0(selColor.value)}</b>
                    <small>{selColor.name} · {pct(selColor.value)}%</small>
                  </>
                ) : (
                  <>
                    <b>${money0(colorTotal)}</b>
                    <small>{t('dash.colorGroups', { count: colorData.length })}</small>
                  </>
                )}
              />
              <div className="dash-legend">
                {colorData.map((c, i) => (
                  <button key={c.name} type="button" className={activeColor === i ? 'on' : ''} onClick={(e) => { e.stopPropagation(); setActiveColor(a => (a === i ? null : i)); }}>
                    <Pip name={c.name} />
                    <span className="dash-legend-name">{c.name}</span>
                    <b>{pct(c.value)}%</b>
                  </button>
                ))}
              </div>
            </div>
          )}
        </div>

        <div className="dash-tile dash-rarity">
          <div className="dash-k">{t('dash.rarity')}</div>
          <div className="dash-rbars">
            {rarityRows.map(r => (
              <div key={r.name} className="dash-rbar">
                <span>{r.name}</span>
                <span className="dim">{num(r.value)}</span>
                <i style={{ width: `${((Number(r.value) || 0) / rarityMax) * 100}%`, background: RARITY_COLOR[r.name] || '#59616d' }} />
              </div>
            ))}
          </div>
        </div>
      </div>

      <div className="dash-sections">
        <div className="dash-col">
          {topValuable.length > 0 && (
            <section className="dash-sec">
              <h3 className="dash-h">{t('dash.mostValuable')}</h3>
              <div className="dash-carousel">{topValuable.map(c => cardTile(c, 'big'))}</div>
            </section>
          )}
          {recentAdditions.length > 0 && (
            <section className="dash-sec">
              <h3 className="dash-h">{t('dash.recentlyAdded')}</h3>
              <div className="dash-carousel">{recentAdditions.map(c => cardTile(c, 'small'))}</div>
            </section>
          )}
        </div>
        <div className="dash-col">
          {topSets.length > 0 && (
            <section className="dash-sec">
              <h3 className="dash-h">{t('dash.topSets')}</h3>
              <div className="dash-setrows">
                {topSets.map(s => (
                  <div key={s.id || s.name} className="dash-setrow">
                    <span className="dash-setrow-name">{s.name} <span className="dim">· {num(s.count)}</span></span>
                    <b>${money0(s.value)}</b>
                    <span className="dash-setrow-bar"><i style={{ width: `${(s.value / setMax) * 100}%` }} /></span>
                  </div>
                ))}
              </div>
            </section>
          )}
          <section className="dash-sec">
            <h3 className="dash-h">{t('dash.tools')}</h3>
            <div className="dash-tools">
              {[
                { key: 'rules', icon: BookOpen, label: t('rules.title') },
                { key: 'limited', icon: Mountain, label: t('nav.limited') },
                { key: 'keywords', icon: Sparkles, label: t('kw.title') },
              ].map(({ key, icon: Icon, label }) => (
                <button key={key} type="button" className="dash-tile dash-tool" onClick={() => onNavigate && onNavigate(key)}>
                  <span className="dash-tool-ic"><Icon size={17} /></span>
                  <span className="dash-tool-label">{label}</span>
                </button>
              ))}
            </div>
          </section>
        </div>
      </div>

      {inspectorCard && (
        <Suspense fallback={<DashboardChunkFallback />}>
          <CardInspectorModal
            card={inspectorCard}
            onClose={() => setInspectorCard(null)}
            onUpdate={onUpdate}
            showToast={showToast}
          />
        </Suspense>
      )}
    </div>
  );
}

export default Dashboard;
