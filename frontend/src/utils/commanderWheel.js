// Commander Wheel: pure helpers (set families, query, pool, wheel geometry).
// A "master set" is a top-level node of /api/sets?tree=1 plus every child
// (Final Fantasy = fin + fic + fca + pfin ...). Scryfall's `e:` matches any
// printing in the set, so reprints (Mystery Booster Commander, Through the
// Ages) are part of the pool by construction.

export const WHEEL_SLICES = 24;

// Flatten a tree node into its set codes (self + all descendants).
export function familyCodes(node) {
  const out = [];
  const walk = (n) => {
    if (!n) return;
    // Roots carry `set_code`, children `code`; token/memorabilia children
    // can never hold a commander.
    const code = n.set_code || n.code;
    if (code && !/^(token|memorabilia)$/.test(n.type || '')) out.push(String(code).toLowerCase());
    (n.children || []).forEach(walk);
  };
  walk(node);
  return [...new Set(out)];
}

// Top-level families, newest first. Families made only of token sets/art
// series can never hold a commander, so they are dropped.
export function masterSets(tree) {
  if (!Array.isArray(tree)) return [];
  return tree
    .map(n => ({
      code: String(n.set_code || '').toLowerCase(),
      name: n.name || n.set_code || '',
      released: n.release_date || '',
      symbol: n.symbol_url || '',
      codes: familyCodes(n),
      childNames: (n.children || []).filter(c => !/^(token|memorabilia)$/.test(c.type || '')).map(c => c.name).filter(Boolean),
    }))
    .filter(f => f.code && f.codes.length && !/\b(tokens?|art series|minigames?)\b/i.test(f.name))
    .sort((a, b) => (b.released || '').localeCompare(a.released || '') || a.name.localeCompare(b.name));
}

export function filterMasterSets(families, text) {
  const q = String(text || '').trim().toLowerCase();
  if (!q) return families;
  return families.filter(f => f.name.toLowerCase().includes(q)
    || f.codes.some(c => c === q)
    || f.childNames.some(n => n.toLowerCase().includes(q)));
}

// Scryfall query for "anything that can lead a deck, printed in these sets".
// `listDates`: release dates of the chosen families. The List (plst) cards a
// product ships with (e.g. Mystery Booster Commander's reprint slot) are
// filed under plst with that product's release date, not under its code.
export function commanderQuery(codes, listDates = []) {
  const list = [...new Set((codes || []).map(c => String(c).toLowerCase()).filter(c => /^[a-z0-9]+$/.test(c)))];
  const dates = [...new Set((listDates || []).filter(d => /^\d{4}-\d{2}-\d{2}$/.test(String(d))))];
  const terms = [...list.map(c => `e:${c}`), ...dates.map(d => `(e:plst date=${d})`)];
  if (!list.length) return '';
  const sets = terms.length === 1 ? terms[0] : `(${terms.join(' or ')})`;
  return `is:commander -is:funny ${sets} unique:prints`;
}

// One entry per card name. Prefer a printing from the chosen sets, then the
// newest one, so the reveal shows the art the user rolled from.
export function dedupeCommanders(rows, codes) {
  const inSet = new Set((codes || []).map(c => String(c).toLowerCase()));
  const best = new Map();
  for (const r of rows || []) {
    if (!r || !r.name) continue;
    const prev = best.get(r.name);
    if (!prev) { best.set(r.name, r); continue; }
    const score = x => [inSet.has(String(x.set_id || '').toLowerCase()) ? 1 : 0, x.image_url ? 1 : 0, x.released_at || ''];
    const [a1, a2, a3] = score(r);
    const [b1, b2, b3] = score(prev);
    if (a1 > b1 || (a1 === b1 && (a2 > b2 || (a2 === b2 && a3 > b3)))) best.set(r.name, r);
  }
  return [...best.values()].sort((a, b) => a.name.localeCompare(b.name));
}

// Uniform pick over the whole pool (not just the visible slices).
export function pickWinner(pool, rand = Math.random) {
  if (!pool || !pool.length) return -1;
  return Math.min(pool.length - 1, Math.floor(rand() * pool.length));
}

// The wheel shows at most WHEEL_SLICES names: the winner plus random others,
// shuffled so the winner's slice moves around between spins.
export function buildSlices(pool, winnerIdx, rand = Math.random, max = WHEEL_SLICES) {
  if (winnerIdx < 0 || !pool || !pool.length) return { slices: [], winnerSlice: -1 };
  const others = pool.map((_, i) => i).filter(i => i !== winnerIdx);
  for (let i = others.length - 1; i > 0; i--) {
    const j = Math.floor(rand() * (i + 1));
    [others[i], others[j]] = [others[j], others[i]];
  }
  const picked = [winnerIdx, ...others.slice(0, Math.max(0, max - 1))];
  for (let i = picked.length - 1; i > 0; i--) {
    const j = Math.floor(rand() * (i + 1));
    [picked[i], picked[j]] = [picked[j], picked[i]];
  }
  return { slices: picked.map(i => pool[i]), winnerSlice: picked.indexOf(winnerIdx) };
}

// Final rotation (deg, clockwise) that lands slice `idx` under the top
// pointer. Slice i spans [i*step, (i+1)*step) measured clockwise from 12
// o'clock; always spins forward at least `turns` full turns from `current`.
export function landingRotation(current, idx, n, turns = 6, jitter = 0.5) {
  if (n <= 0 || idx < 0) return current;
  const step = 360 / n;
  const center = idx * step + step / 2 + (jitter - 0.5) * step * 0.7;
  const target = (360 - center) % 360;
  const base = current - (current % 360) + turns * 360;
  return base + target;
}

const IDENTITY_FILL = { W: '#e9dfb8', U: '#3a7bc8', B: '#5b5560', R: '#c8453a', G: '#3c8c55' };
export function sliceColor(identity, i) {
  const ids = String(identity || '').toUpperCase().replace(/[^WUBRG]/g, '');
  if (ids.length === 1) return IDENTITY_FILL[ids];
  if (ids.length > 1) return '#c9a43a';
  return i % 2 ? '#8a8f98' : '#a3a8b0';
}

export function deckNameFor(card) {
  const name = String(card?.name || '').split(' // ')[0].trim();
  return name ? `${name} (Wheel)` : 'Commander Wheel deck';
}
