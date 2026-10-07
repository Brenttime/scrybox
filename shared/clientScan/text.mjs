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
  const setLookup = new Set(raw.sets);
  // Physical footer key per printing (see physicalKey). Data-driven: any
  // reprint-sheet set numbering its cards '<ORIGINAL SET>-<num>' collides
  // with that original automatically, with no per-set code.
  const phys = raw.printings.map(p => physicalKey(p[1], p[2], setLookup));
  for (const [i, p] of raw.printings.entries()) {
    const k = phys[i];
    if (k.set !== p[1] && /^\d+$/.test(k.num)) setMax.set(k.set, Math.max(setMax.get(k.set) || 0, Number(k.num)));
  }
  return {
    names, nameIx, canonOf, setMax, phys,
    excluded: new Set(raw.excluded),
    sets: raw.sets,
    setRank: new Map(raw.sets.map((c, i) => [c, i])),
    setLookup,
    setLengths: [...new Set(raw.sets.map(c => c.length))].sort((a, b) => b - a),
    printings: raw.printings,          // [id, set, num]
    byTitle: raw.byTitle,              // normalized title -> printing indices
    uniqueAlias: raw.uniqueAlias,      // canonical norm -> [[alias, printing]]
  };
}

// The footer a printing physically carries. Reprint-sheet sets (The List,
// Mystery Booster 2: Scryfall 'plst', numbers like 'ODY-129') print the
// ORIGINAL card's footer ('129/350'), told apart only by a small symbol in
// the bottom-left corner. So a number '<SET>-<N>' whose prefix is another
// known set code is physically set SET, number N. Everything else is its own
// set and number. Two printings of a title with the same key are a collision
// group: footer OCR can never tell them apart.
export function physicalKey(set, num, setLookup) {
  const m = /^([a-z0-9]{2,6})-0*(\d+[a-z]?)[^a-z0-9]*$/i.exec(String(num ?? ''));
  if (m && m[1].toLowerCase() !== set && setLookup.has(m[1].toLowerCase())) {
    const n = normalizeCollector(m[2]);
    return { set: m[1].toLowerCase(), num: n, raw: n, sheet: true };
  }
  // raw: the number as the resolver always compared it (lowercase, as
  // indexed), so ordinary printings match exactly as before.
  return { set, num: normalizeCollector(num), raw: String(num ?? '').toLowerCase(), sheet: false };
}
const physKeyStr = (k) => `${k.set}:${k.num}`;
// Index objects built by hand (tests) may lack .phys: derive it lazily.
function physOf(ix) {
  if (!ix.phys) {
    const look = ix.setLookup || new Set(ix.sets || []);
    ix.phys = ix.printings.map(p => physicalKey(p[1], p[2], look));
  }
  return ix.phys;
}

// Every printing of `title` whose physical footer equals printing pi's
// (pi included, pool order). Length 1 = the footer alone proves pi.
export function physicalTwins(ix, title, pi) {
  const pool = ix.byTitle[title] || [];
  const ph = physOf(ix);
  const k = physKeyStr(ph[pi]);
  const twins = pool.filter(o => o === pi || physKeyStr(ph[o]) === k);
  return twins.includes(pi) ? twins : [pi, ...twins];
}

// server.find_card_by_ocr (the rapidfuzz branch). Returns the CANONICAL title,
// normalized, or null.
// Bounded memo (R2-#11): a still card is re-read every frame and the same
// noisy title string scans all ~35k names for each query variant again. The
// lookup is a pure function of (index, normalized text, threshold), so the
// memo lives on the index object itself (a new index = a new memo) and only
// caches TEXT -> title lookups, never a printing identity across frames.
const FIND_MEMO_MAX = 512;
export function findCardByOcr(ix, text, threshold = NAME_MATCH_MIN) {
  if (!text) return { name: null, score: 0 };
  const memo = ix.__findMemo || (ix.__findMemo = new Map());
  const key = `${threshold}\u0000${normName(text)}`;
  const hit = memo.get(key);
  if (hit) { memo.delete(key); memo.set(key, hit); return { ...hit }; }
  const res = findCardByOcrUncached(ix, text, threshold);
  memo.set(key, res);
  if (memo.size > FIND_MEMO_MAX) memo.delete(memo.keys().next().value);
  return { ...res };
}
export function findCardByOcrUncached(ix, text, threshold = NAME_MATCH_MIN) {
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
// 2003-2010 frames end the line with 'N/T' ('... Coast LLC 50/249'): the
// collector number is N, never the set total T.
export function retroNumber(raw) {
  const t = String(raw).trim();
  const nt = /(?<![\d/])(\d{1,4}[a-z]?)\s*\/\s*(\d{2,4})\s*$/i.exec(t);
  if (nt) return Number.parseInt(nt[1], 10) <= Number(nt[2]) * 2 + 50 ? nt[1].toLowerCase() : null;
  const m = /(\d{1,4}[a-z]?)\s*$/.exec(t);
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

// server._resolve_footer_candidates, on PHYSICAL footers: the evidence must
// name exactly one physical footer (set as printed + number). Returns every
// printing of the title carrying that footer (a collision group when more
// than one: e.g. ODY 129 and The List 'ODY-129'), or null.
// `setless` = numbers allowed to resolve without a set code, as an iterable of
// numbers or a Map(number -> printed set total) from strongNumbers (default:
// every number, the sidecar's behaviour).
export function resolveFooterGroup(ix, title, codes, numbers, setless = numbers) {
  const totals = setless instanceof Map ? setless : new Map([...setless].map(n => [n, 0]));
  const pool = ix.byTitle[title] || [];
  const ph = physOf(ix);
  const exact = new Set();
  for (const code of codes) {
    for (const number of numbers) {
      for (const pi of pool) {
        if (ph[pi].set === code && ph[pi].raw === number) exact.add(pi);
      }
    }
  }
  const group = (hits) => {
    const keys = new Set([...hits].map(pi => physKeyStr(ph[pi])));
    return keys.size === 1 ? physicalTwins(ix, title, [...hits][0]) : null;
  };
  if (exact.size) return group(exact);
  // Contradictory exact set+number evidence is ambiguity, not permission to
  // fall through to the weaker set-less pass and pick one of them.
  const global = new Set();
  for (const [number, total] of totals) {
    for (const pi of pool) {
      if (normalizeCollector(ph[pi].raw) === number && (!total || (ix.setMax.get(ph[pi].set) || 0) >= total)) global.add(pi);
    }
  }
  return global.size ? group(global) : null;
}

// One printing, or null (a collision group is NOT one printing).
export function resolveFooter(ix, title, codes, numbers, setless = numbers) {
  const g = resolveFooterGroup(ix, title, codes, numbers, setless);
  return g && g.length === 1 ? g[0] : null;
}

// Multi-frame vote, constrained to the title's own printings. Consecutive
// frames of one still card garble the collector line differently ("0807303",
// "080305", "080/505" for 080/303), so no token repeats verbatim, but the
// printed, zero-padded number does. A printing wins only if its padded number
// (3+ chars, as modern footers print it) appears in >= 2 distinct frames and
// no other candidate printing's number appears in ANY frame. Set codes read in
// the frames narrow the candidates first.
export function voteFooterGroup(ix, title, frames) {
  let pool = ix.byTitle[title] || [];
  if (pool.length < 2 || frames.length < 2) return null;
  const ph = physOf(ix);
  const codes = footerCodes(ix, frames.flat());
  if (codes.length) {
    const inSet = pool.filter(pi => codes.includes(ph[pi].set));
    if (inSet.length) pool = inSet;
  }
  // Drop the printed set total of every "N/T" read before flattening: once
  // separators are stripped, "080/303" becomes "080303" and would let card
  // #303 (or any number hiding in the total) vote for itself.
  // Kept as separate tokens: concatenating reads ("030" + "030") would mint
  // numbers no strip ever printed ("030030" contains "300").
  const texts = frames.map(raws => raws.flatMap(raw => raw.toLowerCase()
    .replace(/(\d{1,4}[a-z]?)\s*\/\s*\d{2,4}/g, '$1 ')
    .split(/\s+/).map(tok => tok.replace(/[^a-z0-9]/g, '')).filter(Boolean)));
  const key = (pi) => {
    const n = ph[pi].raw;
    return /^\d+$/.test(n) ? n.padStart(3, '0') : n;
  };
  // One vote per PHYSICAL footer: twins (same set as printed + number) are
  // one candidate, never two.
  const hits = new Map();
  for (const pi of pool) {
    const k = key(pi);
    if (k.length < 3) continue;
    // "NNN/303": never let the printed set total vote for card #303.
    if (Number(k) === (ix.setMax.get(ph[pi].set) || -1)) continue;
    const n = texts.filter(toks => toks.some(tok => tok.includes(k))).length;
    if (n) hits.set(physKeyStr(ph[pi]), [pi, n]);
  }
  if (hits.size !== 1) return null;
  const [[, [pi, n]]] = [...hits];
  // The number must be unique among the candidates, too (two printings can
  // share a number across sets when no set code was read).
  const k = key(pi);
  if (pool.some(o => key(o) === k && physKeyStr(ph[o]) !== physKeyStr(ph[pi]))) return null;
  return n >= 2 ? physicalTwins(ix, title, pi) : null;
}

export function voteFooter(ix, title, frames) {
  const g = voteFooterGroup(ix, title, frames);
  return g && g.length === 1 ? g[0] : null;
}
