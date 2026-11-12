//! LANE B unit 6b — POST /api/scan-match route tests (cvScan.js twin).
//!
//! Arms covered without the ORT runtime (env-gated): kill-switch forward,
//! auth 401, bigJson strict-parse forwards, the validation ladder (Missing
//! image / Invalid image data), the notBuilt 503 (incl. its precedence over
//! the set-TypeError 500 — Node's evaluation order), the set 500, the
//! ORT-absent forward, and the searchLimiter 429 headers. The recognition
//! pipeline itself is pinned by the lib test node_truth_parity_on_eval_corpus
//! (vs the frozen :3312 capture in bench/scan-eval/node-truth.json).
//!
//! CV_MODEL_DIR / ORT_DYLIB_PATH are process-global; every test that reads
//! them holds ENV_LOCK across set+call, so parallel tests cannot interleave
//! different values. Limiter state is per peer IP, and each test gets its
//! own ConnectInfo address.

use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Mutex,
};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

fn env_lock() -> &'static Mutex<()> {
    static L: Mutex<()> = Mutex::new(());
    &L
}

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

async fn harness(routes: &[&str], peer: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    let echo = any(|req: axum::extract::Request| async move {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let mut out = HeaderMap::new();
        out.insert("x-node", HeaderValue::from_static("1"));
        (
            StatusCode::OK,
            out,
            axum::Json(json!({
                "pq": parts.uri.path_and_query().unwrap().as_str(),
                "body": String::from_utf8_lossy(&body),
            })),
        )
            .into_response()
    });
    let health = axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({ "status": "ok" }))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db),
        sidecar_url: None,
    };
    let _ = peer; // peer rides the ConnectInfo extension per call
    Harness { router: app(AppState::new(cfg)), _dir: dir }
}

async fn call(
    r: &Router,
    path: &str,
    peer: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut rb = Request::builder().method("POST").uri(path);
    for &(k, v) in headers {
        rb = rb.header(k, v);
    }
    let req = rb
        .extension(ConnectInfo(SocketAddr::new(peer.parse::<IpAddr>().unwrap(), 55555)))
        .body(Body::from(body.unwrap_or("").to_string()))
        .unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, headers, bytes)
}

use axum::extract::Request;

const JSON_CT: [(&str, &str); 2] =
    [("authorization", "Bearer sess-1"), ("content-type", "application/json")];

fn json_body(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap()
}

fn big_b64(n: usize) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, vec![7u8; n])
}

fn img_body(n: usize) -> String {
    format!(r#"{{"image":"data:image/jpeg;base64,{}"}}"#, big_b64(n))
}

/// The lane's real model dir (is_built → true). None elsewhere → skip arms.
fn real_models() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let m = std::env::var_os("CV_MODEL_DIR").filter(|s| !s.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| root.join("data/models"));
    (m.join("cornelius.onnx").exists() && m.join("milo.onnx").exists() && m.join("milo-mtg.npz").exists())
        .then_some(m)
}

fn peer(n: u8) -> String {
    format!("10.9.{}.{n}", 9)
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_off_proxies_everything() {
    let h = harness(&[], "").await;
    let (st, hd, b) = call(&h.router, "/api/scan-match?t=off", &peer(1), &JSON_CT, Some(r#"{"image":"x"}"#)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
    assert_eq!(json_body(&b)["pq"], "/api/scan-match?t=off");
}

#[tokio::test]
async fn get_is_not_ours_and_forwards() {
    let h = harness(&["scan_match"], "").await;
    let req = Request::builder()
        .method("GET")
        .uri("/api/scan-match")
        .extension(ConnectInfo(SocketAddr::new(peer(2).parse().unwrap(), 1)))
        .header("authorization", "Bearer sess-1")
        .body(Body::empty())
        .unwrap();
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-node"], "1");
}

#[tokio::test]
async fn unauthenticated_is_401() {
    let h = harness(&["scan_match"], "").await;
    let (st, _, b) = call(&h.router, "/api/scan-match", &peer(3), &[], Some(r#"{"image":"x"}"#)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(json_body(&b).get("error").is_some());
}

#[tokio::test]
async fn validation_ladder_missing_and_invalid_image() {
    let h = harness(&["scan_match"], "").await;
    // {} → Missing image
    let (st, _, b) = call(&h.router, "/api/scan-match", &peer(4), &JSON_CT, Some("{}")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Missing image" }));
    // image not a string → Missing image (typeof check)
    let (st, _, b) = call(&h.router, "/api/scan-match", &peer(4), &JSON_CT, Some(r#"{"image":5}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Missing image" }));
    // empty string → Missing image
    let (st, _, b) = call(&h.router, "/api/scan-match", &peer(4), &JSON_CT, Some(r#"{"image":""}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Missing image" }));
    // 99-byte buffer → Invalid image data (buf.length < 100)
    let (st, _, b) = call(&h.router, "/api/scan-match", &peer(4), &JSON_CT, Some(&img_body(99))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Invalid image data" }));
    // array body destructures all-undefined → Missing image
    let (st, _, b) = call(&h.router, "/api/scan-match", &peer(4), &JSON_CT, Some("[1,2]")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Missing image" }));
}

#[tokio::test]
async fn strict_parse_arms_forward_to_node() {
    let h = harness(&["scan_match"], "").await;
    // scalar json body: bigJson strict throws → Node owns the SyntaxError page.
    let (st, hd, b) = call(&h.router, "/api/scan-match?t=scalar", &peer(5), &JSON_CT, Some("5")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
    assert_eq!(json_body(&b)["body"], "5");
    // non-json content-type: bigJson skips, handler sees {} → 400 Missing image.
    let (st, _, b) = call(
        &h.router,
        "/api/scan-match?t=form",
        &peer(5),
        &[("authorization", "Bearer sess-1"), ("content-type", "application/x-www-form-urlencoded")],
        Some("image=x"),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Missing image" }));
    // empty body with json CT: body-parser yields {} → 400 Missing image.
    let (st, _, b) = call(&h.router, "/api/scan-match?t=empty", &peer(5), &JSON_CT, Some("")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "Missing image" }));
}

#[tokio::test]
async fn not_built_language_is_503_and_outranks_set_500() {
    let models = tempfile::tempdir().unwrap(); // empty dir → not built
    let _g = env_lock().lock().unwrap();
    unsafe { std::env::set_var("CV_MODEL_DIR", models.path()) };
    let h = harness(&["scan_match"], "").await;
    let (st, hd, b) = call(&h.router, "/api/scan-match", &peer(6), &JSON_CT, Some(&img_body(120))).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    // ETag/CT pair from the res_json twin, Node key order.
    assert!(hd["etag"].to_str().unwrap().starts_with("W/\""));
    assert_eq!(
        json_body(&b),
        json!({
            "lang": "English",
            "candidates": [],
            "verified": false,
            "notBuilt": true,
            "error": "No scan catalog is built for this language yet. An admin can build one from Settings → Scan catalogs.",
        })
    );
    let vb = json_body(&b);
    let keys: Vec<&str> = vb.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["lang", "candidates", "verified", "notBuilt", "error"]);
    // Node evaluates isBuilt BEFORE parseSetList's TypeError: still 503.
    let (st, _, _) = call(&h.router, "/api/scan-match", &peer(6), &JSON_CT, Some(&format!(r#"{{"image":"data:image/jpeg;base64,{}","set":5}}"#, big_b64(120)))).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "notBuilt outranks the set TypeError");
}

#[tokio::test]
async fn set_typeerror_is_500_when_built() {
    let Some(models) = real_models() else { eprintln!("SKIP: lane models absent"); return };
    let _g = env_lock().lock().unwrap();
    unsafe { std::env::set_var("CV_MODEL_DIR", &models) };
    let h = harness(&["scan_match"], "").await;
    for set in ["5", "true", "{\"a\":1}", "[\"who\"]"] {
        let body = format!(r#"{{"image":"data:image/jpeg;base64,{}","set":{set}}}"#, big_b64(120));
        let (st, _, b) = call(&h.router, "/api/scan-match", &peer(7), &JSON_CT, Some(&body)).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "set={set}");
        assert_eq!(json_body(&b), json!({ "error": "Scan match failed" }));
    }
    // cropped truthy table must not throw either.
    let (st, hd, b) = call(
        &h.router,
        "/api/scan-match",
        &peer(7),
        &JSON_CT,
        Some(&format!(r#"{{"image":"data:image/jpeg;base64,{}","cropped":[]}}"#, big_b64(120))),
    )
    .await;
    // [] truthy → cropped; but ORT is unset in this arm → Forward-to-Node.
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
    assert!(json_body(&b)["body"].as_str().unwrap().contains("cropped"));
}

#[tokio::test]
async fn ort_absent_forwards_the_whole_request() {
    let Some(models) = real_models() else { eprintln!("SKIP: lane models absent"); return };
    let _g = env_lock().lock().unwrap();
    unsafe {
        std::env::set_var("CV_MODEL_DIR", &models);
        std::env::set_var("ORT_DYLIB_PATH", "");
    };
    let h = harness(&["scan_match"], "").await;
    let (st, hd, b) = call(&h.router, "/api/scan-match?t=fwd", &peer(8), &JSON_CT, Some(&img_body(120))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
    assert_eq!(json_body(&b)["pq"], "/api/scan-match?t=fwd");
    // the replayed body is byte-identical, image and all.
    assert!(json_body(&b)["body"].as_str().unwrap().contains(&big_b64(120)));
}

#[tokio::test]
async fn search_limiter_429_carries_ratelet_headers() {
    let Some(models) = real_models() else { eprintln!("SKIP: lane models absent"); return };
    let _g = env_lock().lock().unwrap();
    unsafe {
        std::env::set_var("CV_MODEL_DIR", &models);
        std::env::set_var("ORT_DYLIB_PATH", "");
    };
    let h = harness(&["scan_match"], "").await;
    let p = peer(9); // private bucket: 301 forwards, none shared
    let mut last = (StatusCode::OK, HeaderMap::new(), Vec::new());
    for _ in 0..301 {
        last = call(&h.router, "/api/scan-match", &p, &JSON_CT, Some(&img_body(120))).await;
    }
    let (st, hd, b) = last;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hd["ratelimit-policy"], "300;w=900");
    assert_eq!(hd["ratelimit-limit"], "300");
    assert_eq!(hd["ratelimit-remaining"], "0");
    assert!(hd.get("retry-after").is_some());
    assert_eq!(
        json_body(&b),
        json!({ "error": "Too many searches. Please slow down and try again shortly." })
    );
}

#[tokio::test]
async fn compression_twin_layer_is_route_scoped() {
    // The merged cardscan compression twin rides the scan_match sub-router:
    // with the kill switch off the request forwards and the reply from the
    // (identity) fake Node is NOT buffered/gzipped by our layer.
    let h = harness(&[], "").await;
    let (st, hd, _) = call(
        &h.router,
        "/api/scan-match",
        &peer(10),
        &[("authorization", "Bearer sess-1"), ("content-type", "application/json"), ("accept-encoding", "gzip")],
        Some(&img_body(4000)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
    assert!(hd.get("content-encoding").is_none());
}
