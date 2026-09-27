import { lazy, Suspense, useEffect, useState } from 'react';
import { TrendingUp, Coins, Library, Trophy, Plus, ArrowUpRight, Mountain, BookOpen, Sparkles } from 'lucide-react';
import { getCardDisplayName } from '../utils/langHelper';
import { formatPrice, priceText } from '../utils/formatPrice';
import { getPrintingBadgeLabel, getPrintingBadgeStyle } from '../utils/cardPrinting';
import { useT } from '../utils/i18n';
import CardImage from './CardImage';

const DashboardCharts = lazy(() => import('./DashboardCharts'));
const CardInspectorModal = lazy(() => import('./CardInspectorModal'));

function DashboardChunkFallback({ overlay = false, style }) {
  const { t } = useT();
  const status = (
    <div
      className="glass-panel"
      role="status"
      aria-live="polite"
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        gap: '0.75rem',
        color: 'var(--text-secondary)',
        ...style,
      }}
    >
      <div className="spinner" aria-hidden="true" />
      <span>{t('common.loading')}</span>
    </div>
  );

  if (!overlay) return status;
  return (
    <div
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 1000,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        background: 'rgba(0, 0, 0, 0.65)',
        padding: '1rem',
      }}
    >
      {status}
    </div>
  );
}

function Dashboard({ statsTrigger, onNavigate, onUpdate, showToast }) {
  const { t, locale } = useT();
  // Money and dates follow the interface language, not the browser's: a user who
  // picked German sees 1.234,56 and 3.8.2026 even on an en-US browser.
  const money = (n) => (n || 0).toLocaleString(locale, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  const [stats, setStats] = useState(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(null);
  const [timePeriod, setTimePeriod] = useState('30d');
  
  // Timeline Chart State
  const [historyData, setHistoryData] = useState([]);
  const [loadingHistory, setLoadingHistory] = useState(false);

  // Clickable Card Inspector State
  const [inspectorCard, setInspectorCard] = useState(null);

  useEffect(() => {
    fetchStats();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [statsTrigger]);

  useEffect(() => {
    if (stats && stats.summary.totalCards > 0) {
      fetchTimelineHistory();
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timePeriod, stats]);

  const fetchStats = async () => {
    try {
      setLoading(true);
      const response = await fetch('/api/stats');
      if (!response.ok) {
        throw new Error(t('dash.errStats'));
      }
      const data = await response.json();
      setStats(data);
    } catch (err) {
      console.error(err);
      setError(err.message);
    } finally {
      setLoading(false);
    }
  };

  const fetchTimelineHistory = async () => {
    try {
      setLoadingHistory(true);
      const response = await fetch(`/api/stats/history?period=${timePeriod}`);
      if (response.ok) {
        const data = await response.json();
        setHistoryData(data);
      }
    } catch (err) {
      console.error('Error loading history timeline:', err);
    } finally {
      setLoadingHistory(false);
    }
  };

  if (loading) {
    return <div className="spinner"></div>;
  }

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
        <h2 style={{ color: 'var(--text-strong)', marginBottom: '0.5rem' }}>
          {t('dash.emptyTitle')}
        </h2>
        <p style={{ maxWidth: '400px', margin: '0 auto 1.5rem auto' }}>
          {t('dash.emptyBody')}
        </p>
        <div style={{ display: 'flex', justifyContent: 'center', gap: '1rem' }}>
          <div style={{ display: 'inline-block' }}>
            <button className="btn btn-primary" onClick={() => onNavigate && onNavigate('add-cards')}>{t('dash.goToAddCards')}</button>
          </div>
        </div>
      </div>
    );
  }

  const { summary, types, rarities, sets, topValuable, recentAdditions = [], setProgress } = stats;

  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '0.5rem', flexWrap: 'wrap', marginBottom: '1rem' }}>
        <button type="button" className="btn btn-secondary" onClick={() => onNavigate && onNavigate('rules')}>
          <BookOpen size={16} /> {t('rules.title')}
        </button>
        <button type="button" className="btn btn-secondary" onClick={() => onNavigate && onNavigate('limited')}>
          <Mountain size={16} /> {t('nav.limited')}
        </button>
        <button type="button" className="btn btn-secondary" onClick={() => onNavigate && onNavigate('keywords')}>
          <Sparkles size={16} /> {t('kw.title')}
        </button>
      </div>
      {/* Metrics Summary Grid */}
      <div className="metrics-grid">
        {/* Net Worth Card with historical switcher */}
        <div className="glass-panel metric-card accent-networth">
          <div className="metric-header" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <span style={{ display: 'inline-flex', alignItems: 'center', gap: '0.5rem' }}>
              <span className="metric-icon" style={{ width: '28px', height: '28px' }}><TrendingUp size={16} /></span>
              {t('dash.netWorth')}
            </span>
            <div style={{ display: 'flex', gap: '4px', background: 'rgba(255,255,255,0.05)', padding: '2px', borderRadius: '4px' }}>
              {['7d', '30d', '1y', '5y'].map(p => (
                <button 
                  key={p} 
                  type="button" 
                  onClick={() => setTimePeriod(p)}
                  style={{
                    padding: '2px 6px',
                    fontSize: '0.65rem',
                    border: 'none',
                    borderRadius: '3px',
                    background: timePeriod === p ? 'var(--success)' : 'transparent',
                    color: 'var(--text-strong)',
                    cursor: 'pointer',
                    fontWeight: 700,
                    transition: 'all 0.15s ease'
                  }}
                >
                  {p.toUpperCase()}
                </button>
              ))}
            </div>
          </div>
          <div className="metric-value">${money(summary.totalValue)}</div>
          {(() => {
            const change = timePeriod === '7d' ? summary.change7d :
                           timePeriod === '30d' ? summary.change30d :
                           timePeriod === '1y' ? summary.change1y : summary.change5y;
            // change7d/30d use Cardmarket's real avg7/avg30 (only real source
            // available); change1y/5y have no real historical price source
            // anywhere, so they're marked unavailable rather than faked.
            if (!change || !change.available) {
              return (
                <div className="metric-footer" style={{ color: 'var(--text-muted)' }}>
                  <span>{t('dash.noPriceHistory')}</span>
                </div>
              );
            }
            const isPositive = change.abs >= 0;
            return (
              <div className={`metric-footer ${isPositive ? 'positive' : 'negative'}`} style={{ display: 'flex', alignItems: 'center', gap: '4px' }}>
                <TrendingUp size={12} style={{ transform: isPositive ? 'none' : 'rotate(180deg)' }} />
                <span>
                  {isPositive ? '+' : ''}${money(change.abs)} ({isPositive ? '+' : ''}{change.pct}%)
                </span>
              </div>
            );
          })()}
        </div>

        {/* Total Invested (cost basis) */}
        <div className="glass-panel metric-card accent-invested">
          <div className="metric-header">
            <span>{t('dash.totalInvested')}</span>
            <span className="metric-icon"><Coins size={18} /></span>
          </div>
          <div className="metric-value">${money(summary.totalSpent)}</div>
          <div className="metric-footer">
            <span>{t('dash.avgPerCard', { price: formatPrice(summary.avgCardValue) })}</span>
          </div>
        </div>

        {/* Unrealized Gain / ROI */}
        {(() => {
          const roi = summary.roi || { abs: 0, pct: null };
          const isPositive = (roi.abs || 0) >= 0;
          return (
            <div className={`glass-panel metric-card ${isPositive ? 'accent-gain-up' : 'accent-gain-down'}`}>
              <div className="metric-header">
                <span>{t('dash.unrealizedGain')}</span>
                <span className="metric-icon"><ArrowUpRight size={18} style={{ transform: isPositive ? 'none' : 'rotate(90deg)' }} /></span>
              </div>
              <div className="metric-value" style={{ color: isPositive ? '#22c55e' : '#ef4444' }}>
                {isPositive ? '+' : '−'}${money(Math.abs(roi.abs || 0))}
              </div>
              <div className="metric-footer">
                <span>{roi.pct === null ? t('dash.roiUnset') : t('dash.roiVsCost', { pct: `${isPositive ? '+' : ''}${roi.pct}` })}</span>
              </div>
            </div>
          );
        })()}

        {/* Total Cards count */}
        <div className="glass-panel metric-card accent-cards">
          <div className="metric-header">
            <span>{t('dash.totalCards')}</span>
            <span className="metric-icon"><Library size={18} /></span>
          </div>
          <div className="metric-value">{summary.totalCards}</div>
          <div className="metric-footer">
            <span>{t('dash.uniqueCount', { count: summary.uniqueCards })}</span>
          </div>
        </div>
      </div>

      <Suspense
        fallback={(
          <DashboardChunkFallback
            style={{ marginBottom: '1.5rem', minHeight: '288px', padding: '1.5rem 1.75rem' }}
          />
        )}
      >
        <DashboardCharts
          section="timeline"
          historyData={historyData}
          loadingHistory={loadingHistory}
          timePeriod={timePeriod}
        />
      </Suspense>

      {/* Main Charts & Analytics Details */}
      <div className="dashboard-details">
        {/* Left Column: Charts */}
        <Suspense
          fallback={<DashboardChunkFallback style={{ minHeight: '300px', padding: '1.5rem' }} />}
        >
          <DashboardCharts section="analytics" sets={sets} types={types} rarities={rarities} />
        </Suspense>

        {/* Right Column: Mini Tables & Lists */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
          
          {/* Top Valuable Cards */}
          <div className="glass-panel" style={{ flex: 1 }}>
            <h3 className="chart-title" style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
              <Trophy size={18} style={{ color: 'var(--accent-yellow)' }} />
              {t('dash.topValuable')}
            </h3>
            <div style={{ display: 'flex', flexDirection: 'column', gap: '1rem', marginTop: '1.25rem' }}>
              {topValuable.map((card, idx) => (
                <div 
                  key={idx} 
                  onClick={() => setInspectorCard(card)}
                  style={{ 
                    display: 'flex', 
                    gap: '0.75rem', 
                    alignItems: 'center', 
                    background: 'rgba(255, 255, 255, 0.02)', 
                    padding: '0.5rem', 
                    borderRadius: 'var(--radius-sm)', 
                    border: '1px solid var(--border-glass)',
                    cursor: 'pointer',
                    transition: 'all 0.2s ease'
                  }}
                  className="dashboard-card-clickable"
                >
                  <CardImage card={card} style={{ width: '56px', aspectRatio: 0.718, objectFit: 'cover', borderRadius: '5px', boxShadow: '0 2px 6px rgba(0,0,0,0.4)' }} />
                  <div style={{ flex: 1, overflow: 'hidden' }}>
                    <div style={{ fontWeight: 700, fontSize: '0.9rem', color: 'var(--text-strong)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
                      {getCardDisplayName(card.name, card.language, card.printed_name)}
                    </div>
                    <div style={{ fontSize: '0.75rem', color: 'var(--text-secondary)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis', display: 'flex', alignItems: 'center', gap: '0.35rem' }}>
                      <span>{card.set_name} • {card.rarity}</span>
                      {card.printing && card.printing !== 'Normal' && (
                        <span style={{ fontSize: '0.55rem', fontWeight: 800, padding: '1px 4px', borderRadius: '3px', flexShrink: 0, ...getPrintingBadgeStyle(card.printing) }}>
                          {getPrintingBadgeLabel(card.printing)}
                        </span>
                      )}
                    </div>
                  </div>
                  <div style={{ textAlign: 'right' }}>
                    <div style={{ fontWeight: 800, color: 'var(--accent-yellow)', fontSize: '0.95rem' }}>{priceText(card.price_trend, card.price_currency)}<span style={{ fontSize: '0.6rem', fontWeight: 500, color: 'var(--text-muted)' }}> {t('dash.each')}</span></div>
                    <div style={{ fontSize: '0.7rem', color: 'var(--text-muted)' }}>
                      {card.quantity > 1 ? t('dash.qtyTotal', { qty: card.quantity, price: formatPrice(card.price_trend * card.quantity) }) : t('dash.qty', { qty: 1 })}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          </div>

          {/* Recent Additions */}
          {recentAdditions.length > 0 && (
            <div className="glass-panel" style={{ flex: 1 }}>
              <h3 className="chart-title" style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
                <Plus size={18} style={{ color: 'var(--accent-blue)' }} />
                {t('dash.recentAdditions')}
              </h3>
              <div style={{ display: 'flex', flexDirection: 'column', gap: '0.6rem', marginTop: '1.25rem' }}>
                {recentAdditions.map((card, idx) => (
                  <div
                    key={idx}
                    onClick={() => setInspectorCard(card)}
                    style={{ display: 'flex', gap: '0.75rem', alignItems: 'center', background: 'rgba(255, 255, 255, 0.02)', padding: '0.5rem', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)', cursor: 'pointer', transition: 'all 0.2s ease' }}
                    className="dashboard-card-clickable"
                  >
                    <CardImage card={card} style={{ width: '48px', aspectRatio: 0.718, objectFit: 'cover', borderRadius: '5px', boxShadow: '0 2px 6px rgba(0,0,0,0.4)' }} />
                    <div style={{ flex: 1, overflow: 'hidden' }}>
                      <div style={{ fontWeight: 700, fontSize: '0.85rem', color: 'var(--text-strong)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
                        {getCardDisplayName(card.name, card.language, card.printed_name)}
                      </div>
                      <div style={{ fontSize: '0.7rem', color: 'var(--text-secondary)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis', display: 'flex', alignItems: 'center', gap: '0.35rem' }}>
                        <span>{card.set_name} • #{card.number}</span>
                        {card.printing && card.printing !== 'Normal' && (
                          <span style={{ fontSize: '0.55rem', fontWeight: 800, padding: '1px 4px', borderRadius: '3px', flexShrink: 0, ...getPrintingBadgeStyle(card.printing) }}>
                            {getPrintingBadgeLabel(card.printing)}
                          </span>
                        )}
                      </div>
                    </div>
                    <div style={{ textAlign: 'right' }}>
                      <div style={{ fontWeight: 700, color: 'var(--accent-yellow)', fontSize: '0.8rem' }}>{priceText(card.price_trend, card.price_currency)}<span style={{ fontSize: '0.55rem', fontWeight: 500, color: 'var(--text-muted)' }}> {t('dash.each')}</span></div>
                      <div style={{ fontSize: '0.65rem', color: 'var(--text-muted)' }}>{card.quantity > 1 ? t('dash.qty', { qty: card.quantity }) : (card.added_at ? new Date(card.added_at).toLocaleDateString(locale) : '')}</div>
                    </div>
                  </div>
                ))}
              </div>
            </div>
          )}

          {/* Set Completion progress tracker */}
          {setProgress.length > 0 && (
            <div className="glass-panel">
              <h3 className="chart-title">{t('dash.setProgress')}</h3>
              <div className="set-progress-grid" style={{ marginTop: '1rem' }}>
                {setProgress.map((set, idx) => (
                  <div key={idx} className="set-progress-item">
                    <div className="set-progress-header">
                      <span style={{ color: 'var(--text-strong)' }}>{set.setName}</span>
                      <span style={{ color: 'var(--text-secondary)' }}>{set.ownedUnique} / {set.totalCards} ({set.percent}%)</span>
                    </div>
                    <div className="set-progress-bar-bg">
                      <div className="set-progress-bar-fill" style={{ width: `${set.percent}%` }}></div>
                    </div>
                  </div>
                ))}
              </div>
            </div>
          )}

        </div>
      </div>

      {/* Card Inspector Modal Overlay */}
      {inspectorCard && (
        <Suspense fallback={<DashboardChunkFallback overlay style={{ minWidth: '12rem', padding: '1.5rem' }} />}>
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
