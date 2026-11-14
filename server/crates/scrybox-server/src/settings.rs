//! Port of `backend/src/routes/settings.js` (route group `settings`):
//! GET /api/settings, PUT /api/settings, GET /api/settings/version. All
//! three sit behind the global `app.use('/api', authenticateToken)` mount
//! (server.js:454); the PUT additionally runs requireAdmin in-router.
//!
//! Byte-parity surface (BRIEF rule 1): Express `res.json` body shape and
//! key ORDER. GET/PUT-OK answer `getEffectiveSettings()` — exactly 11 keys
//! in source order (public_base_url, price_refresh_days, scan_exclude_*,
//! setup_complete, moxfield_*_interval_min, scryfall_local,
//! scryfall_portal_enabled). GET /version answers `{version, releases_url}`
//! and, with `?check=1`, spreads the cached update-check object (latest,
//! update_available, release_url, published_at) — or `{check_failed:true}`
//! when the GitHub call throws. The env half reads the SAME env names the
//! Node container reads (PUBLIC_BASE_URL, LOCAL_SCRYFALL_ENABLED,
//! LOCAL_WEB_PORTAL_ENABLED; the two flags are strict `=== 'true'`, no trim
//! or lowercasing) — restart-rust.sh mirrors the container values. The
//! version string mirrors backend/package.json (cargo-pinned).
//!
//! Node behaviours reproduced exactly:
//! - body-parser parses BEFORE authentication (malformed JSON PUT answers
//!   the generic 500 without a token; >1 MiB answers the 413 shape).
//! - PUT gates are `!== undefined` — an explicit null RUNS the arm:
//!   price null → Number(null)=0 writes 0; scan/setup null → falsy → 0.
//! - price gate rejects rather than clamps (whole number, 0..30).
//! - moxfield arms use parseInt(v,10) prefix parsing then [1,1440]; a
//!   rejected price/moxfield/base-url arm returns 400 EARLY, after the
//!   earlier arms already committed (Node runs each db.run as its own
//!   autocommit statement — no transaction here, deliberately).
//! - public_base_url: trim; truthy trimmed must match /^https?:\/\//i;
//!   trailing '/' runs are stripped; an empty-after-trim value WRITES ''.
//!   A non-string value is a TypeError in Node's `.trim()` — OUTSIDE the
//!   route try — so Node dies (async-handler rejection is fatal on Node 22,
//!   the lists.js accent precedent). The Rust twin answers the route-catch
//!   500 instead; such a body cannot exist in a live parity run.
//! - update check caches the BUILT data object for 6 h in module state
//!   (`{at, data}`); failed fetches are not cached and answer
//!   check_failed. Cache is keyed by the (overridable) releases URL so a
//!   test pointing at its own mock never shares the entry.
//!
//! `GET /api/settings` on a DB whose app_settings row is MISSING is not an
//! error on Node — every read is `row && ...`-guarded (price lands on 0 via
//! Number(null)). The Rust twin mirrors the no-row fold exactly; only a
//! missing TABLE answers the 500.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Map, Number, Value};

use crate::auth;
use crate::collection::truthy;
use crate::db::DbPool;
use crate::AppState;

/// settings.js RELEASES_API (module const; SCRYBOX_RELEASES_API_URL
/// overrides for tests only).
pub const DEFAULT_RELEASES_API: &str = "https://api.github.com/repos/Brenttime/scrybox/releases/latest";
/// settings.js RELEASES_PAGE — the literal string in every body.
pub const RELEASES_PAGE: &str = "https://github.com/Brenttime/scrybox/releases";
/// settings.js: `const APP_VERSION = require('../../package.json').version`.
pub const DEFAULT_APP_VERSION: &str = "1.8.5";
/// settings.js UPDATE_CACHE_MS.
const UPDATE_CACHE_MS: i64 = 1000 * 60 * 60 * 6;
/// settings.js axios.get timeout.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// express.json({ limit: '1mb' }) — normalJson in server.js.
const BODY_LIMIT: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Environment half of getEffectiveSettings
// ---------------------------------------------------------------------------

/// Node reads `process.env.X` per request; the Rust twin snapshots the SAME
/// variable names at boot (restart-rust.sh mirrors the container values —
/// changing one side's env without the other is a harness fault). `None` =
/// unset (undefined). Tests build the struct directly.
#[derive(Clone, Debug)]
pub struct SettingsEnv {
    pub public_base_url: Option<String>,
    pub local_scryfall_enabled: Option<String>,
    pub local_web_portal_enabled: Option<String>,
    pub app_version: String,
    pub releases_api_url: String,
}

impl SettingsEnv {
    pub fn from_env() -> Self {
        // `process.env.X` is undefined when unset but '' when set-empty —
        // '' is falsy for `||` and fails `=== 'true'`, so both cases agree
        // and Option<String> (None on unset, Some on set) is exact.
        let var = |k: &str| std::env::var(k).ok();
        Self {
            public_base_url: var("PUBLIC_BASE_URL"),
            local_scryfall_enabled: var("LOCAL_SCRYFALL_ENABLED"),
            local_web_portal_enabled: var("LOCAL_WEB_PORTAL_ENABLED"),
            app_version: var("SCRYBOX_APP_VERSION")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_APP_VERSION.to_string()),
            releases_api_url: var("SCRYBOX_RELEASES_API_URL")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_RELEASES_API.to_string()),
        }
    }
}

/// `String(v)` for JSON values (the parseInt/isNewer paths stringify first,
/// exactly like V8 does for `parseInt(v,10)` / `String(v)`).
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// ECMAScript `String.prototype.trim` (WhiteSpace + LineTerminator set).
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `parseInt(String(v),10)` twin: outer-trim, optional sign, longest
/// decimal-digit prefix; no digits => NaN (None). V8 trims leading
/// whitespace before the sign and stops at the first non-digit.
fn parse_int_10(v: &Value) -> Option<f64> {
    let s = js_string(v);
    let t = js_trim(&s);
    let bytes = t.as_bytes();
    let (neg, mut i) = match bytes.first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        _ => (false, 0),
    };
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    t[start..i].parse::<f64>().ok().map(|f| if neg { -f } else { f })
}

/// JS `replace(/^v/i,'')` — strips ONE leading v/V.
fn strip_leading_v(s: &str) -> &str {
    match s.strip_prefix(['v', 'V']) {
        Some(rest) => rest,
        None => s,
    }
}

/// settings.js isNewer — `String(v).replace(/^v/i,'')`, split '.', map
/// `parseInt(n,10)||0`, compare at max length with missing parts as 0.
/// Non-numeric parts collapse to 0 (no rejection).
pub fn is_newer(candidate: &Value, current: &str) -> bool {
    let parts = |v: &str| -> Vec<f64> {
        let s = strip_leading_v(v);
        s.split('.')
            .map(|p| parse_int_10(&Value::String(p.to_string())).unwrap_or(0.0))
            .collect()
    };
    let a = parts(&js_string(candidate));
    let b = parts(current);
    let n = a.len().max(b.len());
    for i in 0..n {
        let av = a.get(i).copied().unwrap_or(0.0);
        let bv = b.get(i).copied().unwrap_or(0.0);
        if av != bv {
            return av > bv;
        }
    }
    false
}

/// Number.isFinite + Number.isInteger on a js_number result.
fn js_is_integer(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0
}

fn json_response(status: StatusCode, map: &Map<String, Value>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(Value::Object(map.clone())),
    )
        .into_response()
}

use crate::util::json_error;

/// The route-catch shape server.js's error middleware answers for a throw
/// inside a ported handler. Node's un-wrapped async arms DIE instead (the
/// lists.js accent precedent); the Rust twin stays up.
fn internal_error(context: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "{context}");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
}

// ---------------------------------------------------------------------------
// updateCache twin (module-level `let updateCache = {at, data}`)
// ---------------------------------------------------------------------------

/// releases-url -> (at_ms, built data object). Node keeps ONE slot; keying
/// by URL is unobservable in production (single URL) and keeps cargo tests
/// from racing over the shared process state.
fn update_cache() -> &'static Mutex<HashMap<String, (i64, Value)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (i64, Value)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

// ---------------------------------------------------------------------------
// getEffectiveSettings
// ---------------------------------------------------------------------------

const EFFECTIVE_SQL: &str = concat!(
    "SELECT public_base_url, price_refresh_days, ",
    "scan_exclude_tokens, scan_exclude_art_cards, scan_exclude_jumpstart, scan_exclude_promos, ",
    "setup_complete, moxfield_decklist_interval_min, moxfield_content_interval_min ",
    "FROM app_settings WHERE id = 1"
);

/// better-sqlite3 value -> serde Value (TEXT/INTEGER/REAL/NULL are the
/// reachable affinities here; a Blob would serialise as a Buffer object on
/// Node, which this schema cannot produce).
fn sql_value(v: rusqlite::types::Value) -> Value {
    use rusqlite::types::Value as RV;
    match v {
        RV::Null => Value::Null,
        RV::Integer(i) => Value::from(i),
        RV::Real(f) => Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
        RV::Text(s) => Value::String(s),
        RV::Blob(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
    }
}

fn get_effective_settings(pool: &DbPool, env: &SettingsEnv) -> Result<Map<String, Value>, String> {
    let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
    let row: Option<Vec<Value>> = conn
        .prepare_cached(EFFECTIVE_SQL)
        .map_err(|e| format!("{e}"))?
        .query_map([], |r| {
            Ok((0..9)
                .map(|i| sql_value(r.get::<_, rusqlite::types::Value>(i).unwrap_or(rusqlite::types::Value::Null)))
                .collect())
        })
        .and_then(|mut rows| rows.next().transpose())
        .map_err(|e| format!("{e}"))?;
    let col = |i: usize| -> Value { row.as_ref().map(|r| r[i].clone()).unwrap_or(Value::Null) };

    let mut map = Map::new();
    // `(row && row.public_base_url) || process.env.PUBLIC_BASE_URL || ''`
    let base = if row.is_some() && truthy(&col(0)) {
        col(0)
    } else {
        match &env.public_base_url {
            Some(s) if !s.is_empty() => Value::String(s.clone()),
            _ => Value::String(String::new()),
        }
    };
    map.insert("public_base_url".into(), base);

    // `Number(row && row.price_refresh_days)` — no-row folds through
    // Number(null)=0; `Number.isInteger(n) && n >= 0 ? n : 1`.
    let n = crate::decks_reads::js_number(Some(&col(1)));
    let days = if js_is_integer(n) && n >= 0.0 { n } else { 1.0 };
    map.insert("price_refresh_days".into(), crate::collection::json_num(days));

    for (key, idx) in [
        ("scan_exclude_tokens", 2usize),
        ("scan_exclude_art_cards", 3),
        ("scan_exclude_jumpstart", 4),
        ("scan_exclude_promos", 5),
        ("setup_complete", 6),
    ] {
        map.insert(key.into(), Value::Bool(row.is_some() && truthy(&col(idx))));
    }
    // `row ? (row.col || 60) : 60` — the raw truthy value passes through.
    for (key, idx, fallback) in [
        ("moxfield_decklist_interval_min", 7usize, 60i64),
        ("moxfield_content_interval_min", 8, 1),
    ] {
        let v = if row.is_some() && truthy(&col(idx)) {
            col(idx)
        } else {
            Value::from(fallback)
        };
        map.insert(key.into(), v);
    }
    map.insert(
        "scryfall_local".into(),
        Value::Bool(env.local_scryfall_enabled.as_deref() == Some("true")),
    );
    map.insert(
        "scryfall_portal_enabled".into(),
        Value::Bool(env.local_web_portal_enabled.as_deref() == Some("true")),
    );
    Ok(map)
}

// ---------------------------------------------------------------------------
// checkForUpdate (GitHub releases, axios twin)
// ---------------------------------------------------------------------------

type HttpsClient =
    Client<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, String>;

fn https_client() -> &'static HttpsClient {
    static CLIENT: OnceLock<HttpsClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("native root certificates")
            .https_or_http()
            .enable_http1()
            .build();
        Client::builder(TokioExecutor::new()).build(https)
    })
}

async fn fetch_releases(url: &str) -> Result<Value, String> {
    let req = hyper::Request::get(url)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "Scrybox")
        .body(String::new())
        .map_err(|e| format!("{url}: {e}"))?;
    let res = tokio::time::timeout(FETCH_TIMEOUT, https_client().request(req))
        .await
        .map_err(|_| format!("{url}: timeout"))?
        .map_err(|e| format!("{url}: {e}"))?;
    let status = res.status().as_u16();
    if !(200..300).contains(&status) {
        // axios rejects non-2xx with this message; the caller answers
        // check_failed either way — the text only reaches the console.
        return Err(format!("Request failed with status code {status}"));
    }
    let body = http_body_util::BodyExt::collect(res.into_body())
        .await
        .map_err(|e| format!("{url}: {e}"))?
        .to_bytes();
    serde_json::from_slice::<Value>(&body).map_err(|e| format!("{url}: invalid JSON response ({e})"))
}

async fn check_for_update(env: &SettingsEnv) -> Result<Value, String> {
    let now = crate::stats::now_ms();
    let hit = update_cache()
        .lock()
        .map_err(|_| "update cache poisoned".to_string())?
        .get(&env.releases_api_url)
        .filter(|(at, _)| now - *at < UPDATE_CACHE_MS)
        .map(|(_, d)| d.clone());
    if let Some(data) = hit {
        return Ok(data);
    }
    let resp = fetch_releases(&env.releases_api_url).await?;
    // `String(resp.data.tag_name || '').replace(/^v/i,'')` — falsy
    // tag_name (missing/null/0/'') => String('') => ''.
    let tag = resp.get("tag_name").cloned().unwrap_or(Value::Null);
    let latest = if truthy(&tag) {
        strip_leading_v(&js_string(&tag)).to_string()
    } else {
        String::new()
    };
    let mut data = Map::new();
    data.insert("latest".into(), Value::String(latest.clone()));
    data.insert(
        "update_available".into(),
        Value::Bool(!latest.is_empty() && is_newer(&Value::String(latest), &env.app_version)),
    );
    let html = resp.get("html_url").cloned().unwrap_or(Value::Null);
    data.insert(
        "release_url".into(),
        if truthy(&html) { html } else { Value::String(RELEASES_PAGE.to_string()) },
    );
    let pub_at = resp.get("published_at").cloned().unwrap_or(Value::Null);
    data.insert("published_at".into(), if truthy(&pub_at) { pub_at } else { Value::Null });
    let data = Value::Object(data);
    update_cache()
        .lock()
        .map_err(|_| "update cache poisoned".to_string())?
        .insert(env.releases_api_url.clone(), (now, data.clone()));
    Ok(data)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET/PUT /api/settings. authenticateToken (via the /api mount) gates both;
/// PUT additionally requireAdmin. Any other method rides the untouched
/// proxy — never 405.
pub async fn route_settings(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("settings") {
        return crate::proxy(State(st), req).await;
    }
    let method = req.method().clone();
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st), req).await;
    };
    match method {
        Method::GET => {
            let env = SettingsEnv::from_env();
            let headers = req.headers().clone();
            if let Err(res) = authenticate(&pool, &headers, true) {
                return res;
            }
            match get_effective_settings(&pool, &env) {
                Ok(map) => json_response(StatusCode::OK, &map),
                Err(e) => {
                    tracing::error!(error = %e, "settings: failed to retrieve settings");
                    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve settings")
                }
            }
        }
        Method::PUT => handle_put(&st, pool, req).await,
        _ => crate::proxy(State(st), req).await,
    }
}

/// GET /api/settings/version. `req.query.check !== '1'` short-circuits the
/// update check; a qs array (repeated key) is !== '1' too — mirrored by
/// requiring exactly one occurrence (see check_param_is_one).
pub async fn route_version(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("settings") {
        return crate::proxy(State(st), req).await;
    }
    if req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st), req).await;
    };
    let env = SettingsEnv::from_env();
    let headers = req.headers().clone();
    if let Err(res) = authenticate(&pool, &headers, true) {
        return res;
    }
    let query = req.uri().query().map(str::to_string);
    let mut base = Map::new();
    base.insert("version".into(), Value::String(env.app_version.clone()));
    base.insert("releases_url".into(), Value::String(RELEASES_PAGE.to_string()));
    if !check_param_is_one(query.as_deref()) {
        return json_response(StatusCode::OK, &base);
    }
    match check_for_update(&env).await {
        Ok(Value::Object(data)) => {
            // `{ ...base, ...(await checkForUpdate()) }`
            for (k, v) in data {
                base.insert(k, v);
            }
            json_response(StatusCode::OK, &base)
        }
        Ok(_) => json_response(StatusCode::OK, &base),
        Err(e) => {
            tracing::warn!("Update check failed: {e}");
            base.insert("check_failed".into(), Value::Bool(true));
            json_response(StatusCode::OK, &base)
        }
    }
}

pub(crate) fn authenticate(pool: &DbPool, headers: &HeaderMap, is_get: bool) -> Result<auth::AuthUser, Response> {
    let token = auth::bearer_token(headers).map(str::to_string);
    auth::authenticate(pool, token.as_deref(), is_get)
}

/// middleware/auth.js requireAdmin (API keys are never admin credentials).
/// Shared with admin_users (iter-40).
pub(crate) fn admin_gate(user: &auth::AuthUser) -> Option<Response> {
    if user.via_api_key {
        return Some(json_error(
            StatusCode::FORBIDDEN,
            "API keys cannot access admin endpoints. Use a login session.",
        ));
    }
    if user.role.as_deref() != Some("admin") {
        return Some(json_error(
            StatusCode::FORBIDDEN,
            "Access denied. Administrator privileges required.",
        ));
    }
    None
}

async fn handle_put(st: &crate::AppState, pool: Arc<DbPool>, req: Request) -> Response {
    // body-parser first (normalJson is mounted above the router): malformed
    // JSON answers the generic 500 before auth; >1 MiB the 413 shape. The
    // headers are cloned up front — body-parser consumes the stream only,
    // auth still sees every header.
    let headers = req.headers().clone();
    let body = match read_json_body(req).await {
        Ok(b) => b,
        Err(res) => return res,
    };
    let user = match authenticate(&pool, &headers, false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    // statsCache.invalidateOnWrite twin: Node bumps the user's stats gen
    // after auth (before the role gate and the handler body) — a PUT that
    // later 400s/403s still bumped in Node. (REVIEW-writes L1.)
    crate::stats::on_native_write(st, user.id);
    if let Some(res) = admin_gate(&user) {
        return res;
    }
    let env = SettingsEnv::from_env();
    // Node runs each accepted arm as its own autocommit UPDATE and returns
    // 400 EARLY from the price/moxfield/base-url gates — earlier writes stay
    // committed. No transaction here: wrapping it would roll back on a 400
    // what Node leaves behind.
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return internal_error("settings: write pool", e),
    };
    // Destructure-gated arms: `x !== undefined` — an explicit null RUNS.
    let gated = |k: &str| -> Option<&Value> { body.get(k) };

    if let Some(v) = gated("price_refresh_days") {
        let n = crate::decks_reads::js_number(Some(v));
        if !js_is_integer(n) || n < 0.0 || n > 30.0 {
            return json_error(
                StatusCode::BAD_REQUEST,
                "Price refresh interval must be a whole number of days from 0 (off) to 30.",
            );
        }
        if let Err(e) = conn.execute(
            "UPDATE app_settings SET price_refresh_days = ?1 WHERE id = 1",
            rusqlite::params![n as i64],
        ) {
            return internal_error("settings: price refresh update", e);
        }
    }
    for key in [
        "scan_exclude_tokens",
        "scan_exclude_art_cards",
        "scan_exclude_jumpstart",
        "scan_exclude_promos",
    ] {
        if let Some(v) = gated(key) {
            if let Err(e) = conn.execute(
                &format!("UPDATE app_settings SET {key} = ?1 WHERE id = 1"),
                rusqlite::params![if truthy(v) { 1 } else { 0 }],
            ) {
                return internal_error("settings: scan flag update", e);
            }
        }
    }
    for (key, msg) in [
        (
            "moxfield_decklist_interval_min",
            "Moxfield decklist interval must be a whole number of minutes (1–1440)",
        ),
        (
            "moxfield_content_interval_min",
            "Moxfield content interval must be a whole number of minutes (1–1440)",
        ),
    ] {
        if let Some(v) = gated(key) {
            let n = match parse_int_10(v) {
                Some(n) if js_is_integer(n) && n >= 1.0 => n.min(24.0 * 60.0) as i64,
                _ => return json_error(StatusCode::BAD_REQUEST, msg),
            };
            if let Err(e) = conn.execute(
                &format!("UPDATE app_settings SET {key} = ?1 WHERE id = 1"),
                rusqlite::params![n],
            ) {
                return internal_error("settings: moxfield interval update", e);
            }
        }
    }
    if let Some(v) = gated("setup_complete") {
        if let Err(e) = conn.execute(
            "UPDATE app_settings SET setup_complete = ?1 WHERE id = 1",
            rusqlite::params![if truthy(v) { 1 } else { 0 }],
        ) {
            return internal_error("settings: setup_complete update", e);
        }
    }
    if let Some(v) = gated("public_base_url") {
        // Node: `public_base_url.trim()` outside the try — a non-string is
        // a TypeError => process death. Rust answers the route-catch 500.
        let Some(s) = v.as_str() else {
            return internal_error(
                "settings: public_base_url non-string (Node twin dies with .trim TypeError)",
                js_string(v),
            );
        };
        let trimmed = js_trim(s);
        if !trimmed.is_empty() {
            let lower = trimmed.to_ascii_lowercase();
            if !lower.starts_with("http://") && !lower.starts_with("https://") {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "Public base URL must start with http:// or https://",
                );
            }
        }
        // `trimmed.replace(/\/+$/,'')` — strip a trailing slash run; an
        // empty trimmed value still WRITES ''.
        let cleaned = trimmed.trim_end_matches('/');
        if let Err(e) = conn.execute(
            "UPDATE app_settings SET public_base_url = ?1 WHERE id = 1",
            rusqlite::params![cleaned],
        ) {
            return internal_error("settings: public_base_url update", e);
        }
    }
    match get_effective_settings(&pool, &env) {
        Ok(map) => json_response(StatusCode::OK, &map),
        Err(e) => {
            tracing::error!(error = %e, "settings: failed to load after update");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update settings")
        }
    }
}

/// express.json(strict) twin for the PUT: only application/json bodies
/// parse; everything else is the `{}` req.body default. Same shape as
/// search_history.rs (limit check, empty shortcut, strict parse).
async fn read_json_body(req: Request) -> Result<Value, Response> {
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().replace(' ', "").contains("application/json"))
        .unwrap_or(false);
    if !is_json {
        return Ok(Value::Object(Default::default()));
    }
    let (b, too_big) = read_capped(req.into_body()).await;
    if too_big {
        return Err(json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Upload too large. Try exporting/importing in smaller batches.",
        ));
    }
    if b.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    match serde_json::from_slice::<Value>(&b) {
        Ok(v @ (Value::Object(_) | Value::Array(_))) => Ok(v),
        _ => {
            tracing::error!("settings: invalid JSON body");
            Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))
        }
    }
}

async fn read_capped(mut body: Body) -> (Vec<u8>, bool) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match body.frame().await {
            Some(Ok(f)) => {
                if !f.is_data() {
                    continue;
                }
                match f.into_data() {
                    Ok(b) if buf.len() + b.len() <= BODY_LIMIT => buf.extend_from_slice(&b),
                    _ => {
                        let mut frames = 0usize;
                        while body.frame().await.is_some() {
                            frames += 1;
                            if frames > 8192 {
                                break;
                            }
                        }
                        return (buf, true);
                    }
                }
            }
            Some(Err(_)) | None => break,
        }
    }
    (buf, false)
}

/// qs/decodeURIComponent twin for simple scalars: percent-decode (invalid
/// escapes stay literal), '+' to space.
fn qs_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() + 1 => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `req.query.check !== '1'`: a repeated simple key becomes an ARRAY in qs
/// (String(['1','1']) === '1,1' !== '1'), so a scalar '1' requires exactly
/// one occurrence whose decoded value is '1'.
fn check_param_is_one(query: Option<&str>) -> bool {
    let Some(q) = query else { return false };
    let mut seen = 0usize;
    let mut last = None;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next().unwrap_or("");
        if qs_decode(key) == "check" {
            seen += 1;
            last = it.next().map(qs_decode);
        }
    }
    seen == 1 && last.as_deref() == Some("1")
}

// ---------------------------------------------------------------------------
// Test-only windows into the private twins (tests/settings.rs).
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub fn parse_int_10_pub(v: &Value) -> Option<f64> {
    parse_int_10(v)
}

#[doc(hidden)]
pub fn check_param_is_one_pub(q: Option<&str>) -> bool {
    check_param_is_one(q)
}

#[doc(hidden)]
pub fn get_effective_settings_pub(pool: &DbPool, env: &SettingsEnv) -> Result<Map<String, Value>, String> {
    get_effective_settings(pool, env)
}

#[doc(hidden)]
pub async fn check_for_update_pub(env: &SettingsEnv) -> Result<Value, String> {
    check_for_update(env).await
}
