//! Port of `backend/src/middleware/auth.js` `authenticateToken`. The SQL and
//! the exact error bodies must stay byte-identical to Node: the parity
//! harness asserts them.

use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use rusqlite::OptionalExtension;

use crate::db::DbPool;

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub role: Option<String>,
    pub via_api_key: bool,
    /// middleware/auth.js builds req.user from the full column list; /me
    /// serialises exactly that. Ported GET routes only consume id/role, so
    /// these stay None on hot paths that pass `full=false`… actually the
    /// middleware always reads all columns (same single row fetch), so we
    /// always populate — the extra columns cost one row-copy.
    pub username: Option<String>,
    pub share_token: Option<String>,
    pub share_enabled: Option<i64>,
    pub api_key: Option<String>,
    pub oidc_sub: Option<String>,
}

/// Extract the Bearer token exactly like Node: only an `Authorization`
/// header starting with `Bearer ` qualifies.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    // Node (middleware/auth.js:7-9): `authHeader.startsWith('Bearer ')`
    // (case-sensitive, single space) then `split(' ')[1]`; empty ⇒ falsy.
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if !raw.starts_with("Bearer ") {
        return None;
    }
    raw.split(' ').nth(1).filter(|s| !s.is_empty())
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

// Built once (concat! of consts): the handlers call this on the hot request
// path and prepare_cached hashes the SQL text on every call.
const SESSION_SQL: &str = concat!(
    "SELECT s.user_id, u.username, u.role, u.share_token, u.share_enabled, u.api_key, u.oidc_sub ",
    "FROM sessions s JOIN users u ON s.user_id = u.id ",
    "WHERE s.token = ?1 AND s.expires_at > DATETIME('now')"
);
const API_KEY_SQL: &str = concat!(
    "SELECT u.id AS user_id, u.username, u.role, u.share_token, u.share_enabled, u.api_key, u.oidc_sub ",
    "FROM users u WHERE u.api_key = ?1"
);

/// Run the two Node lookups (session, then API key) on a pooled connection.
/// `is_get` mirrors the `req.method !== 'GET'` API-key rejection; ported
/// routes are GET-only so it never fires there, kept for exactness.
///
/// Takes the extracted bearer token (not the HeaderMap) so the hot path can
/// run this synchronously on a runtime worker: the whole lookup measured
/// ~5 us on the seeded fixture, cheaper than a spawn_blocking hop.
pub fn authenticate(pool: &DbPool, token: Option<&str>, is_get: bool) -> Result<AuthUser, Response> {
    let Some(token) = token else {
        return Err(json_error(StatusCode::UNAUTHORIZED, "Access denied. No token provided."));
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "auth: cannot open read connection");
            return Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Authentication error"));
        }
    };

    // Node's exact two queries, in order. All seven columns come back
    // (middleware/auth.js selects them for req.user); the tuple maps
    // (user_id, username, role, share_token, share_enabled, api_key, oidc_sub).
    type Row = (i64, Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>, Option<String>);
    let lookup = (|| -> rusqlite::Result<Option<(Row, bool)>> {
        let map = |r: &rusqlite::Row| -> rusqlite::Result<Row> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        };
        let mut stmt = conn.prepare_cached(SESSION_SQL)?;
        if let Some(row) = stmt.query_row([token], map).optional()? {
            return Ok(Some((row, false)));
        }
        let mut stmt = conn.prepare_cached(API_KEY_SQL)?;
        if let Some(row) = stmt.query_row([token], map).optional()? {
            return Ok(Some((row, true)));
        }
        Ok(None)
    })();

    match lookup {
        Ok(Some(((id, username, role, share_token, share_enabled, api_key, oidc_sub), via_api_key))) => {
            if via_api_key && !is_get {
                return Err(json_error(
                    StatusCode::FORBIDDEN,
                    "API keys are read-only. Use a login session for this request.",
                ));
            }
            Ok(AuthUser {
                id,
                role,
                via_api_key,
                username,
                share_token,
                share_enabled,
                api_key,
                oidc_sub,
            })
        }
        Ok(None) => Err(json_error(StatusCode::UNAUTHORIZED, "Invalid or expired session token.")),
        Err(e) => {
            tracing::error!(error = %e, "auth: query failed");
            Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Authentication error"))
        }
    }
}
