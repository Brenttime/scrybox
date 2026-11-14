//! Node-free (SCRYBOX_NODE_UPSTREAM empty) /api/search legs that used to
//! forward to Node (and so answered the proxy's 404/502 with Node gone):
//! POST bodies outside the field gate (q, image), `prints=1`, repeated
//! query keys, the `is:<type>` → `t:` rewrite, the raw-q card_cache
//! fallback when Scryfall is down, and the field-search live-merge failure
//! verdicts (429 / 503). Every expectation is Node's own answer.

use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc, Mutex,
    },
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Query, Request},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

type Hits = Arc<Mutex<Vec<String>>>;

async fn mock_scryfall(hits: Hits, fail: Arc<AtomicU16>) -> String {
    let r = Router::new().route(
        "/cards/search",
        get(move |Query(qs): Query<std::collections::HashMap<String, String>>| {
            let hits = hits.clone();
            let fail = fail.clone();
            async move {
                let q = qs.get("q").cloned().unwrap_or_default();
                hits.lock().unwrap().push(q.clone());
                let f = fail.load(Ordering::SeqCst);
                if f != 0 {
                    return (StatusCode::from_u16(f).unwrap(), axum::Json(json!({"object":"error","status":f}))).into_response();
                }
                axum::Json(json!({
                    "object": "list", "total_cards": 2, "has_more": false,
                    "data": [
                        {"object":"card","id":"11111111-aaaa-4aaa-8aaa-111111111111","name":"Lightning Bolt","lang":"en",
                         "set":"lea","set_name":"Alpha","collector_number":"161","rarity":"common","type_line":"Instant",
                         "cmc":1.0,"color_identity":["R"],"image_uris":{"normal":"http://x/a.jpg"},"prices":{"usd":"500.00"}},
                        {"object":"card","id":"22222222-bbbb-4bbb-8bbb-222222222222","name":"Lightning Bolt","lang":"en",
                         "set":"m10","set_name":"Magic 2010","collector_number":"146","rarity":"common","type_line":"Instant",
                         "cmc":1.0,"color_identity":["R"],"image_uris":{"normal":"http://x/b.jpg"},"prices":{"usd":"1.00"}}
                    ]
                }))
                .into_response()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    format!("http://{addr}")
}

struct H {
    router: Router,
    hits: Hits,
    fail: Arc<AtomicU16>,
    _dir: tempfile::TempDir,
}

async fn harness() -> H {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');",
    )
    .unwrap();
    drop(conn);
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicU16::new(0));
    let base = mock_scryfall(hits.clone(), fail.clone()).await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: String::new(),
        static_dir: None,
        rust_routes: ["search".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(db.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.scry = Arc::new(scrybox_server::scryfall::Scry::new(scrybox_server::scryfall::ScryEnv {
        local: false,
        local_url: base.clone(),
        base_url: base,
        gap_scale: 0.0,
    }));
    H { router: app(st), hits, fail, _dir: dir }
}

async fn call(r: &Router, method: &str, qs: &str, body: Option<&str>) -> (StatusCode, axum::http::HeaderMap, Value) {
    call_from(r, "127.0.0.9", method, qs, body).await
}

async fn call_from(r: &Router, ip: &str, method: &str, qs: &str, body: Option<&str>) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut rb = Request::builder()
        .method(method)
        .uri(if qs.is_empty() { "/api/search".to_string() } else { format!("/api/search?{qs}") })
        .header("authorization", "Bearer sess-1")
        .extension(ConnectInfo(SocketAddr::new(ip.parse::<IpAddr>().unwrap(), 5555)));
    if body.is_some() {
        rb = rb.header("content-type", "application/json");
    }
    let res = r.clone().oneshot(rb.body(Body::from(body.unwrap_or("").to_string())).unwrap()).await.unwrap();
    let st = res.status();
    let h = res.headers().clone();
    let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, h, serde_json::from_slice(&b).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into())))
}

#[tokio::test]
async fn standalone_post_q_is_native() {
    let h = harness().await;
    let (st, hd, v) = call_from(&h.router, "127.0.0.77", "POST", "", Some(r#"{"q":"bolt","scope":"internet","limit":250}"#)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v.as_array().map(|a| a.len()), Some(2), "{v}");
    assert_eq!(v[0]["owned_qty"], 0);
    assert_eq!(hd["x-source"], "scryfall");
    assert_eq!(hd["x-total-count"], "2");
    assert_eq!(h.hits.lock().unwrap().as_slice(), ["bolt"]);
    // Rate-limit headers ride exactly once (the internal re-dispatch is
    // not double-counted).
    assert_eq!(hd["ratelimit-remaining"], "299");
}

#[tokio::test]
async fn standalone_post_image_without_model_is_unranked_native() {
    let h = harness().await;
    let (st, _, v) = call(
        &h.router,
        "POST",
        "",
        Some(r#"{"q":"bolt","scope":"internet","image":"data:image/png;base64,AAAA","cropped":true}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let a = v.as_array().unwrap();
    assert_eq!(a.len(), 2);
    // cvScan not built → scoreCards returns the list untouched.
    assert!(a[0].get("score").is_none());
    assert_eq!(a[0]["set_id"], "lea");
}

#[tokio::test]
async fn standalone_prints_leg_is_native() {
    let h = harness().await;
    let (st, hd, v) = call(&h.router, "GET", "name=Lightning+Bolt&prints=1&limit=250", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v.as_array().map(|a| a.len()), Some(2));
    assert_eq!(hd["x-source"], "scryfall");
    assert_eq!(hd["x-total-count"], "2");
    assert!(hd.get("etag").is_some());
    assert_eq!(h.hits.lock().unwrap()[0], "!\"Lightning Bolt\" unique:prints -is:digital");
    // set-scoped variant
    let (st, _, _) = call(&h.router, "GET", "name=Lightning+Bolt&set=lea&prints=1", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h.hits.lock().unwrap()[1], "!\"Lightning Bolt\" set:lea unique:prints -is:digital");
}

#[tokio::test]
async fn standalone_node_crash_shapes_answer_search_failed() {
    let h = harness().await;
    // qs array → `(nameQuery || '').trim` throws → route catch 500.
    let (st, _, v) = call(&h.router, "GET", "name=a&name=b", None).await;
    assert_eq!((st, v), (StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"Search failed"})));
    let (st, _, v) = call(&h.router, "POST", "", Some(r#"{"name":5}"#)).await;
    assert_eq!((st, v), (StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"Search failed"})));
    assert!(h.hits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn standalone_is_type_rewritten_for_scryfall() {
    let h = harness().await;
    let (st, _, _) = call(&h.router, "GET", "q=is%3Ainstant+-c%3Ag&scope=internet", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h.hits.lock().unwrap()[0], "t:instant -c:g");
}

#[tokio::test]
async fn standalone_raw_q_scryfall_down_falls_back_to_cache() {
    let h = harness().await;
    // Warm card_cache with two English printings.
    let (st, _, _) = call(&h.router, "GET", "q=bolt&scope=internet", None).await;
    assert_eq!(st, StatusCode::OK);
    h.fail.store(500, Ordering::SeqCst);
    let (st, hd, v) = call(&h.router, "GET", "q=lightning&scope=internet", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v.as_array().map(|a| a.len()), Some(2), "{v}");
    assert_eq!(hd["x-source"], "cache");
    assert_eq!(hd["x-total-count"], "2");
    // ORDER BY LOWER(name), set_id, number, id
    assert_eq!(v[0]["set_id"], "lea");
    assert_eq!(v[1]["set_id"], "m10");
    // Catalog operator: no local answer → Node's 503.
    let (st, _, v) = call(&h.router, "GET", "q=o%3Adraw&scope=internet", None).await;
    assert_eq!((st, v), (StatusCode::SERVICE_UNAVAILABLE, json!({"error":"Card API is having trouble. Try again in a moment."})));
}

#[tokio::test]
async fn standalone_field_search_live_failure_verdicts() {
    let h = harness().await;
    h.fail.store(500, Ordering::SeqCst);
    let (st, _, v) = call(&h.router, "GET", "name=zzzunknown", None).await;
    assert_eq!((st, v), (StatusCode::SERVICE_UNAVAILABLE, json!({"error":"Card API is having trouble. Try again in a moment."})));
    let (st, _, v) = call(&h.router, "GET", "name=zzzunknown&scope=internet", None).await;
    assert_eq!((st, v), (StatusCode::SERVICE_UNAVAILABLE, json!({"error":"Card API is having trouble. Try again in a moment."})));
}
