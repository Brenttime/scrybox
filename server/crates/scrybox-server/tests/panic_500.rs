//! LANE B — REVIEW-errors H2: a handler panic must answer Node's generic
//! error handler (server.js:642 `res.status(500).json({error:'Internal
//! server error'})`) instead of hyper resetting the connection.
//!
//! The route under test is the env-gated `/api/__test_panic` in lib.rs
//! (SCRYBOX_TEST_PANIC_ROUTE), registered inside the REAL app() so it rides
//! the exact production layer stack: CatchPanicLayer INNER of the
//! secheaders guard, so the panic-500 comes back out through the guard and
//! gets the same COEP/helmet/cors twin headers Node's stack set before the
//! route threw. Red before the fix (catcher removed): the panic unwinds
//! through oneshot and the test binary dies instead of returning a 500.
//!
//! Byte-level pins: body `{"error":"Internal server error"}` (33B), CT
//! `application/json; charset=utf-8`, express res.send stamp order
//! CT→CL→ETag with ETag W/"21-<b64 sha1[..27]>" (same twin as
//! static_assets::express_json, live-pinned there at :3312).

use std::{collections::BTreeSet, net::SocketAddr};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use scrybox_server::{app, AppState, Config};
use serde_json::json;
use tower::ServiceExt;

const PANIC_BODY: &str = "{\"error\":\"Internal server error\"}";
const PANIC_ETAG: &str = "W/\"21-u8tno/8IdqEY6PFcopkQe0syfE4\"";

async fn spawn_fake_node() -> SocketAddr {
    let echo = any(|req: Request| async move {
        let mut out = HeaderMap::new();
        out.insert(
            HeaderName::from_static("x-node"),
            HeaderValue::from_static("1"),
        );
        (
            StatusCode::OK,
            out,
            axum::Json(json!({"path": req.uri().path()})),
        )
            .into_response()
    });
    let health = get(|| async {
        let mut out = HeaderMap::new();
        out.insert(HeaderName::from_static("x-node"), HeaderValue::from_static("1"));
        (StatusCode::OK, out, axum::Json(json!({"status":"ok"}))).into_response()
    });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

async fn fixture() -> Router {
    let node = spawn_fake_node().await;
    // Read by app() at build time: registers the always-panicking route.
    std::env::set_var("SCRYBOX_TEST_PANIC_ROUTE", "1");
    // cors twin (secheaders::origin_allowed, per-call env read): the
    // explicit allow-list — private origins would make the reject-arm test
    // depend on the host network.
    std::env::set_var("CORS_ORIGIN", "https://scrybox.app");
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: BTreeSet::new(),
        db_path: None,
        sidecar_url: None,
    };
    app(AppState::new(cfg))
}

async fn get_routed(r: &Router, path: &str, ua: Option<&str>, origin: Option<&str>) -> axum::response::Response {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(ua) = ua {
        b = b.header(header::USER_AGENT, ua);
    }
    if let Some(o) = origin {
        b = b.header(header::ORIGIN, o);
    }
    r.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
}

async fn body_str(res: axum::response::Response) -> String {
    String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned()
}

// The core red-before-fix assertion: a panicking native handler answers
// Node's 500 body/status/CT/CL/ETag instead of resetting the connection.
#[tokio::test]
async fn panic_answers_node_500_bytes() {
    let r = fixture().await;
    let res = get_routed(&r, "/api/__test_panic", Some("curl/8.0"), None).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR, "Node answers 500 on handler error");
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "application/json; charset=utf-8",
        "res.json twin"
    );
    assert_eq!(res.headers()[header::CONTENT_LENGTH], PANIC_BODY.len().to_string());
    assert_eq!(res.headers()[header::ETAG], PANIC_ETAG, "etag package twin over the 500 body");
    // Header stamp order: express res.json emits Content-Type before
    // Content-Length, which is inserted before the ETag callback writes it.
    let names: Vec<String> = res
        .headers()
        .keys()
        .map(|k| k.as_str().to_string())
        .collect();
    let ct = names.iter().position(|n| n == "content-type").unwrap();
    let cl = names.iter().position(|n| n == "content-length").unwrap();
    let et = names.iter().position(|n| n == "etag").unwrap();
    assert!(ct < cl && cl < et, "CT→CL→ETag stamp order, got {names:?}");
    assert_eq!(body_str(res).await, PANIC_BODY);
}

// The catcher sits INNER of the secheaders guard: the 500 must carry the
// full helmet-8/COEP/cors twin stack, in guard order, exactly like any
// other native reply (Node's helmet/cors ran before the route threw).
#[tokio::test]
async fn panic_500_carries_security_headers_in_guard_order() {
    let r = fixture().await;
    let res = get_routed(&r, "/api/__test_panic", Some("Mozilla/5.0 (X11; Linux x86_64)"), Some("https://scrybox.app")).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let h = res.headers();
    // The twin set actually emitted by secheaders (helmet_pre/post + COEP),
    // asserted here to prove the panic path is decorated exactly like a
    // normal native reply.
    assert_eq!(h["cross-origin-embedder-policy"], "credentialless", "browser UA gets the COEP twin");
    assert_eq!(
        h["content-security-policy-report-only"],
        scrybox_server::secheaders::CSP_REPORT_ONLY,
        "the exact CSP the guard stamps on every native reply"
    );
    assert_eq!(h["cross-origin-opener-policy"], "same-origin");
    assert_eq!(h["cross-origin-resource-policy"], "same-origin");
    assert_eq!(h["origin-agent-cluster"], "?1");
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["x-dns-prefetch-control"], "off");
    assert_eq!(h["x-download-options"], "noopen");
    assert_eq!(h["x-frame-options"], "SAMEORIGIN");
    assert_eq!(h["x-permitted-cross-domain-policies"], "none");
    assert_eq!(h["x-xss-protection"], "0");
    assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], "https://scrybox.app", "cors twin stamps the allowed origin");
    assert_eq!(h[header::VARY], "User-Agent, Origin, Accept-Encoding", "compressible non-empty JSON + UA + Origin");
    let names: Vec<String> = h.keys().map(|k| k.as_str().to_string()).collect();
    let coep = names.iter().position(|n| n == "cross-origin-embedder-policy").unwrap();
    let vary = names.iter().position(|n| n == "vary").unwrap();
    let csp = names.iter().position(|n| n == "content-security-policy-report-only").unwrap();
    let acao = names.iter().position(|n| n == "access-control-allow-origin").unwrap();
    let ct = names.iter().position(|n| n == "content-type").unwrap();
    assert!(coep < vary && vary < csp, "guard stamp order preserved on the panic path: {names:?}");
    assert!(acao < ct, "ACAO lands with the guard block, before the express tail: {names:?}");
    assert_eq!(body_str(res).await, PANIC_BODY);
}

// Guard short-circuits still win over the panic: a rejected CORS origin is
// the 403 twin and the handler is never reached (no panic, no 500).
#[tokio::test]
async fn rejected_origin_short_circuits_before_panic() {
    let r = fixture().await;
    let res = get_routed(&r, "/api/__test_panic", Some("Mozilla/5.0 (X11)"), Some("https://evil.test")).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let h = res.headers();
    // Rejected origin: guard's 403 twin runs COEP+Vary:User-Agent + full
    // helmet set first (Node ran them before cors threw), then the express
    // CT/ETag tail with hyper's CL last. NO vary:Origin (cors threw before
    // adding it — pinned in secheaders' own reject test) and no ACAO.
    let names: Vec<String> = h.keys().map(|k| k.as_str().to_string()).collect();
    assert_eq!(names[0], "cross-origin-embedder-policy");
    assert_eq!(names[1], "vary");
    assert_eq!(h[header::VARY], "User-Agent");
    assert!(!names.iter().any(|n| n == "access-control-allow-origin"), "rejected origin never gets ACAO");
    let tail = &names[names.len() - 3..];
    assert_eq!(tail, ["content-type", "etag", "content-length"], "express CT→ETag stamp, CL appended: {names:?}");
    assert_eq!(body_str(res).await, "{\"error\":\"Origin not allowed\"}");
}

// Positive control: the extra layer must not disturb normal traffic —
// /api/health still proxies Node's reply (x-node: 1) with the guard stamp.
#[tokio::test]
async fn normal_route_unaffected_by_catcher() {
    let r = fixture().await;
    let res = get_routed(&r, "/api/health", Some("curl/8.0"), None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["x-node"], "1", "proxy twin untouched");
    // Guard's normal path always leads "User-Agent, Origin" (compose at
    // :444) and adds Accept-Encoding for the non-empty JSON health body.
    assert_eq!(res.headers()[header::VARY], "User-Agent, Origin, Accept-Encoding");
}
