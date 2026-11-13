//! POST /api/limited/analyze — the Limited Lands analyzer (routes/limited.js).
//!
//! LANE-A iter-48 port. Node mounts `router.post('/analyze')` under
//! /api/limited behind the global authenticateToken (server.js:454/478); the
//! OTHER methods do not match the Express router and Node answers with its
//! final-handler 404 — so non-POST rides the proxy verbatim.
//!
//! Middleware order for this route (probe-verified against :3312):
//!   1. the GLOBAL normalJson parser (server.js:205-211, limit '1mb',
//!      npm-mb = 1024^2) runs BEFORE authenticateToken: malformed JSON
//!      answers the global handler's 500 and entity.too.large its 413, both
//!      WITHOUT a valid token.
//!   2. authenticateToken (POST → API keys are GET-only reads → 403).
//!   3. invalidateOnWrite (stats bump on every authenticated non-GET).
//!   4. the route body: 413 >20000 UTF-16 units, 400 no entries, 400 >120
//!      distinct, then BATCH=75 chunks through scryPostRetried (4 attempts,
//!      429 re-queue only) and analyzeCards; any scry throw → 502.
//!
//! With LOCAL_SCRYFALL_ENABLED the axios baseURL swap lands the lookups on
//! the scryfall-local sidecar (no pacing), mirrored by Scry::resolve — the
//! parity harness therefore exercises the same data source on both sides.

use std::collections::HashMap;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
};
use serde_json::{json, Map, Value};

use crate::collection_writes::{json_response, read_parts};
use crate::scryfall::ScryOpts;
use crate::settings::authenticate;
use crate::AppState;

/// express.json({limit:'1mb'}) — normalJson (server.js:205). npm `mb` = 1024^2.
const NORMAL_JSON_LIMIT: usize = 1024 * 1024;
/// routes/limited.js BATCH.
const BATCH: usize = 75;

// ---------------------------------------------------------------------------
// JS-string primitives (limitedMana.js works on raw JS strings)
// ---------------------------------------------------------------------------

/// JS WhiteSpace ∪ LineTerminator for String.prototype.trim — notably
/// INCLUDES U+FEFF and EXCLUDES U+0085, unlike Rust's char::is_whitespace.
fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{b}'
            | '\u{c}'
            | '\r'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_ws)
}

/// JS's String(value) for the JSON shapes a Scryfall card can carry.
fn js_string_of(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                if f == 0.0 {
                    "0".to_string() // +0/-0 both stringify "0"
                } else if f.is_nan() {
                    "NaN".to_string()
                } else if f.fract() == 0.0 && f.abs() < 1e21 {
                    format!("{}", f as i64)
                } else {
                    format!("{f}")
                }
            } else {
                n.to_string()
            }
        }
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// JS truthiness on a JSON value (the `a || b || ''` chains in limitedMana).
fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `String(face.x || card.x || '')` for the type_line/oracle_text chains.
fn string_or_chain(face: &Value, card: &Value, key: &str) -> String {
    for src in [face, card] {
        let v = src.get(key);
        if js_truthy(v) {
            return js_string_of(v);
        }
    }
    String::new()
}

/// ASCII word character set for hand-ported \b (JS \w is ASCII-only).
fn is_word_ascii(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `/\bWord\b/.test(hay)` with JS's ASCII word-boundary semantics.
fn word_test(hay: &str, word: &str) -> bool {
    hay.match_indices(word).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + word.len()..].chars().next();
        !before.is_some_and(is_word_ascii) && !after.is_some_and(is_word_ascii)
    })
}

// ---------------------------------------------------------------------------
// parseDecklist (utils/limitedMana.js)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct DeckEntry {
    /// Name as FIRST seen (what goes to /cards/collection).
    pub name: String,
    pub quantity: i64,
}

/// JS \s as an explicit class for the regex crate (includes FEFF, excludes
/// NEL — the crate's \s does the opposite on those two).
const JS_WS_CLASS: &str = r"[\x{9}\x{a}\x{b}\x{c}\x{d}\x{20}\x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]";

fn deck_qty_re() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        // /^(\d{1,3})\s*x?\s+(.+)$/i — \d is ASCII in JS; the dot excludes
        // the four line terminators; the regex crate's leftmost-first
        // preference order matches the JS backtracking engine's.
        regex::Regex::new(&format!(
            r"(?i)\A([0-9]{{1,3}}){ws}*x?{ws}+([^\n\r\x{{2028}}\x{{2029}}]+)\z",
            ws = JS_WS_CLASS
        ))
        .unwrap()
    })
}

fn set_code_re() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        // /\s+\([A-Z0-9]{2,6}\)\s*[\w-]*\s*$/i — JS \w is ASCII [0-9A-Za-z_].
        regex::Regex::new(&format!(
            r"(?i){ws}+\(([A-Za-z0-9]{{2,6}})\){ws}*([0-9A-Za-z_-]*){ws}*\z",
            ws = JS_WS_CLASS
        ))
        .unwrap()
    })
}

fn foil_re() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        // /\s+\*[^*]+\*\s*$/
        regex::Regex::new(&format!(r"{ws}+\*[^*]+\*{ws}*\z", ws = JS_WS_CLASS)).unwrap()
    })
}

fn section_re() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        // /^(sideboard|maybeboard|commander|companion|tokens?)\b/i — the \b
        // checked by hand: JS's \w is ASCII, the crate's \b is Unicode.
        regex::Regex::new(r"(?i)\A(sideboard|maybeboard|commander|companion|tokens?)").unwrap()
    })
}

/// utils/limitedMana.js parseDecklist, faithful line-by-line.
pub fn parse_decklist(text: &str) -> Vec<DeckEntry> {
    let mut out: Vec<DeckEntry> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut seen_card = false;
    // text.split(/\r?\n/): lone \r never splits; the \r of a \r\n pair is
    // consumed by the split, a mid-line \r survives into the line.
    for raw_line in text.split('\n') {
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let line = js_trim(raw_line);
        if line.is_empty() {
            if seen_card {
                break; // first blank line after cards ends the deck
            }
            continue;
        }
        if line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        if line.eq_ignore_ascii_case("deck") {
            continue;
        }
        // /^main(board)?:?$/i
        let low = line.to_ascii_lowercase();
        if low == "main" || low == "main:" || low == "mainboard" || low == "mainboard:" {
            continue;
        }
        if let Some(m) = section_re().find(line) {
            let after = line[m.end()..].chars().next();
            if !after.is_some_and(is_word_ascii) {
                break;
            }
        }
        let (quantity, name_src) = match deck_qty_re().captures(line) {
            Some(caps) => {
                let q: i64 = caps[1].parse().unwrap_or(1);
                (q, caps[2].to_string())
            }
            None => (1, line.to_string()),
        };
        let mut name = name_src;
        name = set_code_re().replace(&name, "").to_string();
        name = foil_re().replace(&name, "").to_string();
        name = js_trim(&name).to_string();
        if name.is_empty() || quantity < 1 {
            continue; // before seen_card — a skipped line never starts the deck
        }
        seen_card = true;
        let key = name.to_lowercase();
        if let Some(&i) = index.get(&key) {
            out[i].quantity += quantity;
        } else {
            index.insert(key, out.len());
            out.push(DeckEntry { name, quantity });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// costPips / analyzeCards (utils/limitedMana.js)
// ---------------------------------------------------------------------------

const COLOR_IDS: [&str; 5] = ["W", "U", "B", "R", "G"];

fn pip_index(id: &str) -> Option<usize> {
    match id {
        "W" => Some(0),
        "U" => Some(1),
        "B" => Some(2),
        "R" => Some(3),
        "G" => Some(4),
        "C" => Some(5),
        _ => None,
    }
}

fn cost_sym_re() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"\{([^}]+)\}").unwrap())
}

/// utils/limitedMana.js costPips over the already-String()ed cost. Hybrid
/// {W/U} counts half to each color, Phyrexian {B/P} counts whole to its
/// color, {2/W} counts whole to the color, {C} is colorless; generic
/// numbers, {X} and {S} are ignored.
pub fn cost_pips(cost: &str) -> [f64; 6] {
    let mut pips = [0.0f64; 6];
    for caps in cost_sym_re().captures_iter(cost) {
        let upper = caps[1].to_uppercase();
        // split('/').filter(p => p !== 'P')
        let parts: Vec<&str> = upper.split('/').filter(|p| *p != "P").collect();
        let colors: Vec<&str> = parts.iter().copied().filter(|p| COLOR_IDS.contains(p)).collect();
        if upper == "C" {
            pips[5] += 1.0;
        } else if !colors.is_empty() {
            for c in &colors {
                pips[pip_index(c).unwrap()] += 1.0 / colors.len() as f64;
            }
        }
    }
    pips
}

fn basics_lookup(name_lower: &str) -> Option<usize> {
    Some(match name_lower {
        "plains" | "snow-covered plains" => 0,
        "island" | "snow-covered island" => 1,
        "swamp" | "snow-covered swamp" => 2,
        "mountain" | "snow-covered mountain" => 3,
        "forest" | "snow-covered forest" => 4,
        "wastes" => 5,
        _ => return None,
    })
}

/// utils/limitedMana.js analyzeCards over resolved (quantity, raw) pairs,
/// returning the res.json payload WITHOUT `notFound` (the route appends it —
/// insertion order matches Node's `{ ...analyzeCards(cards), notFound }`).
pub fn analyze_cards(cards: &[(i64, Value)]) -> Map<String, Value> {
    let mut pips = [0.0f64; 6];
    let mut color_cards = [0i64; 6];
    let mut basics = [0i64; 6];
    let mut lands: Vec<Value> = Vec::new();
    let mut spells: i64 = 0;

    for (quantity, card) in cards {
        // frontFace: card_faces non-empty array → [0], else the card itself.
        let face: &Value = match card.get("card_faces") {
            Some(Value::Array(a)) if !a.is_empty() => &a[0],
            _ => card,
        };
        let type_line = string_or_chain(face, card, "type_line");
        let is_land = word_test(&type_line, "Land");
        let name_lower = js_string_of(card.get("name")).to_lowercase();
        if let Some(bi) = basics_lookup(&name_lower) {
            if is_land {
                basics[bi] += quantity;
                continue;
            }
        }
        if is_land {
            let text = string_or_chain(face, card, "oracle_text");
            let text_low = text.to_lowercase();
            // produced_mana filtered to WUBRGC (strict === includes), then
            // [...new Set(…)] — first-occurrence order.
            let mut produces: Vec<String> = Vec::new();
            if let Some(Value::Array(pm)) = card.get("produced_mana") {
                for p in pm {
                    if let Value::String(s) = p {
                        if pip_index(s).is_some() && !produces.iter().any(|q| q == s) {
                            produces.push(s.clone());
                        }
                    }
                }
            }
            // /enters (the battlefield )?tapped/i && !/unless/i
            let tapped = (text_low.contains("enters tapped")
                || text_low.contains("enters the battlefield tapped"))
                && !text_low.contains("unless");
            let conditional = text_low.contains("spend this mana only")
                || text_low.contains("search your library");
            lands.push(json!({
                "name": card.get("name").cloned().unwrap_or(Value::Null),
                "quantity": quantity,
                "produces": produces,
                "tapped": tapped,
                "conditional": conditional,
            }));
            continue;
        }
        spells += quantity;
        // face.mana_cost != null ? face.mana_cost : card.mana_cost
        let mc_val = match face.get("mana_cost") {
            Some(Value::Null) | None => card.get("mana_cost"),
            Some(v) => Some(v),
        };
        // costPips(String(cost || '')) — falsy costs stringify to ''.
        let mc_str = if js_truthy(mc_val) { js_string_of(mc_val) } else { String::new() };
        let p = cost_pips(&mc_str);
        for id in 0..6 {
            pips[id] += p[id] * (*quantity as f64);
            if p[id] > 0.0 {
                color_cards[id] += quantity;
            }
        }
    }

    let mut pips_out = Map::new();
    let mut cc_out = Map::new();
    let mut basics_out = Map::new();
    for (i, id) in ["W", "U", "B", "R", "G", "C"].iter().enumerate() {
        // Math.round over non-negative sums == Rust's round (half away from
        // zero agrees with JS's half-up for values >= 0) — and JSON.stringify
        // of an integral f64 never shows ".0", so emit i64.
        pips_out.insert((*id).to_string(), json!(pips[i].round() as i64));
        cc_out.insert((*id).to_string(), json!(color_cards[i]));
        basics_out.insert((*id).to_string(), json!(basics[i]));
    }
    let basic_count: i64 = basics.iter().sum();
    let land_count = basic_count
        + lands
            .iter()
            .filter_map(|l| l.get("quantity").and_then(Value::as_i64))
            .sum::<i64>();

    let mut out = Map::new();
    out.insert("pips".into(), Value::Object(pips_out));
    out.insert("colorCards".into(), Value::Object(cc_out));
    out.insert("basics".into(), Value::Object(basics_out));
    out.insert("lands".into(), Value::Array(lands));
    out.insert("spells".into(), json!(spells));
    out.insert("landCount".into(), json!(land_count));
    out.insert("basicCount".into(), json!(basic_count));
    out.insert("deckSize".into(), json!(spells + land_count));
    out
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// normalJson twin with body-parser's type check (application/json AND
/// application/*+json) and strict:true top level (objects/arrays only).
enum BodyOutcome {
    Parsed(Map<String, Value>),
    Invalid,
    TooLarge,
}

fn parse_normal_json(headers: &HeaderMap, bytes: &[u8]) -> BodyOutcome {
    let ct_ok = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            main == "application/json"
                || (main.starts_with("application/") && main.ends_with("+json"))
        })
        .unwrap_or(false);
    if !ct_ok {
        // Parser skipped: req.body stays undefined → `req.body && …` is falsy.
        return BodyOutcome::Parsed(Map::new());
    }
    if bytes.len() > NORMAL_JSON_LIMIT {
        return BodyOutcome::TooLarge;
    }
    if bytes.is_empty() {
        return BodyOutcome::Parsed(Map::new());
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(m)) => BodyOutcome::Parsed(m),
        // strict:true allows arrays; body.list is undefined on one — same
        // behavior as an object without the key.
        Ok(Value::Array(_)) => BodyOutcome::Parsed(Map::new()),
        _ => BodyOutcome::Invalid,
    }
}

fn lookup_failed() -> Response {
    json_response(
        StatusCode::BAD_GATEWAY,
        &json!({ "error": "Card lookup failed. Try again in a moment." }),
    )
}

pub async fn handle(
    method: Method,
    State(st): State<AppState>,
    cc: ConnectInfo<std::net::SocketAddr>,
    req: Request,
) -> Response {
    // No limiter on this route (Node mounts none under /api/limited); the
    // ConnectInfo extraction is kept only so the handler slots into the same
    // router wiring as its rate-limited siblings.
    let _ = &cc;
    if method != Method::POST {
        // Express router.post does not match → Node's final handler answers.
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st), req).await;
    };
    let (parts, bytes) = read_parts(req).await;
    let headers: HeaderMap = parts.headers;

    // (1) normalJson runs BEFORE the auth gate (server.js:207-211): probe-
    // verified — malformed JSON 500s and >1mb 413s with no token at all.
    let body = match parse_normal_json(&headers, &bytes) {
        BodyOutcome::Parsed(m) => m,
        BodyOutcome::Invalid => {
            tracing::error!("limited analyze: invalid JSON body (express.json strict)");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "Internal server error" }),
            );
        }
        BodyOutcome::TooLarge => {
            return json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &json!({ "error": "Upload too large. Try exporting/importing in smaller batches." }),
            )
        }
    };

    // (2) authenticateToken — POST kills the API-key leg (is_get=false).
    if let Err(res) = authenticate(&pool, &headers, false) {
        return res;
    }
    // (3) invalidateOnWrite: every authenticated non-GET bumps the user's
    // stats gen (utils/statsCache.js:60-63); the twin re-authenticates.
    crate::stats::on_proxied_write(&st, &Method::POST, "/api/limited/analyze", &headers);

    // (4) route body (routes/limited.js).
    let text: String = match body.get("list") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(), // typeof req.body.list !== 'string' → ''
    };
    if text.encode_utf16().count() > 20000 {
        return json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            &json!({ "error": "Decklist is too long" }),
        );
    }
    let entries = parse_decklist(&text);
    if entries.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "No cards found in the list" }),
        );
    }
    if entries.len() > 120 {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "Too many distinct cards (max 120)" }),
        );
    }

    let mut found: HashMap<String, Value> = HashMap::new();
    let mut not_found: Vec<Value> = Vec::new();
    let opts = ScryOpts::default();
    for chunk in entries.chunks(BATCH) {
        let identifiers: Vec<Value> =
            chunk.iter().map(|e| json!({ "name": e.name })).collect();
        let resp = match st
            .scry
            .post_retried("/cards/collection", json!({ "identifiers": identifiers }), &opts, 4)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Node logs err.message and answers the generic 502.
                tracing::error!("limited analyze failed {}", e);
                return lookup_failed();
            }
        };
        // Node guards with `(resp.data && resp.data.data) || []`: a
        // non-JSON body (None here) reads as no keys; a NON-iterable
        // data/not_found throws inside the route's try → the same 502.
        let obj: Option<&Map<String, Value>> = resp.as_ref().and_then(Value::as_object);
        match obj.and_then(|o| o.get("data")) {
            None | Some(Value::Null) => {}
            Some(Value::Array(arr)) => {
                for raw in arr {
                    let full = js_string_of(raw.get("name"));
                    found.insert(full.to_lowercase(), raw.clone());
                    // Double-faced cards answer to their front face name too.
                    let front = full.split(" // ").next().unwrap_or("").to_lowercase();
                    found.entry(front).or_insert_with(|| raw.clone());
                }
            }
            // for..of over a string iterates code points; raw.name on a char
            // is undefined → every char lands under the "undefined" key
            // (last wins). No decklist entry ever resolves through it.
            Some(Value::String(s)) => {
                if !s.is_empty() {
                    found.insert("undefined".to_string(), Value::Null);
                }
            }
            Some(_) => return lookup_failed(),
        }
        match obj.and_then(|o| o.get("not_found")) {
            None | Some(Value::Null) => {}
            Some(Value::Array(arr)) => {
                for item in arr {
                    // nf.name — absent → undefined → null through stringify.
                    not_found.push(item.get("name").cloned().unwrap_or(Value::Null));
                }
            }
            Some(Value::String(s)) => {
                for _ in s.chars() {
                    not_found.push(Value::Null);
                }
            }
            Some(_) => return lookup_failed(),
        }
    }

    let mut cards: Vec<(i64, Value)> = Vec::new();
    for e in &entries {
        if let Some(raw) = found.get(&e.name.to_lowercase()) {
            cards.push((e.quantity, raw.clone()));
        }
    }

    let mut out = analyze_cards(&cards);
    out.insert("notFound".into(), Value::Array(not_found));
    // res.json twin: application/json; charset=utf-8 + Express's default weak
    // body ETag (POST never replays as 304 — revalidation is GET/HEAD-only).
    json_response(StatusCode::OK, &Value::Object(out))
}
