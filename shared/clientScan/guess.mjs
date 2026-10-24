// Printing GUESS for a proven title whose printing the footer cannot split
// (Brent, 2026-10-24: overrides the old abstain/picker rule for this case).
// The title is proven; only the printing is guessed, the row is marked
// `guess`, and the user can change it from the tray at any time.
//
// Order (each step only narrows; an empty step is skipped):
//   1. footer evidence the cascade could not use alone: a strong collector
//      number (N/T, or a padded 3-4 digit run) or a clean set-code read
//      ('SOS·EN', 'SOA') that names some of the title's printings;
//   2. scan context: the set of the previous added cards (most recent first);
//   3. fallback: the regular original (no reprint sheet, no promo, no
//      variant treatment), newest first.
// Within the chosen set: regular before variant, then the lowest number.
// Index rows are [id, set, num, released yyyymmdd?, flags?] (flags: 1 =
// variant treatment, 2 = promo / reprint product); older indexes without the
// extra columns still guess, only without the newest-first tie-break.
import { normalizeCollector, strongNumbers } from './text.mjs';

const YEAR = /^(19|20)\d\d$/;
// Brent: the previous 3 added cards are the context; older adds of the same
// scan session only break ties those 3 cannot (a new set's first cards).
export const CONTEXT_PRIMARY = 3;
export const CONTEXT_MAX = 20;

// Numbers a footer read names with some confidence: N/T or seen in two
// strips (strongNumbers), or a 3-4 digit run that is not a year ('C 0106').
export function guessNumbers(raws) {
  const out = new Set([...strongNumbers(raws).keys()].map(normalizeCollector));
  for (const raw of raws) {
    for (const run of String(raw).match(/(?<!\d)\d{3,4}(?!\d)/g) || []) if (!YEAR.test(run)) out.add(normalizeCollector(run));
  }
  return out;
}

// Set codes read as a clean footer token: the whole read is the code, or the
// code followed by the language ('SOS·EN', 'soa', 'SOS EN'). A code found
// inside rules text ('more' -> MOR) is not evidence.
export function guessCodes(ix, raws) {
  const out = new Set();
  for (const raw of raws) {
    const c = String(raw).toLowerCase().replace(/[^a-z0-9]/g, '');
    const code = c.endsWith('en') && ix.setLookup.has(c.slice(0, -2)) ? c.slice(0, -2) : c;
    if (code.length >= 3 && code.length <= 6 && ix.setLookup.has(code)) out.add(code);
  }
  return out;
}

const isSheet = (ix, o) => !!ix.phys[o]?.sheet;
const flagsOf = (ix, o) => Number(ix.printings[o][4]) || 0;
const relOf = (ix, o) => Number(ix.printings[o][3]) || 0;
const numOf = (ix, o) => { const n = Number.parseInt(ix.phys[o]?.num ?? ix.printings[o][2], 10); return Number.isFinite(n) ? n : 1e9; };

// Regular printing first: no reprint sheet, no promo, no variant treatment,
// then newest, then the lowest collector number.
function rank(ix, pool) {
  return [...pool].sort((a, b) => (isSheet(ix, a) - isSheet(ix, b))
    || ((flagsOf(ix, a) & 2) - (flagsOf(ix, b) & 2))
    || ((flagsOf(ix, a) & 1) - (flagsOf(ix, b) & 1))
    || (relOf(ix, b) - relOf(ix, a))
    || (numOf(ix, a) - numOf(ix, b))
    || (a - b));
}

// cands: printing indices still possible for the proven title. sets: set
// codes of the previous added cards, most recent first. Returns
// {pi, guess: 'footer'|'context'|'fallback', sure} or null. `sure` = the
// pick did not need the fallback, or the fallback had exactly one regular
// original to choose from (a single first-frame guess is then safe to make).
export function guessPrinting(ix, cands, { footer = [], sets = [] } = {}) {
  let pool = [...new Set(cands)].filter(o => ix.printings[o]);
  if (!pool.length) return null;
  let basis = 'fallback';
  const nums = guessNumbers(footer);
  const byNum = pool.filter(o => nums.has(ix.phys[o].num));
  if (byNum.length && byNum.length < pool.length) { pool = byNum; basis = 'footer'; }
  const codes = guessCodes(ix, footer);
  const byCode = pool.filter(o => codes.has(ix.printings[o][1]) || codes.has(ix.phys[o].set));
  if (byCode.length && byCode.length < pool.length) { pool = byCode; basis = 'footer'; }
  // Context: the last CONTEXT_PRIMARY adds first (most frequent set, ties
  // to the most recent), then the rest of the scan session the same way.
  const byFreq = (list) => {
    const seen = new Map();
    list.filter(Boolean).forEach((s, i) => { const k = String(s).toLowerCase(); const e = seen.get(k) || { n: 0, first: i }; e.n++; seen.set(k, e); });
    return [...seen.entries()].sort((a, b) => b[1].n - a[1].n || a[1].first - b[1].first).map(([k]) => k);
  };
  const order = [...new Set([...byFreq(sets.slice(0, CONTEXT_PRIMARY)), ...byFreq(sets.slice(CONTEXT_PRIMARY))])];
  for (const s of order) {
    const inSet = pool.filter(o => ix.printings[o][1] === s);
    if (inSet.length) { if (inSet.length < pool.length || basis === 'fallback') basis = 'context'; pool = inSet; break; }
  }
  const ranked = rank(ix, pool);
  const regular = pool.filter(o => !isSheet(ix, o) && !(flagsOf(ix, o) & 3));
  // Several printings of one set (sos 168 / sos 297 showcase) still have a
  // single regular one; a fallback over several sets is a weaker guess.
  const sure = basis !== 'fallback' || regular.length === 1 || (regular.length === 0 && pool.filter(o => !isSheet(ix, o)).length === 1);
  return { pi: ranked[0], guess: basis === 'fallback' ? 'fallback' : basis, sure };
}
