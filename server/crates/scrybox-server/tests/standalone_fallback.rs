//! MVP image regression: in Node-free standalone mode (empty
//! SCRYBOX_NODE_UPSTREAM) the router fallback is the last stop — there is no
//! Node behind it to stamp helmet/COEP/cors or to answer finalhandler's 404.
//!
//! Live-pinned against the Node test instance (:3312), GET /api/does-not-exist:
//!   no token  -> 401 {"error":"Access denied. No token provided."} WITH the
//!                full helmet set (X-Frame-Options, COOP, COEP, CSP-RO, ...)
//!   valid tok -> 404 "Cannot GET /api/does-not-exist" (finalhandler), same
//!                helmet set.
//! Red before the fix: the 401 had no security headers at all and the
//! authenticated request answered 502 {"error":"Upstream unavailable"}.
use axum::{body::Body, extract::Request, http::StatusCode};
use scrybox_server::migrations::{run_boot_migration, MigOpts};
use scrybox_server::{app, AppState, Config};

const PW: &str = concat!("Mvp-", "fallback-", "q7z");

fn helmet_present(h: &axum::http::HeaderMap) {
    for (k, v) in [
        ("x-frame-options", "SAMEORIGIN"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        ("cross-origin-embedder-policy", "credentialless"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("origin-agent-cluster", "?1"),
    ] {
        assert_eq!(h.get(k).map(|v| v.to_str().unwrap()), Some(v), "header {k}: {h:?}");
    }
    assert!(h.contains_key("content-security-policy-report-only"), "{h:?}");
}

#[tokio::test]
async fn standalone_fallback_has_helmet_and_finalhandler_404() {
    run_case(false).await;
}

/// Same pins with the SPA mounted (the shipped image always has
/// SCRYBOX_STATIC_DIR): unmatched /api/* then reaches the proxy through the
/// SPA wildcard route, which used to tag the reply NodeProxied so the guard
/// skipped it — with no Node to have stamped anything.
#[tokio::test]
async fn standalone_spa_api_miss_has_helmet_and_finalhandler_404() {
    run_case(true).await;
}

async fn run_case(with_spa: bool) {
    let dir = tempfile::tempdir().unwrap();
    let spa_dir = dir.path().join("dist");
    std::fs::create_dir_all(&spa_dir).unwrap();
    std::fs::write(spa_dir.join("index.html"), "<!doctype html><title>t</title>").unwrap();
    let db = dir.path().join("bindarr.db");
    run_boot_migration(&db, &MigOpts::default()).unwrap();
    let salt = "0123456789abcdef0123456789abcdef";
    let iters = 10_000u32;
    let h = scrybox_server::auth_support::pbkdf2_sha512(PW.as_bytes(), salt.as_bytes(), iters);
    let mut stored = format!("{iters}:{salt}:");
    for b in h {
        stored.push_str(&format!("{b:02x}"));
    }
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO users (username, password_hash, role, share_token, share_enabled)
             VALUES ('admin', ?1, 'admin', 'fbtokfbtokfbtokfb', 0)",
            rusqlite::params![stored],
        )
        .unwrap();

    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: String::new(),
        static_dir: with_spa.then(|| spa_dir.clone()),
        rust_routes: ["*".to_string()].into_iter().collect(),
        db_path: Some(db),
        sidecar_url: None,
    };
    let router = app(AppState::new(cfg));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap()
    });
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<Body>();
    let send = |req: Request| {
        let client = client.clone();
        async move {
            let (mut parts, body) = req.into_parts();
            parts.uri = format!("http://{addr}{}", parts.uri).parse().unwrap();
            let res = client.request(Request::from_parts(parts, body)).await.unwrap();
            let (p, b) = res.into_parts();
            axum::response::Response::from_parts(p, Body::new(b))
        }
    };

    // Unauthenticated unknown /api path: 401 + helmet.
    let res = send(Request::get("/api/does-not-exist").body(Body::empty()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    helmet_present(res.headers());

    // Login, then the same path authenticated: finalhandler 404 + helmet.
    let res = send(
        Request::post("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"username":"admin","password":"{PW}"}}"#)))
            .unwrap(),
    )
    .await;
    assert!(res.status().is_success(), "login {}", res.status());
    let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let tok = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let res = send(
        Request::get("/api/does-not-exist")
            .header("authorization", format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    helmet_present(res.headers());
    let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&b).contains("<pre>Cannot GET /api/does-not-exist</pre>"));
}
