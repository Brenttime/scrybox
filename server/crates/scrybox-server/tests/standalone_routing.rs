//! Central standalone routing (no Node upstream): Express semantics for
//! EVERY route — HEAD runs GET, case-insensitive + trailing-slash-optional
//! matching (server.js sets neither `case sensitive routing` nor `strict
//! routing`), the global JSON parser before the /api gate, and the admin
//! router's auth+requireAdmin for unmatched /api/admin/*.
use axum::{body::Body, extract::Request, http::StatusCode};
use scrybox_server::migrations::{run_boot_migration, MigOpts};
use scrybox_server::{app, AppState, Config};

const PW: &str = concat!("Mvp-", "routing-", "q7z");

async fn setup(role: &str) -> (std::net::SocketAddr, String) {
    let with_spa = false;
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
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
             VALUES ('admin', ?1, ?2, 'fbtokfbtokfbtokfb', 0)",
            rusqlite::params![stored, role],
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


    let res = send(
        Request::post("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"username":"admin","password":"{PW}"}}"#)))
            .unwrap(),
    )
    .await;
    assert!(res.status().is_success(), "login {}", res.status());
    let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let tok = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["token"].as_str().unwrap().to_string();
    (addr, tok)
}

async fn go(addr: std::net::SocketAddr, method: &str, path: &str, tok: Option<&str>, body: Option<&str>) -> (StatusCode, axum::http::HeaderMap, String) {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http::<Body>();
    let mut b = Request::builder().method(method).uri(format!("http://{addr}{path}"));
    if let Some(t) = tok {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let res = client.request(b.body(Body::from(body.unwrap_or("").to_string())).unwrap()).await.unwrap();
    let (p, body) = res.into_parts();
    let bytes = axum::body::to_bytes(Body::new(body), usize::MAX).await.unwrap();
    (p.status, p.headers, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn head_runs_get_handler_without_body() {
    let (a, tok) = setup("admin").await;
    let (gs, gh, gb) = go(a, "GET", "/api/settings", Some(&tok), None).await;
    assert_eq!(gs, StatusCode::OK, "{gb}");
    let (hs, hh, hb) = go(a, "HEAD", "/api/settings", Some(&tok), None).await;
    assert_eq!(hs, StatusCode::OK);
    assert_eq!(hb, "");
    assert_eq!(hh.get("content-type"), gh.get("content-type"));
    assert_eq!(hh.get("content-length").unwrap().to_str().unwrap(), gb.len().to_string());
    // Unmatched HEAD: finalhandler says "Cannot HEAD" (length pins it).
    let (s, h, b) = go(a, "HEAD", "/api/nope", Some(&tok), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b, "");
    let want = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n</head>\n<body>\n<pre>Cannot HEAD /api/nope</pre>\n</body>\n</html>\n";
    assert_eq!(h.get("content-length").unwrap().to_str().unwrap(), want.len().to_string());
}

#[tokio::test]
async fn case_insensitive_and_trailing_slash() {
    let (a, tok) = setup("admin").await;
    let (_, _, want) = go(a, "GET", "/api/settings", Some(&tok), None).await;
    for p in ["/api/settings/", "/API/Settings", "/Api/SETTINGS/", "/api/settings/?x=1"] {
        let (s, _, b) = go(a, "GET", p, Some(&tok), None).await;
        assert_eq!(s, StatusCode::OK, "{p}: {b}");
        assert_eq!(b, want, "{p}");
    }
    // Unmatched relaxed path still 404s with the path as typed.
    let (s, _, b) = go(a, "DELETE", "/API/Settings/", Some(&tok), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(b.contains("<pre>Cannot DELETE /API/Settings/</pre>"), "{b}");
    // Strict mismatch (double slash) is NOT relaxed.
    let (s, _, _) = go(a, "GET", "/api/settings//", Some(&tok), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unmatched_admin_paths_are_gated() {
    let (a, tok) = setup("member").await;
    let (s, _, b) = go(a, "GET", "/api/admin/nope", None, None).await;
    assert_eq!((s, b.as_str()), (StatusCode::UNAUTHORIZED, r#"{"error":"Access denied. No token provided."}"#));
    let (s, _, b) = go(a, "GET", "/API/Admin/nope", Some(&tok), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
}

#[tokio::test]
async fn body_parser_runs_before_auth_gate() {
    let (a, _tok) = setup("admin").await;
    let (s, _, b) = go(a, "POST", "/api/nope", None, Some("{bad")).await;
    assert_eq!((s, b.as_str()), (StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"Internal server error"}"#));
    let big = format!(r#"{{"a":"{}"}}"#, "x".repeat(1024 * 1024 + 10));
    let (s, _, _) = go(a, "POST", "/api/nope", None, Some(&big)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    // Large-body path: its parser runs AFTER auth -> 401 first.
    let (s, _, _) = go(a, "POST", "/api/import/x", None, Some("{bad")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Non-/api paths still get the global parser.
    let (s, _, _) = go(a, "POST", "/nope", None, Some("{bad")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
}
