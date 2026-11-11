//! Path-level tests for the iter-28 group: `decks_writes`
//! (PUT /api/decks/:id, DELETE /api/decks/:id, DELETE /api/decks/:id/cards/:card_id).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (dk_* cases). These tests
//! pin the contract locally: kill-switch proxying, method/shape guards
//! (nothing 405s through this router), the auth gate with parse-before-auth
//! order, the PUT falsiness contract and Node-driver binding shapes, the
//! deck delete transaction (incl. deck_list_revision invalidation), and the
//! logical-card delete (all printings of the same name leave the deck).

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn spawn_fake_node() -> SocketAddr {
    let echo = any(|req: Request| async move {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let mut out = HeaderMap::new();
        out.insert(HeaderName::from_static("x-node"), HeaderValue::from_static("1"));
        (
            StatusCode::OK,
            out,
            axum::Json(json!({
                "method": parts.method.as_str(),
                "pq": parts.uri.path_and_query().unwrap().as_str(),
                "body": String::from_utf8_lossy(&body),
            })),
        )
            .into_response()
    });
    let health = axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({"status":"ok"}))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

fn seeded_db(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "CREATE TABLE deck_maybe_cards (
            deck_id INTEGER NOT NULL,
            card_id TEXT NOT NULL,
            quantity INTEGER DEFAULT 1,
            PRIMARY KEY(deck_id, card_id)
         );
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL),
                  (2,'u2','x','member','tok-2',0,'k59');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id,name,supertype,subtypes,types,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_estimated)
           VALUES ('c-sol-a','Sol Ring',null,'[]','[\"Artifact\"]','Common','dmu','Dominaria','1',null,0.5,0.5,0,0),
                  ('c-sol-b','Sol Ring ',null,'[]','[\"Artifact\"]','Common','who','Doctor Who','2',null,0.4,0.4,0,0),
                  ('c-other','Counterspell',null,'[]','[\"Instant\"]','Uncommon','dmu','Dominaria','3',null,1.0,1.0,0,0);
         INSERT INTO decks (id,user_id,name,description,checked_out) VALUES
             (931001,1,'Deck One','one',0),
             (931002,1,'Checked Out','co',1),
             (931003,2,'Foreign Deck','fd',0),
             (931004,1,'Empty Deck','',0);
         INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES
             (931001,'c-sol-a',2),(931001,'c-sol-b',1),(931001,'c-other',1),
             (931002,'c-other',1);
         INSERT INTO deck_maybe_cards (deck_id,card_id,quantity) VALUES (931001,'c-other',1);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);
         INSERT OR IGNORE INTO collection_read_revision (id, revision) VALUES (1, 1);
         INSERT OR IGNORE INTO stats_aux_revision (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    path
}

fn router_for(db_path: PathBuf, routes: &[&str], upstream: &str) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    app(AppState::new(cfg))
}

async fn request(r: &Router, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut rb = Request::builder().method(method).uri(path);
    for &(k, v) in headers {
        rb = rb.header(k, v);
    }
    let req = rb.body(Body::from(body.unwrap_or("").to_string())).unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, headers, bytes)
}

fn json_body(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap()
}

const AUTH: [(&str, &str); 1] = [("authorization", "Bearer sess-1")];
const JSON_CT: [(&str, &str); 2] = [("authorization", "Bearer sess-1"), ("content-type", "application/json")];

fn open(db: &PathBuf) -> Connection {
    Connection::open(db).unwrap()
}

#[tokio::test]
async fn kill_switch_proxies_everything_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "ks.db"), &[], &format!("http://{upstream}"));
    let (s, h, b) = request(&r, "PUT", "/api/decks/931001", &JSON_CT, Some("{\"name\":\"x\"}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["pq"], "/api/decks/931001");
    let (_s, h, _) = request(&r, "DELETE", "/api/decks/931001", &AUTH, None).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
    let (_s, h, _) = request(&r, "DELETE", "/api/decks/931001/cards/c-sol-a", &AUTH, None).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn shape_guards_proxy_untouched_never_405() {
    // The 405-router guard from iter-27: every shape/method the port does
    // not own must reach Node, including shapes under /api/decks.
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "shape.db"), &["decks_writes"], &format!("http://{upstream}"));
    for (m, p) in [
        ("GET", "/api/decks"),
        ("GET", "/api/decks/931001"),
        ("GET", "/api/decks/931001/combos"),
        ("POST", "/api/decks/931001/cards"),
        ("PUT", "/api/decks/931001/combos"),
        ("POST", "/api/decks"),
        ("POST", "/api/decks/931001"),
        ("GET", "/api/decks/931001/"), // trailing slash: Express loose match stays proxied
    ] {
        let (_s, h, _) = request(&r, m, p, &JSON_CT, Some("{}")).await;
        assert_eq!(h.get("x-node").unwrap(), "1", "{m} {p} must proxy, not 405/404 from this router");
    }
}

#[tokio::test]
async fn auth_gate_and_parse_order() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "auth.db"), &["decks_writes"], &format!("http://{upstream}"));

    for (m, p) in [("PUT", "/api/decks/931001"), ("DELETE", "/api/decks/931001"), ("DELETE", "/api/decks/931001/cards/c-sol-a")] {
        let (s, _, b) = request(&r, m, p, &[], None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{m} {p}");
        assert_eq!(json_body(&b), json!({ "error": "Access denied. No token provided." }));
    }

    // Malformed JSON WITHOUT a token still answers 500 — parse-before-auth,
    // byte-pinned live against :3312.
    let (s, _, b) = request(
        &r,
        "PUT",
        "/api/decks/931001",
        &[("content-type", "application/json")],
        Some("{oops"),
    )
    .await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({ "error": "Internal server error" }));
    let (s, _, _) = request(
        &r,
        "DELETE",
        "/api/decks/931001",
        &[("content-type", "application/json")],
        Some("{oops"),
    )
    .await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);

    // API key (GET-only credential) on a write -> 403.
    let (s, _, b) = request(
        &r,
        "PUT",
        "/api/decks/931001",
        &[("authorization", "Bearer k59"), ("content-type", "application/json")],
        Some("{\"name\":\"x\"}"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(json_body(&b), json!({ "error": "API keys are read-only. Use a login session for this request." }));
}

#[tokio::test]
async fn put_deck_contract() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "put.db");
    let r = router_for(db.clone(), &["decks_writes"], &format!("http://{upstream}"));

    // Falsy names (JS `!name`) -> 400 before any DB touch.
    for body in ["{}", "{\"name\":\"\"}", "{\"name\":0}", "{\"name\":false}", "{\"name\":null}", "[]"] {
        let (s, _, b) = request(&r, "PUT", "/api/decks/931001", &JSON_CT, Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "body {body}");
        assert_eq!(json_body(&b), json!({ "error": "Deck name is required" }));
    }
    // Non-object JSON body -> express.json strict -> 500 (live-pinned).
    let (s, _, b) = request(&r, "PUT", "/api/decks/931001", &JSON_CT, Some("5")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({ "error": "Internal server error" }));

    // Success + response shape (res.json: charset + weak body ETag).
    let (s, h, b) = request(&r, "PUT", "/api/decks/931001", &JSON_CT, Some("{\"name\":\"Renamed\",\"description\":\"D\"}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({ "message": "Deck updated successfully" }));
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(), "application/json; charset=utf-8");
    assert!(h.get(header::ETAG).unwrap().to_str().unwrap().starts_with("W/\""));
    let conn = open(&db);
    let (name, desc): (String, String) = conn
        .query_row("SELECT name, description FROM decks WHERE id=931001", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap();
    assert_eq!(name, "Renamed");
    assert_eq!(desc, "D");

    // description omitted -> '' (JS `description || ''`).
    let (s, _, _) = request(&r, "PUT", "/api/decks/931001", &JSON_CT, Some("{\"name\":\"NoDesc\"}")).await;
    assert_eq!(s, StatusCode::OK);
    let desc: String = conn
        .query_row("SELECT description FROM decks WHERE id=931001", [], |row| row.get(0))
        .unwrap();
    assert_eq!(desc, "");

    // Node-driver binding shapes (live-pinned on :3312): number -> '42',
    // true -> '1', object/array -> '[object Object]', missing -> ''.
    let (s, _, _) = request(&r, "PUT", "/api/decks/931001", &JSON_CT, Some("{\"name\":42,\"description\":true}")).await;
    assert_eq!(s, StatusCode::OK);
    let (name, desc): (String, String) = conn
        .query_row("SELECT name, description FROM decks WHERE id=931001", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap();
    assert_eq!(name, "42");
    assert_eq!(desc, "1");
    let (s, _, _) = request(&r, "PUT", "/api/decks/931001", &JSON_CT, Some("{\"name\":{\"a\":1},\"description\":[1,2]}")).await;
    assert_eq!(s, StatusCode::OK);
    let (name, desc): (String, String) = conn
        .query_row("SELECT name, description FROM decks WHERE id=931001", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap();
    assert_eq!(name, "[object Object]");
    assert_eq!(desc, "[object Object]");

    // Missing deck and cross-user deck -> identical 404 body.
    for p in ["/api/decks/999999", "/api/decks/931003", "/api/decks/abc"] {
        let (s, _, b) = request(&r, "PUT", p, &JSON_CT, Some("{\"name\":\"x\"}")).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{p}");
        assert_eq!(json_body(&b), json!({ "error": "Deck not found or unauthorized" }));
    }
}

#[tokio::test]
async fn delete_deck_contract() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "del.db");
    let r = router_for(db.clone(), &["decks_writes"], &format!("http://{upstream}"));
    let conn = open(&db);
    let rev0: i64 = conn.query_row("SELECT revision FROM deck_list_revision WHERE id=1", [], |r| r.get(0)).unwrap();

    // Checked-out deck -> 409 and nothing removed.
    let (s, _, b) = request(&r, "DELETE", "/api/decks/931002", &AUTH, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(json_body(&b), json!({ "error": "Check this deck in before changing its cards." }));
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM deck_cards WHERE deck_id=931002", [], |r| r.get(0)).unwrap(),
        1
    );

    // Cross-user / missing -> 404.
    for p in ["/api/decks/931003", "/api/decks/999999", "/api/decks/abc"] {
        let (s, _, b) = request(&r, "DELETE", p, &AUTH, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{p}");
        assert_eq!(json_body(&b), json!({ "error": "Deck not found or unauthorized" }));
    }

    // Happy path: deck + deck_cards + deck_maybe_cards all go; the DB
    // triggers bump deck_list_revision so Node's deck-list cache invalidates.
    let (s, h, b) = request(&r, "DELETE", "/api/decks/931001", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({ "message": "Deck deleted successfully" }));
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(), "application/json; charset=utf-8");
    for (table, col) in [("decks", "id"), ("deck_cards", "deck_id"), ("deck_maybe_cards", "deck_id")] {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {col}=931001"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table} rows for the deleted deck must be gone");
    }
    let rev1: i64 = conn.query_row("SELECT revision FROM deck_list_revision WHERE id=1", [], |r| r.get(0)).unwrap();
    assert!(rev1 > rev0, "deck_list_revision must bump on Rust-side writes (Node cache invalidation)");
}

#[tokio::test]
async fn delete_card_removes_all_printings_of_logical_card() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "card.db");
    let r = router_for(db.clone(), &["decks_writes"], &format!("http://{upstream}"));
    let conn = open(&db);

    // Deck missing -> 404; deck checked out -> 409.
    let (s, _, b) = request(&r, "DELETE", "/api/decks/999999/cards/c-sol-a", &AUTH, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "Deck not found or unauthorized" }));
    let (s, _, b) = request(&r, "DELETE", "/api/decks/931002/cards/c-other", &AUTH, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(json_body(&b), json!({ "error": "Check this deck in before changing its cards." }));

    // Logical-card delete: 'Sol Ring' and 'Sol Ring ' (same TRIM/LOWER key)
    // both leave; the unrelated card stays.
    let (s, _, b) = request(&r, "DELETE", "/api/decks/931001/cards/c-sol-a", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({ "message": "Card removed from deck successfully" }));
    let remaining: Vec<String> = conn
        .prepare("SELECT card_id FROM deck_cards WHERE deck_id=931001 ORDER BY card_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(remaining, vec!["c-other".to_string()]);

    // Removing a card that is not in the deck is still the 200 message
    // (Node answers unconditionally when not checked out).
    let (s, _, b) = request(&r, "DELETE", "/api/decks/931001/cards/c-sol-a", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({ "message": "Card removed from deck successfully" }));
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM deck_cards WHERE deck_id=931001", [], |r| r.get(0)).unwrap(),
        1
    );
}
