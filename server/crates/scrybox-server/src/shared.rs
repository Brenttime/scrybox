//! Port of `backend/src/routes/shared.js` (route group `shared`):
//! GET `/api/shared/:share_token` — the public shared-collection view.
//!
//! Public route (Node mounts it ABOVE the authentication gate, no session
//! required). Parity contract (BRIEF rule 1): byte-identical body for the
//! same DB state — key order included, because deepDiff compares object key
//! order — plus the Express default ETag and If-None-Match 304 replay, and
//! the 404/500 bodies verbatim.
//!
//! Row shaping is the exact `{ ...parseCardRow(row), price_trend:
//! resolveCardPrice(row) }` fold from utils/priceHelpers.js: `resolveCardPrice`
//! reads the RAW row (pre-proxy-rarity), `applyProxyRarity` overwrites
//! is_proxy/rarity in SELECT position and appends `base_rarity` as a new
//! key, then the JSON array columns parse back (`color_identity` is not
//! selected here, so it lands as a trailing `[]` — mirroring JS's
//! `JSON.parse(undefined || '[]')`). Stats counters iterate the formatted
//! rows in ORDER BY added_at DESC with insertion-ordered Maps, and
//! `sets` sorts by the RAW accumulated value (b.value - a.value, stable)
//! before slicing 8 and rounding each with `parseFloat(x.toFixed(2))`.

use std::collections::HashMap;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::Response,
};
use serde_json::{json, Map, Value};

use crate::collection::{body_etag, etag_matches, gzip_bytes, json_num, truthy, wants_encoding};
use crate::db::DbPool;
use crate::stats::{js_fixed_parsed, row_to_object};
use crate::AppState;

/// Byte-identical to the assembled template in shared.js (base filter + the
/// optional trade clause).
const COLLECTION_SQL: &str = r#"
      SELECT
        c.id as entry_id,
        c.card_id,
        c.quantity,
        c.condition,
        c.printing,
        c.language,
        c.added_at,
        c.is_trade,
        c.is_proxy,
        cc.name,
        -- The name as printed on a non-English card, so a shared Japanese
        -- collection reads the way the cards actually look.
        cc.printed_name,
        cc.supertype,
        cc.subtypes,
        cc.types,
        cc.rarity,
        cc.set_id,
        cc.set_name,
        cc.number,
        cc.image_url,
        cc.price_trend,
        cc.price_normal,
        cc.price_holofoil,
        cc.price_estimated
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.user_id = ?
      ORDER BY c.added_at DESC
    "#;

const COLLECTION_TRADE_SQL: &str = r#"
      SELECT
        c.id as entry_id,
        c.card_id,
        c.quantity,
        c.condition,
        c.printing,
        c.language,
        c.added_at,
        c.is_trade,
        c.is_proxy,
        cc.name,
        -- The name as printed on a non-English card, so a shared Japanese
        -- collection reads the way the cards actually look.
        cc.printed_name,
        cc.supertype,
        cc.subtypes,
        cc.types,
        cc.rarity,
        cc.set_id,
        cc.set_name,
        cc.number,
        cc.image_url,
        cc.price_trend,
        cc.price_normal,
        cc.price_holofoil,
        cc.price_estimated
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.user_id = ? AND c.is_trade = 1
      ORDER BY c.added_at DESC
    "#;

pub async fn handle(method: Method, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("shared") || st.db.is_none() || method != Method::GET {
        return crate::proxy(State(st.clone()), req).await;
    }
    let pool = st.db.clone().unwrap();
    let headers = req.headers().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);

    // Express `:share_token` matches exactly one (already percent-decoded)
    // path segment; the mount guarantees the prefix.
    let token_raw = path
        .strip_prefix("/api/shared/")
        .expect("mounted only under /api/shared/");
    let token = match percent_decode(token_raw.as_bytes()) {
        Some(t) => t,
        None => {
            // Invalid percent-encoding: Express's decodeURIComponent throws
            // inside routing and the generic error handler answers 500.
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
        }
    };

    let list_type_trade = query.as_deref().map(parse_trade_query).unwrap_or(false);

    let outcome = tokio::task::spawn_blocking(move || build_body(&pool, &token, list_type_trade)).await;

    let raw = match outcome {
        Ok(Ok(raw)) => raw,
        Ok(Err((status, msg))) => return json_error(status, msg),
        Err(e) => {
            tracing::error!(error = %e, "shared task panicked");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve shared collection");
        }
    };
    let tag = body_etag(&raw);
    if fresh_allows_304(&headers) {
        if let Some(inm) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
            if etag_matches(Some(inm), &tag) {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, tag)
                    .body(Body::empty())
                    .unwrap();
            }
        }
    }
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let use_gzip = wants_encoding(ae, "gzip", raw.len());
    let bytes = if use_gzip { gzip_bytes(&raw) } else { axum::body::Bytes::from(raw) };
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, tag);
    if use_gzip {
        resp = resp
            .header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"))
            .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    resp.body(Body::from(bytes)).unwrap()
}

/// The whole res.json body for a request; Err carries the response
/// (404/500) to return.
fn build_body(pool: &DbPool, token: &str, trade: bool) -> Result<Vec<u8>, (StatusCode, &'static str)> {
    let conn = pool.get().map_err(|e| {
        tracing::error!(error = %e, "shared: pool");
        server_error_body()
    })?;

    // Owner lookup. Multiple users could share a token value (no UNIQUE
    // constraint); both servers take the first row in rowid order.
    let owner: Option<(i64, Value, Option<i64>)> = conn
        .prepare_cached("SELECT id, username, share_enabled FROM users WHERE share_token = ?")
        .map_err(|e| {
            tracing::error!(error = %e, "shared: owner prepare");
            server_error_body()
        })?
        .query_row([token], |r| Ok((r.get(0)?, crate::collection::sqlite_value(r, 1)?, r.get(2)?)))
        .ok();

    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            "This card collection is private or does not exist.",
        )
    };
    let (owner_id, owner_name, _share_enabled) = match owner {
        None => return Err(not_found()),
        Some((_, _, Some(0))) => return Err(not_found()),
        Some(t) => t,
    };

    let sql = if trade { COLLECTION_TRADE_SQL } else { COLLECTION_SQL };
    let mut stmt = conn.prepare_cached(sql).map_err(|e| {
        tracing::error!(error = %e, "shared: prepare");
        server_error_body()
    })?;
    let mut rows = stmt
        .query_map([owner_id], row_to_object)
        .map_err(|e| {
            tracing::error!(error = %e, "shared: query");
            server_error_body()
        })?;
    let raw_rows: Vec<Map<String, Value>> = (&mut rows)
        .map(|r| {
            r.map_err(|e| {
                tracing::error!(error = %e, "shared: row");
                server_error_body()
            })
        })
        .collect::<Result<_, _>>()?;
    drop(rows);

    // formatted = rows.map(row => ({ ...parseCardRow(row), price_trend: resolveCardPrice(row) }))
    let formatted: Vec<Map<String, Value>> = raw_rows
        .iter()
        .map(format_shared_row)
        .collect::<Result<_, _>>()
        .map_err(|e| {
            tracing::error!(error = %e, "shared: parseCardRow");
            server_error_body()
        })?;

    // --- public stats -----------------------------------------------------
    let mut total_cards = 0f64;
    let unique_cards = formatted.len() as i64;
    let mut total_value = 0f64;
    let mut estimated_cards = 0f64;

    let mut types: Vec<(String, f64)> = Vec::new();
    let mut type_idx: HashMap<String, usize> = HashMap::new();
    let mut rarities: Vec<(String, f64)> = Vec::new();
    let mut rar_idx: HashMap<String, usize> = HashMap::new();
    struct SetAcc {
        name: Value,
        count: f64,
        value: f64,
    }
    let mut sets: Vec<(String, SetAcc)> = Vec::new();
    let mut set_idx: HashMap<String, usize> = HashMap::new();

    for row in &formatted {
        // JS: const qty = row.quantity || 1;
        let qty_v = row.get("quantity").unwrap_or(&Value::Null);
        let qty = if truthy(qty_v) { qty_v.as_f64().unwrap_or(1.0) } else { 1.0 };
        let price = row.get("price_trend").and_then(|p| p.as_f64()).unwrap_or(0.0);
        let price_or0 = if truthy(&json_num(price)) { price } else { 0.0 };

        total_cards += qty;
        total_value += qty * price_or0;
        if truthy(row.get("price_estimated").unwrap_or(&Value::Null)) && price_or0 > 0.0 {
            estimated_cards += qty;
        }

        // row.types is guaranteed an Array by format_shared_row (a non-array
        // JSON.parse result would throw row.types.forEach -> 500 in Node).
        let type_list = match row.get("types") {
            Some(Value::Array(a)) => a.clone(),
            _ => {
                tracing::error!("shared: types.forEach of non-array");
                return Err(server_error_body());
            }
        };
        for t in &type_list {
            let key = js_obj_key(t);
            let e = type_idx.entry(key.clone()).or_insert_with(|| {
                types.push((key, 0.0));
                types.len() - 1
            });
            types[*e].1 += qty;
        }
        if type_list.is_empty() {
            let e = type_idx.entry("Colorless".to_string()).or_insert_with(|| {
                types.push(("Colorless".to_string(), 0.0));
                types.len() - 1
            });
            types[*e].1 += qty;
        }

        let rarity_key = match row.get("rarity") {
            Some(v) if truthy(v) => js_obj_key(v),
            _ => "Unknown".to_string(),
        };
        let e = rar_idx.entry(rarity_key.clone()).or_insert_with(|| {
            rarities.push((rarity_key, 0.0));
            rarities.len() - 1
        });
        rarities[*e].1 += qty;

        let set_key = js_obj_key(row.get("set_id").unwrap_or(&Value::Null));
        let e = *set_idx.entry(set_key.clone()).or_insert_with(|| {
            sets.push((
                set_key,
                SetAcc { name: row.get("set_name").cloned().unwrap_or(Value::Null), count: 0.0, value: 0.0 },
            ));
            sets.len() - 1
        });
        sets[e].1.count += qty;
        sets[e].1.value += qty * price_or0;
    }

    // sets: Object.keys order -> sort((a,b) => b.value - a.value) stable ->
    // slice(0, 8); value rounded on output only.
    sets.sort_by(|a, b| {
        b.1.value
            .partial_cmp(&a.1.value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sets.truncate(8);

    let body = json!({
        "owner": owner_name,
        "collection": formatted,
        "stats": {
            "summary": {
                "totalCards": json_num(total_cards),
                "uniqueCards": unique_cards,
                "totalValue": js_fixed_parsed(total_value, 2),
                "estimatedCards": json_num(estimated_cards),
            },
            "types": types.iter().map(|(name, value)| json!({ "name": name, "value": json_num(*value) })).collect::<Vec<_>>(),
            "rarities": rarities.iter().map(|(name, value)| json!({ "name": name, "value": json_num(*value) })).collect::<Vec<_>>(),
            "sets": sets.iter().map(|(id, s)| json!({
                "id": id,
                "name": s.name,
                "count": json_num(s.count),
                "value": js_fixed_parsed(s.value, 2),
            })).collect::<Vec<_>>(),
        }
    });
    Ok(serde_json::to_vec(&body).unwrap_or_else(|_| b"null".to_vec()))
}

/// `{ ...parseCardRow(row), price_trend: resolveCardPrice(row) }` —
/// resolveCardPrice reads the RAW row; insertion-ordered overwrites keep
/// every existing key in SELECT position.
fn format_shared_row(row: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let price = crate::collection::resolve_card_price(
        row.get("is_proxy").unwrap_or(&Value::Null),
        row.get("printing").unwrap_or(&Value::Null),
        row.get("price_trend").unwrap_or(&Value::Null),
        row.get("price_normal").unwrap_or(&Value::Null),
        row.get("price_holofoil").unwrap_or(&Value::Null),
    );
    let mut out = crate::stats::apply_proxy_rarity(row.clone());
    out.insert("subtypes".into(), parse_array_col_shared(row.get("subtypes"))?);
    out.insert("types".into(), parse_array_col_shared(row.get("types"))?);
    out.insert("color_identity".into(), parse_array_col_shared(row.get("color_identity"))?);
    out.insert("price_trend".into(), price);
    Ok(out)
}

/// JSON.parse(row.col || '[]') — same helper semantics as
/// collection::parse_array_col but returns ANY JSON (parseCardRow does not
/// require an array; `row.types.forEach` throws later for non-arrays).
fn parse_array_col_shared(v: Option<&Value>) -> Result<Value, String> {
    match v {
        None | Some(Value::Null) => Ok(Value::Array(Vec::new())),
        Some(Value::String(s)) if s.is_empty() => Ok(Value::Array(Vec::new())),
        Some(Value::String(s)) => serde_json::from_str(s).map_err(|e| format!("bad array column: {e}")),
        Some(other) => Ok(other.clone()),
    }
}

/// Object-key coercion for `typeCounts[t]`-style counters.
fn js_obj_key(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(js_obj_key).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// `req.query.list` under qs: exactly ONE `list=` param whose value is
/// 'trade' selects the trade branch; a repeated key becomes an ARRAY in qs
/// (`!== 'trade'` -> collection, even if the FIRST value was 'trade'), and
/// an empty value is falsy (`|| 'collection'`). qs NEVER throws on
/// malformed % sequences — verified against the container's qs 6.15.3
/// (`qs.parse('a=%ZZ')` returns `{a: '%ZZ'}`), so an undecodable value
/// stays raw and simply fails the 'trade' equality.
fn parse_trade_query(query: &str) -> bool {
    let mut seen = false;
    let mut is_trade = false;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        let key = percent_decode(k.as_bytes()).unwrap_or_else(|| k.to_string());
        if key != "list" {
            continue;
        }
        let value = percent_decode(v.as_bytes()).unwrap_or_else(|| v.to_string());
        if seen {
            return false; // qs array -> !== 'trade'
        }
        seen = true;
        is_trade = value == "trade";
    }
    seen && is_trade
}

/// decodeURIComponent for path/query components ('+' -> space only in
/// query context; path segments keep literal '+'). `None` on invalid
/// percent-sequences (Express throws -> 500).
pub(crate) fn percent_decode(s: &[u8]) -> Option<String> {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'%' => {
                if i + 2 >= s.len() {
                    return None;
                }
                let hi = (s[i + 1] as char).to_digit(16)?;
                let lo = (s[i + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn server_error_body() -> (StatusCode, &'static str) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Failed to retrieve shared collection",
    )
}

/// npm `fresh` (Express req.fresh): a request Cache-Control carrying the
/// standalone `no-cache` directive disables the ETag 304 entirely —
/// verified live against the container's fresh@0.5.2
/// (CACHE_CONTROL_NO_CACHE_REGEXP: case-sensitive comma-delimited
/// directive match; no-store/max-age=0 do NOT match).
fn fresh_allows_304(headers: &axum::http::HeaderMap) -> bool {
    match headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) {
        Some(cc) => !cc.split(',').any(|d| d.trim() == "no-cache"),
        None => true,
    }
}

use crate::util::json_error;

// ---------------------------------------------------------------------------
// Unit tests (pure semantics; live parity is bench/parity.mjs shrd_*)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        m
    }

    #[test]
    fn proxy_copy_reads_as_proxy_rarity_and_zero_price() {
        // Column order mirrors the real shared SELECT: subtypes/types sit
        // mid-row, base_rarity is brand-new at apply time (appends after
        // price_holofoil here), color_identity is brand-new at spread time
        // (appends last).
        let r = row(&[
            ("printing", json!("Holofoil")),
            ("is_proxy", json!(1)),
            ("rarity", json!("Rare")),
            ("subtypes", json!("[\"Creature\"]")),
            ("types", json!("[\"Creature\",\"Goblin\"]")),
            ("price_trend", json!(4.0)),
            ("price_normal", json!(1.0)),
            ("price_holofoil", json!(9.0)),
        ]);
        let f = format_shared_row(&r).unwrap();
        assert_eq!(f["price_trend"], json!(0)); // proxy: worth nothing
        assert_eq!(f["rarity"], json!("Proxy"));
        assert_eq!(f["base_rarity"], json!("Rare"));
        assert_eq!(f["is_proxy"], json!(1));
        let keys: Vec<&String> = f.keys().collect();
        assert_eq!(keys[keys.len() - 2], "base_rarity");
        assert_eq!(keys[keys.len() - 1], "color_identity");
        // subtypes/types kept their row position (before price_trend).
        assert!(keys.iter().position(|k| *k == "subtypes").unwrap() < keys.iter().position(|k| *k == "price_trend").unwrap());
        assert_eq!(f["color_identity"], json!([]));
    }

    #[test]
    fn resolve_price_branches() {
        let base = row(&[("rarity", json!("C")), ("is_proxy", json!(0))]);
        let mut h = base.clone();
        h.insert("printing".into(), json!("Holofoil"));
        h.insert("price_holofoil".into(), json!(3.91));
        h.insert("price_normal".into(), json!(0.32));
        h.insert("price_trend".into(), json!(0.32));
        assert_eq!(format_shared_row(&h).unwrap()["price_trend"], json!(3.91));

        let mut n = base.clone();
        n.insert("printing".into(), json!("Normal"));
        n.insert("price_holofoil".into(), json!(Value::Null));
        n.insert("price_normal".into(), json!(0.12));
        n.insert("price_trend".into(), json!(9.9));
        assert_eq!(format_shared_row(&n).unwrap()["price_trend"], json!(0.12));

        // Normal with null normal price falls through to trend.
        let mut f = base.clone();
        f.insert("printing".into(), json!("Normal"));
        f.insert("price_holofoil".into(), json!(Value::Null));
        f.insert("price_normal".into(), json!(Value::Null));
        f.insert("price_trend".into(), json!(2.5));
        assert_eq!(format_shared_row(&f).unwrap()["price_trend"], json!(2.5));

        // trend null -> 0.
        let mut z = base;
        z.insert("printing".into(), json!("Normal"));
        z.insert("price_holofoil".into(), json!(Value::Null));
        z.insert("price_normal".into(), json!(Value::Null));
        z.insert("price_trend".into(), json!(Value::Null));
        assert_eq!(format_shared_row(&z).unwrap()["price_trend"], json!(0));
    }

    #[test]
    fn bad_array_column_is_error_like_js_parse_throw() {
        let r = row(&[("types", json!("oops")), ("subtypes", json!("[]")), ("is_proxy", json!(0))]);
        assert!(format_shared_row(&r).is_err());
    }

    #[test]
    fn trade_query_qs_semantics() {
        let trade = |q: &str| parse_trade_query(q);
        assert!(!trade(""));
        assert!(!trade("foo=1"));
        assert!(trade("list=trade"));
        assert!(trade("a=1&list=trade&b=2"));
        assert!(!trade("list=collection"));
        assert!(!trade("list=")); // falsy -> 'collection'
        assert!(!trade("list=trade&list=trade")); // qs array !== 'trade'
        assert!(!trade("list=trade&list=foo")); // array, even 2nd non-trade
        assert!(trade("list=trad%65")); // percent-decoded
        // qs does NOT throw on malformed % (container qs 6.15.3:
        // qs.parse('a=%ZZ') -> {a: '%ZZ'}); the raw value simply fails
        // the 'trade' equality.
        assert!(!trade("list=%ZZ"));
        assert!(!trade("other=%ZZ"));
        assert!(trade("list=trade&other=%ZZ")); // other pair keeps raw %ZZ
    }

    #[test]
    fn percent_decode_path_semantics() {
        assert_eq!(percent_decode(b"abc%2Fdef").as_deref(), Some("abc/def"));
        assert_eq!(percent_decode(b"a+b").as_deref(), Some("a+b")); // path: literal +
        assert_eq!(percent_decode(b"%ZZ"), None); // decodeURIComponent throws
        assert_eq!(percent_decode(b"%C3%A9").as_deref(), Some("é"));
    }

    #[test]
    fn object_key_coercions() {
        assert_eq!(js_obj_key(&Value::Null), "null");
        assert_eq!(js_obj_key(&json!("x")), "x");
        assert_eq!(js_obj_key(&json!(2.5)), "2.5");
        assert_eq!(js_obj_key(&json!(true)), "true");
        assert_eq!(js_obj_key(&json!(["a", "b"])), "a,b");
    }
}
