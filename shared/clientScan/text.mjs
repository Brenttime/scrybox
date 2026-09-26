// Text side of the in-browser card reader: CTC decoding, name matching and
// exact-printing resolution. A line-by-line port of the cardscan sidecar
// (server.py: norm_name, find_card_by_ocr, identify_unique_title_printing,
// identify_unique_ocr_printing, _footer_numbers, _footer_codes,
// _resolve_footer_candidates, the retro trailing-number rule). The rule that
// matters most is carried over unchanged: a result is ONE proven printing or
// nothing — never the most likely printing of a name.

export const NAME_MATCH_MIN = 0.90;

// server.norm_name: NFKD fold, lowercase, non-alphanumerics to single spaces.
export function normName(s) {
  return String(s ?? '').toLowerCase().replace(/\u2019/g, "'")
    .normalize('NFKD').replace(/[\u0300-\u036f]/g, '')
    .replace(/[^a-z0-9]+/g, ' ').trim();
}

export function normalizeCollector(value) {
  const v = String(value ?? '').trim().toLowerCase();
  const m = /^0*(\d+)([a-z]*)$/.exec(v);
  if (!m) return v;
  return (m[1].replace(/^0+/, '') || '0') + m[2];
}

// RapidOCR CTCLabelDecode: argmax per step, drop repeats and blanks (0),
// confidence = mean of the kept steps' max probabilities.
export function ctcDecode(data, steps, classes, chars, batchIndex = 0) {
  let text = '', sum = 0, n = 0, prev = -1;
  const base = batchIndex * steps * classes;
  for (let t = 0; t < steps; t++) {
    const o = base + t * classes;
    let best = 0, bv = data[o];
    for (let k = 1; k < classes; k++) { const v = data[o + k]; if (v > bv) { bv = v; best = k; } }
    if (best !== prev && best !== 0) { text += chars[best]; sum += bv; n++; }
    prev = best;
  }
  return { text, conf: n ? sum / n : 0 };
}

// The model's dictionary as RapidOCR builds it: 'blank' first, ' ' last.
export function buildCharset(dictText) {
  const lines = dictText.split('\n');
  if (lines.length && lines[lines.length - 1] === '') lines.pop();
  return ['', ...lines, ' '];
}

// --- fuzzy name matching ---------------------------------------------------
// rapidfuzz fuzz.ratio = normalized Indel similarity = 2*LCS/(|a|+|b|).
// LCS by the bit-parallel Hyyro/Allison-Dix recurrence, multi-word so long
// titles are exact too. Pattern masks are built once per query.
function patternMasks(q) {
  const words = Math.ceil(q.length / 30) || 1;
  const masks = new Map();
  for (let i = 0; i < q.length; i++) {
    let m = masks.get(q.charCodeAt(i));
    if (!m) { m = new Int32Array(words); masks.set(q.charCodeAt(i), m); }
    m[(i / 30) | 0] |= 1 << (i % 30);
  }
  return { masks, words, len: q.length, zero: new Int32Array(words) };
}

const FULL = (1 << 30) - 1;
function lcs(pm, s, V) {
  const { masks, words, len, zero } = pm;
  for (let k = 0; k < words; k++) V[k] = FULL;
  for (let j = 0; j < s.length; j++) {
    const M = masks.get(s.charCodeAt(j)) || zero;
    let carry = 0, borrow = 0;
    for (let k = 0; k < words; k++) {
      const v = V[k], u = v & M[k];
      let sum = v + u + carry; carry = sum >>> 30; sum &= FULL;
      let diff = v - u - borrow; borrow = diff < 0 ? 1 : 0; diff = (diff + (borrow << 30)) & FULL;
      V[k] = sum | diff;
    }
  }
  let zeros = 0;
  for (let i = 0; i < len; i++) if (!((V[(i / 30) | 0] >>> (i % 30)) & 1)) zeros++;
  return zeros;
}

export function ratio(a, b) {
  if (!a.length && !b.length) return 1;
  const pm = patternMasks(a);
  return (2 * lcs(pm, b, new Int32Array(pm.words))) / (a.length + b.length);
}

// rapidfuzz.process.extract(query, choices, ratio, score_cutoff, limit=2):
// the two best choices at or above the cutoff (0..1 here), ties by index.
export function extractTop2(query, choices, cutoff) {
  const pm = patternMasks(query);
  const V = new Int32Array(pm.words);
  const la = query.length;
  let b1 = -1, s1 = -1, b2 = -1, s2 = -1;
  for (let i = 0; i < choices.length; i++) {
    const c = choices[i];
    const lb = c.length;
    const bound = (2 * Math.min(la, lb)) / (la + lb);
    if (bound < cutoff || bound < s2) continue;
    const r = (2 * lcs(pm, c, V)) / (la + lb);
    if (r < cutoff) continue;
    if (r > s1) { b2 = b1; s2 = s1; b1 = i; s1 = r; } else if (r > s2) { b2 = i; s2 = r; }
  }
  const out = [];
  if (b1 >= 0) out.push([b1, s1]);
  if (b2 >= 0) out.push([b2, s2]);
  return out;
}

// --- the index ---------------------------------------------------------------
// Built by cardscan tools/build_client_index.py from the sidecar's own loaders.
export function loadIndex(raw) {
  const names = raw.names;
  const nameIx = new Map(names.map((n, i) => [n, i]));
  const canonOf = (i) => raw.canon[i] ?? names[i];
  // Highest plain collector number per set (for the "N/T" total check).
  const setMax = new Map();
  for (const p of raw.printings) {
    if (/^\d+$/.test(p[2])) setMax.set(p[1], Math.max(setMax.get(p[1]) || 0, Number(p[2])));
  }
  return {
    names, nameIx, canonOf, setMax,
    excluded: new Set(raw.excluded),
    sets: raw.sets,
    setRank: new Map(raw.sets.map((c, i) => [c, i])),
    setLookup: new Set(raw.sets),
    setLengths: [...new Set(raw.sets.map(c => c.length))].sort((a, b) => b - a),
    printings: raw.printings,          // [id, set, num]
    byTitle: raw.byTitle,              // normalized title -> printing indices
    uniqueAlias: raw.uniqueAlias,      // canonical norm -> [[alias, printing]]
  };
}

// server.find_card_by_ocr (the rapidfuzz branch). Returns the CANONICAL title,
// normalized, or null.
export function findCardByOcr(ix, text, threshold = NAME_MATCH_MIN) {
  if (!text) return { name: null, score: 0 };
  const q = normName(text);
  if (!q || ix.excluded.has(q)) return { name: null, score: 0 };
  const exact = ix.nameIx.get(q);
  if (exact != null) return { name: ix.canonOf(exact), score: 1 };
  const queries = [q];
  const parts = q.split(' ');
  for (const [drop, maxNoise] of [[1, 4], [2, 7]]) {
    if (parts.length > drop) {
      const dropped = parts.slice(0, drop).join('');
      const tail = parts.slice(drop).join(' ');
      if (dropped.length <= maxNoise && tail.length >= 4 && !queries.includes(tail)) queries.push(tail);
    }
  }
  let best = -1, bestR = 0, bestMargin = 0;
  queries.forEach((query, qi) => {
    const found = extractTop2(query, ix.names, 0.60);
    if (!found.length) return;
    const penalty = qi === 0 ? 1 : 0.99;
    const r = found[0][1] * penalty;
    const runner = found.length > 1 ? found[1][1] * penalty : 0;
    if (r > bestR) { bestR = r; best = found[0][0]; bestMargin = r - runner; }
  });
  const accepted = bestR >= threshold || (q.length >= 12 && bestR >= 0.86 && bestMargin >= 0.12);
  if (best >= 0 && accepted) return { name: ix.canonOf(best), score: bestR };
  return { name: null, score: bestR };
}

export function uniqueTitlePrinting(ix, title) {
  const m = ix.byTitle[title];
  return m && m.length === 1 ? m[0] : null;
}

export function uniqueOcrPrinting(ix, text, title) {
  const aliases = ix.uniqueAlias[title];
  if (!text || !aliases?.length) return null;
  const query = normName(text);
  const found = extractTop2(query, aliases.map(a => a[0]), 0.86);
  if (!found.length) return null;
  const [bi, score] = found[0];
  const runner = found.length > 1 ? found[1][1] : 0;
  if (score < 0.90 && (query.length < 12 || (score - runner) * 100 < 8)) return null;
  return aliases[bi][1];
}

export function footerNumbers(raws) {
  const numbers = [];
  const add = (n) => { if (n && !numbers.includes(n)) numbers.push(n); };
  for (const raw of raws) {
    for (const run of raw.match(/\d{5,}/g) || []) add(normalizeCollector(run.slice(-4)));
    const lower = raw.toLowerCase();
    let tokens = [...lower.matchAll(/(?<!\d)(\d{1,4}[a-z]?)(?:\s*\/\s*\d{1,4})?/g)].map(m => m[1]);
    if (!tokens.length) {
      const compact = lower.replace(/[^a-z0-9]/g, '');
      tokens = [...compact.matchAll(/(\d{1,4}[a-z]?)/g)].map(m => m[1]);
    }
    for (const token of tokens) {
      for (const cand of [token, token.replace(/[^0-9]/g, '')]) add(normalizeCollector(cand));
    }
  }
  return numbers;
}

export function footerCodes(ix, raws) {
  const codes = [];
  for (const raw of raws) {
    const compact = raw.toLowerCase().replace(/[^a-z0-9]/g, '');
    let best = null;
    for (let pos = 0; pos < Math.min(3, compact.length); pos++) {
      for (const len of ix.setLengths) {
        const cand = compact.slice(pos, pos + len);
        if (cand.length === len && ix.setLookup.has(cand)
          && (best == null || ix.setRank.get(cand) < ix.setRank.get(best))) best = cand;
      }
    }
    if (best && !codes.includes(best)) codes.push(best);
  }
  return codes;
}

// The retro copyright line: only a trailing number, never a (c) year.
export function retroNumber(raw) {
  const m = /(\d{1,4}[a-z]?)\s*$/.exec(String(raw).trim());
  if (!m || /^(19|20)\d\d$/.test(m[1])) return null;
  return m[1];
}

// Collector numbers strong enough to name a printing WITHOUT a set code.
// Stricter than the sidecar on purpose: its global title+number rule accepts
// any digit run, and on the saved phone frames the client's noisier reads
// turned "00895093" into Damn #89 (DRC) and "7/5" into Grief #7 (H2R) — both
// real printings, both wrong. A set-less number must either be printed the
// way a modern footer prints it ("080/303") or be read in two separate strips.
//
// Returns Map(number -> printed set total or 0). A misread digit in "080/303"
// still gives a real printing elsewhere (PP-OCRv5 read "089/303" -> Damn #89,
// DRC), so a "N/T" read also carries T: the resolved set must actually run to
// #T (DRC stops at 184; MH2 goes past 303).
export function strongNumbers(raws) {
  const seen = new Map();
  const strong = new Map();
  raws.forEach((raw, i) => {
    for (const m of raw.toLowerCase().matchAll(/(?<![\d/])(\d{1,4})\s*\/\s*(\d{2,4})(?![\d/])/g)) {
      const n = normalizeCollector(m[1]);
      if (Number(m[1]) <= Number(m[2]) * 2 + 50 && !strong.has(n)) strong.set(n, Number(m[2]));
    }
    for (const n of footerNumbers([raw])) {
      if (!seen.has(n)) seen.set(n, new Set());
      seen.get(n).add(i);
    }
  });
  // Copyright years repeat across strips too; never let one through here.
  for (const [n, strips] of seen) {
    if (strips.size >= 2 && !/^(19|20)\d\d$/.test(n) && !strong.has(n)) strong.set(n, 0);
  }
  return strong;
}

// The retro copyright line has to look like one before its trailing number
// counts ("TM & (c) 2021 Wizards of the Coast 408"), so rules/flavor text that
// happens to end in a digit is not read as a collector number.
export function looksLikeCopyright(raw) {
  return /wizard|coast|\b(19|20)\d\d\b|©|tm\s*&/i.test(String(raw));
}

// server._resolve_footer_candidates: one printing only when the evidence has
// exactly one valid identity; otherwise null.
// `setless` = numbers allowed to resolve without a set code, as an iterable of
// numbers or a Map(number -> printed set total) from strongNumbers (default:
// every number, the sidecar's behaviour).
export function resolveFooter(ix, title, codes, numbers, setless = numbers) {
  const totals = setless instanceof Map ? setless : new Map([...setless].map(n => [n, 0]));
  const pool = ix.byTitle[title] || [];
  const exact = new Set();
  for (const code of codes) {
    for (const number of numbers) {
      for (const pi of pool) {
        const p = ix.printings[pi];
        if (p[1] === code && String(p[2]).toLowerCase() === number) exact.add(pi);
      }
    }
  }
  if (exact.size === 1) return [...exact][0];
  // Contradictory exact set+number evidence is ambiguity, not permission to
  // fall through to the weaker set-less pass and pick one of them.
  if (exact.size > 1) return null;
  const global = new Set();
  for (const [number, total] of totals) {
    for (const pi of pool) {
      const p = ix.printings[pi];
      if (normalizeCollector(p[2]) === number && (!total || (ix.setMax.get(p[1]) || 0) >= total)) global.add(pi);
    }
  }
  return global.size === 1 ? [...global][0] : null;
}

// Multi-frame vote, constrained to the title's own printings. Consecutive
// frames of one still card garble the collector line differently ("0807303",
// "080305", "080/505" for 080/303), so no token repeats verbatim, but the
// printed, zero-padded number does. A printing wins only if its padded number
// (3+ chars, as modern footers print it) appears in >= 2 distinct frames and
// no other candidate printing's number appears in ANY frame. Set codes read in
// the frames narrow the candidates first.
export function voteFooter(ix, title, frames) {
  let pool = ix.byTitle[title] || [];
  if (pool.length < 2 || frames.length < 2) return null;
  const codes = footerCodes(ix, frames.flat());
  if (codes.length) {
    const inSet = pool.filter(pi => codes.includes(ix.printings[pi][1]));
    if (inSet.length) pool = inSet;
  }
  // Drop the printed set total of every "N/T" read before flattening: once
  // separators are stripped, "080/303" becomes "080303" and would let card
  // #303 (or any number hiding in the total) vote for itself.
  const texts = frames.map(raws => raws.join(' ').toLowerCase()
    .replace(/(\d{1,4})\s*\/\s*\d{2,4}/g, '$1 ')
    .replace(/[^a-z0-9]/g, ''));
  const key = (pi) => {
    const n = String(ix.printings[pi][2]).toLowerCase();
    return /^\d+$/.test(n) ? n.padStart(3, '0') : n;
  };
  const hits = new Map();
  for (const pi of pool) {
    const k = key(pi);
    if (k.length < 3) continue;
    // "NNN/303": never let the printed set total vote for card #303.
    if (Number(k) === (ix.setMax.get(ix.printings[pi][1]) || -1)) continue;
    const n = texts.filter(tx => tx.includes(k)).length;
    if (n) hits.set(pi, n);
  }
  if (hits.size !== 1) return null;
  const [[pi, n]] = [...hits];
  // The number must be unique among the candidates, too (two printings can
  // share a number across sets when no set code was read).
  const k = key(pi);
  if (pool.some(o => o !== pi && key(o) === k)) return null;
  return n >= 2 ? pi : null;
}
