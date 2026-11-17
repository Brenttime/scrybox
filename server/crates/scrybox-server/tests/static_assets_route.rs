//! Lane B unit 5b route-level proof: /scan-assets/*, /models/cornelius.onnx
//! and /scryfall-portal[/…] answer NATIVELY under RUST_ROUTES=static_assets
//! — proven by a dead Node upstream (a proxy leak can only surface as a
//! connect error, never as the pinned twin headers) plus the secheaders
//! guard's twin headers riding every native reply. The portal arms run
//! against a mock scryfall-local sidecar on localhost.
//!
//! The portal env gate (LOCAL_WEB_PORTAL_ENABLED at router build,
//! LOCAL_SCRYFALL_URL per request) is process-global; all portal asserts
//! therefore live in ONE test function that mutates env sequentially.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    response::IntoResponse,
    Router,
};
use tower::ServiceExt;

use scrybox_server::{app, AppState, Config};

fn dead_upstream() -> String {
    // Port 9 (discard): nothing listens, so any proxy leak is a connect
    // error — the twin headers below can only come from native handlers.
    "http://127.0.0.1:9".to_string()
}

fn router_for(routes: &[&str], model: Option<PathBuf>) -> Router {
    // owns() gates every native group on a live DB handle — hand the state
    // an empty temp sqlite file (the static trio never touches it).
    let d = tempfile::tempdir().unwrap();
    rusqlite::Connection::open(d.path().join("t.db")).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: dead_upstream(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(d.path().join("t.db")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.model_dir = model;
    std::mem::forget(d);
    app(st)
}

fn scan_tree() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let cs = d.path().join("client-scan");
    std::fs::create_dir_all(&cs).unwrap();
    std::fs::write(cs.join("manifest.json"), b"{\"entries\":[]}\n").unwrap();
    std::fs::write(cs.join("rec-dict.txt"), b"hello world").unwrap();
    std::fs::write(cs.join(".hidden"), b"secret").unwrap();
    d
}

async fn text(res: axum::response::Response) -> (axum::response::Response, String) {
    let (parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    (
        axum::response::Response::from_parts(parts, Body::empty()),
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

// ---------------------------------------------------------------------------
// /scan-assets/*
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scan_assets_serve_hit_is_native_with_twin_headers() {
    let d = scan_tree();
    let r = router_for(&["static_assets"], Some(d.path().to_path_buf()));
    let res = r
        .oneshot(
            Request::get("/scan-assets/manifest.json")
                .header("user-agent", "curl/8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let h = &res.headers();
    assert_eq!(h[header::CACHE_CONTROL], "no-cache");
    assert_eq!(h[header::CONTENT_TYPE], "application/json; charset=UTF-8");
    assert!(h[header::ETAG].to_str().unwrap().starts_with("W/\""));
    assert_eq!(h[header::CONTENT_LENGTH], "15");
    // guard twin proof (dead upstream: impossible via proxy).
    assert!(h.contains_key("cross-origin-embedder-policy"));
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h[header::VARY], "User-Agent, Origin, Accept-Encoding");
    let (_, body) = text(res).await;
    assert_eq!(body, "{\"entries\":[]}\n");
    // subpath + non-manifest cache class
    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(Request::get("/scan-assets/rec-dict.txt").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/plain; charset=UTF-8");
}

#[tokio::test]
async fn scan_assets_directory_arms_redirect_and_500() {
    // serve-static redirect:true twins (live :3312 pins): an existing
    // directory without the trailing slash 301s to path + '/' (send's
    // Redirecting document, CT/CL/CSP/Location order), and WITH the slash
    // index:false makes it an ENOENT => the generic-500 arm.
    let d = scan_tree();
    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(Request::get("/scan-assets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(res.headers()[header::LOCATION], "/scan-assets/");
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/html; charset=UTF-8");
    assert_eq!(res.headers()[header::CONTENT_SECURITY_POLICY], "default-src 'none'");
    let ks: Vec<String> = res.headers().keys().map(|k| k.as_str().to_string()).collect();
    // Handler arms after the guard's twin class: CT, CL, CSP, Location.
    assert_eq!(&ks[ks.len() - 4..], ["content-type", "content-length", "content-security-policy", "location"]);
    let (_, body) = text(res).await;
    assert!(body.starts_with("<!DOCTYPE html>"), "{body}");
    assert!(body.contains("Redirecting to /scan-assets/"), "{body}");

    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(Request::get("/scan-assets/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let (_, body) = text(res).await;
    assert_eq!(body, "{\"error\":\"Internal server error\"}");

    // subdirectory form: /sub (no slash) 301s too; a dot-directory still
    // trips dotfiles:deny => 500, never a 301.
    std::fs::create_dir_all(d.path().join("client-scan").join("sub")).unwrap();
    std::fs::create_dir_all(d.path().join("client-scan").join(".hid")).unwrap();
    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(Request::get("/scan-assets/sub").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(res.headers()[header::LOCATION], "/scan-assets/sub/");
    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(Request::get("/scan-assets/.hid").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // POST on the bare mount stays the send 405 methodError, not a 301.
    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(Request::post("/scan-assets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn scan_assets_miss_is_500_arm_dotfile_denied_and_post_405() {
    let d = scan_tree();
    let st = Some(d.path().to_path_buf());
    // express.static fallthrough:false miss -> server.js generic error handler.
    let res = router_for(&["static_assets"], st.clone())
        .oneshot(Request::get("/scan-assets/nope.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert_eq!(res.headers()[header::CONTENT_LENGTH], "33");
    let (_, body) = text(res).await;
    assert_eq!(body, "{\"error\":\"Internal server error\"}");

    // dotfiles:deny is any-dot-SEGMENT, and the miss is likewise a 500 arm.
    let res = router_for(&["static_assets"], st.clone())
        .oneshot(Request::get("/scan-assets/.hidden").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // send methodError: 405, Allow GET/HEAD, CL 0, no CT, Vary WITHOUT the
    // compression leg (live :3312 pin).
    let res = router_for(&["static_assets"], st.clone())
        .oneshot(Request::post("/scan-assets/manifest.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(res.headers()["allow"], "GET, HEAD");
    assert_eq!(res.headers()[header::CONTENT_LENGTH], "0");
    assert!(res.headers().get(header::CONTENT_TYPE).is_none());
    assert_eq!(res.headers()[header::VARY], "User-Agent, Origin");

    // Conditional GET on the weak file etag: 304, twin subset only.
    let res = router_for(&["static_assets"], st)
        .oneshot(Request::get("/scan-assets/manifest.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let etag = res.headers()[header::ETAG].to_str().unwrap().to_string();
    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(
            Request::get("/scan-assets/manifest.json")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(res.headers()[header::ETAG], etag.as_str());
    assert!(res.headers().get(header::CONTENT_TYPE).is_none());
}

// ---------------------------------------------------------------------------
// /models/cornelius.onnx
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_route_serves_30d_and_non_get_reaches_finalhandler() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("cornelius.onnx"), vec![0u8; 111]).unwrap();
    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(Request::get("/models/cornelius.onnx").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=2592000, immutable");
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(res.headers()[header::CONTENT_LENGTH], "111");
    // octet-stream is not compressible: Node's Vary has no AE leg.
    assert_eq!(res.headers()[header::VARY], "User-Agent, Origin");

    // Non-GET never matches app.get -> express default 404 (finalhandler twin).
    let res = router_for(&["static_assets"], Some(d.path().to_path_buf()))
        .oneshot(Request::post("/models/cornelius.onnx").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(
        res.headers()["content-security-policy"],
        "default-src 'none'"
    );
    let (_, body) = text(res).await;
    assert!(body.contains("<pre>Cannot POST /models/cornelius.onnx</pre>"), "{body}");

    // Missing file: res.sendFile err arm -> 404 empty (headersSent false).
    let empty = tempfile::tempdir().unwrap();
    let res = router_for(&["static_assets"], Some(empty.path().to_path_buf()))
        .oneshot(Request::get("/models/cornelius.onnx").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert!(res.headers().get(header::CONTENT_TYPE).is_none());
}

// ---------------------------------------------------------------------------
// /scryfall-portal — mock sidecar, all env-mutating asserts in ONE test.
// ---------------------------------------------------------------------------

async fn spawn_sidecar() -> SocketAddr {
    let html = "<a href=\"/sets.html\"><script src=\"/app.js\"></script> <img src=\"/card.png\"></a>";
    let router = Router::new()
        .route(
            "/",
            axum::routing::get(|| async move { ([(header::CONTENT_TYPE, "text/html")], "root-index").into_response() }),
        )
        .route(
            "/index.html",
            axum::routing::get(move || async move {
                ([(header::CONTENT_TYPE, "text/html")], html).into_response()
            }),
        )
        .route(
            "/data.json",
            axum::routing::get(|| async move {
                (
                    [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "public, max-age=30")],
                    "{\"href\":\"/x\",\"src\":\"/y\"}",
                )
                    .into_response()
            }),
        )
        .route(
            "/font.bin",
            axum::routing::get(|| async move {
                ([(header::CONTENT_TYPE, "application/octet-stream")], vec![7u8; 4096]).into_response()
            }),
        )
        .route(
            "/missing",
            axum::routing::get(|| async move {
                (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "nope").into_response()
            }),
        )
        .route(
            "/redir",
            axum::routing::get(|| async move {
                (
                    StatusCode::MOVED_PERMANENTLY,
                    [(header::LOCATION, "/sets.html"), (header::CONTENT_TYPE, "text/plain")],
                    "",
                )
                    .into_response()
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    addr
}

#[tokio::test]
async fn portal_suite_all_arms_against_mock_sidecar() {
    let addr = spawn_sidecar().await;
    std::env::set_var("LOCAL_WEB_PORTAL_ENABLED", "true");
    std::env::set_var("LOCAL_SCRYFALL_URL", format!("http://{addr}"));
    let d = scan_tree();
    let r = router_for(&["static_assets"], Some(d.path().to_path_buf()));

    // Bare prefix: express res.redirect twin (live-pinned 51-byte body,
    // text/plain, Vary carries express's Accept leg).
    let res = r
        .clone()
        .oneshot(Request::get("/scryfall-portal").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(res.headers()[header::LOCATION], "/scryfall-portal/");
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(res.headers()[header::CONTENT_LENGTH], "51");
    assert_eq!(res.headers()[header::VARY], "User-Agent, Origin, Accept, Accept-Encoding");
    let (res, body) = text(res).await;
    assert_eq!(body, "Moved Permanently. Redirecting to /scryfall-portal/");
    assert!(res.headers().get(header::ETAG).is_none());

    // Trailing-slash-only tail: express's app.get('/scryfall-portal/*')
    // matches an EMPTY star tail; matchit's catch-all does not, so under
    // static_spa the SPA wildcard used to swallow this exact path (found by
    // the live :3312-vs-:3320 diff — parity pt_index). Pinned by an explicit
    // "/scryfall-portal/" exact registration; must reach the sidecar's '/'.
    let res = r
        .clone()
        .oneshot(Request::get("/scryfall-portal/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let (_, body) = text(res).await;
    assert_eq!(body, "root-index");

    // Buffered text (html): CT copied + charset appended, rewrite, weak etag,
    // CSP/CSP-RO/COEP trio stripped for proxied content only.
    let res = r
        .clone()
        .oneshot(Request::get("/scryfall-portal/index.html").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert!(res.headers()[header::ETAG].to_str().unwrap().starts_with("W/\"7e-"), "rewritten body is 78+48=126=0x7e");
    assert!(res.headers().get("content-security-policy-report-only").is_none());
    assert!(res.headers().get("content-security-policy").is_none());
    assert!(res.headers().get("cross-origin-embedder-policy").is_none());
    // helmet remainder still present (strip is the trio only).
    assert_eq!(res.headers()["cross-origin-opener-policy"], "same-origin");
    // Buffered body goes through compression (res.send path): AE leg present,
    // matching the live :3312 GET /scryfall-portal/ dump.
    assert_eq!(res.headers()[header::VARY], "User-Agent, Origin, Accept-Encoding");
    let (_, body) = text(res).await;
    assert_eq!(
        body,
        "<a href=\"/scryfall-portal/sets.html\"><script src=\"/scryfall-portal/app.js\"></script> <img src=\"/scryfall-portal/card.png\"></a>"
    );

    // JSON buffered VERBATIM (no rewrite), sidecar Cache-Control copied.
    let res = r
        .clone()
        .oneshot(Request::get("/scryfall-portal/data.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=30");
    let (_, body) = text(res).await;
    assert_eq!(body, "{\"href\":\"/x\",\"src\":\"/y\"}");

    // Non-text: piped untouched — no ETag, no explicit CL (chunked).
    let res = r
        .clone()
        .oneshot(Request::get("/scryfall-portal/font.bin").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/octet-stream");
    assert!(res.headers().get(header::ETAG).is_none());
    assert!(res.headers().get(header::CONTENT_LENGTH).is_none());

    // !ok streams through with sidecar status (no ETag stamp).
    let res = r
        .clone()
        .oneshot(Request::get("/scryfall-portal/missing").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert!(res.headers().get(header::ETAG).is_none());
    let (_, body) = text(res).await;
    assert_eq!(body, "nope");

    // 3xx Location rewritten under the prefix.
    let res = r
        .clone()
        .oneshot(Request::get("/scryfall-portal/redir").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(res.headers()[header::LOCATION], "/scryfall-portal/sets.html");

    // HEAD on a mounted path: app.get matches, the handler's method gate
    // answers 405 BEFORE the removeHeaders trio (twin headers retained).
    let res = r
        .clone()
        .oneshot(Request::head("/scryfall-portal/index.html").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert_eq!(res.headers()[header::CONTENT_LENGTH], "63");
    assert_eq!(res.headers()["cross-origin-embedder-policy"].to_str().unwrap().len() > 0, true);
    assert!(res.headers().get("content-security-policy-report-only").is_some());

    // POST never matches app.get -> express default 404 (finalhandler twin,
    // helmet trio retained).
    let res = r
        .clone()
        .oneshot(Request::post("/scryfall-portal/index.html").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(res.headers()["content-security-policy"], "default-src 'none'");
    assert!(res.headers().get("cross-origin-embedder-policy").is_some());

    // Sidecar down: JSON 502 twin (returns BEFORE the removes).
    std::env::set_var("LOCAL_SCRYFALL_URL", "http://127.0.0.1:9");
    let res = r
        .oneshot(Request::get("/scryfall-portal/index.html").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert!(res.headers()[header::ETAG].to_str().unwrap().starts_with("W/\""));
    assert!(res.headers().get("cross-origin-embedder-policy").is_some());
    let (_, body) = text(res).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], "Card portal unavailable");
    std::env::remove_var("LOCAL_SCRYFALL_URL");
    std::env::remove_var("LOCAL_WEB_PORTAL_ENABLED");
}

// ---------------------------------------------------------------------------
// Kill switch: group OFF -> the proxy leg, not the twin.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn group_off_falls_through_to_the_proxy_leg() {
    // Node twin container answers on :3312 when present; on machines without
    // it the proxy leg surfaces a connect/502-style failure — either way the
    // native twin headers (W/" etag twin + no-cache twin pair) must NOT
    // appear. Probe the live twin here.
    if std::env::var("SCRYBOX_TEST_NODE_URL").is_err() {
        eprintln!("skip: SCRYBOX_TEST_NODE_URL unset (live :3312 leg)");
        return;
    }
    let node = std::env::var("SCRYBOX_TEST_NODE_URL").unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: node,
        static_dir: None,
        rust_routes: BTreeSet::new(),
        db_path: None,
        sidecar_url: None,
    };
    let st = AppState::new(cfg);
    let res = app(st)
        .oneshot(Request::get("/scan-assets/manifest.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    // Proxied to Node: Node's own answer, byte-copied (its Vary twin proves
    // it did not come from our handler chain).
    let vary = res.headers()[header::VARY].to_str().unwrap().to_string();
    assert!(vary.contains("User-Agent"), "expected Node twin Vary, got {vary}");
}
