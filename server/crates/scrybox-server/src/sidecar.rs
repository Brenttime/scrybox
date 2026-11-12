//! Port of the live leg of Node's field-filter search:
//! `scryfallApi.js` `runSearch` field-filter branch (specific/broad/fallback
//! queries) → `fetchWindow`/`scryPage` against the scryfall-local sidecar,
//! plus the `normalizeCard` shaping (incl. `utils/oracleId.js`).
//!
//! Node answers a zero-row field-filter search by paging the sidecar with
//! `"<name> unique:prints -is:digital"` (then the first word), normalizing
//! each raw card, sorting by printed number, and replying with the cards —
//! `X-Source: scryfall` when the walk produced rows, `X-Source: cache` with
//! an empty array (plus any `X-Total-Count` the broad query established)
//! when it produced none. This module mirrors that leg exactly, including
//! the quirks: the PAGE_SIZE-175 window math against a sidecar that itself
//! pages at 75, the first-word fallback rules, and per-call `meta.total`
//! assignment (only the broad query may set it, and only if the call returns
//! normally — a 404/422 discards the whole window, partial pages included).
//!
//! Cache side effect (unit 7c): like Node's leg, a non-empty field-filter
//! walk upserts the walked rows into `card_cache` — the caller (search.rs)
//! runs them through the shared `card_cache::cache_normalized_cards` twin
//! before answering, exactly like Node's `await cacheCards(cards)` in
//! runSearch. That write is observable (suites read card_cache right after
//! the response) and it is what keeps walked cards answerable from the cache
//! leg when the sidecar/upstream later goes dark. Both servers share the one
//! card_cache, so both must write it or the two disagree by construction. If
//! a request errors on the sidecar leg, Rust proxies instead — Node then
//! answers verbatim, including its own upserts, retries and 503 semantics.

use serde_json::{Map, Value};

const PRINTS: &str = " unique:prints -is:digital";
const SCRY_PAGE_SIZE: i64 = 175;
/// Upper bound on one sidecar page fetch. The sidecar answers in ms; a hung
/// request must not outlive the request — Rust proxies instead (Node would
/// take the hit itself, which is the parity-preserving behavior anyway).
const PAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Any sidecar failure: the caller must proxy (Node's own walk owns the
/// retries/429s/503 semantics, and proxying reproduces them exactly).
pub struct WalkFailed;

/// Outcome of the field-filter walk, before `attachOwnedQty`.
pub struct WalkOutcome {
    pub cards: Vec<Value>,
    /// meta.total — only ever set by a normally-returning broad-query call.
    pub total: Option<u64>,
    /// 'scryfall' (rows found) or 'cache' (empty walk), per Node.
    pub source: &'static str,
}

// ---------------------------------------------------------------------------
// JS primitives
// ---------------------------------------------------------------------------

/// `encodeURIComponent` — the exact set Node's axios sends in `?q=`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `parseFloat` returning the JS number (None = NaN). Lenient prefix parse:
/// leading whitespace, optional sign, decimal digits with optional `.` and
/// exponent; trailing junk is ignored, a non-numeric start is NaN.
pub fn js_parse_float(s: &str) -> Option<f64> {
    let t = s.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{0}');
    let b = t.as_bytes();
    let mut i = 0;
    let neg = matches!(b.first(), Some(b'-'));
    if matches!(b.first(), Some(b'-') | Some(b'+')) {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_end = i;
    let mut frac = None;
    if i < b.len() && b[i] == b'.' {
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > i + 1 {
            frac = Some((i + 1, j));
            i = j;
        }
    }
    let exp = if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let s0 = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > s0 {
            Some(&t[i + 1..j])
        } else {
            None
        }
    } else {
        None
    };
    if start == int_end && frac.is_none() {
        return None; // no digits at all → NaN
    }
    // Hand Rust only the digits we validated (JS parseFloat is prefix-tolerant).
    let mut s2 = String::new();
    if neg {
        s2.push('-');
    }
    s2.push_str(&t[start..int_end]);
    if let Some((fs, fe)) = frac {
        s2.push('.');
        s2.push_str(&t[fs..fe]);
    }
    if let Some(e) = exp {
        s2.push('e');
        s2.push_str(&e.to_lowercase());
    }
    s2.parse::<f64>().ok()
}

/// A JS number serialized the way `JSON.stringify` prints it: integral
/// doubles lose the `.0` serde_json would add (Node's ETag is computed on
/// `3`, not `3.0`, and while parity compares parsed JSON, byte-identical
/// bodies are the contract the ETag cross-check expects).
fn js_number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.007199254740992e15 {
        Value::from(v as i64)
    } else {
        serde_json::Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `String(x)` for the oracle-text coercion (null is filtered by the caller).
fn js_string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// normalizeCard port (scryfallApi.js normalizeCard + utils/oracleId)
// ---------------------------------------------------------------------------

const COLOR_NAMES: [(&str, &str); 5] =
    [("W", "White"), ("U", "Blue"), ("B", "Black"), ("R", "Red"), ("G", "Green")];

fn color_name(c: &str) -> String {
    COLOR_NAMES.iter().find(|(k, _)| *k == c).map(|(_, v)| v.to_string()).unwrap_or_else(|| c.to_string())
}

fn split_words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

fn capitalize(s: &str) -> String {
    let mut ch = s.chars();
    match ch.next() {
        Some(c) => c.to_uppercase().collect::<String>() + ch.as_str(),
        None => String::new(),
    }
}

fn oracle_id_for(card: Option<&Value>) -> Option<String> {
    let card = card?;
    let o = card.as_object()?;
    if truthy(o.get("oracle_id")) {
        return Some(str_of(o.get("oracle_id").unwrap()));
    }
    let faces = o.get("card_faces").and_then(|v| v.as_array())?;
    let mut ids: Vec<String> = Vec::new();
    for f in faces {
        if let Some(oid) = f.as_object().and_then(|f| f.get("oracle_id")).filter(|v| truthy(Some(v))) {
            let s = str_of(oid);
            if !ids.contains(&s) {
                ids.push(s);
            }
        }
    }
    if ids.len() == 1 {
        Some(ids.into_iter().next().unwrap())
    } else {
        None
    }
}

fn default_search_eligible(raw: &Value) -> Value {
    let layout = raw.get("layout").filter(|v| truthy(Some(v)));
    let Some(layout) = layout else { return Value::Null };
    const EXTRA: [&str; 7] = ["planar", "scheme", "vanguard", "token", "double_faced_token", "emblem", "art_series"];
    if EXTRA.contains(&str_of(layout).to_lowercase().as_str()) {
        Value::from(0)
    } else {
        Value::from(1)
    }
}

/// Node: `const money = (v) => v != null ? parseFloat(v) : null;` — returns
/// Some(NaN) for unparseable values (JS NaN is `!= null`, so it passes the
/// price_trend fallback chain but serializes to null), None for JS null.
fn money_opt(v: Option<&Value>) -> Option<f64> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_f64().or(Some(f64::NAN)),
        Some(Value::String(s)) => Some(js_parse_float(s).unwrap_or(f64::NAN)),
        Some(_) => Some(f64::NAN), // parseFloat(true/[]/{}) → NaN
    }
}

/// NaN serializes as null through JSON.stringify.
fn money_value(v: Option<f64>) -> Value {
    match v {
        None => Value::Null,
        Some(f) if f.is_nan() => Value::Null,
        Some(f) => js_number(f),
    }
}

/// `normalizeCard(raw, lang)` — same field order as the Node object literal
/// (deepDiff compares key order), same fallbacks (`||` vs `??` distinctions
/// preserved). `lang_name` is the runSearch `lang` argument (display name).
pub fn normalize_card(raw: &Value, lang_name: &str) -> Map<String, Value> {
    let obj = raw.as_object();
    // const face = raw && !raw.image_uris && Array.isArray(raw.card_faces) && raw.card_faces.length ? raw.card_faces[0] : raw;
    let face: &Value = match obj {
        Some(o)
            if !truthy(o.get("image_uris"))
                && o.get("card_faces").map(|v| v.is_array()).unwrap_or(false)
                && o.get("card_faces").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false) =>
        {
            &o["card_faces"][0]
        }
        _ => raw,
    };
    let fobj = face.as_object();

    let prices = obj.and_then(|o| o.get("prices")).filter(|v| matches!(v, Value::Object(_)));
    let has_prices = prices.is_some();

    let type_line = obj
        .and_then(|o| o.get("type_line"))
        .filter(|v| truthy(Some(v)))
        .or_else(|| fobj.and_then(|f| f.get("type_line")).filter(|v| truthy(Some(v))))
        .map(str_of)
        .unwrap_or_default();
    let colors_raw = obj.and_then(|o| o.get("colors")).filter(|v| v.is_array()).cloned().or_else(|| fobj.and_then(|f| f.get("colors")).filter(|v| v.is_array()).cloned());
    let color_identity_raw = obj.and_then(|o| o.get("color_identity")).filter(|v| v.is_array()).cloned().or_else(|| fobj.and_then(|f| f.get("color_identity")).filter(|v| v.is_array()).cloned());
    let colors: Vec<Value> = colors_raw.as_ref().and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let color_identity: Vec<Value> = color_identity_raw.as_ref().and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let img = obj
        .and_then(|o| o.get("image_uris"))
        .cloned()
        .or_else(|| fobj.and_then(|f| f.get("image_uris")).cloned())
        .unwrap_or(Value::Object(Map::new()));
    let image_url = ["normal", "large", "small"]
        .iter()
        .map(|k| img.get(*k).filter(|v| truthy(Some(v))).cloned())
        .find_map(|v| v)
        .map(|v| str_of(&v))
        .unwrap_or_default();

    // oracleFields(raw): faces = Array.isArray(card_faces) && len > 1, each
    // shape-normalized to { name, mana_cost, type_line, oracle_text } (all
    // `|| ''`); text = raw.oracle_text ?? join(non-empty face texts, "//").
    let faces: Option<Vec<Value>> = obj
        .and_then(|o| o.get("card_faces"))
        .and_then(|v| v.as_array())
        .filter(|a| a.len() > 1)
        .map(|a| {
            a.iter()
                .map(|f| {
                    let str_or_empty = |k: &str| f.get(k).filter(|v| truthy(Some(v))).map(str_of).unwrap_or_default();
                    let mut m = Map::new();
                    m.insert("name".into(), Value::String(str_or_empty("name")));
                    m.insert("mana_cost".into(), Value::String(str_or_empty("mana_cost")));
                    m.insert("type_line".into(), Value::String(str_or_empty("type_line")));
                    m.insert("oracle_text".into(), Value::String(str_or_empty("oracle_text")));
                    Value::Object(m)
                })
                .collect()
        });
    let oracle_text = match obj.and_then(|o| o.get("oracle_text")) {
        Some(v) if !v.is_null() => Value::String(js_string_of(v)),
        _ => match &faces {
            Some(fs) => Value::String(
                fs.iter()
                    .filter_map(|f| f.get("oracle_text").map(str_of).filter(|s| !s.is_empty()))
                    .collect::<Vec<_>>()
                    .join("\n\n//\n\n"),
            ),
            None => Value::String(String::new()),
        },
    };
    let oracle_faces = match &faces {
        Some(fs) => Value::Array(fs.clone()),
        None => Value::Null,
    };

    let mut o = Map::new();
    // id: `mtg-${raw.id}` — Node prefixes the game tag onto the Scryfall id
    // (template-literal coercion: a missing/null id yields "mtg-undefined" /
    // "mtg-null"; scryfall-shaped rows always carry one).
    let id = match obj.and_then(|x| x.get("id")) {
        Some(Value::Null) | None => "mtg-undefined".to_string(),
        Some(v) => format!("mtg-{}", str_of(v)),
    };
    o.insert("id".into(), Value::String(id));
    o.insert("oracle_id".into(), oracle_id_for(Some(raw)).map(Value::String).unwrap_or(Value::Null));
    o.insert("scryfall_search_eligible".into(), default_search_eligible(raw));
    // name: face.name || raw.name || ''
    let name = fobj
        .and_then(|f| f.get("name"))
        .filter(|v| truthy(Some(v)))
        .or_else(|| obj.and_then(|x| x.get("name")).filter(|v| truthy(Some(v))))
        .map(str_of)
        .unwrap_or_default();
    o.insert("name".into(), Value::String(name));
    o.insert(
        // `supertype` tags these as Magic cards for UI that keys off it —
        // a hardcoded constant in Node's normalizeCard, not a raw field.
        "supertype".into(),
        Value::String("MTG".into()),
    );
    o.insert("subtypes".into(), Value::Array(split_words(&type_line).into_iter().map(Value::String).collect()));
    o.insert("types".into(), Value::Array(colors.iter().map(|c| Value::String(color_name(&str_of(c)))).collect()));
    o.insert(
        "rarity".into(),
        obj.and_then(|x| x.get("rarity"))
            .filter(|v| truthy(Some(v)))
            .map(|v| Value::String(capitalize(&str_of(v))))
            .unwrap_or_else(|| Value::String("Common".into())),
    );
    o.insert("set_id".into(), obj.and_then(|x| x.get("set")).filter(|v| truthy(Some(v))).map(str_of).unwrap_or_default().into());
    o.insert("set_name".into(), obj.and_then(|x| x.get("set_name")).filter(|v| truthy(Some(v))).map(str_of).unwrap_or_default().into());
    o.insert("number".into(), obj.and_then(|x| x.get("collector_number")).filter(|v| truthy(Some(v))).map(str_of).unwrap_or_default().into());
    o.insert("image_url".into(), Value::String(image_url));
    let pget = |k: &str| prices.and_then(|p| p.get(k));
    let usd = money_opt(pget("usd"));
    let usd_foil = money_opt(pget("usd_foil"));
    let usd_etched = money_opt(pget("usd_etched"));
    // price_trend: !hasPrices → null; else usd, usd_foil, usd_etched, 0 —
    // the `!= null` chain passes NaN through (it serializes as null).
    o.insert("price_trend".into(), {
        if !has_prices {
            Value::Null
        } else {
            match usd.or(usd_foil).or(usd_etched) {
                Some(v) => money_value(Some(v)),
                None => Value::from(0),
            }
        }
    });
    o.insert("price_normal".into(), money_value(if has_prices { usd } else { None }));
    o.insert("price_holofoil".into(), money_value(if has_prices { usd_foil } else { None }));
    o.insert("price_etched".into(), money_value(if has_prices { usd_etched } else { None }));
    o.insert(
        "cmc".into(),
        obj.and_then(|x| x.get("cmc"))
            .filter(|v| !v.is_null())
            .map(|v| match v {
                Value::Number(_) => v.clone(),
                Value::String(s) => js_parse_float(s).map(js_number).unwrap_or(Value::Null),
                other => js_parse_float(&js_string_of(other)).map(js_number).unwrap_or(Value::Null),
            })
            .unwrap_or(Value::Null),
    );
    o.insert("color_identity".into(), Value::Array(color_identity.iter().map(|c| Value::String(color_name(&str_of(c)))).collect()));
    o.insert(
        "language".into(),
        match obj.and_then(|x| x.get("lang")).filter(|v| truthy(Some(v))) {
            // toName(raw.lang): accepts code/name/provider, unknown → English.
            Some(l) => Value::String(crate::search::to_name(Some(&str_of(l)))),
            // raw.lang falsy → toName(lang); lang arrives as a display name.
            None => Value::String(lang_name.to_string()),
        },
    );
    o.insert(
        "printed_name".into(),
        obj.and_then(|x| x.get("printed_name"))
            .filter(|v| truthy(Some(v)))
            .or_else(|| fobj.and_then(|f| f.get("printed_name")).filter(|v| truthy(Some(v))))
            .cloned()
            .unwrap_or(Value::Null),
    );
    let purchase = obj.and_then(|x| x.get("purchase_uris")).filter(|v| matches!(v, Value::Object(_)));
    o.insert(
        "tcgplayer_url".into(),
        purchase.and_then(|p| p.get("tcgplayer")).filter(|v| truthy(Some(v))).cloned().unwrap_or(Value::Null),
    );
    o.insert(
        "cardmarket_url".into(),
        purchase.and_then(|p| p.get("cardmarket")).filter(|v| truthy(Some(v))).cloned().unwrap_or(Value::Null),
    );
    // raw.tcgplayer_id ?? raw.tcgplayer_etched_id ?? null
    o.insert(
        "tcgplayer_product_id".into(),
        obj.and_then(|x| x.get("tcgplayer_id")).filter(|v| !v.is_null()).cloned()
            .or_else(|| obj.and_then(|x| x.get("tcgplayer_etched_id")).filter(|v| !v.is_null()).cloned())
            .unwrap_or(Value::Null),
    );
    o.insert("price_source".into(), if has_prices { Value::String("scryfall".into()) } else { Value::Null });
    o.insert("price_currency".into(), Value::String("USD".into()));
    if let Some(f) = obj.and_then(|x| x.get("finishes")).filter(|v| v.is_array()) {
        o.insert("finishes".into(), f.clone());
    }
    if let Some(r) = obj.and_then(|x| x.get("released_at")).filter(|v| truthy(Some(v))) {
        o.insert("released_at".into(), Value::String(str_of(r)));
    }
    o.insert("oracle_text".into(), oracle_text);
    o.insert("oracle_faces".into(), oracle_faces);
    o
}

// ---------------------------------------------------------------------------
// fetchWindow / tryQuery / walk — the Node live leg
// ---------------------------------------------------------------------------

struct Page {
    data: Vec<Value>,
    has_more: bool,
    total: Option<u64>,
}

/// `scryPage(q, lang, page, undefined)` with the sidecar base — GET
/// `{base}/cards/search?q=<enc>&page=N`. `Err(WalkFailed)` on anything Node
/// would not swallow (network, non-JSON, status other than 404/422);
/// `Err(Mismatch)` marks the 404/422 pair `tryQuery` catches.
enum PageErr {
    Mismatch,
    Failed,
}

async fn scry_page(st: &crate::AppState, base: &str, q: &str, page: i64) -> Result<Page, PageErr> {
    let uri = format!("{base}/cards/search?q={}&page={}", encode_uri_component(q), page)
        .parse::<hyper::Uri>()
        .map_err(|_| PageErr::Failed)?;
    let req = hyper::Request::get(uri)
        .header("user-agent", "Scrybox/1.0")
        .body(axum::body::Body::empty())
        .map_err(|_| PageErr::Failed)?;
    let res = tokio::time::timeout(PAGE_TIMEOUT, st.client.request(req))
        .await
        .map_err(|_| PageErr::Failed)?
        .map_err(|_| PageErr::Failed)?;
    let status = res.status().as_u16();
    if status == 404 || status == 422 {
        return Err(PageErr::Mismatch);
    }
    if status != 200 {
        return Err(PageErr::Failed);
    }
    let bytes = http_body_util::BodyExt::collect(res.into_body())
        .await
        .map_err(|_| PageErr::Failed)?
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).map_err(|_| PageErr::Failed)?;
    let data = json.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let has_more = json.get("has_more").map(|v| truthy(Some(v))).unwrap_or(false);
    let total = match json.get("total_cards") {
        Some(Value::Number(n)) => n.as_f64().map(|f| f.max(0.0) as u64),
        Some(Value::Null) | None => None,
        Some(_) => Some(u64::MAX), // non-number total_cards → JS Number(...) → NaN; unreachable with scryfall-local
    };
    Ok(Page { data, has_more, total })
}

/// `fetchWindow` + the `tryQuery` wrapper, for the name-only branch (order
/// undefined, English: no lang keyword, no include_multilingual). Returns
/// `(cards, total_update)` — `total_update` is `Some` only when the call
/// returned normally with a non-null total (Node assigns meta.total then);
/// 404/422 on ANY page discards the window like tryQuery's catch.
async fn try_query(st: &crate::AppState, base: &str, q: &str, lang_name: &str, offset: i64, limit: i64) -> Result<(Vec<Value>, Option<u64>), WalkFailed> {
    let mut page = offset.div_euclid(SCRY_PAGE_SIZE) + 1;
    let mut skip = offset.rem_euclid(SCRY_PAGE_SIZE) as usize;
    let mut out: Vec<Value> = Vec::new();
    let mut total: Option<u64> = None;
    let mut has_more = false;
    while (out.len() as i64) < limit {
        let p = match scry_page(st, base, q, page).await {
            Ok(p) => p,
            Err(PageErr::Mismatch) => return Ok((Vec::new(), None)), // tryQuery catch: [] and meta.total untouched
            Err(PageErr::Failed) => return Err(WalkFailed),
        };
        if p.total.is_some() {
            total = p.total;
        }
        out.extend(p.data.into_iter().skip(skip).map(|raw| Value::Object(normalize_card(&raw, lang_name))));
        skip = 0;
        has_more = p.has_more;
        if !has_more {
            break;
        }
        page += 1;
    }
    let _ = has_more; // tryQuery discards the window's hasMore
    out.truncate(limit as usize);
    Ok((out, total))
}

/// JS: `cleanName.split(/\s+/)[0]` — the first whitespace-delimited chunk of
/// the TRIMMED name, punctuation included ("Shalai," keeps its comma — the
/// exact quirk that makes the fallback empty for comma names). Returns ''
/// only for empty/all-space input, which keeps the fallback out (Node's
/// `if (firstWord && ...)`).
pub(crate) fn first_word(clean: &str) -> &str {
    fn is_js_ws(c: char) -> bool {
        matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
    }
    let end = clean.find(is_js_ws).unwrap_or(clean.len());
    &clean[..end]
}

// ---------------------------------------------------------------------------
// collection-scope q-walk (iter 9) — runWalk port for fetchCollectionNames
// ---------------------------------------------------------------------------

/// Outcome of a `runWalk` name-set walk (collq.rs is the caller; it owns the
/// cache entry shape, persistence and the durable-key contract).
pub struct NamesWalk {
    /// Insertion-ordered deduped lowercased names (JS Set semantics: full
    /// "A // B" plus each trimmed face for DFCs).
    pub names: Vec<String>,
    /// `!hasMore` — false only when the walk stopped at the page cap.
    pub complete: bool,
    /// upstream total_cards as the engine reported it (kept as Value to
    /// round-trip through the durable table byte-exactly).
    pub total: Option<Value>,
    pub fetched_count: u64,
    /// The first `min(500, walk_limit)` RAW cards of the window — the input
    /// to resolveCollectionQuery's bounded card_cache backfill (unit 7c),
    /// which mirrors Node's `fetched.slice(0, 500)` pre-normalize slice.
    pub fetched: Vec<Value>,
}

pub enum NamesWalkErr {
    /// 400/422: the engine's own verdict on this query. Node maps it to
    /// `INVALID_QUERY` 400 and caches NOTHING (every retry re-pays the walk).
    Invalid,
    /// Anything else (transport, timeout, 429, 5xx): Node falls through to
    /// its local-fallback/503 semantics, so Rust proxies instead.
    Transport,
}

/// One page for the names walk. Unlike `scry_page` this keeps 404 and
/// 400/422 distinct because runWalk's catch treats them differently: a 404
/// ANYWHERE collapses the whole window to a durable empty answer, a 400/422
/// aborts the request with INVALID_QUERY, anything else is a transport-style
/// failure (local-fallback / 503 in Node → proxy in Rust).
struct NamesPage {
    data: Vec<Value>,
    has_more: bool,
    total: Option<Value>,
}

async fn names_page(st: &crate::AppState, base: &str, q: &str, page: i64, multilingual: bool) -> Result<NamesPage, NamesWalkErr> {
    let mut uri = format!("{base}/cards/search?q={}&page={}", encode_uri_component(q), page);
    if multilingual {
        uri.push_str("&include_multilingual=true");
    }
    let req = hyper::Request::get(uri.parse::<hyper::Uri>().map_err(|_| NamesWalkErr::Transport)?)
        .header("user-agent", "Scrybox/1.0")
        .body(axum::body::Body::empty())
        .map_err(|_| NamesWalkErr::Transport)?;
    let res = tokio::time::timeout(PAGE_TIMEOUT, st.client.request(req))
        .await
        .map_err(|_| NamesWalkErr::Transport)?
        .map_err(|_| NamesWalkErr::Transport)?;
    let status = res.status().as_u16();
    if status == 404 {
        // runWalk catch: fetched=[], hasMore=false, total=0.
        return Ok(NamesPage { data: Vec::new(), has_more: false, total: Some(Value::from(0)) });
    }
    if status == 400 || status == 422 {
        return Err(NamesWalkErr::Invalid);
    }
    if status != 200 {
        return Err(NamesWalkErr::Transport);
    }
    let bytes = http_body_util::BodyExt::collect(res.into_body())
        .await
        .map_err(|_| NamesWalkErr::Transport)?
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).map_err(|_| NamesWalkErr::Transport)?;
    let data = json.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let has_more = json.get("has_more").map(|v| truthy(Some(v))).unwrap_or(false);
    let total = match json.get("total_cards") {
        Some(Value::Number(n)) => Some(Value::Number(n.clone())),
        Some(Value::Null) | None => None,
        Some(_) => Some(Value::from(u64::MAX)), // JS Number(non-number) → NaN; unreachable with scryfall-local
    };
    Ok(NamesPage { data, has_more, total })
}

/// `fetchWindow(walkQ, lang, 0, walkLimit)` + runWalk's name-set build, for
/// the collection-scope walk. `walk_q` is the durable-key query already
/// (parenthesised for non-English per resolveCollectionQuery, ` unique:cards`
/// appended per fetchCollectionNames); `lang_code`/`multilingual` carry
/// langSearch's non-English scoping (` lang:xx` keyword + the
/// include_multilingual parameter — the engine ignores a bare `lang` URL
/// param, which is why the keyword form is load-bearing).
pub async fn walk_collection_names(st: &crate::AppState, base: &str, walk_q: &str, lang_code: &str, walk_limit: i64) -> Result<NamesWalk, NamesWalkErr> {
    let non_en = lang_code != "en";
    let scoped = if non_en { format!("{walk_q} lang:{lang_code}") } else { walk_q.to_string() };
    let mut page = 1i64;
    let mut out_len: i64 = 0;
    let mut has_more = false;
    let mut total: Option<Value> = None;
    let mut names: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // resolveCollectionQuery's backfill slice is over the WINDOW (fetched,
    // capped at the walk limit), first 500 — keep exactly that raw prefix
    // without buffering the whole 20k walk.
    let fetch_cap = walk_limit.min(500).max(0) as usize;
    let mut fetched: Vec<Value> = Vec::new();
    while out_len < walk_limit {
        let p = names_page(st, base, &scoped, page, non_en).await?;
        if p.total.is_some() {
            total = p.total;
        }
        if fetched.len() < fetch_cap {
            let take = (fetch_cap - fetched.len()).min(p.data.len());
            fetched.extend(p.data[..take].iter().cloned());
        }
        out_len += p.data.len() as i64;
        for raw in &p.data {
            // String(raw.name || ''): falsy (absent, null, false, 0, '') →
            // ''; a number stringifies, anything else object/bool text.
            let full = match raw.get("name") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            };
            let full = full.trim().to_lowercase();
            if full.is_empty() {
                continue;
            }
            let is_dfc = full.contains(" // ");
            if seen.insert(full.clone()) {
                names.push(full.clone());
            }
            if is_dfc {
                for part in full.split(" // ") {
                    let p2 = part.trim();
                    if !p2.is_empty() && seen.insert(p2.to_string()) {
                        names.push(p2.to_string());
                    }
                }
            }
        }
        has_more = p.has_more;
        if !has_more {
            break;
        }
        page += 1;
    }
    // fetchWindow's cut: cards = out.slice(0, limit) and
    // hasMore = hasMore || out.length > limit — a walk that stopped because
    // the window filled is a REAL but INCOMPLETE prefix (runWalk's comment).
    let truncated = out_len > walk_limit;
    Ok(NamesWalk { names, complete: !has_more && !truncated, total, fetched_count: out_len.min(walk_limit).max(0) as u64, fetched })
}

/// The field-filter walk for the name-only branch:
/// `exact = []` (no number ⇒ specificQuery null), broad query, first-word
/// fallback, dedup-by-id merge, printed-number stable sort. Empty merge →
/// Node's `[] + X-Source: cache` with the broad query's total.
pub async fn walk_name(st: &crate::AppState, base: &str, clean_name: &str, lang_name: &str, offset: i64, limit: i64) -> Result<WalkOutcome, WalkFailed> {
    let broad_query = format!("{clean_name}{PRINTS}");
    let fw = first_word(clean_name);
    let fallback_query =
        if !fw.is_empty() && fw != clean_name { Some(format!("{fw}{PRINTS}")) } else { None };

    let (mut merged, meta_total) = {
        let (cards, total) = try_query(st, base, &broad_query, lang_name, offset, limit).await?;
        (cards, total) // only the broad query may set meta.total
    };
    if merged.is_empty() {
        if let Some(fq) = &fallback_query {
            let (cards, _ignored) = try_query(st, base, fq, lang_name, offset, limit).await?;
            merged = cards;
        }
    }
    // exact + cards dedup by id, first occurrence wins (exact is always [] here).
    let mut seen = std::collections::HashSet::new();
    merged.retain(|c| {
        let id = c.get("id").map(str_of).unwrap_or_default();
        seen.insert(id)
    });
    if merged.is_empty() {
        return Ok(WalkOutcome { cards: Vec::new(), total: meta_total, source: "cache" });
    }
    merged.truncate(limit as usize);
    // exactIds empty ⇒ plain stable sort by parseInt(card.number, 10) || 0.
    merged.sort_by_key(|c| js_int_key(c.get("number")));
    Ok(WalkOutcome { cards: merged, total: meta_total, source: "scryfall" })
}

/// `parseInt(x, 10) || 0` over the normalized `number` string.
fn js_int_key(v: Option<&Value>) -> i64 {
    let Some(s) = v.map(str_of) else { return 0 };
    crate::search::js_parse_int(&s).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encode_uri_component_matches_js() {
        assert_eq!(encode_uri_component("Sol Ring"), "Sol%20Ring");
        assert_eq!(encode_uri_component("q=1&2"), "q%3D1%262");
        assert_eq!(encode_uri_component("a!~*'()-_."), "a!~*'()-_.");
        assert_eq!(encode_uri_component("é"), "%C3%A9");
        assert_eq!(encode_uri_component("a+b/c"), "a%2Bb%2Fc");
    }

    #[test]
    fn parse_float_matches_js() {
        assert_eq!(js_parse_float("19.90"), Some(19.9));
        assert_eq!(js_parse_float("20"), Some(20.0));
        assert_eq!(js_parse_float(" 4.5e2x"), Some(450.0));
        assert_eq!(js_parse_float("-3"), Some(-3.0));
        assert_eq!(js_parse_float("6E-2"), Some(0.06));
        assert_eq!(js_parse_float(".5"), Some(0.5));
        assert_eq!(js_parse_float("abc"), None);
        assert_eq!(js_parse_float(""), None);
    }

    #[test]
    fn first_word_is_whitespace_split_with_punctuation() {
        assert_eq!(first_word("Shalai, Voice of Plenty"), "Shalai,"); // comma kept — Node quirk
        assert_eq!(first_word("Elvish Mystic"), "Elvish");
        assert_eq!(first_word("Sol"), "Sol");
        assert_eq!(first_word(""), "");
    }

    #[test]
    fn normalize_card_shape_matches_node() {
        let raw = json!({
            "id": "abc", "oracle_id": "o1", "layout": "token", "name": "Test",
            "cmc": 2.5, "colors": ["R", "X"], "color_identity": ["R"],
            "type_line": "Creature — Cat", "rarity": "rare", "set": "xyz",
            "set_name": "XYZ", "collector_number": "12a",
            "image_uris": { "large": "L" }, "released_at": "2020-01-01",
            "prices": { "usd": "", "usd_foil": "3.50" },
            "finishes": ["nonfoil"],
        });
        let m = normalize_card(&raw, "English");
        assert_eq!(m["id"], "mtg-abc");
        assert_eq!(m["supertype"], "MTG");
        assert_eq!(m["scryfall_search_eligible"], 0, "token layout is an extra");
        assert_eq!(m["types"], json!(["Red", "X"]));
        assert_eq!(m["subtypes"], json!(["Creature", "Cat"]));
        assert_eq!(m["rarity"], "Rare");
        assert_eq!(m["number"], "12a");
        assert_eq!(m["image_url"], "L", "falls through normal to large");
        assert_eq!(m["cmc"], 2.5);
        assert_eq!(m["price_normal"], Value::Null, "\"\" parses to NaN → serializes null");
        assert_eq!(m["price_holofoil"], 3.5);
        assert_eq!(m["price_trend"], Value::Null, "NaN passes the != null chain, serializes null");
        assert_eq!(m["finishes"], json!(["nonfoil"]));
        assert_eq!(m["released_at"], "2020-01-01");
        assert_eq!(m["language"], "English");
        assert_eq!(m["price_source"], "scryfall");
        // no prices → all-null money, price_trend null (not 0)
        let m2 = normalize_card(&json!({ "id": "z", "rarity": "" }), "English");
        assert_eq!(m2["price_trend"], Value::Null);
        assert_eq!(m2["rarity"], "Common", "empty rarity defaults Common");
        assert!(m2.get("finishes").is_none(), "undefined key is omitted");
        assert!(m2.get("released_at").is_none());
    }

    #[test]
    fn normalize_card_multiface_oracle_fields() {
        let raw = json!({
            "id": "mf", "layout": "transform",
            "card_faces": [
                { "name": "Day", "mana_cost": "", "type_line": "Creature", "oracle_text": "A" },
                { "name": "Night", "mana_cost": "", "type_line": "Creature", "oracle_text": "" },
            ],
        });
        let m = normalize_card(&raw, "English");
        assert_eq!(
            m["oracle_faces"],
            json!([
                { "name": "Day", "mana_cost": "", "type_line": "Creature", "oracle_text": "A" },
                { "name": "Night", "mana_cost": "", "type_line": "Creature", "oracle_text": "" },
            ])
        );
        assert_eq!(m["oracle_text"], "A", "empty face texts filtered from the join");
        assert_eq!(m["name"], "Day", "face 0 without image_uris");
        assert_eq!(m["oracle_id"], Value::Null);
    }
}
