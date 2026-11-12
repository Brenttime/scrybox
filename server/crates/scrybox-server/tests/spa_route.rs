//! Lane B unit 5c route-level proof: the SPA mount (express.static root +
//! app.get('*') catch-all twins) answers NATIVELY under RUST_ROUTES=static_spa
//! with a build dir, proven with a dead Node upstream — the twin reply headers
//! and bodies below can only come from spa.rs. The internal /api proxy leg of
//! the wildcard (Node's catch-all `req.path.startsWith('/api') => next()`,
//! which for Rust means the router-level Node upstream) runs against a live
//! fake Node and must pass Node's stamped reply through UNTOUCHED (the
//! secheaders NodeProxied marker): no twin re-stamping, no duplicate COEP.
//! Kill-switch arms: group off, or group on without a dist dir, every
//! non-API path proxies to Node, exactly like a dist-less Node install.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use tower::ServiceExt;

use scrybox_server::{app, AppState, Config};

fn dead_upstream() -> String {
    // Port 9 (discard): nothing listens, so any proxy leak is a connect
    // error — the asserts below can only come from the native handlers.
    "http://127.0.0.1:9".to_string()
}

fn router_for(routes: &[&str], static_dir: Option<PathBuf>, upstream: String) -> Router {
    router_for_model(routes, static_dir, upstream, None)
}

fn router_for_model(routes: &[&str], static_dir: Option<PathBuf>, upstream: String, model: Option<PathBuf>) -> Router {
    // owns() gates every native group on a live DB handle — hand the state
    // an empty temp sqlite file (the SPA mount never touches it).
    let d = tempfile::tempdir().unwrap();
    rusqlite::Connection::open(d.path().join("t.db")).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream,
        static_dir,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(d.path().join("t.db")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.model_dir = model;
    std::mem::forget(d);
    app(st)
}

/// A miniature vite tree: hashed /assets chunk, public/ fixed-name file,
/// index.html for the catch-all.
fn spa_tree() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("index.html"), "<html>spa-index</html>").unwrap();
    std::fs::create_dir_all(d.path().join("assets")).unwrap();
    std::fs::write(d.path().join("assets").join("App.abc123.js"), "console.log(1)").unwrap();
    std::fs::write(d.path().join("logo.svg"), "<svg/>").unwrap();
    d
}

/// Fake Node: marks every proxied reply and stamps its OWN COEP (Node's
/// middleware chain already ran) so the passthrough leg can assert the guard
/// neither strips, duplicates, nor re-stamps anything.
async fn spawn_fake_node() -> SocketAddr {
    let echo = any(|req: Request| async move {
        let (parts, _body) = req.into_parts();
        let mut out = axum::http::HeaderMap::new();
        out.insert("x-node", "1".parse().unwrap());
        out.insert("x-node-path", parts.uri.path().parse().unwrap());
        out.insert("cross-origin-embedder-policy", "require-corp".parse().unwrap());
        (StatusCode::IM_A_TEAPOT, out, "node-body").into_response()
    });
    let r = Router::new().route("/api/health", any(|| async { (StatusCode::OK, "{}") })).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

async fn text(res: axum::response::Response) -> (axum::response::Response, String) {
    let (parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    (
        axum::response::Response::from_parts(parts, Body::empty()),
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn get(r: Router, uri: &str) -> axum::response::Response {
    r.oneshot(Request::get(uri).header("user-agent", "curl/8").body(Body::empty()).unwrap())
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Native arms (dead upstream: any proxy leak is a connect error)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn root_and_deep_link_serve_index_natively_with_twin_headers() {
    let d = spa_tree();
    let r = router_for(&["static_spa"], Some(d.path().to_path_buf()), dead_upstream());
    let res = get(r, "/").await;
    assert_eq!(res.status(), StatusCode::OK);
    let h = &res.headers();
    assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=UTF-8");
    // Catch-all sendFile twin: public, max-age=0 (setHeaders never ran).
    assert_eq!(h[header::CACHE_CONTROL], "public, max-age=0");
    assert!(h[header::ETAG].to_str().unwrap().starts_with("W/\""));
    // The unit-5a guard rode the reply: COEP first of the twin class
    // (coepFor twin: non-webkit UA => credentialless).
    let ks: Vec<String> = h.keys().map(|k| k.as_str().to_string()).collect();
    assert_eq!(ks.first().unwrap(), "cross-origin-embedder-policy");
    assert_eq!(h["cross-origin-embedder-policy"], "credentialless");
    // helmet runs with contentSecurityPolicy disabled on SPA/Node replies:
    // the enforced CSP is absent, the REPORT-ONLY twin rides (live :3312 pin).
    assert!(!h.contains_key(header::CONTENT_SECURITY_POLICY));
    assert!(h.contains_key("content-security-policy-report-only"));

    // Deep link: identical bytes + tag (the sendFile path, not a redirect).
    let r2 = router_for(&["static_spa"], Some(d.path().to_path_buf()), dead_upstream());
    let (res2, b2) = text(get(r2, "/collection/deep/x").await).await;
    assert_eq!(res2.status(), StatusCode::OK);
    assert_eq!(b2, "<html>spa-index</html>");
    assert_eq!(res2.headers()[header::ETAG], h[header::ETAG]);
}

#[tokio::test]
async fn static_class_cache_controls_follow_setheaders_twin() {
    let d = spa_tree();
    let r = router_for(&["static_spa"], Some(d.path().to_path_buf()), dead_upstream());
    let res = get(r, "/logo.svg").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/svg+xml");

    let r2 = router_for(&["static_spa"], Some(d.path().to_path_buf()), dead_upstream());
    let res2 = get(r2, "/assets/App.abc123.js").await;
    assert_eq!(res2.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
    assert_eq!(res2.headers()[header::CONTENT_TYPE], "application/javascript; charset=UTF-8");
}

#[tokio::test]
async fn directory_get_redirects_with_the_pinned_document() {
    let d = spa_tree();
    let r = router_for(&["static_spa"], Some(d.path().to_path_buf()), dead_upstream());
    let (res, b) = text(get(r, "/assets").await).await;
    assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(res.headers()[header::LOCATION], "/assets/");
    assert_eq!(res.headers()[header::CONTENT_LENGTH], "156");
    assert!(b.contains("Redirecting to /assets/"), "{b}");
}

#[tokio::test]
async fn non_get_on_spa_land_is_finalhandler_404_natively() {
    let d = spa_tree();
    let r = router_for(&["static_spa"], Some(d.path().to_path_buf()), dead_upstream());
    let res = r
        .oneshot(
            Request::post("/collection")
                .header("user-agent", "curl/8")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let h = res.headers();
    // finalhandler stamp order live-pinned :3312 (POST /collection):
    // CSP, Content-Type, Content-Length (native twin; the guard twins ride
    // ahead of these).
    let ks: Vec<String> = h.keys().map(|k| k.as_str().to_string()).collect();
    let i_ct = ks.iter().position(|k| k == "content-type").unwrap();
    let i_csp = ks.iter().position(|k| k == "content-security-policy").unwrap();
    let i_cl = ks.iter().position(|k| k == "content-length").unwrap();
    assert!(i_csp < i_ct && i_ct < i_cl, "{ks:?}");
    let (_, b) = text(res).await;
    assert!(b.contains("Cannot POST /collection"), "{b}");
}

#[tokio::test]
async fn scan_assets_stays_native_when_both_groups_are_on() {
    // SPA wildcard is registered AFTER the trio routes and matchit prefers
    // the specific mount: the trio must not be swallowed by the wildcard.
    let d = tempfile::tempdir().unwrap();
    let cs = d.path().join("client-scan");
    std::fs::create_dir_all(&cs).unwrap();
    std::fs::write(cs.join("manifest.json"), b"{\"entries\":[]}\n").unwrap();
    let r = router_for_model(
        &["static_spa", "static_assets"],
        Some(d.path().to_path_buf()),
        dead_upstream(),
        Some(d.path().to_path_buf()),
    );
    let res = get(r, "/scan-assets/manifest.json").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-cache");
}

// ---------------------------------------------------------------------------
// The internal /api proxy leg + kill switches (live fake Node)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_under_the_wildcard_proxies_and_guard_passes_node_output_through() {
    let node = spawn_fake_node().await;
    let d = spa_tree();
    let r = router_for(&["static_spa"], Some(d.path().to_path_buf()), format!("http://{node}"));
    let (res, b) = text(get(r, "/api/no-such-ported-route").await).await;
    assert_eq!(res.status(), StatusCode::IM_A_TEAPOT);
    assert_eq!(b, "node-body");
    let h = &res.headers();
    assert_eq!(h["x-node"], "1");
    assert_eq!(h["x-node-path"], "/api/no-such-ported-route");
    // NodeProxied passthrough: Node's own single COEP survives, the guard
    // neither re-stamps (no helmet twin class at all) nor duplicates COEP.
    assert_eq!(h.get_all("cross-origin-embedder-policy").iter().count(), 1);
    assert_eq!(h["cross-origin-embedder-policy"], "require-corp");
    assert!(!h.contains_key(header::CONTENT_SECURITY_POLICY));
    assert!(!h.contains_key("content-security-policy-report-only"));
    assert!(!h.contains_key("x-frame-options"));
}

#[tokio::test]
async fn apifoo_prefix_proxies_like_nodes_startswith_gate() {
    // Node's catch-all gates on req.path.startsWith('/api') — '/apifoo' is
    // NOT SPA land there either; it next()s past every mount to
    // finalhandler. The Rust twin proxies the same prefix to Node.
    let node = spawn_fake_node().await;
    let d = spa_tree();
    let r = router_for(&["static_spa"], Some(d.path().to_path_buf()), format!("http://{node}"));
    let res = get(r, "/apifoo").await;
    assert_eq!(res.headers()["x-node"], "1");
    assert_eq!(res.headers()["x-node-path"], "/apifoo");
}

#[tokio::test]
async fn trio_paths_proxy_when_static_assets_group_is_off() {
    // With static_assets OFF the trio lives behind the SPA wildcard and
    // hands off to Node (which owns those mounts).
    let node = spawn_fake_node().await;
    let d = spa_tree();
    let r = router_for(&["static_spa"], Some(d.path().to_path_buf()), format!("http://{node}"));
    for p in ["/scan-assets/manifest.json", "/models/cornelius.onnx", "/scryfall-portal"] {
        let res = get(r.clone(), p).await;
        assert_eq!(res.headers()["x-node"], "1", "{p} must proxy");
    }
}

#[tokio::test]
async fn kill_switch_group_off_proxies_the_spa_to_node() {
    let node = spawn_fake_node().await;
    let d = spa_tree();
    let r = router_for(&[], Some(d.path().to_path_buf()), format!("http://{node}"));
    let res = get(r, "/collection").await;
    assert_eq!(res.status(), StatusCode::IM_A_TEAPOT);
    assert_eq!(res.headers()["x-node"], "1");
}

#[tokio::test]
async fn group_on_without_build_dir_is_inert() {
    // A dist-less install: Node serves the SPA, Rust stays out of the way
    // even with the group listed.
    let node = spawn_fake_node().await;
    let r = router_for(&["static_spa"], None, format!("http://{node}"));
    let res = get(r, "/collection").await;
    assert_eq!(res.headers()["x-node"], "1");
    let r2 = router_for(&["static_spa"], None, format!("http://{node}"));
    let res2 = get(r2, "/").await;
    assert_eq!(res2.headers()["x-node"], "1");
}
