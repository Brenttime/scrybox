//! Port of `backend/src/middleware/auth.js` `authenticateToken`. The SQL and
//! the exact error bodies must stay byte-identical to Node: the parity
//! harness asserts them.

use axum::{
    http::{header, HeaderMap, StatusCode},
    response::Response,
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

use crate::util::json_error;

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
///
/// NOTE (REVIEW M5): `pool.get()` can open a connection / wait on the live
/// cap, both of which block the calling thread. Callers on async-runtime
/// workers on the hot request path should prefer [`authenticate_async`];
/// this sync form stays for code already on a blocking thread.
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
    authenticate_on(&conn, token, is_get)
}

/// M5 twin of [`authenticate`] for handlers running on runtime workers:
/// checks out through `DbPool::get_async()` (idle-only pop on the worker,
/// blocking-pool hop when no connection is idle), so neither the auth
/// lookup nor a pool stall can freeze the worker for up to `busy_timeout`.
/// Same SQL, same bodies — parity is unaffected.
pub async fn authenticate_async(
    pool: &std::sync::Arc<DbPool>,
    token: Option<&str>,
    is_get: bool,
) -> Result<AuthUser, Response> {
    let Some(token) = token else {
        return Err(json_error(StatusCode::UNAUTHORIZED, "Access denied. No token provided."));
    };
    let conn = match pool.get_async().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "auth: cannot open read connection");
            return Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Authentication error"));
        }
    };
    authenticate_on(&conn, token, is_get)
}

/// The shared lookup body: Node's exact two queries, in order, on an
/// already-held connection.
fn authenticate_on(conn: &rusqlite::Connection, token: &str, is_get: bool) -> Result<AuthUser, Response> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn seeded_pool(tag: &str) -> DbPool {
        let dir = std::env::temp_dir().join(format!("scrybox-authasync-{}-{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.db");
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT, role TEXT,
                        share_token TEXT, share_enabled INTEGER, api_key TEXT, oidc_sub TEXT);
             CREATE TABLE sessions (token TEXT PRIMARY KEY, user_id INTEGER, expires_at TEXT);
             INSERT INTO users VALUES (7, 'member', 'member', 'share-7', 1, 'apikey-7', NULL);
             INSERT INTO sessions VALUES ('sess-7', 7, DATETIME('now', '+1 hour'));",
        )
        .unwrap();
        drop(conn);
        DbPool::open(&p, 2).unwrap()
    }

    #[test]
    fn authenticate_async_session_key_and_reject_paths() {
        // The M5 twin answers exactly like the sync authenticate: session
        // hit, API-key hit (via_api_key), the read-only API-key 403 shape
        // (Err response), and the unknown-token 401 (Err response).
        let pool = Arc::new(seeded_pool("core"));
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();

        let p = Arc::clone(&pool);
        let user = rt.block_on(async move { authenticate_async(&p, Some("sess-7"), true).await.unwrap() });
        assert_eq!(user.id, 7);
        assert!(!user.via_api_key);

        let p = Arc::clone(&pool);
        let user = rt.block_on(async move { authenticate_async(&p, Some("apikey-7"), true).await.unwrap() });
        assert_eq!(user.id, 7);
        assert!(user.via_api_key);

        let p = Arc::clone(&pool);
        assert!(rt.block_on(async move { authenticate_async(&p, Some("apikey-7"), false).await }).is_err());
        let p = Arc::clone(&pool);
        assert!(rt.block_on(async move { authenticate_async(&p, Some("nope"), true).await }).is_err());
        let p = Arc::clone(&pool);
        assert!(rt.block_on(async move { authenticate_async(&p, None, true).await }).is_err());

        // Under the live cap the async path still completes (blocking hop).
        let held = pool.get().unwrap();
        let p = Arc::clone(&pool);
        let user2 = rt.block_on(async move { authenticate_async(&p, Some("sess-7"), true).await.unwrap() });
        assert_eq!(user2.id, 7);
        drop((held, user2));
    }
}
