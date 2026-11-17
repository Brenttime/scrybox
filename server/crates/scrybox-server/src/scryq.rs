//! Scrybox query semantics over local-scryfall's parser. There is NO
//! tokenizer/parser here: `parse` calls `local_scryfall::query::parse_syntax`
//! (the engine's own tokenizer + boolean grammar, Scrybox dialect) and lowers
//! its surface AST by transform. On top of that tree: `analyze` (mode /
//! printing classification), `toScryfall`, `stripDirectives`, the otag gate,
//! and `backend/src/utils/rawQuerySql.js`'s SQL compiler (`compileWhere` →
//! one SQLite predicate over `card_cache cc`, kept for card_cache/collection
//! rows that the card index does not model: printed_name, cached language).
//!
//! The SQL text and the parameter list are emitted byte-for-byte the way
//! Node builds them (same expressions, same parenthesisation, same bind
//! order), so a compiled predicate selects exactly the rows Node's does —
//! including SQL NULL / three-valued-logic behaviour under `NOT (...)`.
//! The differential corpus in `tests/scryq_differential.rs` pins this
//! against Node's own compiler run over the same fixture DB.
//!
//! JS-isms mirrored deliberately:
//!  * `\s` / `String#trim` use the JS whitespace set (`is_js_ws`);
//!  * the operator regex `^([a-z_]+)(!=|<=|>=|:|=|<|>)` is ASCII-only even
//!    under the `i` flag (non-unicode canonicalization never maps a
//!    non-ASCII char onto ASCII);
//!  * `Number(v)` (mv) and `parseInt(v, 10)` (number/color count) parsing;
//!  * bound JS numbers: integral values bind as INTEGER, others as REAL
//!    (node-sqlite3's binding; comparisons are numeric either way).

use rusqlite::types::Value as Sql;

#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxError(pub String);

/// JS `\s` (and the `String#trim` set) — local-scryfall's Scrybox dialect.
pub use local_scryfall::query::is_js_ws;
use local_scryfall::query as lsq;

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_ws)
}

// ---------------------------------------------------------------- tables

fn alias(op: &str) -> &str {
    match op {
        "t" => "type",
        "c" | "colour" => "color",
        "id" | "ci" | "commander" | "cmd" => "identity",
        "r" => "rarity",
        "s" | "e" | "edition" => "set",
        "cn" => "number",
        "language" => "lang",
        "cmc" | "manavalue" => "mv",
        "oracletag" | "function" => "otag",
        "art" | "arttag" => "atag",
        "m" => "mana",
        "o" => "oracle",
        "fo" => "fulloracle",
        "kw" => "keyword",
        "pow" => "power",
        "tou" => "toughness",
        "pt" => "powtou",
        "loy" => "loyalty",
        "def" => "defense",
        "b" => "block",
        "st" => "settype",
        "f" | "legal" => "format",
        "a" => "artist",
        "ft" => "flavor",
        "wm" => "watermark",
        "paperprints" => "papersets",
        other => other,
    }
}

const LOCAL_OPERATORS: &[&str] = &["name", "is", "type", "color", "identity", "rarity", "set", "number", "lang", "mv"];
pub const DIRECTIVES: &[&str] = &["include", "unique", "order", "sort", "direction", "prefer", "display"];
const CATALOG_OPERATORS: &[&str] = &[
    "otag", "atag", "mana", "devotion", "produces", "oracle", "fulloracle", "keyword",
    "power", "toughness", "powtou", "loyalty", "defense", "has", "isflag", "not",
    "in", "block", "settype", "cube", "format", "banned", "restricted",
    "usd", "eur", "tix", "artist", "artists", "flavor", "watermark", "border", "frame",
    "stamp", "game", "year", "date", "new", "illustrations", "prints", "sets", "papersets",
    "lore", "oracleid", "illustrationid", "layout", "regex",
];
const PRINTING_OPERATORS: &[&str] = &[
    "set", "number", "lang", "atag", "artist", "artists", "flavor", "watermark", "border",
    "frame", "stamp", "game", "year", "date", "new", "usd", "eur", "tix", "isflag", "not",
    "illustrationid",
];
pub const TYPE_WORDS: &[&str] = &[
    "land", "creature", "artifact", "enchantment", "instant", "sorcery", "planeswalker",
    "battle", "legendary", "basic", "snow", "tribal", "kindred", "world", "ongoing",
    "basic-land", "conspiracy", "plane", "phenomenon", "scheme", "vanguard", "dungeon",
];
/// oracleTags.js LOCAL_COLLECTION_OPERATORS.
const LOCAL_COLLECTION_OPERATORS: &[&str] = &[
    "color", "identity", "rarity", "lang", "mv", "otag",
    "include", "unique", "order", "sort", "direction", "prefer", "display",
];

const COLOR_WORDS: &[(&str, &str)] = &[
    ("white", "w"), ("blue", "u"), ("black", "b"), ("red", "r"), ("green", "g"),
    ("azorius", "wu"), ("dimir", "ub"), ("rakdos", "br"), ("gruul", "rg"), ("selesnya", "gw"),
    ("orzhov", "wb"), ("izzet", "ur"), ("golgari", "bg"), ("boros", "rw"), ("simic", "gu"),
    ("bant", "gwu"), ("esper", "wub"), ("grixis", "ubr"), ("jund", "brg"), ("naya", "rgw"),
    ("abzan", "wbg"), ("jeskai", "urw"), ("sultai", "bgu"), ("mardu", "rwb"), ("temur", "gur"),
    ("quandrix", "gu"), ("silverquill", "wb"), ("witherbloom", "bg"), ("lorehold", "rw"), ("prismari", "ur"),
    ("chaos", "ubrg"), ("aggression", "wbrg"), ("altruism", "wurg"), ("growth", "wubg"), ("artifice", "wubr"),
];

const RARITY_ORDER: &[&str] = &["common", "uncommon", "rare", "special", "mythic", "bonus"];

#[derive(Debug, Clone, PartialEq)]
pub enum Color {
    Mask(i64),
    /// parseInt of a digit run (huge runs lose precision like JS).
    Count(f64),
    Multi,
}

/// parseColorValue — Err when Node throws.
pub fn parse_color_value(value: &str) -> Result<Color, SyntaxError> {
    let v = js_trim(value).to_lowercase();
    if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Color::Count(js_parse_int(&v).unwrap_or(f64::NAN)));
    }
    if v == "m" || v == "multicolor" || v == "multicolored" {
        return Ok(Color::Multi);
    }
    if v == "c" || v == "colorless" {
        return Ok(Color::Mask(0));
    }
    let letters = COLOR_WORDS.iter().find(|(w, _)| *w == v).map(|(_, l)| l.to_string()).unwrap_or(v);
    if letters.is_empty() || !letters.chars().all(|c| matches!(c, 'w' | 'u' | 'b' | 'r' | 'g')) {
        return Err(SyntaxError(format!("Unknown color \"{value}\"")));
    }
    let mut mask = 0;
    for ch in letters.chars() {
        mask |= color_bit(ch);
    }
    Ok(Color::Mask(mask))
}

fn color_bit(c: char) -> i64 {
    match c {
        'w' => 1,
        'u' => 2,
        'b' => 4,
        'r' => 8,
        _ => 16,
    }
}

fn resolve_rarity_target(value: &str) -> String {
    let v = js_trim(value).to_lowercase();
    match v.as_str() {
        "c" => "common".into(),
        "u" => "uncommon".into(),
        "r" => "rare".into(),
        "s" => "special".into(),
        "m" => "mythic".into(),
        "b" => "bonus".into(),
        _ => v,
    }
}

fn rarity_index(w: &str) -> i64 {
    RARITY_ORDER.iter().position(|r| *r == w).map(|i| i as i64).unwrap_or(-1)
}

/// resolveLanguageTarget: LANG_INDEX (name lower, code lower, scryfall raw;
/// later rows overwrite earlier keys like Map#set).
fn resolve_language_target(value: &str) -> Option<String> {
    use std::sync::OnceLock;
    static IDX: OnceLock<std::collections::HashMap<String, String>> = OnceLock::new();
    let idx = IDX.get_or_init(|| {
        let v: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../../shared/languages.json")).expect("languages.json");
        let mut m = std::collections::HashMap::new();
        for l in &v {
            let name = l["name"].as_str().unwrap_or("").to_string();
            let code = l["code"].as_str().unwrap_or("");
            m.insert(name.to_lowercase(), name.clone());
            m.insert(code.to_lowercase(), name.clone());
            if let Some(s) = l["scryfall"].as_str() {
                if !s.is_empty() && s != code {
                    m.insert(s.to_string(), name.clone());
                }
            }
        }
        m
    });
    idx.get(&js_trim(value).to_lowercase()).cloned()
}

/// JS `parseInt(s, 10)` as an f64 (None = NaN).
pub fn js_parse_int(s: &str) -> Option<f64> {
    let t = js_trim(s);
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: f64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// JS `Number(s)` for a string (NaN → None, ±Infinity → Some(inf)).
pub fn js_number(s: &str) -> Option<f64> {
    let t = js_trim(s);
    if t.is_empty() {
        return Some(0.0);
    }
    let lower = t.to_ascii_lowercase();
    for (p, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(r) = lower.strip_prefix(p) {
            if r.is_empty() || !r.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            let mut n = 0f64;
            for c in r.chars() {
                n = n * radix as f64 + c.to_digit(radix).unwrap() as f64;
            }
            return Some(n);
        }
    }
    let (sign, body) = match t.as_bytes()[0] {
        b'-' => (-1.0, &t[1..]),
        b'+' => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    if body == "Infinity" {
        return Some(sign * f64::INFINITY);
    }
    // StrUnsignedDecimalLiteral
    let b = body.as_bytes();
    let mut i = 0;
    let mut int_digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            frac_digits += 1;
        }
    }
    if int_digits + frac_digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let s0 = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s0 {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    let n: f64 = body.parse().ok()?;
    Some(sign * n)
}

fn num_sql(n: f64) -> Sql {
    if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        Sql::Integer(n as i64)
    } else {
        Sql::Real(n)
    }
}

// ---------------------------------------------------------------- AST

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    /// `{op:'not', term}` (negation, `-( )`, and `not:<TYPE_WORD>`).
    Not(Box<Node>),
    Name { value: String, exact: bool, quoted: bool },
    Op(OpLeaf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpLeaf {
    /// canonical operator (ALIASES applied; is→isflag/not rewrites).
    pub op: String,
    /// typed (lowercased) keyword, re-emitted by toScryfall.
    pub raw: String,
    pub cmp: String,
    pub value: String,
    pub quoted: bool,
    pub regex: bool,
    pub catalog: bool,
    pub color: Option<Color>,
}

fn make_leaf(op: Option<&str>, cmp: &str, value: &str, exact: bool, quoted: bool, regex: bool) -> Result<Node, SyntaxError> {
    let Some(raw) = op else {
        if !exact && !quoted && value.eq_ignore_ascii_case("colorless") && value.len() == 9 {
            return Ok(Node::Op(OpLeaf {
                op: "color".into(),
                raw: "c".into(),
                cmp: ":".into(),
                value: "c".into(),
                quoted: false,
                regex: false,
                catalog: false,
                color: None,
            }));
        }
        return Ok(Node::Name { value: value.to_lowercase(), exact, quoted });
    };
    let op = alias(raw).to_string();
    let mut leaf = OpLeaf {
        op: op.clone(),
        raw: raw.to_string(),
        cmp: cmp.to_string(),
        value: value.to_string(),
        quoted,
        regex,
        catalog: false,
        color: None,
    };
    if op == "is" || op == "not" {
        let v = value.to_lowercase();
        if TYPE_WORDS.contains(&v.as_str()) {
            let is_leaf = OpLeaf { op: "is".into(), raw: "is".into(), value: v, ..leaf };
            return Ok(if op == "not" { Node::Not(Box::new(Node::Op(is_leaf))) } else { Node::Op(is_leaf) });
        }
        leaf.op = if op == "is" { "isflag".into() } else { "not".into() };
        return Ok(Node::Op(leaf));
    }
    if !LOCAL_OPERATORS.contains(&op.as_str()) && !DIRECTIVES.contains(&op.as_str()) && !CATALOG_OPERATORS.contains(&op.as_str()) {
        leaf.catalog = true;
        return Ok(Node::Op(leaf));
    }
    if leaf.regex {
        leaf.catalog = true;
        return Ok(Node::Op(leaf));
    }
    if op == "color" || op == "identity" {
        leaf.color = Some(parse_color_value(value)?);
    }
    if op == "mv" {
        let v = value.to_lowercase();
        if !(v == "even" || v == "odd") && !js_number(&v).is_some_and(|n| n.is_finite()) {
            return Err(SyntaxError(format!("Mana value must be a number, even or odd: \"{value}\"")));
        }
    }
    if ["name", "type", "set", "lang", "is"].contains(&op.as_str()) && ![":", "!=", "="].contains(&cmp) {
        return Err(SyntaxError(format!("\"{raw}\" does not support \"{cmp}\"")));
    }
    Ok(Node::Op(leaf))
}

/// Parse with local-scryfall's parser (the ONE Scryfall-syntax parser —
/// `local_scryfall::query::parse_syntax`, Scrybox dialect) and lower its
/// surface AST into the Scrybox-annotated tree by AST transform:
/// operator aliases, `is:`/`not:<type>` → type leaves, bare `colorless` →
/// `c:c`, lowercased names, color/mv/comparator validation.
pub fn parse(query: &str) -> Result<Node, SyntaxError> {
    let trimmed = js_trim(query);
    if trimmed.is_empty() {
        return Err(SyntaxError("Empty query".into()));
    }
    let ast = lsq::parse_syntax(trimmed).map_err(|e| SyntaxError(e.0))?;
    lower(ast)
}

fn lower(n: lsq::Node) -> Result<Node, SyntaxError> {
    Ok(match n {
        lsq::Node::And(ts) => Node::And(ts.into_iter().map(lower).collect::<Result<_, _>>()?),
        lsq::Node::Or(ts) => Node::Or(ts.into_iter().map(lower).collect::<Result<_, _>>()?),
        lsq::Node::Not(t) => Node::Not(Box::new(lower(*t)?)),
        lsq::Node::Atom(a) => {
            let leaf = make_leaf(a.key.as_deref(), &a.cmp, &a.value, a.exact, a.quoted, a.regex)?;
            if a.neg { Node::Not(Box::new(leaf)) } else { leaf }
        }
        other => return Err(SyntaxError(format!("unexpected parse node {other:?}"))),
    })
}

#[derive(Debug, Clone)]
pub struct Analysis {
    pub ast: Node,
    pub local: bool,
    pub operators: Vec<String>,
    pub printing: bool,
}

fn walk_leaves<'a>(n: &'a Node, f: &mut dyn FnMut(&'a Node)) {
    match n {
        Node::Not(t) => walk_leaves(t, f),
        Node::And(ts) | Node::Or(ts) => ts.iter().for_each(|t| walk_leaves(t, f)),
        _ => f(n),
    }
}

pub fn analyze(query: &str) -> Result<Analysis, SyntaxError> {
    let ast = parse(query)?;
    let mut ops: Vec<String> = Vec::new();
    let mut add = |s: &str| {
        if !ops.iter().any(|o| o == s) {
            ops.push(s.to_string());
        }
    };
    walk_leaves(&ast, &mut |n| match n {
        Node::Name { .. } => add("name"),
        Node::Op(l) => {
            add(&l.op);
            if l.catalog {
                add("regex");
            }
        }
        _ => {}
    });
    let local = ops.iter().all(|o| LOCAL_OPERATORS.contains(&o.as_str()) || DIRECTIVES.contains(&o.as_str()));
    let printing = ops.iter().any(|o| PRINTING_OPERATORS.contains(&o.as_str()));
    Ok(Analysis { ast, local, operators: ops, printing })
}

/// oracleTags.supportsLocalCollectionQuery.
pub fn supports_local_collection_query(a: &Analysis) -> bool {
    a.operators.iter().any(|o| o == "otag") && a.operators.iter().all(|o| LOCAL_COLLECTION_OPERATORS.contains(&o.as_str()))
}

// ---------------------------------------------------------------- serializer

fn fmt_value(v: &str, quoted: bool) -> String {
    if quoted || v.is_empty() || v.chars().any(|c| is_js_ws(c) || c == '(' || c == ')' || c == '"') {
        format!("\"{}\"", v.replace('"', "\\\""))
    } else {
        v.to_string()
    }
}

fn leaf_to_scryfall(n: &Node) -> String {
    match n {
        Node::Name { value, exact, quoted } => {
            if *exact {
                format!("!{}", fmt_value(value, *quoted || value.chars().any(is_js_ws)))
            } else {
                fmt_value(value, *quoted)
            }
        }
        Node::Op(l) if l.op == "is" => {
            let v = l.value.replace('-', " ");
            let q = v.chars().any(is_js_ws);
            format!("t{}{}", if l.cmp == "!=" { "!=" } else { ":" }, fmt_value(&v, q))
        }
        Node::Op(l) => {
            let value = if l.regex { format!("/{}/", l.value) } else { fmt_value(&l.value, l.quoted) };
            let raw = if l.raw.is_empty() { &l.op } else { &l.raw };
            let cmp = if l.cmp.is_empty() { ":" } else { &l.cmp };
            format!("{raw}{cmp}{value}")
        }
        _ => String::new(),
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Parent {
    None,
    And,
    Or,
}

fn node_to_scryfall(n: &Node, parent: Parent) -> String {
    match n {
        Node::And(ts) => {
            let s = ts.iter().map(|t| node_to_scryfall(t, Parent::And)).collect::<Vec<_>>().join(" ");
            if parent != Parent::None && parent != Parent::And { format!("({s})") } else { s }
        }
        Node::Or(ts) => {
            let s = ts.iter().map(|t| node_to_scryfall(t, Parent::Or)).collect::<Vec<_>>().join(" or ");
            if parent != Parent::None { format!("({s})") } else { s }
        }
        Node::Not(inner) => {
            let leafy = !matches!(**inner, Node::And(_) | Node::Or(_) | Node::Not(_));
            if leafy { format!("-{}", leaf_to_scryfall(inner)) } else { format!("-({})", node_to_scryfall(inner, Parent::None)) }
        }
        _ => leaf_to_scryfall(n),
    }
}

pub fn to_scryfall(ast: &Node) -> String {
    node_to_scryfall(ast, Parent::None)
}

pub fn strip_directives(n: &Node) -> Option<Node> {
    match n {
        Node::And(ts) | Node::Or(ts) => {
            let mut terms: Vec<Node> = ts.iter().filter_map(strip_directives).collect();
            if terms.is_empty() {
                return None;
            }
            if terms.len() == 1 {
                return terms.pop();
            }
            Some(if matches!(n, Node::And(_)) { Node::And(terms) } else { Node::Or(terms) })
        }
        Node::Not(t) => strip_directives(t).map(|i| Node::Not(Box::new(i))),
        Node::Op(l) if DIRECTIVES.contains(&l.op.as_str()) => None,
        _ => Some(n.clone()),
    }
}

// ---------------------------------------------------------------- SQL (rawQuerySql.js)

const JT: &str = "cc.subtypes";

fn push(p: &mut Vec<Sql>, v: Sql) -> &'static str {
    p.push(v);
    "?"
}

fn text(s: impl Into<String>) -> Sql {
    Sql::Text(s.into())
}

fn subtypes_line() -> String {
    format!("COALESCE((SELECT group_concat(LOWER(je.value), ' ') FROM json_each({JT}) je), '')")
}

fn negate_if(sql: String, cmp: &str) -> String {
    if cmp == "!=" { format!("NOT ({sql})") } else { sql }
}

fn name_match(p: &mut Vec<Sql>, value: &str, exact: bool) -> String {
    if exact {
        let a = push(p, text(value));
        let b = push(p, text(value));
        let c = push(p, text(format!("{value} // %")));
        let d = push(p, text(format!("% // {value}")));
        return format!("(LOWER(cc.name) = {a} OR LOWER(COALESCE(cc.printed_name, '')) = {b} OR LOWER(cc.name) LIKE {c} OR LOWER(cc.name) LIKE {d})");
    }
    let a = push(p, text(format!("%{value}%")));
    let b = push(p, text(format!("%{value}%")));
    format!("(cc.name LIKE {a} OR COALESCE(cc.printed_name, '') LIKE {b})")
}

fn is_match(p: &mut Vec<Sql>, raw: &str) -> String {
    let val = raw.replace('-', " ").to_lowercase();
    if val.contains(' ') {
        return format!("(' ' || {} || ' ') LIKE {}", subtypes_line(), push(p, text(format!("% {val} %"))));
    }
    format!("EXISTS (SELECT 1 FROM json_each({JT}) je WHERE LOWER(je.value) = {})", push(p, text(val)))
}

fn type_match(p: &mut Vec<Sql>, raw: &str) -> String {
    format!("{} LIKE {}", subtypes_line(), push(p, text(format!("%{}%", js_trim(raw).to_lowercase()))))
}

fn mask_expr(column: &str) -> String {
    format!(
        "COALESCE((SELECT SUM(DISTINCT CASE LOWER(x.value) WHEN 'white' THEN 1 WHEN 'blue' THEN 2 WHEN 'black' THEN 4 WHEN 'red' THEN 8 WHEN 'green' THEN 16 ELSE 0 END) FROM json_each(CASE WHEN {column} IS NULL OR {column} = '' THEN '[]' ELSE {column} END) x), 0)"
    )
}

fn popcount_expr(m: &str) -> String {
    format!("(({m} & 1) + (({m} >> 1) & 1) + (({m} >> 2) & 1) + (({m} >> 3) & 1) + (({m} >> 4) & 1))")
}

fn sql_cmp_op(cmp: &str) -> &'static str {
    match cmp {
        ":" | "=" => "=",
        "!=" => "<>",
        "<" => "<",
        "<=" => "<=",
        ">" => ">",
        ">=" => ">=",
        _ => "undefined",
    }
}

fn sql_compare(expr: &str, cmp: &str, ph: &str) -> String {
    format!("{expr} {} {ph}", sql_cmp_op(cmp))
}

fn color_match(p: &mut Vec<Sql>, leaf: &OpLeaf, column: &str, default_cmp: &str) -> Result<String, String> {
    let parsed = match &leaf.color {
        Some(c) => c.clone(),
        None => parse_color_value(&leaf.value).map_err(|e| e.0)?,
    };
    let m = mask_expr(column);
    Ok(match parsed {
        Color::Multi => negate_if(format!("{} >= 2", popcount_expr(&m)), &leaf.cmp),
        Color::Count(n) => {
            let cmp = if leaf.cmp == ":" { "=" } else { leaf.cmp.as_str() };
            let ph = push(p, num_sql(n));
            sql_compare(&popcount_expr(&m), cmp, ph)
        }
        Color::Mask(q) => {
            let op = if leaf.cmp == ":" { if q == 0 { "=" } else { default_cmp } } else { leaf.cmp.as_str() };
            let superset = format!("(({m}) & {q}) = {q}");
            let subset = format!("(({m}) | {q}) = {q}");
            match op {
                "=" => format!("({m}) = {q}"),
                "!=" => format!("({m}) <> {q}"),
                ">=" => superset,
                ">" => format!("({superset} AND ({m}) <> {q})"),
                "<=" => subset,
                "<" => format!("({subset} AND ({m}) <> {q})"),
                _ => "0".into(),
            }
        }
    })
}

fn rarity_rank_expr() -> String {
    let stored = "' ' || LOWER(REPLACE(COALESCE(cc.rarity, ''), '_', ' ')) || ' '";
    let cases: Vec<String> = RARITY_ORDER.iter().enumerate().map(|(i, w)| format!("WHEN {stored} LIKE '% {w} %' THEN {i}")).collect();
    format!("(CASE {} ELSE -1 END)", cases.join(" "))
}

fn rarity_match(p: &mut Vec<Sql>, leaf: &OpLeaf) -> String {
    let target = resolve_rarity_target(&leaf.value);
    let ti = rarity_index(&target);
    if ti < 0 {
        let padded = "' ' || LOWER(REPLACE(COALESCE(cc.rarity, ''), '_', ' ')) || ' '";
        return negate_if(format!("{padded} LIKE {}", push(p, text(format!("% {target} %")))), &leaf.cmp);
    }
    let rank = rarity_rank_expr();
    let cmp = if leaf.cmp == ":" { "=" } else { leaf.cmp.as_str() };
    if cmp == "!=" {
        return format!("{rank} <> {}", push(p, Sql::Integer(ti)));
    }
    let ph = push(p, Sql::Integer(ti));
    format!("({rank} >= 0 AND {})", sql_compare(&rank, cmp, ph))
}

fn set_match(p: &mut Vec<Sql>, leaf: &OpLeaf) -> String {
    negate_if(
        format!("LOWER(COALESCE(cc.set_id, '')) = {}", push(p, text(js_trim(&leaf.value).to_lowercase()))),
        &leaf.cmp,
    )
}

fn all_digits_expr(expr: &str) -> String {
    let mut stripped = expr.to_string();
    for d in "0123456789".chars() {
        stripped = format!("REPLACE({stripped}, '{d}', '')");
    }
    format!("LENGTH({expr}) > 0 AND LENGTH({expr}) = LENGTH({stripped})")
}

fn number_match(p: &mut Vec<Sql>, leaf: &OpLeaf) -> String {
    let v = js_trim(&leaf.value);
    let stored = "TRIM(COALESCE(cc.number, ''))";
    if leaf.cmp == ":" || leaf.cmp == "=" || leaf.cmp == "!=" {
        let mut ors = vec![format!("LOWER({stored}) = {}", push(p, text(v.to_lowercase())))];
        if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
            let n = js_parse_int(v).unwrap_or(f64::NAN);
            ors.push(format!("({} AND CAST(cc.number AS INTEGER) = {})", all_digits_expr("cc.number"), push(p, num_sql(n))));
        }
        return negate_if(format!("({})", ors.join(" OR ")), &leaf.cmp);
    }
    let Some(n) = js_parse_int(v).filter(|n| n.is_finite()) else { return "0".into() };
    let ph = push(p, num_sql(n));
    format!("(CAST({stored} AS INTEGER) > 0 OR {stored} GLOB '0*') AND {}", sql_compare(&format!("CAST({stored} AS INTEGER)"), &leaf.cmp, ph))
}

fn lang_match(p: &mut Vec<Sql>, leaf: &OpLeaf) -> String {
    let v = js_trim(&leaf.value).to_lowercase();
    if v == "any" {
        return if leaf.cmp == "!=" { "0".into() } else { "1".into() };
    }
    let Some(target) = resolve_language_target(&v) else {
        return if leaf.cmp == "!=" { "1".into() } else { "0".into() };
    };
    negate_if(format!("LOWER(COALESCE(cc.language, '')) = {}", push(p, text(target.to_lowercase()))), &leaf.cmp)
}

fn mv_match(p: &mut Vec<Sql>, leaf: &OpLeaf) -> String {
    let v = js_trim(&leaf.value).to_lowercase();
    if v == "even" || v == "odd" {
        let sql = format!(
            "cc.cmc IS NOT NULL AND CAST(cc.cmc AS REAL) = CAST(cc.cmc AS INTEGER) AND CAST(cc.cmc AS INTEGER) % 2 = {}",
            if v == "even" { 0 } else { 1 }
        );
        return if leaf.cmp == "!=" { format!("(cc.cmc IS NOT NULL AND NOT ({sql}))") } else { format!("({sql})") };
    }
    let Some(num) = js_number(&v).filter(|n| n.is_finite()) else { return "0".into() };
    let cmp = if leaf.cmp == ":" { "=" } else { leaf.cmp.as_str() };
    let ph = push(p, num_sql(num));
    format!("(cc.cmc IS NOT NULL AND {})", sql_compare("CAST(cc.cmc AS REAL)", cmp, ph))
}

fn oracle_tag_match(p: &mut Vec<Sql>, raw: &str) -> String {
    let alias = push(p, text(js_trim(raw).to_lowercase()));
    format!(
        "EXISTS (\n    SELECT 1\n    FROM oracle_tag_generations otg\n    CROSS JOIN oracle_tag_aliases ota\n    CROSS JOIN oracle_tag_closure otc\n    CROSS JOIN oracle_tag_assignments otm\n    WHERE otg.active = 1\n      AND ota.generation_id = otg.id AND ota.alias = {alias}\n      AND otc.generation_id = otg.id AND otc.ancestor_tag_id = ota.tag_id\n      AND otm.generation_id = otg.id\n      AND otm.tag_id = otc.descendant_tag_id\n      AND otm.oracle_id = cc.oracle_id\n  )"
    )
}

fn leaf_to_sql(n: &Node, p: &mut Vec<Sql>) -> Result<String, String> {
    let l = match n {
        Node::Name { value, exact, .. } => return Ok(name_match(p, value, *exact)),
        Node::Op(l) => l,
        _ => return Err("not a leaf".into()),
    };
    Ok(match l.op.as_str() {
        "name" => negate_if(name_match(p, &l.value.to_lowercase(), false), &l.cmp),
        "is" => negate_if(is_match(p, &l.value), &l.cmp),
        "type" => negate_if(type_match(p, &l.value), &l.cmp),
        "color" => color_match(p, l, "cc.types", ">=")?,
        "identity" => color_match(p, l, "cc.color_identity", "<=")?,
        "rarity" => rarity_match(p, l),
        "set" => set_match(p, l),
        "number" => number_match(p, l),
        "lang" => lang_match(p, l),
        "mv" => mv_match(p, l),
        "otag" => negate_if(oracle_tag_match(p, &l.value), &l.cmp),
        op if DIRECTIVES.contains(&op) => "1".into(),
        op => return Err(format!("rawQuerySql: unhandled operator \"{op}:\"")),
    })
}

fn is_composite(n: &Node) -> bool {
    matches!(n, Node::And(_) | Node::Or(_) | Node::Not(_)) || matches!(n, Node::Op(l) if l.op == "not")
}

fn node_to_sql(n: &Node, p: &mut Vec<Sql>) -> Result<String, String> {
    match n {
        Node::And(ts) => {
            let mut parts = Vec::new();
            for t in ts {
                let s = node_to_sql(t, p)?;
                parts.push(if is_composite(t) { format!("({s})") } else { s });
            }
            Ok(if parts.len() == 1 { parts.pop().unwrap() } else { format!("({})", parts.join(" AND ")) })
        }
        Node::Or(ts) => {
            let mut parts = Vec::new();
            for t in ts {
                let s = node_to_sql(t, p)?;
                parts.push(if is_composite(t) { format!("({s})") } else { s });
            }
            Ok(format!("({})", parts.join(" OR ")))
        }
        Node::Not(t) => Ok(format!("NOT ({})", node_to_sql(t, p)?)),
        Node::Op(l) if l.op == "not" => Err("rawQuerySql: not: without term".into()),
        _ => leaf_to_sql(n, p),
    }
}

/// compileWhere: `AND <predicate>` + params.
pub fn compile_where(ast: &Node, p: &mut Vec<Sql>) -> Result<String, String> {
    Ok(format!("AND {}", node_to_sql(ast, p)?))
}

/// compileRawQuery: (sql, params, countSql, countParams).
pub fn compile_raw_query(ast: &Node, language: &str, limit: i64, offset: i64) -> Result<(String, Vec<Sql>, String, Vec<Sql>), String> {
    let mut p = Vec::new();
    let where_sql = compile_where(ast, &mut p)?;
    let base = "FROM card_cache cc WHERE cc.language = ?";
    let sql = format!("SELECT cc.* {base}{where_sql} ORDER BY LOWER(cc.name), cc.set_id, cc.number, cc.id LIMIT ? OFFSET ?");
    let count_sql = format!("SELECT COUNT(*) AS n {base}{where_sql}");
    let mut count_params = vec![Sql::Text(language.to_string())];
    count_params.extend(p);
    let mut params = count_params.clone();
    params.push(Sql::Integer(limit));
    params.push(Sql::Integer(offset));
    Ok((sql, params, count_sql, count_params))
}

/// compileCollectionQuery (eligibleOnly) → (sql, params, countSql, countParams);
/// `window` = None for the unbounded list.
pub fn compile_collection_query(ast: &Node, user_id: i64, window: Option<(i64, i64)>, eligible_only: bool) -> Result<(String, Vec<Sql>, String, Vec<Sql>), String> {
    let mut p = Vec::new();
    let where_sql = compile_where(ast, &mut p)?;
    let base = format!(
        "FROM collection c\n    JOIN card_cache cc ON cc.id = c.card_id\n    WHERE c.user_id = ? AND c.quantity > 0{}",
        if eligible_only { "\n      AND cc.scryfall_search_eligible = 1" } else { "" }
    );
    let sql = format!(
        "SELECT c.id AS entry_id, c.card_id, c.quantity, c.condition, c.printing, c.language, c.purchase_price, c.added_at, c.is_trade, c.notes, c.is_proxy, cc.oracle_id, cc.name, cc.printed_name, cc.supertype, cc.subtypes, cc.types, cc.cmc, cc.color_identity, cc.rarity, cc.set_id, cc.set_name, cc.number, cc.image_url, cc.price_trend, cc.price_normal, cc.price_holofoil, cc.price_etched, cc.price_currency, cc.price_source, cc.price_estimated, cc.tcgplayer_url, cc.cardmarket_url, cc.tcgplayer_product_id {base} {where_sql}\n      ORDER BY c.added_at DESC, c.id DESC{}",
        if window.is_some() { " LIMIT ? OFFSET ?" } else { "" }
    );
    let mut count_params = vec![Sql::Integer(user_id)];
    count_params.extend(p);
    let mut params = count_params.clone();
    if let Some((l, o)) = window {
        params.push(Sql::Integer(l));
        params.push(Sql::Integer(o));
    }
    Ok((sql, params, format!("SELECT COUNT(*) AS n {base} {where_sql}"), count_params))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(q: &str) -> String {
        let a = analyze(q).unwrap();
        strip_directives(&a.ast).map(|n| to_scryfall(&n)).unwrap_or_default()
    }

    #[test]
    fn serializer_matches_node_samples() {
        assert_eq!(ts("Lightning Bolt"), "lightning bolt");
        assert_eq!(ts("is:basic-land c:g"), "t:\"basic land\" c:g");
        assert_eq!(ts("not:land"), "-t:land");
        assert_eq!(ts("(t:elf or t:goblin) -c:r"), "(t:elf or t:goblin) -c:r");
        assert_eq!(ts("!\"Fire // Ice\" unique:prints"), "!\"fire // ice\"");
        assert_eq!(ts("/^goblin/"), "name:/^goblin/");
        assert_eq!(ts("colorless"), "c:c");
        assert_eq!(ts("a or b c"), "a or (b c)");
        assert_eq!(ts("-(a or b)"), "-(a or b)");
    }

    #[test]
    fn analysis_modes() {
        assert!(analyze("t:elf r>=rare mv<=3 s:lea cn:12 lang:ja").unwrap().local);
        assert!(!analyze("o:draw").unwrap().local);
        assert!(analyze("s:lea").unwrap().printing);
        assert!(analyze("c:zz").is_err());
        assert!(analyze("mv:abc").is_err());
        assert!(analyze("t>elf").is_err());
        assert!(analyze("a and").is_err());
        assert!(analyze("(a").is_err());
        assert!(analyze("a)").is_err());
        assert!(analyze("-").is_err());
        assert!(analyze("-or").is_err());
        assert!(analyze("c:lorehold").is_ok());
        assert_eq!(js_number("0x10"), Some(16.0));
        assert_eq!(js_number(" 2.5e1 "), Some(25.0));
        assert_eq!(js_number("inf"), None);
        assert_eq!(js_number("1e400"), Some(f64::INFINITY));
    }
}
