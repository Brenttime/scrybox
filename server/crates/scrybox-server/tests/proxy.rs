//! P0 exit-criteria tests: the proxy is transparent and health tracks Node.

use std::{collections::BTreeSet, net::SocketAddr};

use axum::{
    body::Bytes,
    extract::Request,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use scrybox_server::{app, AppState, Config};

/// Fake Node: echoes method, path+query, body and selected headers.
async fn spawn_fake_node(healthy: bool) -> SocketAddr {
    let echo = any(|req: Request| async move {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let h = |k: &str| parts.headers.get(k).map(|v| v.to_str().unwrap().to_string());
        let mut out = HeaderMap::new();
        out.insert("x-node", "1".parse().unwrap());
        out.insert("cross-origin-embedder-policy", "require-corp".parse().unwrap());
        (
            StatusCode::IM_A_TEAPOT,
            out,
            axum::Json(serde_json::json!({
                "method": parts.method.as_str(),
                "pq": parts.uri.path_and_query().unwrap().as_str(),
                "body": String::from_utf8_lossy(&body),
                "auth": h("authorization"),
                "xff": h("x-forwarded-for"),
                "host": h("host"),
                "conn_hdr": h("x-hop"),
            })),
        )
            .into_response()
    });
    let health = get(move || async move {
        if healthy {
            (StatusCode::OK, axum::Json(serde_json::json!({"status":"ok"})))
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"status":"error"})))
        }
    });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

async fn spawn_rust(upstream: String, static_dir: Option<std::path::PathBuf>) -> SocketAddr {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream,
        static_dir,
        rust_routes: BTreeSet::new(),
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let router = app(AppState::new(cfg));
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
    });
    addr
}

/// Tiny HTTP/1.1 client over raw TCP so tests do not pull in reqwest.
async fn raw(addr: SocketAddr, req: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    // Decode chunked bodies crudely: take the JSON object span.
    let body = match (body.find('{'), body.rfind('}')) {
        (Some(a), Some(b)) => body[a..=b].to_string(),
        _ => body.to_string(),
    };
    (status, head.to_lowercase(), body)
}

#[tokio::test]
async fn proxies_method_path_query_body_and_headers() {
    let node = spawn_fake_node(true).await;
    let rust = spawn_rust(format!("http://{node}"), None).await;
    let body = r#"{"username":"a","password":"b"}"#;
    let req = format!(
        "POST /api/auth/login?x=1&y=%20z HTTP/1.1\r\nHost: scrybox.test\r\nAuthorization: Bearer tok\r\nConnection: close, x-hop\r\nx-hop: secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (status, head, resp) = raw(rust, &req).await;
    assert_eq!(status, 418, "upstream status passes through");
    assert!(head.contains("x-node: 1"));
    assert!(head.contains("cross-origin-embedder-policy: require-corp"), "upstream headers untouched");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["method"], "POST");
    assert_eq!(v["pq"], "/api/auth/login?x=1&y=%20z");
    assert_eq!(v["body"], body);
    assert_eq!(v["auth"], "Bearer tok");
    assert_eq!(v["host"], "scrybox.test", "Host is preserved for CORS/PUBLIC_BASE_URL logic");
    assert_eq!(v["xff"], "127.0.0.1", "client IP appended for Node rate limiters");
    assert!(v["conn_hdr"].is_null(), "Connection-named hop-by-hop header stripped");
}

#[tokio::test]
async fn appends_to_existing_forwarded_for() {
    let node = spawn_fake_node(true).await;
    let rust = spawn_rust(format!("http://{node}"), None).await;
    let req = "GET /api/collection HTTP/1.1\r\nHost: h\r\nX-Forwarded-For: 10.0.0.9\r\nConnection: close\r\n\r\n";
    let (_, _, resp) = raw(rust, req).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["xff"], "10.0.0.9, 127.0.0.1");
}

#[tokio::test]
async fn health_ok_when_node_ok() {
    let node = spawn_fake_node(true).await;
    let rust = spawn_rust(format!("http://{node}"), None).await;
    let (status, _, body) = raw(rust, "GET /api/health HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"status":"ok"}"#);
}

#[tokio::test]
async fn health_503_when_node_unhealthy_or_down() {
    let node = spawn_fake_node(false).await;
    let rust = spawn_rust(format!("http://{node}"), None).await;
    let (status, _, _) = raw(rust, "GET /api/health HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status, 503);

    // Nothing listening upstream.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let rust = spawn_rust(format!("http://{dead}"), None).await;
    let (status, _, _) = raw(rust, "GET /api/health HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status, 503);
    let (status, _, body) = raw(rust, "GET /api/stats HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status, 502);
    assert_eq!(body, r#"{"error":"Upstream unavailable"}"#);
}

#[tokio::test]
async fn static_hits_served_misses_fall_through_to_node() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<html>spa</html>").unwrap();
    std::fs::create_dir(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/app.js"), "console.log(1)").unwrap();
    let node = spawn_fake_node(true).await;
    let rust = spawn_rust(format!("http://{node}"), Some(dir.path().to_path_buf())).await;

    let (status, head, _) = raw(rust, "GET /assets/app.js HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status, 200);
    assert!(!head.contains("x-node"), "static file served by Rust");

    let (status, head, _) = raw(rust, "GET /api/stats HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status, 418);
    assert!(head.contains("x-node: 1"), "API miss proxied to Node");

    let (status, head, _) = raw(rust, "GET /decks/42 HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status, 418);
    assert!(head.contains("x-node: 1"), "SPA deep link proxied to Node");
}

#[allow(dead_code)]
fn _unused(_: Bytes) {}
