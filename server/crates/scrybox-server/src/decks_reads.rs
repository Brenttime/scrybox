//! Native port of the deck READ routes from `backend/src/routes/decks.js`:
//! `GET /` (deck list), `GET /:id` (deck detail) and `GET /:id/locations`
//! (checkout coverage). Mounted under the `decks_reads` name in RUST_ROUTES;
//! everything else on /api/decks (POSTs, checkout/return, combos, synergy,
//! register-collection) stays proxied to Node untouched. PUT/DELETE on the
//! shared shapes delegate to `decks_writes` when that group is owned
//! (`route_entry` in lib.rs, `handle_card` in decks_writes).
//!
//! Every status, body and message was written against the actual Node source
//! (routes/decks.js:85-156, 337-520, utils/deckPricing.js, deckCommander.js,
//! deckOwnership.js, deckListCache.js, deckDisplayPrintings.js, deckRules.js,
//! priceHelpers.js parseCardRow/applyProxyRarity) plus live byte-pinning
//! against the :3312 container (dk/dr parity cases in bench/parity.mjs).
//!
//! Key JS semantics reproduced deliberately:
//!   * `getOrBuild`'s per-user deck-list cache keyed on deck_list_revision
//!     (utils/deckListCache.js), incl. the X-Deck-List-Cache hit/miss header,
//!     the DECK_LIST_CACHE=0 kill and the "only store if the revision did not
//!     move while building" rule. The store lives on AppState (per process,
//!     like Node's module-level Map).
//!   * `res.json`'s Express-default weak ETag + If-None-Match/304 with the
//!     fresh@0.5.2 standalone-no-cache gate (same twin as search_history).
//!   * JSON key order is the object-literal/spread order of the Node code
//!     (serde_json preserve_order + first-position-stable re-insert).
//!   * `Number()`/`||`/falsy collapse rules on the price paths.
//!   * `String.prototype.localeCompare` (full-icu CLDR root) for the
//!     locations tie-break and the owned-printing pick, via icu_collator's
//!     root collator — 0-diff vs the container's ICU 78.3 on the whole
//!     card_cache name corpus + 22k pairwise probes.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::extract::Request;
use icu_collator::{options::CollatorOptions, Collator, CollatorPreferences};
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::poison::LockExt;
use crate::auth::{bearer_token, AuthUser};
use crate::collection::{body_etag, etag_matches, gzip_bytes, json_num, sqlite_value, wants_encoding};
use crate::collection_writes::{
    internal_error, is_basic_land, json_error, node_exact, parse_body, read_parts, too_large, BodyOutcome,
};
use crate::db::DbPool;
use crate::stats::apply_proxy_rarity;
use crate::AppState;

// ---------------------------------------------------------------------------
// Route entries
// ---------------------------------------------------------------------------

/// GET /api/decks. Non-GET shapes never mount here (POST /api/decks stays
/// fully proxied); defensive proxy anyway.
pub async fn handle_list(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("decks_reads") || st.db.is_none() || req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let pool = st.db.clone().unwrap();
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    // express.json runs globally ahead of the auth gate (iter-28 live pin):
    // a GET carrying an unparseable JSON body answers 500 before auth.
    match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => {}
    }
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match crate::auth::authenticate_async(&pool, token.as_deref(), true).await {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };

    let headers = parts.headers;
    let store = st.deck_list_cache.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(|e| {
            tracing::error!(error = %e, "decks_reads: pool");
        })?;
        build_deck_list_cached(&conn, user.id, &store).map_err(|e| {
            tracing::error!(error = %e, "decks_reads: list build");
        })
    })
    .await;
    let (raw, hit) = match outcome {
        Ok(Ok(v)) => v,
        _ => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve decks"),
    };
    json_read_response(
        &headers,
        raw,
        [("x-deck-list-cache", if hit { "hit" } else { "miss" })],
    )
}

pub async fn handle_detail(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    guard_get(st, req, "decks_reads", |pool, headers, user| async move {
        let id2 = id.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let conn = pool.get().map_err(|e| {
                tracing::error!(error = %e, "decks_reads: pool");
                DetailError::Db
            })?;
            build_deck_detail(&conn, &id2, user.id)
        })
        .await;
        match outcome {
            Ok(Ok(raw)) => json_read_response(&headers, raw, Vec::<(String, String)>::new()),
            Ok(Err(DetailError::NotFound)) => json_error(StatusCode::NOT_FOUND, "Deck not found"),
            Ok(Err(DetailError::Unresolved)) => json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "This deck contains cards whose details are unavailable. Remove or re-import them before continuing.",
            ),
            Ok(Err(DetailError::Db)) | Err(_) => {
                json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve deck details")
            }
        }
    })
    .await
}

pub async fn handle_locations(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    guard_get(st, req, "decks_reads", |pool, headers, user| async move {
        let id2 = id.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let conn = pool.get().map_err(|e| {
                tracing::error!(error = %e, "decks_reads: pool");
                DetailError::Db
            })?;
            build_deck_locations(&conn, &id2, user.id)
        })
        .await;
        match outcome {
            Ok(Ok(raw)) => json_read_response(&headers, raw, Vec::<(String, String)>::new()),
            Ok(Err(DetailError::NotFound)) => json_error(StatusCode::NOT_FOUND, "Deck not found"),
            Ok(Err(DetailError::Unresolved)) => {
                json_error(StatusCode::UNPROCESSABLE_ENTITY, "Deck contains cards with missing metadata")
            }
            Ok(Err(DetailError::Db)) | Err(_) => {
                json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve checkout coverage")
            }
        }
    })
    .await
}

/// Dispatcher for the /api/decks/{id} shape shared by the two decks groups.
/// Method + kill-switch decides; anything this pair does not own proxies.
pub async fn route_entry(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if req.method() == Method::GET && st.owns("decks_reads") {
        return handle_detail(Path(id), State(st), req).await;
    }
    if st.owns("decks_writes") {
        return crate::decks_writes::handle_entry(Path(id), State(st), req).await;
    }
    crate::proxy(State(st), req).await
}

/// Shared prologue: kill switch, body-parse-before-auth, auth gate.
pub(crate) async fn guard_get<F, Fut>(st: AppState, req: Request, group: &str, f: F) -> Response
where
    F: FnOnce(Arc<DbPool>, HeaderMap, AuthUser) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    if !st.owns(group) || st.db.is_none() || req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let pool = st.db.clone().unwrap();
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => {}
    }
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match crate::auth::authenticate_async(&pool, token.as_deref(), true).await {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    f(pool, parts.headers, user).await
}

/// Express-default-body-ETag JSON read: If-None-Match + fresh -> 304 with the
/// tag and no body (Content-Type is removed on 304 by res.send), else 200 with
/// content-type/etag (+ optional extra headers, compression like the shared
/// module's res.json-through-compression path).
pub(crate) fn json_read_response<I, T>(headers: &HeaderMap, raw: Vec<u8>, extras: I) -> Response
where
    I: IntoIterator<Item = (T, T)>,
    T: Into<String>,
{
    let tag = body_etag(&raw);
    let extras: Vec<(String, String)> =
        extras.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
    if fresh_allows_304(headers) {
        if let Some(inm) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
            if etag_matches(Some(inm), &tag) {
                let mut b = Response::builder().status(StatusCode::NOT_MODIFIED).header(header::ETAG, &tag);
                for (k, v) in extras {
                    if let (Ok(k), Ok(v)) = (k.parse::<header::HeaderName>(), v.parse::<HeaderValue>()) {
                        b = b.header(k, v);
                    }
                }
                return b.body(Body::empty()).unwrap();
            }
        }
    }
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let use_gzip = wants_encoding(ae, "gzip", raw.len());
    let bytes = if use_gzip { Body::from(gzip_bytes(&raw)) } else { Body::from(raw) };
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, tag);
    if use_gzip {
        resp = resp
            .header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"))
            .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    for (k, v) in extras {
        if let (Ok(k), Ok(v)) = (k.parse::<header::HeaderName>(), v.parse::<HeaderValue>()) {
            resp = resp.header(k, v);
        }
    }
    resp.body(bytes).unwrap()
}

/// npm `fresh` (Express req.fresh): a request Cache-Control carrying the
/// standalone `no-cache` directive disables the ETag 304 entirely —
/// live-pinned vs the container's fresh@0.5.2 (iter-27).
pub(crate) fn fresh_allows_304(headers: &HeaderMap) -> bool {
    match headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) {
        Some(cc) => !cc.split(',').any(|d| d.trim() == "no-cache"),
        None => true,
    }
}

// ---------------------------------------------------------------------------
// JS numeric helpers
// ---------------------------------------------------------------------------

/// ECMAScript WhiteSpace + LineTerminator set used by Number(String).
/// Identical to the JS regex `\s` class (used by the decklist importer).
pub(crate) fn js_trim(s: &str) -> &str {
    let is_ws = |c: char| {
        matches!(
            c,
            ' ' | '\t' | '\u{000b}' | '\u{000c}' | '\r' | '\n' | '\u{00a0}' | '\u{feff}'
                | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
        )
    };
    let t = s.trim_start_matches(is_ws);
    t.trim_end_matches(is_ws)
}

/// JS `Number(x)` over a driver/JSON value (ToPrimitive first, so arrays
/// stringify through join(',')).
pub(crate) fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN, // undefined
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(other) => {
            let s = js_to_string(Some(other));
            let s = js_trim(&s);
            if s.is_empty() {
                return 0.0;
            }
            match s {
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                _ => {
                    let (sign, body) = match s.as_bytes()[0] {
                        b'-' => (-1.0, &s[1..]),
                        b'+' => (1.0, &s[1..]),
                        _ => (1.0, s),
                    };
                    if body.len() > 2 && body.starts_with("0x") {
                        return i64::from_str_radix(&body[2..], 16).map(|i| sign * i as f64).unwrap_or(f64::NAN);
                    }
                    js_decimal(body).map(|f| sign * f).unwrap_or(f64::NAN)
                }
            }
        }
    }
}

/// Strict decimal/exponent grammar (Rust's f64 parser accepts `inf`/`_`/
/// leading `+`, JS's does not; we pre-validate the token).
fn js_decimal(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    // Optional exponent (JS Number accepts 1e3 / 1e+3 / 1E-3).
    if i < b.len() && (b[i] | 0x20) == b'e' {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let mut exp_digits = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            exp_digits += 1;
        }
        if exp_digits == 0 {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    s.parse::<f64>().ok()
}

/// JS `String(x)` for values the driver can return.
pub(crate) fn js_to_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|e| match e {
                Value::Null => String::new(),
                Value::Array(inner) => inner
                    .iter()
                    .map(|e2| match e2 {
                        Value::Null => String::new(),
                        _ => js_to_string(Some(e2)),
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                _ => js_to_string(Some(e)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// JS `x || y` over driver values (falsy: false, 0, -0, NaN, "", null, undefined).
fn js_or<'a>(v: Option<&'a Value>, fallback: &'a Value) -> &'a Value {
    match v {
        None | Some(Value::Null) => fallback,
        Some(Value::Bool(false)) => fallback,
        Some(Value::String(s)) if s.is_empty() => fallback,
        Some(Value::Number(n)) => {
            if n.as_f64().map(|f| f == 0.0 || f.is_nan()).unwrap_or(false) {
                fallback
            } else {
                v.unwrap()
            }
        }
        Some(x) => x,
    }
}

/// JS `Math.round` (half up toward +Infinity = floor(x + 0.5) per spec).
fn js_math_round(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    (x + 0.5).floor()
}

/// `Math.round((v + Number.EPSILON) * 100) / 100` rendered through the
/// JSON.stringify number rules.
pub(crate) fn round_cents(v: f64) -> Value {
    let r = js_math_round((v + f64::EPSILON) * 100.0) / 100.0;
    json_num(r)
}

// ---------------------------------------------------------------------------
// Price helpers — utils/deckPricing.js
// ---------------------------------------------------------------------------

const PRICE_FIELDS: [&str; 4] = ["price_normal", "price_holofoil", "price_etched", "price_trend"];

fn cheapest_eligible_price(row: &Map<String, Value>) -> Option<f64> {
    let mut cheapest: Option<f64> = None;
    for field in PRICE_FIELDS {
        let value = js_number(row.get(field));
        if !value.is_finite() || value <= 0.0 {
            continue;
        }
        if cheapest.is_none_or(|c| value < c) {
            cheapest = Some(value);
        }
    }
    cheapest
}

/// deckPricing.currentPrintingPrice — USD gate with the same COALESCE
/// semantics as the floor query.
pub(crate) fn current_printing_price(row: &Map<String, Value>) -> Option<f64> {
    let currency = match row.get("price_currency") {
        None | Some(Value::Null) => "USD".to_string(),
        Some(v) => js_to_string(Some(v)).to_uppercase(),
    };
    if currency != "USD" {
        return None;
    }
    cheapest_eligible_price(row)
}

/// deckPricing.deckCurrentPrintValues over formatted card rows.
pub(crate) fn deck_current_print_values(rows: &[Map<String, Value>]) -> Map<String, Value> {
    let mut value = 0f64;
    let mut unpriced_copies = 0f64;
    let mut estimated_copies = 0f64;
    for row in rows {
        let quantity = {
            let q = js_number(row.get("quantity"));
            if q.is_nan() {
                0.0
            } else {
                q
            }
        };
        if quantity <= 0.0 {
            continue;
        }
        match row.get("current_price") {
            None | Some(Value::Null) => unpriced_copies += quantity,
            Some(price) => {
                let price = js_number(Some(price));
                value += quantity * price;
                if js_number(row.get("price_estimated")) != 0.0 && !js_number(row.get("price_estimated")).is_nan()
                    && price > 0.0
                {
                    estimated_copies += quantity;
                }
            }
        }
    }
    let mut out = Map::new();
    out.insert("current_printing_value".into(), round_cents(value));
    out.insert("current_unpriced_cards".into(), json_num(unpriced_copies));
    out.insert("current_estimated_cards".into(), json_num(estimated_copies));
    out
}

pub(crate) fn empty_deck_minimum_value() -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("minimum_value".into(), Value::from(0));
    out.insert("minimum_value_currency".into(), Value::from("USD"));
    out.insert("unpriced_cards".into(), Value::from(0));
    out.insert("unpriced_card_types".into(), Value::from(0));
    out
}

pub(crate) fn sql_card_key(alias: &str) -> String {
    format!("LOWER(TRIM(COALESCE({alias}.name, '')))")
}

pub(crate) fn chunked<T>(values: &[T], size: usize) -> Vec<&[T]> {
    values.chunks(size).collect()
}

// ---------------------------------------------------------------------------
// DB row plumbing — node-sqlite3 row objects (duplicate column names keep the
// first position, last value — serde_json's preserve_order Map insert has the
// same semantics).
// ---------------------------------------------------------------------------

pub(crate) fn rows_map(conn: &Connection, sql: &str, params: &[SqlValue]) -> Result<Vec<Map<String, Value>>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(sql)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let mut m = Map::with_capacity(cols.len());
        for (i, c) in cols.iter().enumerate() {
            m.insert(c.clone(), sqlite_value(r, i)?);
        }
        out.push(m);
    }
    Ok(out)
}

pub(crate) fn one_row(conn: &Connection, sql: &str, params: &[SqlValue]) -> Result<Option<Map<String, Value>>, rusqlite::Error> {
    Ok(rows_map(conn, sql, params)?.into_iter().next())
}

// ---------------------------------------------------------------------------
// getDeckRequirements / getCheapestPrintings / computeMinimumValues
// ---------------------------------------------------------------------------

fn get_deck_requirements(conn: &Connection, deck_ids: &[i64]) -> Result<Vec<Map<String, Value>>, rusqlite::Error> {
    let mut requirements = Vec::new();
    for ids in chunked(deck_ids, 500) {
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!(
            "
      SELECT
        dc.deck_id,
        COALESCE(NULLIF({}, ''), 'missing:' || dc.card_id) AS card_key,
        SUM(dc.quantity) AS quantity
      FROM deck_cards dc
      LEFT JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
      WHERE dc.deck_id IN ({placeholders})
        AND dc.quantity > 0
      GROUP BY dc.deck_id, card_key
    ",
            sql_card_key("deck_cc")
        );
        let params: Vec<SqlValue> = ids.iter().map(|i| SqlValue::Integer(*i)).collect();
        requirements.extend(rows_map(conn, &sql, &params)?);
    }
    Ok(requirements)
}

pub(crate) struct CheapestPrinting {
    pub(crate) card_id: Value,
    price: f64,
}

pub(crate) fn get_cheapest_printings(
    conn: &Connection,
    card_keys: &[String],
) -> Result<HashMap<String, CheapestPrinting>, rusqlite::Error> {
    let mut best: HashMap<String, CheapestPrinting> = HashMap::new();
    for keys in chunked(card_keys, 500) {
        let value_rows = vec!["(?)"; keys.len()].join(", ");
        let sql = format!(
            "
      WITH required(card_key) AS (VALUES {value_rows})
      SELECT
        {} AS card_key,
        priced_cc.id,
        priced_cc.price_normal,
        priced_cc.price_holofoil,
        priced_cc.price_etched,
        priced_cc.price_trend
      FROM required
      CROSS JOIN card_cache priced_cc INDEXED BY idx_card_cache_logical_key
      WHERE {} = required.card_key
        AND UPPER(COALESCE(priced_cc.price_currency, 'USD')) = 'USD'
        AND COALESCE(priced_cc.price_estimated, 0) = 0
    ",
            sql_card_key("priced_cc"),
            sql_card_key("priced_cc")
        );
        let params: Vec<SqlValue> = keys.iter().map(|k| SqlValue::Text(k.clone())).collect();
        for row in rows_map(conn, &sql, &params)? {
            let Some(price) = cheapest_eligible_price(&row) else { continue };
            let key = match row.get("card_key") {
                Some(Value::String(s)) => s.clone(),
                _ => continue, // NULL key rows are never requested keys
            };
            let id = row.get("id").cloned().unwrap_or(Value::Null);
            // Tie-break on String(id) lexicographic (JS `<` = UTF-16 code
            // units — Rust's [u16] comparison is the same order).
            let replace = match best.get(&key) {
                None => true,
                Some(cur) => {
                    price < cur.price
                        || (price == cur.price && js_str_lt(&js_to_string(Some(&id)), &js_to_string(Some(&cur.card_id))))
                }
            };
            if replace {
                best.insert(key, CheapestPrinting { card_id: id, price });
            }
        }
    }
    Ok(best)
}

fn js_str_lt(a: &str, b: &str) -> bool {
    a.encode_utf16().cmp(b.encode_utf16()) == std::cmp::Ordering::Less
}

pub(crate) fn get_cheapest_prices(conn: &Connection, card_keys: &[String]) -> Result<HashMap<String, f64>, rusqlite::Error> {
    Ok(get_cheapest_printings(conn, card_keys)?
        .into_iter()
        .map(|(k, p)| (k, p.price))
        .collect())
}

/// getDeckMinimumValues + computeMinimumValues (deck flavour).
fn get_deck_minimum_values(conn: &Connection, deck_ids: &[i64]) -> Result<HashMap<i64, Map<String, Value>>, rusqlite::Error> {
    // [...new Set(deckIds.map(Number).filter(Number.isSafeInteger))]
    let mut normalized: Vec<i64> = Vec::new();
    for id in deck_ids {
        if id.abs() <= 9_007_199_254_740_991 && !normalized.contains(id) {
            normalized.push(*id);
        }
    }
    let mut values: HashMap<i64, (f64, f64, i64)> = normalized.iter().map(|id| (*id, (0.0, 0.0, 0))).collect();
    if normalized.is_empty() {
        return Ok(HashMap::new());
    }
    let requirements = get_deck_requirements(conn, &normalized)?;

    // unique truthy non-missing keys, first-seen order
    let mut card_keys: Vec<String> = Vec::new();
    for r in &requirements {
        if let Some(Value::String(k)) = r.get("card_key") {
            if !k.is_empty() && !k.starts_with("missing:") && !card_keys.contains(k) {
                card_keys.push(k.clone());
            }
        }
    }
    let cheapest = get_cheapest_prices(conn, &card_keys)?;

    for r in &requirements {
        let Some(owner) = r.get("deck_id").and_then(|v| v.as_i64()) else { continue };
        let Some(value) = values.get_mut(&owner) else { continue };
        let quantity = {
            let q = js_number(r.get("quantity"));
            if q.is_nan() {
                0.0
            } else {
                q
            }
        };
        if quantity <= 0.0 {
            continue;
        }
        match r.get("card_key").and_then(|v| v.as_str()).and_then(|k| cheapest.get(k)) {
            None => {
                value.1 += quantity;
                value.2 += 1;
            }
            Some(price) => value.0 += quantity * price,
        }
    }

    Ok(values
        .into_iter()
        .map(|(id, (minimum, unpriced_cards, unpriced_types))| {
            let mut m = empty_deck_minimum_value();
            m.insert("minimum_value".into(), round_cents(minimum));
            m.insert("unpriced_cards".into(), json_num(unpriced_cards));
            m.insert("unpriced_card_types".into(), Value::from(unpriced_types));
            (id, m)
        })
        .collect())
}

// ---------------------------------------------------------------------------
// getDeckCommanders (utils/deckCommander.js, verbatim SQL)
// ---------------------------------------------------------------------------

fn word_match(word: &str) -> String {
    format!("INSTR(kind_json, '\"{word}\"')")
}

fn commander_sql() -> String {
    format!(
        "
  WITH deck_rows AS (
    SELECT
      d.id                          AS deck_id,
      dc.card_id,
      COALESCE(dc.quantity, 0)      AS quantity,
      COALESCE(dc.is_commander, 0)  AS is_commander,
      cc.name,
      NULLIF(cc.image_url, '')      AS image_url,
      COALESCE(cc.cmc, 0)           AS cmc,
      CASE WHEN json_valid(COALESCE(cc.color_identity, '[]'))
           THEN COALESCE(cc.color_identity, '[]') ELSE '[]' END AS ci_json,
      LOWER(COALESCE(cc.types, '[]') || COALESCE(cc.subtypes, '[]')) AS kind_json
    FROM decks d
    JOIN deck_cards dc ON dc.deck_id = d.id
    JOIN card_cache cc ON cc.id = dc.card_id
    WHERE d.user_id = ?
  ),
  row_colors AS (
    SELECT DISTINCT
      deck_id,
      card_id,
      CASE UPPER(je.value)
        WHEN 'W' THEN 'W' WHEN 'WHITE' THEN 'W'
        WHEN 'U' THEN 'U' WHEN 'BLUE'  THEN 'U'
        WHEN 'B' THEN 'B' WHEN 'BLACK' THEN 'B'
        WHEN 'R' THEN 'R' WHEN 'RED'   THEN 'R'
        WHEN 'G' THEN 'G' WHEN 'GREEN' THEN 'G'
      END AS letter
    FROM deck_rows
    CROSS JOIN json_each(deck_rows.ci_json) je
  ),
  deck_colors AS (
    -- A deck's color identity is the union of its cards' identities.
    SELECT DISTINCT deck_id, letter FROM row_colors WHERE letter IS NOT NULL
  ),
  candidates AS (
    SELECT *
    FROM deck_rows
    WHERE is_commander = 1
       OR ({legendary} > 0 AND {creature} > 0)
       OR {companion} > 0
       OR {mate} > 0
       OR {mandate} > 0
       OR {council} > 0
       OR {carriage} > 0
       OR {agent} > 0
  ),
  overlap AS (
    -- How much of the candidate's identity the deck actually plays.
    SELECT c.deck_id, c.card_id, COUNT(DISTINCT rc.letter) AS color_overlap
    FROM candidates c
    JOIN row_colors  rc  ON rc.deck_id  = c.deck_id AND rc.card_id  = c.card_id
    JOIN deck_colors dc2 ON dc2.deck_id = c.deck_id AND dc2.letter  = rc.letter
    GROUP BY c.deck_id, c.card_id
  ),
  ranked AS (
    SELECT
      c.deck_id,
      c.card_id   AS commander_card_id,
      c.name      AS commander_name,
      c.image_url AS commander_image_url,
      ROW_NUMBER() OVER (
        PARTITION BY c.deck_id
        ORDER BY c.is_commander DESC,
                 CASE WHEN c.quantity > 0 THEN 0 ELSE 1 END,
                 COALESCE(o.color_overlap, 0) DESC,
                 c.quantity DESC,
                 c.cmc,
                 c.name,
                 c.card_id
      ) AS rn
    FROM candidates c
    LEFT JOIN overlap o ON o.deck_id = c.deck_id AND o.card_id = c.card_id
  )
  SELECT deck_id, commander_card_id, commander_name, commander_image_url
  FROM ranked
  WHERE rn = 1
",
        legendary = word_match("legendary"),
        creature = word_match("creature"),
        companion = word_match("companion"),
        mate = word_match("mate"),
        mandate = word_match("mandate"),
        council = word_match("council"),
        carriage = word_match("carriage"),
        agent = word_match("agent"),
    )
}

fn get_deck_commanders(conn: &Connection, user_id: i64) -> Result<HashMap<i64, Map<String, Value>>, rusqlite::Error> {
    let rows = rows_map(conn, &commander_sql(), &[SqlValue::Integer(user_id)])?;
    let mut out = HashMap::new();
    for r in rows {
        if let Some(id) = r.get("deck_id").and_then(|v| v.as_i64()) {
            let mut m = Map::new();
            for k in ["commander_card_id", "commander_name", "commander_image_url"] {
                m.insert(k.into(), r.get(k).cloned().unwrap_or(Value::Null));
            }
            out.insert(id, m);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// getDeckOwnership (utils/deckOwnership.js, verbatim SQL)
// ---------------------------------------------------------------------------

fn get_deck_ownership(conn: &Connection, user_id: i64) -> Result<HashMap<i64, (i64, i64)>, rusqlite::Error> {
    let sql = format!(
        "
  WITH need AS (
    SELECT
      dc.deck_id,
      {key} AS card_key,
      SUM(dc.quantity) AS need_qty,
      MAX({basic}) AS is_basic
    FROM deck_cards dc
    JOIN decks d ON d.id = dc.deck_id
    JOIN card_cache cc ON cc.id = dc.card_id
    WHERE d.user_id = ? AND dc.quantity > 0
    GROUP BY dc.deck_id, {key}
  ),
  owned AS (
    SELECT {key_o} AS card_key, SUM(o.quantity) AS owned_qty
    FROM card_cache occ
    CROSS JOIN collection o
    WHERE o.card_id = occ.id AND o.user_id = ? AND o.quantity > 0
      AND {key_o} IN (SELECT card_key FROM need)
    GROUP BY {key_o}
  )
  SELECT
    need.deck_id,
    SUM(CASE WHEN need.is_basic = 0 AND COALESCE(owned.owned_qty, 0) < need.need_qty
             THEN 1 ELSE 0 END) AS missing_card_types,
    SUM(CASE WHEN need.is_basic = 0 AND COALESCE(owned.owned_qty, 0) < need.need_qty
             THEN need.need_qty - COALESCE(owned.owned_qty, 0) ELSE 0 END) AS missing_copies
  FROM need
  LEFT JOIN owned ON owned.card_key = need.card_key
  GROUP BY need.deck_id
",
        key = sql_card_key("cc"),
        basic = crate::collection_writes::sql_is_basic_land_expr("cc"),
        key_o = sql_card_key("occ"),
    );
    let rows = rows_map(conn, &sql, &[SqlValue::Integer(user_id), SqlValue::Integer(user_id)])?;
    let mut out = HashMap::new();
    for r in rows {
        if let Some(id) = r.get("deck_id").and_then(|v| v.as_i64()) {
            let num = |k: &str| r.get(k).and_then(|v| v.as_f64()).filter(|f| f.is_finite()).unwrap_or(0.0) as i64; // Number(x) || 0
            out.insert(id, (num("missing_card_types"), num("missing_copies")));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// applyOwnedPrintings (utils/deckDisplayPrintings.js)
// ---------------------------------------------------------------------------

const PRINTING_FIELDS: [&str; 13] = [
    "id", "printed_name", "rarity", "set_id", "set_name", "number", "image_url",
    "price_trend", "price_normal", "price_holofoil", "price_etched", "price_currency", "price_estimated",
];

pub(crate) fn root_collator() -> icu_collator::CollatorBorrowed<'static> {
    Collator::try_new(
        CollatorPreferences::from_locale_strict(&icu_locale::Locale::try_from_str("und").unwrap()).unwrap(),
        CollatorOptions::default(),
    )
    .unwrap()
}

fn choose_printings(rows: &[Map<String, Value>], owned: &[Map<String, Value>]) -> HashMap<String, Value> {
    let mut by_key: HashMap<String, Vec<(Value, f64)>> = HashMap::new();
    let mut key_order: Vec<String> = Vec::new();
    for o in owned {
        if let Some(Value::String(k)) = o.get("card_key") {
            let entry = by_key.entry(k.clone()).or_default();
            if entry.is_empty() {
                key_order.push(k.clone());
            }
            entry.push((
                o.get("card_id").cloned().unwrap_or(Value::Null),
                js_number(o.get("qty")),
            ));
        }
    }
    let _ = key_order;
    let coll = root_collator();
    let mut picks = HashMap::new();
    for row in rows {
        let Some(Value::String(key)) = row.get("card_key") else { continue };
        let Some(candidates) = by_key.get(key) else { continue };
        if candidates.is_empty() {
            continue;
        }
        if candidates.iter().any(|(id, _)| Some(id) == row.get("id")) {
            continue;
        }
        // [...candidates].sort((a,b)=>(b.qty-a.qty)||String(a.card_id).localeCompare(String(b.card_id)))[0]
        // — stable full sort's first element == argmin under that total order.
        let mut best: Option<&(Value, f64)> = None;
        for c in candidates {
            best = Some(match best {
                None => c,
                Some(b) => {
                    if c.1 > b.1 {
                        c
                    } else if c.1 == b.1
                        && coll.compare(&js_to_string(Some(&c.0)), &js_to_string(Some(&b.0))) == std::cmp::Ordering::Less
                    {
                        c
                    } else {
                        b
                    }
                }
            });
        }
        if let Some((id, _)) = best {
            picks.insert(key.clone(), id.clone());
        }
    }
    picks
}

fn apply_owned_printings(conn: &Connection, user_id: i64, rows: Vec<Map<String, Value>>) -> Result<Vec<Map<String, Value>>, rusqlite::Error> {
    let mut keys: Vec<String> = Vec::new();
    for r in &rows {
        if let Some(Value::String(k)) = r.get("card_key") {
            if !k.is_empty() && !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    if keys.is_empty() {
        return Ok(rows);
    }
    let marks = vec!["?"; keys.len()].join(",");
    let owned_sql = format!(
        "
    SELECT {} AS card_key, c.card_id, SUM(c.quantity) AS qty
    FROM card_cache cc
    CROSS JOIN collection c
    WHERE c.card_id = cc.id AND c.user_id = ? AND c.quantity > 0
      AND {} IN ({marks})
    GROUP BY c.card_id
  ",
        sql_card_key("cc"),
        sql_card_key("cc")
    );
    let mut params: Vec<SqlValue> = vec![SqlValue::Integer(user_id)];
    params.extend(keys.iter().map(|k| SqlValue::Text(k.clone())));
    let owned = rows_map(conn, &owned_sql, &params)?;
    let picks = choose_printings(&rows, &owned);
    if picks.is_empty() {
        return Ok(rows);
    }

    let mut ids: Vec<String> = Vec::new();
    for id in picks.values() {
        let s = js_to_string(Some(id));
        if !ids.contains(&s) {
            ids.push(s);
        }
    }
    let marks = vec!["?"; ids.len()].join(",");
    let printings_sql = format!(
        "SELECT {} FROM card_cache WHERE id IN ({marks})",
        PRINTING_FIELDS.join(", ")
    );
    let params: Vec<SqlValue> = ids.iter().map(|i| SqlValue::Text(i.clone())).collect();
    let printings = rows_map(conn, &printings_sql, &params)?;
    let mut by_id: HashMap<String, Map<String, Value>> = HashMap::new();
    for p in printings {
        if let Some(id) = p.get("id").map(|v| js_to_string(Some(v))) {
            by_id.insert(id, p);
        }
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            let key = match row.get("card_key") {
                Some(Value::String(k)) => k.clone(),
                _ => return row,
            };
            let Some(pick) = picks.get(&key) else { return row };
            let Some(printing) = by_id.get(&js_to_string(Some(pick))) else { return row };
            let mut merged = row.clone();
            for f in PRINTING_FIELDS {
                if let Some(v) = printing.get(f) {
                    merged.insert(f.into(), v.clone());
                }
            }
            merged.insert("deck_list_card_id".into(), row.get("id").cloned().unwrap_or(Value::Null));
            merged
        })
        .collect())
}

// ---------------------------------------------------------------------------
// parseCardRow (utils/priceHelpers.js)
// ---------------------------------------------------------------------------

/// JSON.parse(row.col || '[]') — ANY JSON value (Node only throws later on
/// `.forEach` of a non-array, which these routes never call). Err = JSON.parse
/// threw -> the route's catch answered 500. (Same twin as shared.rs; noted
/// deviation: JSON.parse's Infinity/NaN literals, unreachable for cached rows.)
fn parse_array_col(v: Option<&Value>) -> Result<Value, ()> {
    match v {
        None | Some(Value::Null) => Ok(Value::Array(Vec::new())),
        Some(Value::String(s)) if s.is_empty() => Ok(Value::Array(Vec::new())),
        Some(Value::String(s)) => serde_json::from_str(s).map_err(|_| ()),
        Some(other) => Ok(other.clone()),
    }
}

pub(crate) fn parse_card_row(row: &Map<String, Value>) -> Result<Map<String, Value>, ()> {
    let mut out = apply_proxy_rarity(row.clone());
    out.insert("subtypes".into(), parse_array_col(row.get("subtypes"))?);
    out.insert("types".into(), parse_array_col(row.get("types"))?);
    out.insert("color_identity".into(), parse_array_col(row.get("color_identity"))?);
    Ok(out)
}

// ---------------------------------------------------------------------------
// GET /api/decks — buildDeckList + deckListCache.getOrBuild
// ---------------------------------------------------------------------------

const LIST_SQL: &str = "
      SELECT
        d.id,
        d.name,
        d.description,
        d.format,
        d.category,
        d.accent_color,
        d.target_size,
        d.created_at,
        d.checked_out,
        d.checked_out_at,
        d.source,
        d.moxfield_public_id,
        COUNT(DISTINCT CASE
          WHEN dc.quantity > 0 THEN CASE
            WHEN deck_cc.id IS NULL THEN 'missing:' || dc.card_id
            ELSE LOWER(TRIM(COALESCE(deck_cc.name, '')))
          END
        END) as total_card_types,
        COALESCE(SUM(CASE WHEN dc.quantity > 0 THEN dc.quantity ELSE 0 END), 0) as total_cards,
        COUNT(DISTINCT CASE WHEN dc.quantity > 0 AND deck_cc.id IS NULL THEN dc.card_id END) as unresolved_card_types
      FROM decks d
      LEFT JOIN deck_cards dc ON d.id = dc.deck_id
      LEFT JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
      WHERE d.user_id = ?
      GROUP BY d.id
      ORDER BY d.created_at DESC
    ";

fn current_revision(conn: &Connection) -> Result<Option<i64>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached("SELECT revision FROM deck_list_revision WHERE id = 1")?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(r) => Ok(r.get::<_, Option<i64>>(0)?),
        None => Ok(None),
    }
}

fn cache_enabled() -> bool {
    std::env::var("DECK_LIST_CACHE").map(|v| v != "0").unwrap_or(true)
}

/// getOrBuild + buildDeckList; returns (body bytes, hit).
fn build_deck_list_cached(
    conn: &Connection,
    user_id: i64,
    store: &std::sync::Mutex<HashMap<i64, (i64, Vec<u8>)>>,
) -> Result<(Vec<u8>, bool), rusqlite::Error> {
    build_deck_list_cached_with(conn, user_id, store, |c| build_deck_list(c, user_id))
}

/// Literal port of deckListCache.js getOrBuild (Node lines 19-24): the
/// revision is captured ONCE before the build; the body is stored only if
/// that same pre-build revision is still current afterwards. Reading the
/// revision twice after the build (the pre-fix shape) would happily cache a
/// body that was assembled while the data moved under it (H2). `build` is
/// injectable so tests can race a real revision-bumping write against the
/// build; production passes build_deck_list.
fn build_deck_list_cached_with(
    conn: &Connection,
    user_id: i64,
    store: &std::sync::Mutex<HashMap<i64, (i64, Vec<u8>)>>,
    build: impl Fn(&Connection) -> Result<Vec<u8>, rusqlite::Error>,
) -> Result<(Vec<u8>, bool), rusqlite::Error> {
    // Node: `const revision = await currentRevision(conn);` before build().
    let revision = if cache_enabled() { current_revision(conn)? } else { None };
    if let Some(revision) = revision {
        if let Some(hit) = store.guard().get(&user_id) {
            if hit.0 == revision {
                return Ok((hit.1.clone(), true));
            }
        }
    }
    let body = build(conn)?;
    // "Only store if nothing moved while building": compare against the
    // PRE-build revision (Node: `revision != null && (await
    // currentRevision(conn)) === revision`), so a mid-build bump drops the body.
    if let Some(revision) = revision {
        if current_revision(conn)? == Some(revision) {
            store.guard().insert(user_id, (revision, body.clone()));
        }
    }
    Ok((body, false))
}

fn build_deck_list(conn: &Connection, user_id: i64) -> Result<Vec<u8>, rusqlite::Error> {
    let rows = rows_map(conn, LIST_SQL, &[SqlValue::Integer(user_id)])?;
    let ids: Vec<i64> = rows.iter().filter_map(|r| r.get("id").and_then(|v| v.as_i64())).collect();
    let values = get_deck_minimum_values(conn, &ids)?;
    let commanders = get_deck_commanders(conn, user_id)?;
    let ownership = get_deck_ownership(conn, user_id)?;

    let out: Vec<Value> = rows
        .into_iter()
        .map(|mut row| {
            let id = row.get("id").and_then(|v| v.as_i64());
            let own = id.and_then(|i| ownership.get(&i)).cloned().unwrap_or((0, 0));
            row.insert("missing_card_types".into(), Value::from(own.0));
            row.insert("missing_copies".into(), Value::from(own.1));
            let mins = id.and_then(|i| values.get(&i)).cloned().unwrap_or_else(empty_deck_minimum_value);
            for k in ["minimum_value", "minimum_value_currency", "unpriced_cards", "unpriced_card_types"] {
                row.insert(k.into(), mins.get(k).cloned().unwrap_or(Value::Null));
            }
            let commander = id.and_then(|i| commanders.get(&i));
            for k in ["commander_name", "commander_image_url", "commander_card_id"] {
                row.insert(
                    k.into(),
                    commander.and_then(|c| c.get(k)).cloned().unwrap_or(Value::Null),
                );
            }
            Value::Object(row)
        })
        .collect();
    Ok(serde_json::to_vec(&Value::Array(out)).unwrap_or_else(|_| b"[]".to_vec()))
}

// ---------------------------------------------------------------------------
// GET /api/decks/:id
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum DetailError {
    NotFound,
    Unresolved,
    Db,
}

fn unresolved_count(conn: &Connection, deck_id: &str) -> Result<f64, rusqlite::Error> {
    let sql = "
    SELECT COUNT(*) AS count
    FROM deck_cards dc
    LEFT JOIN card_cache cc ON cc.id = dc.card_id
    WHERE dc.deck_id = ? AND dc.quantity > 0 AND cc.id IS NULL
  ";
    let row = one_row(conn, sql, &[SqlValue::Text(deck_id.to_string())])?;
    // Number(row && row.count) || 0
    Ok(row
        .as_ref()
        .and_then(|r| r.get("count"))
        .and_then(|v| v.as_f64())
        .filter(|f| f.is_finite())
        .unwrap_or(0.0))
}

fn cards_query() -> String {
    format!(
        "
      WITH requested AS (
        SELECT
          {key} AS card_key,
          MIN(dc.card_id) AS representative_id,
          SUM(dc.quantity) AS quantity,
          MAX(COALESCE(dc.is_commander, 0)) AS is_commander
        FROM deck_cards dc
        JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
        WHERE dc.deck_id = ? AND dc.quantity > 0
        GROUP BY {key}
      ),
      owned AS (
        SELECT {key_o} AS card_key, SUM(owned.quantity) AS owned_qty
        FROM card_cache owned_cc
        CROSS JOIN collection owned
        WHERE owned.card_id = owned_cc.id AND owned.user_id = ? AND owned.quantity > 0
          AND {key_o} IN (SELECT card_key FROM requested)
        GROUP BY {key_o}
      )
      SELECT
        requested.quantity,
        requested.card_key,
        requested.is_commander,
        cc.cmc,
        cc.color_identity,
        cc.oracle_text,
        cc.oracle_faces,
        cc.id,
        cc.name, cc.printed_name,
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
        cc.price_etched,
        cc.price_currency,
        cc.price_estimated,
        COALESCE(owned.owned_qty, 0) AS owned_qty
      FROM requested
      JOIN card_cache cc ON cc.id = requested.representative_id
      LEFT JOIN owned ON owned.card_key = requested.card_key
      ORDER BY cc.name
    ",
        key = sql_card_key("deck_cc"),
        key_o = sql_card_key("owned_cc"),
    )
}

fn maybe_query() -> String {
    format!(
        "
      WITH owned AS (
        SELECT {key_o} AS card_key, SUM(owned.quantity) AS owned_qty
        FROM card_cache owned_cc
        CROSS JOIN collection owned
        WHERE owned.card_id = owned_cc.id AND owned.user_id = ? AND owned.quantity > 0
          AND {key_o} IN (
            SELECT {key_m} FROM deck_maybe_cards maybe_dm
            JOIN card_cache maybe_cc ON maybe_cc.id = maybe_dm.card_id
            WHERE maybe_dm.deck_id = ?)
        GROUP BY {key_o}
      )
      SELECT dm.quantity, {key} AS card_key,
        cc.id, cc.name, cc.printed_name, cc.supertype, cc.subtypes, cc.types, cc.rarity,
        cc.set_id, cc.set_name, cc.number, cc.image_url, cc.cmc, cc.color_identity,
        cc.price_trend, cc.price_normal, cc.price_holofoil, cc.price_etched, cc.price_currency, cc.price_estimated,
        COALESCE(owned.owned_qty, 0) AS owned_qty
      FROM deck_maybe_cards dm
      JOIN card_cache cc ON cc.id = dm.card_id
      LEFT JOIN owned ON owned.card_key = {key}
      WHERE dm.deck_id = ? AND dm.quantity > 0
      ORDER BY cc.name
    ",
        key_o = sql_card_key("owned_cc"),
        key_m = sql_card_key("maybe_cc"),
        key = sql_card_key("cc"),
    )
}

fn build_deck_detail(conn: &Connection, deck_id: &str, user_id: i64) -> Result<Vec<u8>, DetailError> {
    let deck = one_row(
        conn,
        "SELECT * FROM decks WHERE id = ? AND user_id = ?",
        &[SqlValue::Text(deck_id.to_string()), SqlValue::Integer(user_id)],
    )
    .map_err(|_| DetailError::Db)?;
    let Some(deck) = deck else { return Err(DetailError::NotFound) };
    if unresolved_count(conn, deck_id).map_err(|_| DetailError::Db)? > 0.0 {
        return Err(DetailError::Unresolved);
    }

    let cards_rows = rows_map(
        conn,
        &cards_query(),
        &[SqlValue::Text(deck_id.to_string()), SqlValue::Integer(user_id)],
    )
    .map_err(|_| DetailError::Db)?;
    // Show the user's own copy, not the deck list's printing.
    let cards = apply_owned_printings(conn, user_id, cards_rows).map_err(|_| DetailError::Db)?;

    let mut keys: Vec<String> = Vec::new();
    for c in &cards {
        if let Some(Value::String(k)) = c.get("card_key") {
            if !k.is_empty() {
                keys.push(k.clone());
            }
        }
    }
    let cheapest = get_cheapest_prices(conn, &keys).map_err(|_| DetailError::Db)?;

    let mut formatted: Vec<Map<String, Value>> = Vec::with_capacity(cards.len());
    for row in &cards {
        let mut parsed = parse_card_row(row).map_err(|_| DetailError::Db)?;
        parsed.insert(
            "current_price".into(),
            current_printing_price(row).map(json_num).unwrap_or(Value::Null),
        );
        parsed.insert(
            "cheapest_price".into(),
            match row.get("card_key").and_then(|v| v.as_str()).and_then(|k| cheapest.get(k)) {
                Some(p) => json_num(*p),
                None => Value::Null,
            },
        );
        formatted.push(parsed);
    }

    let deck_id_num = deck.get("id").and_then(|v| v.as_i64());
    let values = deck_id_num
        .map(|i| get_deck_minimum_values(conn, &[i]).map_err(|_| DetailError::Db))
        .transpose()?
        .unwrap_or_default();
    let current_values = deck_current_print_values(&formatted);

    let maybe_rows = rows_map(
        conn,
        &maybe_query(),
        &[
            SqlValue::Integer(user_id),
            SqlValue::Text(deck_id.to_string()),
            SqlValue::Text(deck_id.to_string()),
        ],
    )
    .map_err(|_| DetailError::Db)?;
    let maybeboard: Vec<Value> = maybe_rows
        .iter()
        .map(|row| {
            let mut parsed = parse_card_row(row).map_err(|_| DetailError::Db)?;
            parsed.insert(
                "current_price".into(),
                current_printing_price(row).map(json_num).unwrap_or(Value::Null),
            );
            Ok(Value::Object(parsed))
        })
        .collect::<Result<Vec<_>, DetailError>>()?;

    let mut body = deck;
    let mins = deck_id_num
        .and_then(|i| values.get(&i))
        .cloned()
        .unwrap_or_else(empty_deck_minimum_value);
    for k in ["minimum_value", "minimum_value_currency", "unpriced_cards", "unpriced_card_types"] {
        body.insert(k.into(), mins.get(k).cloned().unwrap_or(Value::Null));
    }
    for k in ["current_printing_value", "current_unpriced_cards", "current_estimated_cards"] {
        body.insert(k.into(), current_values.get(k).cloned().unwrap_or(Value::Null));
    }
    body.insert("cards".into(), Value::Array(formatted.into_iter().map(Value::Object).collect()));
    body.insert("maybeboard".into(), Value::Array(maybeboard));
    Ok(serde_json::to_vec(&Value::Object(body)).unwrap_or_else(|_| b"{}".to_vec()))
}

// ---------------------------------------------------------------------------
// GET /api/decks/:id/locations — getDeckCoverageRows
// ---------------------------------------------------------------------------

pub(crate) fn coverage_sql() -> String {
    format!(
        "
    WITH requested AS (
      SELECT
        {key_d} AS card_key,
        MIN(dc.card_id) AS representative_id,
        SUM(dc.quantity) AS required_qty
      FROM deck_cards dc
      JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
      WHERE dc.deck_id = ? AND dc.quantity > 0
      GROUP BY {key_d}
    ),
    owned AS (
      SELECT {key_o} AS card_key, SUM(owned.quantity) AS owned_qty
      FROM card_cache owned_cc
      CROSS JOIN collection owned
      WHERE owned.card_id = owned_cc.id AND owned.user_id = ? AND owned.quantity > 0
        AND {key_o} IN (SELECT card_key FROM requested)
      GROUP BY {key_o}
    ),
    locked AS (
      SELECT {key_l} AS card_key, SUM(locked_dc.quantity) AS locked_qty
      FROM deck_cards locked_dc
      JOIN card_cache locked_cc ON locked_cc.id = locked_dc.card_id
      JOIN decks locked_deck ON locked_deck.id = locked_dc.deck_id
      WHERE locked_deck.checked_out = 1
        AND locked_dc.quantity > 0
        AND locked_deck.user_id = ?
        AND locked_deck.id != ?
      GROUP BY {key_l}
    )
    SELECT
      requested.representative_id AS card_id,
      cc.name, cc.printed_name, cc.supertype, cc.subtypes,
      cc.set_name, cc.number, cc.image_url,
      requested.required_qty,
      COALESCE(owned.owned_qty, 0) AS owned_qty,
      COALESCE(locked.locked_qty, 0) AS locked_qty
    FROM requested
    JOIN card_cache cc ON cc.id = requested.representative_id
    LEFT JOIN owned ON owned.card_key = requested.card_key
    LEFT JOIN locked ON locked.card_key = requested.card_key
  ",
        key_d = sql_card_key("deck_cc"),
        key_o = sql_card_key("owned_cc"),
        key_l = sql_card_key("locked_cc"),
    )
}

fn build_deck_locations(conn: &Connection, deck_id: &str, user_id: i64) -> Result<Vec<u8>, DetailError> {
    let deck = one_row(
        conn,
        "SELECT id FROM decks WHERE id = ? AND user_id = ?",
        &[SqlValue::Text(deck_id.to_string()), SqlValue::Integer(user_id)],
    )
    .map_err(|_| DetailError::Db)?;
    if deck.is_none() {
        return Err(DetailError::NotFound);
    }
    if unresolved_count(conn, deck_id).map_err(|_| DetailError::Db)? > 0.0 {
        return Err(DetailError::Unresolved);
    }
    let cards = rows_map(
        conn,
        &coverage_sql(),
        &[
            SqlValue::Text(deck_id.to_string()),
            SqlValue::Integer(user_id),
            SqlValue::Integer(user_id),
            SqlValue::Text(deck_id.to_string()),
        ],
    )
    .map_err(|_| DetailError::Db)?;

    let mut results: Vec<(f64, Value /*name*/, Map<String, Value>)> = Vec::with_capacity(cards.len());
    for card in &cards {
        let required = js_number(card.get("required_qty"));
        let owned = js_number(card.get("owned_qty"));
        let locked = js_number(card.get("locked_qty"));
        let available = owned - locked;
        let missing = (required - available).max(0.0);
        let mut m = Map::new();
        m.insert("card_id".into(), card.get("card_id").cloned().unwrap_or(Value::Null));
        m.insert("name".into(), card.get("name").cloned().unwrap_or(Value::Null));
        m.insert(
            "card_name".into(),
            js_or(card.get("printed_name"), card.get("name").unwrap_or(&Value::Null)).clone(),
        );
        m.insert("set_name".into(), card.get("set_name").cloned().unwrap_or(Value::Null));
        m.insert("number".into(), card.get("number").cloned().unwrap_or(Value::Null));
        m.insert("image_url".into(), card.get("image_url").cloned().unwrap_or(Value::Null));
        m.insert("required".into(), json_num(required));
        m.insert("owned".into(), json_num(owned));
        m.insert("in_use".into(), json_num(locked));
        m.insert("available".into(), json_num(available.max(0.0)));
        m.insert("missing".into(), json_num(missing));
        m.insert("covered".into(), Value::Bool(missing == 0.0));
        // Basic lands are exempt exactly where checkout exempts them
        // (deckRules.isBasicLand over the raw driver row values).
        let probe = {
            let mut p = Map::new();
            p.insert("name".into(), card.get("name").cloned().unwrap_or(Value::Null));
            p.insert("supertype".into(), card.get("supertype").cloned().unwrap_or(Value::Null));
            p.insert("subtypes".into(), card.get("subtypes").cloned().unwrap_or(Value::Null));
            p
        };
        m.insert("is_basic_land".into(), Value::Bool(is_basic_land(&probe)));
        results.push((missing, card.get("name").cloned().unwrap_or(Value::Null), m));
    }

    // results.sort((a, b) => (b.missing - a.missing) || a.name.localeCompare(b.name));
    // Node throws (TypeError on a null name) when the comparator is invoked
    // with a null name; a 2+ row deck with a null name always hits that in
    // practice — mirror by erroring to the route's 500.
    if results.len() >= 2 && results.iter().any(|(_, n, _)| !matches!(n, Value::String(_))) {
        return Err(DetailError::Db);
    }
    let coll = root_collator();
    results.sort_by(|a, b| {
        match b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal) {
            std::cmp::Ordering::Equal => {
                let an = js_to_string(Some(&a.1));
                let bn = js_to_string(Some(&b.1));
                coll.compare(&an, &bn)
            }
            other => other,
        }
    });
    let out: Vec<Value> = results.into_iter().map(|(_, _, m)| Value::Object(m)).collect();
    Ok(serde_json::to_vec(&Value::Array(out)).unwrap_or_else(|_| b"[]".to_vec()))
}

// ---------------------------------------------------------------------------
// Unit tests (integration contract lives in tests/decks_reads.rs; live
// byte-parity in bench/parity.mjs dr_* cases)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn js_number_matches_node_number() {
        assert!(js_number(None).is_nan());
        assert_eq!(js_number(Some(&json!(null))), 0.0);
        assert_eq!(js_number(Some(&json!(true))), 1.0);
        assert_eq!(js_number(Some(&json!(42))), 42.0);
        assert_eq!(js_number(Some(&json!(" 007 "))), 7.0);
        assert_eq!(js_number(Some(&json!(""))), 0.0);
        assert_eq!(js_number(Some(&json!("1e3"))), 1000.0);
        assert_eq!(js_number(Some(&json!(".5"))), 0.5);
        assert_eq!(js_number(Some(&json!("0x10"))), 16.0);
        assert!(js_number(Some(&json!("abc"))).is_nan());
        assert!(js_number(Some(&json!("inf"))).is_nan()); // JS Number('inf') === NaN
        assert_eq!(js_number(Some(&json!("Infinity"))), f64::INFINITY);
        assert_eq!(js_number(Some(&json!([5]))), 5.0); // ToPrimitive join
        assert!(js_number(Some(&json!([1, 2]))).is_nan());
        assert!(js_number(Some(&json!({})) ).is_nan());
    }

    #[test]
    fn js_math_round_matches_node() {
        assert_eq!(js_math_round(2.5), 3.0);
        assert_eq!(js_math_round(-2.5), -2.0);
        assert_eq!(js_math_round(2.4999999), 2.0);
        assert_eq!(js_math_round(-0.5), -0.0);
    }

    #[test]
    fn round_cents_cents_math() {
        // 0.1+0.2 floor artifact must round to 0.3 like Node.
        assert_eq!(round_cents(0.1 + 0.2), json!(0.3));
        assert_eq!(round_cents(0.0), json!(0));
        assert_eq!(round_cents(12.0), json!(12));
    }

    #[test]
    fn cheapest_eligible_and_current_printing() {
        let row = json!({
            "price_normal": 0.5, "price_holofoil": 0.9, "price_etched": 0,
            "price_trend": 0.4, "price_currency": null
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(cheapest_eligible_price(&row), Some(0.4));
        assert_eq!(current_printing_price(&row), Some(0.4));
        let eur = json!({"price_normal": 0.5, "price_currency": "eur"}).as_object().unwrap().clone();
        assert_eq!(current_printing_price(&eur), None);
        let allnull = json!({"price_currency": "USD"}).as_object().unwrap().clone();
        assert_eq!(current_printing_price(&allnull), None);
    }

    #[test]
    fn deck_current_print_values_math() {
        let rows: Vec<Map<String, Value>> = [
            json!({"quantity": 2, "current_price": 0.3, "price_estimated": 0}),
            json!({"quantity": 1, "current_price": null, "price_estimated": 0}),
            json!({"quantity": 2, "current_price": 0.6, "price_estimated": 1}),
            json!({"quantity": 0, "current_price": 1.0, "price_estimated": 0}),
        ]
        .into_iter()
        .map(|v| v.as_object().unwrap().clone())
        .collect();
        let out = deck_current_print_values(&rows);
        assert_eq!(out.get("current_printing_value"), Some(&json!(1.8)));
        assert_eq!(out.get("current_unpriced_cards"), Some(&json!(1)));
        assert_eq!(out.get("current_estimated_cards"), Some(&json!(2)));
    }

    /// H2 (review/REVIEW-concurrency-jobs.md): getOrBuild must capture the
    /// revision BEFORE the build and only store if it is still current
    /// afterwards (Node deckListCache.js:19-24). Pre-fix, the store decision
    /// re-read the revision twice AFTER the build, so a build that raced a
    /// revision-bumping write cached its mixed body under the new revision.
    #[test]
    fn deck_list_store_requires_revision_unchanged_across_build() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("h2.db")).unwrap();
        conn.execute_batch(include_str!("../tests/schema_fixture.sql")).unwrap();
        conn.execute_batch(
            "INSERT INTO users (id, username, password_hash, role, share_token) VALUES (1,'u1','x','member','tok-h2');
             INSERT INTO decks (id, user_id, name, created_at) VALUES (101, 1, 'D', '2026-01-01 00:00:00');
             INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);",
        )
        .unwrap();
        let store: std::sync::Mutex<HashMap<i64, (i64, Vec<u8>)>> =
            std::sync::Mutex::new(HashMap::new());

        // 1) Quiet build: stored under the current revision; second call hits
        //    without running the builder.
        let quiet = |_: &Connection| Ok(b"body-A".to_vec());
        let (b1, hit1) = build_deck_list_cached_with(&conn, 1, &store, quiet).unwrap();
        assert!(!hit1 && b1 == b"body-A");
        let rev1 = current_revision(&conn).unwrap().unwrap();
        assert_eq!(store.guard().get(&1), Some(&(rev1, b"body-A".to_vec())));
        let (b2, hit2) =
            build_deck_list_cached_with(&conn, 1, &store, |_: &Connection| -> Result<Vec<u8>, rusqlite::Error> {
                unreachable!()
            })
            .unwrap();
        assert!(hit2 && b2 == b"body-A");

        // 2) The race: builder bumps deck_list_revision mid-build (simulates
        //    the concurrent deck write the body never fully observed). Node
        //    drops such a body; pre-fix Rust stored it under the NEW revision.
        let racy = |c: &Connection| -> Result<Vec<u8>, rusqlite::Error> {
            c.execute_batch(
                "INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (101, 'z-etali', 1);",
            )
            .unwrap(); // deck_list_rev_deck_cards_insert fires
            Ok(b"body-B-mixed".to_vec())
        };
        let (b3, hit3) = build_deck_list_cached_with(&conn, 2, &store, racy).unwrap();
        assert!(!hit3 && b3 == b"body-B-mixed"); // fresh body is still served once
        let after = current_revision(&conn).unwrap().unwrap();
        assert!(after > rev1, "builder must have bumped the revision");
        assert!(
            !store.guard().contains_key(&2),
            "H1/H2: body built while the revision moved must NOT be cached (would serve stale until the next bump)"
        );

        // 3) After the dropped build, a clean build stores under the new revision.
        let (b4, hit4) =
            build_deck_list_cached_with(&conn, 2, &store, |_: &Connection| Ok(b"body-C".to_vec())).unwrap();
        assert!(!hit4 && b4 == b"body-C");
        assert_eq!(store.guard().get(&2), Some(&(after, b"body-C".to_vec())));
    }

    #[test]
    fn key_order_is_first_position_stable() {
        // Mirrors the V8 property semantics rows_map relies on.
        let mut m = Map::new();
        m.insert("a".into(), json!(1));
        m.insert("b".into(), json!(2));
        m.insert("a".into(), json!(3));
        assert_eq!(m.keys().collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn collator_matches_pinned_node_icu_vectors() {
        // Vectors generated with the container's Node 22.23.4 (full-icu 78.3):
        // a.localeCompare(b) signs.
        let coll = root_collator();
        let sign = |a: &str, b: &str| coll.compare(a, b) as i8;
        // ICU expands Æ→AE at primary: 'Æther' sorts WITH 'Aether' names.
        assert_eq!(sign("Æther Snap", "Aether Vial"), -1);
        // Lowercase before uppercase at tertiary (opposite of code units).
        assert_eq!(sign("giant Slug", "Gibbering Descent"), -1);
        assert_eq!(sign("apple", "Apple"), -1);
        // Underscore names sort before quote-led names (CLDR primary order).
        assert_eq!(sign("_____", "\"Ach! Hans, Run!\""), -1);
        // Pinned from the container: Node says -1 (CLDR sorts '+' before '1').
        assert_eq!(sign("+2 Mace", "17-Year Cicadas"), -1);
        // Equal primary but case-distinguishing → not 0 (auto sensitivity).
        assert_ne!(sign("Ab", "ab"), 0);
    }

    #[test]
    fn js_or_matches_falsy_collapse() {
        let name = json!("Name");
        assert_eq!(js_or(Some(&json!(null)), &name), &name);
        assert_eq!(js_or(Some(&json!("")), &name), &name);
        assert_eq!(js_or(Some(&json!(0)), &name), &name);
        assert_eq!(js_or(Some(&json!("P")), &name), &json!("P"));
        assert_eq!(js_or(None, &name), &name);
    }
}

#[cfg(test)]
mod poison_m2 {
    //! REVIEW-errors-maint M2 (lane-A remainder): the deck-list body store
    //! locks through `crate::poison::LockExt`; one panic under the guard
    //! must not turn every later deck-list read into a permanent 500.
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn deck_list_store_survives_a_poisoning_panic() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("m2.db")).unwrap();
        conn.execute_batch(include_str!("../tests/schema_fixture.sql")).unwrap();
        conn.execute_batch("INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);")
            .unwrap();
        let store: std::sync::Mutex<HashMap<i64, (i64, Vec<u8>)>> =
            std::sync::Mutex::new(HashMap::new());
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            std::thread::scope(|s| {
                s.spawn(|| {
                    let _g = store.lock().unwrap();
                    panic!("simulated panic under the deck-list store guard");
                });
            });
        }));
        assert!(poisoned.is_err(), "precondition: the poisoning thread must panic");
        // Pre-fix (`.lock().unwrap()`) this panicked with "mutex is poisoned".
        let (body, hit) = build_deck_list_cached_with(&conn, 1, &store, |_: &Connection| {
            Ok(b"body-M2".to_vec())
        })
        .unwrap();
        assert!(!hit && body == b"body-M2", "cold build must survive poisoning");
        let rev = current_revision(&conn).unwrap().unwrap();
        assert_eq!(store.guard().get(&1), Some(&(rev, b"body-M2".to_vec())), "store write survives");
        let (_, hit2) = build_deck_list_cached_with(&conn, 1, &store, |_: &Connection| -> Result<Vec<u8>, rusqlite::Error> {
            unreachable!("second call must hit the poisoned-but-recovered store")
        })
        .unwrap();
        assert!(hit2, "cache hit must survive a poisoned store");
    }
}
