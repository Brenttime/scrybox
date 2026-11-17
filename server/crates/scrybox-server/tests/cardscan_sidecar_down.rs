//! LANE B unit 6a — the sidecar-down arms. CARDSCAN_URL is a process-wide
//! boot snapshot (cardscan.rs base_url OnceLock mirrors Node's module-load
//! const), so each URL gets its OWN test binary: a second test in the same
//! binary would silently reuse whichever URL snapshotted first.
//! This binary: refused (bound-but-not-listening port) → 503
//! {ok:false,unavailable:true,error:'Scanner service is not running'}.
//! The DNS arm lives in cardscan_dns_down.rs; the junk-body 502 arm lives in
//! the main cardscan_route suite via the mock's 502 route.

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
use serde_json::{json, Value};
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
    // Holds the bound-but-not-listening fd: dropping it frees the port and
    // a bind(0) mock in a sibling binary could claim it (the original
    // port-cursor flake shape).
    _fd: i32,
}

async fn harness(url: &str, fd: i32) -> Harness {
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
        _fd: fd,
    }
}

async fn post(r: &Router, path: &str, ct: &str, body: String) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", "Bearer sess-1")
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

/// Raw AF_INET socket, bound to an ephemeral 127.0.0.1 port, NEVER listen()ed:
/// connects get kernel RST (ECONNREFUSED) while the returned fd keeps the
/// port off-limits to bind(0) elsewhere. Caller must hold the fd for the
/// duration of the test.
fn bound_unlistening_port() -> (i32, u16) {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        assert!(fd >= 0, "socket() failed");
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as u16,
            sin_port: 0,
            sin_addr: libc::in_addr { s_addr: u32::from_be(0x7f00_0001) },
            sin_zero: [0; 8],
        };
        let rc = libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as u32,
        );
        assert_eq!(rc, 0, "bind() failed");
        let mut name: libc::sockaddr_in = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let rc = libc::getsockname(
            fd,
            &mut name as *mut _ as *mut libc::sockaddr,
            &mut len,
        );
        assert_eq!(rc, 0, "getsockname() failed");
        (fd, u16::from_be(name.sin_port))
    }
}

#[tokio::test]
async fn refused_sidecar_is_503_unavailable() {
    // A bound-but-NOT-listening socket: the kernel RSTs every SYN to it
    // (→ the route's ECONNREFUSED arm) and the port stays claimed for the
    // test's lifetime. The old bind-then-drop freed the port, and a bind(0)
    // mock in a sibling test binary used to steal it and answer 200 — the
    // workspace-run flake (std::net::TcpSocket is unstable, hence libc).
    // The tempdir/DB now live in the Harness too: iter-19 root-caused the
    // residual flake to a dropped tempdir deleting the shared OnceLock DB
    // path under workspace scheduling → DbPool::open Err → Rust routes
    // disabled → the proxy answered the fake node's 200.
    let (fd, port) = bound_unlistening_port();
    let h = harness(&format!("http://127.0.0.1:{port}"), fd).await;
    let (st, v) = post(&h.router, "/api/cardscan", "application/json", r#"{"cards":[{"a":1}]}"#.to_string()).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v, json!({"ok":false,"unavailable":true,"error":"Scanner service is not running"}));
    let (st, v) = post(&h.router, "/api/cardscan/detect", "image/jpeg", "J".repeat(200)).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["unavailable"], true);
}
