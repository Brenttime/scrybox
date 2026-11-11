//! Native port of the deck META WRITE routes from
//! `backend/src/routes/decks.js`: `PUT /:id`, `DELETE /:id` and
//! `DELETE /:id/cards/:card_id`. Mounted under the `decks_writes` name in
//! RUST_ROUTES; everything else on /api/decks (reads, POST, cards POST,
//! checkout/return, combos, synergy, locations, register-collection) stays
//! proxied to Node untouched.
//!
//! Every status, body and message was written against the actual Node
//! source (routes/decks.js lines 522-570 and 677-727, db.js sqlite3
//! bindings, utils/cardIdentity.js) plus live byte-pinning against the
//! :3312 container — deliberate deviations are documented on the functions.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::extract::Request;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;

use crate::auth::{authenticate, bearer_token};
use crate::collection::{sqlite_value, truthy};
use crate::collection_writes::{
    alloc_lock, internal_error, js_falsy, json_error, json_response, parse_body, read_parts,
    too_large, BodyOutcome,
};
use crate::db::DbPool;
use crate::AppState;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Binding semantics — verified live against Node :3312 (PUT /api/decks/931001
// with each JSON shape; stored value read back from the DB copy):
//   true -> '1' (node-sqlite3 binds bool as INTEGER 1/0, TEXT affinity renders it)
//   42   -> '42'        1e400 -> 'Inf'   (REAL affinity rendering)
//   {}   -> '[object Object]'   [1,2] and [] -> '[object Object]'
// (the sqlite3 driver's generic-value path, not JS String(): String([1,2])
// would be '1,2', which the live probe disproved).
// ---------------------------------------------------------------------------

fn js_bind(v: &Value) -> SqlValue {
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

/// JS `description || ''` over a body value.
fn desc_or_empty(v: Option<&Value>) -> SqlValue {
    match v {
        Some(x) if !js_falsy(Some(x)) => js_bind(x),
        _ => SqlValue::Text(String::new()),
    }
}

fn get_col(conn: &Connection, sql: &str, params: &[SqlValue], idx: usize) -> Result<Option<Value>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(sqlite_value(row, idx)?)),
    }
}

// ---------------------------------------------------------------------------
// PUT /api/decks/:id — routes/decks.js:522.
// ---------------------------------------------------------------------------

fn put_deck(pool: &DbPool, id: &str, user_id: i64, body: &Value) -> Response {
    // `const { name, description } = req.body` then `if (!name)`. parse_body
    // has already reduced non-object JSON to {} (arrays) or 500 (primitives),
    // matching Node's strict-mode + error-handler answers (live-verified).
    let name = body.get("name");
    if js_falsy(name) {
        return json_error(StatusCode::BAD_REQUEST, "Deck name is required");
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update deck"),
    };
    let changes = conn.execute(
        "UPDATE decks SET name = ?1, description = ?2 WHERE id = ?3 AND user_id = ?4",
        rusqlite::params_from_iter(
            [js_bind(name.unwrap()), desc_or_empty(body.get("description")), SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)]
                .iter(),
        ),
    );
    match changes {
        Ok(0) => json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized"),
        Ok(_) => json_response(StatusCode::OK, &json!({ "message": "Deck updated successfully" })),
        Err(e) => {
            tracing::error!(error = ?e, "deck put failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update deck")
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/decks/:id — routes/decks.js:548. Node wraps the whole flow in
// withAllocationLock + withDedicatedTransaction (BEGIN IMMEDIATE); the
// in-process lock serialises same-process writers, BEGIN IMMEDIATE
// serialises against Node's own writes on the shared WAL file. The DB
// triggers on decks/deck_cards bump deck_list_revision, so Node's deck-list
// cache invalidates without any HTTP-side hook.
// ---------------------------------------------------------------------------

async fn delete_deck(pool: &DbPool, id: &str, user_id: i64) -> Response {
    const FAILED: &str = "Failed to delete deck";
    let _guard = alloc_lock().lock().await;
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED),
    };
    let rollback = || {
        let _ = conn.execute_batch("ROLLBACK");
    };
    if conn.execute_batch("BEGIN IMMEDIATE").is_err() {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
    }
    let deck = match get_col(
        &conn,
        "SELECT id, checked_out FROM decks WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
        1,
    ) {
        Ok(v) => v,
        Err(_) => {
            rollback();
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
        }
    };
    let Some(deck) = deck else {
        rollback();
        return json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized");
    };
    if truthy(&deck) {
        rollback();
        return json_error(StatusCode::CONFLICT, "Check this deck in before changing its cards.");
    }
    for sql in [
        "DELETE FROM deck_cards WHERE deck_id = ?1",
        "DELETE FROM deck_maybe_cards WHERE deck_id = ?1",
    ] {
        if conn
            .execute(sql, rusqlite::params_from_iter([SqlValue::Text(id.to_string())].iter()))
            .is_err()
        {
            rollback();
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
        }
    }
    let deleted = conn.execute(
        "DELETE FROM decks WHERE id = ?1 AND user_id = ?2",
        rusqlite::params_from_iter([SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)].iter()),
    );
    // Node throws 'Deck disappeared during deletion' (-> catch -> 500) unless
    // exactly one row went away.
    if !matches!(deleted, Ok(1)) {
        rollback();
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
    }
    if conn.execute_batch("COMMIT").is_err() {
        rollback();
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
    }
    json_response(StatusCode::OK, &json!({ "message": "Deck deleted successfully" }))
}

// ---------------------------------------------------------------------------
// DELETE /api/decks/:id/cards/:card_id — routes/decks.js:677. No allocation
// lock and no transaction in Node (plain db.get + db.run autocommit). The
// guarded DELETE removes every printing of the same logical card (cardIdentity
// sqlCardKey) but only while the deck is checked-in.
// ---------------------------------------------------------------------------

fn delete_deck_card(pool: &DbPool, id: &str, card_id: &str, user_id: i64) -> Response {
    const FAILED: &str = "Failed to remove card from deck";
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED),
    };
    let idp = SqlValue::Text(id.to_string());
    let uid = SqlValue::Integer(user_id);
    let cid = SqlValue::Text(card_id.to_string());
    let deck = match get_col(
        &conn,
        "SELECT id, checked_out FROM decks WHERE id = ?1 AND user_id = ?2",
        &[idp.clone(), uid.clone()],
        1,
    ) {
        Ok(v) => v,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED),
    };
    let Some(deck) = deck else {
        return json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized");
    };
    if truthy(&deck) {
        return json_error(StatusCode::CONFLICT, "Check this deck in before changing its cards.");
    }
    let removed = conn.execute(
        "DELETE FROM deck_cards
         WHERE deck_id = ?1
           AND EXISTS (
             SELECT 1 FROM decks guarded_deck
             WHERE guarded_deck.id = deck_cards.deck_id
               AND guarded_deck.user_id = ?2
               AND guarded_deck.checked_out = 0
           )
           AND (
             card_id = ?3
             OR card_id IN (
               SELECT existing_cc.id
               FROM card_cache existing_cc
               JOIN card_cache target_cc ON target_cc.id = ?3
               WHERE LOWER(TRIM(COALESCE(existing_cc.name, ''))) = LOWER(TRIM(COALESCE(target_cc.name, '')))
             )
           )",
        rusqlite::params_from_iter([idp.clone(), uid.clone(), cid.clone()].iter()),
    );
    match removed {
        Ok(0) => {
            // Node's checked-out race re-check: only when a deck still exists
            // and is checked out does the 0-change DELETE become a 409.
            let current = get_col(
                &conn,
                "SELECT checked_out FROM decks WHERE id = ?1 AND user_id = ?2",
                &[idp, uid],
                0,
            );
            if matches!(&current, Ok(Some(v)) if truthy(v)) {
                return json_error(
                    StatusCode::CONFLICT,
                    "The deck was checked out before the card could be removed.",
                );
            }
            json_response(StatusCode::OK, &json!({ "message": "Card removed from deck successfully" }))
        }
        Ok(_) => json_response(StatusCode::OK, &json!({ "message": "Card removed from deck successfully" })),
        Err(e) => {
            tracing::error!(error = ?e, "deck card delete failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
        }
    }
}

// ---------------------------------------------------------------------------
// Router entries — methods/shapes the port does not own proxy untouched, so
// /api/decks/5/combos etc. never 405 through this router.
// ---------------------------------------------------------------------------

pub async fn handle_entry(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    if method != Method::PUT && method != Method::DELETE {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    let headers: &HeaderMap = &parts.headers;

    // express.json runs globally ahead of the auth gate: parse failures
    // outrank auth errors (live-verified: bad JSON with no token -> 500).
    let body = match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };

    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return crate::collection_writes::node_exact(res).await,
    };

    if method == Method::PUT {
        put_deck(&pool, &id, user.id, &body)
    } else {
        delete_deck(&pool, &id, user.id).await
    }
}

pub async fn handle_card(
    Path((id, card_id)): Path<(String, String)>,
    State(st): State<AppState>,
    req: Request,
) -> Response {
    if req.method() != Method::DELETE {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    let headers: &HeaderMap = &parts.headers;

    let body = match parse_body(headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => Value::Null,
    };
    let _ = body; // the route ignores the parsed body; parsing only mirrors error order.

    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return crate::collection_writes::node_exact(res).await,
    };

    delete_deck_card(&pool, &id, &card_id, user.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_bind_matches_node_driver() {
        assert!(matches!(js_bind(&json!(true)), SqlValue::Integer(1)));
        assert!(matches!(js_bind(&json!(false)), SqlValue::Integer(0)));
        assert!(matches!(js_bind(&json!(42)), SqlValue::Integer(42)));
        assert!(matches!(js_bind(&json!("x")), SqlValue::Text(s) if s == "x"));
        assert!(matches!(js_bind(&json!(null)), SqlValue::Null));
        assert!(matches!(js_bind(&json!({})), SqlValue::Text(s) if s == "[object Object]"));
        assert!(matches!(js_bind(&json!([1, 2])), SqlValue::Text(s) if s == "[object Object]"));
        if let SqlValue::Real(f) = js_bind(&json!(1.5)) {
            assert_eq!(f, 1.5);
        } else {
            panic!("real expected");
        }
    }

    #[test]
    fn desc_or_empty_falsy_contract() {
        // undefined, null, false, 0, NaN, "" all collapse to ''.
        assert!(matches!(desc_or_empty(None), SqlValue::Text(s) if s.is_empty()));
        assert!(matches!(desc_or_empty(Some(&json!(null))), SqlValue::Text(s) if s.is_empty()));
        assert!(matches!(desc_or_empty(Some(&json!(0))), SqlValue::Text(s) if s.is_empty()));
        assert!(matches!(desc_or_empty(Some(&json!(""))), SqlValue::Text(s) if s.is_empty()));
        assert!(matches!(desc_or_empty(Some(&json!("d"))), SqlValue::Text(s) if s == "d"));
        assert!(matches!(desc_or_empty(Some(&json!(true))), SqlValue::Integer(1)));
    }
}
