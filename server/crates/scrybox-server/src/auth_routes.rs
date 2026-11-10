//! Native port of `backend/src/routes/auth.js` (+ middleware/auth.js
//! authLimiter / authenticateToken, utils/authHelpers.js, utils/oidc.js).
//! Mounted under the `auth` name in RUST_ROUTES; unmatched methods/paths
//! under /api/auth proxy to Node so its 404 pages stay canonical.
//!
//! Every route was written against the actual routes/auth.js source and
//! probed against :3312 (iter 20):
//! - every JSON response carries the Express default weak body ETag
//!   (`collection::body_etag`) + Content-Type `application/json; charset=utf-8`;
//! - the GLOBAL express.json (1 mb, strict) runs before every route, so an
//!   unparseable `application/json` body answers the generic error
//!   handler's 500 `{"error":"Internal server error"}` (body-parser's own
//!   400 is not special-cased by server.js) and an oversized one answers
//!   the 413 `Upload too large...` — both reproduced here, on unknown
//!   sub-paths too, because the global parser runs before the router;
//! - authLimiter (20/15min, shared bucket per IP) covers ONLY
//!   login/register/bootstrap/oidc-login/oidc-callback — api-key and
//!   settings have no limiter in Node;
//! - helmet/CORS headers are NOT emitted here, matching the already-merged
//!   collection/search/stats ports — deferred "static/TLS/headers" group.
//! - this is the FIRST Rust code that writes the shared DB (sessions/users/
//!   settings). WAL + busy_timeout serialise against Node the same way two
//!   Node writers would.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use serde_json::{json, Map, Value};

use crate::auth::{authenticate, bearer_token, AuthUser};
use crate::auth_support::*;
use crate::collection::body_etag;
use crate::db::DbPool;
use crate::ip_limit::client_ip_key;
use crate::oidc;
use crate::{proxy, AppState};

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

fn json_response(status: StatusCode, value: &Value) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    (
        status,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8")),
            (header::ETAG, HeaderValue::from_str(&body_etag(&body)).unwrap()),
        ],
        Body::from(body),
    )
        .into_response()
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    json_response(status, &json!({ "error": msg }))
}

fn redirect(status: StatusCode, location: &str) -> Response {
    (
        status,
        [(
            header::LOCATION,
            HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/")),
        )],
    )
        .into_response()
}

/// JS truthiness for JSON values (Node's `password.length < 8`,
/// `share_enabled ? 1 : 0`, `regenerate_share_token`, `!current_password`
/// all test JS truthiness of whatever JSON arrived).
fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

// ---------------------------------------------------------------------------
// Body parsing (global express.json 1 mb + strict, mirroring Node's ladder)
// ---------------------------------------------------------------------------

enum BodyOutcome {
    Parsed(Value),
    /// entity.parse.failed (malformed / non-object JSON) → global 500.
    Invalid,
    /// entity.too.large → 413 with the export/import message.
    TooLarge,
}

fn is_json_ct(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            main == "application/json"
        })
        .unwrap_or(false)
}

async fn read_body(headers: &HeaderMap, body: Body, replay: bool) -> BodyOutcome {
    if !is_json_ct(headers) {
        return BodyOutcome::Parsed(Value::Object(Map::new()));
    }
    let collected = http_body_util::Limited::new(body, 1024 * 1024).collect().await;
    let bytes = match collected {
        Ok(c) => c.to_bytes(),
        Err(_) => return BodyOutcome::TooLarge,
    };
    if bytes.is_empty() {
        return BodyOutcome::Parsed(Value::Object(Map::new()));
    }
    let parsed = match serde_json::from_slice::<Value>(&bytes) {
        // express.json({strict:true}): only objects/arrays are legal
        // top-level values; anything else is entity.parse.failed → the
        // global 500. An array destructures to an empty body object.
        Ok(v) if v.is_object() => v,
        Ok(v) if v.is_array() => Value::Object(Map::new()),
        _ => return BodyOutcome::Invalid,
    };
    let _ = replay;
    BodyOutcome::Parsed(parsed)
}

fn body_str<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key).and_then(Value::as_str)
}

// ---------------------------------------------------------------------------
// Session helpers (utils/authHelpers.js)
// ---------------------------------------------------------------------------

/// `new Date(+7d).toISOString().slice(0,19).replace('T',' ')` — UTC civil
/// time formatted YYYY-MM-DD HH:MM:SS. (pub for parity tests.)
pub fn session_expiry_str(now_ms: u64) -> String {
    let secs = now_ms / 1000 + 7 * 24 * 3600;
    let days = secs.div_euclid(86_400) as i64;
    let rem = secs.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `generateSession`: 32 random bytes hex, 7-day expiry, INSERT.
fn generate_session(pool: &DbPool, user_id: i64) -> Result<String, rusqlite::Error> {
    let token = random_hex(32);
    let expires = session_expiry_str(now_ms());
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO sessions (token, user_id, expires_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![token, user_id, expires],
    )?;
    Ok(token)
}

fn user_count(pool: &DbPool) -> Result<i64, rusqlite::Error> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare_cached("SELECT COUNT(*) count FROM users")?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(r) => Ok(r.get::<_, i64>(0)?),
        None => Ok(0),
    }
}

/// Node reads ALLOW_REGISTRATION once at module load; env vars are fixed
/// per process in production, and tests need per-test control, so this
/// reads live — same observable behaviour in every real deployment.
fn registration_enabled() -> bool {
    std::env::var("ALLOW_REGISTRATION").map(|v| v == "true").unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Router entry
// ---------------------------------------------------------------------------

const KNOWN: &[(&str, &str)] = &[
    ("GET", "/api/auth/config"),
    ("GET", "/api/auth/me"),
    ("POST", "/api/auth/register"),
    ("POST", "/api/auth/bootstrap"),
    ("POST", "/api/auth/login"),
    ("POST", "/api/auth/logout"),
    ("POST", "/api/auth/api-key"),
    ("DELETE", "/api/auth/api-key"),
    ("PUT", "/api/auth/settings"),
    ("GET", "/api/auth/oidc/login"),
    ("GET", "/api/auth/oidc/callback"),
];

/// The single `authLimiter` instance in middleware/auth.js is mounted on
/// exactly these routes (NOT api-key/settings — verified against source).
const LIMITED: &[(&str, &str)] = &[
    ("POST", "/api/auth/login"),
    ("POST", "/api/auth/register"),
    ("POST", "/api/auth/bootstrap"),
    ("GET", "/api/auth/oidc/login"),
    ("GET", "/api/auth/oidc/callback"),
];

pub async fn handle(
    method: Method,
    uri: Uri,
    State(st): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Body,
) -> Response {
    let full_path = uri.path().to_string();
    let raw_query = uri.query().map(|s| s.to_string());
    // Express 4 (strict:false) matches '/login' for '/login/' too.
    let path = full_path
        .strip_suffix('/')
        .filter(|p| !p.is_empty() && *p != "/")
        .unwrap_or(&full_path)
        .to_string();
    let key = (method.as_str(), path.as_str());
    let known = KNOWN.contains(&key);
    let limited = LIMITED.contains(&key);

    // Global express.json precedes even the limiter and Node's own 404 for
    // unknown sub-paths, so its 413/500 outrank everything below.
    let body_val: Value = match read_body(&headers, body, !known).await {
        BodyOutcome::Parsed(v) => v,
        BodyOutcome::TooLarge => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Upload too large. Try exporting/importing in smaller batches.",
            )
        }
        BodyOutcome::Invalid => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }
    };

    let rebuild = |body: Body| {
        let mut req = axum::http::Request::builder()
            .method(method.clone())
            .uri(uri.clone())
            .body(body)
            .unwrap_or_default();
        *req.headers_mut() = headers.clone();
        req.extensions_mut().insert(ConnectInfo(peer));
        req
    };

    if !known {
        return proxy(State(st.clone()), rebuild(Body::empty())).await;
    }
    let Some(pool) = st.db.clone() else {
        return proxy(State(st.clone()), rebuild(Body::empty())).await;
    };

    let verdict = if limited {
        let ipkey = client_ip_key(Some(peer.ip()), &headers, &st.trust_proxy);
        Some(st.auth_limiter.hit(&ipkey, now_ms()))
    } else {
        None
    };
    let add_limit_headers = |res: Response| -> Response {
        match &verdict {
            Some(v) => {
                let mut res = res;
                let h = res.headers_mut();
                h.insert("ratelimit-policy", HeaderValue::from_static("20;w=900"));
                h.insert("ratelimit-limit", HeaderValue::from_static("20"));
                h.insert(
                    "ratelimit-remaining",
                    HeaderValue::from_str(&v.remaining.to_string()).unwrap(),
                );
                h.insert(
                    "ratelimit-reset",
                    HeaderValue::from_str(&v.reset_secs.to_string()).unwrap(),
                );
                if v.blocked {
                    h.insert(
                        "retry-after",
                        HeaderValue::from_str(&v.reset_secs.to_string()).unwrap(),
                    );
                }
                res
            }
            None => res,
        }
    };

    if let Some(v) = &verdict {
        if v.blocked {
            return add_limit_headers(json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts. Please try again later.",
            ));
        }
    }

    let res = match key {
        ("GET", "/api/auth/config") => handle_config(&pool),
        ("GET", "/api/auth/me") => handle_me(&pool, &headers),
        ("POST", "/api/auth/register") => handle_register(&pool, &body_val).await,
        ("POST", "/api/auth/bootstrap") => handle_bootstrap(&pool, &body_val).await,
        ("POST", "/api/auth/login") => handle_login(&pool, &body_val).await,
        ("POST", "/api/auth/logout") => handle_logout(&pool, &headers),
        ("POST", "/api/auth/api-key") => handle_api_key_post(&pool, &headers),
        ("DELETE", "/api/auth/api-key") => handle_api_key_delete(&pool, &headers),
        ("PUT", "/api/auth/settings") => handle_settings(&pool, &headers, &body_val).await,
        ("GET", "/api/auth/oidc/login") => handle_oidc_login(&headers).await,
        ("GET", "/api/auth/oidc/callback") => {
            handle_oidc_callback(&pool, &headers, raw_query.as_deref()).await
        }
        _ => unreachable!("known route table"),
    };
    let res = add_limit_headers(res);
    apply_conditional_get(&method, &headers, res)
}

/// Express's res.send conditional-GET: on GET/HEAD 2xx with a matching
/// If-None-Match, answer 304 with the same headers and no body.
fn apply_conditional_get(method: &Method, headers: &HeaderMap, res: Response) -> Response {
    if !(method == Method::GET || method == Method::HEAD) || !res.status().is_success() {
        return res;
    }
    let Some(etag) = res.headers().get(header::ETAG).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    else {
        return res;
    };
    let inm = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    if !crate::collection::etag_matches(inm, &etag) {
        return res;
    }
    let mut out = Response::new(Body::empty());
    *out.status_mut() = StatusCode::NOT_MODIFIED;
    for (k, v) in res.headers() {
        if k == header::CONTENT_LENGTH {
            continue;
        }
        out.headers_mut().insert(k.clone(), v.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Handlers (byte-exact to routes/auth.js — error strings included)
// ---------------------------------------------------------------------------

fn handle_config(pool: &DbPool) -> Response {
    // Node catches the count error and keeps setupRequired=false.
    let setup_required = user_count(pool).map(|c| c == 0).unwrap_or_else(|e| {
        tracing::error!(error = %e, "auth config: user count failed");
        false
    });
    json_response(
        StatusCode::OK,
        &json!({
            "registrationEnabled": registration_enabled(),
            "setupRequired": setup_required,
            "oidcEnabled": oidc::is_oidc_enabled(),
            "oidcProviderName": oidc::get_provider_name(),
        }),
    )
}

/// `res.json({ user: req.user })` — the middleware's req.user object,
/// exact key order: id, username, role, share_token, share_enabled,
/// api_key (|| ''), oidc_sub (|| null), via_api_key.
fn handle_me(pool: &DbPool, headers: &HeaderMap) -> Response {
    let token = bearer_token(headers).map(|s| s.to_string());
    match authenticate(pool, token.as_deref(), true) {
        Ok(u) => json_response(
            StatusCode::OK,
            &json!({
                "user": {
                    "id": u.id,
                    "username": u.username,
                    "role": u.role,
                    "share_token": u.share_token,
                    "share_enabled": u.share_enabled,
                    "api_key": u.api_key.unwrap_or_default(),
                    "oidc_sub": u.oidc_sub,
                    "via_api_key": u.via_api_key,
                }
            }),
        ),
        Err(res) => res,
    }
}

async fn handle_register(pool: &DbPool, body: &Value) -> Response {
    if !registration_enabled() {
        return json_error(
            StatusCode::FORBIDDEN,
            "Registration is disabled. Ask an administrator for an account.",
        );
    }
    let username = body_str(body, "username");
    let password = body_str(body, "password");
    let (Some(u), Some(p)) = (username.filter(|s| !s.is_empty()), password.filter(|s| !s.is_empty()))
    else {
        return json_error(StatusCode::BAD_REQUEST, "Username and password are required");
    };
    let clean = u.trim().to_lowercase();
    if clean.chars().count() < 3 {
        return json_error(StatusCode::BAD_REQUEST, "Username must be at least 3 characters");
    }
    if p.chars().count() < 8 {
        return json_error(StatusCode::BAD_REQUEST, "Password must be at least 8 characters");
    }
    let existing = (|| -> Result<bool, rusqlite::Error> {
        let conn = pool.get()?;
        let mut stmt = conn.prepare_cached("SELECT id FROM users WHERE username = ?1")?;
        Ok(stmt.query_row([&clean], |_| Ok(())).is_ok())
    })();
    match existing {
        Ok(true) => {
            return json_error(StatusCode::BAD_REQUEST, "Username is already taken")
        }
        Err(e) => {
            tracing::error!(error = %e, "auth register: lookup failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to register");
        }
        _ => {}
    }
    let hash = match tokio::task::spawn_blocking({
        let p = p.to_string();
        move || hash_password(&p)
    })
    .await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "auth register: hash failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to register");
        }
    };
    let share_token = random_hex(16);
    let insert = (|| -> Result<i64, rusqlite::Error> {
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO users (username, password_hash, role, share_token, share_enabled) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![clean, hash, "member", share_token, 0],
        )?;
        Ok(conn.last_insert_rowid())
    })();
    let id = match insert {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "auth register: insert failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to register");
        }
    };
    let token = match generate_session(pool, id) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "auth register: session failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to register");
        }
    };
    json_response(
        StatusCode::CREATED,
        &json!({
            "message": "Registration successful",
            "token": token,
            "user": {
                "username": clean,
                "role": "member",
                "share_token": share_token,
                "share_enabled": 0,
            }
        }),
    )
}

async fn handle_bootstrap(pool: &DbPool, body: &Value) -> Response {
    // Open by necessity until the first account exists (auth.js:167-176).
    let password = body_str(body, "password").filter(|s| !s.is_empty());
    let Some(p) = password else {
        return json_error(StatusCode::BAD_REQUEST, "A password is required");
    };
    if p.chars().count() < 8 {
        return json_error(StatusCode::BAD_REQUEST, "Password must be at least 8 characters");
    }
    match user_count(pool) {
        Ok(0) => {}
        Ok(_) => {
            return json_error(
                StatusCode::CONFLICT,
                "This instance is already set up. Log in instead.",
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "auth bootstrap: count failed");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create the owner account",
            );
        }
    }
    let hash = match tokio::task::spawn_blocking({
        let p = p.to_string();
        move || hash_password(&p)
    })
    .await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "auth bootstrap: hash failed");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create the owner account",
            );
        }
    };
    let share_token = random_hex(16);
    let insert = (|| -> Result<i64, rusqlite::Error> {
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO users (username, password_hash, role, share_token, share_enabled) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["admin", hash, "admin", share_token, 0],
        )?;
        Ok(conn.last_insert_rowid())
    })();
    let id = match insert {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "auth bootstrap: insert failed");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create the owner account",
            );
        }
    };
    // Orphan rows skipped by initDb, which found no account to hand them to.
    if let Err(e) = pool.get().and_then(|c| {
        c.execute(
            "UPDATE collection SET user_id = ?1 WHERE user_id IS NULL",
            rusqlite::params![id],
        )?;
        Ok(())
    }) {
        tracing::error!(error = %e, "auth bootstrap: adopt failed");
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create the owner account",
        );
    }
    let token = match generate_session(pool, id) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "auth bootstrap: session failed");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create the owner account",
            );
        }
    };
    json_response(
        StatusCode::CREATED,
        &json!({
            "message": "Setup complete",
            "token": token,
            "user": {
                "username": "admin",
                "role": "admin",
                "share_token": share_token,
                "share_enabled": 0,
                "api_key": "",
            }
        }),
    )
}

async fn handle_login(pool: &DbPool, body: &Value) -> Response {
    let username = body_str(body, "username");
    let password = body_str(body, "password");
    let (Some(u), Some(p)) = (username.filter(|s| !s.is_empty()), password.filter(|s| !s.is_empty()))
    else {
        return json_error(StatusCode::BAD_REQUEST, "Username and password are required");
    };
    let clean = u.trim().to_lowercase();
    let row = (|| -> Result<Option<(i64, String, String, Option<String>, Option<String>, Option<i64>, Option<String>)>, rusqlite::Error> {
        let conn = pool.get()?;
        let mut stmt = conn.prepare_cached(
            "SELECT id, username, role, password_hash, share_token, share_enabled, api_key FROM users WHERE username = ?1",
        )?;
        let mut rows = stmt.query([clean.as_str()])?;
        Ok(rows.next()?.map(|r| {
            (
                r.get(0).unwrap(),
                r.get(1).unwrap(),
                r.get(2).unwrap(),
                r.get(3).ok().flatten(),
                r.get(4).ok().flatten(),
                r.get(5).ok().flatten(),
                r.get(6).ok().flatten(),
            )
        }))
    })();
    let fail_401 = || json_error(StatusCode::UNAUTHORIZED, "Invalid username or password");
    let err_500 = |ctx: &str, e: &dyn std::fmt::Display| {
        tracing::error!(error = %e, ctx = %ctx, "auth login");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, "Login failed")
    };
    let row = match row {
        Ok(r) => r,
        Err(e) => return err_500("query", &e),
    };
    let Some((id, uname, role, stored_hash, share_token, share_enabled, api_key)) = row else {
        return fail_401();
    };
    let ok = tokio::task::spawn_blocking({
        let p = p.to_string();
        let h = stored_hash.clone().unwrap_or_default();
        move || verify_password(&p, &h)
    })
    .await
    .unwrap_or(false);
    if !ok {
        return fail_401();
    }
    let _ = id;
    let token = match generate_session(pool, id) {
        Ok(t) => t,
        Err(e) => return err_500("session", &e),
    };
    json_response(
        StatusCode::OK,
        &json!({
            "message": "Login successful",
            "token": token,
            "user": {
                "username": uname,
                "role": role,
                "share_token": share_token,
                "share_enabled": share_enabled,
                "api_key": api_key.unwrap_or_default(),
            }
        }),
    )
}

/// POST /logout runs authenticateToken FIRST (401 without a token) and
/// deletes the session token from the Authorization header (not the body).
fn handle_logout(pool: &DbPool, headers: &HeaderMap) -> Response {
    let token = bearer_token(headers).map(|s| s.to_string());
    // authenticateToken: any method reaches the handler with is_get=false
    // for logout (POST) — API-key bearer gets the read-only 403 first.
    if let Err(res) = authenticate(pool, token.as_deref(), false) {
        return res;
    }
    let r = pool.get().and_then(|c| {
        c.execute("DELETE FROM sessions WHERE token = ?1", rusqlite::params![token])?;
        Ok(())
    });
    match r {
        Ok(_) => json_response(StatusCode::OK, &json!({ "message": "Logged out successfully" })),
        Err(e) => {
            tracing::error!(error = %e, "auth logout");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Logout failed")
        }
    }
}

fn handle_api_key_post(pool: &DbPool, headers: &HeaderMap) -> Response {
    let token = bearer_token(headers).map(|s| s.to_string());
    let user = match authenticate(pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let key = format!("bnd_{}", random_hex(24));
    let r = pool.get().and_then(|c| {
        c.execute(
            "UPDATE users SET api_key = ?1 WHERE id = ?2",
            rusqlite::params![key, user.id],
        )?;
        Ok(())
    });
    match r {
        Ok(_) => json_response(StatusCode::OK, &json!({ "api_key": key })),
        Err(e) => {
            tracing::error!(error = %e, "auth api-key post");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create API key")
        }
    }
}

fn handle_api_key_delete(pool: &DbPool, headers: &HeaderMap) -> Response {
    let token = bearer_token(headers).map(|s| s.to_string());
    let user = match authenticate(pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let r = pool.get().and_then(|c| {
        c.execute("UPDATE users SET api_key = NULL WHERE id = ?1", rusqlite::params![user.id])?;
        Ok(())
    });
    match r {
        Ok(_) => json_response(StatusCode::OK, &json!({ "message": "API key revoked" })),
        Err(e) => {
            tracing::error!(error = %e, "auth api-key delete");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to revoke API key")
        }
    }
}

async fn handle_settings(pool: &DbPool, headers: &HeaderMap, body: &Value) -> Response {
    let token = bearer_token(headers).map(|s| s.to_string());
    let user = match authenticate(pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let err_500 = |ctx: &str, e: &dyn std::fmt::Display| {
        tracing::error!(error = %e, ctx = %ctx, "auth settings");
        json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update settings")
    };
    if body.get("password").is_some() {
        let newpw = body_str(body, "password");
        let too_short = match newpw {
            None => true, // JS undefined.length throws → catch → 500
            Some(p) => p.chars().count() < 8,
        };
        if too_short {
            return json_error(
                StatusCode::BAD_REQUEST,
                "Password must be at least 8 characters",
            );
        }
        let cur = (|| -> Result<Option<String>, rusqlite::Error> {
            let conn = pool.get()?;
            let mut stmt = conn.prepare_cached("SELECT password_hash FROM users WHERE id = ?1")?;
            let mut rows = stmt.query([user.id])?;
            Ok(rows.next()?.map(|r| r.get::<_, Option<String>>(0).ok().flatten().unwrap_or_default()))
        })();
        let cur = match cur {
            Ok(Some(c)) => c,
            Ok(None) => return err_500("no-user", &"user row vanished"),
            Err(e) => return err_500("query", &e),
        };
        let current_password = body_str(body, "current_password");
        let ok = match current_password {
            None | Some("") => false,
            Some(cp) => {
                tokio::task::spawn_blocking({
                    let cp = cp.to_string();
                    let c = cur.clone();
                    move || verify_password(&cp, &c)
                })
                .await
                .unwrap_or(false)
            }
        };
        if !ok {
            return json_error(StatusCode::UNAUTHORIZED, "Current password is incorrect");
        }
        let hash = match tokio::task::spawn_blocking({
            let p = newpw.unwrap().to_string();
            move || hash_password(&p)
        })
        .await {
            Ok(h) => h,
            Err(e) => return err_500("hash", &e),
        };
        if let Err(e) = pool.get().and_then(|c| {
            c.execute(
                "UPDATE users SET password_hash = ?1 WHERE id = ?2",
                rusqlite::params![hash, user.id],
            )?;
            Ok(())
        }) {
            return err_500("update", &e);
        }
    }

    if body.get("share_enabled").is_some() {
        let on = if js_truthy(body.get("share_enabled")) { 1 } else { 0 };
        if let Err(e) = pool.get().and_then(|c| {
            c.execute(
                "UPDATE users SET share_enabled = ?1 WHERE id = ?2",
                rusqlite::params![on, user.id],
            )?;
            Ok(())
        }) {
            return err_500("share", &e);
        }
    }

    if js_truthy(body.get("regenerate_share_token")) {
        let newtok = random_hex(16);
        if let Err(e) = pool.get().and_then(|c| {
            c.execute(
                "UPDATE users SET share_token = ?1 WHERE id = ?2",
                rusqlite::params![newtok, user.id],
            )?;
            Ok(())
        }) {
            return err_500("regen", &e);
        }
    }

    let updated = (|| -> Result<Option<(String, String, Option<String>, Option<i64>, Option<String>)>, rusqlite::Error> {
        let conn = pool.get()?;
        let mut stmt = conn.prepare_cached(
            "SELECT username, role, share_token, share_enabled, api_key FROM users WHERE id = ?1",
        )?;
        let mut rows = stmt.query([user.id])?;
        Ok(rows.next()?.map(|r| {
            (
                r.get(0).unwrap(),
                r.get(1).unwrap(),
                r.get(2).ok().flatten(),
                r.get(3).ok().flatten(),
                r.get(4).ok().flatten(),
            )
        }))
    })();
    match updated {
        Ok(Some((username, role, share_token, share_enabled, api_key))) => json_response(
            StatusCode::OK,
            &json!({
                "message": "Settings updated successfully",
                "user": {
                    "username": username,
                    "role": role,
                    "share_token": share_token,
                    "share_enabled": share_enabled,
                    "api_key": api_key.unwrap_or_default(),
                }
            }),
        ),
        Ok(None) => err_500("gone", &"user row vanished"),
        Err(e) => err_500("final", &e),
    }
}

// ---------------------------------------------------------------------------
// OIDC routes
// ---------------------------------------------------------------------------

async fn handle_oidc_login(headers: &HeaderMap) -> Response {
    if !oidc::is_oidc_enabled() {
        return json_error(StatusCode::NOT_FOUND, "OIDC authentication is not enabled.");
    }
    let proto =
        headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    match oidc::build_authorization_url(proto.as_deref(), host.as_deref()).await {
        Ok(url) => redirect(StatusCode::FOUND, &url),
        Err(e) => {
            tracing::warn!(error = %e, "OIDC login initiation failed");
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "Failed to initiate OIDC login", "message": e }),
            )
        }
    }
}

fn query_str(qs: &[(String, String)], key: &str) -> Option<String> {
    let mut it = qs.iter().filter(|(k, _)| k == key);
    let first = it.next()?.1.clone();
    if it.next().is_some() {
        return None; // qs array → the !code/!state truthiness would pass;
                     // exchange would see "a,b" — unreachable parity edge, treat as missing.
    }
    Some(first)
}

async fn handle_oidc_callback(
    pool: &DbPool,
    headers: &HeaderMap,
    raw_query: Option<&str>,
) -> Response {
    if !oidc::is_oidc_enabled() {
        return json_error(StatusCode::NOT_FOUND, "OIDC authentication is not enabled.");
    }
    let qs: Vec<(String, String)> = raw_query
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default();
    let base = {
        let b = std::env::var("PUBLIC_BASE_URL").unwrap_or_default();
        if b.is_empty() { String::new() } else { trim_trailing_slashes(&b) }
    };
    let frontend_redirect = |params: [(&str, String); 1]| -> Response {
        let qs = params
            .iter()
            .map(|(k, v)| format!("{}={}", form_urlencode(k), form_urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        redirect(StatusCode::FOUND, &format!("{base}/?{qs}"))
    };

    if let Some(err) = query_str(&qs, "error") {
        let desc = query_str(&qs, "error_description");
        tracing::error!("OIDC IdP error: {err} - {}", desc.clone().unwrap_or_default());
        let msg = desc.filter(|d| !d.is_empty()).unwrap_or(err);
        return frontend_redirect([("oidc_error", msg)]);
    }
    let code = match query_str(&qs, "code") {
        Some(c) if !c.is_empty() => c,
        _ => {
            return frontend_redirect([(
                "oidc_error",
                "Missing authorization code or state from identity provider.".to_string(),
            )])
        }
    };
    let state = match query_str(&qs, "state") {
        Some(s) if !s.is_empty() => s,
        _ => {
            return frontend_redirect([(
                "oidc_error",
                "Missing authorization code or state from identity provider.".to_string(),
            )])
        }
    };
    let proto =
        headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let outcome: Result<i64, String> = async {
        let ok = oidc::exchange_code(&code, &state, proto.as_deref(), host.as_deref())
            .await
            .map_err(|e| e)?;
        let (sub, username) = (ok.sub.clone(), ok.username.clone());

        // 1. Match by existing oidc_sub.
        let by_sub = (|| -> Result<Option<(i64, Option<String>)>, rusqlite::Error> {
            let conn = pool.get()?;
            let mut stmt = conn.prepare_cached("SELECT id, oidc_sub FROM users WHERE oidc_sub = ?1")?;
            let mut rows = stmt.query([&sub])?;
            Ok(rows.next()?.map(|r| (r.get(0).unwrap(), r.get(1).ok().flatten())))
        })()
        .map_err(|e| e.to_string())?;
        let mut user_id = by_sub.map(|(id, _)| id);

        // 2. Username link (operator-trusted IdP usernames only).
        if user_id.is_none() {
            let existing = (|| -> Result<Option<(i64, Option<String>)>, rusqlite::Error> {
                let conn = pool.get()?;
                let mut stmt = conn.prepare_cached("SELECT id, oidc_sub FROM users WHERE username = ?1")?;
                let mut rows = stmt.query([&username])?;
                Ok(rows.next()?.map(|r| (r.get(0).unwrap(), r.get(1).ok().flatten())))
            })()
            .map_err(|e| e.to_string())?;
            if let Some((id, bound)) = existing.clone() {
                if !oidc::is_username_link_enabled() {
                    return Err("A Scrybox account with this username already exists. An administrator must set OIDC_ALLOW_USERNAME_LINK=true to attach single sign-on to it.".into());
                }
                if bound.is_some() && bound.as_deref() != Some(sub.as_str()) {
                    return Err("That username is already linked to a different single sign-on identity. Ask an administrator.".into());
                }
                pool.get()
                    .and_then(|c| {
                        c.execute(
                            "UPDATE users SET oidc_sub = ?1 WHERE id = ?2",
                            rusqlite::params![sub, id],
                        )?;
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                user_id = Some(id);
            }
        }

        // 3. Provision (first-run owner bootstrap or auto-provision).
        if user_id.is_none() {
            let count = user_count(pool).map_err(|e| e.to_string())?;
            let locked_hash = format!("oidc_locked:{}", random_hex(16));
            let share_token = random_hex(16);
            let (uname, role) = if count == 0 {
                ("admin".to_string(), "admin")
            } else if oidc::is_auto_provision_enabled() || registration_enabled() {
                let mut final_username = username.clone();
                let mut suffix = 1u32;
                loop {
                    let taken = (|| -> Result<bool, rusqlite::Error> {
                        let conn = pool.get()?;
                        let mut stmt = conn.prepare_cached("SELECT id FROM users WHERE username = ?1")?;
                        Ok(stmt.query_row([&final_username], |_| Ok(())).is_ok())
                    })()
                    .map_err(|e| e.to_string())?;
                    if !taken {
                        break;
                    }
                    final_username = format!("{username}-{suffix}");
                    suffix += 1;
                }
                (final_username, oidc::get_default_role())
            } else {
                return Err("No matching Scrybox account found. Auto-provisioning is disabled; ask an administrator to create your account.".into());
            };
            let id = (|| -> Result<i64, rusqlite::Error> {
                let conn = pool.get()?;
                conn.execute(
                    "INSERT INTO users (username, password_hash, role, share_token, share_enabled, oidc_sub) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![uname, locked_hash, role, share_token, 0, sub],
                )?;
                Ok(conn.last_insert_rowid())
            })()
            .map_err(|e| e.to_string())?;
            if count == 0 {
                pool.get()
                    .and_then(|c| {
                        c.execute(
                            "UPDATE collection SET user_id = ?1 WHERE user_id IS NULL",
                            rusqlite::params![id],
                        )?;
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
            }
            user_id = Some(id);
        }
        Ok(user_id.unwrap())
    }
    .await;

    match outcome {
        Err(msg) => {
            tracing::error!("OIDC callback processing failed: {msg}");
            frontend_redirect([("oidc_error", msg)])
        }
        Ok(uid) => match generate_session(pool, uid) {
            Ok(token) => frontend_redirect([("oidc_token", token)]),
            Err(e) => {
                tracing::error!("OIDC callback session failed: {e}");
                frontend_redirect([("oidc_error", e.to_string())])
            }
        },
    }
}

fn trim_trailing_slashes(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

// keep the AuthUser import meaningful for rustc's unused checker
#[allow(unused_imports)]
use AuthUser as _AuthUserMarker;
