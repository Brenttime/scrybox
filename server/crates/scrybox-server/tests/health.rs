//! Iter-17 tests: the cached `/api/health` handler serves Node's healthy
//! response from memory while fresh, re-probes when stale, degrades to the
//! live 503 path when Node goes bad, and honours the kill switch.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::Request,
    http::StatusCode,
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use scrybox_server::{app, AppState, Config};

/// Fake Node: counts `/api/health` hits, toggles health, answers with the
/// headers Node really sets on that route (X-App-Name + helmet-ish COEP).
async fn spawn_counting_node(healthy: Arc<AtomicBool>) -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = Arc::clone(&hits);
    let health = get(move || {
        let hits = Arc::clone(&hits2);
        let healthy = Arc::clone(&healthy);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let mut h = axum::http::HeaderMap::new();
            h.insert("x-app-name", "Scrybox".parse().unwrap());
            h.insert("cross-origin-embedder-policy", "require-corp".parse().unwrap());
            if healthy.load(Ordering::SeqCst) {
                (StatusCode::OK, h, axum::Json(serde_json::json!({ "status": "ok" }))).into_response()
            } else {
                (StatusCode::SERVICE_UNAVAILABLE, h, axum::Json(serde_json::json!({ "status": "db_unavailable" })))
                    .into_response()
            }
        }
    });
    let r = Router::new().route("/api/health", health).fallback(any(|| async { StatusCode::NOT_FOUND }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    (addr, hits)
}

async fn get_health(addr: SocketAddr) -> (StatusCode, String, Option<String>, Option<String>) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{addr}/api/health"))
        .body(axum::body::Body::empty())
        .unwrap();
    let res = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<axum::body::Body>()
        .request(req)
        .await
        .unwrap();
    let status = res.status();
    let app_name = res.headers().get("x-app-name").map(|v| v.to_str().unwrap().to_string());
    let coep = res.headers().get("cross-origin-embedder-policy").map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(axum::body::Body::new(res.into_body()), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string(), app_name, coep)
}

fn cfg_for(upstream: String) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream,
        static_dir: None,
        rust_routes: Default::default(),
        db_path: None,
        sidecar_url: None,
    }
}

/// A burst served within the TTL must not reach Node after the first probe,
/// and the served response must be Node's own body + headers.
#[tokio::test]
async fn cached_burst_hits_node_once() {
    let healthy = Arc::new(AtomicBool::new(true));
    let (node, hits) = spawn_counting_node(Arc::clone(&healthy)).await;

    let cfg = cfg_for(format!("http://{node}"));
    let state = AppState::new(cfg);
    // Prober parked (long tick) and long TTL: only the handler's own probes
    // can reach Node, and only after TTL expiry.
    state.health_cache.set_intervals(Duration::from_secs(3600), Duration::from_secs(3600));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rust = l.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
    });

    let (status, body, app_name, coep) = get_health(rust).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "{\"status\":\"ok\"}");
    assert_eq!(app_name.as_deref(), Some("Scrybox"));
    assert_eq!(coep.as_deref(), Some("require-corp"));
    let after_first = hits.load(Ordering::SeqCst);
    assert!(after_first >= 1);

    for _ in 0..50 {
        let (s, b, _, _) = get_health(rust).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, "{\"status\":\"ok\"}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), after_first, "cached burst must not re-probe Node");
}

/// Once the TTL expires the next request re-probes Node and refreshes.
#[tokio::test]
async fn ttl_expiry_reprobes_node() {
    let healthy = Arc::new(AtomicBool::new(true));
    let (node, hits) = spawn_counting_node(Arc::clone(&healthy)).await;

    let cfg = cfg_for(format!("http://{node}"));
    let state = AppState::new(cfg);
    state.health_cache.set_intervals(Duration::from_secs(3600), Duration::from_millis(150));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rust = l.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
    });

    get_health(rust).await;
    let before = hits.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    get_health(rust).await;
    assert!(hits.load(Ordering::SeqCst) > before, "expired entry must trigger a fresh probe");
}

/// Node turning unhealthy must surface as the 503 live-path body quickly
/// (prober clears the cache on failure; bounded stale window <= tick+TTL).
#[tokio::test]
async fn node_down_degrades_to_live_503() {
    let healthy = Arc::new(AtomicBool::new(true));
    let (node, hits) = spawn_counting_node(Arc::clone(&healthy)).await;

    let cfg = cfg_for(format!("http://{node}"));
    let state = AppState::new(cfg);
    state.health_cache.set_intervals(Duration::from_millis(100), Duration::from_millis(1000));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rust = l.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
    });

    let (s, _, _, _) = get_health(rust).await;
    assert_eq!(s, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(50)).await; // let the prober cache
    healthy.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await; // >= 2 ticks

    let (s, body, _, _) = get_health(rust).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body.contains("db_unavailable") || body.contains("upstream_status"),
        "503 must carry the live-path body, got {body}"
    );
    // And it keeps hitting Node while unhealthy (cache stays empty).
    let a = hits.load(Ordering::SeqCst);
    get_health(rust).await;
    assert!(hits.load(Ordering::SeqCst) > a);

    // Recovery: healthy again -> ok resumes within a couple of ticks.
    healthy.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (s, body, _, _) = get_health(rust).await;
    assert_eq!(s, StatusCode::OK, "must recover, got {body}");
}

/// Kill switch path: cache disabled -> every request costs a Node probe
/// (the env var `SCRYBOX_HEALTH_CACHE=0` just produces this state; env
/// parsing itself is unit-tested in the lib's store_tests).
#[tokio::test]
async fn kill_switch_probes_every_request() {
    let healthy = Arc::new(AtomicBool::new(true));
    let (node, hits) = spawn_counting_node(Arc::clone(&healthy)).await;

    let cfg = cfg_for(format!("http://{node}"));
    let state = AppState::new(cfg);
    state.health_cache.set_enabled(false);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rust = l.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
    });

    for _ in 0..5 {
        let (s, b, _, _) = get_health(rust).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, "{\"status\":\"ok\"}");
    }
    assert!(hits.load(Ordering::SeqCst) >= 5, "disabled cache must probe Node per request");
}

// ---- LANE B unit 7: Node-free standalone (/api/health answered locally) ----

async fn get_health_full(addr: SocketAddr) -> (StatusCode, String, Vec<(String, String)>) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{addr}/api/health"))
        .body(axum::body::Body::empty())
        .unwrap();
    let res = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<axum::body::Body>()
        .request(req)
        .await
        .unwrap();
    let status = res.status();
    let headers = res
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    let bytes = axum::body::to_bytes(axum::body::Body::new(res.into_body()), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string(), headers)
}

/// Empty SCRYBOX_NODE_UPSTREAM + a real DB copy: Node's own handler shape —
/// 200, {"status":"ok"}, X-App-Name, express JSON ETag twin, and NO
/// upstream/cache machinery involved (prober never spawned; there is no peer).
#[tokio::test]
async fn standalone_answers_node_handler_shape() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bindarr.db");
    rusqlite::Connection::open(&db).unwrap().execute_batch("CREATE TABLE t(x)").unwrap();

    let mut cfg = cfg_for(String::new());
    cfg.db_path = Some(db);
    let state = AppState::new(cfg);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rust = l.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
    });

    let (s, body, headers) = get_health_full(rust).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, "{\"status\":\"ok\"}");
    let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(h("x-app-name").as_deref(), Some("Scrybox"));
    assert_eq!(h("etag").as_deref(), Some("W/\"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg\""));
    assert_eq!(h("content-type").as_deref(), Some("application/json; charset=utf-8"));
    assert_eq!(h("content-length").as_deref(), Some("15"));
}

/// No SCRYBOX_DB_PATH (pool missing) => Node's !dbReady arm, 503
/// {"status":"db_initializing"} — again with zero upstream traffic possible.
#[tokio::test]
async fn standalone_without_db_is_db_initializing() {
    let cfg = cfg_for(String::new());
    assert!(cfg.db_path.is_none());
    let state = AppState::new(cfg);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rust = l.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
    });

    let (s, body, headers) = get_health_full(rust).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "{\"status\":\"db_initializing\"}");
    assert!(headers.iter().any(|(n, v)| n == "etag" && v.starts_with("W/\"1c-")));
}
