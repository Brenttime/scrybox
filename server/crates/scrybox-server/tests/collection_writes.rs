//! In-process tests for the native collection WRITE routes
//! (src/collection_writes.rs): GET /api/collection/cardlist,
//! POST /api/collection/bulk, PUT/DELETE /api/collection/:id.
//!
//! Every expected status/body/message below was taken from
//! backend/src/routes/collection.js (and its helpers collectionHelpers.js,
//! deckRules.js, splitPrice.js, cardIdentity.js, shared/cardListText.js) —
//! the Node source is the parity reference. Harness mirrors tests/collection.rs.

use std::{collections::BTreeSet, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderValue, Method, StatusCode},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tower::ServiceExt;

use scrybox_server::{app, AppState, Config};

const SCHEMA_FIXTURE: &str = include_str!("schema_fixture.sql");

async fn send(
    db_path: PathBuf,
    routes: &[&str],
    upstream: &str,
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    let mut rb = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        rb = rb.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => rb
            .header(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => rb.body(Body::empty()).unwrap(),
    };
    let res = app(AppState::new(cfg)).oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    (parts.status, parts.headers, axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec())
}

/// Call expecting a Node-exact res.json body: application/json; charset=utf-8.
async fn call_json(
    db_path: PathBuf,
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let (s, h, b) = send(db_path, &["collection_writes"], "http://127.0.0.1:1", method, path, token, body).await;
    assert_eq!(
        h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json; charset=utf-8"),
        "content-type for {path} (body: {})",
        String::from_utf8_lossy(&b)
    );
    (s, serde_json::from_slice(&b).unwrap_or_else(|e| panic!("json {path}: {e}: {}", String::from_utf8_lossy(&b))))
}

async fn put(state_db: PathBuf, path: &str, body: &str) -> (StatusCode, Value) {
    call_json(state_db, Method::PUT, path, Some("tok-session"), Some(body)).await
}

async fn bulk(state_db: PathBuf, body: &str) -> (StatusCode, Value) {
    call_json(state_db, Method::POST, "/api/collection/bulk", Some("tok-session"), Some(body)).await
}

fn open(db_path: &PathBuf) -> Connection {
    // Fixture-side connection only (asserts + seed surgery). FK enforcement
    // is relaxed here so tests can simulate orphaned collection rows the
    // same way the live DB ends up with them (card_cache rows removed after
    // the fact). The server's own pool connection keeps FK ON — unaffected.
    let conn = Connection::open(db_path).expect("open");
    conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    conn
}

fn scalar(db_path: &PathBuf, sql: &str) -> i64 {
    open(db_path)
        .query_row(sql, rusqlite::params![], |r| Ok(r.get::<_, i64>(0)?))
        .unwrap_or(-1)
}

fn text(db_path: &PathBuf, sql: &str) -> String {
    open(db_path)
        .query_row(sql, rusqlite::params![], |r| Ok(r.get::<_, Option<String>>(0).ok().flatten()))
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn real(db_path: &PathBuf, sql: &str) -> Option<f64> {
    open(db_path)
        .query_row(sql, rusqlite::params![], |r| Ok(r.get::<_, Option<f64>>(0).ok().flatten()))
        .ok()
        .flatten()
}

fn exec(db_path: &PathBuf, sql: &str) {
    open(db_path).execute_batch(sql).expect("exec");
}

/// Seeded DB (same shape as tests/collection.rs) plus the fixtures the write
/// routes need. Sessions: tok-session (user 1), tok-other (user 2),
/// tok-empty (user 3); api key tok-api on user 1.
fn seeded_db(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("bindarr.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    // Same stubs as tests/collection.rs so the prod revision triggers apply.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(SCHEMA_FIXTURE).unwrap();
    conn.execute_batch(
        "
        INSERT INTO users (id, username, password_hash, role, share_token)
          VALUES (1, 'admin', 'x', 'admin', 'share-admin'),
                 (2, 'brenttime', 'x', 'member', 'share-brenttime'),
                 (3, 'empty', 'x', 'member', 'share-empty');
        UPDATE users SET api_key = 'tok-api' WHERE id = 1;
        INSERT INTO sessions (token, user_id, expires_at)
          VALUES ('tok-session', 1, '2999-01-01'),
                 ('tok-other', 2, '2999-01-01'),
                 ('tok-empty', 3, '2999-01-01');

        INSERT INTO card_cache (id, name, supertype, subtypes, set_id, number, price_trend)
          VALUES ('c1', 'Lightning Bolt', 'Instant', '[\"Instant\"]', 'JUD', '124', 45.0),
                 ('c1b', 'Lightning Bolt', 'Instant', '[\"Instant\"]', 'M10', '77', 40.0),
                 ('c2', 'Counterspell', 'Instant', '[\"Instant\"]', 'M10', '42', 4.0),
                 ('c3', 'Ancestral Recall', 'Instant', '[\"Instant\"]', NULL, NULL, 1.0),
                 ('c4', 'Plains', 'Land', '[\"Basic\",\"Land\"]', 'JUD', '1', 0.25);

        INSERT INTO collection
          (id, card_id, user_id, quantity, condition, printing, language, purchase_price, favorite, is_trade, added_at, notes, is_proxy)
          VALUES
            (101, 'c1', 1, 4, 'Near Mint', 'Normal', 'English', NULL, 0, 0, '2026-01-02 00:00:00', '', 0),
            (102, 'c2', 1, 1, 'Near Mint', 'Normal', 'English', NULL, 0, 0, '2026-01-01 00:00:00', '', 0),
            (103, 'c3', 1, 1, 'Near Mint', 'Normal', 'English', NULL, 0, 0, '2026-01-01 00:00:00', '', 0),
            (104, 'c4', 1, 1, 'Near Mint', 'Normal', 'English', NULL, 0, 0, '2026-01-01 00:00:00', '', 0),
            (201, 'c2', 2, 1, 'Near Mint', 'Normal', 'English', NULL, 0, 0, '2026-01-01 00:00:00', '', 0);
        INSERT INTO decks (id, user_id, name, checked_out) VALUES (501, 1, 'Deck', 0);
        ",
    )
    .unwrap();
    path
}

fn db() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    (dir, path)
}

// ---------------------------------------------------------------------------
// GET /api/collection/cardlist
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cardlist_plain_exact() {
    let (_d, db) = db();
    let (s, h, b) =
        send(db, &["collection_writes"], "http://127.0.0.1:1", Method::GET, "/api/collection/cardlist", Some("tok-session"), None)
            .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
        "text/plain; charset=utf-8"
    );
    assert!(h.get(header::ETAG).is_some());
    // ORDER BY c.added_at DESC, c.id DESC.
    assert_eq!(String::from_utf8(b).unwrap(), "4 Lightning Bolt\n1 Plains\n1 Ancestral Recall\n1 Counterspell");
}

#[tokio::test]
async fn cardlist_detailed_exact() {
    let (_d, db) = db();
    let (s, _h, b) = send(
        db,
        &["collection_writes"],
        "http://127.0.0.1:1",
        Method::GET,
        "/api/collection/cardlist?style=detailed",
        Some("tok-session"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        String::from_utf8(b).unwrap(),
        "4 Lightning Bolt (JUD) 124\n1 Plains (JUD) 1\n1 Ancestral Recall\n1 Counterspell (M10) 42"
    );
}

#[tokio::test]
async fn cardlist_empty_is_empty_string() {
    let (_d, db) = db();
    let (s, h, b) = send(
        db,
        &["collection_writes"],
        "http://127.0.0.1:1",
        Method::GET,
        "/api/collection/cardlist",
        Some("tok-empty"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(), "text/plain; charset=utf-8");
    assert!(b.is_empty());
}

#[tokio::test]
async fn cardlist_422_when_unresolved() {
    let (_d, db) = db();
    // FK enforcement is ON (like Node's db.js), so simulate an orphan by
    // deleting the cached card after linking a collection row to it.
    exec(
        &db,
        "INSERT INTO card_cache (id, name) VALUES ('ghost', 'Ghost Card');
         INSERT INTO collection (card_id, user_id, quantity) VALUES ('ghost', 3, 1);
         DELETE FROM card_cache WHERE id = 'ghost';",
    );
    let (s, body) =
        call_json(db, Method::GET, "/api/collection/cardlist", Some("tok-empty"), None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "The collection contains cards whose details are unavailable. Repair their metadata before exporting."
    );
}

#[tokio::test]
async fn cardlist_401_no_token() {
    let (_d, db) = db();
    let (s, body) = call_json(db, Method::GET, "/api/collection/cardlist", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "Access denied. No token provided.");
}

// ---------------------------------------------------------------------------
// PUT /api/collection/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_404_unknown_and_other_user() {
    let (_d, db) = db();
    let (s, body) = put(db.clone(), "/api/collection/999999", "{}").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Collection entry not found");

    // Entry 201 belongs to user 2 — invisible to user 1.
    let (s, body) = put(db, "/api/collection/201", "{}").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Collection entry not found");
}

#[tokio::test]
async fn put_printing_holofoil_success() {
    let (_d, db) = db();
    let (s, body) = put(db.clone(), "/api/collection/104", r#"{"printing":"Holofoil"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Collection entry updated successfully" }));
    assert_eq!(text(&db, "SELECT printing FROM collection WHERE id = 104"), "Holofoil");
}

#[tokio::test]
async fn put_printing_invalid_400() {
    let (_d, db) = db();
    let (s, body) = put(db.clone(), "/api/collection/104", r#"{"printing":"Gold"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // collection.js:730 — `Invalid printing: ${printing}`.
    assert_eq!(body["error"], "Invalid printing: Gold");
    assert_eq!(text(&db, "SELECT printing FROM collection WHERE id = 104"), "Normal");
}

#[tokio::test]
async fn put_card_id_change_same_name_ok_different_name_400() {
    let (_d, db) = db();
    // Same canonical name (c1 -> c1b, both 'Lightning Bolt'): allowed.
    let (s, body) = put(db.clone(), "/api/collection/101", r#"{"card_id":"c1b"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["message"], "Collection entry updated successfully");
    assert_eq!(text(&db, "SELECT card_id FROM collection WHERE id = 101"), "c1b");

    // Different card name: rejected verbatim.
    let (s, body) = put(db.clone(), "/api/collection/102", r#"{"card_id":"c1"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "A printing change must stay on the same card.");
    assert_eq!(text(&db, "SELECT card_id FROM collection WHERE id = 102"), "c2");
}

#[tokio::test]
async fn put_card_id_cache_miss_forwards_to_node() {
    // Target printing NOT in card_cache — Node would hydrate from Scryfall
    // (cardApi.js), Rust must not. node_upstream points at an unroutable port,
    // so the proxy's 502 proves the forward attempt fired; native handling
    // would have answered 404/400/500 directly.
    let (_d, db) = db();
    let (s, _h, b) = send(
        db.clone(),
        &["collection_writes"],
        "http://127.0.0.1:9",
        Method::PUT,
        "/api/collection/101",
        Some("tok-session"),
        Some(r#"{"card_id":"not-in-cache-404"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "expected the proxy forward to fail: {}", String::from_utf8_lossy(&b));
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["error"], "Upstream unavailable");
    // The native path must NOT have touched the row.
    assert_eq!(text(&db, "SELECT card_id FROM collection WHERE id = 101"), "c1");
}

#[tokio::test]
async fn put_quantity_grow_inserts_single_copy_rows() {
    let (_d, db) = db();
    let (s, body) = put(db.clone(), "/api/collection/102", r#"{"quantity":3}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["message"], "Collection entry updated successfully");
    // Stack reconciled to 3 copies, every row a single copy.
    assert_eq!(
        scalar(&db, "SELECT COALESCE(SUM(quantity),0) FROM collection WHERE card_id='c2' AND user_id=1"),
        3
    );
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM collection WHERE card_id='c2' AND user_id=1 AND quantity <> 1"),
        0
    );
}

#[tokio::test]
async fn put_quantity_shrink_trims_siblings_then_row() {
    let (_d, db) = db();
    // Same physical stack as 102 (c2 / Near Mint / Normal / English / !proxy).
    exec(
        &db,
        "INSERT INTO collection (id, card_id, user_id, quantity, condition, printing, language, notes, is_proxy)
         VALUES (111, 'c2', 1, 2, 'Near Mint', 'Normal', 'English', '', 0),
                (112, 'c2', 1, 1, 'Near Mint', 'Normal', 'English', '', 0);",
    );
    // Stack total 1 + 2 + 1 = 4 → target 2. Siblings trim newest-first
    // (ORDER BY id DESC), the edited row is only touched last.
    let (s, body) = put(db.clone(), "/api/collection/102", r#"{"quantity":2}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["message"], "Collection entry updated successfully");
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM collection WHERE id = 112"), 0);
    assert_eq!(scalar(&db, "SELECT quantity FROM collection WHERE id = 111"), 1);
    assert_eq!(scalar(&db, "SELECT quantity FROM collection WHERE id = 102"), 1);
    assert_eq!(
        scalar(&db, "SELECT COALESCE(SUM(quantity),0) FROM collection WHERE card_id='c2' AND user_id=1"),
        2
    );

    // Trim all the way to 1 → edited row keeps one copy, all siblings gone.
    exec(
        &db,
        "INSERT INTO collection (id, card_id, user_id, quantity, condition, printing, language, notes, is_proxy)
         VALUES (113, 'c2', 1, 1, 'Near Mint', 'Normal', 'English', '', 0);",
    );
    let (s, _b) = put(db.clone(), "/api/collection/102", r#"{"quantity":1}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM collection WHERE card_id='c2' AND user_id=1"),
        1
    );
    assert_eq!(scalar(&db, "SELECT quantity FROM collection WHERE id = 102"), 1);
}

#[tokio::test]
async fn put_quantity_ignores_zero_rows_and_grows_to_target() {
    let (_d, db) = db();
    // A legacy zero-quantity row shares the stack key but is excluded from
    // sibling reconciliation (quantity > 0) and never deleted.
    exec(
        &db,
        "INSERT INTO collection (id, card_id, user_id, quantity, condition, printing, language, notes, is_proxy)
         VALUES (113, 'c2', 1, 0, 'Near Mint', 'Normal', 'English', '', 0);",
    );
    let (s, _b) = put(db.clone(), "/api/collection/102", r#"{"quantity":4}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        scalar(&db, "SELECT COALESCE(SUM(quantity),0) FROM collection WHERE card_id='c2' AND user_id=1"),
        4
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM collection WHERE id = 113"), 1);
}

#[tokio::test]
async fn put_quantity_zero_floors_at_one_never_deletes() {
    // Node: Math.max(1, parseInt(quantity,10) || 1) — 0/NaN/garbage → 1.
    // A PUT can never delete a row via quantity (brief said 0 deletes; the
    // Node source wins — recorded as a brief deviation).
    let (_d, db) = db();
    let (s, body) = put(db.clone(), "/api/collection/102", r#"{"quantity":0}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["message"], "Collection entry updated successfully");
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM collection WHERE id = 102"), 1);
    assert_eq!(scalar(&db, "SELECT quantity FROM collection WHERE id = 102"), 1);

    let (s, _b) = put(db.clone(), "/api/collection/102", r#"{"quantity":"abc"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(scalar(&db, "SELECT quantity FROM collection WHERE id = 102"), 1);
}

#[tokio::test]
async fn put_trade_flip_and_notes() {
    let (_d, db) = db();
    let (s, body) = put(db.clone(), "/api/collection/101", r#"{"is_trade":true}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["message"], "Collection entry updated successfully");
    assert_eq!(scalar(&db, "SELECT is_trade FROM collection WHERE id = 101"), 1);

    let (s, _b) = put(db.clone(), "/api/collection/101", r#"{"is_trade":0}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(scalar(&db, "SELECT is_trade FROM collection WHERE id = 101"), 0);

    let (s, _b) = put(db.clone(), "/api/collection/101", r#"{"notes":"playtested"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(text(&db, "SELECT notes FROM collection WHERE id = 101"), "playtested");
}

#[tokio::test]
async fn put_allocation_conflict_409() {
    let (_d, db) = db();
    // User 2 owns 2 copies of Counterspell (201 + 202); a checked-out deck
    // locks 2 → reducing the stack to 1 would drop owned below locked.
    exec(
        &db,
        "INSERT INTO collection (id, card_id, user_id, quantity, condition, printing, language, notes, is_proxy)
         VALUES (202, 'c2', 2, 1, 'Near Mint', 'Normal', 'English', '', 0);
         INSERT INTO decks (id, user_id, name, checked_out) VALUES (502, 2, 'Locked', 1);
         INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (502, 'c2', 2);",
    );
    let (s, body) =
        call_json(db.clone(), Method::PUT, "/api/collection/201", Some("tok-other"), Some(r#"{"quantity":1}"#))
            .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(
        body["error"],
        "Check in the deck using this card before reducing the owned quantity."
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM collection WHERE user_id = 2 AND card_id = 'c2'"), 2);
}

#[tokio::test]
async fn put_api_key_write_403() {
    let (_d, db) = db();
    let (s, body) =
        call_json(db, Method::PUT, "/api/collection/101", Some("tok-api"), Some(r#"{"notes":"x"}"#)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "API keys are read-only. Use a login session for this request.");
}

#[tokio::test]
async fn put_unauthenticated_401() {
    let (_d, db) = db();
    let (s, body) = call_json(db, Method::PUT, "/api/collection/101", None, Some("{}")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "Access denied. No token provided.");
}

// ---------------------------------------------------------------------------
// DELETE /api/collection/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_200_404_409() {
    let (_d, db) = db();
    // 404 unknown.
    let (s, body) =
        call_json(db.clone(), Method::DELETE, "/api/collection/999999", Some("tok-session"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Collection entry not found");

    // 404 other user's entry.
    let (s, _b) =
        call_json(db.clone(), Method::DELETE, "/api/collection/201", Some("tok-session"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // 409 while a checked-out deck locks the card.
    exec(
        &db,
        "INSERT INTO decks (id, user_id, name, checked_out) VALUES (502, 2, 'Locked', 1);
         INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (502, 'c2', 1);",
    );
    let (s, body) =
        call_json(db.clone(), Method::DELETE, "/api/collection/201", Some("tok-other"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(body["error"], "Check in the deck using this card before removing it.");
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM collection WHERE id = 201"), 1);

    // 404 without a token.
    let (s, body) = call_json(db.clone(), Method::DELETE, "/api/collection/101", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "Access denied. No token provided.");

    // 403 via API key.
    let (s, body) =
        call_json(db.clone(), Method::DELETE, "/api/collection/101", Some("tok-api"), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "API keys are read-only. Use a login session for this request.");

    // 200 happy path (nothing locks Ancestral Recall).
    let (s, body) =
        call_json(db.clone(), Method::DELETE, "/api/collection/103", Some("tok-session"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Card removed from collection" }));
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM collection WHERE id = 103"), 0);
}

// ---------------------------------------------------------------------------
// POST /api/collection/bulk
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_validates_entry_ids_action_and_value() {
    let (_d, db) = db();
    let (s, body) = bulk(db.clone(), "{}").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "entry_ids is required");

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[],"action":"delete"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "entry_ids is required");

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":"nope","action":"delete"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "entry_ids is required");

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"zap"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid action");

    // 'favorite' is not in BULK_ACTIONS (collection.js:852).
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"favorite"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid action");

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":["abc"],"action":"delete"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "No valid entry_ids");

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"condition","value":"Mint"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid condition");

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"printing","value":"Foil"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid printing");

    let (s, body) = bulk(
        db.clone(),
        r#"{"entry_ids":[101],"action":"purchase_split","value":{"total":-5}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "total must be a non-negative number");

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"purchase_split"}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "total must be a non-negative number");

    // Unauthenticated bulk → 401 (auth runs before validation).
    let (s, body) =
        call_json(db.clone(), Method::POST, "/api/collection/bulk", None, Some("{}")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "Access denied. No token provided.");

    // API key write → 403.
    let (s, body) = call_json(
        db,
        Method::POST,
        "/api/collection/bulk",
        Some("tok-api"),
        Some(r#"{"entry_ids":[101],"action":"trade"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "API keys are read-only. Use a login session for this request.");
}

#[tokio::test]
async fn bulk_delete_trade_condition_printing_counts_and_messages() {
    let (_d, db) = db();
    exec(
        &db,
        "INSERT INTO collection (id, card_id, user_id, quantity) VALUES
           (301, 'c1', 1, 1), (302, 'c2', 1, 1), (303, 'c3', 1, 1), (304, 'c4', 1, 1);",
    );

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[301,302],"action":"delete"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Deleted 2 card(s)", "affected": 2 }));
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM collection WHERE id IN (301,302)"), 0);

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[303,304],"action":"trade"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Updated 2 card(s)", "affected": 2 }));
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM collection WHERE is_trade=1 AND id IN (303,304)"),
        2
    );

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[303],"action":"untrade"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Updated 1 card(s)", "affected": 1 }));
    assert_eq!(scalar(&db, "SELECT is_trade FROM collection WHERE id = 303"), 0);

    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[303],"action":"condition","value":"Damaged"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Set condition on 1 card(s)", "affected": 1 }));
    assert_eq!(text(&db, "SELECT condition FROM collection WHERE id = 303"), "Damaged");

    let (s, body) =
        bulk(db.clone(), r#"{"entry_ids":[303,304],"action":"printing","value":"Holofoil"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Set printing on 2 card(s)", "affected": 2 }));
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM collection WHERE printing='Holofoil' AND id IN (303,304)"),
        2
    );
}

#[tokio::test]
async fn bulk_delete_conflicts_with_checked_out_deck() {
    let (_d, db) = db();
    // User 1's deck 501 goes checked-out locking all 4 Bolts; deleting any
    // Bolt row takes owned below locked.
    exec(
        &db,
        "UPDATE decks SET checked_out = 1 WHERE id = 501;
         INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (501, 'c1', 4);",
    );
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"delete"}"#).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(body["error"], "Check in the deck using the selected card before removing it.");
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM collection WHERE id = 101"), 1);
}

#[tokio::test]
async fn bulk_purchase_split_weighted_and_equal() {
    let (_d, db) = db();
    exec(
        &db,
        "INSERT INTO collection (id, card_id, user_id, quantity) VALUES
           (311, 'c1', 1, 1), (312, 'c2', 1, 1), (313, 'c3', 1, 1);",
    );
    // Market 45/4/1 → weighted $50: 45.00 / 4.00 / 1.00 (splitPrice integer
    // cents, remainder lands on the last card).
    let (s, body) = bulk(
        db.clone(),
        r#"{"entry_ids":[311,312,313],"action":"purchase_split","value":{"total":50}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Split $50.00 across 3 card(s) (by value)", "affected": 3 }));
    assert_eq!(real(&db, "SELECT purchase_price FROM collection WHERE id = 311"), Some(45.0));
    assert_eq!(real(&db, "SELECT purchase_price FROM collection WHERE id = 312"), Some(4.0));
    assert_eq!(real(&db, "SELECT purchase_price FROM collection WHERE id = 313"), Some(1.0));

    // Equal $10/3 → 3.33 / 3.33 / 3.34 (drift on the last card).
    let (s, body) = bulk(
        db.clone(),
        r#"{"entry_ids":[311,312,313],"action":"purchase_split","value":{"total":10,"method":"equal"}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Split $10.00 across 3 card(s) (evenly)", "affected": 3 }));
    assert_eq!(real(&db, "SELECT purchase_price FROM collection WHERE id = 311"), Some(3.33));
    assert_eq!(real(&db, "SELECT purchase_price FROM collection WHERE id = 312"), Some(3.33));
    assert_eq!(real(&db, "SELECT purchase_price FROM collection WHERE id = 313"), Some(3.34));

    // Weighted with zero market data falls back to equal and says 'evenly'.
    exec(&db, "UPDATE card_cache SET price_trend = 0 WHERE id IN ('c1','c2','c3');");
    let (s, body) = bulk(
        db.clone(),
        r#"{"entry_ids":[311,312,313],"action":"purchase_split","value":{"total":"9"}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Split $9.00 across 3 card(s) (evenly)", "affected": 3 }));
    assert_eq!(real(&db, "SELECT purchase_price FROM collection WHERE id = 311"), Some(3.0));
    assert_eq!(real(&db, "SELECT purchase_price FROM collection WHERE id = 313"), Some(3.0));
}

#[tokio::test]
async fn bulk_purchase_split_rejects_others_entries() {
    let (_d, db) = db();
    // Entry 201 belongs to user 2 — user 1 selecting only it finds no rows.
    let (s, body) =
        bulk(db, r#"{"entry_ids":[201],"action":"purchase_split","value":{"total":5}}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "No valid entries");
}

#[tokio::test]
async fn bulk_add_to_deck_success() {
    let (_d, db) = db();
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"add_to_deck","value":501}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!({ "message": "Added 4 card(s) to deck", "affected": 4, "rejected": 0 }));
    assert_eq!(
        scalar(&db, "SELECT quantity FROM deck_cards WHERE deck_id = 501 AND card_id = 'c1'"),
        4
    );
}

#[tokio::test]
async fn bulk_add_to_deck_only_own_rejection() {
    let (_d, db) = db();
    // Deck already carries 2 Counterspell; the user owns only 1 → newQty 3 > owned 1.
    exec(&db, "INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (501, 'c2', 2);");
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[102],"action":"add_to_deck","value":501}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "message": "You only own 1 copy of Counterspell.", "affected": 0, "rejected": 1 })
    );
    assert_eq!(
        scalar(&db, "SELECT quantity FROM deck_cards WHERE deck_id = 501 AND card_id = 'c2'"),
        2
    );
}

#[tokio::test]
async fn bulk_add_to_deck_over_four_rejection() {
    let (_d, db) = db();
    // Own 10 Bolts (4 + fresh 6), deck already has 4 → newQty 10 > 4.
    exec(&db, "INSERT INTO collection (id, card_id, user_id, quantity) VALUES (115, 'c1', 1, 6);");
    exec(&db, "INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (501, 'c1', 4);");
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[115],"action":"add_to_deck","value":501}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "message": "Cannot have more than 4 copies of Lightning Bolt.", "affected": 0, "rejected": 1 })
    );
    assert_eq!(
        scalar(&db, "SELECT quantity FROM deck_cards WHERE deck_id = 501 AND card_id = 'c1'"),
        4
    );
}

#[tokio::test]
async fn bulk_add_to_deck_basic_land_bypasses_four_limit() {
    let (_d, db) = db();
    // Owned Plains = 1 (entry 104) + 8 (entry 116) = 9; deck already has 1
    // → newQty 9 > 4 — allowed only because Plains is a basic land.
    exec(
        &db,
        "INSERT INTO collection (id, card_id, user_id, quantity) VALUES (116, 'c4', 1, 8);
         INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (501, 'c4', 1);",
    );
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[116],"action":"add_to_deck","value":501}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["message"], "Added 8 card(s) to deck");
    assert_eq!(body["rejected"], 0);
    assert_eq!(
        scalar(&db, "SELECT quantity FROM deck_cards WHERE deck_id = 501 AND card_id = 'c4'"),
        9
    );
}

#[tokio::test]
async fn bulk_add_to_deck_invalid_deck_and_checked_out() {
    let (_d, db) = db();
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"add_to_deck","value":0}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid deck_id");

    let (s, body) =
        bulk(db.clone(), r#"{"entry_ids":[101],"action":"add_to_deck","value":99999}"#).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Deck not found");

    // Another user's deck reads as not found.
    exec(&db, "INSERT INTO decks (id, user_id, name) VALUES (504, 2, 'Theirs');");
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[101],"action":"add_to_deck","value":504}"#).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Deck not found");

    exec(&db, "UPDATE decks SET checked_out = 1 WHERE id = 501;");
    let (s, body) = bulk(db, r#"{"entry_ids":[101],"action":"add_to_deck","value":501}"#).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(body["error"], "Check this deck in before changing its cards.");
}

#[tokio::test]
async fn bulk_add_to_deck_merges_alt_printings() {
    let (_d, db) = db();
    // The deck carries TWO physical printings of one game card. Adding one
    // more copy must land the whole logical quantity on the selected
    // printing (c1b) and zero-then-drop the legacy alt row (c1).
    exec(
        &db,
        "INSERT INTO collection (id, card_id, user_id, quantity) VALUES (118, 'c1b', 1, 1);
         INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (501, 'c1', 1), (501, 'c1b', 1);",
    );
    let (s, body) = bulk(db.clone(), r#"{"entry_ids":[118],"action":"add_to_deck","value":501}"#).await;
    assert_eq!(s, StatusCode::OK);
    // Node keys newQty on SUM(deck quantity across all printings of the
    // logical card) (collection.js:903-912): 1 + 1 + 1 new = 3 on the keeper.
    assert_eq!(body["affected"], 1);
    assert_eq!(body["message"], "Added 1 card(s) to deck");
    assert_eq!(
        scalar(&db, "SELECT quantity FROM deck_cards WHERE deck_id = 501 AND card_id = 'c1b'"),
        3
    );
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM deck_cards WHERE deck_id = 501 AND card_id = 'c1'"),
        0
    );
}
