//! Native port of the deck META WRITE routes from
//! `backend/src/routes/decks.js`: `PUT /:id`, `DELETE /:id` and
//! `DELETE /:id/cards/:card_id`, `POST /` (create + decklist import),
//! `POST /:id/cards`, `POST /:id/register-collection`, `PUT /:id/checkout`
//! and `PUT /:id/return`. Mounted under the `decks_writes` name in
//! RUST_ROUTES; everything else on /api/decks (combos, synergy, and the
//! POST /:id/cards Scryfall-hydration leg, which forwards the whole request
//! to Node) stays proxied. Reads delegate to `decks_reads` when
//! that group is owned (`route_entry`/`route_collection` in lib.rs).
//! GET list / detail / locations stay with `decks_reads`; combos/synergy
//! (external Spellbook client) stay proxied to Node untouched.
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
    alloc_lock, get_row_map, internal_error, is_basic_land, js_falsy, js_parse_int_v, js_to_string,
    json_error, json_response, json_to_sql_value, node_exact, parse_body, read_parts, record_price,
    rebuild, sql_is_basic_land_expr, too_large, tr, validate_deck_addition, BodyOutcome,
};
use crate::decks_reads::{coverage_sql, js_number, js_trim, rows_map, sql_card_key};
use crate::db::DbPool;
use crate::AppState;
use serde_json::{json, Map, Value};

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
    // js_falsy(None) is true — the 400 above already ran; let-else drops the
    // checked-then-unwrapped bind below (errors-maint L1).
    let Some(name_v) = name else {
        return json_error(StatusCode::BAD_REQUEST, "Deck name is required");
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update deck"),
    };
    let changes = conn.execute(
        "UPDATE decks SET name = ?1, description = ?2 WHERE id = ?3 AND user_id = ?4",
        rusqlite::params_from_iter(
            [js_bind(name_v), desc_or_empty(body.get("description")), SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)]
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
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
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
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
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

// ---------------------------------------------------------------------------
// POST /api/decks — routes/decks.js:159 (create + optional decklist import)
// ---------------------------------------------------------------------------

/// `/api/decks` dispatcher used when BOTH `decks_reads` and `decks_writes`
/// are on (one axum route per path): GET -> decks_reads list, POST -> create,
/// anything else -> Node (never 405; Express falls through to its JSON 404).
pub async fn route_collection(State(st): State<AppState>, req: Request) -> Response {
    if req.method() == Method::GET && st.owns("decks_reads") {
        crate::decks_reads::handle_list(State(st), req).await
    } else if req.method() == Method::POST && st.owns("decks_writes") {
        handle_create(State(st), req).await
    } else {
        crate::proxy(State(st), req).await
    }
}

/// POST /api/decks. express.json runs before the auth gate, so a malformed
/// body outranks 401 (parse -> auth, same twin as handle_entry).
pub async fn handle_create(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("decks_writes") || req.method() != Method::POST {
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
    create_deck(&pool, user.id, &body)
}

fn create_deck(pool: &DbPool, user_id: i64, body: &Value) -> Response {
    const FAILED: &str = "Failed to create deck";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("decks create: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    let name = body.get("name");
    if js_falsy(name) {
        return json_error(StatusCode::BAD_REQUEST, "Deck name is required");
    }
    // js_falsy(None) is true — the 400 above already ran; let-else drops the
    // checked-then-unwrapped bind below (errors-maint L1).
    let Some(name_v) = name else {
        return json_error(StatusCode::BAD_REQUEST, "Deck name is required");
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    // Destructuring defaults apply only to `undefined` (missing key); a
    // present-but-null value binds NULL through the driver.
    let field = |key: &str, default: &str| -> SqlValue {
        match body.get(key) {
            None => SqlValue::Text(default.to_string()),
            Some(v) => js_bind(v),
        }
    };
    // const targetSizeNum = parseInt(target_size, 10) || 60;
    let target_size = match js_parse_int_v(body.get("target_size")) {
        Some(q) if q != 0 => q,
        _ => 60,
    };
    if let Err(e) = conn.execute(
        "INSERT INTO decks (name, description, format, category, accent_color, target_size, user_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            js_bind(name_v),
            field("description", ""),
            field("format", "Standard"),
            field("category", "Competitive"),
            field("accent_color", "#eab308"),
            target_size,
            user_id,
        ],
    ) {
        return fail(&e);
    }
    let new_deck_id = conn.last_insert_rowid();

    // if (decklist_text && typeof decklist_text === 'string') — empty and
    // non-string values skip the import entirely.
    if let Some(Value::String(text)) = body.get("decklist_text") {
        if !text.is_empty() {
            for line in text.split('\n') {
                let trimmed = js_trim(line);
                if trimmed.is_empty() {
                    continue;
                }
                let Some((qty, name_part)) = decklist_line_match(trimmed) else { continue };
                let card_name = js_trim(&name_part);
                let card = match get_row_map(
                    &conn,
                    "SELECT id FROM card_cache WHERE LOWER(name) = LOWER(?1) LIMIT 1",
                    &[SqlValue::Text(card_name.to_string())],
                ) {
                    Ok(r) => r,
                    Err(e) => return fail(&e),
                };
                if let Some(row) = card {
                    let card_id = row.get("id").cloned().unwrap_or(Value::Null);
                    // Node autocommits every line: a mid-import failure 500s
                    // with the deck (and earlier lines) already written — no
                    // rollback, matching the Node driver's half-applied state.
                    if let Err(e) = conn.execute(
                        "INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (?1, ?2, ?3) ON CONFLICT(deck_id, card_id) DO UPDATE SET quantity = quantity + EXCLUDED.quantity",
                        rusqlite::params![new_deck_id, json_to_sql_value(&card_id), qty],
                    ) {
                        return fail(&e);
                    }
                }
            }
        }
    }
    json_response(
        StatusCode::CREATED,
        &json!({ "message": "Deck created successfully", "id": new_deck_id }),
    )
}

/// `/^(\d+)x?\s+(.+)$/i` over the JS-trimmed line. JS `\s` is the exact
/// js_trim set; `.` excludes \n\r\u2028\u2029 and `$` anchors at the end of
/// input, so a line-terminator anywhere in the tail kills the match. The
/// greedy digit run and `x?` never backtrack to a successful alternative
/// (a shortened run leaves a digit before \s+; refusing an available x
/// leaves that x before \s+), so a single deterministic scan is exact.
fn decklist_line_match(line: &str) -> Option<(SqlValue, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let digits: String = chars[..i].iter().collect();
    let mut p = i;
    if matches!(chars.get(p), Some('x') | Some('X')) {
        p += 1;
    }
    let ws_start = p;
    while p < chars.len() && js_regex_ws(chars[p]) {
        p += 1;
    }
    if p == ws_start {
        return None;
    }
    let rest: Vec<char> = chars[p..].to_vec();
    if rest.is_empty() || rest.iter().any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')) {
        return None;
    }
    let qty = digits
        .parse::<i64>()
        .map(SqlValue::Integer)
        .unwrap_or_else(|_| SqlValue::Real(digits.parse::<f64>().unwrap_or(f64::INFINITY)));
    Some((qty, rest.into_iter().collect()))
}

/// JS regex `\s` class (identical set to decks_reads::js_trim).
fn js_regex_ws(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\u{000b}' | '\u{000c}' | '\r' | '\n' | '\u{00a0}' | '\u{feff}'
            | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

// ---------------------------------------------------------------------------
// POST /api/decks/:id/cards — routes/decks.js:571
// ---------------------------------------------------------------------------

enum AddCard {
    Response(Response),
    /// card_cache miss: Node would run cardApi.getCardById -> Scryfall
    /// (lane B's territory), so the untouched request goes to Node, exactly
    /// like the collection_adds Forward policy.
    Forward,
}

pub async fn handle_add_card(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("decks_writes") || req.method() != Method::POST {
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
    match add_card(&pool, &id, user.id, &body) {
        AddCard::Response(res) => res,
        AddCard::Forward => crate::proxy(State(st), rebuild(parts, &bytes)).await,
    }
}

fn add_card(pool: &DbPool, id: &str, user_id: i64, body: &Value) -> AddCard {
    const FAILED: &str = "Failed to add card to deck";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("decks add-card: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    let card_id_v = body.get("card_id");
    if js_falsy(card_id_v) {
        return AddCard::Response(json_error(StatusCode::BAD_REQUEST, "card_id is required"));
    }
    // js_falsy(None) is true — the 400 above already ran; let-else drops the
    // two checked-then-unwrapped binds below (errors-maint L1).
    let Some(card_id) = card_id_v else {
        return AddCard::Response(json_error(StatusCode::BAD_REQUEST, "card_id is required"));
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return AddCard::Response(fail(&e)),
    };
    let deck = match get_row_map(
        &conn,
        "SELECT id, checked_out FROM decks WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    ) {
        Ok(d) => d,
        Err(e) => return AddCard::Response(fail(&e)),
    };
    let Some(deck) = deck else {
        return AddCard::Response(json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized"));
    };
    if tr(deck.get("checked_out")) {
        return AddCard::Response(json_error(
            StatusCode::CONFLICT,
            "Check this deck in before changing its cards.",
        ));
    }
    let card_probe = js_bind(card_id);
    let cached = match get_row_map(
        &conn,
        "SELECT id FROM card_cache WHERE id = ?1",
        &[card_probe.clone()],
    ) {
        Ok(c) => c,
        Err(e) => return AddCard::Response(fail(&e)),
    };
    if cached.is_none() {
        if !matches!(card_id_v, Some(Value::String(_))) {
            // Node reaches cardApi.getCardById, whose `cardId.startsWith`
            // throws TypeError on a non-string -> the route's generic 500.
            return AddCard::Response(fail(&"card_id hydration TypeError (non-string)"));
        }
        return AddCard::Forward;
    }
    let ek = sql_card_key("existing_cc");
    let tk = sql_card_key("target_cc");
    let equivalent = match get_row_map(
        &conn,
        &format!(
            "SELECT dc.card_id
             FROM deck_cards dc
             JOIN card_cache existing_cc ON existing_cc.id = dc.card_id
             JOIN card_cache target_cc ON target_cc.id = ?1
             WHERE dc.deck_id = ?2
               AND {ek} = {tk}
             ORDER BY (dc.card_id = ?3) DESC, dc.card_id
             LIMIT 1"
        ),
        &[card_probe.clone(), SqlValue::Text(id.to_string()), card_probe.clone()],
    ) {
        Ok(r) => r,
        Err(e) => return AddCard::Response(fail(&e)),
    };
    let effective = equivalent
        .as_ref()
        .and_then(|r| r.get("card_id").cloned())
        // Node binds the raw body value for the else-branch (driver
        // coercion happens at bind time via json_to_sql_value).
        .unwrap_or_else(|| card_id.clone());
    // const { card_id, quantity = 1 } = req.body — destructuring default.
    let quantity = body.get("quantity").cloned().unwrap_or(Value::from(1));
    let qty_opt = js_parse_int_v(Some(&quantity));
    // validateDeckAddition parses the same raw value; a NaN/negative input
    // must flow through its 'Invalid quantity' branch (ordering preserved).
    let new_qty = qty_opt.map_or(f64::NAN, |q| q as f64);
    match validate_deck_addition(&conn, user_id, &effective, new_qty) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => return AddCard::Response(json_error(StatusCode::BAD_REQUEST, &msg)),
        Err(e) => return AddCard::Response(fail(&e)),
    }
    let parsed_quantity = qty_opt.unwrap_or(1);
    let eff_sql = json_to_sql_value(&effective);
    if equivalent.is_some() {
        // Collapse alt-printing rows into one representative, then drop the
        // zeros — two autocommit statements like the Node driver, never a tx.
        let updated = conn.execute(
            &format!(
                "UPDATE deck_cards
                 SET quantity = CASE WHEN card_id = ?1 THEN ?2 ELSE 0 END
                 WHERE deck_id = ?3
                   AND EXISTS (
                     SELECT 1 FROM decks guarded_deck
                     WHERE guarded_deck.id = deck_cards.deck_id
                       AND guarded_deck.user_id = ?4
                       AND guarded_deck.checked_out = 0
                   )
                   AND card_id IN (
                     SELECT existing_cc.id
                     FROM card_cache existing_cc
                     JOIN card_cache target_cc ON target_cc.id = ?5
                     WHERE {ek} = {tk}
                   )"
            ),
            rusqlite::params![eff_sql.clone(), parsed_quantity, SqlValue::Text(id.to_string()), SqlValue::Integer(user_id), eff_sql.clone()],
        );
        match updated {
            Ok(0) => {
                return AddCard::Response(json_error(
                    StatusCode::CONFLICT,
                    "The deck was checked out before the card update could be saved.",
                ))
            }
            Ok(_) => {}
            Err(e) => return AddCard::Response(fail(&e)),
        }
        if let Err(e) = conn.execute(
            "DELETE FROM deck_cards WHERE deck_id = ?1 AND quantity <= 0",
            rusqlite::params![SqlValue::Text(id.to_string())],
        ) {
            return AddCard::Response(fail(&e));
        }
    } else {
        let inserted = conn.execute(
            "INSERT INTO deck_cards (deck_id, card_id, quantity)
             SELECT ?1, ?2, ?3
             WHERE EXISTS (
               SELECT 1 FROM decks guarded_deck
               WHERE guarded_deck.id = ?4
                 AND guarded_deck.user_id = ?5
                 AND guarded_deck.checked_out = 0
             )",
            rusqlite::params![SqlValue::Text(id.to_string()), eff_sql.clone(), parsed_quantity, SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
        );
        match inserted {
            Ok(0) => {
                return AddCard::Response(json_error(
                    StatusCode::CONFLICT,
                    "The deck was checked out before the card could be added.",
                ))
            }
            Ok(_) => {}
            Err(e) => return AddCard::Response(fail(&e)),
        }
    }
    let cache_card = match get_row_map(
        &conn,
        "SELECT price_trend FROM card_cache WHERE id = ?1",
        &[eff_sql.clone()],
    ) {
        Ok(r) => r,
        Err(e) => return AddCard::Response(fail(&e)),
    };
    if let Some(row) = cache_card {
        let price = row.get("price_trend").cloned().unwrap_or(Value::Null);
        if let Err(e) = record_price(&conn, &eff_sql, &price) {
            // Unlike register-collection this throw is NOT swallowed —
            // Node's outer catch turns it into the plain 500.
            return AddCard::Response(fail(&e));
        }
    }
    AddCard::Response(json_response(
        StatusCode::OK,
        &json!({ "message": "Card added/updated in deck successfully" }),
    ))
}

// ---------------------------------------------------------------------------
// POST /api/decks/:id/register-collection — routes/decks.js:728
// ---------------------------------------------------------------------------

pub async fn handle_register(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("decks_writes") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes, oversize) = read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers: &HeaderMap = &parts.headers;
    // The route ignores the body but express.json parsed it ahead of auth,
    // so a malformed one still 500s before the 401.
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
    register_deck(&pool, &id, user.id)
}

fn register_deck(pool: &DbPool, id: &str, user_id: i64) -> Response {
    const FAILED: &str = "Failed to register deck in collection";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("decks register: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let deck = match get_row_map(
        &conn,
        "SELECT id, name FROM decks WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    ) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    let Some(deck) = deck else {
        return json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized");
    };
    // One statement (register-vs-checkout linearizable), returning the rows.
    let inserted = match rows_map(
        &conn,
        "INSERT INTO collection (
            card_id, user_id, quantity, condition, printing, language,
            purchase_price, is_trade
          )
          SELECT
            dc.card_id,
            d.user_id,
            dc.quantity,
            'Near Mint',
            'Normal',
            COALESCE(NULLIF(cc.language, ''), 'English'),
            0,
            0
          FROM decks d
          JOIN deck_cards dc ON dc.deck_id = d.id
          JOIN card_cache cc ON cc.id = dc.card_id
          WHERE d.id = ?1
            AND d.user_id = ?2
            AND d.checked_out = 0
            AND dc.quantity > 0
            AND NOT EXISTS (
              SELECT 1
              FROM deck_cards unresolved
              LEFT JOIN card_cache cached ON cached.id = unresolved.card_id
              WHERE unresolved.deck_id = d.id
                AND unresolved.quantity > 0
                AND cached.id IS NULL
            )
          ORDER BY dc.card_id
          RETURNING card_id, quantity",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    ) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    if inserted.is_empty() {
        let current = match get_row_map(
            &conn,
            "SELECT checked_out FROM decks WHERE id = ?1 AND user_id = ?2",
            &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
        ) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        let Some(current) = current else {
            return json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized");
        };
        if tr(current.get("checked_out")) {
            return json_error(StatusCode::CONFLICT, "This deck is already checked out for play.");
        }
        let source_state = match get_row_map(
            &conn,
            "SELECT
              COUNT(*) AS card_types,
              COALESCE(SUM(CASE WHEN cc.id IS NULL THEN 1 ELSE 0 END), 0) AS unresolved
            FROM deck_cards dc
            LEFT JOIN card_cache cc ON cc.id = dc.card_id
            WHERE dc.deck_id = ?1 AND dc.quantity > 0",
            &[SqlValue::Text(id.to_string())],
        ) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        let ss = source_state.unwrap_or_default();
        if !tr(ss.get("card_types")) {
            return json_error(StatusCode::BAD_REQUEST, "This deck has no cards to register.");
        }
        if tr(ss.get("unresolved")) {
            return json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "This deck contains cards whose details are unavailable. Refresh or re-import it before registering.",
            );
        }
        return json_error(
            StatusCode::CONFLICT,
            "The deck changed before it could be registered. Refresh and try again.",
        );
    }
    // Price history is auxiliary: Node console.errors and continues.
    let price_ids: Vec<SqlValue> = inserted
        .iter()
        .map(|r| r.get("card_id").cloned().map(|v| json_to_sql_value(&v)).unwrap_or(SqlValue::Null))
        .collect();
    if !price_ids.is_empty() {
        let placeholders: Vec<String> = (1..=price_ids.len()).map(|i| format!("?{i}")).collect();
        match rows_map(
            &conn,
            &format!(
                "SELECT id, price_trend FROM card_cache WHERE id IN ({})",
                placeholders.join(", ")
            ),
            &price_ids,
        ) {
            Ok(rows) => {
                for row in rows {
                    let cid = row.get("id").cloned().map(|v| json_to_sql_value(&v)).unwrap_or(SqlValue::Null);
                    let price = row.get("price_trend").cloned().unwrap_or(Value::Null);
                    if let Err(e) = record_price(&conn, &cid, &price) {
                        tracing::error!("decks register price for {cid:?}: {e:?}");
                    }
                }
            }
            // The sweep SELECT is NOT individually guarded in Node — a throw
            // lands in the route catch and 500s the whole request (the
            // per-card recordPrice errors above are the caught ones).
            Err(e) => return fail(&e),
        }
    }
    // const total = inserted.reduce((s, c) => s + Number(c.quantity || 0), 0)
    let total: f64 = inserted
        .iter()
        .map(|r| r.get("quantity").filter(|q| truthy(q)).map(|q| js_number(Some(q))).unwrap_or(0.0))
        .sum();
    let name = js_to_string(deck.get("name"));
    json_response(
        StatusCode::CREATED,
        &json!({
            "message": format!("Added {} cards from {name} to your collection.", js_to_string(Some(&crate::collection::json_num(total)))),
            "added": crate::collection::json_num(total),
            "card_types": inserted.len(),
        }),
    )
}

// ---------------------------------------------------------------------------
// PUT /api/decks/:id/checkout — routes/decks.js:837
// ---------------------------------------------------------------------------

pub async fn handle_checkout(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("decks_writes") || req.method() != Method::PUT {
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
    checkout_deck(&pool, &id, user.id).await
}

fn checkout_sql() -> String {
    format!(
        "
      WITH target_keys AS (
        SELECT DISTINCT {key_t} AS card_key
        FROM deck_cards target_dc
        JOIN card_cache target_cc ON target_cc.id = target_dc.card_id
        WHERE target_dc.deck_id = ?1 AND target_dc.quantity > 0
      ),
      owned AS (
        SELECT {key_o} AS card_key, SUM(owned.quantity) AS owned_qty
        FROM card_cache owned_cc
        CROSS JOIN collection owned
        WHERE owned.card_id = owned_cc.id
          AND owned.user_id = ?2
          AND owned.quantity > 0
          AND {key_o} IN (SELECT card_key FROM target_keys)
        GROUP BY {key_o}
      ),
      locked AS (
        SELECT {key_l} AS card_key, SUM(locked_dc.quantity) AS locked_qty
        FROM deck_cards locked_dc
        JOIN card_cache locked_cc ON locked_cc.id = locked_dc.card_id
        JOIN decks locked_deck ON locked_deck.id = locked_dc.deck_id
        WHERE locked_deck.user_id = ?3
          AND locked_deck.checked_out = 1
          AND locked_dc.quantity > 0
          AND locked_deck.id != ?4
          AND {key_l} IN (SELECT card_key FROM target_keys)
        GROUP BY {key_l}
      )
      UPDATE decks AS target
      SET checked_out = 1, checked_out_at = CURRENT_TIMESTAMP
      WHERE target.id = ?5
        AND target.user_id = ?6
        AND target.checked_out = 0
        AND EXISTS (
          SELECT 1 FROM deck_cards present
          WHERE present.deck_id = target.id AND present.quantity > 0
        )
        AND NOT EXISTS (
          SELECT 1
          FROM deck_cards unresolved
          LEFT JOIN card_cache unresolved_cc ON unresolved_cc.id = unresolved.card_id
          WHERE unresolved.deck_id = target.id
            AND unresolved.quantity > 0
            AND unresolved_cc.id IS NULL
        )
        AND NOT EXISTS (
          SELECT 1
          FROM decks orphan_deck
          JOIN deck_cards orphan_dc ON orphan_dc.deck_id = orphan_deck.id
          LEFT JOIN card_cache orphan_cc ON orphan_cc.id = orphan_dc.card_id
          WHERE orphan_deck.user_id = target.user_id
            AND orphan_deck.checked_out = 1
            AND orphan_dc.quantity > 0
            AND orphan_cc.id IS NULL
        )
        AND NOT EXISTS (
          SELECT 1
          FROM (
            SELECT {key_n} AS card_key, SUM(needed_dc.quantity) AS needed_qty
            FROM deck_cards needed_dc
            JOIN card_cache needed_cc ON needed_cc.id = needed_dc.card_id
            WHERE needed_dc.deck_id = ?7
              AND needed_dc.quantity > 0
              AND NOT {basic}
            GROUP BY {key_n}
          ) needed
          LEFT JOIN owned ON owned.card_key = needed.card_key
          LEFT JOIN locked ON locked.card_key = needed.card_key
          WHERE needed.needed_qty >
            COALESCE(owned.owned_qty, 0) - COALESCE(locked.locked_qty, 0)
        )
      RETURNING id
    ",
        key_t = sql_card_key("target_cc"),
        key_o = sql_card_key("owned_cc"),
        key_l = sql_card_key("locked_cc"),
        key_n = sql_card_key("needed_cc"),
        basic = sql_is_basic_land_expr("needed_cc"),
    )
}

async fn checkout_deck(pool: &DbPool, id: &str, user_id: i64) -> Response {
    const FAILED: &str = "Failed to checkout deck";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("decks checkout: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    // H3/M3: no pool checkout is held while waiting on alloc_lock below.
    // withAllocationLock wraps only the guarded mutation; the diagnostics
    // below run outside it, like Node's. The checkout query's connection is
    // taken INSIDE the lock — pre-fix a single connection was taken up
    // front and held across the alloc_lock wait, so under the pool's live
    // cap queued writers could starve the lock holder (deadlock).
    let checked_out = {
        let _guard = alloc_lock().lock().await;
        let conn = match pool.get() {
            Ok(c) => c,
            Err(e) => return fail(&e),
        };
        rows_map(
            &conn,
            &checkout_sql(),
            &[
                SqlValue::Text(id.to_string()),
                SqlValue::Integer(user_id),
                SqlValue::Integer(user_id),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(id.to_string()),
                SqlValue::Integer(user_id),
                SqlValue::Text(id.to_string()),
            ],
        )
    };
    let checked_out = match checked_out {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    if !checked_out.is_empty() {
        return json_response(StatusCode::OK, &json!({ "message": "Deck checked out successfully" }));
    }
    // Diagnostics + mutation run outside the lock, like Node's — this
    // checkout is taken only on the slow path, after every alloc_lock wait.
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let deck = match get_row_map(
        &conn,
        "SELECT id, checked_out FROM decks WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    ) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    let Some(deck) = deck else {
        return json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized");
    };
    if tr(deck.get("checked_out")) {
        return json_error(StatusCode::CONFLICT, "This deck is already checked out for play.");
    }
    let source_state = match get_row_map(
        &conn,
        "SELECT
          COUNT(*) AS card_types,
          COALESCE(SUM(CASE WHEN cc.id IS NULL THEN 1 ELSE 0 END), 0) AS unresolved
        FROM deck_cards dc
        LEFT JOIN card_cache cc ON cc.id = dc.card_id
        WHERE dc.deck_id = ?1 AND dc.quantity > 0",
        &[SqlValue::Text(id.to_string())],
    ) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let ss = source_state.unwrap_or_default();
    if !tr(ss.get("card_types")) {
        return json_error(StatusCode::BAD_REQUEST, "This deck has no cards to check out.");
    }
    if tr(ss.get("unresolved")) {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "This deck contains cards whose details are unavailable. Refresh or re-import it before checkout.",
        );
    }
    let cards = match rows_map(
        &conn,
        &coverage_sql(),
        &[
            SqlValue::Text(id.to_string()),
            SqlValue::Integer(user_id),
            SqlValue::Integer(user_id),
            SqlValue::Text(id.to_string()),
        ],
    ) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let mut errors: Vec<Value> = Vec::new();
    for card in &cards {
        let mut probe = Map::new();
        probe.insert("name".into(), card.get("name").cloned().unwrap_or(Value::Null));
        probe.insert("supertype".into(), card.get("supertype").cloned().unwrap_or(Value::Null));
        probe.insert("subtypes".into(), card.get("subtypes").cloned().unwrap_or(Value::Null));
        if is_basic_land(&probe) {
            continue;
        }
        let available = js_or_number(card.get("owned_qty")) - js_or_number(card.get("locked_qty"));
        if js_or_number(card.get("required_qty")) > available {
            let deficit = js_number(card.get("required_qty")) - available;
            errors.push(Value::String(format!(
                "Missing {}x {} (Owned: {}, In Use: {})",
                js_to_string(Some(&crate::collection::json_num(deficit))),
                js_to_string(card.get("name")),
                js_to_string(card.get("owned_qty")),
                js_to_string(card.get("locked_qty")),
            )));
        }
    }
    if !errors.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "Not enough cards available to check out this deck.", "details": errors }),
        );
    }
    json_response(
        StatusCode::CONFLICT,
        &json!({ "error": "Card availability changed before checkout completed. Refresh and try again." }),
    )
}

/// `Number(x || 0)` — the diagnostics' falsy-collapse before Number().
fn js_or_number(v: Option<&Value>) -> f64 {
    match v {
        Some(x) if truthy(x) => js_number(Some(x)),
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// PUT /api/decks/:id/return — routes/decks.js:987
// ---------------------------------------------------------------------------

pub async fn handle_return(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("decks_writes") || req.method() != Method::PUT {
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
    return_deck(&pool, &id, user.id)
}

fn return_deck(pool: &DbPool, id: &str, user_id: i64) -> Response {
    const FAILED: &str = "Failed to return deck";
    let fail = |e: &dyn std::fmt::Debug| {
        tracing::error!("decks return: {e:?}");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let deck = match get_row_map(
        &conn,
        "SELECT id FROM decks WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    ) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    if deck.is_none() {
        return json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized");
    }
    // Ownership was proven by the SELECT; the UPDATE itself is id-scoped
    // only — and returns 200 even when it changes 0 rows.
    if let Err(e) = conn.execute(
        "UPDATE decks SET checked_out = 0, checked_out_at = NULL WHERE id = ?1",
        rusqlite::params![SqlValue::Text(id.to_string())],
    ) {
        return fail(&e);
    }
    json_response(
        StatusCode::OK,
        &json!({ "message": "Deck returned to collection successfully" }),
    )
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkout_deck_never_holds_a_checkout_across_alloc_lock() {
        // H3/M3 (REVIEW-writes): pre-fix, checkout_deck took the pool's
        // only connection BEFORE awaiting alloc_lock. A lock holder that
        // then needed a connection (the bulk_delete/delete_inner lock-then-
        // get shape) starved forever — deadlock under the pool's live cap.
        // Red-before-fix: with the get-before-lock pattern the holder join
        // below times out. Positive control: pre-fix (and post-fix) a
        // writer with no competing lock holder completes normally — see
        // tests/decks_writes.rs checkout coverage.
        let dir = std::env::temp_dir().join(format!("scrybox-checkout-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        {
            let setup = rusqlite::Connection::open(&path).unwrap();
            setup.execute_batch(include_str!("../tests/schema_fixture.sql")).unwrap();
        }
        // Exactly one connection may be LIVE.
        let pool = std::sync::Arc::new(
            crate::db::DbPool::open_with(
                &path,
                crate::db::PoolConfig { max: 1, cache_kb: 2048, mmap_kb: 0, live_max: 1 },
            )
            .unwrap(),
        );

        // Lock holder: takes alloc_lock, waits until the writer is queued,
        // THEN needs a connection (lock-then-get, like bulk_delete).
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let hp = std::sync::Arc::clone(&pool);
        let holder = tokio::spawn(async move {
            let _g = alloc_lock().lock().await;
            let _ = held_tx.send(());
            // Wait until the queued writer has had time to reach its
            // (pre-fix) get-before-lock block, then demand a connection.
            let _ = go_rx.await;
            let c = hp.get().unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(c);
        });
        held_rx.await.unwrap();

        // Queued writer: must not be holding a checkout while it waits on
        // the lock. Deck 999 does not exist → 404 after the lock section.
        let cp = std::sync::Arc::clone(&pool);
        let writer = tokio::spawn(async move { checkout_deck(&cp, "999", 1).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = go_tx.send(());

        tokio::time::timeout(std::time::Duration::from_secs(5), holder)
            .await
            .expect("M3: alloc_lock holder starved — checkout held across the lock wait")
            .unwrap();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), writer)
            .await
            .expect("M3: checkout_deck deadlocked behind the pool cap")
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(pool.live_count(), 0);
    }
}
