//! Node-zero (standalone, SCRYBOX_NODE_UPSTREAM empty) fallbacks for the
//! collection-writes / import-export / settings / admin-users / manabox /
//! cards-tags handlers. Every case here used to reach `crate::proxy` (or an
//! export replay) and, with no Node, answered 502 "Upstream unavailable" or
//! the wrong finalhandler 404. Each assertion pins Node's own answer.

use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Path, Request},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

type Hits = Arc<Mutex<Vec<String>>>;

const KNOWN: &str = "aaaaaaaa-0000-0000-0000-000000000001";

fn scry_card(id: &str, lang: &str, name: &str) -> Value {
    json!({
        "object": "card", "id": id, "name": name, "lang": lang, "printed_name": name,
        "set": "eld", "set_name": "Throne of Eldraine", "collector_number": "126",
        "rarity": "common", "type_line": "Instant", "cmc": 2.0, "color_identity": ["R"],
        "image_uris": {"normal": "http://x/n.jpg"}, "prices": {"usd": "0.10"}
    })
}

async fn mock_scryfall(hits: Hits, ja_ok: bool) -> String {
    let h1 = hits.clone();
    let h2 = hits.clone();
    let h3 = hits.clone();
    let r = Router::new()
        .route(
            "/cards/{id}",
            get(move |Path(id): Path<String>| {
                let h = h1.clone();
                async move {
                    h.lock().unwrap().push(format!("/cards/{id}"));
                    if id == KNOWN {
                        axum::Json(scry_card(&id, "en", "Fling")).into_response()
                    } else {
                        (StatusCode::NOT_FOUND, axum::Json(json!({"object":"error","status":404}))).into_response()
                    }
                }
            }),
        )
        .route(
            "/cards/{set}/{num}/{lang}",
            get(move |Path((set, num, lang)): Path<(String, String, String)>| {
                let h = h2.clone();
                async move {
                    h.lock().unwrap().push(format!("/cards/{set}/{num}/{lang}"));
                    if ja_ok {
                        axum::Json(scry_card("aaaaaaaa-0000-0000-0000-0000000000aa", &lang, "Fling-ja")).into_response()
                    } else {
                        (StatusCode::NOT_FOUND, axum::Json(json!({"object":"error","status":404}))).into_response()
                    }
                }
            }),
        )
        .route(
            "/cards/collection",
            post(move |body: axum::Json<Value>| {
                let h = h3.clone();
                async move {
                    h.lock().unwrap().push(format!("POST /cards/collection {}", body.0));
                    axum::Json(json!({"object":"list","not_found":[],"data":[]})).into_response()
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    format!("http://{addr}")
}

struct H {
    router: Router,
    db: std::path::PathBuf,
    hits: Hits,
    _dir: tempfile::TempDir,
}

async fn harness_with(ja_ok: bool) -> H {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL), (2,'boss','x','admin','tok-2',0,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-2', 2, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id, name, set_id, number, language) VALUES
           ('mtg-cached-en', 'Fling', 'eld', '126', 'English'),
           ('mtg-other', 'Shock', 'm19', '156', 'English');
         INSERT INTO collection (id, card_id, user_id, quantity) VALUES (7, 'mtg-cached-en', 1, 1);",
    )
    .unwrap();
    drop(conn);
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let base = mock_scryfall(hits.clone(), ja_ok).await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: String::new(),
        static_dir: None,
        rust_routes: ["*".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(db.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.scry = Arc::new(scrybox_server::scryfall::Scry::new(scrybox_server::scryfall::ScryEnv {
        local: false,
        local_url: base.clone(),
        base_url: base,
        gap_scale: 0.0,
    }));
    H { router: app(st), db, hits, _dir: dir }
}

async fn harness() -> H {
    harness_with(true).await
}

async fn call(r: &Router, method: &str, uri: &str, token: Option<&str>, body: Option<&str>) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::new("127.0.0.9".parse::<IpAddr>().unwrap(), 5555)));
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let req = b.body(body.map(|s| Body::from(s.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let st = res.status();
    let h = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, h, String::from_utf8_lossy(&bytes).into_owned())
}

fn js(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::String(s.to_string()))
}

fn cannot(method: &str, path: &str) -> String {
    format!("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n</head>\n<body>\n<pre>Cannot {method} {path}</pre>\n</body>\n</html>\n")
}

// --- collection_writes ----------------------------------------------------

#[tokio::test]
async fn put_entry_card_id_cache_miss_fetches_or_404s() {
    let h = harness().await;
    // Scryfall 404 → Node's `Card ID ${newCardId} not found.` (was 502).
    let (st, _, b) = call(&h.router, "PUT", "/api/collection/7", Some("sess-1"), Some(r#"{"card_id":"mtg-nope"}"#)).await;
    assert_eq!((st, js(&b)), (StatusCode::NOT_FOUND, json!({"error":"Card ID mtg-nope not found."})));
    // Scryfall hit: same-name printing cached, then the swap lands.
    let id = format!("mtg-{KNOWN}");
    let (st, _, b) = call(&h.router, "PUT", "/api/collection/7", Some("sess-1"), Some(&format!(r#"{{"card_id":"{id}"}}"#))).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let got: String = Connection::open(&h.db).unwrap().query_row("SELECT card_id FROM collection WHERE id=7", [], |r| r.get(0)).unwrap();
    assert_eq!(got, id);
    assert!(h.hits.lock().unwrap().iter().any(|x| x == &format!("/cards/{KNOWN}")));
}

#[tokio::test]
async fn add_localized_miss_fetches_printing_in_language() {
    let h = harness().await;
    let (st, _, b) = call(&h.router, "POST", "/api/collection", Some("sess-1"), Some(r#"{"card_id":"mtg-cached-en","language":"Japanese"}"#)).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(js(&b)["message"], "Card added to collection");
    let got: String = Connection::open(&h.db).unwrap()
        .query_row("SELECT card_id FROM collection ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    assert_eq!(got, "mtg-aaaaaaaa-0000-0000-0000-0000000000aa", "filed against the localized printing");
    assert!(h.hits.lock().unwrap().iter().any(|x| x == "/cards/eld/126/ja"), "{:?}", h.hits.lock().unwrap());
}

#[tokio::test]
async fn add_localized_miss_without_printing_keeps_picked_row() {
    let h = harness_with(false).await;
    let (st, _, b) = call(&h.router, "POST", "/api/collection", Some("sess-1"), Some(r#"{"card_id":"mtg-cached-en","language":"Japanese"}"#)).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let got: String = Connection::open(&h.db).unwrap()
        .query_row("SELECT card_id FROM collection ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    assert_eq!(got, "mtg-cached-en");
    // bulk-add rides the same leg (was a whole-request Forward).
    let (st, _, b) = call(&h.router, "POST", "/api/collection/bulk-add", Some("sess-1"),
        Some(r#"{"card_ids":["mtg-cached-en", 42],"language":"Japanese"}"#)).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let v = js(&b);
    assert_eq!(v["message"], "Added 1 of 2 cards; 1 failed.");
    // Non-string id cache miss: Node's getCardById `.startsWith` TypeError.
    assert_eq!(v["failed"], json!([{"card_id":42,"error":"Failed to add card","index":1}]));
}

#[tokio::test]
async fn add_non_string_card_id_miss_is_nodes_500() {
    let h = harness().await;
    let (st, _, b) = call(&h.router, "POST", "/api/collection", Some("sess-1"), Some(r#"{"card_id":12345}"#)).await;
    assert_eq!((st, js(&b)), (StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"Failed to add card"})));
}

#[tokio::test]
async fn put_delete_on_literal_collection_segments_hit_the_id_route() {
    let h = harness().await;
    for seg in ["bulk", "cardlist", "bulk-add"] {
        for m in ["PUT", "DELETE"] {
            let (st, _, b) = call(&h.router, m, &format!("/api/collection/{seg}"), Some("sess-1"), Some("{}")).await;
            assert_eq!((st, js(&b)), (StatusCode::NOT_FOUND, json!({"error":"Collection entry not found"})), "{m} {seg}");
        }
    }
}

#[tokio::test]
async fn unowned_methods_get_nodes_parser_gate_and_finalhandler() {
    let h = harness().await;
    // No token: the /api gate's 401.
    let (st, _, b) = call(&h.router, "PATCH", "/api/collection/7", None, None).await;
    assert_eq!((st, js(&b)), (StatusCode::UNAUTHORIZED, json!({"error":"Access denied. No token provided."})));
    // Malformed JSON: global normalJson runs BEFORE auth → generic 500.
    let (st, _, b) = call(&h.router, "PATCH", "/api/collection/7", None, Some("{bad")).await;
    assert_eq!((st, js(&b)), (StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"Internal server error"})));
    // Authenticated: finalhandler 404 HTML.
    for (m, p) in [("PATCH", "/api/collection/7"), ("GET", "/api/collection/bulk"), ("POST", "/api/settings"),
                   ("DELETE", "/api/settings/version"), ("PUT", "/api/export"), ("GET", "/api/import"),
                   ("GET", "/api/manabox-sync/preview"), ("POST", "/api/manabox-sync/runs"),
                   ("POST", "/api/cards/x/oracle"), ("GET", "/api/cards/tag-stats")] {
        let (st, hd, b) = call(&h.router, m, p, Some("sess-1"), None).await;
        assert_eq!((st, b), (StatusCode::NOT_FOUND, cannot(m, p)), "{m} {p}");
        assert_eq!(hd["content-type"], "text/html; charset=utf-8");
    }
}

#[tokio::test]
async fn admin_unowned_methods_run_admin_router_gate_first() {
    let h = harness().await;
    let (st, _, b) = call(&h.router, "DELETE", "/api/admin/users", None, None).await;
    assert_eq!((st, js(&b)), (StatusCode::UNAUTHORIZED, json!({"error":"Access denied. No token provided."})));
    let (st, _, b) = call(&h.router, "DELETE", "/api/admin/users", Some("sess-1"), None).await;
    assert_eq!((st, js(&b)), (StatusCode::FORBIDDEN, json!({"error":"Access denied. Administrator privileges required."})));
    let (st, _, b) = call(&h.router, "GET", "/api/admin/users/1", Some("sess-2"), None).await;
    assert_eq!((st, b), (StatusCode::NOT_FOUND, cannot("GET", "/api/admin/users/1")));
}

// --- import / export --------------------------------------------------------

#[tokio::test]
async fn import_json_parse_error_is_v8_text() {
    let h = harness().await;
    let (st, _, b) = call(&h.router, "POST", "/api/import", Some("sess-1"), Some(r#"{"format":"json","data":"abc"}"#)).await;
    assert_eq!((st, js(&b)), (StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"Import failed","message":"Unexpected token 'a', \"abc\" is not valid JSON"})));
    let (st, _, b) = call(&h.router, "POST", "/api/import", Some("sess-1"), Some(r#"{"format":"json","data":"[{\"a\":1,}]"}"#)).await;
    assert_eq!((st, js(&b)), (StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"Import failed","message":"Expected double-quoted property name in JSON at position 8 (line 1 column 9)"})));
}

#[tokio::test]
async fn import_object_card_id_binds_like_node_sqlite3() {
    let h = harness().await;
    // node-sqlite3 binds the object as '[object Object]' → cache miss →
    // Scryfall resolve (by name) → not recognized → 400 rejected report.
    let (st, _, b) = call(&h.router, "POST", "/api/import", Some("sess-1"),
        Some(r#"{"format":"json","data":[{"card_id":{"x":1},"name":"Bolt"}]}"#)).await;
    assert_eq!((st, js(&b)), (StatusCode::BAD_REQUEST, json!({
        "error":"Import rejected: 1 of 1 row(s) are not valid MTG printings. No rows were imported.",
        "rejected":[{"index":0,"reason":"card was not recognized by Scryfall as an MTG printing"}]})));
    assert!(h.hits.lock().unwrap().iter().any(|x| x.starts_with("POST /cards/collection")));
}

#[tokio::test]
async fn import_insert_failure_is_node_sqlite3_text() {
    let h = harness().await;
    // condition outside the CHECK list → the INSERT fails inside the tx.
    let (st, _, b) = call(&h.router, "POST", "/api/import", Some("sess-1"),
        Some(r#"{"format":"json","data":[{"card_id":"mtg-cached-en","condition":"Mint"}]}"#)).await;
    // mtg-cached-en starts with mtg- → prepared; the CHECK rejects the row.
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{b}");
    let v = js(&b);
    assert_eq!(v["error"], "Import failed");
    assert!(v["message"].as_str().unwrap().starts_with("SQLITE_CONSTRAINT: CHECK constraint failed"), "{v}");
    // Object quantity binds as '[object Object]' text (Node does the same).
    let (st, _, b) = call(&h.router, "POST", "/api/import", Some("sess-1"),
        Some(r#"{"format":"json","data":[{"card_id":"mtg-cached-en","quantity":{"n":2}}]}"#)).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let q: String = Connection::open(&h.db).unwrap()
        .query_row("SELECT quantity FROM collection ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    assert_eq!(q, "[object Object]");
}

#[tokio::test]
async fn export_db_failure_is_export_failed_body() {
    let h = harness().await;
    Connection::open(&h.db).unwrap().execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE card_cache;").unwrap();
    let (st, _, b) = call(&h.router, "GET", "/api/export?format=json", Some("sess-1"), None).await;
    assert_eq!((st, js(&b)), (StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"Export failed","message":"SQLITE_ERROR: no such table: card_cache"})));
    let (st, _, b) = call(&h.router, "POST", "/api/export", Some("sess-1"), Some(r#"{"entry_ids":[7]}"#)).await;
    assert_eq!((st, js(&b)), (StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"Export failed","message":"SQLITE_ERROR: no such table: card_cache"})));
}

#[tokio::test]
async fn export_array_ecosystem_never_502() {
    let h = harness().await;
    let (st, _, b) = call(&h.router, "GET", "/api/export?ecosystem=a&ecosystem=b", Some("sess-1"), None).await;
    assert_eq!((st, js(&b)), (StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"Internal server error"})));
}
