//! LANE A — card-list CARD-BODY writes (iteration 33).
//!
//! The remaining seven `/api/lists/:id/...` write routes from
//! `backend/src/routes/lists.js` — add card, bulk add, bulk remove, remove
//! from collection, move to cheapest printings, change printing, remove card.
//! Together with iter-31's reads and iter-32's meta writes this completes the
//! lists group under the single `lists_writes` kill switch.
//!
//! Scryfall hydration policy (same as the collection/decks/list-create
//! twins): `cardApi.getCardById` is lane B's territory, so the moment a
//! native path would need it the *whole untouched request* is forwarded to
//! Node — before any local statement has written, never a half-applied
//! replay. A non-string `card_id` that reaches the hydration leg is a Node
//! TypeError: the single-add route dies with the generic 500 (Node's route
//! catch), the bulk route's per-entry catch swallows it into `failed`.
//!
//! `withAllocationLock` + `db.withTransaction` (BEGIN IMMEDIATE) for
//! remove-from-collection reuse the collection_writes twins: one in-process
//! FIFO mutex plus a real write transaction against the shared WAL file.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::extract::Request;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::auth::{authenticate, bearer_token};
use crate::collection_writes::{
    alloc_lock, get_row_map, internal_error, js_falsy, js_parse_int_v, js_to_string, json_error,
    json_response, json_to_sql_value, logical_inventory_status, node_exact, parse_body, read_parts,
    rebuild, too_large, BodyOutcome, Tx, WriteError,
};
use crate::db::DbPool;
use crate::decks_reads::{get_cheapest_printings, rows_map, sql_card_key};
use crate::lists_writes::{exact_int, js_bind};
use crate::AppState;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// JS numeric accumulator output: `res.json` prints integral doubles
/// without a decimal point (JSON.stringify(3) === "3"), keeps 3.5 as-is.
fn num_out(v: f64) -> Value {
    match exact_int(v) {
        Some(i) => Value::Number(i.into()),
        None => serde_json::Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null),
    }
}

/// JS `Math.max(1, parseInt(x, 10) || 1)` — NaN/0/negatives all clamp to 1.
fn qty_or_one(raw: Option<&Value>) -> i64 {
    match js_parse_int_v(raw) {
        None => 1,
        Some(i) if i < 1 => 1,
        Some(i) => i,
    }
}

/// `Number(x)` on DB-returned quantities (INTEGER/REAL from the driver;
/// NULL reaches only the `qty > row.quantity` comparison, where JS
/// `qty > null` is true and `Math.min(qty, null)` is 0).
fn js_num_opt(v: Option<&Value>) -> Option<f64> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        Some(_) => Some(f64::NAN),
    }
}

fn internal_fail(message: &'static str, ctx: &str, e: &dyn std::fmt::Debug) -> Response {
    tracing::error!("{ctx}: {e:?}");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, message)
}

fn list_exists(conn: &Connection, id: &str, user_id: i64) -> Result<bool, WriteError> {
    let row = get_row_map(
        conn,
        "SELECT id FROM card_lists WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    )?;
    Ok(row.is_some())
}

// ---------------------------------------------------------------------------
// POST /api/lists/:id/cards — routes/lists.js:491
// ---------------------------------------------------------------------------

enum AddOne {
    Response(Response),
    /// card_cache miss for a string id: Node hydrates via Scryfall.
    Forward,
}

pub async fn handle_add_card(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers: &HeaderMap = &parts.headers;
    let body = match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // statsCache.invalidateOnWrite twin: Node bumps the user's stats gen
    // after auth, before the handler body (REVIEW-writes L1).
    crate::stats::on_native_write(&st, user.id);
    match add_one(&pool, &id, user.id, &body) {
        AddOne::Response(res) => res,
        AddOne::Forward => crate::proxy(State(st), rebuild(parts, &bytes)).await,
    }
}

/// Single-add fold (routes/lists.js:520): SET semantics — collapse every
/// printing-equivalent row onto the effective id, zero the others, purge the
/// zeros. Two autocommit statements like the Node driver, never a tx.
fn set_fold(
    conn: &Connection,
    id: &str,
    card_probe: &SqlValue,
    card_raw: &Value,
    qty: i64,
) -> Result<(), WriteError> {
    let ek = sql_card_key("existing_cc");
    let tk = sql_card_key("target_cc");
    let equivalent = get_row_map(
        conn,
        &format!(
            "SELECT lc.card_id
             FROM list_cards lc
             JOIN card_cache existing_cc ON existing_cc.id = lc.card_id
             JOIN card_cache target_cc ON target_cc.id = ?1
             WHERE lc.list_id = ?2
               AND {ek} = {tk}
             ORDER BY (lc.card_id = ?3) DESC, lc.card_id
             LIMIT 1"
        ),
        &[card_probe.clone(), SqlValue::Text(id.to_string()), card_probe.clone()],
    )?;
    let effective = equivalent
        .as_ref()
        .and_then(|r| r.get("card_id").cloned())
        .unwrap_or_else(|| card_raw.clone());
    let eff_sql = json_to_sql_value(&effective);
    let qty_sql = SqlValue::Integer(qty);
    if equivalent.is_some() {
        conn.execute(
            &format!(
                "UPDATE list_cards
                 SET quantity = CASE WHEN card_id = ?1 THEN ?2 ELSE 0 END
                 WHERE list_id = ?3
                   AND card_id IN (
                     SELECT existing_cc.id
                     FROM card_cache existing_cc
                     JOIN card_cache target_cc ON target_cc.id = ?4
                     WHERE {ek} = {tk}
                   )"
            ),
            rusqlite::params![eff_sql.clone(), qty_sql, SqlValue::Text(id.to_string()), eff_sql.clone()],
        )?;
        conn.execute(
            "DELETE FROM list_cards WHERE list_id = ?1 AND quantity <= 0",
            rusqlite::params![SqlValue::Text(id.to_string())],
        )?;
    } else {
        conn.execute(
            "INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?1, ?2, ?3)",
            rusqlite::params![SqlValue::Text(id.to_string()), eff_sql, qty_sql],
        )?;
    }
    Ok(())
}

/// Bulk-add fold (routes/lists.js:574): INCREMENT the equivalent row (the
/// scanner re-send case), insert a fresh row when the card is new to the
/// list. Returns after one statement either way.
fn inc_fold(
    conn: &Connection,
    id: &str,
    card_probe: &SqlValue,
    qty: i64,
) -> Result<(), WriteError> {
    let ek = sql_card_key("existing_cc");
    let tk = sql_card_key("target_cc");
    let equivalent = get_row_map(
        conn,
        &format!(
            "SELECT lc.card_id FROM list_cards lc
             JOIN card_cache existing_cc ON existing_cc.id = lc.card_id
             JOIN card_cache target_cc ON target_cc.id = ?1
             WHERE lc.list_id = ?2 AND {ek} = {tk}
             ORDER BY (lc.card_id = ?3) DESC, lc.card_id LIMIT 1"
        ),
        &[card_probe.clone(), SqlValue::Text(id.to_string()), card_probe.clone()],
    )?;
    match equivalent.as_ref().and_then(|r| r.get("card_id").cloned()) {
        Some(equivalent_id) => {
            conn.execute(
                "UPDATE list_cards SET quantity = quantity + ?1 WHERE list_id = ?2 AND card_id = ?3",
                rusqlite::params![SqlValue::Integer(qty), SqlValue::Text(id.to_string()), json_to_sql_value(&equivalent_id)],
            )?;
        }
        None => {
            conn.execute(
                "INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?1, ?2, ?3)",
                rusqlite::params![SqlValue::Text(id.to_string()), card_probe.clone(), SqlValue::Integer(qty)],
            )?;
        }
    }
    Ok(())
}

fn add_one(pool: &DbPool, id: &str, user_id: i64, body: &Value) -> AddOne {
    const FAILED: &str = "Failed to add card to list";
    let fail = |e: &dyn std::fmt::Debug| internal_fail(FAILED, "lists add-card", e);
    let card_id_v = body.get("card_id");
    // `if (!card_id) return 400` sits before the try.
    if js_falsy(card_id_v) {
        return AddOne::Response(json_error(StatusCode::BAD_REQUEST, "card_id is required"));
    }
    // `const { quantity = 1 } = req.body` — destructuring default.
    let quantity = body.get("quantity").cloned().unwrap_or(Value::from(1));
    let qty = qty_or_one(Some(&quantity));
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return AddOne::Response(fail(&e)),
    };
    let exists = match list_exists(&conn, id, user_id) {
        Ok(b) => b,
        Err(e) => return AddOne::Response(fail(&e)),
    };
    if !exists {
        return AddOne::Response(json_error(StatusCode::NOT_FOUND, "List not found or unauthorized"));
    }
    let probe = js_bind(card_id_v.unwrap());
    let cached = match get_row_map(&conn, "SELECT id FROM card_cache WHERE id = ?1", std::slice::from_ref(&probe)) {
        Ok(c) => c,
        Err(e) => return AddOne::Response(fail(&e)),
    };
    if cached.is_none() {
        if !matches!(card_id_v, Some(Value::String(_))) {
            // Node reaches cardApi.getCardById, whose `cardId.startsWith`
            // throws TypeError on a non-string -> route catch -> generic 500.
            return AddOne::Response(fail(&"card_id hydration TypeError (non-string)"));
        }
        return AddOne::Forward;
    }
    match set_fold(&conn, id, &probe, card_id_v.unwrap(), qty) {
        Ok(()) => AddOne::Response(json_response(StatusCode::OK, &json!({ "message": "Card added to list" }))),
        Err(e) => AddOne::Response(fail(&e)),
    }
}

// ---------------------------------------------------------------------------
// POST /api/lists/:id/cards/bulk — routes/lists.js:556
// ---------------------------------------------------------------------------

/// Per-entry pre-pass verdict: hydrate-check runs for EVERY entry before any
/// local statement writes, so a Forward replays a pristine request on Node.
enum Pre {
    Skip,
    Cached,
    Hydrate,
    /// Non-string card_id on a cache miss: Node's inner catch turns the
    /// getCardById TypeError into failed.push(cardId).
    Failed,
    /// Probe errored (driver bind explosion etc.) — inner-catch failure.
    Error,
}

pub async fn handle_bulk_add(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers: &HeaderMap = &parts.headers;
    let body = match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // statsCache.invalidateOnWrite twin: Node bumps the user's stats gen
    // after auth, before the handler body (REVIEW-writes L1).
    crate::stats::on_native_write(&st, user.id);
    // `Array.isArray(req.body?.cards) ? cards : []` then the 1-250 gate —
    // both outside the try.
    let Some(entries) = bulk_entries(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "cards must be 1-250 entries");
    };
    match bulk_add(&pool, &id, user.id, &entries) {
        Ok(AddOne::Response(res)) => res,
        Ok(AddOne::Forward) => crate::proxy(State(st), rebuild(parts, &bytes)).await,
        Err(e) => internal_fail("Failed to add cards to list", "lists bulk-add", &e),
    }
}

fn bulk_entries(body: &Value) -> Option<Vec<Value>> {
    let arr = body.get("cards").and_then(|c| c.as_array())?;
    if arr.is_empty() || arr.len() > 250 {
        return None;
    }
    Some(arr.clone())
}

fn bulk_add(pool: &DbPool, id: &str, user_id: i64, entries: &[Value]) -> Result<AddOne, WriteError> {
    let conn = pool.get()?;
    if !list_exists(&conn, id, user_id)? {
        return Ok(AddOne::Response(json_error(
            StatusCode::NOT_FOUND,
            "List not found or unauthorized",
        )));
    }
    // Pre-pass: which entries would touch getCardById?
    let mut pre: Vec<Pre> = Vec::with_capacity(entries.len());
    for entry in entries {
        let card_id = entry.get("card_id");
        if js_falsy(card_id) {
            pre.push(Pre::Skip);
            continue;
        }
        match get_row_map(&conn, "SELECT id FROM card_cache WHERE id = ?1", &[js_bind(card_id.unwrap())]) {
            Ok(Some(_)) => pre.push(Pre::Cached),
            Ok(None) => {
                pre.push(if matches!(card_id, Some(Value::String(_))) {
                    Pre::Hydrate
                } else {
                    Pre::Failed
                })
            }
            Err(_) => pre.push(Pre::Error),
        }
    }
    if pre.iter().any(|p| matches!(p, Pre::Hydrate)) {
        return Ok(AddOne::Forward);
    }
    let mut added: i64 = 0;
    let mut failed: Vec<Value> = Vec::new();
    for (entry, p) in entries.iter().zip(pre) {
        match p {
            Pre::Skip => continue,
            Pre::Failed | Pre::Error => {
                failed.push(entry.get("card_id").cloned().unwrap_or(Value::Null));
                continue;
            }
            Pre::Cached => {}
            Pre::Hydrate => unreachable!("forwarded above"),
        }
        let card_id = entry.get("card_id").unwrap();
        let qty = qty_or_one(entry.get("quantity"));
        let probe = js_bind(card_id);
        match inc_fold(&conn, id, &probe, qty) {
            Ok(()) => added += qty,
            Err(e) => {
                tracing::warn!("lists bulk-add entry: {e:?}");
                failed.push(card_id.clone());
            }
        }
    }
    if !failed.is_empty() && added == 0 {
        return Ok(AddOne::Response(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to add cards to list",
        )));
    }
    Ok(AddOne::Response(json_response(
        StatusCode::OK,
        &json!({ "added": added, "failed": failed }),
    )))
}

// ---------------------------------------------------------------------------
// POST /api/lists/:id/cards/bulk-remove — routes/lists.js:598
// ---------------------------------------------------------------------------

pub async fn handle_bulk_remove(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers: &HeaderMap = &parts.headers;
    let body = match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // statsCache.invalidateOnWrite twin: Node bumps the user's stats gen
    // after auth, before the handler body (REVIEW-writes L1).
    crate::stats::on_native_write(&st, user.id);
    let Some(entries) = bulk_entries(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "cards must be 1-250 entries");
    };
    match bulk_remove(&pool, &id, user.id, &entries) {
        Ok(res) => res,
        Err(e) => internal_fail("Failed to remove cards from list", "lists bulk-remove", &e),
    }
}

fn bulk_remove(pool: &DbPool, id: &str, user_id: i64, entries: &[Value]) -> Result<Response, WriteError> {
    let conn = pool.get()?;
    if !list_exists(&conn, id, user_id)? {
        return Ok(json_error(StatusCode::NOT_FOUND, "List not found or unauthorized"));
    }
    // No per-entry try/catch in Node's twin: one bad entry aborts the whole
    // route with the generic 500. `removed` is the only response field.
    let ek = sql_card_key("existing_cc");
    let tk = sql_card_key("target_cc");
    let sql = format!(
        "SELECT lc.card_id, lc.quantity FROM list_cards lc
         JOIN card_cache existing_cc ON existing_cc.id = lc.card_id
         JOIN card_cache target_cc ON target_cc.id = ?1
         WHERE lc.list_id = ?2 AND {ek} = {tk}
         ORDER BY (lc.card_id = ?3) DESC, lc.card_id LIMIT 1"
    );
    let mut removed: f64 = 0.0;
    for entry in entries {
        let card_id = entry.get("card_id");
        if js_falsy(card_id) {
            continue;
        }
        let card_id = card_id.unwrap();
        let probe = js_bind(card_id);
        // Cards whose printing is unknown to the cache never match the
        // target_cc join — Node skips them silently, as we do.
        let Some(row) = get_row_map(&conn, &sql, &[probe.clone(), SqlValue::Text(id.to_string()), probe])? else {
            continue;
        };
        let qty = qty_or_one(entry.get("quantity"));
        let have = js_num_opt(row.get("quantity"));
        let row_id = json_to_sql_value(row.get("card_id").unwrap());
        let id_sql = SqlValue::Text(id.to_string());
        match have {
            Some(h) if h > qty as f64 => {
                conn.execute(
                    "UPDATE list_cards SET quantity = quantity - ?1 WHERE list_id = ?2 AND card_id = ?3",
                    rusqlite::params![SqlValue::Integer(qty), id_sql, row_id],
                )?;
            }
            _ => {
                conn.execute(
                    "DELETE FROM list_cards WHERE list_id = ?1 AND card_id = ?2",
                    rusqlite::params![id_sql, row_id],
                )?;
            }
        }
        removed += match have {
            Some(h) => (qty as f64).min(h),
            None => 0.0,
        };
    }
    Ok(json_response(StatusCode::OK, &json!({ "removed": num_out(removed) })))
}

// ---------------------------------------------------------------------------
// POST /api/lists/:id/remove-from-collection — routes/lists.js:430
// ---------------------------------------------------------------------------

pub async fn handle_remove_from_collection(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers: &HeaderMap = &parts.headers;
    let body = match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // statsCache.invalidateOnWrite twin: Node bumps the user's stats gen
    // after auth, before the handler body (REVIEW-writes L1).
    crate::stats::on_native_write(&st, user.id);
    // `!!(req.body && req.body.dry_run)` — any truthy value; non-object bodies false.
    let dry_run = body.get("dry_run").is_some_and(|v| !js_falsy(Some(v)));
    // Node wraps the whole flow in withAllocationLock; the transaction only
    // covers the non-dry pass (db.withTransaction, BEGIN IMMEDIATE).
    let _guard = alloc_lock().lock().await;
    match remove_from_collection(&pool, &id, user.id, dry_run) {
        Ok(res) => res,
        Err(e) => internal_fail(
            "Failed to remove list cards from collection",
            "lists remove-from-collection",
            &e,
        ),
    }
}

fn remove_from_collection(pool: &DbPool, id: &str, user_id: i64, dry_run: bool) -> Result<Response, WriteError> {
    let conn = pool.get()?;
    if !list_exists(&conn, id, user_id)? {
        return Ok(json_error(StatusCode::NOT_FOUND, "List not found or unauthorized"));
    }
    let work = || -> Result<Value, WriteError> {
        let rows = rows_map(
            &conn,
            "SELECT lc.card_id, lc.quantity, cc.name
             FROM list_cards lc
             JOIN card_cache cc ON cc.id = lc.card_id
             WHERE lc.list_id = ?1 AND lc.quantity > 0",
            &[SqlValue::Text(id.to_string())],
        )?;
        let mut budget: HashMap<String, f64> = HashMap::new();
        let mut requested: f64 = 0.0;
        let mut removed: f64 = 0.0;
        let mut short: Vec<Value> = Vec::new();
        for row in &rows {
            let quantity = js_num_opt(row.get("quantity")).unwrap_or(0.0);
            requested += quantity;
            let card_id_v = row.get("card_id").cloned().unwrap_or(Value::Null);
            let status = logical_inventory_status(&conn, user_id, &card_id_v)?;
            let key = match &status {
                Some(s) => js_to_string(s.get("card_key")),
                None => js_to_string(row.get("name")),
            };
            if !budget.contains_key(&key) {
                let initial = match &status {
                    Some(s) => {
                        let owned = js_num_opt(s.get("owned_qty")).unwrap_or(0.0);
                        let locked = js_num_opt(s.get("locked_qty")).unwrap_or(0.0);
                        (owned - locked).max(0.0)
                    }
                    None => 0.0,
                };
                budget.insert(key.clone(), initial);
            }
            let available = budget.get(&key).copied().unwrap_or(0.0);
            let mut want = quantity.min(available);
            let got = want;
            if want > 0.0 {
                // Node's owned query joins the target row to resolve the
                // logical key in SQL; exact printing first, then newest rows.
                let owned_sql = format!(
                    "SELECT c.id, c.quantity FROM collection c
                     JOIN card_cache owned_cc ON owned_cc.id = c.card_id
                     JOIN card_cache target ON target.id = ?1
                     WHERE c.user_id = ?2 AND c.quantity > 0
                       AND {own_key} = {tgt_key}
                     ORDER BY (c.card_id = ?3) DESC, c.id DESC",
                    own_key = sql_card_key("owned_cc"),
                    tgt_key = sql_card_key("target")
                );
                let owned_rows = rows_map(
                    &conn,
                    &owned_sql,
                    &[json_to_sql_value(&card_id_v), SqlValue::Integer(user_id), json_to_sql_value(&card_id_v)],
                )?;
                for entry in owned_rows {
                    if want <= 0.0 {
                        break;
                    }
                    let eqty = js_num_opt(entry.get("quantity")).unwrap_or(0.0);
                    let take = want.min(eqty);
                    if !dry_run {
                        let entry_id = json_to_sql_value(entry.get("id").unwrap_or(&Value::Null));
                        let user_sql = SqlValue::Integer(user_id);
                        if take >= eqty {
                            conn.execute(
                                "DELETE FROM collection WHERE id = ?1 AND user_id = ?2",
                                rusqlite::params![entry_id, user_sql],
                            )?;
                        } else {
                            let take_sql = match exact_int(take) {
                                Some(i) => SqlValue::Integer(i),
                                None => SqlValue::Real(take),
                            };
                            conn.execute(
                                "UPDATE collection SET quantity = quantity - ?1 WHERE id = ?2 AND user_id = ?3",
                                rusqlite::params![take_sql, entry_id, user_sql],
                            )?;
                        }
                    }
                    want -= take;
                }
            }
            let taken = got - want;
            let entry = budget.entry(key).or_insert(0.0);
            *entry -= taken;
            removed += taken;
            if taken < quantity {
                short.push(json!({
                    "card_id": card_id_v,
                    "name": row.get("name").cloned().unwrap_or(Value::Null),
                    "wanted": row.get("quantity").cloned().unwrap_or(Value::Null),
                    "removed": num_out(taken),
                }));
            }
        }
        Ok(json!({
            "dry_run": dry_run,
            "requested": num_out(requested),
            "removed": num_out(removed),
            "short": short,
        }))
    };
    let body_json = if dry_run {
        // Node calls work() directly — per-statement autocommit, no tx.
        work()?
    } else {
        let mut tx = Tx::begin(&conn)?;
        let out = work()?;
        tx.commit()?;
        out
    };
    Ok(json_response(StatusCode::OK, &body_json))
}

// ---------------------------------------------------------------------------
// PUT /api/lists/:id/cheapest-printings — routes/lists.js:308
// ---------------------------------------------------------------------------

pub async fn handle_cheapest_printings(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::PUT {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers: &HeaderMap = &parts.headers;
    match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => {}
    }
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // statsCache.invalidateOnWrite twin: Node bumps the user's stats gen
    // after auth, before the handler body (REVIEW-writes L1).
    crate::stats::on_native_write(&st, user.id);
    match cheapest_printings(&pool, &id, user.id) {
        Ok(res) => res,
        Err(e) => internal_fail("Failed to move cards to cheapest printings", "lists cheapest-printings", &e),
    }
}

fn cheapest_printings(pool: &DbPool, id: &str, user_id: i64) -> Result<Response, WriteError> {
    let conn = pool.get()?;
    if !list_exists(&conn, id, user_id)? {
        return Ok(json_error(StatusCode::NOT_FOUND, "List not found or unauthorized"));
    }
    let key_sql = sql_card_key("cc");
    let key_rows = rows_map(
        &conn,
        &format!(
            "SELECT DISTINCT {key_sql} AS card_key
             FROM list_cards lc
             JOIN card_cache cc ON cc.id = lc.card_id
             WHERE lc.list_id = ?1 AND lc.quantity > 0"
        ),
        &[SqlValue::Text(id.to_string())],
    )?;
    let card_keys: Vec<String> = key_rows
        .iter()
        .filter_map(|r| r.get("card_key").and_then(|v| v.as_str()))
        .filter(|k| !k.is_empty())
        .map(|s| s.to_string())
        .collect();
    let cheapest = get_cheapest_printings(&conn, &card_keys)?;
    let demand_sql = format!(
        "SELECT {key_sql} AS card_key, SUM(lc.quantity) AS quantity
         FROM list_cards lc
         JOIN card_cache cc ON cc.id = lc.card_id
         WHERE lc.list_id = ?1 AND lc.quantity > 0
         GROUP BY card_key"
    );
    let demands = rows_map(&conn, &demand_sql, &[SqlValue::Text(id.to_string())])?;
    let mc_key = sql_card_key("mc_cc");
    let sib_key = sql_card_key("sibling");
    let mut tx = Tx::begin(&conn)?;
    let mut moved: i64 = 0;
    for demand in &demands {
        let Some(Value::String(card_key)) = demand.get("card_key") else { continue };
        let Some(target) = cheapest.get(card_key) else { continue };
        let misaligned = get_row_map(
            &conn,
            &format!(
                "SELECT 1 FROM list_cards mc
                 LEFT JOIN card_cache mc_cc ON mc_cc.id = mc.card_id
                 WHERE mc.list_id = ?1 AND mc.quantity > 0 AND {mc_key} = ?2 AND mc.card_id != ?3
                 LIMIT 1"
            ),
            &[SqlValue::Text(id.to_string()), SqlValue::Text(card_key.clone()), json_to_sql_value(&target.card_id)],
        )?;
        if misaligned.is_none() {
            // Already the sole row and already the cheapest — Node's twin
            // `if (!misaligned) continue;`, keeping reruns honest after a
            // price refresh.
            continue;
        }
        conn.execute(
            "INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?1, ?2, ?3)
             ON CONFLICT(list_id, card_id) DO UPDATE SET quantity = excluded.quantity",
            rusqlite::params![
                SqlValue::Text(id.to_string()),
                json_to_sql_value(&target.card_id),
                json_to_sql_value(demand.get("quantity").unwrap_or(&Value::Null))
            ],
        )?;
        conn.execute(
            &format!(
                "DELETE FROM list_cards
                 WHERE list_id = ?1 AND card_id != ?2
                   AND card_id IN (SELECT sibling.id FROM card_cache sibling WHERE {sib_key} = ?3)"
            ),
            rusqlite::params![SqlValue::Text(id.to_string()), json_to_sql_value(&target.card_id), SqlValue::Text(card_key.clone())],
        )?;
        moved += 1;
    }
    tx.commit()?;
    Ok(json_response(
        StatusCode::OK,
        &json!({ "message": "Cards moved to their cheapest printings", "moved": moved }),
    ))
}

// ---------------------------------------------------------------------------
// PUT /api/lists/:id/cards/:card_id/printing — routes/lists.js:637
// ---------------------------------------------------------------------------

pub async fn handle_printing(
    Path((id, card_id)): Path<(String, String)>,
    State(st): State<AppState>,
    req: Request,
) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::PUT {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers: &HeaderMap = &parts.headers;
    let body = match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // statsCache.invalidateOnWrite twin: Node bumps the user's stats gen
    // after auth, before the handler body (REVIEW-writes L1).
    crate::stats::on_native_write(&st, user.id);
    // `req.body?.card_id ? String(req.body.card_id) : ''` — falsy gate first,
    // then JS String() coercion (numbers -> "5", objects -> "[object Object]").
    let card_v = body.get("card_id");
    if js_falsy(card_v) {
        return json_error(StatusCode::BAD_REQUEST, "card_id is required");
    }
    let to_id = js_to_string(card_v);
    match change_printing(&pool, &id, &card_id, user.id, &to_id) {
        Ok(Some(res)) => res,
        Ok(None) => crate::proxy(State(st), rebuild(parts, &bytes)).await,
        Err(e) => internal_fail("Failed to change printing", "lists change-printing", &e),
    }
}

/// Ok(None) = hydration Forward (target printing not cached; Node hydrates
/// or 404s `Card <id> not found on any card provider.`).
fn change_printing(pool: &DbPool, id: &str, from_id: &str, user_id: i64, to_id: &str) -> Result<Option<Response>, WriteError> {
    let conn = pool.get()?;
    if !list_exists(&conn, id, user_id)? {
        return Ok(Some(json_error(StatusCode::NOT_FOUND, "List not found or unauthorized")));
    }
    let row = get_row_map(
        &conn,
        "SELECT quantity FROM list_cards WHERE list_id = ?1 AND card_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Text(from_id.to_string())],
    )?;
    let Some(row) = row else {
        return Ok(Some(json_error(StatusCode::NOT_FOUND, "Card not in this list")));
    };
    if to_id == from_id {
        return Ok(Some(json_response(
            StatusCode::OK,
            &json!({ "message": "Printing unchanged", "card_id": to_id }),
        )));
    }
    let to_cached = get_row_map(
        &conn,
        "SELECT id FROM card_cache WHERE id = ?1",
        &[SqlValue::Text(to_id.to_string())],
    )?;
    if to_cached.is_none() {
        return Ok(None);
    }
    let ka = sql_card_key("a");
    let kb = sql_card_key("b");
    let same = get_row_map(
        &conn,
        &format!(
            "SELECT 1 FROM card_cache a JOIN card_cache b ON b.id = ?1 WHERE a.id = ?2 AND {ka} = {kb}"
        ),
        &[SqlValue::Text(to_id.to_string()), SqlValue::Text(from_id.to_string())],
    )?;
    if same.is_none() {
        return Ok(Some(json_error(StatusCode::BAD_REQUEST, "Pick a printing of the same card")));
    }
    let mut tx = Tx::begin(&conn)?;
    conn.execute(
        "INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?1, ?2, ?3)
         ON CONFLICT(list_id, card_id) DO UPDATE SET quantity = quantity + excluded.quantity",
        rusqlite::params![
            SqlValue::Text(id.to_string()),
            SqlValue::Text(to_id.to_string()),
            json_to_sql_value(row.get("quantity").unwrap_or(&Value::Null))
        ],
    )?;
    conn.execute(
        "DELETE FROM list_cards WHERE list_id = ?1 AND card_id = ?2",
        rusqlite::params![SqlValue::Text(id.to_string()), SqlValue::Text(from_id.to_string())],
    )?;
    tx.commit()?;
    Ok(Some(json_response(
        StatusCode::OK,
        &json!({ "message": "Printing changed", "card_id": to_id }),
    )))
}

// ---------------------------------------------------------------------------
// DELETE /api/lists/:id/cards/:card_id — routes/lists.js:673
// ---------------------------------------------------------------------------

pub async fn handle_remove_card(Path((id, card_id)): Path<(String, String)>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("lists_writes") || req.method() != Method::DELETE {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers: &HeaderMap = &parts.headers;
    match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => {}
    }
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // statsCache.invalidateOnWrite twin: Node bumps the user's stats gen
    // after auth, before the handler body (REVIEW-writes L1).
    crate::stats::on_native_write(&st, user.id);
    match remove_card(&pool, &id, &card_id, user.id) {
        Ok(res) => res,
        Err(e) => internal_fail("Failed to remove card from list", "lists remove-card", &e),
    }
}

fn remove_card(pool: &DbPool, id: &str, card_id: &str, user_id: i64) -> Result<Response, WriteError> {
    let conn = pool.get()?;
    if !list_exists(&conn, id, user_id)? {
        return Ok(json_error(StatusCode::NOT_FOUND, "List not found or unauthorized"));
    }
    let cc_key = sql_card_key("cc");
    let target_key = sql_card_key("target");
    conn.execute(
        &format!(
            "DELETE FROM list_cards
             WHERE list_id = ?1
               AND (card_id = ?2 OR card_id IN (SELECT cc.id FROM card_cache cc WHERE {cc_key} = (SELECT {target_key} FROM card_cache target WHERE target.id = ?2)))"
        ),
        rusqlite::params![SqlValue::Text(id.to_string()), SqlValue::Text(card_id.to_string())],
    )?;
    Ok(json_response(StatusCode::OK, &json!({ "message": "Card removed from list" })))
}

fn _import_anchors(b: Body) -> Response {
    b.into_response()
}
