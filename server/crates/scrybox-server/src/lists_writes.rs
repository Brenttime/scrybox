//! Native port of the card-list META-WRITE routes from
//! `backend/src/routes/lists.js`: `POST /` (create, routes/lists.js:96),
//! `PUT /order` (:292), `PUT /:id` (meta update, :379) and `DELETE /:id` (:410).
//! Mounted under the `lists_writes` name in RUST_ROUTES; the card-body writes
//! (POST /:id/cards, bulk, bulk-remove, printing, cheapest-printings,
//! remove-from-collection) stay proxied to Node — the /api/lists/{id}
//! dispatcher answers GET natively (lists_reads), PUT/DELETE here, and
//! forwards every other method, so shapes are never 405'd.
//!
//! Every status, body and message was written against the actual Node source
//! (routes/lists.js, utils/listImport.js parseListLine/parseListText/
//! findCachedCard) plus live byte-pinning against the :3312 container
//! (lw_* parity cases) and the Node-oracle fixture table
//! (tests/fixtures/list_import_oracle.json, generated from the real
//! listImport.js).
//!
//! Key JS semantics reproduced deliberately:
//!   * POST create's Scryfall-hydration leg stays Node-side: if ANY parsed
//!     list_text name misses the cache, the WHOLE request is forwarded
//!     verbatim BEFORE any local INSERT (the decks_writes create/
//!     add-card idiom) — zero partial-write risk, Node's own lastID and
//!     cache writes proceed untouched. The native path therefore only ever
//!     answers the all-cached shape; `unmatched` is structurally empty.
//!   * parseListLine is a hand-rolled twin of the three JS regexes: the
//!     /\s+/g collapse (JS \s class, which swallows \u2028/\u2029/nbsp so
//!     the later '.'-and-\s* logic only ever sees single spaces), the
//!     HEADER /^(deck|main ?deck|...)\s*:?\s*$/i (ASCII-only folding —
//!     Node-oracle-verified: İ/ı/ſ do NOT match), the qty
//!     /^(\d+)\s*x?\s+(.+)$/i with full JS backtracking preference ("4 x
//!     Bolt" -> qty 4 name "Bolt" via the x?-then-fail path, "4 xtra
//!     stuff" -> qty 4 name "xtra stuff" via the \s*-shrink path), and the
//!     trailing "(SET) num//foil *X*" ref whose backtracking is emulated
//!     in preference order. ref[0] always spans to end-of-input, so JS's
//!     unit-indexed `slice(0, len - ref[0].length)` is exactly the
//!     character prefix before the match start.
//!   * qty = Math.max(1, parseInt(digits,10) || 1): pure ASCII digits,
//!     nearest-double decimal, 0 -> 1; a > i64 run binds as REAL like
//!     node-sqlite3 + INTEGER affinity would.
//!   * PUT /order: `Array.isArray(req.body?.ids)` then map(Number).filter(
//!     Number.isInteger) — null -> 0 survives, true -> 1 survives, {} ->
//!     NaN dies, '4' -> 4 survives, 4.5 dies; sort_order is the POST-FILTER
//!     index. Empty or >1000 answers 400 without touching the DB.
//!   * PUT /:id accent gate: `accent_color != null && accent_color
//!     .startsWith('#')` — a present non-string non-null value throws
//!     TypeError on the Node side -> caught -> 500 'Failed to update list'.
//!     The create gate is different: `typeof accent_color === 'string' &&
//!     startsWith('#')` -> any non-string just falls back to '#10b981'.
//!   * DELETE relies on Node's `PRAGMA foreign_keys = ON` cascade for
//!     list_cards; Rust's pool does not enable FK enforcement, so the
//!     cascade is mirrored explicitly (delete list_cards rows, then the
//!     parent, in one BEGIN IMMEDIATE) — same as the decks_writes delete
//!     precedent, same observable end state.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::extract::Request;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::auth::{authenticate, bearer_token};
use crate::collection_writes::{
    get_row_map, internal_error, js_falsy, js_to_string, json_error, json_response,
    json_to_sql_value, node_exact, parse_body, read_parts, rebuild, too_large, BodyOutcome, Tx,
};
use crate::decks_reads::{js_number, js_trim};
use crate::db::DbPool;
use crate::AppState;

// ---------------------------------------------------------------------------
// Route entries
// ---------------------------------------------------------------------------

/// /api/lists collection dispatcher: GET stays with lists_reads when that
/// group is on, POST create is native here, everything else forwards.
pub async fn route_collection(State(st): State<AppState>, req: Request) -> Response {
    if req.method() == Method::POST && st.owns("lists_writes") {
        return handle_create(State(st), req).await;
    }
    if req.method() == Method::GET && st.owns("lists_reads") {
        return crate::lists_reads::handle_collection(State(st), req).await;
    }
    crate::proxy(State(st), req).await
}

/// POST /api/lists (routes/lists.js:96). express.json runs before the auth
/// gate, so a malformed body outranks 401 (parse -> auth, decks_writes twin).
pub async fn handle_create(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    let body = match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    match create_list(&pool, user.id, &body) {
        CreateOutcome::Response(res) => res,
        CreateOutcome::Forward => crate::proxy(State(st), rebuild(parts, &bytes)).await,
    }
}

/// PUT /api/lists/order (routes/lists.js:292).
pub async fn handle_order(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::PUT {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    let body = match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    reorder_lists(&pool, user.id, &body)
}

/// PUT /api/lists/:id (routes/lists.js:379).
pub async fn handle_update(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::PUT {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    let body = match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    update_list(&pool, &id, user.id, &body)
}

/// DELETE /api/lists/:id (routes/lists.js:410). express.json still runs
/// ahead of auth for DELETE (live-pinned on the decks twin: bad JSON with
/// no token -> 500), so the body is read and parsed before authenticate.
pub async fn handle_delete(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::DELETE {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => {} // route ignores the body; parse only mirrors error order
    }
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    delete_list(&pool, &id, user.id)
}

// ---------------------------------------------------------------------------
// utils/listImport.js — parseListLine / parseListText / findCachedCard twins
// ---------------------------------------------------------------------------

/// JS \s (the exact class used by /\s+/g, trim and the regexes' \s tokens).
fn js_ws(c: char) -> bool {
    matches!(c,
        ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | '\u{a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `String(raw).replace(/\s+/g, ' ')` — every run of JS-\s becomes one space.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if js_ws(c) {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

fn ascii_lower(c: char) -> char {
    if c.is_ascii_uppercase() {
        (c as u8 + 32) as char
    } else {
        c
    }
}

fn is_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

fn is_num_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

fn is_alpha(c: char) -> bool {
    c.is_ascii_alphabetic()
}

/// The HEADER regex body words (case-folded). 'main ?deck' expands to both
/// 'main deck' and 'maindeck'; 'commanders?' / 'tokens?' likewise.
const HEADER_WORDS: [&str; 11] = [
    "deck", "main deck", "maindeck", "sideboard", "commander", "commanders",
    "companion", "maybeboard", "token", "tokens", "about",
];

/// HEADER /^(deck|main ?deck|maindeck|sideboard|commanders?|companion|
///          maybeboard|tokens?|about)\s*:?\s*$/i over the collapsed line.
/// Post-collapse the \s tokens can only be single spaces. JS /i is ASCII-
/// only folding here (Node-oracle: İ, ı and ſ do NOT match their ASCII look
/// alikes), so an ASCII lowercase compare is exact.
fn header_test(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let lowered: String = chars.iter().map(|c| ascii_lower(*c)).collect();
    for word in HEADER_WORDS {
        if !lowered.starts_with(word) {
            continue;
        }
        // \s*:?\s*$ over the remainder: spaces, optional single ':', spaces.
        let mut seen_colon = false;
        let mut ok = true;
        for &c in &chars[word.chars().count()..] {
            if c == ' ' {
                continue;
            }
            if c == ':' && !seen_colon {
                seen_colon = true;
                continue;
            }
            ok = false;
            break;
        }
        if ok {
            return true;
        }
    }
    false
}

/// The qty regex /^(\d+)\s*x?\s+(.+)$/i over the collapsed line. Returns
/// (qty_digits_str, rest) with the JS backtracking preference order: \s*
/// greedy-then-shrink, x? try-then-skip, \s+ at least one space, (.+) at
/// least one char to end-of-input.
fn qty_match(line: &str) -> Option<(String, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut d = 0usize;
    while d < chars.len() && is_digit(chars[d]) {
        d += 1;
    }
    if d == 0 {
        return None;
    }
    let digits: String = chars[..d].iter().collect();
    // \s* — greedy first, shrinking; x? — try, then skip.
    let mut ws_end = d;
    while ws_end < chars.len() && chars[ws_end] == ' ' {
        ws_end += 1;
    }
    let mut spaces = ws_end;
    loop {
        let pos = spaces; // index after the consumed \s* run
        // x? present:
        if pos < chars.len() && matches!(chars[pos], 'x' | 'X') {
            let mut p2 = pos + 1;
            let ws_start = p2;
            while p2 < chars.len() && chars[p2] == ' ' {
                p2 += 1;
            }
            if p2 > ws_start && p2 < chars.len() {
                return Some((digits.clone(), chars[p2..].iter().collect()));
            }
        }
        // x? absent:
        let mut p2 = pos;
        let ws_start = p2;
        while p2 < chars.len() && chars[p2] == ' ' {
            p2 += 1;
        }
        if p2 > ws_start && p2 < chars.len() {
            return Some((digits.clone(), chars[p2..].iter().collect()));
        }
        if spaces == d {
            return None;
        }
        spaces -= 1;
    }
}


/// parseInt(digits, 10) || 1 then Math.max(1, _) over a pure-digit run:
/// nearest-double decimal, 0/NaN -> 1, never below 1.
fn qty_value(digits: &str) -> SqlValue {
    // Leading zeros are fine for Rust's decimal parse; digits are non-empty.
    let n: f64 = digits.parse::<f64>().unwrap_or(f64::NAN);
    let q = if n.is_nan() || n == 0.0 { 1.0 } else { n };
    let q = q.max(1.0);
    exact_int(q).map_or(SqlValue::Real(q), SqlValue::Integer)
}

/// JS-Number-as-INTEGER binding probe: an exact i64-representable integer.
pub(crate) fn exact_int(v: f64) -> Option<i64> {
    if v.fract() == 0.0 && v.abs() <= 9.223372036854776e18 {
        let i = v as i64;
        if i as f64 == v {
            Some(i)
        } else {
            None
        }
    } else {
        None
    }
}

/// The trailing "(SET) num//foil *X*" ref. Emulates the JS match
/// /\s*\(([A-Za-z0-9]{2,13})\)\s*([A-Za-z0-9-]+(?:\/\/\d+)?)?\s*
///   (\*[A-Z]\*)?\s*$/i left-to-right over the collapsed line: leftmost
/// start whose span reaches end-of-input, greedy-then-shrink preference at
/// each optional piece. Returns the match start index (chars); the match
/// always ends at end-of-input.
fn setref_start(rest: &[char]) -> Option<usize> {
    (0..=rest.len()).find(|&s| try_setref_at(rest, s).is_some())
}

/// Tail after the optional number group: ' '* ('*' [A-Za-z] '*')? ' '* $,
/// stamp tried-present-then-absent. Returns true when the tail can consume
/// exactly through end-of-input.
fn tail_ok(chars: &[char], mut p: usize) -> bool {
    while p < chars.len() && chars[p] == ' ' {
        p += 1;
    }
    // try stamp present
    if p + 2 < chars.len() && chars[p] == '*' && is_alpha(chars[p + 1]) && chars[p + 2] == '*' {
        let mut q = p + 3;
        while q < chars.len() && chars[q] == ' ' {
            q += 1;
        }
        if q == chars.len() {
            return true;
        }
    }
    // stamp absent
    while p < chars.len() && chars[p] == ' ' {
        p += 1;
    }
    p == chars.len()
}

fn try_setref_at(chars: &[char], s: usize) -> Option<()> {
    let mut p = s;
    while p < chars.len() && chars[p] == ' ' {
        p += 1;
    }
    if p >= chars.len() || chars[p] != '(' {
        return None;
    }
    p += 1;
    // (SET) — greedy [A-Za-z0-9]{2,13} then ')' with shrink-backtracking.
    let run_start = p;
    let mut run_end = p;
    while run_end < chars.len() && is_alnum(chars[run_end]) && run_end - run_start < 13 {
        run_end += 1;
    }
    let mut matched_close = None;
    let mut k = run_end;
    while k >= run_start + 2 && k > run_start {
        if k < chars.len() && chars[k] == ')' {
            matched_close = Some(k);
            break;
        }
        k -= 1;
    }
    let close = matched_close?;
    p = close + 1;
    // ' '* before the optional number group.
    while p < chars.len() && chars[p] == ' ' {
        p += 1;
    }
    // Number group present (greedy, with trailing '//digits', both shrunk
    // in JS preference order), then tail.
    let num_start = p;
    let mut num_end = p;
    while num_end < chars.len() && is_num_char(chars[num_end]) {
        num_end += 1;
    }
    if num_end > num_start {
        // with (//digits)? present
        let w = num_end;
        if w + 2 <= chars.len() && w + 1 < chars.len() && chars[w] == '/' && chars[w + 1] == '/' {
            let mut wd = w + 2;
            while wd < chars.len() && is_digit(chars[wd]) {
                wd += 1;
            }
            if wd > w + 2 {
                // greedy digits, shrink
                let mut q = wd;
                while q > w + 2 {
                    if tail_ok(chars, q) {
                        return Some(());
                    }
                    q -= 1;
                }
            }
        }
        // number without //digits, shrink the [A-Za-z0-9-]+ run
        let mut q = num_end;
        while q > num_start {
            if tail_ok(chars, q) {
                return Some(());
            }
            q -= 1;
        }
    }
    // number group absent
    if tail_ok(chars, num_start) {
        return Some(());
    }
    None
}

/// Parsed list line, utils/listImport.js shape. Only qty/name matter to the
/// native route (setRef/number feed the unmatched text, unreachable natively),
/// but they are produced faithfully and pinned by the oracle fixture.
#[derive(Debug, PartialEq)]
pub struct ParsedLine {
    pub qty: SqlValue,
    pub name: String,
    pub set_ref: String,
    #[allow(dead_code)]
    pub set: String,
    #[allow(dead_code)]
    pub number: String,
}

pub fn parse_list_line(raw: &str) -> Option<ParsedLine> {
    // String(raw || ''): raw is always a string from split; empty collapses
    // to the falsy-line return below.
    let collapsed = collapse_ws(raw);
    let line = js_trim(&collapsed).to_string();
    if line.is_empty() || line.starts_with("//") || line.starts_with('#') || header_test(&line) {
        return None;
    }
    let mut qty = SqlValue::Integer(1);
    let mut rest = line.clone();
    if let Some((digits, tail)) = qty_match(&line) {
        qty = qty_value(&digits);
        rest = js_trim(&tail).to_string();
    }
    let mut set_ref = String::new();
    let mut set = String::new();
    let mut number = String::new();
    let chars: Vec<char> = rest.chars().collect();
    if let Some(s) = setref_start(&chars) {
        let span: String = chars[s..].iter().collect();
        set_ref = js_trim(&span).to_string();
        // Re-derive set/number from the span for the oracle contract.
        let (set_part, num_part) = setref_groups(&chars[s..]);
        set = set_part.to_lowercase();
        number = num_part;
        // JS: rest.slice(0, rest.length - ref[0].length) — the match spans
        // [s, end), so this is exactly the char prefix, then .trim().
        rest = js_trim(&chars[..s].iter().collect::<String>()).to_string();
    }
    if rest.is_empty() {
        return None;
    }
    Some(ParsedLine { qty, name: rest, set_ref, set, number })
}

/// Extract (SET, number) from a matched ref span (chars starting at the
/// match start, guaranteed to satisfy the ref grammar through end-of-input).
fn setref_groups(span: &[char]) -> (String, String) {
    let mut p = 0usize;
    while p < span.len() && span[p] == ' ' {
        p += 1;
    }
    debug_assert!(p < span.len() && span[p] == '(');
    p += 1;
    let run_start = p;
    while p < span.len() && is_alnum(span[p]) {
        p += 1;
    }
    // the matched set length is the greedy run (2..=13) ending at ')'
    let mut set_end = p.min(run_start + 13);
    while set_end >= run_start + 2 && (set_end >= span.len() || span[set_end] != ')') {
        set_end -= 1;
    }
    let set: String = span[run_start..set_end].iter().collect();
    let mut p = set_end + 1; // past ')'
    while p < span.len() && span[p] == ' ' {
        p += 1;
    }
    let num_start = p;
    while p < span.len() && is_num_char(span[p]) {
        p += 1;
    }
    let mut num_end = p;
    if p + 2 < span.len() && span[p] == '/' && span[p + 1] == '/' {
        let mut wd = p + 2;
        while wd < span.len() && is_digit(span[wd]) {
            wd += 1;
        }
        if wd > p + 2 {
            num_end = wd;
        }
    }
    let number: String = span[num_start..num_end].iter().collect();
    (set, number)
}

/// parseListText: String(text).split(/\r?\n/), map(parseListLine), filter.
pub fn parse_list_text(text: &str) -> Vec<ParsedLine> {
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter_map(parse_list_line)
        .collect()
}

/// findCachedCard (utils/listImport.js:36) — exact SQL twin.
fn find_cached_card(conn: &Connection, name: &str) -> Result<Option<Value>, crate::collection_writes::WriteError> {
    get_row_map(
        conn,
        "SELECT id FROM card_cache
         WHERE LOWER(name) = LOWER(?1) OR LOWER(name) LIKE LOWER(?2) OR LOWER(name) LIKE LOWER(?3)
         ORDER BY CASE WHEN LOWER(name) = LOWER(?4) THEN 0 ELSE 1 END
         LIMIT 1",
        &[
            SqlValue::Text(name.to_string()),
            SqlValue::Text(format!("{name} // %")),
            SqlValue::Text(format!("% // {name}")),
            SqlValue::Text(name.to_string()),
        ],
    )
    .map(|m| m.and_then(|row| row.get("id").cloned()))
}

/// cleanIcon: typeof string && /^[a-z0-9-]{1,32}$/.test(v) — the class is
/// ASCII-only and $ has no multiline meaning in JS test(), so a plain byte
/// scan is exact.
fn clean_icon(v: Option<&Value>) -> SqlValue {
    match v {
        Some(Value::String(s)) => {
            let bytes = s.as_bytes();
            if !bytes.is_empty()
                && bytes.len() <= 32
                && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            {
                SqlValue::Text(s.clone())
            } else {
                SqlValue::Null
            }
        }
        _ => SqlValue::Null,
    }
}

/// node-sqlite3 binding of a raw JSON value (decks_writes twin — live-
/// pinned: bool -> INTEGER 1/0, objects/arrays -> '[object Object]').
pub(crate) fn js_bind(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(if *b { 1 } else { 0 }),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        _ => SqlValue::Text("[object Object]".to_string()),
    }
}

// ---------------------------------------------------------------------------
// POST / — create
// ---------------------------------------------------------------------------

enum CreateOutcome {
    Response(Response),
    /// At least one parsed name misses the cache: the whole request goes to
    /// Node untouched (Scryfall hydration + lastID + unmatched all Node-side).
    Forward,
}

fn create_list(pool: &DbPool, user_id: i64, body: &Value) -> CreateOutcome {
    const FAILED: &str = "Failed to create list";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("lists create: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    let name = body.get("name");
    if js_falsy(name) || js_trim(&js_to_string(name)).is_empty() {
        return CreateOutcome::Response(json_error(StatusCode::BAD_REQUEST, "List name is required"));
    }
    // const accent = typeof accent_color === 'string' && accent_color.startsWith('#') ? accent_color : '#10b981';
    let accent = match body.get("accent_color") {
        Some(Value::String(s)) if s.starts_with('#') => s.clone(),
        _ => "#10b981".to_string(),
    };
    // description || '' — falsy (incl. undefined/null) -> '', truthy raw value binds through the driver.
    let desc = match body.get("description") {
        Some(v) if !js_falsy(Some(v)) => js_bind(v),
        _ => SqlValue::Text(String::new()),
    };
    let icon = clean_icon(body.get("icon"));
    let lines = match body.get("list_text") {
        Some(Value::String(t)) => parse_list_text(t),
        _ => Vec::new(),
    };

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return CreateOutcome::Response(fail(&e)),
    };

    // Hydration gate BEFORE the INSERT: any uncached name forwards the whole
    // request, so Node performs the single authoritative INSERT.
    if !lines.is_empty() {
        for l in &lines {
            match find_cached_card(&conn, &l.name) {
                Ok(Some(_)) => {}
                Ok(None) => return CreateOutcome::Forward,
                Err(e) => return CreateOutcome::Response(fail(&e)),
            }
        }
    }

    if let Err(e) = conn.execute(
        "INSERT INTO card_lists (name, description, accent_color, icon, user_id) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            SqlValue::Text(js_trim(&js_to_string(name)).to_string()),
            desc,
            SqlValue::Text(accent),
            icon,
            SqlValue::Integer(user_id),
        ],
    ) {
        return CreateOutcome::Response(fail(&e));
    }
    let list_id = conn.last_insert_rowid();

    let mut matched: i64 = 0;
    let mut unmatched: Vec<Value> = Vec::new();
    for l in &lines {
        let card = match find_cached_card(&conn, &l.name) {
            Ok(c) => c,
            Err(e) => return CreateOutcome::Response(fail(&e)),
        };
        let Some(card) = card else {
            unmatched.push(Value::String(if l.set_ref.is_empty() {
                l.name.clone()
            } else {
                format!("{} ({})", l.name, l.set_ref)
            }));
            continue;
        };
        if let Err(e) = conn.execute(
            "INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?1, ?2, ?3)
             ON CONFLICT(list_id, card_id) DO UPDATE SET quantity = quantity + EXCLUDED.quantity",
            rusqlite::params![
                SqlValue::Integer(list_id),
                json_to_sql_value(&card),
                l.qty.clone(),
            ],
        ) {
            return CreateOutcome::Response(fail(&e));
        }
        matched += 1;
    }
    CreateOutcome::Response(json_response(
        StatusCode::CREATED,
        &json!({ "message": "List created", "id": list_id, "matched": matched,
                 "unmatched": unmatched, "lines": lines.len() }),
    ))
}

// ---------------------------------------------------------------------------
// PUT /order — hand-arranged ordering
// ---------------------------------------------------------------------------

fn reorder_lists(pool: &DbPool, user_id: i64, body: &Value) -> Response {
    const FAILED: &str = "Failed to reorder lists";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("lists order: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    // Array.isArray(req.body?.ids) ? map(Number).filter(Number.isInteger) : []
    let ids: Vec<f64> = match body.get("ids") {
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| js_number(Some(v)))
            .filter(|n| n.is_finite() && n.fract() == 0.0)
            .collect(),
        _ => Vec::new(),
    };
    if ids.is_empty() || ids.len() > 1000 {
        return json_error(StatusCode::BAD_REQUEST, "ids must be 1-1000 list ids");
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    // Node db.withTransaction: BEGIN IMMEDIATE / COMMIT / ROLLBACK-throw.
    let mut tx = match Tx::begin(&conn) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };
    for (i, id) in ids.iter().enumerate() {
        let bound = exact_int(*id).map_or(SqlValue::Real(*id), SqlValue::Integer);
        if let Err(e) = conn.execute(
            "UPDATE card_lists SET sort_order = ?1 WHERE id = ?2 AND user_id = ?3",
            rusqlite::params![SqlValue::Integer(i as i64), bound, SqlValue::Integer(user_id)],
        ) {
            tx.rollback();
            return fail(&e);
        }
    }
    if let Err(e) = tx.commit() {
        tx.rollback();
        return fail(&e);
    }
    json_response(StatusCode::OK, &json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// PUT /:id — meta update
// ---------------------------------------------------------------------------

fn update_list(pool: &DbPool, id: &str, user_id: i64, body: &Value) -> Response {
    const FAILED: &str = "Failed to update list";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("lists update: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    let name = body.get("name");
    if js_falsy(name) || js_trim(&js_to_string(name)).is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "List name is required");
    }
    // accent_color != null && accent_color.startsWith('#') — a non-string
    // non-null value throws TypeError in Node -> catch -> 500.
    let accent = match body.get("accent_color") {
        None | Some(Value::Null) => SqlValue::Null,
        Some(Value::String(s)) => {
            if s.starts_with('#') {
                SqlValue::Text(s.clone())
            } else {
                SqlValue::Null
            }
        }
        Some(_) => return internal_error_fail(FAILED),
    };
    let desc = match body.get("description") {
        Some(v) if !v.is_null() => js_bind(v),
        _ => SqlValue::Text(String::new()),
    };
    let icon = clean_icon(body.get("icon"));
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let changes = match conn.execute(
        "UPDATE card_lists SET name = ?1, description = ?2, accent_color = ?3, icon = ?4
         WHERE id = ?5 AND user_id = ?6",
        rusqlite::params![
            SqlValue::Text(js_trim(&js_to_string(name)).to_string()),
            desc,
            accent,
            icon,
            SqlValue::Text(id.to_string()),
            SqlValue::Integer(user_id),
        ],
    ) {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    if changes == 0 {
        return json_error(StatusCode::NOT_FOUND, "List not found or unauthorized");
    }
    json_response(StatusCode::OK, &json!({ "message": "List updated" }))
}

fn internal_error_fail(message: &'static str) -> Response {
    // Node: the route-level catch answers 500 with the route's own message.
    tracing::error!("lists update: accent_color TypeError twin");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, message)
}

// ---------------------------------------------------------------------------
// DELETE /:id — cascade-twin delete
// ---------------------------------------------------------------------------

fn delete_list(pool: &DbPool, id: &str, user_id: i64) -> Response {
    const FAILED: &str = "Failed to delete list";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("lists delete: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let list = match get_row_map(
        &conn,
        "SELECT id FROM card_lists WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    ) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    if list.is_none() {
        return json_error(StatusCode::NOT_FOUND, "List not found or unauthorized");
    }
    // Node relies on PRAGMA foreign_keys = ON cascade for list_cards; this
    // pool doesn't enforce FKs, so the cascade is mirrored explicitly inside
    // one BEGIN IMMEDIATE (decks_writes delete precedent).
    let mut tx = match Tx::begin(&conn) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };
    if let Err(e) = conn.execute(
        "DELETE FROM list_cards WHERE list_id = (SELECT id FROM card_lists WHERE id = ?1 AND user_id = ?2)",
        rusqlite::params![SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    ) {
        tx.rollback();
        return fail(&e);
    }
    if let Err(e) = conn.execute(
        "DELETE FROM card_lists WHERE id = ?1",
        rusqlite::params![SqlValue::Text(id.to_string())],
    ) {
        tx.rollback();
        return fail(&e);
    }
    if let Err(e) = tx.commit() {
        tx.rollback();
        return fail(&e);
    }
    json_response(StatusCode::OK, &json!({ "message": "List deleted" }))
}

// Keep the Body/IntoResponse imports honest for the proxy rebuild path even
// if future refactors drop one use site.
#[allow(dead_code)]
fn _import_anchors(b: Body) -> Response {
    b.into_response()
}
