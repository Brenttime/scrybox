//! Port of `normalizeCard` + `oracleFields` + `scryfallUuid` from
//! `backend/src/scryfallApi.js` (and `utils/oracleId.js`). Maps a raw
//! Scryfall card object onto the `card_cache` shape the whole app speaks.

use serde_json::{Map, Value};

use crate::search::to_name;

const COLOR_NAMES: [(&str, &str); 5] = [
    ("W", "White"),
    ("U", "Blue"),
    ("B", "Black"),
    ("R", "Red"),
    ("G", "Green"),
];

fn color_name(c: &str) -> String {
    COLOR_NAMES
        .iter()
        .find(|(k, _)| *k == c)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| c.to_string())
}

/// `oracleIdForCard` (utils/oracleId.js): top-level oracle_id, else the
/// shared face identity only when every populated face agrees.
pub fn oracle_id_for_card(card: &Value) -> Option<String> {
    if let Some(o) = card.get("oracle_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        return Some(o.to_string());
    }
    let mut ids: Vec<String> = Vec::new();
    if let Some(faces) = card.get("card_faces").and_then(|v| v.as_array()) {
        for face in faces {
            if let Some(o) = face.get("oracle_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                let o = o.to_string();
                if !ids.contains(&o) {
                    ids.push(o);
                }
            }
        }
    }
    if ids.len() == 1 {
        Some(ids.pop().unwrap())
    } else {
        None
    }
}

/// `defaultSearchEligible`: /cards/search hides these supplemental layouts
/// unless asked for; the local planner's rows must mirror that pool.
pub fn default_search_eligible(card: &Value) -> Option<i64> {
    const EXTRA_LAYOUTS: [&str; 7] = [
        "planar",
        "scheme",
        "vanguard",
        "token",
        "double_faced_token",
        "emblem",
        "art_series",
    ];
    let layout = card.get("layout").and_then(|v| v.as_str())?;
    Some(if EXTRA_LAYOUTS.contains(&layout.to_ascii_lowercase().as_str()) { 0 } else { 1 })
}

/// `scryfallUuid` — strip the `mtg-` prefix, keep only real UUIDs.
pub fn scryfall_uuid(id: Option<&str>) -> Option<String> {
    let raw = id.unwrap_or("").strip_prefix("mtg-").unwrap_or(id.unwrap_or(""));
    if raw.len() != 36 {
        return None;
    }
    let bytes = raw.as_bytes();
    let groups = [8, 4, 4, 4, 12];
    let mut i = 0;
    for (g, &len) in groups.iter().enumerate() {
        for _ in 0..len {
            let b = *bytes.get(i)?;
            if !b.is_ascii_hexdigit() {
                return None;
            }
            i += 1;
        }
        if g < groups.len() - 1 {
            if bytes.get(i) != Some(&b'-') {
                return None;
            }
            i += 1;
        }
    }
    Some(raw.to_string())
}

/// `oracleFields`: per-face rules text for multi-face cards (len > 1),
/// joined fallback when the top level carries none.
fn oracle_fields(raw: &Map<String, Value>) -> (Value, Value) {
    let faces = match raw.get("card_faces").and_then(|v| v.as_array()) {
        Some(a) if a.len() > 1 => {
            let mut out = Vec::with_capacity(a.len());
            for f in a {
                let fm = f.as_object();
                let get = |k: &str| -> String {
                    fm.and_then(|m| m.get(k))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let mut o = Map::new();
                o.insert("name".into(), Value::String(get("name")));
                o.insert("mana_cost".into(), Value::String(get("mana_cost")));
                o.insert("type_line".into(), Value::String(get("type_line")));
                o.insert("oracle_text".into(), Value::String(get("oracle_text")));
                out.push(Value::Object(o));
            }
            Some(out)
        }
        _ => None,
    };
    let text = match raw.get("oracle_text") {
        Some(v) if !v.is_null() => v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()),
        _ => match &faces {
            Some(fs) => fs
                .iter()
                .filter_map(|f| f.get("oracle_text").and_then(|v| v.as_str()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n//\n\n"),
            None => String::new(),
        },
    };
    (
        Value::String(text),
        faces.map(Value::Array).unwrap_or(Value::Null),
    )
}

fn str_field<'a>(raw: &'a Map<String, Value>, face: &Map<String, Value>, key: &str) -> String {
    raw.get(key)
        .and_then(|v| v.as_str())
        .or_else(|| face.get(key).and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string()
}

fn str_or_null(raw: &Map<String, Value>, face: &Map<String, Value>, key: &str) -> Option<String> {
    raw.get(key)
        .and_then(|v| v.as_str())
        .or_else(|| face.get(key).and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn str_array(raw: &Map<String, Value>, face: &Map<String, Value>, key: &str) -> Vec<String> {
    raw.get(key)
        .and_then(|v| v.as_array())
        .or_else(|| face.get(key).and_then(|v| v.as_array()))
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

/// Scryfall money: string or number (or null). Node's `money(v)` =
/// `v != null ? parseFloat(v) : null` — unparseable values become NULL in
/// the bind (NaN), so anything that is not null but not a number maps to
/// None here.
fn money(v: Option<&Value>) -> Option<f64> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(_) => None,
    }
}

/// `normalizeCard(raw, lang)` — full field-for-field port. Returns the
/// normalized object including the response-only hints (`finishes`,
/// `released_at`) cacheNormalizedCards ignores.
pub fn normalize_card(raw: &Value, lang: Option<&str>) -> Value {
    let raw_m = match raw.as_object() {
        Some(m) => m,
        None => return Value::Null,
    };
    // DFCs carry art/type on card_faces[0]; fall back to the front face only
    // when the top level has no image_uris.
    let face_val = raw_m
        .get("image_uris")
        .filter(|v| !v.is_null())
        .is_none()
        .then(|| raw_m.get("card_faces").and_then(|v| v.as_array()).filter(|a| !a.is_empty()))
        .flatten()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_else(|| raw.clone());
    let face = face_val.as_object().cloned().unwrap_or_default();

    let language = to_name(
        raw_m.get("lang").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).or(lang),
    );

    let img_src = raw_m
        .get("image_uris")
        .filter(|v| v.is_object())
        .or_else(|| face.get("image_uris").filter(|v| v.is_object()))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));

    let type_line = str_field(raw_m, &face, "type_line");
    let colors = str_array(raw_m, &face, "colors");
    let color_identity = str_array(raw_m, &face, "color_identity");

    // A response WITHOUT a `prices` object says nothing about the market:
    // leave the numbers null and price_source null so the cache write keeps
    // the row's last real quote instead of stamping $0 over it.
    let has_prices = raw_m.get("prices").map(|v| v.is_object()).unwrap_or(false);
    let prices = raw_m.get("prices").and_then(|v| v.as_object());
    let usd = money(prices.and_then(|p| p.get("usd")));
    let usd_foil = money(prices.and_then(|p| p.get("usd_foil")));
    let usd_etched = money(prices.and_then(|p| p.get("usd_etched")));
    let cmc = money(raw_m.get("cmc"));

    let price_trend = if !has_prices {
        Value::Null
    } else {
        Value::from(usd.or(usd_foil).or(usd_etched).unwrap_or(0.0))
    };

    let rarity = match raw_m.get("rarity").and_then(|v| v.as_str()) {
        Some(r) if !r.is_empty() => {
            let mut c = r.chars();
            let first = c.next().map(|ch| ch.to_uppercase().collect::<String>()).unwrap_or_default();
            format!("{first}{}", c.as_str())
        }
        _ => "Common".to_string(),
    };

    let subtypes: Vec<String> = type_line
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let types: Vec<String> = colors.iter().map(|c| color_name(c)).collect();
    let ci: Vec<String> = color_identity.iter().map(|c| color_name(c)).collect();

    let image_url = ["normal", "large", "small"]
        .iter()
        .find_map(|k| img_src.get(*k).and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string();

    let (oracle_text, oracle_faces) = oracle_fields(raw_m);

    let purchase = raw_m.get("purchase_uris").and_then(|v| v.as_object());
    let tcgplayer_product_id = raw_m
        .get("tcgplayer_id")
        .or_else(|| raw_m.get("tcgplayer_etched_id"))
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or(Value::Null);

    let mut out = Map::new();
    out.insert("id".into(), Value::String(format!("mtg-{}", raw_m.get("id").and_then(|v| v.as_str()).unwrap_or(""))));
    out.insert("oracle_id".into(), oracle_id_for_card(raw).map(Value::String).unwrap_or(Value::Null));
    out.insert(
        "scryfall_search_eligible".into(),
        default_search_eligible(raw).map(Value::from).unwrap_or(Value::Null),
    );
    out.insert("name".into(), Value::String({
        // Node: `face.name || raw.name || ''` — FACE FIRST (a face-picked DFC
        // row keeps its front-face name), then raw. NOT str_field's raw-first.
        let n = face.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !n.is_empty() {
            n.to_string()
        } else {
            raw_m.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()
        }
    }));
    out.insert("supertype".into(), Value::String("MTG".into()));
    out.insert("subtypes".into(), Value::Array(subtypes.into_iter().map(Value::String).collect()));
    out.insert("types".into(), Value::Array(types.into_iter().map(Value::String).collect()));
    out.insert("rarity".into(), Value::String(rarity));
    out.insert("set_id".into(), Value::String(raw_m.get("set").and_then(|v| v.as_str()).unwrap_or("").to_string()));
    out.insert("set_name".into(), Value::String(raw_m.get("set_name").and_then(|v| v.as_str()).unwrap_or("").to_string()));
    out.insert("number".into(), Value::String(raw_m.get("collector_number").and_then(|v| v.as_str()).unwrap_or("").to_string()));
    out.insert("image_url".into(), Value::String(image_url));
    out.insert("price_trend".into(), price_trend);
    out.insert("price_normal".into(), usd.map(Value::from).unwrap_or(Value::Null));
    out.insert("price_holofoil".into(), usd_foil.map(Value::from).unwrap_or(Value::Null));
    out.insert("price_etched".into(), usd_etched.map(Value::from).unwrap_or(Value::Null));
    out.insert("cmc".into(), cmc.map(Value::from).unwrap_or(Value::Null));
    out.insert("color_identity".into(), Value::Array(ci.into_iter().map(Value::String).collect()));
    out.insert("language".into(), Value::String(language));
    out.insert("printed_name".into(), str_or_null(raw_m, &face, "printed_name").map(Value::String).unwrap_or(Value::Null));
    out.insert("tcgplayer_url".into(), purchase.and_then(|p| p.get("tcgplayer")).and_then(|v| v.as_str()).map(|s| Value::String(s.to_string())).unwrap_or(Value::Null));
    out.insert("cardmarket_url".into(), purchase.and_then(|p| p.get("cardmarket")).and_then(|v| v.as_str()).map(|s| Value::String(s.to_string())).unwrap_or(Value::Null));
    out.insert("tcgplayer_product_id".into(), tcgplayer_product_id);
    out.insert("price_source".into(), if has_prices { Value::String("scryfall".into()) } else { Value::Null });
    out.insert("price_currency".into(), Value::String("USD".into()));
    if let Some(f) = raw_m.get("finishes").filter(|v| v.is_array()) {
        out.insert("finishes".into(), f.clone());
    }
    if let Some(r) = raw_m.get("released_at").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        out.insert("released_at".into(), Value::String(r.to_string()));
    }
    out.insert("oracle_text".into(), oracle_text);
    out.insert("oracle_faces".into(), oracle_faces);
    Value::Object(out)
}
