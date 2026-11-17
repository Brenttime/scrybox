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
/// (`js_number_to_string`); arrays follow `Array.prototype.toString` =
/// `join(",")` where null renders empty and nested arrays recurse; objects
/// are `"[object Object]"`.
pub fn js_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            // JS JSON.parse (and the Node sqlite driver) yield IEEE-754
            // doubles, so exact-int u64/i64 serde variants beyond 2^53 must
            // print as the *rounded double*, not the exact integer — always
            // format through f64 (String(18446744073709551615) ===
            // "18446744073709552000" in V8).
            match n.as_f64() {
                Some(f) => js_number_to_string(f),
                None => n.to_string(),
            }
        }
        Value::Array(a) => a.iter().map(js_array_join_elem).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// `Array.prototype.join(",")` element rendering: null (and undefined, which
/// cannot appear in JSON) render as the empty string; nested arrays recurse;
/// everything else goes through String().
fn js_array_join_elem(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Array(inner) => inner.iter().map(js_array_join_elem).collect::<Vec<_>>().join(","),
        other => js_to_string(other),
    }
}

/// JS `String(x)` where an absent property (undefined) prints as "undefined".
pub fn js_to_string_opt(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(x) => js_to_string(x),
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

/// JS Number→String per ECMA-262 `Number::toString` (shortest round-trip
/// decimal digits s with exponent n: fixed notation for -6 < n <= 21,
/// exponential `d(.ddd)e±XX` otherwise; -0 prints "0"; NaN/Infinity spelled
/// JS-style). The shortest digits come from serde_json's ryu serializer —
/// its digit selection matches V8's `String()` on ties where Rust's own
/// `{:e}` rounds differently (e.g. the double -873199428122901.25 prints
/// "…01.2" in V8, "…01.3" from `{:e}`; fuzz-verified against node).
pub fn js_number_to_string(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if f == 0.0 {
        return "0".to_string(); // covers -0 (String(-0) === "0")
    }
    let neg = f < 0.0;
    let a = f.abs();
    let sj = serde_json::Number::from_f64(a)
        .map(|n| n.to_string())
        .unwrap_or_else(|| a.to_string());
    let (mant, exp) = match sj.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (sj.as_str(), 0),
    };
    let frac_len = mant.split_once('.').map(|(_, fr)| fr.len() as i32).unwrap_or(0);
    let digits_raw: String = mant.chars().filter(|c| *c != '.').collect();
    let digits = digits_raw.trim_end_matches('0'); // ryu's shortest form keeps one "0" for integral values
    let digits = if digits.is_empty() { "0" } else { digits };
    let k = digits.len() as i32;
    let n = exp + digits_raw.len() as i32 - frac_len; // ECMAScript's decimal-point position
    let body = if n > 0 && n <= 21 {
        if n >= k {
            format!("{}{}", digits, "0".repeat((n - k) as usize))
        } else {
            format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
        }
    } else if n <= 0 && n > -6 {
        format!("0.{}{}", "0".repeat((-n) as usize), digits)
    } else {
        let e = n - 1;
        let m = if k > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            digits.to_string()
        };
        format!("{}e{}{}", m, if e < 0 { "-" } else { "+" }, e.abs())
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
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
    crate::util::js_parse_int_unicode(t)
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
    // exec-sync L1: quantities are parseInt of untrusted JSON; saturate the
    // summary sums instead of wrapping (release builds have no overflow
    // checks; Node's float sum differs only in these absurd cases).
    for e in entries {
        match e.board {
            "mainboard" => counts[0] = counts[0].saturating_add(e.quantity),
            "sideboard" => counts[1] = counts[1].saturating_add(e.quantity),
            "maybeboard" => counts[2] = counts[2].saturating_add(e.quantity),
            "commanders" => counts[3] = counts[3].saturating_add(e.quantity),
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
            // errors-maint L1: bind in the truthy check instead of
            // js_truthy(x) + x.unwrap() (same semantics, no panic edge).
            // `(c.image_uris && (c.image_uris.normal || c.image_uris.large)) || ''` —
            // present-but-empty `normal` falls through to `large`.
            if let Some(n) = iu.get("normal").filter(|v| js_truthy(Some(v))) {
                js_to_string(n)
            } else if let Some(l) = iu.get("large").filter(|v| js_truthy(Some(v))) {
                js_to_string(l)
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

// ---------------------------------------------------------------------------
// Value-kind matrix for JS `String(x)` over JSON values.
//
// Ground truth captured from Node (the reference backend runs V8):
//   node -e 'for (const [n,v] of VALS) console.log(JSON.stringify(n)+"\t"+String(v))'
// Values are written as JSON *text* through serde_json::from_str where the
// f64 round-trip matters (18446744073709551615, 5.0), matching how request
// bodies actually arrive.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod js_string_matrix_tests {
    use super::{js_to_string, js_to_string_opt};
    use serde_json::{json, Value};

    fn v(text: &str) -> Value {
        serde_json::from_str(text).expect("matrix literal must parse")
    }

    #[test]
    fn string_kinds() {
        assert_eq!(js_to_string(&json!("abc")), "abc");
        assert_eq!(js_to_string(&json!("")), "");
        assert_eq!(js_to_string(&json!(" ")), " ");
    }

    #[test]
    fn bool_and_null_kinds() {
        assert_eq!(js_to_string(&json!(true)), "true");
        assert_eq!(js_to_string(&json!(false)), "false");
        assert_eq!(js_to_string(&json!(null)), "null");
    }

    #[test]
    fn number_kinds_match_v8() {
        // (JSON literal, String(literal) as printed by node)
        let cases = [
            ("0", "0"),
            ("-0", "0"),
            ("5", "5"),
            ("-5", "-5"),
            ("9007199254740991", "9007199254740991"),
            ("10000000000000000000", "10000000000000000000"),
            ("18446744073709551615", "18446744073709552000"),
            ("100000000000000000000", "100000000000000000000"),
            ("1e21", "1e+21"),
            ("1000000000000000000000", "1e+21"),
            ("1.5e21", "1.5e+21"),
            ("1.23e22", "1.23e+22"),
            ("1e300", "1e+300"),
            ("1.5", "1.5"),
            ("-1.5", "-1.5"),
            ("0.1", "0.1"),
            ("0.30000000000000004", "0.30000000000000004"),
            ("1e-6", "0.000001"),
            ("0.000001", "0.000001"),
            ("1e-7", "1e-7"),
            ("5e-324", "5e-324"),
            ("1e-5", "0.00001"),
            ("5.0", "5"),
            ("10.0", "10"),
        ];
        for (src, want) in cases {
            assert_eq!(js_to_string(&v(src)), want, "String({src}) via js_to_string");
        }
    }

    #[test]
    fn array_kinds_match_v8_join() {
        // Array.prototype.toString === join(","): null/undefined render
        // EMPTY (not "null"), nested arrays recurse, objects stringify.
        assert_eq!(js_to_string(&json!([])), "");
        assert_eq!(js_to_string(&json!([1, 2, 3])), "1,2,3");
        assert_eq!(js_to_string(&json!([null])), "");
        assert_eq!(js_to_string(&json!([null, 1])), ",1");
        assert_eq!(js_to_string(&json!([null, "a", null])), ",a,");
        assert_eq!(js_to_string(&json!([1, [null, 2], 3])), "1,,2,3");
        assert_eq!(js_to_string(&json!([{}])), "[object Object]");
        assert_eq!(js_to_string(&json!([1, [2, [null, [true]]]])), "1,2,,true");
    }

    #[test]
    fn object_kinds() {
        assert_eq!(js_to_string(&json!({ "a": 1 })), "[object Object]");
        assert_eq!(js_to_string(&json!({})), "[object Object]");
    }

    #[test]
    fn undefined_via_option_opt() {
        assert_eq!(js_to_string_opt(None), "undefined");
        assert_eq!(js_to_string_opt(Some(&json!(null))), "null");
        assert_eq!(js_to_string_opt(Some(&json!("x"))), "x");
    }
}
