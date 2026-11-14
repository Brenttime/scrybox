//! MVP standalone: GET /api/search with a raw Scryfall-syntax `q` on the
//! database/internet scopes (the Add Cards search box). In hybrid mode this
//! leg proxies to Node; with no Node upstream it used to answer 502
//! "Upstream unavailable". It is now served natively (search.rs RawQ).

use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
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

async fn mock_scryfall(hits: Hits) -> String {
    let r = Router::new().route(
        "/cards/search",
        get(move |Query(qs): Query<std::collections::HashMap<String, String>>| {
            let hits = hits.clone();
            async move {
                let q = qs.get("q").cloned().unwrap_or_default();
                hits.lock().unwrap().push(q.clone());
                if q.contains("zzznomatch") {
                    return (StatusCode::NOT_FOUND, axum::Json(json!({"object":"error","status":404}))).into_response();
                }
                if q.contains("badsyntax(") {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"object":"error","status":400}))).into_response();
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
    db: std::path::PathBuf,
    hits: Hits,
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
    let base = mock_scryfall(hits.clone()).await;
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
    H { router: app(st), db, hits, _dir: dir }
}

async fn get_search(r: &Router, qs: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let req = Request::builder()
        .uri(format!("/api/search?{qs}"))
        .header("authorization", "Bearer sess-1")
        .extension(ConnectInfo(SocketAddr::new("127.0.0.9".parse::<IpAddr>().unwrap(), 5555)))
        .body(Body::empty())
        .unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let st = res.status();
    let h = res.headers().clone();
    let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, h, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn standalone_raw_q_internet_search_is_native() {
    let h = harness().await;
    let (st, hd, v) = get_search(
        &h.router,
        "q=%21%22Lightning+Bolt%22+unique%3Aprints&scope=internet&lang=en&page=1&limit=250",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "Lightning Bolt");
    assert_eq!(arr[0]["id"], "mtg-11111111-aaaa-4aaa-8aaa-111111111111");
    assert_eq!(arr[0]["owned_qty"], 0);
    assert_eq!(hd["x-total-count"], "2");
    assert_eq!(hd["x-source"], "scryfall");
    // Verbatim (parser-rejected subset) upstream query, English unscoped.
    assert_eq!(h.hits.lock().unwrap()[0], "!\"lightning bolt\" unique:prints", "toScryfall(ast): Node lowercases name leaves");
    // cacheCards wrote the printings.
    let n: i64 = Connection::open(&h.db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM card_cache WHERE name='Lightning Bolt'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn standalone_raw_q_error_arms() {
    let h = harness().await;
    let (st, _, v) = get_search(&h.router, "q=zzznomatch&scope=internet").await;
    assert_eq!((st, v), (StatusCode::OK, json!([])));
    let (st, _, v) = get_search(&h.router, "q=badsyntax(&scope=database").await;
    assert_eq!((st, v), (StatusCode::BAD_REQUEST, json!({"error":"INVALID_QUERY"})));
    // Non-English: (q) scoping + lang:code + include_multilingual.
    let (st, _, _) = get_search(&h.router, "q=bolt&lang=ja").await;
    assert_eq!(st, StatusCode::OK);
    assert!(h.hits.lock().unwrap().iter().any(|q| q == "(bolt) lang:ja"), "{:?}", h.hits.lock().unwrap());
}
