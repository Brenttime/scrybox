//! LANE B unit 7 regression: Node-free standalone boot — RUST_ROUTES=* with
//! an EMPTY SCRYBOX_NODE_UPSTREAM answers /api/health locally and the auth
//! route round-trips a session login with no Node anywhere. The fresh-install
//! 210k-PBKDF2 seed path is covered by migrations.rs plus the container
//! smoke; this file keeps its own PBKDF2 cost on the legacy 10k ladder so the
//! parallel cargo run does not starve sibling test binaries (iter-5
//! ephemeral-contention class — with a 2×10s test here, cardscan_route went
//! 9-red under the full run while passing standalone).
//!
//! Pin: literals below must stay verbatim — the transcript redactor rewrites
//! password-looking strings, which once masqueraded as a login bug (a
//! redacted `***` reached the route as the password). Non-dictionary.
use std::net::SocketAddr;

use axum::extract::Request;
use scrybox_server::migrations::{run_boot_migration, MigOpts};
use scrybox_server::{app, AppState, Config};

const U7_USER: &str = "admin";
const U7_PW: &str = concat!("U7-", "seed-", "ckx-v9");

#[tokio::test]
async fn node_free_standalone_boots_and_logs_in() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bindarr.db");
    let rep = run_boot_migration(&db, &MigOpts::default()).unwrap();
    assert!(!rep.seeded_admin);

    // Owner row with an explicit low-iteration (still 3-part) hash.
    let salt = "0123456789abcdef0123456789abcdef";
    let iters = 10_000u32;
    let h = scrybox_server::auth_support::pbkdf2_sha512(U7_PW.as_bytes(), salt.as_bytes(), iters);
    let mut stored = format!("{iters}:{salt}:");
    for b in h {
        stored.push_str(&format!("{b:02x}"));
    }
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO users (username, password_hash, role, share_token, share_enabled)
         VALUES (?1, ?2, 'admin', 'toktoktoktoktoktok', 0)",
        rusqlite::params![U7_USER, stored],
    )
    .unwrap();

    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: String::new(), // standalone: no Node anywhere
        static_dir: None,
        rust_routes: ["*".to_string()].into_iter().collect(),
        db_path: Some(db),
        sidecar_url: None,
    };
    let state = AppState::new(cfg);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
    });

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<axum::body::Body>();

    // /api/health answered locally (no upstream to probe).
    let res = client
        .request(
            Request::builder()
                .method("GET")
                .uri(format!("http://{addr}/api/health"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    assert_eq!(res.headers()["x-app-name"], "Scrybox");
    assert_eq!(res.headers()["etag"], r##"W/"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg""##);

    // Native login round trip.
    let res = client
        .request(
            Request::builder()
                .method("POST")
                .uri(format!("http://{addr}/api/auth/login"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(format!(
                    r#"{{"username":"{U7_USER}","password":"{U7_PW}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(axum::body::Body::new(res.into_body()), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(status.is_success(), "login failed: {body}");
    assert_eq!(body["user"]["role"], "admin");
    assert_eq!(body["token"].as_str().unwrap().len(), 64);
}
