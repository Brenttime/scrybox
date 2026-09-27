// Scryfall search syntax for Scrybox: one parser, three consumers.
//
//   1. The collection screen filters its loaded rows in the browser (compileQuery).
//   2. The backend answers what it can from SQLite (utils/rawQuerySql.js walks the AST).
//   3. Everything else goes to Scryfall itself (toScryfall(ast) re-serializes the query).
//
// The parser accepts the FULL Scryfall grammar: every comparator (: = != < <= > >=),
// quoted values, /regex/ values, `!exact` names, `-` negation, `and` / `or`,
// parentheses, and every documented keyword (o:, kw:, pow:, f:, usd:, a:, atag:,
// otag:, is:, not:, unique:, order:, ...). A keyword it does not know is a
// QuerySyntaxError, so typos are NAMED instead of silently matching nothing.
//
// Routing (analyze().mode):
//   local    every term can be answered from stored card rows (name, type, color,
//            identity, rarity, set, collector number, language, mana value, and
//            display directives). Instant, no API call.
//   catalog  at least one term needs Scryfall's card database (oracle text,
//            tagger tags, formats, prices, artists, regexes, ...). The caller
//            sends toScryfall(ast) upstream; oracle tags can also be answered
//            from the local Oracle Tags index (backend/src/oracleTags.js).
//
// Semantics follow Scryfall's live API (checked 2026-09-25):
//   c:wu == c>=wu (colors include), id:g == id<=g (identity fits), c:m multicolor,
//   c=2 color count, guild/shard/wedge/college names, r>rare (special, mythic,
//   bonus), r:s special, r:b bonus, m: is MANA COST (not mana value), mv/cmc/
//   manavalue take comparators and even/odd, t=x == t:x, name=x == name:x.
// Scrybox extension: is:<card type> (is:land, is:creature, is:basic-land) reads the
// type line. Scryfall rejects those, so toScryfall() rewrites them to t:.
//
// CJS on purpose: the server can require it, Vite bundles it for the browser, and
// both test suites import it.
const LANG_TABLE = require('./languages.json');

class QuerySyntaxError extends Error {
  constructor(message) {
    super(message);
    this.name = 'QuerySyntaxError';
  }
}

const LANG_INDEX = new Map();
for (const l of LANG_TABLE) {
  LANG_INDEX.set(l.name.toLowerCase(), l.name);
  LANG_INDEX.set(l.code.toLowerCase(), l.name);
  if (l.scryfall && l.scryfall !== l.code) LANG_INDEX.set(l.scryfall, l.name);
}

// ---------------------------------------------------------------- operators

// Typed keyword -> canonical keyword. Local keywords are canonicalized so the JS
// and SQL evaluators see one spelling; the typed spelling is kept on the leaf
// (`raw`) and re-emitted verbatim when the query goes upstream.
const ALIASES = new Map(Object.entries({
  t: 'type',
  c: 'color', colour: 'color',
  id: 'identity', ci: 'identity', commander: 'identity', cmd: 'identity',
  r: 'rarity',
  s: 'set', e: 'set', edition: 'set',
  cn: 'number',
  language: 'lang',
  cmc: 'mv', manavalue: 'mv',
  oracletag: 'otag', function: 'otag',
  art: 'atag', arttag: 'atag',
  m: 'mana',
  o: 'oracle', fo: 'fulloracle', kw: 'keyword',
  pow: 'power', tou: 'toughness', pt: 'powtou', loy: 'loyalty', def: 'defense',
  b: 'block', st: 'settype',
  f: 'format', legal: 'format',
  a: 'artist', ft: 'flavor', wm: 'watermark',
  paperprints: 'papersets',
}));

// Answerable from a stored card row.
const LOCAL_OPERATORS = new Set([
  'name', 'is', 'type', 'color', 'identity', 'rarity', 'set', 'number', 'lang', 'mv',
]);

// Display / result-shape directives. They never filter a stored row.
const DIRECTIVES = new Set(['include', 'unique', 'order', 'sort', 'direction', 'prefer', 'display']);

// Everything else Scryfall understands. These route to the catalog.
const CATALOG_OPERATORS = new Set([
  'otag', 'atag', 'mana', 'devotion', 'produces', 'oracle', 'fulloracle', 'keyword',
  'power', 'toughness', 'powtou', 'loyalty', 'defense', 'has', 'isflag', 'not',
  'in', 'block', 'settype', 'cube', 'format', 'banned', 'restricted',
  'usd', 'eur', 'tix', 'artist', 'artists', 'flavor', 'watermark', 'border', 'frame',
  'stamp', 'game', 'year', 'date', 'new', 'illustrations', 'prints', 'sets', 'papersets',
  'lore', 'oracleid', 'illustrationid', 'layout', 'regex',
]);

// Terms whose meaning depends on the exact PRINTING (art, frame, price, set...).
// A collection intersect for these must match printings, not game-card names.
const PRINTING_OPERATORS = new Set([
  'set', 'number', 'lang', 'atag', 'artist', 'artists', 'flavor', 'watermark', 'border',
  'frame', 'stamp', 'game', 'year', 'date', 'new', 'usd', 'eur', 'tix', 'isflag', 'not',
  'illustrationid',
]);

// Card types and supertypes: the values `is:` answers locally (Scrybox extension).
const TYPE_WORDS = new Set([
  'land', 'creature', 'artifact', 'enchantment', 'instant', 'sorcery', 'planeswalker',
  'battle', 'legendary', 'basic', 'snow', 'tribal', 'kindred', 'world', 'ongoing',
  'basic-land', 'conspiracy', 'plane', 'phenomenon', 'scheme', 'vanguard', 'dungeon',
]);

const COMPARATORS = [':', '!=', '<=', '>=', '=', '<', '>'];

// ---------------------------------------------------------------- colors

const COLOR_ORDER = ['w', 'u', 'b', 'r', 'g'];
const COLOR_BIT = { w: 1, u: 2, b: 4, r: 8, g: 16 };
const COLOR_WORDS = {
  white: 'w', blue: 'u', black: 'b', red: 'r', green: 'g',
  azorius: 'wu', dimir: 'ub', rakdos: 'br', gruul: 'rg', selesnya: 'gw',
  orzhov: 'wb', izzet: 'ur', golgari: 'bg', boros: 'rw', simic: 'gu',
  bant: 'gwu', esper: 'wub', grixis: 'ubr', jund: 'brg', naya: 'rgw',
  abzan: 'wbg', jeskai: 'urw', sultai: 'bgu', mardu: 'rwb', temur: 'gur',
  quandrix: 'gu', silverquill: 'wb', witherbloom: 'bg', lorehold: 'rw', prismari: 'ur',
  chaos: 'ubrg', aggression: 'wbrg', altruism: 'wurg', growth: 'wubg', artifice: 'wubr',
};
// Stored rows carry display names ('Red'); map them back to letters.
const STORED_COLOR = { white: 'w', blue: 'u', black: 'b', red: 'r', green: 'g' };

// Parse a color operand into { kind: 'mask', mask } | { kind: 'count', n } |
// { kind: 'multi' }. `c` / colorless is mask 0.
function parseColorValue(value) {
  const v = String(value).trim().toLowerCase();
  if (/^\d+$/.test(v)) return { kind: 'count', n: parseInt(v, 10) };
  if (v === 'm' || v === 'multicolor' || v === 'multicolored') return { kind: 'multi' };
  if (v === 'c' || v === 'colorless') return { kind: 'mask', mask: 0 };
  const letters = COLOR_WORDS[v] || v;
  if (!/^[wubrg]+$/.test(letters)) throw new QuerySyntaxError(`Unknown color "${value}"`);
  let mask = 0;
  for (const ch of letters) mask |= COLOR_BIT[ch];
  return { kind: 'mask', mask };
}

function storedColorMask(list) {
  let mask = 0;
  for (const name of list || []) {
    const letter = STORED_COLOR[String(name).toLowerCase()] || (COLOR_BIT[String(name).toLowerCase()] ? String(name).toLowerCase() : null);
    if (letter) mask |= COLOR_BIT[letter];
  }
  return mask;
}

function popcount(mask) {
  let n = 0;
  for (let m = mask; m; m >>= 1) n += m & 1;
  return n;
}

function compareNumbers(a, cmp, b) {
  switch (cmp) {
    case ':': case '=': return a === b;
    case '!=': return a !== b;
    case '<': return a < b;
    case '<=': return a <= b;
    case '>': return a > b;
    case '>=': return a >= b;
    default: return false;
  }
}

// `defaultCmp` is what `:` means for this operator: '>=' for color, '<=' for identity.
function colorTest(cardMask, parsed, cmp, defaultCmp) {
  if (parsed.kind === 'multi') {
    const multi = popcount(cardMask) >= 2;
    return cmp === '!=' ? !multi : multi;
  }
  if (parsed.kind === 'count') return compareNumbers(popcount(cardMask), cmp === ':' ? '=' : cmp, parsed.n);
  const q = parsed.mask;
  // c:c / id:c both mean "no colors".
  const op = cmp === ':' ? (q === 0 ? '=' : defaultCmp) : cmp;
  const superset = (cardMask & q) === q;
  const subset = (cardMask | q) === q;
  switch (op) {
    case '=': return cardMask === q;
    case '!=': return cardMask !== q;
    case '>=': return superset;
    case '>': return superset && cardMask !== q;
    case '<=': return subset;
    case '<': return subset && cardMask !== q;
    default: return false;
  }
}

// ---------------------------------------------------------------- rarity

const RARITY_ORDER = ['common', 'uncommon', 'rare', 'special', 'mythic', 'bonus'];
const RARITY_ALIASES = { c: 'common', u: 'uncommon', r: 'rare', s: 'special', m: 'mythic', b: 'bonus' };

function resolveRarityTarget(value) {
  const v = String(value).trim().toLowerCase();
  return RARITY_ALIASES[v] || v;
}

function rarityIndex(word) {
  return RARITY_ORDER.indexOf(word);
}

// Stored rarities are display words, sometimes two ('mythic rare'). The first word
// that names a known rarity is the rank.
function storedRarityIndex(stored) {
  const words = String(stored || '').toLowerCase().replace(/_/g, ' ').split(/\s+/);
  for (const w of words) {
    const i = rarityIndex(w);
    if (i >= 0) return i;
  }
  return -1;
}

// ---------------------------------------------------------------- languages

function resolveLanguageTarget(value) {
  const v = String(value).trim().toLowerCase();
  return LANG_INDEX.get(v) || null;
}

// Kept for older importers.
function resolveColorTarget(value) {
  const v = String(value).trim().toLowerCase();
  if (v === 'c') return 'colorless';
  const names = { w: 'white', u: 'blue', b: 'black', r: 'red', g: 'green' };
  return names[v] || v;
}

// ---------------------------------------------------------------- tokenizer

function readQuoted(s, i) {
  // s[i] is the opening quote.
  let j = i + 1;
  let val = '';
  while (j < s.length && s[j] !== '"') {
    if (s[j] === '\\' && s[j + 1] === '"') { val += '"'; j += 2; continue; }
    val += s[j];
    j += 1;
  }
  if (j >= s.length) throw new QuerySyntaxError('Unterminated quoted phrase — close it with "');
  return { value: val, end: j + 1 };
}

function readRegex(s, i) {
  // s[i] is the opening slash.
  let j = i + 1;
  let val = '';
  while (j < s.length && s[j] !== '/') {
    if (s[j] === '\\' && j + 1 < s.length) { val += s[j] + s[j + 1]; j += 2; continue; }
    val += s[j];
    j += 1;
  }
  if (j >= s.length) throw new QuerySyntaxError('Unterminated regular expression — close it with /');
  return { value: val, end: j + 1 };
}

function readBare(s, i) {
  let j = i;
  while (j < s.length && !/\s/.test(s[j]) && s[j] !== '(' && s[j] !== ')') j += 1;
  return { value: s.slice(i, j), end: j };
}

function tokenize(query) {
  const tokens = [];
  const s = String(query);
  let i = 0;
  while (i < s.length) {
    const ch = s[i];
    if (/\s/.test(ch)) { i += 1; continue; }
    if (ch === '(') { tokens.push({ type: 'open' }); i += 1; continue; }
    if (ch === ')') { tokens.push({ type: 'close' }); i += 1; continue; }

    let neg = false;
    if (ch === '-') {
      if (s[i + 1] === '(') { tokens.push({ type: 'open', neg: true }); i += 2; continue; }
      neg = true;
      i += 1;
      if (i >= s.length || /\s/.test(s[i]) || s[i] === ')') throw new QuerySyntaxError('"-" must negate a term');
    }

    // Exact name: !fire or !"fire // ice"
    if (s[i] === '!' && s[i + 1] !== '=') {
      i += 1;
      const r = s[i] === '"' ? readQuoted(s, i) : readBare(s, i);
      if (!r.value) throw new QuerySyntaxError('"!" needs a card name');
      tokens.push({ type: 'term', neg, op: null, value: r.value, exact: true, quoted: s[i] === '"' });
      i = r.end;
      continue;
    }

    if (s[i] === '"') {
      const r = readQuoted(s, i);
      tokens.push({ type: 'term', neg, op: null, value: r.value, quoted: true });
      i = r.end;
      continue;
    }

    // keyword + comparator?
    const m = /^([a-z_]+)(!=|<=|>=|:|=|<|>)/i.exec(s.slice(i));
    if (m) {
      const op = m[1].toLowerCase();
      const cmp = m[2];
      i += m[0].length;
      let value;
      let quoted = false;
      let regex = false;
      if (s[i] === '"') {
        const r = readQuoted(s, i);
        value = r.value; quoted = true; i = r.end;
      } else if (s[i] === '/') {
        const r = readRegex(s, i);
        value = r.value; regex = true; i = r.end;
      } else {
        const r = readBare(s, i);
        value = r.value; i = r.end;
      }
      if (!value && !quoted) throw new QuerySyntaxError(`Operator "${op}${cmp}" needs a value`);
      tokens.push({ type: 'term', neg, op, cmp, value, quoted, regex });
      continue;
    }

    // Bare regex name: /^goblin/
    if (s[i] === '/') {
      const r = readRegex(s, i);
      tokens.push({ type: 'term', neg, op: 'name', cmp: ':', value: r.value, regex: true });
      i = r.end;
      continue;
    }

    const r = readBare(s, i);
    i = r.end;
    const word = r.value;
    if (/^(or|and)$/i.test(word)) {
      if (neg) throw new QuerySyntaxError(`Cannot negate "${word.toLowerCase()}"`);
      tokens.push({ type: word.toLowerCase() });
      continue;
    }
    tokens.push({ type: 'term', neg, op: null, value: word });
  }
  return tokens;
}

// ---------------------------------------------------------------- parser

function makeLeaf(t) {
  if (!t.op) {
    if (!t.exact && !t.quoted && /^colorless$/i.test(t.value)) {
      return { kind: 'op', op: 'color', raw: 'c', cmp: ':', value: 'c' };
    }
    return { kind: 'name', value: t.value.toLowerCase(), exact: !!t.exact, quoted: !!t.quoted };
  }
  const raw = t.op;
  let op = ALIASES.get(raw) || raw;
  const leaf = { kind: 'op', op, raw, cmp: t.cmp, value: t.value, quoted: !!t.quoted, regex: !!t.regex };

  if (op === 'is' || op === 'not') {
    const v = String(t.value).toLowerCase();
    if (TYPE_WORDS.has(v)) {
      const isLeaf = { ...leaf, op: 'is', raw: 'is', value: v };
      return op === 'not' ? { op: 'not', term: isLeaf } : isLeaf;
    }
    leaf.op = op === 'is' ? 'isflag' : 'not';
    return leaf;
  }
  if (!LOCAL_OPERATORS.has(op) && !DIRECTIVES.has(op) && !CATALOG_OPERATORS.has(op)) {
    // Scryfall adds keywords over time; it is the authority on its own grammar.
    // An unrecognized keyword routes upstream, and Scryfall's 400 names a typo.
    leaf.catalog = true;
    return leaf;
  }
  if (leaf.regex) {
    // SQLite and the collection filter do not run regexes; Scryfall does.
    leaf.catalog = true;
    return leaf;
  }
  // Validate the local operators up front so a typo is named, not silently empty.
  if (op === 'color' || op === 'identity') leaf.color = parseColorValue(t.value);
  if (op === 'mv') {
    const v = String(t.value).toLowerCase();
    if (!/^(even|odd)$/.test(v) && !Number.isFinite(Number(v))) {
      throw new QuerySyntaxError(`Mana value must be a number, even or odd: "${t.value}"`);
    }
  }
  if (['name', 'type', 'set', 'lang', 'is'].includes(op) && !['!=', ':', '='].includes(t.cmp)) {
    throw new QuerySyntaxError(`"${raw}" does not support "${t.cmp}"`);
  }
  return leaf;
}

function parseTokens(tokens) {
  let pos = 0;
  const peek = () => tokens[pos];
  const next = () => tokens[pos++];

  function parseOr() {
    let left = parseAnd();
    while (peek() && peek().type === 'or') {
      next();
      const right = parseAnd();
      left = left.op === 'or' ? { op: 'or', terms: [...left.terms, right] } : { op: 'or', terms: [left, right] };
    }
    return left;
  }

  function parseAnd() {
    const terms = [];
    for (;;) {
      const t = peek();
      if (!t) break;
      if (t.type === 'or' || t.type === 'close') break;
      next();
      if (t.type === 'and') {
        if (!terms.length || !peek() || peek().type === 'or' || peek().type === 'close') {
          throw new QuerySyntaxError('"and" needs a term on both sides');
        }
        continue;
      }
      terms.push(parseTerm(t));
    }
    if (!terms.length) throw new QuerySyntaxError('Empty query or empty parenthesis group');
    return terms.length === 1 ? terms[0] : { op: 'and', terms };
  }

  function parseTerm(t) {
    if (t.type === 'open') {
      const inner = parseOr();
      const close = peek();
      if (!close || close.type !== 'close') throw new QuerySyntaxError('Unbalanced parenthesis');
      next();
      return t.neg ? { op: 'not', term: inner } : inner;
    }
    const leaf = makeLeaf(t);
    return t.neg ? { op: 'not', term: leaf } : leaf;
  }

  const ast = parseOr();
  if (pos < tokens.length) throw new QuerySyntaxError('Unbalanced parenthesis');
  return ast;
}

function parse(query) {
  const trimmed = String(query || '').trim();
  if (!trimmed) throw new QuerySyntaxError('Empty query');
  return parseTokens(tokenize(trimmed));
}

// ---------------------------------------------------------------- JS evaluator

// The type line as stored: every word of it lives in `subtypes` (supertype is
// always 'MTG'). Kept identical to rawQuerySql.subtypesLine().
function typeLineOf(card) {
  return (card.subtypes || []).map(x => String(x).toLowerCase()).join(' ');
}

function nameMatches(card, value, exact) {
  const names = [card.name, card.printed_name].filter(Boolean).map(n => String(n).toLowerCase());
  if (!exact) return names.some(n => n.includes(value));
  return names.some(n => n === value || n.split(' // ').includes(value));
}

function numberCompare(stored, cmp, v) {
  const s = String(stored != null ? stored : '').trim().toLowerCase();
  const val = String(v).trim().toLowerCase();
  const sn = parseInt(s, 10);
  const vn = parseInt(val, 10);
  if (cmp === ':' || cmp === '=' || cmp === '!=') {
    let eq = s === val;
    if (!eq && /^\d+$/.test(s) && /^\d+$/.test(val)) eq = sn === vn;
    return cmp === '!=' ? !eq : eq;
  }
  if (!Number.isFinite(sn) || !Number.isFinite(vn)) return false;
  return compareNumbers(sn, cmp, vn);
}

function evalLeaf(leaf, card) {
  if (leaf.kind === 'name') return nameMatches(card, leaf.value, leaf.exact);
  const v = String(leaf.value).trim().toLowerCase();
  const negate = leaf.cmp === '!=';
  switch (leaf.op) {
    case 'name': {
      const hit = nameMatches(card, v, false);
      return negate ? !hit : hit;
    }
    case 'is': {
      const val = v.replace(/-/g, ' ');
      const line = typeLineOf(card);
      const words = line.split(' ');
      const hit = val.includes(' ') ? line.includes(val) : words.includes(val);
      return negate ? !hit : hit;
    }
    case 'type': {
      const hit = typeLineOf(card).includes(v);
      return negate ? !hit : hit;
    }
    case 'color':
      return colorTest(storedColorMask(card.types), leaf.color || parseColorValue(leaf.value), leaf.cmp, '>=');
    case 'identity':
      return colorTest(storedColorMask(card.color_identity), leaf.color || parseColorValue(leaf.value), leaf.cmp, '<=');
    case 'rarity': {
      const target = resolveRarityTarget(leaf.value);
      const ti = rarityIndex(target);
      const si = storedRarityIndex(card.rarity);
      if (ti < 0) {
        // Unknown word: word-level match on the stored rarity (e.g. a two-word display rarity).
        const hit = String(card.rarity || '').toLowerCase().replace(/_/g, ' ').split(' ').includes(target);
        return negate ? !hit : hit;
      }
      if (si < 0) return negate;
      return compareNumbers(si, leaf.cmp === ':' ? '=' : leaf.cmp, ti);
    }
    case 'set': {
      const hit = String(card.set_id || '').toLowerCase() === v;
      return negate ? !hit : hit;
    }
    case 'number':
      return numberCompare(card.number, leaf.cmp, v);
    case 'lang': {
      if (v === 'any') return !negate;
      const target = resolveLanguageTarget(leaf.value);
      const hit = !!target && String(card.language || '').toLowerCase() === target.toLowerCase();
      return negate ? !hit : hit;
    }
    case 'mv': {
      if (card.cmc == null) return false;
      const cmc = Number(card.cmc);
      if (v === 'even' || v === 'odd') {
        const hit = Number.isInteger(cmc) && (cmc % 2 === 0) === (v === 'even');
        return negate ? !hit : hit;
      }
      return compareNumbers(cmc, leaf.cmp === ':' ? '=' : leaf.cmp, Number(v));
    }
    default:
      if (DIRECTIVES.has(leaf.op)) return true;
      throw new QuerySyntaxError(`"${leaf.raw}:" needs Scryfall's card database`);
  }
}

function evalNode(node, card) {
  if (node.op === 'or') return node.terms.some(t => evalNode(t, card));
  if (node.op === 'and') return node.terms.every(t => evalNode(t, card));
  if (node.op === 'not' && node.term) return !evalNode(node.term, card);
  return evalLeaf(node, card);
}

// ---------------------------------------------------------------- analysis

function walkLeaves(ast, fn) {
  (function walk(n) {
    if (n.op === 'not' && n.term) return walk(n.term);
    if (n.op === 'and' || n.op === 'or') { n.terms.forEach(walk); return; }
    fn(n);
  })(ast);
}

// Every canonical operator in the tree, in order of first appearance. Bare words
// and quoted phrases are `name`; regex terms add `regex` so they route to the catalog.
function collectOperators(ast) {
  const ops = new Set();
  walkLeaves(ast, (n) => {
    if (n.kind === 'name') ops.add('name');
    else {
      ops.add(n.op);
      if (n.catalog) ops.add('regex');
    }
  });
  return ops;
}

function isLocalOperator(op) {
  return LOCAL_OPERATORS.has(op) || DIRECTIVES.has(op);
}

function analyze(query) {
  const ast = parse(query);
  const operators = [...collectOperators(ast)];
  const mode = operators.every(isLocalOperator) ? 'local' : 'catalog';
  const printing = operators.some(op => PRINTING_OPERATORS.has(op));
  return { mode, ast, operators, printing };
}

function classifyQuery(query) {
  return { mode: analyze(query).mode };
}

function compileQuery(query) {
  const { ast, operators } = analyze(query);
  const foreign = operators.find(op => !isLocalOperator(op));
  if (foreign) throw new QuerySyntaxError(`"${foreign}" needs Scryfall's card database`);
  return (card) => evalNode(ast, card);
}

function matches(card, query) {
  return compileQuery(query)(card);
}

// ---------------------------------------------------------------- serializer

function fmtValue(value, quoted) {
  const v = String(value);
  if (quoted || /[\s()"]/.test(v) || v === '') return `"${v.replace(/"/g, '\\"')}"`;
  return v;
}

function leafToScryfall(n) {
  if (n.kind === 'name') {
    if (n.exact) return `!${fmtValue(n.value, n.quoted || /\s/.test(n.value))}`;
    return n.quoted ? fmtValue(n.value, true) : fmtValue(n.value, false);
  }
  if (n.op === 'is') {
    // Scrybox extension -> Scryfall's type operator.
    const v = n.value.replace(/-/g, ' ');
    return `t${n.cmp === '!=' ? '!=' : ':'}${fmtValue(v, /\s/.test(v))}`;
  }
  const value = n.regex ? `/${n.value}/` : fmtValue(n.value, n.quoted);
  return `${n.raw || n.op}${n.cmp || ':'}${value}`;
}

function nodeToScryfall(n, parent) {
  if (n.op === 'and') {
    const s = n.terms.map(t => nodeToScryfall(t, 'and')).join(' ');
    return parent && parent !== 'and' ? `(${s})` : s;
  }
  if (n.op === 'or') {
    const s = n.terms.map(t => nodeToScryfall(t, 'or')).join(' or ');
    return parent ? `(${s})` : s;
  }
  if (n.op === 'not' && n.term) {
    const inner = n.term;
    const leafy = inner.op !== 'and' && inner.op !== 'or' && !(inner.op === 'not' && inner.term);
    return leafy ? `-${leafToScryfall(inner)}` : `-(${nodeToScryfall(inner, null)})`;
  }
  return leafToScryfall(n);
}

// A copy of the tree without display directives (unique:, order:, ...). Returns
// null when nothing but directives remains. Collection walks choose their own
// result shape, so a user's unique:prints must not change what is intersected.
function stripDirectives(node) {
  if (node.op === 'and' || node.op === 'or') {
    const terms = node.terms.map(stripDirectives).filter(Boolean);
    if (!terms.length) return null;
    return terms.length === 1 ? terms[0] : { op: node.op, terms };
  }
  if (node.op === 'not' && node.term) {
    const inner = stripDirectives(node.term);
    return inner ? { op: 'not', term: inner } : null;
  }
  if (node.kind === 'op' && DIRECTIVES.has(node.op)) return null;
  return node;
}

// The query exactly as Scryfall's API should receive it.
function toScryfall(astOrQuery) {
  const ast = typeof astOrQuery === 'string' ? parse(astOrQuery) : astOrQuery;
  return nodeToScryfall(ast, null);
}

module.exports = {
  QuerySyntaxError,
  parse,
  analyze,
  classifyQuery,
  compileQuery,
  matches,
  toScryfall,
  stripDirectives,
  walkLeaves,
  parseColorValue,
  colorTest,
  COLOR_BIT,
  COLOR_ORDER,
  RARITY_ORDER,
  resolveColorTarget,
  resolveRarityTarget,
  resolveLanguageTarget,
  rarityIndex,
  LOCAL_OPERATORS,
  DIRECTIVES,
  PRINTING_OPERATORS,
};
