import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const srcDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const dashboard = readFileSync(join(srcDir, 'components', 'Dashboard.jsx'), 'utf8');

// The Bento dashboard draws its sparkline and donut as inline SVG, so it must
// never pull Recharts into the main chunk.
assert.doesNotMatch(dashboard, /from ['"]recharts['"]/, 'Dashboard must not import Recharts');
assert.ok(!existsSync(join(srcDir, 'components', 'DashboardCharts.jsx')), 'old DashboardCharts must stay removed');
assert.doesNotMatch(dashboard, /import\s+CardInspectorModal\s+from/, 'Dashboard must not statically import CardInspectorModal');
assert.match(dashboard, /lazy\(\(\)\s*=>\s*import\(['"]\.\/CardInspectorModal['"]\)\)/, 'CardInspectorModal must be lazy-loaded');
assert.match(dashboard, /inspectorCard\s*&&[\s\S]*?<Suspense[\s\S]*?<CardInspectorModal/, 'the inspector chunk must only render after a card is opened');

for (const useful of ['topValuable', 'recentAdditions', 'colorValues', 'rarities']) {
  assert.ok(dashboard.includes(useful), `${useful} must remain on the Dashboard`);
}
// Brent replaced Invested with a Cards tile.
assert.ok(!/totalSpent|totalInvested/.test(dashboard), 'Dashboard must not show Invested');
assert.ok(dashboard.includes('summary.totalCards'), 'Cards tile shows the collection total');

console.log('PASS: dashboardChunking.test.js');
