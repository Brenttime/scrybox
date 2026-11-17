//! Lane B unit 6c: native catalog build controls + modelAssets admin routes
//! (RUST_ROUTES=catalog_build). Auth ladder, list/progress shapes, build
//! lifecycle smoke (start → done → last), stop-when-idle, unknown-game 409,
//! body-parser forwards, models status/download, kill-switch proxying.

use std::{collections::BTreeSet, net::SocketAddr};

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use scrybox_server::{app, AppState, Config};

const SESS: Option<&str> = Some("Bearer tok-session");

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
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

fn seeded_db(dir: &std::path::Path) {
    let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS set_data_gaps (language TEXT, set_id TEXT, reason TEXT);
         INSERT INTO users (id, username, password_hash, role, api_key, share_token) VALUES (1,'u1','x','admin','tok-api','share-1');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-session', 1, '2099-01-01 00:00:00');
         INSERT INTO users (id, username, password_hash, role, share_token) VALUES (2,'u2','x','member','share-2');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-member', 2, '2099-01-01 00:00:00');",
    )
    .unwrap();
}

fn write_shared(dir: &std::path::Path) -> std::path::PathBuf {
    let shared = dir.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(
        shared.join("languages.json"),
        serde_json::to_vec(&json!([
            {"code": "en", "name": "English", "scryfall": "en"},
            {"code": "ja", "name": "Japanese", "scryfall": "ja"}
        ]))
        .unwrap(),
    )
    .unwrap();
    shared
}

async fn router_for(
    dir: &std::path::Path,
    routes: &[&str],
    upstream: &str,
) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(dir.join("db.sqlite")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.shared_dir = Some(write_shared(dir));
    let model = dir.join("models");
    std::fs::create_dir_all(&model).unwrap();
    st.model_dir = Some(model);
    app(st)
}

fn get_uri(uri: &str, auth: Option<&str>) -> Request {
    let mut b = Request::get(uri).body(Body::empty()).unwrap();
    if let Some(t) = auth {
        b.headers_mut().insert("authorization", t.parse().unwrap());
    }
    b
}

fn post_json(uri: &str, auth: Option<&str>, body: &str) -> Request {
    let mut b = Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    if let Some(t) = auth {
        b.headers_mut().insert("authorization", t.parse().unwrap());
    }
    b
}

async fn body_val(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn admin_ladder(r: Router, uri: &str) {
    let res = r.clone().oneshot(get_uri(uri, None)).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_val(res).await, json!({"error": "Access denied. No token provided."}));
    let res = r.clone().oneshot(get_uri(uri, Some("Bearer nope"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = r.clone().oneshot(get_uri(uri, Some("Bearer tok-api"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_val(res).await,
        json!({"error": "API keys cannot access admin endpoints. Use a login session."})
    );
    let res = r.clone().oneshot(get_uri(uri, Some("Bearer tok-member"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Access denied. Administrator privileges required."})
    );
}

#[tokio::test]
async fn catalogs_auth_ladder_and_list_shape() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["catalog_build"], &format!("http://{node}")).await;
    admin_ladder(r.clone(), "/api/admin/catalogs").await;

    let res = r.clone().oneshot(get_uri("/api/admin/catalogs", SESS)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_val(res).await;
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(keys, ["catalogs", "progress", "last"]);
    assert!(v["catalogs"].is_array());

    let res = r.clone().oneshot(get_uri("/api/admin/catalogs/progress", SESS)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_val(res).await;
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(keys, ["progress", "last"]);
}

#[tokio::test]
async fn build_unknown_game_is_409_and_stop_idle_is_false() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["catalog_build"], &format!("http://{node}")).await;

    // Wait out any job another test may have left running (process-global).
    for _ in 0..100 {
        let res = r.clone().oneshot(get_uri("/api/admin/catalogs/progress", SESS)).await.unwrap();
        if body_val(res).await["progress"].is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let res = r
        .clone()
        .oneshot(post_json("/api/admin/catalogs/build", SESS, r#"{"game":"pokemon"}"#))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let v = body_val(res).await;
    assert_eq!(v["error"], "unknown game pokemon");
    assert!(v.get("progress").is_some());

    let res = r.oneshot(post_json("/api/admin/catalogs/stop", SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_val(res).await;
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(keys, ["stopped", "progress"]);
    assert_eq!(v["stopped"], false);
}

#[tokio::test]
async fn build_smoke_runs_to_done_and_last() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["catalog_build"], &format!("http://{node}")).await;

    // Serialize on the process-global job slot.
    let started = loop {
        let res = r.clone().oneshot(get_uri("/api/admin/catalogs/progress", SESS)).await.unwrap();
        if body_val(res).await["progress"].is_null() {
            let res = r
                .clone()
                .oneshot(post_json(
                    "/api/admin/catalogs/build",
                    SESS,
                    r#"{"game":"mtg","skipCache":true,"sets":["zzz"]}"#,
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let v = body_val(res).await;
            assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["progress"]);
            break v["progress"].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let keys: Vec<&str> = started.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(
        keys,
        ["game", "lang", "sets", "phase", "done", "total", "message", "startedAt", "cancelled"]
    );
    assert_eq!(started["game"], "mtg");
    assert_eq!(started["lang"], "English");
    assert_eq!(started["sets"], json!(["zzz"]));
    assert_eq!(started["phase"], "starting");

    // The skipCache + empty-embed job finishes on its own; poll for last.
    let mut last = Value::Null;
    for _ in 0..100 {
        let res = r.clone().oneshot(get_uri("/api/admin/catalogs/progress", SESS)).await.unwrap();
        let v = body_val(res).await;
        if !v["last"].is_null() && v["progress"].is_null() {
            last = v["last"].clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!last.is_null(), "job never reached done");
    assert_eq!(last["phase"], "done");
    assert_eq!(last["message"], "0 cards embedded (0 new, 0 reused, 0 failed)");
    assert_eq!(last["cancelled"], false);
}

#[tokio::test]
async fn build_bad_json_forwards_to_node() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["catalog_build"], &format!("http://{node}")).await;
    let res = r
        .oneshot(post_json("/api/admin/catalogs/build", SESS, "{oops"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("x-node").map(|v| v.to_str().unwrap().to_string()).unwrap_or_else(|| format!("<no x-node, status {}>", res.status().as_u16())), "1");
}

#[tokio::test]
async fn unknown_subpath_falls_through_to_node() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["catalog_build"], &format!("http://{node}")).await;
    let res = r
        .clone()
        .oneshot(get_uri("/api/admin/catalogs/nope", SESS))
        .await
        .unwrap();
    // Not a mounted route — the catch-all proxy owns it.
    assert_eq!(res.headers().get("x-node").map(|v| v.to_str().unwrap().to_string()).unwrap_or_else(|| format!("<no x-node, status {}>", res.status().as_u16())), "1");
}

#[tokio::test]
async fn models_status_shape_and_download_409() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["catalog_build"], &format!("http://{node}")).await;
    admin_ladder(r.clone(), "/api/admin/models").await;

    let res = r.clone().oneshot(get_uri("/api/admin/models", SESS)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_val(res).await;
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(keys, ["dir", "license", "models", "catalogs", "progress", "last"]);
    assert_eq!(v["dir"], dir.path().join("models").to_string_lossy().to_string());
    assert!(v["models"].is_array() && !v["models"].as_array().unwrap().is_empty());
    assert!(v["models"].as_array().unwrap()[0].get("present").unwrap().is_boolean());

    let res = r
        .clone()
        .oneshot(post_json("/api/admin/models/download", SESS, r#"{"what":"bogus"}"#))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(body_val(res).await["error"], "nothing to download for \"bogus\"");

    // Non-GET on /api/admin/models falls through to Node's 404.
    let res = r
        .oneshot(post_json("/api/admin/models", SESS, "{}"))
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").map(|v| v.to_str().unwrap().to_string()).unwrap_or_else(|| format!("<no x-node, status {}>", res.status().as_u16())), "1");
}

#[tokio::test]
async fn kill_switch_off_proxies_everything() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &[], &format!("http://{node}")).await;
    for uri in ["/api/admin/catalogs", "/api/admin/catalogs/progress", "/api/admin/models"] {
        let res = r.clone().oneshot(get_uri(uri, SESS)).await.unwrap();
        assert_eq!(res.headers().get("x-node").unwrap(), "1", "{uri} should proxy");
    }
    let res = r
        .oneshot(post_json("/api/admin/catalogs/build", SESS, "{}"))
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").map(|v| v.to_str().unwrap().to_string()).unwrap_or_else(|| format!("<no x-node, status {}>", res.status().as_u16())), "1");
}
