//! Rust port of `backend/src/utils/mfxPayload.js` (Lane B unit 3a): the pure
//! Moxfield payload helpers — no DB, no network. Board extraction, the
//! `mtg-<scryfall_id>` card-id mapping, format labels/target sizes, and the
//! synthesized card_cache row for cards Scryfall has never heard of.
//!
//! Byte-parity notes (Node is the spec):
//! - `card.scryfall_id` is a truthiness probe, and `scryboxCardId` coerces it
//!   via a template literal (String(x) semantics — see js_to_string).
//! - `mfxFormatLabel`'s default branch returns `format || 'Commander / EDH'`:
//!   ANY truthy raw value passes through unchanged (including numbers/bools).
//!   Node would bind that raw value into the TEXT column (SQLite stores 5 as
//!   INTEGER); we stringify it (`"5"`) so the column stays TEXT. Divergence is
//!   observable only for non-string formats, which Moxfield never sends.
//! - `synthesizeMoxfieldCard` mirrors JS coercions exactly: `(normal || large)`
//!   falls through on present-but-empty, `Number(c.cmc)` full-numeric parse
//!   (`' 3 '`→3, `true`→1, `false`→0, `''`→0, `'3abc'`→NaN→null).

use serde_json::{Map, Value};

/// Boards mirrored into deck_cards. Commander decks put their commander(s)
/// in `commanders`, not the mainboard — a 99+1 layout — so they mirror into
/// the same 100-card slot.
pub const MIRROR_BOARDS: [&str; 3] = ["mainboard", "commanders", "sideboard"];

/// One entry of the flat ordered list the sync works on: which board it came
/// from, the raw Moxfield card block, and the quantity.
#[derive(Clone, Debug)]
pub struct BoardEntry {
    pub board: &'static str,
    pub card: Value,
    pub quantity: i64,
}

/// JS truthiness for JSON values (`if (!card || !card.scryfall_id)`, `||`
/// chains): null/false/0/-0/0.0/"" are falsy; everything else truthy.
pub fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => {
            n.as_f64().map(|f| f != 0.0).unwrap_or(true) // big ints are truthy
        }
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// JS `String(x)` over JSON values. Numbers use JS Number→String formatting
/// (integral f64 prints without a decimal point); arrays/objects approximate
/// `Array.prototype.join` / `"[object Object]"` (Moxfield payloads never carry
/// these where String() runs, except trivially).
pub fn js_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return i.to_string();
            }
            if let Some(u) = n.as_u64() {
                return u.to_string();
            }
            if let Some(f) = n.as_f64() {
                return js_number_to_string(f);
            }
            n.to_string()
        }
        Value::Array(a) => a.iter().map(js_to_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS `Number(x)` over JSON values. Returns None for NaN (and for null /
/// absent, which callers must have excluded via `!= null` — Number(null) is
/// 0 but `x != null` already rejected it upstream).
pub fn js_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0), // unreachable under the `!= null` guard
        Value::String(s) => js_string_to_number(s),
        // Number([]) === 0; Number([x]) === Number(x); multi-element arrays
        // and objects coerce to NaN.
        Value::Array(a) => match a.as_slice() {
            [] => Some(0.0),
            [only] => js_number(only),
            _ => None,
        },
        Value::Object(_) => None,
    }
}

/// JS `Number(str)`: trims whitespace, accepts the full numeric grammar
/// (sign, decimal, exponent, 0x hex, Infinity); anything else is NaN.
fn js_string_to_number(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return Some(0.0); // Number('') === 0
    }
    let lower = t.to_lowercase();
    if lower == "infinity" || lower == "+infinity" {
        return None; // JS gives Infinity; not a finite cmc — treat as NaN-safe None
    }
    if lower == "-infinity" {
        return None;
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).ok().map(|v| v as f64);
    }
    // Rust's parse::<f64> accepts "inf"/"nan"/"1.5e3" — reject the non-JS
    // spellings so '3abc', 'inf', 'NaN' are NaN like JS.
    if ["inf", "nan", "infinity", "+inf", "-inf"].contains(&lower.as_str()) {
        return None;
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// JS Number→String for non-integral f64 (integral values never reach here).
pub fn js_number_to_string(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e21 {
        // Integral, but not representable as i64/u64 (caller fell through).
        return format!("{:.0}", f);
    }
    // Rust's Display for f64 matches JS for the common JSON cases (shortest
    // round-trip form; 'e' notation at |x| >= 1e21 or < 1e-6 like JS).
    let s = f.to_string();
    s
}

/// JS `parseInt(v, 10)`: strings parse the leading signed integer run
/// (stops at the first non-digit); numbers are first String()'d (JS parseInt
/// operates on the string form), then the same leading-run parse; anything
/// else (bool/null/object/undecodable string) is NaN.
pub fn js_parse_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => {
            let s = js_to_string(&Value::Number(n.clone()));
            parse_int_radix10(&s)
        }
        Value::String(s) => parse_int_radix10(s),
        _ => None,
    }
}

fn parse_int_radix10(t: &str) -> Option<i64> {
    let t = t.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (sign, digits) = match t.as_bytes().first() {
        Some(b'+') => (1i64, &t[1..]),
        Some(b'-') => (-1i64, &t[1..]),
        _ => (1i64, t),
    };
    let run: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    if run.is_empty() {
        return None; // NaN
    }
    run.parse::<i64>().ok().map(|n| sign * n)
}

/// `Math.max(0, parseInt(entry.quantity, 10) || 0)` from extractDeckCards.
fn entry_quantity(v: Option<&Value>) -> i64 {
    js_parse_int(v).filter(|n| *n != 0).unwrap_or(0).max(0)
}

fn board_cards<'a>(details: Option<&'a Value>, board_name: &str) -> Option<Vec<&'a Value>> {
    let board = details?.get("boards")?.get(board_name)?;
    let cards = board.get("cards")?;
    // `if (!cards || typeof cards !== 'object') continue;` — arrays pass the
    // typeof check and Object.values() walks them in index order.
    if let Some(map) = cards.as_object() {
        Some(map.values().collect())
    } else if let Some(arr) = cards.as_array() {
        Some(arr.iter().collect())
    } else {
        None
    }
}

fn extract_board(details: Option<&Value>, board_name: &'static str) -> Vec<BoardEntry> {
    let Some(entries) = board_cards(details, board_name) else {
        return Vec::new();
    };
    // Object.values order = JS insertion order (Moxfield's cardId keys are
    // non-numeric UUIDs, so no integer-key reordering); serde_json is built
    // with preserve_order for exactly this.
    let mut out = Vec::new();
    for entry in entries {
        // `const card = entry && entry.card;` — non-object entries yield no
        // `card` member and are skipped.
        let card = match entry {
            Value::Object(m) => match m.get("card") {
                Some(c) if c.is_object() => c,
                _ => continue,
            },
            _ => continue,
        };
        // `if (!card || !card.scryfall_id) continue;` — falsy scryfall_id
        // (missing, null, "", 0, false) skips; truthy non-strings pass.
        if !js_truthy(card.get("scryfall_id")) {
            continue;
        }
        let quantity = entry_quantity(entry.get("quantity"));
        if quantity == 0 {
            continue;
        }
        out.push(BoardEntry { board: board_name, card: card.clone(), quantity });
    }
    out
}

/// Flat ordered list over mainboard/commanders/sideboard (never maybeboard).
pub fn extract_deck_cards(details: &Value) -> Vec<BoardEntry> {
    let mut out = Vec::new();
    for b in MIRROR_BOARDS {
        out.extend(extract_board(Some(details), b));
    }
    out
}

/// Maybeboard entries, kept apart so they never reach deck_cards.
pub fn extract_maybe_cards(details: &Value) -> Vec<BoardEntry> {
    extract_board(Some(details), "maybeboard")
}

/// Sum of quantities per board, for the summary columns.
pub fn board_counts(entries: &[BoardEntry]) -> [i64; 4] {
    let mut counts = [0i64; 4]; // mainboard, sideboard, maybeboard, commanders
    for e in entries {
        match e.board {
            "mainboard" => counts[0] += e.quantity,
            "sideboard" => counts[1] += e.quantity,
            "maybeboard" => counts[2] += e.quantity,
            "commanders" => counts[3] += e.quantity,
            _ => {}
        }
    }
    counts
}

/// Map a Moxfield card onto Scrybox's card_cache id: `mtg-${card.scryfall_id}`
/// — a template literal, so a non-string truthy scryfall_id coerces via
/// String() (number 123 → "mtg-123"), never to bare "mtg-".
pub fn scrybox_card_id(card: &Value) -> String {
    match card {
        Value::Object(m) => match m.get("scryfall_id") {
            Some(v) if !v.is_null() => format!("mtg-{}", js_to_string(v)),
            _ => "mtg-".to_string(),
        },
        _ => "mtg-".to_string(),
    }
}

/// `String(format || '')` from mfxPayload.js: falsy formats (null/absent/""/
/// 0/false) become ""; truthy values go through String() first.
fn format_key(format: Option<&Value>) -> String {
    match format {
        Some(v) if js_truthy(Some(v)) => js_to_string(v),
        _ => String::new(),
    }
}

/// Moxfield's format string → the label the deck builder stores.
pub fn mfx_format_label(format: Option<&Value>) -> String {
    match format_key(format).to_lowercase().as_str() {
        "commander" | "edh" | "oathbreaker" => "Commander / EDH".to_string(),
        "standard" => "Standard".to_string(),
        "modern" => "Modern".to_string(),
        "pioneer" => "Pioneer".to_string(),
        "legacy" => "Legacy".to_string(),
        "vintage" => "Vintage".to_string(),
        "pauper" => "Pauper".to_string(),
        _ => {
            // `default: return format || 'Commander / EDH'` — the RAW value
            // passes through when truthy (stringified here; see module docs),
            // 'Commander / EDH' when falsy.
            match format {
                Some(v) if js_truthy(Some(v)) => js_to_string(v),
                _ => "Commander / EDH".to_string(),
            }
        }
    }
}

/// Constructed formats with a sideboard get 75, everything else (commander) 100.
pub fn target_size_for_format(format: Option<&Value>) -> i64 {
    if ["standard", "modern", "pioneer", "legacy", "vintage"]
        .contains(&format_key(format).to_lowercase().as_str())
    {
        75
    } else {
        100
    }
}

const COLOR_NAMES: [(&str, &str); 5] = [
    ("W", "White"),
    ("U", "Blue"),
    ("B", "Black"),
    ("R", "Red"),
    ("G", "Green"),
];

/// `colors.map(x => COLOR_NAMES[x] || x)` — non-string entries index to
/// undefined and pass through (stringified for the TEXT column).
fn color_words(colors: Option<&Value>) -> Vec<String> {
    let arr = match colors {
        Some(Value::Array(a)) => a,
        _ => return Vec::new(), // Array.isArray guard
    };
    arr.iter()
        .map(|x| {
            let key = js_to_string(x);
            COLOR_NAMES
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, n)| n.to_string())
                .unwrap_or(key)
        })
        .collect()
}

/// The shape `scryfallApi.normalizeCard` produces, written straight through
/// `cacheCards`. `price_source='moxfield'` marks the row as not-a-real-Scryfall
/// card for the price sweep and any other Scryfall-keyed feature.
pub fn synthesize_moxfield_card(scrybox_id: &str, card: &Value) -> Map<String, Value> {
    let c = card;
    // `c.type_line || ''` — falsy becomes "".
    let type_line = match c.get("type_line") {
        Some(v) if js_truthy(Some(v)) => js_to_string(v),
        _ => String::new(),
    };
    // `typeLine.split(/[^A-Za-z]+/).filter(Boolean)` — ASCII letters only
    // (JS [^A-Za-z] drops non-ASCII letters too).
    let subtypes: Vec<String> = type_line
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let color_words = color_words(c.get("colors"));
    // `(c.image_uris && (c.image_uris.normal || c.image_uris.large)) || ''` —
    // present-but-empty `normal` falls through to `large`.
    let image = match c.get("image_uris") {
        Some(iu) if js_truthy(Some(iu)) => {
            let normal = iu.get("normal");
            let large = iu.get("large");
            if js_truthy(normal) {
                js_to_string(normal.unwrap())
            } else if js_truthy(large) {
                js_to_string(large.unwrap())
            } else {
                String::new()
            }
        }
        _ => String::new(),
    };
    // `c.cmc != null && !Number.isNaN(Number(c.cmc)) ? Number(c.cmc) : null`
    let cmc = match c.get("cmc") {
        Some(v) if !v.is_null() => js_number(v).map(Value::from),
        _ => None,
    };
    // `String(c.cn != null ? c.cn : '')`
    let number = match c.get("cn") {
        Some(v) if !v.is_null() => js_to_string(v),
        _ => String::new(),
    };
    let mut m = Map::new();
    m.insert("id".into(), Value::String(scrybox_id.to_string()));
    // `c.name || 'Unknown'`
    m.insert(
        "name".into(),
        match c.get("name") {
            Some(v) if js_truthy(Some(v)) => Value::String(js_to_string(v)),
            _ => Value::String("Unknown".into()),
        },
    );
    m.insert("supertype".into(), Value::String("MTG".into()));
    m.insert("subtypes".into(), Value::Array(subtypes.into_iter().map(Value::String).collect()));
    m.insert("types".into(), Value::Array(color_words.clone().into_iter().map(Value::String).collect()));
    m.insert("rarity".into(), Value::String("Common".into()));
    // `c.set || ''` / `c.set_name || ''`
    m.insert(
        "set_id".into(),
        match c.get("set") {
            Some(v) if js_truthy(Some(v)) => Value::String(js_to_string(v)),
            _ => Value::String(String::new()),
        },
    );
    m.insert(
        "set_name".into(),
        match c.get("set_name") {
            Some(v) if js_truthy(Some(v)) => Value::String(js_to_string(v)),
            _ => Value::String(String::new()),
        },
    );
    m.insert("number".into(), Value::String(number));
    m.insert("image_url".into(), Value::String(image));
    m.insert("price_trend".into(), Value::from(0));
    m.insert("price_normal".into(), Value::Null);
    m.insert("price_holofoil".into(), Value::Null);
    m.insert("cmc".into(), cmc.unwrap_or(Value::Null));
    m.insert("color_identity".into(), Value::Array(color_words.into_iter().map(Value::String).collect()));
    m.insert("language".into(), Value::String("English".into()));
    m.insert("printed_name".into(), Value::Null);
    m.insert("tcgplayer_url".into(), Value::Null);
    m.insert("cardmarket_url".into(), Value::Null);
    m.insert("tcgplayer_product_id".into(), Value::Null);
    m.insert("price_currency".into(), Value::String("USD".into()));
    m.insert("price_source".into(), Value::String("moxfield".into()));
    m
}
