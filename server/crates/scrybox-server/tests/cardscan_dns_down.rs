//! LANE B unit 6a (DNS arm) — separate binary from cardscan_sidecar_down.rs
//! because cardscan.rs base_url() is a PROCESS-WIDE boot snapshot (mirrors
//! Node's module-load `const BASE = process.env.CARDSCAN_URL ...`): two
//! tests in one binary would share whichever URL snapshotted first, and the
//! second test would silently exercise the wrong failure mode (both map to
//! the same upstreamError 503 body, so it passed vacuously — iter-19 fix).

use std::{collections::BTreeSet, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    routing::any,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::Value;
use tower::ServiceExt;

fn seeded_db(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("t.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');",
    )
    .unwrap();
    path
}

struct Harness {
    router: Router,
    _dir: tempfile::TempDir,
}

async fn harness(url: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    std::env::set_var("CARDSCAN_URL", url);
    let node = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, Router::new().fallback(any(|| async { (StatusCode::OK, "node") })))
                .await
                .unwrap()
        });
        a
    };
    Harness {
        router: app(AppState::new(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            node_upstream: format!("http://{node}"),
            static_dir: None,
            rust_routes: ["cardscan"].into_iter().map(String::from).collect::<BTreeSet<_>>(),
            db_path: Some(db),
            sidecar_url: None,
        })),
        _dir: dir,
    }
}

#[tokio::test]
async fn dns_failure_is_503_unavailable() {
    let h = harness("http://cardscan-does-not-exist.invalid:8321").await;
    let req = Request::builder().method("GET").uri("/api/cardscan/status").header("authorization", "Bearer sess-1").body(Body::empty()).unwrap();
    let resp = h.router.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
    // status GET also maps through upstreamError on DNS failure.
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v, serde_json::json!({"ok":false,"unavailable":true,"error":"Scanner service is not running"}));
}
