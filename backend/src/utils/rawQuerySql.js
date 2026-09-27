// Compile a Scryfall-syntax query (the LOCAL subset — terms the stored
// card_cache rows can answer, plus otag: via the local Oracle Tags index) into a
// single SQLite predicate over card_cache.
//
// The AST comes from shared/scryfallQuery.js analyze(), the single source of
// truth for what the syntax MEANS; this file only translates it, so the SQL path
// and the browser's JS evaluator cannot drift.
//
// Translated here:
//   word / "phrase" / name:x / !exact   name or printed_name
//   is:<type> / type:x / t=x            type line (subtypes array)
//   c: color (types column) / id: identity (color_identity) with : = != < <= > >=,
//   color counts (c=2), multicolor (c:m), guild/shard names
//   r: rarity with comparators (r>=rare)
//   s: set, cn: collector number (comparators), lang:, mv:/cmc: (comparators, even/odd)
//   otag:x                              local Oracle Tags (parent tags include children)
//   include:/unique:/order:/...         display directives — no filter
//   -x / -(a b) / or / and / (a b)      negation, OR, AND, grouping
const {
  resolveRarityTarget,
  resolveLanguageTarget,
  parseColorValue,
  rarityIndex,
  RARITY_ORDER,
  COLOR_BIT,
  DIRECTIVES,
} = require('../../../shared/scryfallQuery.js');

const JT = 'cc.subtypes';

function push(p, value) {
  p.push(value);
  return '?';
}

function subtypesLine() {
  return `COALESCE((SELECT group_concat(LOWER(je.value), ' ') FROM json_each(${JT}) je), '')`;
}

function negateIf(sql, cmp) {
  return cmp === '!=' ? `NOT (${sql})` : sql;
}

function nameMatch(p, value, exact) {
  if (exact) {
    const a = push(p, value);
    const b = push(p, value);
    const c = push(p, `${value} // %`);
    const d = push(p, `% // ${value}`);
    return `(LOWER(cc.name) = ${a} OR LOWER(COALESCE(cc.printed_name, '')) = ${b} OR LOWER(cc.name) LIKE ${c} OR LOWER(cc.name) LIKE ${d})`;
  }
  const a = push(p, `%${value}%`);
  const b = push(p, `%${value}%`);
  return `(cc.name LIKE ${a} OR COALESCE(cc.printed_name, '') LIKE ${b})`;
}

function isMatch(p, rawValue) {
  const val = String(rawValue).replace(/-/g, ' ').toLowerCase();
  if (val.includes(' ')) return `(' ' || ${subtypesLine()} || ' ') LIKE ${push(p, `% ${val} %`)}`;
  return `EXISTS (SELECT 1 FROM json_each(${JT}) je WHERE LOWER(je.value) = ${push(p, val)})`;
}

function typeMatch(p, rawValue) {
  return `${subtypesLine()} LIKE ${push(p, `%${String(rawValue).trim().toLowerCase()}%`)}`;
}

// Bitmask of a JSON array of color display names ('["Red","Blue"]').
function maskExpr(column) {
  const names = { white: COLOR_BIT.w, blue: COLOR_BIT.u, black: COLOR_BIT.b, red: COLOR_BIT.r, green: COLOR_BIT.g };
  const cases = Object.entries(names).map(([n, bit]) => `WHEN '${n}' THEN ${bit}`).join(' ');
  return `COALESCE((SELECT SUM(DISTINCT CASE LOWER(x.value) ${cases} ELSE 0 END) FROM json_each(CASE WHEN ${column} IS NULL OR ${column} = '' THEN '[]' ELSE ${column} END) x), 0)`;
}

function popcountExpr(m) {
  return `((${m} & 1) + ((${m} >> 1) & 1) + ((${m} >> 2) & 1) + ((${m} >> 3) & 1) + ((${m} >> 4) & 1))`;
}

function sqlCompare(expr, cmp, ph) {
  const op = { ':': '=', '=': '=', '!=': '<>', '<': '<', '<=': '<=', '>': '>', '>=': '>=' }[cmp];
  return `${expr} ${op} ${ph}`;
}

function colorMatch(p, leaf, column, defaultCmp) {
  const parsed = leaf.color || parseColorValue(leaf.value);
  const m = maskExpr(column);
  if (parsed.kind === 'multi') return negateIf(`${popcountExpr(m)} >= 2`, leaf.cmp);
  if (parsed.kind === 'count') return sqlCompare(popcountExpr(m), leaf.cmp === ':' ? '=' : leaf.cmp, push(p, parsed.n));
  const q = parsed.mask;
  const op = leaf.cmp === ':' ? (q === 0 ? '=' : defaultCmp) : leaf.cmp;
  const superset = `((${m}) & ${q}) = ${q}`;
  const subset = `((${m}) | ${q}) = ${q}`;
  switch (op) {
    case '=': return `(${m}) = ${q}`;
    case '!=': return `(${m}) <> ${q}`;
    case '>=': return superset;
    case '>': return `(${superset} AND (${m}) <> ${q})`;
    case '<=': return subset;
    case '<': return `(${subset} AND (${m}) <> ${q})`;
    default: return '0';
  }
}

function rarityRankExpr() {
  const stored = `' ' || LOWER(REPLACE(COALESCE(cc.rarity, ''), '_', ' ')) || ' '`;
  const cases = RARITY_ORDER.map((w, i) => `WHEN ${stored} LIKE '% ${w} %' THEN ${i}`).join(' ');
  return `(CASE ${cases} ELSE -1 END)`;
}

function rarityMatch(p, leaf) {
  const target = resolveRarityTarget(leaf.value);
  const ti = rarityIndex(target);
  if (ti < 0) {
    const padded = `' ' || LOWER(REPLACE(COALESCE(cc.rarity, ''), '_', ' ')) || ' '`;
    return negateIf(`${padded} LIKE ${push(p, `% ${target} %`)}`, leaf.cmp);
  }
  const rank = rarityRankExpr();
  const cmp = leaf.cmp === ':' ? '=' : leaf.cmp;
  if (cmp === '!=') return `${rank} <> ${push(p, ti)}`;
  return `(${rank} >= 0 AND ${sqlCompare(rank, cmp, push(p, ti))})`;
}

function setMatch(p, leaf) {
  return negateIf(`LOWER(COALESCE(cc.set_id, '')) = ${push(p, String(leaf.value).trim().toLowerCase())}`, leaf.cmp);
}

function allDigitsExpr(expr) {
  let stripped = expr;
  for (const d of '0123456789') stripped = `REPLACE(${stripped}, '${d}', '')`;
  return `LENGTH(${expr}) > 0 AND LENGTH(${expr}) = LENGTH(${stripped})`;
}

function numberMatch(p, leaf) {
  const v = String(leaf.value).trim();
  const stored = `TRIM(COALESCE(cc.number, ''))`;
  if (leaf.cmp === ':' || leaf.cmp === '=' || leaf.cmp === '!=') {
    const ors = [`LOWER(${stored}) = ${push(p, v.toLowerCase())}`];
    if (/^\d+$/.test(v)) {
      ors.push(`(${allDigitsExpr('cc.number')} AND CAST(cc.number AS INTEGER) = ${push(p, parseInt(v, 10))})`);
    }
    return negateIf(`(${ors.join(' OR ')})`, leaf.cmp);
  }
  const n = parseInt(v, 10);
  if (!Number.isFinite(n)) return '0';
  // Numeric prefix of the collector number ("123a" -> 123), like the JS parseInt.
  return `(CAST(${stored} AS INTEGER) > 0 OR ${stored} GLOB '0*') AND ${sqlCompare(`CAST(${stored} AS INTEGER)`, leaf.cmp, push(p, n))}`;
}

function langMatch(p, leaf) {
  const v = String(leaf.value).trim().toLowerCase();
  if (v === 'any') return leaf.cmp === '!=' ? '0' : '1';
  const target = resolveLanguageTarget(v);
  if (!target) return leaf.cmp === '!=' ? '1' : '0';
  return negateIf(`LOWER(COALESCE(cc.language, '')) = ${push(p, target.toLowerCase())}`, leaf.cmp);
}

function mvMatch(p, leaf) {
  const v = String(leaf.value).trim().toLowerCase();
  if (v === 'even' || v === 'odd') {
    const sql = `cc.cmc IS NOT NULL AND CAST(cc.cmc AS REAL) = CAST(cc.cmc AS INTEGER) AND CAST(cc.cmc AS INTEGER) % 2 = ${v === 'even' ? 0 : 1}`;
    return leaf.cmp === '!=' ? `(cc.cmc IS NOT NULL AND NOT (${sql}))` : `(${sql})`;
  }
  const num = Number(v);
  if (!Number.isFinite(num)) return '0';
  return `(cc.cmc IS NOT NULL AND ${sqlCompare('CAST(cc.cmc AS REAL)', leaf.cmp === ':' ? '=' : leaf.cmp, push(p, num))})`;
}

function oracleTagMatch(p, rawValue) {
  const alias = push(p, String(rawValue).trim().toLowerCase());
  return `EXISTS (
    SELECT 1
    FROM oracle_tag_generations otg
    CROSS JOIN oracle_tag_aliases ota
    CROSS JOIN oracle_tag_closure otc
    CROSS JOIN oracle_tag_assignments otm
    WHERE otg.active = 1
      AND ota.generation_id = otg.id AND ota.alias = ${alias}
      AND otc.generation_id = otg.id AND otc.ancestor_tag_id = ota.tag_id
      AND otm.generation_id = otg.id
      AND otm.tag_id = otc.descendant_tag_id
      AND otm.oracle_id = cc.oracle_id
  )`;
}

function leafToSql(node, p) {
  if (node.kind === 'name') return nameMatch(p, node.value, node.exact);
  switch (node.op) {
    case 'name': return negateIf(nameMatch(p, String(node.value).toLowerCase(), false), node.cmp);
    case 'is': return negateIf(isMatch(p, node.value), node.cmp);
    case 'type': return negateIf(typeMatch(p, node.value), node.cmp);
    case 'color': return colorMatch(p, node, 'cc.types', '>=');
    case 'identity': return colorMatch(p, node, 'cc.color_identity', '<=');
    case 'rarity': return rarityMatch(p, node);
    case 'set': return setMatch(p, node);
    case 'number': return numberMatch(p, node);
    case 'lang': return langMatch(p, node);
    case 'mv': return mvMatch(p, node);
    case 'otag': return negateIf(oracleTagMatch(p, node.value), node.cmp);
    default:
      if (DIRECTIVES.has(node.op)) return '1';
      throw new Error(`rawQuerySql: unhandled operator "${node.op}:"`);
  }
}

// Wrap a composite node in parentheses so it keeps its precedence when nested.
function isComposite(node) {
  return node.op === 'and' || node.op === 'or' || node.op === 'not';
}

function nodeToSql(node, p) {
  if (node.op === 'and') {
    const parts = node.terms.map(t => isComposite(t) ? `(${nodeToSql(t, p)})` : nodeToSql(t, p));
    return parts.length === 1 ? parts[0] : `(${parts.join(' AND ')})`;
  }
  if (node.op === 'or') {
    const parts = node.terms.map(t => isComposite(t) ? `(${nodeToSql(t, p)})` : nodeToSql(t, p));
    return `(${parts.join(' OR ')})`;
  }
  if (node.op === 'not') {
    return `NOT (${nodeToSql(node.term, p)})`;
  }
  return leafToSql(node, p);
}

// Build { whereSql, params } for one card_cache row. `whereSql` begins with
// "AND <predicate>" (caller prepends the base table + language filter).
function compileWhere(ast) {
  const params = [];
  const whereSql = `AND ${nodeToSql(ast, params)}`;
  return { whereSql, params };
}

// Full SELECT + COUNT for a raw local query, scoped to one language.
//   language  display name ('English', 'Japanese', ...) — the requested lang
//   ast       from analyze(query).ast
//   limit/offset  1-based page window
//   orderBy     stable deterministic order so paging never shuffles
// Returns { sql, params, countSql, countParams }.
function compileRawQuery({ ast, language, limit = 60, offset = 0 }) {
  const { whereSql, params } = compileWhere(ast);
  const orderBy = `ORDER BY LOWER(cc.name), cc.set_id, cc.number, cc.id`;
  const base = `FROM card_cache cc WHERE cc.language = ?`;
  const sql = `SELECT cc.* ${base}${whereSql} ${orderBy} LIMIT ? OFFSET ?`;
  const countSql = `SELECT COUNT(*) AS n ${base}${whereSql}`;
  return {
    sql,
    params: [language, ...params, limit, offset],
    countSql,
    countParams: [language, ...params],
  };
}

// Collection-catalog SELECT + COUNT. Unlike the internet cache query above,
// this returns physical owned rows and therefore orders by the collection's
// stable newest-first key. EXISTS-based tag matching cannot duplicate a row
// even when an oracle card is tagged by several descendants.
function compileCollectionQuery({ ast, userId, limit = 60, offset = 0, eligibleOnly = true }) {
  const { whereSql, params } = compileWhere(ast);
  const base = `FROM collection c
    JOIN card_cache cc ON cc.id = c.card_id
    WHERE c.user_id = ? AND c.quantity > 0${eligibleOnly ? `
      AND cc.scryfall_search_eligible = 1` : ''}`;
  const projection = `
    c.id AS entry_id,
    c.card_id,
    c.quantity,
    c.condition,
    c.printing,
    c.language,
    c.purchase_price,
    c.added_at,
    c.is_trade,
    c.notes,
    cc.oracle_id,
    cc.name,
    cc.printed_name,
    cc.supertype,
    cc.subtypes,
    cc.types,
    cc.cmc,
    cc.color_identity,
    cc.rarity,
    cc.set_id,
    cc.set_name,
    cc.number,
    cc.image_url,
    cc.price_trend,
    cc.price_normal,
    cc.price_holofoil,
    cc.price_etched,
    cc.price_currency,
    cc.price_source,
    cc.tcgplayer_url,
    cc.cardmarket_url,
    cc.tcgplayer_product_id`;
  const windowSql = limit == null ? '' : ' LIMIT ? OFFSET ?';
  return {
    sql: `SELECT ${projection} ${base} ${whereSql}
      ORDER BY c.added_at DESC, c.id DESC${windowSql}`,
    params: limit == null ? [userId, ...params] : [userId, ...params, limit, offset],
    countSql: `SELECT COUNT(*) AS n ${base} ${whereSql}`,
    countParams: [userId, ...params],
  };
}

module.exports = { compileRawQuery, compileCollectionQuery, compileWhere };
