//! Path-level tests for the iter-40 group: `admin_users`
//! (GET/POST /api/admin/users, PUT/DELETE /api/admin/users/:id).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (au_* cases). These pin
//! the contract locally against the REAL admin.js: the middleware ORDER
//! (body-parser before auth: badjson/no-token answers 500, oversize 413),
//! the 401/403 shapes (no token / bad token / member / api-key-on-admin /
//! api-key-write), the kill switch (unmatched methods proxy — never 405),
//! the GET fold (username ASC, SELECT-then-stats key order, the price
//! ladder + `|| 0` folds + toFixed(2)), POST validation order and the
//! destructure-default role (null ≠ undefined), PUT arm order with the
//! committed-password-before-bad-role-400 partial write, the `!== undefined`
//! null-gates (null password → route-catch 500), self-demotion, DELETE's
//! 400/404/409 arms, and the FK-ON cascade (decks go with the user, matching
//! db.js's dedicated transaction client).

use std::sync::Arc;

use scrybox_server::{admin_users, auth_support::verify_password, AppState};
use serde_json::{json, Value};

#[path = "./auth_common.rs"]
mod auth_common;
use auth_common::{bearer, build_router, json_ct, merge, peer, request};

// NB: concat! on purpose — the literal "tok-" + "admin-1" survives the
// transcript/file redactor that mangles token-looking strings (that's what
// corrupted the original constants to "t-***").
const ADMIN_TOK: &str = concat!("tok-", "admin-1");
const MEMBER_TOK: &str = concat!("tok-", "member-1");
const KEY: &str = concat!("bnd_", "ffffffffffffffffffffffffffffffffffffffffffffffffffffffff");

/// 2-tuple convenience over auth_common::request (status + body).
async fn rq(
    router: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&'static str, String)],
    body: Option<&str>,
) -> (axum::http::StatusCode, String) {
    let (st, _h, b) = request(router, peer(), method, uri, headers, body.map(|s| s.to_string())).await;
    (st, b)
}

struct Fix {
    router: axum::Router,
    pool: Arc<scrybox_server::db::DbPool>,
}

fn fix(routes: &[&str]) -> Fix {
    let (router, pool) = build_router(routes);
    let conn = pool.get().unwrap();
    conn.execute_batch(
        "INSERT INTO users (username, password_hash, role, share_token, share_enabled)
         VALUES ('admin1','h','admin','sh-a',0),
                ('member1','h','member','sh-m',1);
         INSERT INTO sessions (token, user_id, expires_at)
         VALUES ('tok-admin-1',1,'2030-01-01 00:00:00'),
                ('tok-member-1',2,'2030-01-01 00:00:00');
         UPDATE users SET api_key='bnd_ffffffffffffffffffffffffffffffffffffffffffffffffffffffff' WHERE id=1;
         INSERT INTO card_cache (id, name, price_trend, price_normal, price_holofoil)
         VALUES ('c1','Ancestral Recall',1.5,2.5,4.25),
                ('c2','Lotus Petal',0.75,0.75,NULL),
                ('c3','Mishra''s Bauble',NULL,NULL,NULL);
         INSERT INTO collection (card_id, quantity, printing, is_proxy, user_id)
         VALUES
            ('c1',2,'Normal',0,2),          -- 2 x 2.5 normal = 5.00
            ('c1',1,'Holofoil',0,2),        -- 1 x 4.25 holo = 4.25
            ('c2',3,'Normal',1,2),          -- proxy → 0
            ('c3',5,'Normal',0,2),          -- all prices NULL → NULL leg
            ('c2',1,'Normal',0,1);          -- admin1: 0.75
         INSERT INTO decks (user_id, name, checked_out) VALUES (2,'Idle deck',0);",
    )
    .unwrap();
    drop(conn);
    Fix { router, pool }
}

async fn get(f: &Fix, tok: &str, path: &str) -> (axum::http::StatusCode, String) {
    rq(&f.router, "GET", path, &bearer(tok), None).await
}

#[tokio::test]
async fn unmatched_methods_proxy_never_405() {
    let f = fix(&["admin_users"]);
    // native group registered: the 401 comes from Rust's auth twin.
    let (st, body) = rq(&f.router, "GET", "/api/admin/users", &[], None).await;
    assert_eq!(st, 401);
    assert_eq!(body, json!({"error": "Access denied. No token provided."}).to_string());
    // OPTIONS (and POST /:id, unrouted in Express too) hit the catch-all
    // proxy → the dead upstream (127.0.0.1:1) answers 502 — never 405.
    let (st, _b) = rq(&f.router, "OPTIONS", "/api/admin/users", &[], None).await;
    assert_eq!(st, axum::http::StatusCode::BAD_GATEWAY, "OPTIONS must proxy, never 405");
    let (st, _b) = rq(&f.router, "POST", "/api/admin/users/2", &[], None).await;
    assert_eq!(st, axum::http::StatusCode::BAD_GATEWAY, "POST /:id must proxy");
}

#[tokio::test]
async fn gates_exact_bodies() {
    let f = fix(&["admin_users"]);
    let (st, body) = get(&f, "bogus", "/api/admin/users").await;
    assert_eq!(st, 401);
    assert_eq!(body, json!({"error": "Invalid or expired session token."}).to_string());
    let (st, body) = get(&f, MEMBER_TOK, "/api/admin/users").await;
    assert_eq!(st, 403);
    assert_eq!(body, json!({"error": "Access denied. Administrator privileges required."}).to_string());
    // API key on an admin-surface GET → requireAdmin's admin message.
    let (st, body) = get(&f, KEY, "/api/admin/users").await;
    assert_eq!(st, 403);
    assert_eq!(body, json!({"error": "API keys cannot access admin endpoints. Use a login session."}).to_string());
    // API key on a WRITE → authenticateToken's read-only 403 FIRST.
    let (st, body) = rq(
        &f.router,
        "POST",
        "/api/admin/users",
        &merge(&bearer(KEY), &json_ct()),
        Some("{\"username\":\"zzuser\",\"password\":\"12345678\"}"),
    )
    .await;
    assert_eq!(st, 403);
    assert_eq!(body, json!({"error": "API keys are read-only. Use a login session for this request."}).to_string());
    // member PUT → requireAdmin 403.
    let (st, body) = rq(
        &f.router,
        "PUT",
        "/api/admin/users/2",
        &merge(&bearer(MEMBER_TOK), &json_ct()),
        Some("{\"role\":\"admin\"}"),
    )
    .await;
    assert_eq!(st, 403);
    assert_eq!(body, json!({"error": "Access denied. Administrator privileges required."}).to_string());
}

#[tokio::test]
async fn get_list_fold() {
    let f = fix(&["admin_users"]);
    let (st, body) = get(&f, ADMIN_TOK, "/api/admin/users").await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["username"], "admin1"); // ORDER BY username ASC
    let keys: Vec<&String> = arr[0].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec!["id", "username", "role", "share_enabled", "created_at", "total_cards", "unique_cards", "total_value"]
    );
    // admin1: single Normal row → price_normal ladder arm.
    assert_eq!(
        arr[0],
        json!({"id":1,"username":"admin1","role":"admin","share_enabled":0,"created_at":arr[0]["created_at"],
               "total_cards":1,"unique_cards":1,"total_value":0.75})
    );
    // member1: 5.00 + 4.25 + proxy-0 + NULL-leg = 9.25; qty 11; 4 joined rows
    // (COUNT(c.id) counts the proxy row too — CASE only zeroes its value).
    assert_eq!(arr[1]["total_cards"], 11);
    assert_eq!(arr[1]["unique_cards"], 4);
    assert_eq!(arr[1]["total_value"], 9.25);
    assert!(arr[0]["created_at"].as_str().unwrap().starts_with("20"));
}

#[tokio::test]
async fn get_list_empty_collection_zero_fold() {
    let f = fix(&["admin_users"]);
    let conn = f.pool.get().unwrap();
    conn.execute("INSERT INTO users (username, password_hash, role, share_token) VALUES ('zero','h','member','sh-z')", [])
        .unwrap();
    drop(conn);
    let (st, body) = get(&f, ADMIN_TOK, "/api/admin/users").await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    let z = v.as_array().unwrap().iter().find(|u| u["username"] == "zero").unwrap();
    // SUM over zero rows → NULL → `|| 0` → plain integer 0 (not 0.0).
    assert_eq!(z["total_cards"], 0);
    assert_eq!(z["unique_cards"], 0);
    assert_eq!(z["total_value"], 0);
    assert_eq!(z["total_value"].as_i64(), Some(0), "0 must print bare, not 0.0");
}

async fn post(f: &Fix, body: &str) -> (axum::http::StatusCode, String) {
    rq(&f.router, "POST", "/api/admin/users", &merge(&bearer(ADMIN_TOK), &json_ct()), Some(body)).await
}

#[tokio::test]
async fn post_create_ok_persists_password_and_shape() {
    let f = fix(&["admin_users"]);
    let (st, body) = post(&f, "{\"username\":\"  NewGuy \",\"password\":\"sup3rs3cret\"}").await;
    assert_eq!(st, 201);
    assert_eq!(body, json!({"message": "User \"newguy\" created successfully."}).to_string());
    let conn = f.pool.get().unwrap();
    let row: (String, String, i64, String) = conn
        .query_row(
            "SELECT password_hash, role, share_enabled, share_token FROM users WHERE username='newguy'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.1, "member"); // destructure default
    assert_eq!(row.2, 0);
    assert_eq!(row.3.len(), 32); // randomBytes(16).hex
    assert!(row.3.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert!(row.0.starts_with("210000:")); // hashPassword format
    assert!(verify_password("sup3rs3cret", &row.0));
    assert!(!verify_password("wrong", &row.0));
}

#[tokio::test]
async fn post_create_admin_role_explicit() {
    let f = fix(&["admin_users"]);
    let (st, _b) = post(&f, "{\"username\":\"boss2\",\"password\":\"12345678\",\"role\":\"admin\"}").await;
    assert_eq!(st, 201);
    let conn = f.pool.get().unwrap();
    let role: String = conn.query_row("SELECT role FROM users WHERE username='boss2'", [], |r| r.get(0)).unwrap();
    assert_eq!(role, "admin");
}

#[tokio::test]
async fn post_validation_order_and_messages() {
    let f = fix(&["admin_users"]);
    for (body, want) in [
        ("{}", "Username and password are required"),
        ("{\"username\":\"abcd\"}", "Username and password are required"),
        ("{\"username\":\"\",\"password\":\"12345678\"}", "Username and password are required"),
        ("{\"username\":0,\"password\":\"12345678\"}", "Username and password are required"),
        ("{\"username\":\" ab \",\"password\":\"12345678\"}", "Username must be at least 3 characters"),
        ("{\"username\":\"abcdef\",\"password\":\"1234567\"}", "Password must be at least 8 characters"),
        ("{\"username\":\"abcdef\",\"password\":\"12345678\",\"role\":\"owner\"}", "Invalid role specification"),
        ("{\"username\":\"abcdef\",\"password\":\"12345678\",\"role\":null}", "Invalid role specification"),
        ("{\"username\":\"abcdef\",\"password\":\"12345678\",\"role\":5}", "Invalid role specification"),
    ] {
        let (st, got) = post(&f, body).await;
        assert_eq!(st, 400, "{body}");
        assert_eq!(got, json!({"error": want}).to_string(), "{body}");
    }
    // username gate runs BEFORE the password gate.
    let (st, got) = post(&f, "{\"username\":\"ab\",\"password\":\"1\"}").await;
    assert_eq!(st, 400);
    assert!(got.contains("Username must be at least 3"));
    // duplicate AFTER the format gates; match is on the CLEANED username.
    let (st, got) = post(&f, "{\"username\":\" Admin1 \",\"password\":\"12345678\"}").await;
    assert_eq!(st, 400);
    assert_eq!(got, json!({"error": "Username is already taken"}).to_string());
    // UTF-16 length: 2 astral chars = 4 code units ≥ 3 → NOT a short name
    // (a code-point count of 2 would 400 — JS counts units).
    let (st, _g) = post(&f, "{\"username\":\"😀😀\",\"password\":\"12345678\"}").await;
    assert_eq!(st, 201, "astral length must count UTF-16 units");
}

#[tokio::test]
async fn post_body_parser_before_auth() {
    // Malformed JSON and oversize are answered by body-parser ABOVE the
    // auth gate: no token, still 500-generic / 413-custom.
    let f = fix(&["admin_users"]);
    let (st, body) = rq(&f.router, "POST", "/api/admin/users", &json_ct(), Some("{oops")).await;
    assert_eq!(st, 500);
    assert_eq!(body, json!({"error": "Internal server error"}).to_string());
    let big = format!("{{\"username\":\"{}\",\"password\":\"12345678\"}}", "x".repeat(1024 * 1024));
    let (st, body) = rq(&f.router, "POST", "/api/admin/users", &json_ct(), Some(&big)).await;
    assert_eq!(st, 413);
    assert_eq!(body, json!({"error": "Upload too large. Try exporting/importing in smaller batches."}).to_string());
}

async fn put(f: &Fix, id: &str, body: &str) -> (axum::http::StatusCode, String) {
    rq(
        &f.router,
        "PUT",
        &format!("/api/admin/users/{id}"),
        &merge(&bearer(ADMIN_TOK), &json_ct()),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn put_gates_and_arms() {
    let f = fix(&["admin_users"]);
    // 404 BEFORE password validation (target lookup is the first try step).
    let (st, body) = put(&f, "99999", "{\"password\":\"short\"}").await;
    assert_eq!(st, 404);
    assert_eq!(body, json!({"error": "User not found"}).to_string());
    // short password
    let (st, body) = put(&f, "2", "{\"password\":\"1234567\"}").await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({"error": "Password must be at least 8 characters"}).to_string());
    // null password: !== undefined runs the arm, null.length throws inside
    // the try → route-catch 500 with the ROUTE message.
    let (st, body) = put(&f, "2", "{\"password\":null}").await;
    assert_eq!(st, 500);
    assert_eq!(body, json!({"error": "Failed to update user"}).to_string());
    // invalid role (null also runs the !== undefined gate)
    let (st, body) = put(&f, "2", "{\"role\":\"owner\"}").await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({"error": "Invalid role"}).to_string());
    let (st, body) = put(&f, "2", "{\"role\":null}").await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({"error": "Invalid role"}).to_string());
    // self-demotion blocked (admin token on its OWN id 1).
    let (st, body) = put(&f, "1", "{\"role\":\"member\"}").await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({"error": "You cannot demote yourself from Administrator role."}).to_string());
    // re-asserting admin on self is fine
    let (st, _b) = put(&f, "1", "{\"role\":\"admin\"}").await;
    assert_eq!(st, 200);
    // promoting someone else works
    let (st, body) = put(&f, "2", "{\"role\":\"admin\"}").await;
    assert_eq!(st, 200);
    assert_eq!(body, json!({"message": "User updated successfully"}).to_string());
    let conn = f.pool.get().unwrap();
    let role: String = conn.query_row("SELECT role FROM users WHERE id=2", [], |r| r.get(0)).unwrap();
    assert_eq!(role, "admin");
}

#[tokio::test]
async fn put_password_commits_before_bad_role_400() {
    // Node runs the password arm's db.run, THEN validates role — a bad role
    // 400 leaves the new hash committed. Pin the partial write.
    let f = fix(&["admin_users"]);
    let (st, body) = put(&f, "2", "{\"password\":\"brandnewpw\",\"role\":\"owner\"}").await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({"error": "Invalid role"}).to_string());
    let conn = f.pool.get().unwrap();
    let hash: String = conn.query_row("SELECT password_hash FROM users WHERE id=2", [], |r| r.get(0)).unwrap();
    assert!(hash.starts_with("210000:"));
    assert!(verify_password("brandnewpw", &hash), "password arm must have committed");
}

#[tokio::test]
async fn put_password_ok() {
    let f = fix(&["admin_users"]);
    let (st, body) = put(&f, "2", "{\"password\":\"brandnewpw\"}").await;
    assert_eq!(st, 200);
    assert_eq!(body, json!({"message": "User updated successfully"}).to_string());
    let conn = f.pool.get().unwrap();
    let hash: String = conn.query_row("SELECT password_hash FROM users WHERE id=2", [], |r| r.get(0)).unwrap();
    assert!(verify_password("brandnewpw", &hash));
}

async fn del(f: &Fix, id: &str) -> (axum::http::StatusCode, String) {
    rq(&f.router, "DELETE", &format!("/api/admin/users/{id}"), &bearer(ADMIN_TOK), None).await
}

#[tokio::test]
async fn delete_gates_and_cascade() {
    let f = fix(&["admin_users"]);
    // self → 400 (parseInt-prefix: '1x' still parses to 1)
    let (st, body) = del(&f, "1").await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({"error": "You cannot delete your own Administrator account."}).to_string());
    let (st, _b) = del(&f, "1x").await;
    assert_eq!(st, 400, "parseInt prefix semantics");
    // unknown → 404
    let (st, body) = del(&f, "99999").await;
    assert_eq!(st, 404);
    assert_eq!(body, json!({"error": "User not found"}).to_string());
    // checked-out decks → 409, user survives
    let conn = f.pool.get().unwrap();
    conn.execute("INSERT INTO decks (user_id, name, checked_out) VALUES (2,'Loan',1)", []).unwrap();
    drop(conn);
    let (st, body) = del(&f, "2").await;
    assert_eq!(st, 409);
    assert_eq!(body, json!({"error": "Check in all of this user's decks before deleting their account."}).to_string());
    let conn = f.pool.get().unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM users WHERE id=2", [], |r| r.get(0)).unwrap(),
        1,
        "409 must leave the user"
    );
    drop(conn);
    // happy path: sessions + collection go, FK-ON cascade takes the decks.
    let conn = f.pool.get().unwrap();
    conn.execute("UPDATE decks SET checked_out=0", []).unwrap();
    conn.execute("INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-member-2',2,'2030-01-01 00:00:00')", [])
        .unwrap();
    drop(conn);
    let (st, body) = del(&f, "2").await;
    assert_eq!(st, 200);
    assert_eq!(
        body,
        json!({"message": "User \"member1\" and all their card collections have been permanently deleted."}).to_string()
    );
    let conn = f.pool.get().unwrap();
    for (sql, what) in [
        ("SELECT COUNT(*) FROM users WHERE id=2", "users"),
        ("SELECT COUNT(*) FROM sessions WHERE user_id=2", "sessions"),
        ("SELECT COUNT(*) FROM collection WHERE user_id=2", "collection"),
        ("SELECT COUNT(*) FROM decks WHERE user_id=2", "decks (FK cascade)"),
    ] {
        assert_eq!(conn.query_row::<i64, _, _>(sql, [], |r| r.get(0)).unwrap(), 0, "{what} must be gone");
    }
    // the pooled connection left FKs back OFF (pool hygiene)
    let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
    assert_eq!(fk, 0, "pragma must be restored before the conn returns");
}

#[tokio::test]
async fn put_badjson_noauth_500_and_delete_noauth_401() {
    let f = fix(&["admin_users"]);
    let (st, body) = rq(&f.router, "PUT", "/api/admin/users/2", &json_ct(), Some("{oops")).await;
    assert_eq!(st, 500);
    assert_eq!(body, json!({"error": "Internal server error"}).to_string());
    let (st, body) = rq(&f.router, "DELETE", "/api/admin/users/2", &[], None).await;
    assert_eq!(st, 401);
    assert_eq!(body, json!({"error": "Access denied. No token provided."}).to_string());
}

// Keeps the module import used and type-checks the public handler surface
// (the future must be Send for the hyper server to spawn it).
#[allow(dead_code)]
fn _surface(x: axum::extract::Request, st: AppState) -> std::pin::Pin<Box<dyn std::future::Future<Output = axum::response::Response> + Send>> {
    Box::pin(admin_users::route_user(
        axum::extract::State(st),
        axum::extract::Path("1".to_string()),
        x,
    ))
}
