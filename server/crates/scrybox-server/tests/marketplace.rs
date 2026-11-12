//! Path-level tests for the iter-41 slice: `marketplace` slice 1
//! (GET/PUT /api/marketplace/accounts, GET /api/marketplace/recent/:source).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (mp_* cases). These pin
//! the contract locally against routes/marketplaceOrders.js +
//! utils/marketplaceOrders.js: the middleware ORDER (body-parser before
//! auth: badjson answers the generic 500 with NO token, oversize 413),
//! the auth twin (API keys GET-only), the accountView mask ladder and key
//! order, the PUT arm order (Nothing-to-save → 404 → clear/enabled-only/
//! validation, email-before-token validation), the recent gates (auth →
//! source → 404 → creds), the whole-list sort-then-slice fold with
//! hasMore, the UUID detail count upgrade (list numbers kept on detail
//! failure), the guard arms (401/429 no list_unavailable, non-2xx 502 with
//! it), and the proxy fallback for everything unregistered (never 405).
//! The ManaPool leg is exercised through an injected stub client, exactly
//! like the Node unit tests inject `httpGet`.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use scrybox_server::marketplace::{
    self, mask_email, mask_secret, map_condition, order_lines, MpHttp, MpResponse,
};
use scrybox_server::{AppState, Config};
use serde_json::{json, Value};

#[path = "./auth_common.rs"]
mod auth_common;
use auth_common::{bearer, json_ct, merge, peer, request};

// concat! on purpose — survives the transcript redactor (see admin_users).
const MEMBER_TOK: &str = concat!("tok-", "mp-member-1");
const OTHER_TOK: &str = concat!("tok-", "mp-other-1");
const KEY: &str = concat!("bnd_", "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
const STUB_BASE: &str = "http://stub.invalid/api/v1";

fn stub_base_once() {
    static ON: std::sync::Once = std::sync::Once::new();
    ON.call_once(|| {
        std::env::set_var("MANAPOOL_API_BASE", STUB_BASE);
    });
}

/// Scripted httpGet twin: responses pop in call order; every call records
/// (url, extra headers) for assertions.
pub struct StubHttp {
    script: Mutex<VecDeque<(u16, Value)>>,
    pub log: Mutex<Vec<(String, Vec<(String, String)>)>>,
}

impl StubHttp {
    fn new(responses: Vec<(u16, Value)>) -> Arc<Self> {
        Arc::new(Self { script: Mutex::new(responses.into()), log: Mutex::new(Vec::new()) })
    }
    fn urls(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().map(|(u, _)| u.clone()).collect()
    }
    fn last_headers(&self) -> Vec<(String, String)> {
        self.log.lock().unwrap().last().map(|(_, h)| h.clone()).unwrap_or_default()
    }
}

impl MpHttp for StubHttp {
    fn get(
        &self,
        url: String,
        extra_headers: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = Result<MpResponse, marketplace::MpError>> + Send + 'static>>
    {
        self.log.lock().unwrap().push((url.clone(), extra_headers));
        let next = self.script.lock().unwrap().pop_front();
        Box::pin(async move {
            match next {
                Some((status, body)) => Ok(MpResponse { status, body }),
                None => panic!("stub ManaPool ran out of scripted responses for {url}"),
            }
        })
    }
}

struct Fix {
    router: axum::Router,
    pool: Arc<scrybox_server::db::DbPool>,
}

fn build(stub: Arc<StubHttp>) -> Fix {
    stub_base_once();
    let dir = std::env::temp_dir().join(format!("scrybox-mp-it-{}", scrybox_server::auth_support::uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bindarr.db"), []).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: "http://127.0.0.1:1".to_string(),
        static_dir: None, // matches restart-rust.sh (SCRYBOX_STATIC_DIR empty):
                          // the fallback is then proxy(), like the live box.
                          // (ServeDir-as-fallback answers unregistered POSTs
                          // with 405 — a lane-B static-mount nuance.)
        rust_routes: ["marketplace".to_string()].into_iter().collect(),
        db_path: Some(dir.join("bindarr.db")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.mp_http = stub;
    let pool = st.db.clone().unwrap();
    pool.get().unwrap().execute_batch(auth_common::schema_fixture()).unwrap();
    pool.get()
        .unwrap()
        .execute_batch(
            "INSERT INTO users (username, password_hash, role, share_token, manapool_email, manapool_token, manapool_enabled, manapool_saved_at, api_key)
             VALUES ('mpmember','h','member','sh-mp','player@example.com','abcdefghij',1,'2026-01-02T03:04:05.000Z','bnd_eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'),
                    ('mpother','h','member','sh-mp2',NULL,NULL,0,NULL,NULL);
             INSERT INTO sessions (token, user_id, expires_at)
             VALUES ('tok-mp-member-1',1,'2030-01-01 00:00:00'),
                    ('tok-mp-other-1',2,'2030-01-01 00:00:00');",
        )
        .unwrap();
    Fix { router: scrybox_server::app(st), pool }
}

async fn rq(
    f: &Fix,
    method: &str,
    uri: &str,
    headers: &[(&'static str, String)],
    body: Option<&str>,
) -> (axum::http::StatusCode, String) {
    let (st, _h, b) =
        request(&f.router, peer(), method, uri, headers, body.map(|s| s.to_string())).await;
    (st, b)
}

fn list_body(rows: Vec<Value>) -> Value {
    json!({ "data": rows })
}

fn row(number: &str, stamp: &str, id: Option<&str>) -> Value {
    let mut r = json!({
        "order_number": number,
        "created_at": stamp,
        "status": "shipped",
        "total_cents": 1234,
        "order_seller_details": [{ "item_count": 4 }],
    });
    match id {
        Some(i) => r.as_object_mut().unwrap().insert("id".into(), json!(i)),
        None => r.as_object_mut().unwrap().insert("id".into(), json!("not-a-uuid")),
    };
    r
}

const UUID_A: &str = "11111111-2222-3333-4444-555555555555";

fn detail_body() -> Value {
    json!({ "order": { "order_seller_details": [ { "items": [
        { "product": { "single": { "name": "Sol Ring", "set": "10E", "number": "235" } }, "quantity": 2 },
        { "product": { "sealed": { "name": "Draft Booster Box", "product_type": "bundle" } }, "quantity": 1 },
        { "quantity": 3 }
    ] } ] } })
}

// ---------------------------------------------------------------------------
// accounts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accounts_get_masked_byte_exact() {
    let f = build(StubHttp::new(vec![]));
    let (st, body) = rq(&f, "GET", "/api/marketplace/accounts", &bearer(MEMBER_TOK), None).await;
    assert_eq!(st, 200);
    assert_eq!(
        body,
        json!({ "manapool": {
            "configured": true, "enabled": true,
            "email": "pl\u{2022}\u{2022}\u{2022}\u{2022}@example.com",
            "token": "abc\u{2026}ghij (10 chars)",
            "savedAt": "2026-01-02T03:04:05.000Z"
        } })
        .to_string()
    );
}

#[tokio::test]
async fn accounts_get_unconfigured_and_gates() {
    let f = build(StubHttp::new(vec![]));
    let (st, body) = rq(&f, "GET", "/api/marketplace/accounts", &bearer(OTHER_TOK), None).await;
    assert_eq!(st, 200);
    assert_eq!(
        body,
        json!({ "manapool": {
            "configured": false, "enabled": false, "email": "", "token": "", "savedAt": null
        } })
        .to_string()
    );
    let (st, body) = rq(&f, "GET", "/api/marketplace/accounts", &[], None).await;
    assert_eq!(st, 401);
    assert_eq!(body, json!({ "error": "Access denied. No token provided." }).to_string());
    let (st, body) = rq(&f, "GET", "/api/marketplace/accounts", &bearer("bogus"), None).await;
    assert_eq!(st, 401);
    assert_eq!(body, json!({ "error": "Invalid or expired session token." }).to_string());
    // API keys pass on GET (no admin gate on this router).
    let (st, _b) = rq(&f, "GET", "/api/marketplace/accounts", &bearer(KEY), None).await;
    assert_eq!(st, 200);
}

#[tokio::test]
async fn put_body_parser_precedes_auth() {
    let f = build(StubHttp::new(vec![]));
    // Malformed JSON: generic 500 BEFORE auth would have answered 401.
    let (st, body) = rq(
        &f,
        "PUT",
        "/api/marketplace/accounts",
        &json_ct(),
        Some("{oops"),
    )
    .await;
    assert_eq!(st, 500);
    assert_eq!(body, json!({ "error": "Internal server error" }).to_string());
    // Oversize: 413 before auth.
    let big = format!("{{\"manapool\":{{\"token\":\"{}\"}}}}", "x".repeat(2 * 1024 * 1024));
    let (st, body) = rq(&f, "PUT", "/api/marketplace/accounts", &json_ct(), Some(&big)).await;
    assert_eq!(st, 413);
    assert!(body.contains("too large"));
}

#[tokio::test]
async fn put_api_key_readonly_and_nothing_to_save() {
    let f = build(StubHttp::new(vec![]));
    let (st, body) = rq(
        &f,
        "PUT",
        "/api/marketplace/accounts",
        &merge(&bearer(KEY), &json_ct()),
        Some("{\"manapool\":{\"clear\":true}}"),
    )
    .await;
    assert_eq!(st, 403);
    assert_eq!(
        body,
        json!({ "error": "API keys are read-only. Use a login session for this request." })
            .to_string()
    );
    // Key absent → 400 before the row load (works for a real user anyway).
    let (st, body) =
        rq(&f, "PUT", "/api/marketplace/accounts", &merge(&bearer(MEMBER_TOK), &json_ct()), Some("{}"))
            .await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({ "error": "Nothing to save" }).to_string());
    // Explicit null is NOT undefined: it runs the clear arm.
    let (st, body) = rq(
        &f,
        "PUT",
        "/api/marketplace/accounts",
        &merge(&bearer(MEMBER_TOK), &json_ct()),
        Some("{\"manapool\":null}"),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(
        body,
        json!({ "ok": true, "accounts": { "manapool": {
            "configured": false, "enabled": false, "email": "", "token": "", "savedAt": null
        } } })
        .to_string()
    );
    let cleared: (Option<String>, Option<String>, i64, Option<String>) = f
        .pool
        .get()
        .unwrap()
        .query_row(
            "SELECT manapool_email, manapool_token, manapool_enabled, manapool_saved_at FROM users WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(cleared, (None, None, 0, None));
}

#[tokio::test]
async fn put_validation_order_and_enabled_only() {
    let f = build(StubHttp::new(vec![]));
    // Bad email beats missing token (email check first).
    let (st, body) = rq(
        &f,
        "PUT",
        "/api/marketplace/accounts",
        &merge(&bearer(MEMBER_TOK), &json_ct()),
        Some("{\"manapool\":{\"email\":\"a@b\",\"token\":\"\"}}"),
    )
    .await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({ "error": "ManaPool email is not valid" }).to_string());
    // Missing token.
    let (st, body) = rq(
        &f,
        "PUT",
        "/api/marketplace/accounts",
        &merge(&bearer(MEMBER_TOK), &json_ct()),
        Some("{\"manapool\":{\"email\":\"a@b.c\",\"token\":\"  \"}}"),
    )
    .await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({ "error": "A ManaPool access token is required" }).to_string());
    // Enabled-only: row already configured → keep credential + savedAt.
    let (st, body) = rq(
        &f,
        "PUT",
        "/api/marketplace/accounts",
        &merge(&bearer(MEMBER_TOK), &json_ct()),
        Some("{\"manapool\":{\"enabled\":false}}"),
    )
    .await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["accounts"]["manapool"]["enabled"], json!(false));
    assert_eq!(v["accounts"]["manapool"]["configured"], json!(true));
    assert_eq!(v["accounts"]["manapool"]["savedAt"], json!("2026-01-02T03:04:05.000Z"));
    // Save a fresh credential: persisted + reloaded, savedAt stamped.
    let (st, body) = rq(
        &f,
        "PUT",
        "/api/marketplace/accounts",
        &merge(&bearer(MEMBER_TOK), &json_ct()),
        Some("{\"manapool\":{\"email\":\" new@ex.com \",\"token\":\"tok123456\",\"enabled\":false}}"),
    )
    .await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["accounts"]["manapool"]["email"], json!("ne\u{2022}\u{2022}@ex.com"));
    let (email, token, enabled, saved): (String, String, i64, String) = f
        .pool
        .get()
        .unwrap()
        .query_row(
            "SELECT manapool_email, manapool_token, manapool_enabled, manapool_saved_at FROM users WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(email, "new@ex.com"); // trimmed on write
    assert_eq!(token, "tok123456");
    assert_eq!(enabled, 0);
    assert!(saved.ends_with('Z') && saved.matches('-').count() == 2);
}

#[tokio::test]
async fn dangling_session_answers_401() {
    let f = build(StubHttp::new(vec![]));
    // authenticate twin JOINs users (Node's authenticateToken), so a session
    // whose user vanished 401s — the route's 'User not found' is the
    // same-query guard both sides keep.
    f.pool.get().unwrap().execute_batch(
        "PRAGMA foreign_keys=OFF;
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-dangling-1', 99, '2030-01-01 00:00:00');",
    ).unwrap();
    let (st, body) =
        rq(&f, "PUT", "/api/marketplace/accounts", &merge(&bearer(concat!("tok-", "dangling-1")), &json_ct()), Some("{\"manapool\":{\"clear\":true}}"))
            .await;
    assert_eq!(st, 401);
    assert_eq!(body, json!({ "error": "Invalid or expired session token." }).to_string());
}

// ---------------------------------------------------------------------------
// recent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recent_gates_order() {
    let f = build(StubHttp::new(vec![]));
    // Auth precedes the source gate.
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/bogus", &[], None).await;
    assert_eq!(st, 401);
    assert_eq!(body, json!({ "error": "Access denied. No token provided." }).to_string());
    // Source gate (uppercase lowercases through).
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/TCGPlayer", &bearer(OTHER_TOK), None).await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({ "error": "unknown source" }).to_string());
    // Creds gate.
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/ManaPool", &bearer(OTHER_TOK), None).await;
    assert_eq!(st, 400);
    assert_eq!(body, json!({ "error": "ManaPool is not configured or is turned off" }).to_string());
}

#[tokio::test]
async fn recent_sort_slice_and_upgrade() {
    let rows = vec![
        row("1001", "2026-01-01", None),
        row("1002", "2026-01-05", Some(UUID_A)),
        row("1003", "2026-01-03", None),
        row("1004", "2026-01-04", None),
        row("1005", "2026-01-02", None),
    ];
    let stub = StubHttp::new(vec![
        (200, list_body(rows)),
        (200, detail_body()),
    ]);
    let f = build(stub.clone());
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/manapool", &bearer(MEMBER_TOK), None).await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["source"], json!("manapool"));
    assert_eq!(v["offset"], json!(0));
    assert_eq!(v["total"], json!(5));
    assert_eq!(v["hasMore"], json!(true));
    let orders = v["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 3);
    // Newest first: 01-05, 01-04, 01-03.
    assert_eq!(orders[0]["number"], json!("1002"));
    assert_eq!(orders[1]["number"], json!("1004"));
    assert_eq!(orders[2]["number"], json!("1003"));
    // Summary key order is the Node literal's.
    let keys: Vec<&str> = orders[0].as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, ["number", "placedAt", "status", "cardCount", "lineCount", "total_cents"]);
    // UUID row upgraded from the detail: 2 copies of 1 real line; the
    // booster box (sealed) and the name-less line (junk) are excluded.
    assert_eq!(orders[0]["cardCount"], json!(2));
    assert_eq!(orders[0]["lineCount"], json!(1));
    // Non-uuid rows keep the list fold (sellers' item_count sum = 4).
    assert_eq!(orders[1]["cardCount"], json!(4));
    assert_eq!(orders[1]["lineCount"], json!(0));
    assert_eq!(orders[0]["placedAt"], json!("2026-01-05"));
    assert_eq!(orders[0]["total_cents"], json!(1234));
    // Whole-list URL carries NO paging params; provider headers present.
    let urls = stub.urls();
    assert_eq!(urls[0], format!("{STUB_BASE}/buyer/orders"));
    assert_eq!(urls[1], format!("{STUB_BASE}/buyer/orders/{UUID_A}"));
    let h = stub.last_headers();
    assert!(h.contains(&("X-ManaPool-Email".to_string(), "player@example.com".to_string())));
    assert!(h.contains(&("X-ManaPool-Access-Token".to_string(), "abcdefghij".to_string())));
}

#[tokio::test]
async fn recent_offset_pages_and_offset_parsing() {
    let rows = vec![
        row("1001", "2026-01-01", None),
        row("1002", "2026-01-05", None),
        row("1003", "2026-01-03", None),
        row("1004", "2026-01-04", None),
        row("1005", "2026-01-02", None),
    ];
    let stub = StubHttp::new(vec![
        (200, list_body(rows.clone())),
        (200, list_body(rows.clone())),
        (200, list_body(rows)),
    ]);
    let f = build(stub.clone());
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/manapool?offset=3", &bearer(MEMBER_TOK), None).await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["offset"], json!(3));
    assert_eq!(v["hasMore"], json!(false));
    assert_eq!(v["orders"].as_array().unwrap().len(), 2);
    assert_eq!(v["orders"][0]["number"], json!("1005"));
    // Junk offsets fold to 0; negatives clamp to 0.
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/manapool?offset=abc", &bearer(MEMBER_TOK), None).await;
    assert_eq!(
        (st, serde_json::from_str::<Value>(&body).unwrap()["offset"].clone()),
        (axum::http::StatusCode::OK, json!(0))
    );
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/manapool?offset=-5", &bearer(MEMBER_TOK), None).await;
    assert_eq!(
        (st, serde_json::from_str::<Value>(&body).unwrap()["offset"].clone()),
        (axum::http::StatusCode::OK, json!(0))
    );
}

#[tokio::test]
async fn recent_guard_arms() {
    // 401/429 from the list are AUTH problems: no list_unavailable.
    let stub = StubHttp::new(vec![(401, json!({ "message": "nope" }))]);
    let f = build(stub.clone());
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/manapool", &bearer(MEMBER_TOK), None).await;
    assert_eq!(st, 401);
    assert_eq!(
        body,
        json!({ "error": marketplace::MP_UNAUTH_MESSAGE, "list_unavailable": false }).to_string()
    );
    let stub = StubHttp::new(vec![(429, json!({}))]);
    let f = build(stub.clone());
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/manapool", &bearer(MEMBER_TOK), None).await;
    assert_eq!(st, 429);
    assert_eq!(
        body,
        json!({ "error": marketplace::MP_RATE_LIMIT_MESSAGE, "list_unavailable": false }).to_string()
    );
    // Any other non-2xx: 502 + list_unavailable.
    let stub = StubHttp::new(vec![(500, json!({}))]);
    let f = build(stub.clone());
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/manapool", &bearer(MEMBER_TOK), None).await;
    assert_eq!(st, 502);
    assert_eq!(
        body,
        json!({ "error": "ManaPool did not return an order list (status 500).", "list_unavailable": true })
            .to_string()
    );
    // A detail failure keeps the list's own counts (summary still 200).
    let stub = StubHttp::new(vec![(200, list_body(vec![row("1002", "2026-01-05", Some(UUID_A))])), (500, json!({}))]);
    let f = build(stub.clone());
    let (st, body) = rq(&f, "GET", "/api/marketplace/recent/manapool", &bearer(MEMBER_TOK), None).await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["orders"][0]["cardCount"], json!(4));
    assert_eq!(v["orders"][0]["lineCount"], json!(0));
}

#[tokio::test]
async fn unregistered_methods_and_preview_add_proxy() {
    let f = build(StubHttp::new(vec![]));
    // preview/add are native for POST (slice 2); every OTHER method on
    // them still rides the catch-all proxy (dead upstream → 502), never a
    // local 404/405. Unregistered shapes on accounts/recent proxy too.
    for (m, p) in [
        ("GET", "/api/marketplace/preview"),
        ("PUT", "/api/marketplace/preview"),
        ("GET", "/api/marketplace/add"),
        ("DELETE", "/api/marketplace/add"),
        ("POST", "/api/marketplace/accounts"),
        ("DELETE", "/api/marketplace/accounts"),
        ("POST", "/api/marketplace/recent/manapool"),
    ] {
        let (st, _b) = rq(&f, m, p, &merge(&bearer(MEMBER_TOK), &json_ct()), Some("{}")).await;
        let (st2, b2) = rq(&f, m, p, &[], None).await;
        assert_eq!(st, 502, "{m} {p} must proxy, never 404/405; got {st} / {st2} body2={b2}");
    }
    // A POST without a token hits the NATIVE auth gate (401), not the proxy.
    let (st, _b) =
        rq(&f, "POST", "/api/marketplace/preview", &json_ct(), Some("{}")).await;
    assert_eq!(st, 401);
    let (st, _b) = rq(&f, "POST", "/api/marketplace/add", &json_ct(), Some("{}")).await;
    assert_eq!(st, 401);
}

// ---------------------------------------------------------------------------
// pure folds
// ---------------------------------------------------------------------------

#[test]
fn mask_ladders() {
    assert_eq!(mask_secret(Some(&json!(""))), "");
    assert_eq!(mask_secret(Some(&json!("12345678"))), "\u{2022}\u{2022}\u{2022}\u{2022}");
    assert_eq!(mask_secret(Some(&json!("123456789"))), "123\u{2026}6789 (9 chars)");
    assert_eq!(mask_email(Some(&json!(""))), "");
    assert_eq!(mask_email(Some(&json!("at.example.com"))), "a\u{2026}"); // no '@'
    assert_eq!(mask_email(Some(&json!("ab@x.io"))), "ab\u{2022}\u{2022}@x.io");
    assert_eq!(mask_email(Some(&json!("@x.io"))), "@\u{2026}"); // indexOf('@')=0 is falsy in JS
}

#[test]
fn condition_ladder() {
    assert_eq!(map_condition(Some(&json!("nm"))), "Near Mint"); // upper hits 'NM'
    assert_eq!(map_condition(Some(&json!("Near Mint"))), "Near Mint");
    assert_eq!(map_condition(Some(&json!("damaged"))), "Near Mint"); // toUpperCase has no 'DAMAGED' key
    assert_eq!(map_condition(Some(&json!(""))), "Near Mint");
    assert_eq!(map_condition(Some(&json!("weird"))), "Near Mint");
}

#[test]
fn order_lines_fold_counts() {
    let order = detail_body()["order"].clone();
    let l = order_lines(&order, false);
    assert_eq!(l.card_copies, 2.0);
    assert_eq!(l.card_lines, 1);
    let l = order_lines(&order, true);
    assert_eq!(l.lines.len(), 3);
    assert!(l.lines[1].sealed);
    assert!(l.lines[2].junk);
    assert_eq!(l.card_copies, 2.0);
}

#[tokio::test]
async fn human_number_resolves_uuid_through_list() {
    // fetchManapoolOrder with a human number: list lookup first, detail
    // rides the row's UUID id.
    let stub = StubHttp::new(vec![
        (200, list_body(vec![json!({ "order_number": "MP-77", "id": UUID_A })])),
        (200, detail_body()),
    ]);
    let creds = marketplace::Creds { email: "e@x.io".into(), token: "t".into() };
    let r =
        marketplace::fetch_manapool_order(stub.as_ref(), &creds, "MP-77").await.unwrap();
    assert_eq!(r.status, 200);
    let urls = stub.urls();
    assert_eq!(urls.len(), 2);
    assert!(urls[1].ends_with(&format!("/buyer/orders/{UUID_A}")));
}

// ---------------------------------------------------------------------------
// Slice 2 — POST /api/marketplace/preview and /api/marketplace/add
// ---------------------------------------------------------------------------

#[path = "./mock_scryfall.rs"]
mod mock_scryfall;

use mock_scryfall::raw_card;

async fn build_scry(stub: Arc<StubHttp>) -> (Fix, Arc<mock_scryfall::MockState>) {
    stub_base_once();
    let dir = std::env::temp_dir().join(format!(
        "scrybox-mp2-it-{}",
        scrybox_server::auth_support::uuid_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bindarr.db"), []).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: "http://127.0.0.1:1".to_string(),
        static_dir: None,
        rust_routes: ["marketplace".to_string()].into_iter().collect(),
        db_path: Some(dir.join("bindarr.db")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.mp_http = stub;
    let pool = st.db.clone().unwrap();
    pool.get().unwrap().execute_batch(auth_common::schema_fixture()).unwrap();
    pool.get()
        .unwrap()
        .execute_batch(
            "INSERT INTO users (username, password_hash, role, share_token, manapool_email, manapool_token, manapool_enabled, manapool_saved_at, api_key)
             VALUES ('mpmember','h','member','sh-mp','player@example.com','abcdefghij',1,'2026-01-02T03:04:05.000Z','bnd_eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'),
                    ('mpother','h','member','sh-mp2',NULL,NULL,0,NULL,NULL);
             INSERT INTO sessions (token, user_id, expires_at)
             VALUES ('tok-mp-member-1',1,'2030-01-01 00:00:00'),
                    ('tok-mp-other-1',2,'2030-01-01 00:00:00');",
        )
        .unwrap();
    let (addr, scry) = mock_scryfall::spawn_mock().await;
    st.scry = Arc::new(mock_scryfall::scry_for(addr, 0.0));
    (Fix { router: scrybox_server::app(st), pool }, scry)
}

/// A member's collection stack (card_cache row first — the FK).
fn seed_owned(pool: &scrybox_server::db::DbPool, card_id: &str, user: i64, qty: i64) {
    let conn = pool.get().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO card_cache (id, name) VALUES (?1, 'seeded')",
        rusqlite::params![card_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO collection (card_id, quantity, user_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![card_id, qty, user],
    )
    .unwrap();
}

/// ManaPool detail body: header + 2 card lines + 1 sealed + 1 junk.
fn card_detail() -> Value {
    json!({ "order": {
        "number": "MP-9",
        "placedAt": "2026-01-05T10:00:00.000Z",
        "currency": "USD",
        "total_cents": 950,
        "items": [
            { "product": { "single": { "name": "Card A", "set": "10E", "number": "1", "scryfallId": "aaa" } },
              "quantity": 2, "unit_price_cents": 150 },
            { "product": { "single": { "name": "Card B", "set": "M19", "number": "9" } },
              "quantity": 1, "unit_price_cents": 2500 },
            { "product": { "sealed": { "name": "Draft Booster Box", "product_type": "bundle" } }, "quantity": 1 },
            { "quantity": 3 }
        ]
    } })
}

fn card_batches() -> Vec<Value> {
    vec![json!({
        "data": [raw_card("aaa", "Card A", "10e", "1", Some(1.5)),
                 raw_card("ccc", "Card B", "m19", "9", Some(25.0))],
        "not_found": []
    })]
}

fn mp_post_body(number: &str, extra: Option<&str>) -> String {
    let e = extra.map(|s| format!(",{s}")).unwrap_or_default();
    format!("{{\"source\":\"manapool\",\"order_number\":\"{number}\"{e}}}")
}

async fn post(f: &Fix, path: &str, tok: &'static str, body: &str) -> (axum::http::StatusCode, String) {
    let h = auth_common::merge(&bearer(tok), &json_ct());
    rq(f, "POST", path, &h, Some(body)).await
}

#[tokio::test]
async fn preview_body_parser_runs_before_auth() {
    // express.json is mounted before authenticate: malformed JSON with NO
    // token answers the generic 500 (never 401), and an oversize body 413s
    // without ever reaching the credential lookup.
    let f = build(StubHttp::new(vec![]));
    let (st, body) = rq(&f, "POST", "/api/marketplace/preview", &json_ct(), Some("{")).await;
    assert_eq!(st, 500);
    assert!(body.contains("Internal server error"), "{body}");
    let h = auth_common::merge(&bearer(MEMBER_TOK), &json_ct());
    let (st, _b) = rq(
        &f,
        "POST",
        "/api/marketplace/add",
        &h,
        Some(&format!("{{\"x\":\"{}\"}}", "y".repeat(2 * 1024 * 1024))),
    )
    .await;
    assert_eq!(st, 413);
}

#[tokio::test]
async fn preview_gate_order_no_network() {
    let stub = StubHttp::new(vec![]);
    let f = build(stub.clone());
    // no token -> 401 (auth before the source gate).
    let (st, _) = rq(&f, "POST", "/api/marketplace/preview", &json_ct(), Some("{}")).await;
    assert_eq!(st, 401);
    // API keys are GET-only on this router.
    let (st, _) =
        post(&f, "/api/marketplace/preview", KEY, &mp_post_body("MP-1", None)).await;
    assert_eq!(st, 403);
    // source gate first, then shape, then credentials.
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, "{\"source\":\"tcgplayer\",\"order_number\":\"MP-1\"}").await;
    assert_eq!(st, 400);
    assert_eq!(b, json!({ "error": "source must be one of: manapool" }).to_string());
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, "{\"source\":\"manapool\"}").await;
    assert_eq!(st, 400);
    assert_eq!(b, json!({ "error": "order_number is required" }).to_string());
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, "{\"source\":\"manapool\",\"order_number\":\"#12\"}").await;
    assert_eq!(st, 400);
    assert_eq!(b, json!({ "error": "order_number is required" }).to_string());
    // Disabled/absent credentials -> the not-configured arm.
    let (st, b) = post(&f, "/api/marketplace/preview", OTHER_TOK, &mp_post_body("MP-1", None)).await;
    assert_eq!(st, 400);
    assert_eq!(b, json!({ "error": "ManaPool is not configured or is turned off" }).to_string());
    assert!(stub.urls().is_empty(), "no provider call before the gates pass");
}

#[tokio::test]
async fn preview_provider_error_arms() {
    // 401 -> the saved-credentials message, status echoed.
    let stub = StubHttp::new(vec![(401, json!({}))]);
    let f = build(stub.clone());
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, &mp_post_body(UUID_A, None)).await;
    assert_eq!(st, 401);
    assert_eq!(b, json!({ "error": marketplace::MP_UNAUTH_MESSAGE }).to_string());
    // 404 (human number: list leg first, then the id-less detail).
    let stub = StubHttp::new(vec![
        (200, list_body(vec![row("MP-99", "2026-01-01T00:00:00Z", None)])),
        (404, json!({ "message": "gone" })),
    ]);
    let f = build(stub.clone());
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, &mp_post_body("MP-77", None)).await;
    assert_eq!(st, 404);
    let v: Value = serde_json::from_str(&b).unwrap();
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .starts_with("ManaPool has no order MP-77 for this account."),
        "{b}"
    );
    // 422 from parseOrderPayload carries observedKeys.
    let stub = StubHttp::new(vec![(200, json!({ "foo": 1, "bar": "x" }))]);
    let f = build(stub);
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, &mp_post_body(UUID_A, None)).await;
    assert_eq!(st, 422);
    assert_eq!(
        b,
        json!({ "error": "The order response did not include any usable line items.",
                "observedKeys": ["foo", "bar"] })
            .to_string()
    );
}

#[tokio::test]
async fn preview_ok_byte_exact_with_owned_and_extras() {
    let stub = StubHttp::new(vec![(200, card_detail())]);
    let (f, scry) = build_scry(stub.clone()).await;
    *scry.collection_batches.lock().unwrap() = card_batches();
    seed_owned(&f.pool, "mtg-aaa", 1, 3);
    seed_owned(&f.pool, "mtg-ccc", 2, 9); // other user — must not leak
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, &mp_post_body(UUID_A, None)).await;
    assert_eq!(st, 200);
    assert_eq!(
        b,
        json!({
            "number": "MP-9",
            "placedAt": "2026-01-05T10:00:00.000Z",
            "status": null,
            "currency": "USD",
            "total_cents": 950,
            "lines": [
                { "name": "Card A", "set_code": "10e", "number": "1", "scryfall_id": "aaa",
                  "tcgplayer_product_id": null, "quantity": 2, "price_cents": 150,
                  "is_foil": false, "condition": "Near Mint", "year": null,
                  "sealed": false, "junk": false },
                { "name": "Card B", "set_code": "m19", "number": "9", "scryfall_id": null,
                  "tcgplayer_product_id": null, "quantity": 1, "price_cents": 2500,
                  "is_foil": false, "condition": "Near Mint", "year": null,
                  "sealed": false, "junk": false }
            ],
            "lineCount": 2,
            "cards": [
                { "card_id": "mtg-aaa", "name": "Card A",
                  "image_url": "https://cards.example/aaa.png", "set_code": "10e",
                  "number": "1", "quantity": 2, "is_foil": false, "condition": "Near Mint",
                  "price_cents": 150, "matched": true, "owned": 3 },
                { "card_id": "mtg-ccc", "name": "Card B",
                  "image_url": "https://cards.example/ccc.png", "set_code": "m19",
                  "number": "9", "quantity": 1, "is_foil": false, "condition": "Near Mint",
                  "price_cents": 2500, "matched": true, "owned": 0 }
            ],
            "totalListed": 4,
            "totalCopies": 3,
            "resolvedCount": 2,
            "unresolved": 0,
            "extras": 2,
            "sealedCount": 2,
            "extrasIncluded": false,
            "price_cents": 2800
        })
        .to_string()
    );
    // One provider detail call (UUID skips the list leg), one resolver batch.
    assert_eq!(stub.urls().len(), 1);
    assert_eq!(scry.hits_for("POST /cards/collection").len(), 1);
    // Non-UUID scryfall ids drop to the set/number identifier arm; the
    // _line back-pointer never leaves Rust.
    let body = &scry.hits_for("POST /cards/collection")[0].1;
    assert!(body.contains("\"set\":\"10e\"") && !body.contains("_line"), "{body}");
    assert!(!body.contains("aaa") && !body.contains("ccc"), "{body}");
}

#[tokio::test]
async fn preview_include_extras_keeps_lines_in_parsed() {
    let stub = StubHttp::new(vec![(200, card_detail())]);
    let (f, scry) = build_scry(stub).await;
    *scry.collection_batches.lock().unwrap() = card_batches();
    let (st, b) = post(
        &f,
        "/api/marketplace/preview",
        MEMBER_TOK,
        &mp_post_body(UUID_A, Some("\"include_extras\":true")),
    )
    .await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["lineCount"], 4);
    assert_eq!(v["extrasIncluded"], true);
    assert_eq!(v["extras"], 2);
    assert_eq!(v["totalListed"], 4);
    // Sealed/junk never reach the resolver or the cards fold.
    assert_eq!(v["resolvedCount"], 2);
    assert_eq!(v["cards"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn preview_early_answer_omits_totals_when_no_lines() {
    // A sealed-only order: orderLines yields nothing, and Node's early
    // return deliberately omits totalCopies / extrasIncluded / price_cents.
    let order = json!({ "order": { "number": "MP-1", "items": [
        { "product": { "sealed": { "name": "Draft Booster Box", "product_type": "bundle" } }, "quantity": 1 }
    ] } });
    let stub = StubHttp::new(vec![(200, order)]);
    let (f, scry) = build_scry(stub).await;
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, &mp_post_body(UUID_A, None)).await;
    assert_eq!(st, 200);
    assert_eq!(
        b,
        json!({ "number": "MP-1", "placedAt": null, "status": null, "currency": null,
                "total_cents": null, "lines": [], "lineCount": 0,
                "cards": [], "totalListed": 0, "resolvedCount": 0, "unresolved": 0,
                "extras": 0, "sealedCount": 0 })
            .to_string()
    );
    assert_eq!(scry.hits_for("POST /cards/collection").len(), 0);
}

#[tokio::test]
async fn add_ok_files_stacks_with_copies_and_prices() {
    let stub = StubHttp::new(vec![(200, card_detail())]);
    let (f, scry) = build_scry(stub).await;
    *scry.collection_batches.lock().unwrap() = card_batches();
    let (st, b) = post(
        &f,
        "/api/marketplace/add",
        MEMBER_TOK,
        &mp_post_body(UUID_A, Some("\"copies\":2")),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(
        b,
        json!({
            "order": { "number": "MP-9", "lineCount": 2 },
            "added": 6,
            "resolved": 2,
            "totalListed": 4,
            "unresolved": 0,
            "failed": [],
            "message": "Added 6 cards from order MP-9."
        })
        .to_string()
    );
    let conn = f.pool.get().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT card_id, quantity, condition, printing, purchase_price, language
               FROM collection ORDER BY id",
        )
        .unwrap();
    let rows: Vec<(String, i64, String, String, f64, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap()
        .map(|x| x.unwrap())
        .collect();
    // copies=2 doubles each line's quantity; per-line mapCondition grade
    // always beats the body-level condition (mapCondition never returns a
    // non-whitelist value); stackable collapses each card into one row;
    // prices ride from unit_price_cents / 100 through toFixed(2).
    assert_eq!(
        rows,
        vec![
            ("mtg-aaa".into(), 4, "Near Mint".into(), "Normal".into(), 1.5, "English".into()),
            ("mtg-ccc".into(), 2, "Near Mint".into(), "Normal".into(), 25.0, "English".into()),
        ]
    );
}

#[tokio::test]
async fn add_printing_mode_and_language_override() {
    let stub = StubHttp::new(vec![(200, card_detail())]);
    let (f, scry) = build_scry(stub).await;
    *scry.collection_batches.lock().unwrap() = card_batches();
    // Localized printings for the German pass (plan_add's printingInLanguage
    // leg). Node files these under the LOCALIZED card_id while qtyById stays
    // keyed by the requested id — the Map misses and each row counts a
    // single copy. Faithful quirk, pinned here.
    f.pool.get().unwrap().execute_batch(
        "INSERT INTO card_cache (id, name, set_id, number, language) VALUES
            ('aaa-de','Karte A','10e','1','German'),
            ('ccc-de','Karte B','m19','9','German');",
    )
    .unwrap();
    let (st, b) = post(
        &f,
        "/api/marketplace/add",
        MEMBER_TOK,
        &mp_post_body(UUID_A, Some("\"printing_mode\":\"foil\",\"language\":\"German\"")),
    )
    .await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["added"], 2, "localized filing counts 1 copy per row (Node qtyById miss): {b}");
    assert_eq!(v["message"], "Added 2 cards from order MP-9.");
    let conn = f.pool.get().unwrap();
    let rows: Vec<(String, i64, String, String)> = conn
        .prepare("SELECT card_id, quantity, printing, language FROM collection ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|x| x.unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![
            ("aaa-de".into(), 2, "Holofoil".into(), "German".into()),
            ("ccc-de".into(), 1, "Holofoil".into(), "German".into()),
        ]
    );
}

#[tokio::test]
async fn add_nothing_to_add_message() {
    let order = json!({ "order": { "number": "MP-1", "items": [
        { "product": { "sealed": { "name": "Draft Booster Box", "product_type": "bundle" } }, "quantity": 1 },
        { "quantity": 2 }
    ] } });
    let stub = StubHttp::new(vec![(200, order)]);
    let (f, scry) = build_scry(stub).await;
    let (st, b) = post(&f, "/api/marketplace/add", MEMBER_TOK, &mp_post_body(UUID_A, None)).await;
    assert_eq!(st, 200);
    assert_eq!(
        b,
        json!({
            "order": { "number": "MP-1", "lineCount": 0 },
            "added": 0,
            "resolved": 0,
            "totalListed": 0,
            "unresolved": 0,
            "failed": [],
            "message": "Nothing to add \u{2014} no cards in the order resolved to catalogue cards."
        })
        .to_string()
    );
    assert_eq!(scry.hits_for("POST /cards/collection").len(), 0);
    let n: i64 =
        f.pool.get().unwrap().query_row("SELECT COUNT(*) FROM collection", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn add_numeric_scryfall_id_is_the_node_typeerror() {
    // addOrderToCollection's key builder calls .toLowerCase() straight on a
    // truthy scryfall_id — a NUMBER id throws in V8; Node's route catch
    // answers 500 with the raw TypeError message. previewOrder wraps
    // String() first and keeps working.
    let order = json!({ "order": { "number": "MP-1", "currency": "USD", "items": [
        { "product": { "single": { "name": "Card A", "set": "10E", "number": "1", "scryfallId": 123 } },
          "quantity": 1, "unit_price_cents": 150 }
    ] } });
    let stub = StubHttp::new(vec![(200, order.clone()), (200, order)]);
    let (f, scry) = build_scry(stub).await;
    *scry.collection_batches.lock().unwrap() =
        vec![json!({ "data": [raw_card("123", "Card A", "10e", "1", Some(1.5))], "not_found": [] })];
    let (st, b) = post(&f, "/api/marketplace/add", MEMBER_TOK, &mp_post_body(UUID_A, None)).await;
    assert_eq!(st, 500);
    assert_eq!(
        b,
        json!({ "error": "l.scryfall_id.toLowerCase is not a function" }).to_string()
    );
    // The throw happens BEFORE the resolver is touched.
    assert_eq!(scry.hits_for("POST /cards/collection").len(), 0);
    // Preview rides String(scryfall_id) and resolves fine.
    let (st, b) = post(&f, "/api/marketplace/preview", MEMBER_TOK, &mp_post_body(UUID_A, None)).await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["cards"][0]["card_id"], "mtg-123");
    assert_eq!(v["resolvedCount"], 1);
}

#[tokio::test]
async fn parse_and_price_folds_units() {
    // toFixed(2) rounds half away from zero on the exact double (0.125 ->
    // "0.13"); {:.2} would give 0.12 and the stored REAL would diverge.
    assert_eq!(marketplace::js_tofixed2_number(12.5 / 100.0), 0.13);
    assert_eq!(marketplace::js_tofixed2_number(150.0 / 100.0), 1.5);
    assert_eq!(marketplace::js_tofixed2_number(2500.0 / 100.0), 25.0);
    assert!(marketplace::js_tofixed2_number(f64::NAN).is_nan());

    // parseOrderPayload: direct body, wantNumber fallback, payment fallback.
    let body = json!({
        "items": [ { "product": { "single": { "name": "X" } }, "quantity": 1 } ],
        "payment": { "total_cents": 4200 }
    });
    let p = marketplace::parse_order_payload(&body, "  MP-3 ", false).unwrap();
    assert_eq!(p.number, json!("MP-3"));
    assert_eq!(p.total_cents, json!(4200));
    assert_eq!(p.placed_at, Value::Null);
    assert_eq!(p.currency, Value::Null);
    assert_eq!(p.lines.lines.len(), 1);
    // list form picks by orderNumberMatches, else head when want is blank.
    let list = json!({ "orders": [
        { "orderNumber": "ORDER-4", "items": [ { "product": { "single": { "name": "Y" } } } ] },
        { "orderNumber": "5", "items": [ { "product": { "single": { "name": "Z" } } } ] }
    ] });
    let p = marketplace::parse_order_payload(&list, "#4", false).unwrap();
    assert_eq!(p.number, json!("ORDER-4"));
    let p = marketplace::parse_order_payload(&list, "", false).unwrap();
    assert_eq!(p.number, json!("ORDER-4"));
    // no order found -> 422 + observedKeys.
    let e = marketplace::parse_order_payload(&json!({ "orders": [] }), "MP-1", false)
        .unwrap_err();
    assert_eq!(e.status, Some(422));
    assert_eq!(e.observed_keys.unwrap(), vec![Value::String("orders".into())]);
}
