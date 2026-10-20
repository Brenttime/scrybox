import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  Cell,
  Legend,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';
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

// MTG-ish slice colours for the value-by-colour hero (readable on dark).
const VALUE_COLORS = {
  White: '#f3ecc8', Blue: '#4ea3e0', Black: '#8a7f86', Red: '#e8604c', Green: '#4fb06a',
  Multicolor: '#e8c25b', Colorless: '#a7adb8', Land: '#b07a4a',
};

function ColorValueHero({ colorValues = [], totalValue = 0 }) {
  const { t } = useT();
  const total = colorValues.reduce((a, c) => a + (Number(c.value) || 0), 0) || totalValue || 0;
  const data = colorValues.map((c) => ({
    ...c,
    label: c.name,
    fill: VALUE_COLORS[c.name] || '#6b7280',
  }));
  const money = (v) => `$${Number(v || 0).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
  return (
    <div className="glass-panel dash-color-value">
      <h3 className="chart-title">{t('dash.valueByColor')}</h3>
      {data.length === 0 ? (
        <div className="chart-empty">{t('dash.noTypeData')}</div>
      ) : (
        <div className="dash-cv-body">
          <div className="dash-cv-chart">
            <ResponsiveContainer width="100%" height="100%">
              <PieChart>
                <Pie data={data} dataKey="value" nameKey="label" cx="50%" cy="50%" innerRadius="58%" outerRadius="92%" paddingAngle={2} stroke="none" isAnimationActive={false}>
                  {data.map((d) => <Cell key={d.name} fill={d.fill} />)}
                </Pie>
                <Tooltip
                  contentStyle={{ backgroundColor: 'var(--bg-secondary)', borderColor: 'var(--border-glass)' }}
                  itemStyle={{ color: 'var(--text-strong)' }}
                  formatter={(v, n) => [`${money(v)} · ${total ? Math.round((v / total) * 100) : 0}%`, n]}
                />
              </PieChart>
            </ResponsiveContainer>
            <div className="dash-cv-center">
              <span className="dash-cv-total">{money(total)}</span>
              <span className="dash-cv-sub">{t('dash.totalValue')}</span>
            </div>
          </div>
          <ul className="dash-cv-legend">
            {data.map((d) => (
              <li key={d.name}>
                <span className="dash-cv-dot" style={{ background: d.fill }} />
                <span className="dash-cv-name">{d.label}</span>
                <span className="dash-cv-val">{money(d.value)}</span>
                <span className="dash-cv-pct">{total ? Math.round((d.value / total) * 100) : 0}%</span>
              </li>
            ))}
          </ul>
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
