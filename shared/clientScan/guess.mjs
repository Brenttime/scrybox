// Printing GUESS for a proven title whose printing the footer cannot split
// (Brent, 2026-10-24: overrides the old abstain/picker rule for this case).
// The title is proven; only the printing is guessed, the row is marked
// `guess`, and the user can change it from the tray at any time.
//
// Order (each step only narrows; an empty step is skipped):
//   1. footer evidence the cascade could not use alone: a strong collector
//      number (N/T, or a padded 3-4 digit run) or a clean set-code read
//      ('SOS·EN', 'SOA') that names some of the title's printings;
//   2. scan context: the sets of the previous 3 added cards;
//   3. fallback: the regular original (no reprint sheet, no promo, no
//      variant treatment), newest first.
// Within the chosen set: regular before variant, then the lowest number.
// Index rows are [id, set, num, released yyyymmdd?, flags?] (flags: 1 =
// variant treatment, 2 = promo / reprint product, 4 = old card frame,
// 8 = single finish);
// older indexes without the extra columns still guess, only without the
// newest-first tie-break and with fewer first-look guesses.
import { normalizeCollector, strongNumbers } from './text.mjs';

const YEAR = /^(19|20)\d\d$/;
// Brent: the sets of the previous 3 added cards are the context.
export const CONTEXT_PRIMARY = 3;

// Numbers a footer read names with some confidence, with the set total an
// 'N/T' read carries (0 = none): strongNumbers (N/T, or seen in two strips),
// never a year, plus a zero-padded modern 4-digit number in one strip
// ('C 0106', 'U 0058'; a year never starts with 0). A number only counts for
// a printing whose set runs to that total (089/303 is not DRC 89: DRC stops
// at 184), as in the footer resolver.
export function guessNumbers(raws) {
  const out = new Map();
  for (const [n, total] of strongNumbers(raws)) {
    if (!YEAR.test(n)) out.set(normalizeCollector(n), total || 0);
  }
  for (const raw of raws) {
    // Not after a '/': a padded set total ('050/0249') is never a number.
    for (const run of String(raw).replace(/\s*\/\s*/g, '/').match(/(?<![\d/])0\d{3}(?![\d/])/g) || []) {
      const n = normalizeCollector(run);
      if (!out.has(n)) out.set(n, 0);
    }
  }
  return out;
}
const numberFits = (ix, o, nums) => {
  if (!nums.has(ix.phys[o].num)) return false;
  const total = nums.get(ix.phys[o].num);
  return !total || (ix.setMax.get(ix.phys[o].set) || 0) >= total;
};

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

// How far from the set's main printing (lower = more regular): reprint
// sheet, promo / product set, variant treatment, single finish.
const tierOf = (ix, o) => (isSheet(ix, o) ? 8 : 0) + (flagsOf(ix, o) & 2 ? 4 : 0) + (flagsOf(ix, o) & 1 ? 2 : 0) + (flagsOf(ix, o) & 8 ? 1 : 0);

// Regular printing first (tierOf), then newest, then the lowest number.
function rank(ix, pool) {
  return [...pool].sort((a, b) => (tierOf(ix, a) - tierOf(ix, b))
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
  const byNum = pool.filter(o => numberFits(ix, o, nums));
  if (byNum.length && byNum.length < pool.length) { pool = byNum; basis = 'footer'; }
  const codes = guessCodes(ix, footer);
  const byCode = pool.filter(o => codes.has(ix.printings[o][1]) || codes.has(ix.phys[o].set));
  if (byCode.length && byCode.length < pool.length) { pool = byCode; basis = 'footer'; }
  // Context: the sets of the previous CONTEXT_PRIMARY added cards (Brent),
  // most frequent first, ties to the most recent.
  const seen = new Map();
  sets.slice(0, CONTEXT_PRIMARY).filter(Boolean).forEach((s, i) => { const k = String(s).toLowerCase(); const e = seen.get(k) || { n: 0, first: i }; e.n++; seen.set(k, e); });
  const order = [...seen.entries()].sort((a, b) => b[1].n - a[1].n || a[1].first - b[1].first).map(([k]) => k);
  for (const s of order) {
    const inSet = pool.filter(o => ix.printings[o][1] === s);
    if (inSet.length) { if (inSet.length < pool.length || basis === 'fallback') basis = 'context'; pool = inSet; break; }
  }
  const ranked = rank(ix, pool);
  // `sure` (a guess safe to make on the first look, before the deeper footer
  // stages): the pick is no sheet or promo printing, it is strictly the most
  // regular printing of its set (tierOf: soa 37, the two-finish main slot,
  // over the single-finish 102 / 167; fra 213 over showcase 298), and no
  // printing of that set uses another frame era (mh2 135 / retro 408: the
  // number is on the copyright line, only the deeper stages read it; the
  // 2026-10-24 replay lost 17 such proofs to a first-look guess). A fallback
  // pick must also be the single most regular candidate overall. The next
  // frame still runs the full cascade once (pipeline verify), so a deeper
  // footer proof corrects a wrong first-look guess.
  const pick = ranked[0];
  const all = [...new Set(cands)].filter(o => ix.printings[o]);
  const t = tierOf(ix, pick);
  const sameSet = all.filter(o => ix.printings[o][1] === ix.printings[pick][1]);
  const sure = t < 4
    && sameSet.every(o => o === pick || tierOf(ix, o) > t)
    && sameSet.every(o => (flagsOf(ix, o) & 4) === (flagsOf(ix, pick) & 4))
    && (basis !== 'fallback' || all.every(o => o === pick || tierOf(ix, o) > t));
  return { pi: pick, guess: basis, sure };
}
