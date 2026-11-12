//! Port of the ADMIN USERS group from `backend/src/routes/admin.js`
//! (Lane A iter-40, RUST_ROUTES=`admin_users`):
//!   GET    /api/admin/users
//!   POST   /api/admin/users
//!   PUT    /api/admin/users/:id
//!   DELETE /api/admin/users/:id
//! The rest of admin.js (seed-cards, catalogs, models) is Lane B's unit-4
//! surface and stays proxied; so do DELETE /api/admin/users/:id siblings on
//! other methods (POST /users/:id is not routed in Express — the catch-all
//! proxy answers it exactly like Node's 404).
//!
//! Middleware order mirrored from server.js + admin.js:17:
//!   body-parser (normalJson, 1 MiB, strict) — BEFORE authentication, so a
//!   malformed-JSON POST answers the generic 500 / oversize a 413 with no
//!   token at all; then authenticateToken; then the router-level
//!   requireAdmin twin (`settings::admin_gate`); then handler. (Node's
//!   statsCache.invalidateOnWrite sits on the bare '/api' mounts, BELOW
//!   server.js's '/api/admin' mount — it never sees admin-surface writes;
//!   the Rust on_proxied_write call is hygiene-only, see authorize().)
//!
//! Deliberate deviations (same rulings as the settings/lists ports):
//! - non-string username/password in POST and PUT reach a TypeError in
//!   Node's async handler — an unhandled rejection there is fatal on Node
//!   22 (crash, no HTTP answer). The Rust twin never dies: PUT answers the
//!   route-catch 500 ('Failed to update user'), POST the generic 500.
//!   Such bodies cannot appear in a live parity run.
//! - non-JSON content-type POST/PUT: body-parser leaves req.body undefined
//!   and Node's destructuring dies; the port treats it as `{}` (the
//!   collection_writes::parse_body convention).
//! - Node's DELETE /users/:id runs on a dedicated connection with
//!   PRAGMA foreign_keys = ON (db.js:154), so dropping the user CASCADES
//!   their decks + deck_cards. The Rust twin sets the same pragma for the
//!   transaction and restores OFF before the pooled connection returns.
//!
//! Value-level pins:
//! - GET per-user stats SQL runs VERBATIM (card_cache join, is_proxy zero,
//!   Holofoil/Normal price ladder, ELSE price_trend) — identical SQL,
//!   identical doubles both sides; only the JS `||` folds and
//!   `parseFloat((v||0).toFixed(2))` need twins (collection_writes' value
//!   helpers + stats::js_fixed_parsed).
//! - GET key order: SELECT order (id, username, role, share_enabled,
//!   created_at) then total_cards, unique_cards, total_value — the Node
//!   spread `{...u, total_cards, unique_cards, total_value}` order.
//! - POST hash = db.hashPassword (PBKDF2 210000:hex-salt:hex, sha512,
//!   auth_support::hash_password) and share_token = randomBytes(16).hex
//!   (auth_support::random_hex(16)); neither value is in any response, so
//!   parity asserts shape + persistence (login with the created password).
//! - PUT gates are `!== undefined` — an explicit null RUNS the arm
//!   (null.length TypeError → route-catch 500 'Failed to update user';
//!   a number's .length is undefined, skips the <8 gate, then pbkdf2Sync
//!   throws on the non-string — same 500).
//! - PUT commits the password arm BEFORE the role arm validates: a 400
//!   'Invalid role' leaves the new password committed (separate autocommit
//!   db.runs, no transaction).
//! - DELETE self-check uses parseInt(id,10) (prefix parse: '2abc' === 2)
//!   and runs BEFORE the try, but it is a plain response, not a throw.

use axum::extract::{Path, Request, State};
use axum::http::{header::HeaderMap, Method, StatusCode};
use axum::response::Response;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};

use crate::auth;
use crate::auth_support::{hash_password, random_hex};
use crate::collection_writes::{
    alloc_lock, get_row_map, internal_error, js_falsy, js_parse_int_v, js_to_string, json_error,
    json_response, parse_body, rebuild, too_large, BodyOutcome,
};
use crate::db::DbPool;
use crate::decks_reads::{js_trim, rows_map};
use crate::settings::{admin_gate, authenticate};
use crate::stats::js_fixed_parsed;
use crate::AppState;

// ---------------------------------------------------------------------------
// Router entries
// ---------------------------------------------------------------------------

/// `/api/admin/users` — GET list + POST create; every other method rides the
/// catch-all proxy (never a local 404/405).
pub async fn route_users(State(st): State<AppState>, req: Request) -> Response {
    match req.method().clone() {
        axum::http::Method::GET => handle_get(State(st), req).await,
        axum::http::Method::POST => handle_post(State(st), req).await,
        _ => crate::proxy(State(st), req).await,
    }
}

/// `/api/admin/users/:id` — PUT update + DELETE delete; other methods proxy.
pub async fn route_user(State(st): State<AppState>, Path(id): Path<String>, req: Request) -> Response {
    match req.method().clone() {
        axum::http::Method::PUT => handle_put(State(st), id, req).await,
        axum::http::Method::DELETE => handle_delete(State(st), id, req).await,
        _ => crate::proxy(State(st), req).await,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// JS `String#length` counts UTF-16 code units (the auth port's rule).
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `x || 0` for the two integer aggregates (0/NaN/Null fold to plain 0).
fn or_zero(v: Option<&Value>) -> Value {
    if let Some(Value::Number(n)) = v {
        if let Some(f) = n.as_f64() {
            if f != 0.0 && f.is_finite() {
                return Value::Number(n.clone());
            }
        }
    }
    Value::from(0)
}

/// get_row_map's WriteError inside the DELETE transaction closure: Sql
/// variants keep their cause; Bind is unreachable here (all binds scalar).
fn werr(e: crate::collection_writes::WriteError) -> rusqlite::Error {
    match e {
        crate::collection_writes::WriteError::Sql(x) => x,
        crate::collection_writes::WriteError::Bind => rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
            Some("bind".into()),
        ),
    }
}

fn fail(context: &str, status: StatusCode, msg: &str) -> Response {
    tracing::error!("admin_users {context}");
    json_error(status, msg)
}

/// Node: authenticateToken → (invalidateOnWrite never runs for admin:
/// server.js mounts '/api/admin' above the /api gate) → requireAdmin.
/// `is_get` false: API-key writes are rejected by auth itself (403
/// read-only) BEFORE any bump. The bump below is Rust-side cache hygiene
/// only — stats bodies depend solely on (user, data_revision), none of
/// these four routes touch the requester's collection, so it can never
/// change a byte parity compares.
async fn authorize(
    st: &AppState,
    pool: &DbPool,
    headers: &HeaderMap,
    method: &Method,
    is_get: bool,
    path: &str,
) -> Result<auth::AuthUser, Response> {
    let headers = headers.clone();
    let method = method.clone();
    let user = authenticate(pool, &headers, is_get)?;
    if !is_get {
        crate::stats::on_proxied_write(st, &method, path, &headers);
    }
    if let Some(res) = admin_gate(&user) {
        return Err(res);
    }
    Ok(user)
}

// ---------------------------------------------------------------------------
// GET /api/admin/users — admin.js:122
// ---------------------------------------------------------------------------

const USERS_SQL: &str = "SELECT id, username, role, share_enabled, created_at FROM users ORDER BY username ASC";
/// Verbatim from admin.js (only whitespace normalised): the CASE ladder
/// zeroes proxies, prefers the printing-matched price when > 0, else falls
/// through to price_trend. Same SQL both sides ⇒ same doubles.
const STATS_SQL: &str = "SELECT COUNT(c.id) as unique_cards, SUM(c.quantity) as total_cards,\n          SUM(c.quantity * CASE\n            WHEN COALESCE(c.is_proxy, 0) = 1 THEN 0\n            WHEN c.printing = 'Holofoil' AND cc.price_holofoil IS NOT NULL AND cc.price_holofoil > 0 THEN cc.price_holofoil\n            WHEN c.printing = 'Normal' AND cc.price_normal IS NOT NULL AND cc.price_normal > 0 THEN cc.price_normal\n            ELSE cc.price_trend\n          END) as total_value\n        FROM collection c\n        JOIN card_cache cc ON c.card_id = cc.id\n        WHERE c.user_id = ?";

async fn handle_get(State(st): State<AppState>, req: Request) -> Response {
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers = req.headers().clone();
    let method = req.method().clone();
    drop(req);
    let user = match authorize(&st, &pool, &headers, &method, true, "/api/admin/users").await {
        Ok(u) => u,
        Err(res) => return res,
    };
    let _ = &user; // GET does not branch on identity beyond the gate
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "admin_users GET pool");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve users list");
        }
    };
    let users = match rows_map(&conn, USERS_SQL, &[]) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "admin_users GET users query");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve users list");
        }
    };
    let mut out: Vec<Value> = Vec::with_capacity(users.len());
    for u in users {
        let uid = match u.get("id") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            other => js_to_string(other).parse::<i64>().unwrap_or(0),
        };
        let stats = match get_row_map(&conn, STATS_SQL, &[SqlValue::Integer(uid)]) {
            Ok(Some(s)) => s,
            Ok(None) => Map::new(), // COUNT always returns a row; defensive like `stats.x || 0`
            Err(e) => {
                tracing::error!(error = ?e, "admin_users GET stats query");
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve users list");
            }
        };
        let mut m = u; // SELECT order: id, username, role, share_enabled, created_at
        m.insert("total_cards".into(), or_zero(stats.get("total_cards")));
        m.insert("unique_cards".into(), or_zero(stats.get("unique_cards")));
        m.insert(
            "total_value".into(),
            match stats.get("total_value") {
                Some(Value::Number(n)) => n.as_f64().map(|f| js_fixed_parsed(f, 2)).unwrap_or(Value::from(0)),
                _ => Value::from(0),
            },
        );
        out.push(Value::Object(m));
    }
    json_response(StatusCode::OK, &Value::Array(out))
}

// ---------------------------------------------------------------------------
// POST /api/admin/users — admin.js:162
// ---------------------------------------------------------------------------

async fn handle_post(State(st): State<AppState>, req: Request) -> Response {

    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    // body-parser BEFORE auth (server.js mount order).
    let (parts, bytes) = crate::collection_writes::read_parts(req).await;
    match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(body) => {
            let headers = parts.headers.clone();
            let method = parts.method.clone();
            let _req2 = rebuild(parts, &bytes);
            let user = match authorize(&st, &pool, &headers, &method, false, "/api/admin/users").await {
                Ok(u) => u,
                Err(res) => return res,
            };
            let _ = &user;

            let username = body.get("username");
            let password = body.get("password");
            if js_falsy(username) || js_falsy(password) {
                return json_error(StatusCode::BAD_REQUEST, "Username and password are required");
            }
            // Non-string username/password: Node throws a TypeError in the
            // async handler (fatal rejection). Documented divergence: a
            // plain 500 keeps the process alive.
            let Some(raw_name) = username.and_then(Value::as_str) else {
                return fail("POST username TypeError (divergence)", StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
            };
            let Some(raw_pass) = password.and_then(Value::as_str) else {
                return fail("POST password TypeError (divergence)", StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
            };
            let clean_username = js_trim(raw_name).to_lowercase();
            if js_len(&clean_username) < 3 {
                return json_error(StatusCode::BAD_REQUEST, "Username must be at least 3 characters");
            }
            if js_len(raw_pass) < 8 {
                return json_error(StatusCode::BAD_REQUEST, "Password must be at least 8 characters");
            }
            // Destructure default `role = 'member'` fires ONLY on undefined;
            // an explicit null stays null → fails the !== 'member' && !==
            // 'admin' pair → 400.
            let role_val = match body.get("role") {
                None => "member",
                Some(Value::String(r)) if r == "member" || r == "admin" => r.as_str(),
                _ => return json_error(StatusCode::BAD_REQUEST, "Invalid role specification"),
            };
            let conn = match pool.get() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "admin_users POST pool");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create user");
                }
            };
            // --- route try { } catch → 'Failed to create user' ---
            match get_row_map(&conn, "SELECT id FROM users WHERE username = ?", &[SqlValue::Text(clean_username.clone())]) {
                Ok(Some(_)) => return json_error(StatusCode::BAD_REQUEST, "Username is already taken"),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = ?e, "admin_users POST lookup");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create user");
                }
            }
            let pass_for_hash = raw_pass.to_string();
            let hashed = match tokio::task::spawn_blocking(move || hash_password(&pass_for_hash)).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(error = %e, "admin_users POST hash");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create user");
                }
            };
            let share_token = random_hex(16);
            match conn.execute(
                "INSERT INTO users (username, password_hash, role, share_token, share_enabled) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![clean_username, hashed, role_val, share_token, 0],
            ) {
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(error = %e, "admin_users POST insert");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create user");
                }
            }
            json_response(
                StatusCode::CREATED,
                &Value::Object(Map::from_iter([(
                    "message".to_string(),
                    Value::String(format!("User \"{}\" created successfully.", clean_username)),
                )])),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// PUT /api/admin/users/:id — admin.js:201
// ---------------------------------------------------------------------------

async fn handle_put(State(st): State<AppState>, id: String, req: Request) -> Response {
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = crate::collection_writes::read_parts(req).await;
    match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(body) => {
            let headers = parts.headers.clone();
            let method = parts.method.clone();
            let _req2 = rebuild(parts, &bytes);
            let user = match authorize(&st, &pool, &headers, &method, false, "/api/admin/users/:id").await {
                Ok(u) => u,
                Err(res) => return res,
            };
            let conn = match pool.get() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "admin_users PUT pool");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update user");
                }
            };
            // --- route try { } catch → 'Failed to update user' ---
            let target = match get_row_map(
                &conn,
                "SELECT id, username, role FROM users WHERE id = ?",
                &[SqlValue::Text(id.clone())],
            ) {
                Ok(Some(t)) => t,
                Ok(None) => return json_error(StatusCode::NOT_FOUND, "User not found"),
                Err(e) => {
                    tracing::error!(error = ?e, "admin_users PUT lookup");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update user");
                }
            };
            let _ = &target; // Node reads it only for the 404 gate
            // password !== undefined gate: an explicit null RUNS the arm.
            if let Some(pw) = body.get("password") {
                let Some(pw) = pw.as_str() else {
                    // null.length TypeError / number→pbkdf2Sync throw: both
                    // land in Node's route catch.
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update user");
                };
                if js_len(pw) < 8 {
                    return json_error(StatusCode::BAD_REQUEST, "Password must be at least 8 characters");
                }
                let pw_owned = pw.to_string();
                let hashed = match tokio::task::spawn_blocking(move || hash_password(&pw_owned)).await {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!(error = %e, "admin_users PUT hash");
                        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update user");
                    }
                };
                if let Err(e) = conn.execute(
                    "UPDATE users SET password_hash = ? WHERE id = ?",
                    rusqlite::params![hashed, id],
                ) {
                    tracing::error!(error = %e, "admin_users PUT password");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update user");
                }
            }
            if body.get("role").is_some() {
                let role = match body.get("role") {
                    Some(Value::String(r)) if r == "member" || r == "admin" => r.as_str(),
                    _ => return json_error(StatusCode::BAD_REQUEST, "Invalid role"),
                };
                if js_parse_int_v(Some(&Value::String(id.clone()))) == Some(user.id) && role != "admin" {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "You cannot demote yourself from Administrator role.",
                    );
                }
                if let Err(e) = conn.execute(
                    "UPDATE users SET role = ? WHERE id = ?",
                    rusqlite::params![role, id],
                ) {
                    tracing::error!(error = %e, "admin_users PUT role");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update user");
                }
            }
            json_response(
                StatusCode::OK,
                &Value::Object(Map::from_iter([(
                    "message".to_string(),
                    Value::String("User updated successfully".to_string()),
                )])),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/admin/users/:id — admin.js:238
// ---------------------------------------------------------------------------

async fn handle_delete(State(st): State<AppState>, id: String, req: Request) -> Response {
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers = req.headers().clone();
    let method = req.method().clone();
    drop(req);
    let user = match authorize(&st, &pool, &headers, &method, false, "/api/admin/users/:id").await {
        Ok(u) => u,
        Err(res) => return res,
    };
    // parseInt(id, 10) === req.user.id — prefix parse, outside the try.
    if js_parse_int_v(Some(&Value::String(id.clone()))) == Some(user.id) {
        return json_error(StatusCode::BAD_REQUEST, "You cannot delete your own Administrator account.");
    }
    const FAILED: &str = "Failed to delete user";
    let _guard = alloc_lock().lock().await;
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "admin_users DELETE pool");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
        }
    };
    // withDedicatedSqliteTransaction twin: fresh client with FK ON (db.js:154)
    // ⇒ deleting the user CASCADES their decks/deck_cards (the sessions and
    // collection deletes Node runs explicitly are then double-safety). The
    // pragma is restored before the pooled connection returns to the pool.
    let restore = || {
        let _ = conn.execute_batch("PRAGMA foreign_keys = OFF;");
    };
    if conn.execute_batch("PRAGMA foreign_keys = ON;").is_err() {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
    }
    let result = (|| -> Result<String, rusqlite::Error> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let rollback = || {
            let _ = conn.execute_batch("ROLLBACK");
        };
        let target = match get_row_map(
            &conn,
            "SELECT id, username FROM users WHERE id = ?",
            &[SqlValue::Text(id.clone())],
        ) {
            Ok(Some(t)) => t,
            Ok(None) => {
                rollback();
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                    Some("404".into()),
                ));
            }
            Err(e) => {
                rollback();
                return Err(werr(e));
            }
        };
        let username = target.get("username").map(|v| js_to_string(Some(v))).unwrap_or_default();
        let checked = match get_row_map(
            &conn,
            "SELECT COUNT(*) AS count FROM decks WHERE user_id = ? AND checked_out = 1",
            &[SqlValue::Text(id.clone())],
        ) {
            Ok(Some(c)) => c,
            Ok(None) => Map::new(),
            Err(e) => {
                rollback();
                return Err(werr(e));
            }
        };
        let checked_count = checked.get("count").and_then(Value::as_i64).unwrap_or(0);
        if checked_count > 0 {
            rollback();
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                Some("409".into()),
            ));
        }
        conn.execute("DELETE FROM sessions WHERE user_id = ?", rusqlite::params![id])?;
        conn.execute("DELETE FROM collection WHERE user_id = ?", rusqlite::params![id])?;
        let deleted = conn.execute("DELETE FROM users WHERE id = ?", rusqlite::params![id])?;
        if deleted != 1 {
            rollback();
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                Some("vanished".into()),
            ));
        }
        conn.execute_batch("COMMIT")?;
        Ok(username)
    })();
    restore();
    match result {
        Ok(username) => json_response(
            StatusCode::OK,
            &Value::Object(Map::from_iter([(
                "message".to_string(),
                Value::String(format!(
                    "User \"{}\" and all their card collections have been permanently deleted.",
                    username
                )),
            )])),
        ),
        Err(e) => match e {
            rusqlite::Error::SqliteFailure(_, Some(tag)) if tag == "404" => {
                json_error(StatusCode::NOT_FOUND, "User not found")
            }
            rusqlite::Error::SqliteFailure(_, Some(tag)) if tag == "409" => json_error(
                StatusCode::CONFLICT,
                "Check in all of this user's decks before deleting their account.",
            ),
            other => {
                tracing::error!(error = %other, "admin_users DELETE tx");
                json_error(StatusCode::INTERNAL_SERVER_ERROR, FAILED)
            }
        },
    }
}
