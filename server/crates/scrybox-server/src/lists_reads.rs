//! Native port of the card-list READ routes from `backend/src/routes/lists.js`:
//! `GET /` (list collection, routes/lists.js:33), `GET /card-matches` (:71),
//! `GET /:id` (detail, :153) and `GET /:id/cardlist` (text export, :233).
//! Mounted under the `lists_reads` name in RUST_ROUTES; every other
//! /api/lists route (POSTs, PUTs, DELETEs, /order, cheapest-printings,
//! printing, bulk, remove-from-collection) stays proxied to Node untouched —
//! the /api/lists/{id} dispatcher answers GET natively and forwards every
//! other method, so shapes are never 405'd here.
//!
//! Every status, body and message was written against the actual Node source
//! (routes/lists.js, utils/deckPricing.js getListMinimumValues/computeMinimumValues/
//! deckCurrentPrintValues, utils/priceHelpers.js parseCardRow,
//! shared/cardListText.js buildCardListText, utils/cardIdentity.js sqlCardKey)
//! plus live byte-pinning against the :3312 container (lr_* parity cases).
//!
//! Key JS semantics reproduced deliberately:
//!   * `res.json`'s Express-default weak ETag + If-None-Match/304 with the
//!     fresh@0.5.2 standalone-no-cache gate (shared twin in decks_reads).
//!   * GET /card-matches' answer object is keyed by numeric list_id — a JS
//!     object orders integer-like keys ASCENDING NUMERICALLY regardless of
//!     insertion order, so the Rust build sorts keys as i64 before serialise.
//!   * short/absent `q` (<2 chars after trim) answers `{}` without touching
//!     the DB; LIKE metacharacters in q are backslash-escaped (ESCAPE '\').
//!   * detail: no parseInt gate (a non-numeric id simply finds no row -> 404);
//!     the unresolved gate answers 422 with the LIST wording, not the deck's;
//!     totals come from getListMinimumValues over [Number(id)] (raw param,
//!     JS Number semantics, non-safe-integers filtered) plus
//!     deckCurrentPrintValues over the PER-ROW (unfolded) list_cards rows.
//!   * cardlist: `res.type('text/plain').send(text)` — text/plain ETag/304
//!     path; `?missing=1` gates the owned CTE (`WHERE ? = 1`) so quantities
//!     become requested-minus-owned and covered lines drop out; detailed
//!     style renders `qty Name (SET) num` from cc.set_id/cc.number.
//!   * buildCardListText: `parseInt(quantity,10)` NaN -> qty 1, qty<=0 line
//!     dropped (filter(Boolean)); name is norm(String(v).trim()) — a null
//!     name still emits "3 " with trailing space (truthy line, kept).

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::extract::Request;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::collection::{body_etag, etag_matches, gzip_bytes, json_num, wants_encoding};
use crate::collection_writes::{js_parse_int_v, js_to_string, json_error, tr};
use crate::decks_reads::{
    chunked, current_printing_price, deck_current_print_values, empty_deck_minimum_value,
    fresh_allows_304, get_cheapest_prices, guard_get, json_read_response, js_number, one_row,
    parse_card_row, round_cents, rows_map, sql_card_key,
};
use crate::AppState;

// ---------------------------------------------------------------------------
// Route entries
// ---------------------------------------------------------------------------

/// GET /api/lists (routes/lists.js:33). Anything else on the shape proxies.
pub async fn handle_collection(State(st): State<AppState>, req: Request) -> Response {
    guard_get(st, req, "lists_reads", |pool, headers, user| async move {
        let outcome = tokio::task::spawn_blocking(move || {
            let conn = pool.get().map_err(|e| {
                tracing::error!(error = %e, "lists_reads: pool");
            })?;
            build_list_collection(&conn, user.id).map_err(|e| {
                tracing::error!(error = %e, "lists_reads: collection build");
            })
        })
        .await;
        match outcome {
            Ok(Ok(raw)) => json_read_response(&headers, raw, Vec::<(String, String)>::new()),
            _ => json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve lists"),
        }
    })
    .await
}

/// GET /api/lists/card-matches (routes/lists.js:71).
pub async fn handle_card_matches(State(st): State<AppState>, req: Request) -> Response {
    let uri = req.uri().clone();
    guard_get(st, req, "lists_reads", move |pool, headers, user| async move {
        // const q = String(req.query.q || '').trim(); if (q.length < 2) return res.json({});
        let q = qs_string(uri.query(), "q").trim().to_string();
        // JS string length counts UTF-16 code units — one astral char
        // (surrogate pair) is length 2 and DOES reach the LIKE query.
        if q.encode_utf16().count() < 2 {
            return json_read_response(&headers, b"{}".to_vec(), Vec::<(String, String)>::new());
        }
        // const like = `%${q.replace(/[\\%_]/g, ch => `\\${ch}`)}%`
        let mut escaped = String::new();
        for ch in q.chars() {
            if ch == '\\' || ch == '%' || ch == '_' {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
        let like = format!("%{escaped}%");
        let outcome = tokio::task::spawn_blocking(move || {
            let conn = pool.get().map_err(|e| {
                tracing::error!(error = %e, "lists_reads: pool");
            })?;
            build_card_matches(&conn, user.id, &like).map_err(|e| {
                tracing::error!(error = %e, "lists_reads: card-matches build");
            })
        })
        .await;
        match outcome {
            Ok(Ok(raw)) => json_read_response(&headers, raw, Vec::<(String, String)>::new()),
            _ => json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to search lists"),
        }
    })
    .await
}

/// Dispatcher for /api/lists/{id}: only GET is owned this stage; PUT/DELETE
/// stay fully proxied until the lists WRITE group lands.
pub async fn route_entry(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if req.method() == Method::GET && st.owns("lists_reads") {
        return handle_detail(Path(id), State(st), req).await;
    }
    crate::proxy(State(st), req).await
}

/// GET /api/lists/:id (routes/lists.js:153).
pub async fn handle_detail(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    guard_get(st, req, "lists_reads", move |pool, headers, user| async move {
        let outcome = tokio::task::spawn_blocking(move || {
            let conn = pool.get().map_err(|e| {
                tracing::error!(error = %e, "lists_reads: pool");
                ListError::Db
            })?;
            build_list_detail(&conn, &id, user.id)
        })
        .await;
        match outcome {
            Ok(Ok(raw)) => json_read_response(&headers, raw, Vec::<(String, String)>::new()),
            Ok(Err(ListError::NotFound)) => json_error(StatusCode::NOT_FOUND, "List not found"),
            Ok(Err(ListError::Unresolved)) => json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "This list contains cards whose details are unavailable. Remove or re-import them before continuing.",
            ),
            Ok(Err(ListError::Db)) | Err(_) => {
                json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve list details")
            }
        }
    })
    .await
}

/// GET /api/lists/:id/cardlist (routes/lists.js:233).
pub async fn handle_cardlist(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    let uri = req.uri().clone();
    guard_get(st, req, "lists_reads", move |pool, headers, user| async move {
        // Node: style = req.query.style === 'detailed' ? 'detailed' : 'plain';
        //       missingOnly = req.query.missing === '1'.
        let style_detailed = qs_equals(uri.query(), "style", "detailed");
        let missing_only = qs_equals(uri.query(), "missing", "1");
        let outcome = tokio::task::spawn_blocking(move || {
            let conn = pool.get().map_err(|e| {
                tracing::error!(error = %e, "lists_reads: pool");
                ListError::Db
            })?;
            build_list_cardlist(&conn, &id, user.id, style_detailed, missing_only)
        })
        .await;
        match outcome {
            Ok(Ok(text)) => text_read_response(&headers, text.as_bytes()),
            Ok(Err(ListError::NotFound)) => json_error(StatusCode::NOT_FOUND, "List not found"),
            Ok(Err(ListError::Unresolved)) => json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "This list contains cards whose details are unavailable. Remove or re-import them before exporting.",
            ),
            Ok(Err(ListError::Db)) | Err(_) => {
                json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to build card list")
            }
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// Query-string helper — qs (express default 'extended') twin for the three
// keys this group reads. Live-pinned against the container's own qs:
//   repeated `key=` -> ARRAY, so `style === 'detailed'` / `missing === '1'`
//   are FALSE for arrays (a repeated key can never select a mode);
//   `q[]=v` pushes; scalar+`q[]=` mixes into one array (qs merges);
//   `q[k]=v` -> object -> String() = '[object Object]' (still searched);
//   array q stringifies through Array.join(',') before the trim/length gate;
//   ';' is NOT a delimiter; valueless `key` -> '' (falsy, not '1').
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum QsVal {
    Missing,
    Scalar(String),
    Array(Vec<String>),
    Obj,
}

fn qs_get(query: Option<&str>, key: &str) -> QsVal {
    let q = match query {
        Some(q) => q,
        None => return QsVal::Missing,
    };
    let mut result: Option<QsVal> = None;
    for (k, v) in form_urlencoded::parse(q.as_bytes()) {
        // Bracket forms: `key[]` pushes onto the array, any other
        // `key[...]` makes the value an object.
        let (base, push) = match k.split_once('[') {
            None => (k.as_ref(), None),
            Some((base, rest)) => {
                if rest != "]" {
                    if base == key {
                        result = Some(QsVal::Obj);
                    }
                    continue;
                }
                (base, Some(true))
            }
        };
        if base != key || result == Some(QsVal::Obj) {
            continue;
        }
        let push = push.unwrap_or(false);
        result = Some(match result.take() {
            None if push => QsVal::Array(vec![v.into_owned()]),
            None => QsVal::Scalar(v.into_owned()),
            Some(QsVal::Scalar(s)) => QsVal::Array(vec![s, v.into_owned()]),
            Some(QsVal::Array(mut a)) => {
                a.push(v.into_owned());
                QsVal::Array(a)
            }
            Some(QsVal::Obj) => QsVal::Obj,
            Some(QsVal::Missing) => QsVal::Missing,
        });
    }
    result.unwrap_or(QsVal::Missing)
}

/// `String(req.query[key] || '')` — arrays join with commas, objects stringify
/// to '[object Object]', absent/'' are empty.
fn qs_string(query: Option<&str>, key: &str) -> String {
    match qs_get(query, key) {
        QsVal::Scalar(s) => s,
        QsVal::Array(v) => v.join(","),
        QsVal::Obj => "[object Object]".to_string(),
        QsVal::Missing => String::new(),
    }
}

/// `req.query[key] === expected` — never true for arrays or objects.
fn qs_equals(query: Option<&str>, key: &str, expected: &str) -> bool {
    matches!(qs_get(query, key), QsVal::Scalar(v) if v == expected)
}

// ---------------------------------------------------------------------------
// Shared DB helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ListError {
    NotFound,
    Unresolved,
    Db,
}

/// unresolvedListCardCount (routes/lists.js:14) — `Number(row && row.count) || 0`.
fn unresolved_list_card_count(conn: &Connection, list_id: &str) -> Result<f64, ListError> {
    let row = one_row(
        conn,
        "
    SELECT COUNT(*) AS count
    FROM list_cards lc
    LEFT JOIN card_cache cc ON cc.id = lc.card_id
    WHERE lc.list_id = ? AND lc.quantity > 0 AND cc.id IS NULL
  ",
        &[SqlValue::Text(list_id.to_string())],
    )
    .map_err(|_| ListError::Db)?;
    let n = row
        .as_ref()
        .and_then(|r| r.get("count"))
        .and_then(js_number_or)
        .unwrap_or(0.0);
    Ok(n)
}

fn js_number_or(v: &Value) -> Option<f64> {
    let n = js_number(Some(v));
    if n.is_nan() {
        None
    } else {
        Some(n)
    }
}

/// getListMinimumValues + computeMinimumValues (list flavour) — same fold as
/// decks_reads::get_deck_minimum_values over the list_cards table. `ids` are
/// already the JS `map(Number).filter(Number.isSafeInteger)` normalisation
/// results.
fn get_list_minimum_values(
    conn: &Connection,
    list_ids: &[i64],
) -> Result<HashMap<i64, Map<String, Value>>, rusqlite::Error> {
    let mut normalized: Vec<i64> = Vec::new();
    for id in list_ids {
        if id.abs() <= 9_007_199_254_740_991 && !normalized.contains(id) {
            normalized.push(*id);
        }
    }
    let mut values: HashMap<i64, (f64, f64, i64)> =
        normalized.iter().map(|id| (*id, (0.0, 0.0, 0))).collect();
    if normalized.is_empty() {
        return Ok(HashMap::new());
    }
    // getListRequirements verbatim.
    let mut requirements: Vec<Map<String, Value>> = Vec::new();
    for ids in chunked(normalized.as_slice(), 500) {
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!(
            "
      SELECT
        lc.list_id,
        COALESCE(NULLIF({}, ''), 'missing:' || lc.card_id) AS card_key,
        SUM(lc.quantity) AS quantity
      FROM list_cards lc
      LEFT JOIN card_cache list_cc ON list_cc.id = lc.card_id
      WHERE lc.list_id IN ({placeholders})
        AND lc.quantity > 0
      GROUP BY lc.list_id, card_key
    ",
            sql_card_key("list_cc")
        );
        let params: Vec<SqlValue> = ids.iter().map(|i| SqlValue::Integer(*i)).collect();
        requirements.extend(rows_map(conn, &sql, &params)?);
    }

    // [...new Set(requirements.map(r => r.card_key).filter(k => k && !k.startsWith('missing:')))]
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
        let Some(owner) = r.get("list_id").and_then(|v| v.as_i64()) else { continue };
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

/// JS `Number(str)` then `[...new Set(map(Number).filter(Number.isSafeInteger))]`
/// for a single raw path param (detail's `[Number(id)]`).
fn number_param(id: &str) -> Option<i64> {
    let n = js_number(Some(&Value::String(id.to_string())));
    if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991f64 {
        Some(n as i64)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// GET /api/lists — list collection (routes/lists.js:33 verbatim SQL)
// ---------------------------------------------------------------------------

const LIST_SQL: &str = "
       SELECT
         l.id, l.name, l.description, l.accent_color, l.icon, l.created_at,
         COUNT(DISTINCT CASE
           WHEN lc.quantity > 0 THEN CASE
             WHEN list_cc.id IS NULL THEN 'missing:' || lc.card_id
             ELSE LOWER(TRIM(COALESCE(list_cc.name, '')))
           END
         END) AS total_card_types,
         COALESCE(SUM(CASE WHEN lc.quantity > 0 THEN lc.quantity ELSE 0 END), 0) AS total_cards,
         COUNT(DISTINCT CASE WHEN lc.quantity > 0 AND list_cc.id IS NULL THEN lc.card_id END) AS unresolved_card_types
       FROM card_lists l
       LEFT JOIN list_cards lc ON l.id = lc.list_id
       LEFT JOIN card_cache list_cc ON list_cc.id = lc.card_id
       WHERE l.user_id = ?
       GROUP BY l.id
       ORDER BY (l.sort_order IS NULL) DESC, l.sort_order ASC, l.created_at DESC
    ";

fn build_list_collection(conn: &Connection, user_id: i64) -> Result<Vec<u8>, rusqlite::Error> {
    let rows = rows_map(conn, LIST_SQL, &[SqlValue::Integer(user_id)])?;
    let ids: Vec<i64> = rows.iter().filter_map(|r| r.get("id").and_then(|v| v.as_i64())).collect();
    let values = get_list_minimum_values(conn, &ids)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut body = r.clone();
            let mins = r
                .get("id")
                .and_then(|v| v.as_i64())
                .and_then(|i| values.get(&i))
                .cloned()
                .unwrap_or_else(empty_deck_minimum_value);
            for k in ["minimum_value", "minimum_value_currency", "unpriced_cards", "unpriced_card_types"] {
                body.insert(k.into(), mins.get(k).cloned().unwrap_or(Value::Null));
            }
            Value::Object(body)
        })
        .collect();
    Ok(serde_json::to_vec(&Value::Array(out)).unwrap_or_else(|_| b"[]".to_vec()))
}

// ---------------------------------------------------------------------------
// GET /api/lists/card-matches (routes/lists.js:71)
// ---------------------------------------------------------------------------

fn build_card_matches(conn: &Connection, user_id: i64, like: &str) -> Result<Vec<u8>, rusqlite::Error> {
    let rows = rows_map(
        conn,
        "
         SELECT DISTINCT lc.list_id AS list_id, cc.name AS name
           FROM card_lists l
           JOIN list_cards lc ON lc.list_id = l.id AND lc.quantity > 0
           JOIN card_cache cc ON cc.id = lc.card_id
          WHERE l.user_id = ? AND cc.name LIKE ? ESCAPE '\\'
          ORDER BY cc.name",
        &[SqlValue::Integer(user_id), SqlValue::Text(like.to_string())],
    )?;
    // const out = {}; for (const r of rows) (out[r.list_id] ||= []).push(r.name);
    // JS object: integer-like keys enumerate ASCENDING NUMERICALLY.
    let mut buckets: Vec<(i64, Vec<String>)> = Vec::new();
    for r in &rows {
        let Some(list_id) = r.get("list_id").and_then(|v| v.as_i64()) else { continue };
        let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        match buckets.iter_mut().find(|(k, _)| *k == list_id) {
            Some((_, v)) => v.push(name),
            None => buckets.push((list_id, vec![name])),
        }
    }
    buckets.sort_by_key(|(k, _)| *k);
    let mut map = Map::new();
    for (k, names) in buckets {
        map.insert(k.to_string(), Value::Array(names.into_iter().map(Value::String).collect()));
    }
    Ok(serde_json::to_vec(&Value::Object(map)).unwrap_or_else(|_| b"{}".to_vec()))
}

// ---------------------------------------------------------------------------
// GET /api/lists/:id — detail (routes/lists.js:153)
// ---------------------------------------------------------------------------

fn build_list_detail(conn: &Connection, list_id: &str, user_id: i64) -> Result<Vec<u8>, ListError> {
    let list = one_row(
        conn,
        "SELECT * FROM card_lists WHERE id = ? AND user_id = ?",
        &[SqlValue::Text(list_id.to_string()), SqlValue::Integer(user_id)],
    )
    .map_err(|_| ListError::Db)?
    .ok_or(ListError::NotFound)?;

    if unresolved_list_card_count(conn, list_id)? > 0.0 {
        return Err(ListError::Unresolved);
    }

    // requested/owned CTE fold (verbatim).
    let cards_rows = rows_map(
        conn,
        &format!(
            "
       WITH requested AS (
         SELECT
           {key} AS card_key,
           MIN(lc.card_id) AS representative_id,
           SUM(lc.quantity) AS quantity
         FROM list_cards lc
         JOIN card_cache list_cc ON list_cc.id = lc.card_id
         WHERE lc.list_id = ? AND lc.quantity > 0
         GROUP BY {key}
       ),
       owned AS (
         SELECT
           {own_key} AS card_key,
           SUM(collection_row.quantity) AS quantity
         FROM card_cache owned_cc
         CROSS JOIN collection collection_row
         WHERE collection_row.card_id = owned_cc.id
           AND collection_row.user_id = ? AND collection_row.quantity > 0
           AND {own_key} IN (SELECT card_key FROM requested)
         GROUP BY {own_key}
       )
       SELECT
         requested.quantity,
         cc.id, cc.name, cc.printed_name,
         cc.supertype, cc.subtypes, cc.types,
         cc.rarity, cc.set_id, cc.set_name, cc.number,
         cc.image_url, cc.price_trend, cc.price_estimated,
         COALESCE(owned.quantity, 0) AS owned_qty
       FROM requested
       JOIN card_cache cc ON cc.id = requested.representative_id
       LEFT JOIN owned ON owned.card_key = requested.card_key
       ORDER BY cc.name ASC",
            key = sql_card_key("list_cc"),
            own_key = sql_card_key("owned_cc"),
        ),
        &[SqlValue::Text(list_id.to_string()), SqlValue::Integer(user_id)],
    )
    .map_err(|_| ListError::Db)?;

    // const values = await getListMinimumValues(db, [Number(id)]);
    let values = match number_param(list_id) {
        Some(n) => get_list_minimum_values(conn, &[n]).map_err(|_| ListError::Db)?,
        None => HashMap::new(),
    };

    // currentRows: PER list_cards row (unfolded), priced through
    // currentPrintingPrice, folded by deckCurrentPrintValues.
    let current_rows = rows_map(
        conn,
        "
      SELECT lc.quantity,
             cc.price_normal, cc.price_holofoil, cc.price_etched, cc.price_trend, cc.price_currency, cc.price_estimated
      FROM list_cards lc
      JOIN card_cache cc ON cc.id = lc.card_id
      WHERE lc.list_id = ? AND lc.quantity > 0
    ",
        &[SqlValue::Text(list_id.to_string())],
    )
    .map_err(|_| ListError::Db)?;
    let current_inputs: Vec<Map<String, Value>> = current_rows
        .iter()
        .map(|row| {
            let mut m = Map::new();
            m.insert("quantity".into(), row.get("quantity").cloned().unwrap_or(Value::Null));
            m.insert(
                "current_price".into(),
                current_printing_price(row).map(json_num).unwrap_or(Value::Null),
            );
            m.insert("price_estimated".into(), row.get("price_estimated").cloned().unwrap_or(Value::Null));
            m
        })
        .collect();
    let current_values = deck_current_print_values(&current_inputs);

    let mut body = list;
    let mins = number_param(list_id)
        .and_then(|i| values.get(&i))
        .cloned()
        .unwrap_or_else(empty_deck_minimum_value);
    for k in ["minimum_value", "minimum_value_currency", "unpriced_cards", "unpriced_card_types"] {
        body.insert(k.into(), mins.get(k).cloned().unwrap_or(Value::Null));
    }
    for k in ["current_printing_value", "current_unpriced_cards", "current_estimated_cards"] {
        body.insert(k.into(), current_values.get(k).cloned().unwrap_or(Value::Null));
    }
    let cards: Vec<Value> = cards_rows
        .iter()
        .map(|r| parse_card_row(r).map(Value::Object).map_err(|_| ListError::Db))
        .collect::<Result<_, _>>()?;
    body.insert("cards".into(), Value::Array(cards));
    Ok(serde_json::to_vec(&Value::Object(body)).unwrap_or_else(|_| b"{}".to_vec()))
}

// ---------------------------------------------------------------------------
// GET /api/lists/:id/cardlist — text export (routes/lists.js:233)
// ---------------------------------------------------------------------------

fn build_list_cardlist(
    conn: &Connection,
    list_id: &str,
    user_id: i64,
    style_detailed: bool,
    missing_only: bool,
) -> Result<String, ListError> {
    let list = one_row(
        conn,
        "SELECT id FROM card_lists WHERE id = ? AND user_id = ?",
        &[SqlValue::Text(list_id.to_string()), SqlValue::Integer(user_id)],
    )
    .map_err(|_| ListError::Db)?
    .ok_or(ListError::NotFound)?;
    let _ = list;

    if unresolved_list_card_count(conn, list_id)? > 0.0 {
        return Err(ListError::Unresolved);
    }

    let rows = rows_map(
        conn,
        &format!(
            "
       WITH requested AS (
         SELECT
           {key} AS card_key,
           MIN(lc.card_id) AS representative_id,
           SUM(lc.quantity) AS quantity
         FROM list_cards lc
         JOIN card_cache list_cc ON list_cc.id = lc.card_id
         WHERE lc.list_id = ? AND lc.quantity > 0
         GROUP BY {key}
       ),
       owned AS (
         SELECT
           {own_key} AS card_key,
           SUM(collection_row.quantity) AS quantity
         FROM card_cache owned_cc
         CROSS JOIN collection collection_row
         WHERE ? = 1
           AND collection_row.card_id = owned_cc.id
           AND collection_row.user_id = ? AND collection_row.quantity > 0
           AND {own_key} IN (SELECT card_key FROM requested)
         GROUP BY {own_key}
       )
       SELECT requested.quantity - COALESCE(owned.quantity, 0) AS quantity, cc.name, cc.set_id, cc.number
       FROM requested
       JOIN card_cache cc ON cc.id = requested.representative_id
       LEFT JOIN owned ON owned.card_key = requested.card_key
       WHERE requested.quantity - COALESCE(owned.quantity, 0) > 0
       ORDER BY cc.name ASC",
            key = sql_card_key("list_cc"),
            own_key = sql_card_key("owned_cc"),
        ),
        &[
            SqlValue::Text(list_id.to_string()),
            SqlValue::Integer(if missing_only { 1 } else { 0 }),
            SqlValue::Integer(user_id),
        ],
    )
    .map_err(|_| ListError::Db)?;

    // shared/cardListText.js buildCardListText(rows, style).
    let mut lines: Vec<String> = Vec::with_capacity(rows.len());
    for row in &rows {
        let qty = js_parse_int_v(row.get("quantity")).unwrap_or(1);
        if qty <= 0 {
            continue;
        }
        let name = norm(row.get("name"));
        let line = if style_detailed {
            // norm(card.set_id || card.set_code).toUpperCase() — the rows only
            // carry set_id (no set_code column exists on this select).
            let set = match row.get("set_id") {
                Some(v) if tr(Some(v)) => norm(Some(v)).to_uppercase(),
                _ => String::new(),
            };
            let num = norm(row.get("number"));
            let mut l = name;
            if !set.is_empty() {
                l.push_str(&format!(" ({set})"));
            }
            if !num.is_empty() {
                l.push_str(&format!(" {num}"));
            }
            format!("{qty} {l}")
        } else {
            format!("{qty} {name}")
        };
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

/// shared/cardListText.js `norm`: String(v == null ? '' : v).trim().
fn norm(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_to_string(Some(x)).trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Text response twin (res.type('text/plain').send(string)): text/plain
// charset=utf-8, express ETag, conditional-GET 304, compression.
// ---------------------------------------------------------------------------

fn text_read_response(headers: &HeaderMap, bytes: &[u8]) -> Response {
    let tag = body_etag(bytes);
    if fresh_allows_304(headers) {
        if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
            if etag_matches(Some(inm), &tag) {
                return (
                    StatusCode::NOT_MODIFIED,
                    [(header::ETAG, HeaderValue::from_str(&tag).unwrap())],
                )
                    .into_response();
            }
        }
    }
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let use_gzip = wants_encoding(ae, "gzip", bytes.len());
    let body = if use_gzip { Body::from(gzip_bytes(bytes)) } else { Body::from(bytes.to_vec()) };
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::ETAG, tag);
    if use_gzip {
        resp = resp
            .header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"))
            .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    resp.body(body).unwrap()
}

// ---------------------------------------------------------------------------
// Unit tests (route contract in tests/lists_reads.rs; live byte-parity in
// bench/parity.mjs lr_* cases)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn qs_twin_matches_live_qs_vectors() {
        // Scalars.
        assert!(qs_equals(Some("style=detailed"), "style", "detailed"));
        assert_eq!(qs_string(Some("q=Sol%20Ring"), "q"), "Sol Ring");
        assert_eq!(qs_string(Some("q=%25a%20b%2B"), "q"), "%a b+");
        assert_eq!(qs_get(Some("a=1"), "style"), QsVal::Missing);
        assert_eq!(qs_get(None, "style"), QsVal::Missing);
        // Valueless key -> '' (falsy, not '1').
        assert!(!qs_equals(Some("missing"), "missing", "1"));
        assert_eq!(qs_string(Some("missing=&q=ab"), "q"), "ab");
        // Repeated key -> ARRAY: === 'detailed'/'1' false, String() joins.
        assert!(!qs_equals(Some("style=a&style=b"), "style", "detailed"));
        assert!(!qs_equals(Some("style=detailed&style=x"), "style", "detailed"));
        assert!(!qs_equals(Some("missing=1&missing=0"), "missing", "1"));
        assert_eq!(qs_string(Some("q=a&q=b"), "q"), "a,b");
        // Bracket forms.
        assert_eq!(qs_string(Some("q[]=Sol%20Ring"), "q"), "Sol Ring");
        assert_eq!(qs_string(Some("q=a&q[]=b"), "q"), "a,b");
        assert_eq!(qs_string(Some("q[a]=b"), "q"), "[object Object]");
        // ';' is NOT a delimiter (whole remainder is the value).
        assert_eq!(qs_string(Some("q=a;b=c"), "q"), "a;b=c");
    }

    #[test]
    fn like_escape_backslashes_metachars() {
        let q = "100%_x\\y";
        let mut escaped = String::new();
        for ch in q.chars() {
            if ch == '\\' || ch == '%' || ch == '_' {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
        assert_eq!(format!("%{escaped}%"), "%100\\%\\_x\\\\y%");
    }

    #[test]
    fn number_param_matches_js_number_is_safe_integer() {
        assert_eq!(number_param("12"), Some(12));
        assert_eq!(number_param("0012"), Some(12));
        assert_eq!(number_param("12.0"), Some(12));
        assert_eq!(number_param(" 12 "), Some(12));
        assert_eq!(number_param("1e2"), Some(100));
        assert_eq!(number_param("abc"), None);
        assert_eq!(number_param("12.5"), None);
        assert_eq!(number_param(""), Some(0), "JS Number('') === 0 (safe integer)");
    }

    #[test]
    fn card_list_line_plain_and_detailed() {
        // buildCardListText over {quantity, name, set_id, number} rows.
        let rows: Vec<Map<String, Value>> = [
            json!({"quantity": 4, "name": " Lightning Bolt ", "set_id": "jud", "number": "124"}),
            json!({"quantity": 0, "name": "Zero", "set_id": null, "number": null}),
            json!({"quantity": 1, "name": null, "set_id": "", "number": ""}),
            json!({"quantity": "2x", "name": "Weird", "set_id": null, "number": null}),
        ]
        .iter()
        .map(|v| v.as_object().unwrap().clone())
        .collect();
        let mut plain: Vec<String> = Vec::new();
        let mut detailed: Vec<String> = Vec::new();
        for row in &rows {
            let qty = js_parse_int_v(row.get("quantity")).unwrap_or(1);
            if qty <= 0 {
                continue;
            }
            let name = norm(row.get("name"));
            plain.push(format!("{qty} {name}"));
            let set = match row.get("set_id") {
                Some(v) if tr(Some(v)) => norm(Some(v)).to_uppercase(),
                _ => String::new(),
            };
            let num = norm(row.get("number"));
            let mut l = name;
            if !set.is_empty() {
                l.push_str(&format!(" ({set})"));
            }
            if !num.is_empty() {
                l.push_str(&format!(" {num}"));
            }
            detailed.push(format!("{qty} {l}"));
        }
        // parseInt('2x')=2; null name -> "1 " trailing space (truthy line kept).
        assert_eq!(plain.join("\n"), "4 Lightning Bolt\n1 \n2 Weird");
        assert_eq!(detailed.join("\n"), "4 Lightning Bolt (JUD) 124\n1 \n2 Weird");
    }
}
