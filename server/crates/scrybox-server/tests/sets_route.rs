//! Native `GET /api/sets` tests (Lane B unit 1): auth gate, byte-shape of
//! the flat list, Cache-Control/ETag/304/gzip behaviour, and the ?tree=1
//! family fold against the mock Scryfall list.

use std::{collections::BTreeSet, net::SocketAddr, sync::Arc};

use axum::{
    extract::Request,
    http::{header, StatusCode},
    Router,
};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use tower::ServiceExt;

use scrybox_server::{
    app, collection::body_etag,
    sets::ScryfallSetsMemo,
    AppState, Config,
};

#[path = "mock_scryfall.rs"]
mod mock_scryfall;
use mock_scryfall::spawn_mock;

async fn spawn_fake_node() -> SocketAddr {
    let echo = axum::routing::any(|req: Request| async move {
        let parts = req.into_parts().0;
        (
            StatusCode::OK,
            [("x-node", "1")],
            axum::Json(json!({"pq": parts.uri.path_and_query().unwrap().as_str()})),
        )
    });
    let health = axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({"status":"ok"}))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    use axum::Router;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

fn seeded_db(dir: &std::path::Path, name: &str) -> Connection {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(include_str!("scry_schema.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, api_key, share_token) VALUES (1,'u1','x','admin','tok-api','share-1');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-session', 1, '2099-01-01 00:00:00');",
    )
    .unwrap();
    conn
}

async fn router_for(db_path: std::path::PathBuf, routes: &[&str], upstream: &str) -> Router {
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

fn seed_sets(conn: &Connection) -> Vec<Map<String, Value>> {
    let rows = vec![
        ("mtg-fdn", "Foundation", "Foundation", 235, 275, "2024-11-15", "fdn", "https://c/fdn.svg", "https://c/fdn.svg"),
        ("mtg-40k", "40K Universe", "40K", 236, 260, "2025-08-01", "40k", "https://c/40k.svg", "https://c/40k.svg"),
    ];
    let mut expected = Vec::new();
    for (id, name, series, pt, total, rel, code, sym, logo) in &rows {
        conn.execute(
            "INSERT INTO sets (id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![id, name, series, pt, total, rel, code, sym, logo],
        )
        .unwrap();
        let mut m = Map::new();
        m.insert("id".into(), json!(id));
        m.insert("name".into(), json!(name));
        m.insert("series".into(), json!(series));
        m.insert("printed_total".into(), json!(pt));
        m.insert("total".into(), json!(total));
        m.insert("release_date".into(), json!(rel));
        m.insert("set_code".into(), json!(code));
        m.insert("symbol_url".into(), json!(sym));
        m.insert("logo_url".into(), json!(logo));
        expected.push(m);
    }
    expected
}

#[tokio::test]
async fn sets_flat_list_byte_shape_and_headers() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "sets.db");
    let expected = seed_sets(&conn);
    let node = spawn_fake_node().await;
    let router = router_for(dir.path().join("sets.db"), &["sets"], &format!("http://{node}")).await;

    let expected_body = Value::Array(expected.into_iter().map(Value::Object).collect()).to_string();

    let res = router
        .clone()
        .oneshot(
            Request::get("/api/sets")
                .header("authorization", "Bearer tok-session")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/json; charset=utf-8");
    assert_eq!(res.headers()["cache-control"], "private, max-age=3600");
    let etag = res.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(etag, body_etag(expected_body.as_bytes()));
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], expected_body.as_bytes(), "flat list must be byte-identical");

    // Conditional GET replays as 304.
    let res = router
        .clone()
        .oneshot(
            Request::get("/api/sets")
                .header("authorization", "Bearer tok-session")
                .header("if-none-match", etag.as_str())
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);

    // gzip when offered and over threshold: build a big list so compression
    // engages (>1KB).
    for i in 0..40 {
        conn.execute(
            "INSERT INTO sets (id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url)
             VALUES (?1,?2,'S',10,10,'2020-01-01',?1,'s','s')",
            rusqlite::params![format!("mtg-big{i}"), format!("Big Set {i} padding padding padding")],
        )
        .unwrap();
    }
    let res = router
        .oneshot(
            Request::get("/api/sets")
                .header("authorization", "Bearer tok-session")
                .header("accept-encoding", "gzip")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-encoding"], "gzip");
    // unit-5a stack: COEP mw adds User-Agent, cors adds Origin, compression
    // appends Accept-Encoding (matches the Node instance's curl -D output).
    assert_eq!(res.headers()["vary"], "User-Agent, Origin, Accept-Encoding");
    let gz = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    // Decodes (i.e. is valid gzip) and holds all 42 sets.
    use flate2::read::GzDecoder;
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut GzDecoder::new(&gz[..]), &mut out).unwrap();
    let parsed: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 42);
}

#[tokio::test]
async fn sets_auth_gate_matches_node() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "auth.db");
    seed_sets(&conn);
    let node = spawn_fake_node().await;
    // Not owned by Rust: proxies (control). Owned: the port answers auth.
    let owned = router_for(dir.path().join("auth.db"), &["sets"], &format!("http://{node}")).await;

    let res = owned
        .clone()
        .oneshot(Request::get("/api/sets").body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&body),
        r#"{"error":"Access denied. No token provided."}"#
    );

    let res = owned
        .clone()
        .oneshot(
            Request::get("/api/sets")
                .header("authorization", "Bearer bogus")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // API key GET is allowed (Node: api keys are GET-only).
    let res = owned
        .clone()
        .oneshot(
            Request::get("/api/sets")
                .header("authorization", "Bearer tok-api")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // POST is not owned: proxies to Node.
    let res = owned
        .oneshot(Request::post("/api/sets").body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.headers()["x-node"], "1");
}

#[tokio::test]
async fn sets_tree_folds_children() {
    let (addr, mock) = spawn_mock().await;
    *mock.sets.lock().unwrap() = json!({"data": [
        {"code": "fdn", "name": "Foundation", "set_type": "expansion", "card_count": 235},
        {"code": "PFDN", "name": "Foundation Tokens", "set_type": "token", "card_count": 40,
         "parent_set_code": "FDN"},
        {"code": "tecl", "name": "Secret Lair X", "set_type": "memorabilia", "card_count": 4,
         "parent_set_code": "ecl"},
        {"code": "alb", "name": "Alchemy", "set_type": "alchemy", "card_count": 80,
         "parent_set_code": "dsn", "digital": true}
    ]});
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "tree.db");
    // DB rows: the fdn parent + the PFDN child (must be dropped from the
    // parent list) + an unrelated parent whose only child is digital.
    for (id, code) in [("mtg-fdn", "fdn"), ("mtg-PFDN", "PFDN"), ("mtg-dsn", "dsn")] {
        conn.execute(
            "INSERT INTO sets (id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url)
             VALUES (?1,'N','S',1,1,'2024-01-01',?2,'s','s')",
            rusqlite::params![id, code],
        )
        .unwrap();
    }
    let node = spawn_fake_node().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: ["sets".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(dir.path().join("tree.db")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.scry = Arc::new(scrybox_server::scryfall::Scry::new(scrybox_server::scryfall::ScryEnv {
        local: true,
        local_url: format!("http://{addr}"),
        base_url: format!("http://{addr}"),
        gap_scale: 0.001,
    }));
    st.scryfall_sets = std::sync::Arc::new(std::sync::Mutex::new(None)) as ScryfallSetsMemo;
    let router = app(st);

    let res = router
        .clone()
        .oneshot(
            Request::get("/api/sets?tree=1")
                .header("authorization", "Bearer tok-session")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let parsed: Vec<Value> = serde_json::from_slice(&body).unwrap();
    let ids: Vec<&str> = parsed.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"mtg-fdn"));
    assert!(!ids.contains(&"mtg-PFDN"), "child codes are filtered out of the parent list");
    // dsn keeps its row: its only child is digital (not a child code).
    assert!(ids.contains(&"mtg-dsn"));
    let fdn = parsed.iter().find(|s| s["id"] == "mtg-fdn").unwrap();
    assert_eq!(
        fdn["children"],
        json!([{"code": "PFDN", "name": "Foundation Tokens", "cardCount": 40, "type": "token"}])
    );
    let dsn = parsed.iter().find(|s| s["id"] == "mtg-dsn").unwrap();
    assert_eq!(dsn["children"], json!([]));

    // Scan exclusions drop matching child types (app_settings flags).
    conn.execute("UPDATE app_settings SET scan_exclude_tokens = 1 WHERE id = 1", []).unwrap();
    let mut st = AppState::new(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: ["sets".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(dir.path().join("tree.db")),
        sidecar_url: None,
    });
    st.scry = Arc::new(scrybox_server::scryfall::Scry::new(scrybox_server::scryfall::ScryEnv {
        local: true,
        local_url: format!("http://{addr}"),
        base_url: format!("http://{addr}"),
        gap_scale: 0.001,
    }));
    let router = app(st);
    let res = router
        .oneshot(
            Request::get("/api/sets?tree=1")
                .header("authorization", "Bearer tok-session")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let parsed: Vec<Value> = serde_json::from_slice(&body).unwrap();
    let fdn = parsed.iter().find(|s| s["id"] == "mtg-fdn").unwrap();
    assert_eq!(fdn["children"], json!([]), "excluded token children disappear");
    // …and once the child list no longer contains PFDN, the PFDN row is a
    // parent again (matches routes/sets.js filter order exactly).
    assert!(parsed.iter().any(|s| s["id"] == "mtg-PFDN"));
}
