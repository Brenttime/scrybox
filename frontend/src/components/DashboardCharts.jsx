import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  Cell,
  Legend,
  Pie,
  PieChart,
  Sector,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';
import { useState } from 'react';
import { useT } from '../utils/i18n';

const COLORS = [
  '#3b82f6', '#10b981', '#f59e0b', '#ef4444', '#8b5cf6',
  '#ec4899', '#14b8a6', '#f43f5e', '#a855f7', '#6366f1',
];

const TYPE_COLORS = {
  White: '#fef08a',
  Blue: '#3b82f6',
  Black: '#334155',
  Red: '#ef4444',
  Green: '#10b981',
  Land: '#d97706',
};

function TimelineChart({ historyData, loadingHistory, timePeriod }) {
  const { t } = useT();

  return (
    <div className="glass-panel" style={{ marginBottom: '1.5rem', padding: '1.5rem 1.75rem' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '1rem' }}>
        <h3 className="chart-title" style={{ margin: 0 }}>{t('dash.timelineTitle')}</h3>
        <span style={{ fontSize: '0.75rem', color: 'var(--text-secondary)' }}>
          {t('dash.timelineRange', { range: timePeriod.toUpperCase() })}
        </span>
      </div>
      <div className="chart-container" style={{ height: '240px', position: 'relative' }}>
        {loadingHistory ? (
          <div className="spinner" style={{ position: 'absolute', top: '45%', left: '45%' }}></div>
        ) : historyData.length < 2 ? (
          <div className="chart-empty">{t('dash.notEnoughHistory')}</div>
        ) : (
          <ResponsiveContainer width="100%" height="100%">
            <AreaChart data={historyData} margin={{ top: 10, right: 10, left: -10, bottom: 0 }}>
              <defs>
                <linearGradient id="colorVal" x1="0" y1="0" x2="0" y2="1">
                  <stop offset="5%" stopColor="var(--success)" stopOpacity={0.4}/>
                  <stop offset="95%" stopColor="var(--success)" stopOpacity={0.0}/>
                </linearGradient>
              </defs>
              <XAxis dataKey="date" stroke="var(--text-secondary)" style={{ fontSize: '0.7rem' }} />
              <YAxis stroke="var(--text-secondary)" style={{ fontSize: '0.7rem' }} tickFormatter={(v) => `$${v}`} />
              <Tooltip
                contentStyle={{ backgroundColor: 'var(--bg-secondary)', borderColor: 'var(--border-glass)' }}
                labelStyle={{ color: 'var(--text-primary)' }}
                formatter={(v) => [`$${v}`, t('dash.portfolioValue')]}
              />
              <Area type="monotone" dataKey="value" stroke="var(--success)" strokeWidth={2} fillOpacity={1} fill="url(#colorVal)" />
            </AreaChart>
          </ResponsiveContainer>
        )}
      </div>
    </div>
  );
}

function AnalyticsCharts({ sets, types, rarities }) {
  const { t } = useT();
  const typeColorLookup = Object.fromEntries(
    Object.entries(TYPE_COLORS).map(([key, value]) => [key.toLowerCase(), value]),
  );
  const typeChartData = types.map((type, index) => {
    const fill = typeColorLookup[String(type.name).toLowerCase()] || COLORS[index % COLORS.length];
    return { name: type.name, value: type.value, color: fill, fill };
  });
  const rarityChartData = rarities.map((rarity, index) => ({
    ...rarity,
    fill: COLORS[index % COLORS.length],
  }));

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
      <div className="glass-panel">
        <h3 className="chart-title">{t('dash.valueBySet')}</h3>
        <div className="chart-container">
          {sets.length === 0 ? (
            <div className="chart-empty">{t('dash.noSetData')}</div>
          ) : (
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={sets} layout="vertical" margin={{ left: 10, right: 30, top: 10, bottom: 10 }}>
                <XAxis type="number" stroke="var(--text-secondary)" tickFormatter={(v) => `$${v}`} />
                <YAxis dataKey="name" type="category" width={120} stroke="var(--text-secondary)" tickLine={false} axisLine={false} style={{ fontSize: '0.8rem' }} />
                <Tooltip
                  contentStyle={{ backgroundColor: 'var(--bg-secondary)', borderColor: 'var(--border-glass)' }}
                  labelStyle={{ color: 'var(--text-primary)' }}
                  formatter={(v) => [`$${v}`, t('dash.value')]}
                />
                <Bar dataKey="value" fill="var(--accent-red)" radius={[0, 4, 4, 0]} />
              </BarChart>
            </ResponsiveContainer>
          )}
        </div>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: '1fr', gap: '1.5rem' }}>
        <div className="glass-panel">
          <h3 className="chart-title">{t('dash.colorDistribution')}</h3>
          <div className="chart-container" style={{ height: '220px' }}>
            {typeChartData.length === 0 ? (
              <div className="chart-empty">{t('dash.noTypeData')}</div>
            ) : (
              <ResponsiveContainer width="100%" height="100%">
                <PieChart>
                  <Pie
                    data={typeChartData}
                    cx="50%"
                    cy="50%"
                    innerRadius={50}
                    outerRadius={80}
                    paddingAngle={3}
                    dataKey="value"
                  >
                    {typeChartData.map((entry, index) => (
                      <Cell key={`cell-${index}`} fill={entry.color} />
                    ))}
                  </Pie>
                  <Tooltip
                    contentStyle={{ backgroundColor: 'var(--bg-secondary)', borderColor: 'var(--border-glass)' }}
                    itemStyle={{ color: 'var(--text-strong)' }}
                    labelStyle={{ color: 'var(--text-strong)' }}
                    formatter={(v) => [v, t('dash.cards')]}
                  />
                  <Legend
                    verticalAlign="bottom"
                    height={36}
                    iconSize={10}
                    style={{ fontSize: '0.75rem' }}
                    formatter={(value) => <span style={{ color: 'var(--text-secondary)' }}>{value}</span>}
                  />
                </PieChart>
              </ResponsiveContainer>
            )}
          </div>
        </div>

        <div className="glass-panel">
          <h3 className="chart-title">{t('dash.rarityDistribution')}</h3>
          <div className="chart-container" style={{ height: '220px' }}>
            {rarityChartData.length === 0 ? (
              <div className="chart-empty">{t('dash.noRarityData')}</div>
            ) : (
              <ResponsiveContainer width="100%" height="100%">
                <PieChart>
                  <Pie
                    data={rarityChartData}
                    cx="50%"
                    cy="50%"
                    innerRadius={45}
                    outerRadius={78}
                    paddingAngle={3}
                    dataKey="value"
                  >
                    {rarityChartData.map((entry, index) => (
                      <Cell key={`cell-${index}`} fill={entry.fill} />
                    ))}
                  </Pie>
                  <Tooltip
                    contentStyle={{ backgroundColor: 'var(--bg-secondary)', borderColor: 'var(--border-glass)' }}
                    itemStyle={{ color: 'var(--text-strong)' }}
                    labelStyle={{ color: 'var(--text-strong)' }}
                    formatter={(v) => [v, t('dash.cards')]}
                  />
                  <Legend
                    verticalAlign="bottom"
                    height={36}
                    iconSize={10}
                    style={{ fontSize: '0.75rem' }}
                    formatter={(value) => <span style={{ color: 'var(--text-secondary)', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{value}</span>}
                  />
                </PieChart>
              </ResponsiveContainer>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

// Minimal colour donut: tap a slice to pop it out and fade the rest; the
// centre then shows that slice's value.
const VALUE_COLORS = {
  White: { fill: '#F4E7B5' },
  Blue: { fill: '#3A8FE3' },
  Black: { fill: '#7A5C99' },
  Red: { fill: '#E5533D' },
  Green: { fill: '#2EAA67' },
  Multicolor: { fill: '#E0A82E' },
  Colorless: { fill: '#9BA6B4' },
  Land: { fill: '#8C6447' },
};

function ActiveSlice(props) {
  const { cx, cy, innerRadius, outerRadius, startAngle, endAngle, fill } = props;
  return <Sector cx={cx} cy={cy} innerRadius={innerRadius - 2} outerRadius={outerRadius + 10} startAngle={startAngle} endAngle={endAngle} fill={fill} />;
}

function ColorValueHero({ colorValues = [], totalValue = 0 }) {
  const { t } = useT();
  const [active, setActive] = useState(null);
  const total = colorValues.reduce((a, c) => a + (Number(c.value) || 0), 0) || totalValue || 0;
  const data = colorValues.map((c) => ({ ...c, fill: (VALUE_COLORS[c.name] || {}).fill || '#6b7280' }));
  const money = (v) => `$${Number(v || 0).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
  const sel = active !== null ? data[active] : null;
  const pct = (v) => (total ? Math.round((v / total) * 100) : 0);
  return (
    <div className="glass-panel dash-color-value">
      <h3 className="chart-title">{t('dash.valueByColor')}</h3>
      {data.length === 0 ? (
        <div className="chart-empty">{t('dash.noTypeData')}</div>
      ) : (
        <div className="dash-cv-chart" onClick={() => setActive(null)}>
          <ResponsiveContainer width="100%" height="100%">
            <PieChart>
              <Pie
                data={data} dataKey="value" nameKey="name" cx="50%" cy="50%"
                innerRadius="56%" outerRadius="86%" paddingAngle={1.5} stroke="none"
                isAnimationActive={false}
                activeIndex={active ?? undefined} activeShape={ActiveSlice}
                onClick={(_, i, e) => { e?.stopPropagation?.(); setActive(a => (a === i ? null : i)); }}
                style={{ cursor: 'pointer', outline: 'none' }}
              >
                {data.map((d, i) => (
                  <Cell key={d.name} fill={d.fill} fillOpacity={active === null || active === i ? 1 : 0.22} style={{ transition: 'fill-opacity .2s', outline: 'none' }} />
                ))}
              </Pie>
            </PieChart>
          </ResponsiveContainer>
          <div className="dash-cv-center">
            {sel ? (
              <>
                <span className="dash-cv-name">{sel.name}</span>
                <span className="dash-cv-total">{money(sel.value)}</span>
                <span className="dash-cv-sub">{t('dash.colorSliceSub', { pct: pct(sel.value), count: Number(sel.count || 0).toLocaleString() })}</span>
              </>
            ) : (
              <>
                <span className="dash-cv-total">{money(total)}</span>
                <span className="dash-cv-sub">{t('dash.tapSlice')}</span>
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

function DashboardCharts({ section, ...props }) {
  if (section === 'colorValue') {
    return <ColorValueHero {...props} />;
  }
  if (section === 'timeline') {
    return <TimelineChart {...props} />;
  }
  return <AnalyticsCharts {...props} />;
}

export default DashboardCharts;
