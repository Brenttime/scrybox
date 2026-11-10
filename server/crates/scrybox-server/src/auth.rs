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
}

/// Extract the Bearer token exactly like Node: only an `Authorization`
/// header starting with `Bearer ` qualifies.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    raw.strip_prefix("Bearer ").map(|t| t.trim())
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

    // Node's exact two queries, in order. Only user_id/role are consumed by
    // ported routes, but the FROM/WHERE clauses are what must match.
    let lookup = (|| -> rusqlite::Result<Option<(i64, Option<String>, bool)>> {
        let mut stmt = conn.prepare_cached(SESSION_SQL)?;
        if let Some(row) = stmt
            .query_row([token], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(2)?)))
            .optional()?
        {
            return Ok(Some((row.0, row.1, false)));
        }
        let mut stmt = conn.prepare_cached(API_KEY_SQL)?;
        if let Some(row) = stmt
            .query_row([token], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(2)?)))
            .optional()?
        {
            return Ok(Some((row.0, row.1, true)));
        }
        Ok(None)
    })();

    match lookup {
        Ok(Some((id, role, via_api_key))) => {
            if via_api_key && !is_get {
                return Err(json_error(
                    StatusCode::FORBIDDEN,
                    "API keys are read-only. Use a login session for this request.",
                ));
            }
            Ok(AuthUser { id, role, via_api_key })
        }
        Ok(None) => Err(json_error(StatusCode::UNAUTHORIZED, "Invalid or expired session token.")),
        Err(e) => {
            tracing::error!(error = %e, "auth: query failed");
            Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Authentication error"))
        }
    }
}
