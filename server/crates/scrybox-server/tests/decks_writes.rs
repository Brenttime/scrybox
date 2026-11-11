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
    // iter-30 legs: with the group OFF everything still proxies untouched.
    for (m, p) in [
        ("POST", "/api/decks"),
        ("POST", "/api/decks/931004/cards"),
        ("POST", "/api/decks/931001/register-collection"),
        ("PUT", "/api/decks/931001/checkout"),
        ("PUT", "/api/decks/931001/return"),
    ] {
        let (_s, h, _) = request(&r, m, p, &JSON_CT, Some("{}")).await;
        assert_eq!(h.get("x-node").unwrap(), "1", "{m} {p}");
    }
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
        ("PUT", "/api/decks/931001/combos"),
        ("POST", "/api/decks/931001"),
        ("GET", "/api/decks/931001/"), // trailing slash: Express loose match stays proxied
        ("DELETE", "/api/decks/931001/checkout"), // owned shape, wrong method
        ("PATCH", "/api/decks/931001/register-collection"),
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

    for (m, p) in [
        ("PUT", "/api/decks/931001"),
        ("DELETE", "/api/decks/931001"),
        ("DELETE", "/api/decks/931001/cards/c-sol-a"),
        ("POST", "/api/decks"),
        ("POST", "/api/decks/931004/cards"),
        ("POST", "/api/decks/931001/register-collection"),
        ("PUT", "/api/decks/931001/checkout"),
        ("PUT", "/api/decks/931001/return"),
    ] {
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

// ---------------------------------------------------------------------------
// iter-30: POST /api/decks, POST /:id/cards, POST /:id/register-collection,
// PUT /:id/checkout, PUT /:id/return
// ---------------------------------------------------------------------------

fn fund(conn: &Connection, card_id: &str, user_id: i64, qty: i64) {
    conn.execute(
        "INSERT INTO collection (card_id, user_id, quantity) VALUES (?1, ?2, ?3)",
        rusqlite::params![card_id, user_id, qty],
    )
    .unwrap();
}

fn deck_cards_of(conn: &Connection, deck_id: i64) -> Vec<(String, i64)> {
    conn.prepare("SELECT card_id, quantity FROM deck_cards WHERE deck_id=?1 ORDER BY card_id")
        .unwrap()
        .query_map(rusqlite::params![deck_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn create_deck_contract() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "create.db");
    let r = router_for(db.clone(), &["decks_writes"], &format!("http://{upstream}"));
    let conn = open(&db);

    // Falsy name -> 400 before any DB touch.
    for body in ["{}", "{\"name\":\"\"}", "{\"name\":0}", "{\"name\":null}", "[]"] {
        let (s, _, b) = request(&r, "POST", "/api/decks", &JSON_CT, Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "body {body}");
        assert_eq!(json_body(&b), json!({ "error": "Deck name is required" }));
    }

    // Minimal create + defaults (description '', format Standard, ...).
    let (s, h, b) = request(&r, "POST", "/api/decks", &JSON_CT, Some("{\"name\":\"Fresh\"}")).await;
    assert_eq!(s, StatusCode::CREATED);
    let created = json_body(&b);
    assert_eq!(created["message"], "Deck created successfully");
    let new_id = created["id"].as_i64().unwrap();
    assert!(new_id > 931004, "id {new_id}");
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(), "application/json; charset=utf-8");
    let row: (String, String, String, String, String, i64) = conn
        .query_row(
            "SELECT name, description, format, category, accent_color, target_size FROM decks WHERE id=?1",
            rusqlite::params![new_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(row, ("Fresh".into(), "".into(), "Standard".into(), "Competitive".into(), "#eab308".into(), 60));

    // parseInt(target_size,10) || 60 twin: strings parse, junk/0/null -> 60.
    for (body, want) in [
        ("{\"name\":\"TS1\",\"target_size\":\"40\"}", 40),
        ("{\"name\":\"TS2\",\"target_size\":\"abc\"}", 60),
        ("{\"name\":\"TS3\",\"target_size\":0}", 60),
        ("{\"name\":\"TS4\",\"target_size\":null}", 60),
        ("{\"name\":\"TS5\",\"target_size\":20}", 20),
    ] {
        let (s, _, b) = request(&r, "POST", "/api/decks", &JSON_CT, Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{body}");
        let id = json_body(&b)["id"].as_i64().unwrap();
        let ts: i64 = conn.query_row("SELECT target_size FROM decks WHERE id=?1", rusqlite::params![id], |r| r.get(0)).unwrap();
        assert_eq!(ts, want, "{body}");
    }

    // Decklist import: /^(\d+)x?\s+(.+)$/i per trimmed line, unresolved
    // names silently skipped, 'Sol Ring' resolves to the first cache match.
    let body = json!({"name":"Imported","decklist_text":"4x Sol Ring\n 1x Counterspell \nnot a line\n\n"}).to_string();
    let (s, _, b) = request(&r, "POST", "/api/decks", &JSON_CT, Some(&body)).await;
    assert_eq!(s, StatusCode::CREATED);
    let id = json_body(&b)["id"].as_i64().unwrap();
    assert_eq!(deck_cards_of(&conn, id), vec![("c-other".to_string(), 1), ("c-sol-a".to_string(), 4)]);

    // Empty string / non-string decklist_text import nothing.
    for body in ["{\"name\":\"E1\",\"decklist_text\":\"\"}", "{\"name\":\"E2\",\"decklist_text\":5}"] {
        let (s, _, b) = request(&r, "POST", "/api/decks", &JSON_CT, Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{body}");
        let id = json_body(&b)["id"].as_i64().unwrap();
        assert!(deck_cards_of(&conn, id).is_empty(), "{body}");
    }
}

#[tokio::test]
async fn add_card_contract() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "add.db");
    let r = router_for(db.clone(), &["decks_writes"], &format!("http://{upstream}"));
    let conn = open(&db);

    // Missing card_id -> 400; unknown deck -> 404; checked-out deck -> 409.
    let (s, _, b) = request(&r, "POST", "/api/decks/931004/cards", &JSON_CT, Some("{\"quantity\":1}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "card_id is required" }));
    let (s, _, b) = request(&r, "POST", "/api/decks/999999/cards", &JSON_CT, Some("{\"card_id\":\"c-other\"}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "Deck not found or unauthorized" }));
    let (s, _, b) = request(&r, "POST", "/api/decks/931002/cards", &JSON_CT, Some("{\"card_id\":\"c-other\"}")).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(json_body(&b), json!({ "error": "Check this deck in before changing its cards." }));

    // cardApi.getCardById would throw on a non-string cardId; the throw is
    // inside the route's try, so Node answers its catch message.
    fund(&conn, "c-other", 1, 10);
    let (s, _, b) = request(&r, "POST", "/api/decks/931004/cards", &JSON_CT, Some("{\"card_id\":42}")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({ "error": "Failed to add card to deck" }));

    // Cache-miss with a string card_id -> whole request forwarded to Node
    // (Scryfall hydration stays on the node lane).
    let (s, h, b) = request(&r, "POST", "/api/decks/931004/cards", &JSON_CT, Some("{\"card_id\":\"c-unknown-xyz\"}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["body"], "{\"card_id\":\"c-unknown-xyz\"}");

    // Ownership gate: nothing owned -> 400 with the exact Node message.
    let (s, _, b) = request(&r, "POST", "/api/decks/931004/cards", &JSON_CT, Some("{\"card_id\":\"c-sol-a\"}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "You only own 0 copies of Sol Ring." }));

    // quantity destructuring default (1) + string quantity, and price
    // history recorded for the effective card.
    fund(&conn, "c-sol-a", 1, 5);
    let (s, _, b) = request(&r, "POST", "/api/decks/931004/cards", &JSON_CT, Some("{\"card_id\":\"c-sol-a\",\"quantity\":\"3\"}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({ "message": "Card added/updated in deck successfully" }));
    assert_eq!(deck_cards_of(&conn, 931004), vec![("c-sol-a".to_string(), 3)]);
    let ph: i64 = conn
        .query_row("SELECT COUNT(*) FROM price_history WHERE card_id='c-sol-a' AND price=0.5", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ph, 1);

    // Invalid quantity flows through validateDeckAddition (parseInt -> NaN).
    let (s, _, b) = request(&r, "POST", "/api/decks/931004/cards", &JSON_CT, Some("{\"card_id\":\"c-sol-a\",\"quantity\":\"abc\"}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Invalid quantity" }));
    // Non-basic > 4 copies blocked.
    fund(&conn, "c-other", 1, 10);
    let (s, _, b) = request(&r, "POST", "/api/decks/931004/cards", &JSON_CT, Some("{\"card_id\":\"c-other\",\"quantity\":5}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Cannot have more than 4 copies of Counterspell." }));

    // Alt-printing collapse: deck 931001 holds c-sol-a x2 and c-sol-b x1
    // (same LOWER(TRIM(name)) key); adding c-sol-b repoints the rows to the
    // requested printing and drops the zeros.
    let (s, _, _) = request(&r, "POST", "/api/decks/931001/cards", &JSON_CT, Some("{\"card_id\":\"c-sol-b\",\"quantity\":4}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(deck_cards_of(&conn, 931001), vec![("c-other".to_string(), 1), ("c-sol-b".to_string(), 4)]);
}

#[tokio::test]
async fn register_collection_contract() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "reg.db");
    let r = router_for(db.clone(), &["decks_writes"], &format!("http://{upstream}"));
    let conn = open(&db);
    conn.execute_batch(
        "INSERT INTO decks (id,user_id,name,checked_out) VALUES (931005,1,'Broken',0);
         INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES (931005,'c-missing-x',1);",
    )
    .unwrap();

    // Empty / unresolved / foreign decks.
    let (s, _, b) = request(&r, "POST", "/api/decks/931004/register-collection", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "This deck has no cards to register." }));
    let (s, _, b) = request(&r, "POST", "/api/decks/931005/register-collection", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(&b),
        json!({ "error": "This deck contains cards whose details are unavailable. Refresh or re-import it before registering." })
    );
    let (s, _, b) = request(&r, "POST", "/api/decks/931003/register-collection", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "Deck not found or unauthorized" }));
    // Checked out (deck 931002 holds c-other for user 1).
    let (s, _, b) = request(&r, "POST", "/api/decks/931002/register-collection", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(json_body(&b), json!({ "error": "This deck is already checked out for play." }));

    // Happy path: deck 931001 = c-sol-a x2 + c-sol-b x1 + c-other x1.
    let (s, _, b) = request(&r, "POST", "/api/decks/931001/register-collection", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        json_body(&b),
        json!({ "message": "Added 4 cards from Deck One to your collection.", "added": 4, "card_types": 3 })
    );
    let rows: Vec<(String, i64, String, i64)> = conn
        .prepare("SELECT card_id, quantity, language, is_trade FROM collection WHERE user_id=1 ORDER BY card_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("c-other".to_string(), 1, "English".to_string(), 0),
            ("c-sol-a".to_string(), 2, "English".to_string(), 0),
            ("c-sol-b".to_string(), 1, "English".to_string(), 0),
        ]
    );
    // Auxiliary price sweep: one row per distinct card, failures swallowed.
    let ph: i64 = conn.query_row("SELECT COUNT(DISTINCT card_id) FROM price_history", [], |r| r.get(0)).unwrap();
    assert_eq!(ph, 3);

    // No dedup: registering twice duplicates the rows (Node has no guard).
    let (s, _, _) = request(&r, "POST", "/api/decks/931001/register-collection", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::CREATED);
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM collection WHERE user_id=1", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 6);
}

#[tokio::test]
async fn checkout_contract() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "checkout.db");
    let r = router_for(db.clone(), &["decks_writes"], &format!("http://{upstream}"));
    let conn = open(&db);
    conn.execute_batch(
        "INSERT INTO decks (id,user_id,name,checked_out) VALUES (931005,1,'Broken',0);
         INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES (931005,'c-missing-x',1);",
    )
    .unwrap();

    // Error-first ladder: 404 / already / empty / unresolved.
    let (s, _, b) = request(&r, "PUT", "/api/decks/999999/checkout", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "Deck not found or unauthorized" }));
    let (s, _, _b) = request(&r, "PUT", "/api/decks/931003/checkout", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "foreign deck");
    let (s, _, b) = request(&r, "PUT", "/api/decks/931002/checkout", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(json_body(&b), json!({ "error": "This deck is already checked out for play." }));
    let (s, _, b) = request(&r, "PUT", "/api/decks/931004/checkout", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "This deck has no cards to check out." }));
    let (s, _, b) = request(&r, "PUT", "/api/decks/931005/checkout", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(&b),
        json!({ "error": "This deck contains cards whose details are unavailable. Refresh or re-import it before checkout." })
    );

    // Shortfall diagnostics: deck 931006 (Counterspell x1) with deck 931002
    // (same key) checked out elsewhere -> available -1, deficit 2, and the
    // template prints the raw row values (COALESCEd to 0).
    conn.execute_batch(
        "INSERT INTO decks (id,user_id,name,checked_out) VALUES (931006,1,'Shortfall',0);
         INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES (931006,'c-other',1);",
    )
    .unwrap();
    let (s, _, b) = request(&r, "PUT", "/api/decks/931006/checkout", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&b),
        json!({ "error": "Not enough cards available to check out this deck.", "details": ["Missing 2x Counterspell (Owned: 0, In Use: 1)"] })
    );

    // Happy path: 3x Sol Ring (both printings fold into one key) and
    // Counterspell net of the deck-931002 lock (owned 2, locked 1).
    fund(&conn, "c-sol-a", 1, 3);
    fund(&conn, "c-other", 1, 2);
    let (s, _, b) = request(&r, "PUT", "/api/decks/931001/checkout", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({ "message": "Deck checked out successfully" }));
    let (co, at): (i64, Option<String>) = conn
        .query_row("SELECT checked_out, checked_out_at FROM decks WHERE id=931001", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(co, 1);
    assert!(at.is_some());

    // Second checkout -> 409 already-out.
    let (s, _, b) = request(&r, "PUT", "/api/decks/931001/checkout", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(json_body(&b), json!({ "error": "This deck is already checked out for play." }));

    // Return: 200, flags cleared, idempotent second return, 404s.
    let (s, _, b) = request(&r, "PUT", "/api/decks/931001/return", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({ "message": "Deck returned to collection successfully" }));
    let (co, at): (i64, Option<String>) = conn
        .query_row("SELECT checked_out, checked_out_at FROM decks WHERE id=931001", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(co, 0);
    assert_eq!(at, None);
    let (s, _, _) = request(&r, "PUT", "/api/decks/931001/return", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::OK, "returning an unchecked deck is still 200");
    let (s, _, _) = request(&r, "PUT", "/api/decks/931003/return", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = request(&r, "PUT", "/api/decks/999999/return", &JSON_CT, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
