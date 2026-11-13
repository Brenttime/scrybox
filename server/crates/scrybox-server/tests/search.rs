//! Ported `GET /api/search` tests. The contract is Node's field-filter
//! local-cache path: bare JSON array of `parseCardRow` rows (card_cache
//! column order) with `owned_qty` appended, `X-Source: cache`, the Express
//! default body ETag, and proxy fallback for every request Node would answer
//! from Scryfall/the sidecar instead.

use std::{
    collections::BTreeSet,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use axum::{
    extract::Query,
    body::Body,
    extract::{ConnectInfo, Request},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "tok-session";

/// card_cache columns in fixture table order, plus the appended owned_qty.
fn card_keys() -> Vec<&'static str> {
    vec![
        "id", "name", "supertype", "subtypes", "types", "rarity", "set_id", "set_name", "number",
        "image_url", "price_trend", "price_normal", "price_holofoil", "price_currency",
        "price_source", "cmc", "color_identity", "language", "printed_name",
        "tcgplayer_product_id", "last_updated", "tcgplayer_url", "cardmarket_url", "price_etched",
        "oracle_id", "scryfall_search_eligible", "oracle_text", "oracle_faces", "price_estimated",
        "owned_qty",
    ]
}

fn seeded_db(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("search-test.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    // Node's db.js owns this migration; the q leg shares the table, so the
    // fixture mirrors it.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_query_cache (
           query TEXT NOT NULL, lang TEXT NOT NULL DEFAULT '', names TEXT NOT NULL,
           upstream_total INTEGER, fetched_count INTEGER NOT NULL DEFAULT 0,
           complete INTEGER NOT NULL DEFAULT 1, resolved_at INTEGER NOT NULL,
           expires_at INTEGER NOT NULL, PRIMARY KEY (query, lang));",
    )
    .unwrap();
    conn.execute_batch(
        "
        INSERT INTO users (id, username, password_hash, role, share_token, api_key)
          VALUES (1, 'u1', 'x', 'admin', 'share1', 'tok-api'),
                 (2, 'u2', 'x', 'member', 'share2', NULL);
        INSERT INTO sessions (token, user_id, expires_at)
          VALUES ('tok-session', 1, '2099-01-01 00:00:00');
        INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number,
                                image_url, price_trend, price_normal, price_holofoil, price_currency,
                                price_source, cmc, color_identity, language, printed_name,
                                tcgplayer_product_id, last_updated, tcgplayer_url, cardmarket_url,
                                price_etched, oracle_id, scryfall_search_eligible, oracle_text,
                                oracle_faces, price_estimated)
          VALUES
            ('cc1', 'Sol Ring', 'Artifact', '[\"Artifact\"]', '[]', 'Uncommon', 'brc',
             'The Brothers'' War Commander', '160', 'https://x/1.jpg', 4.01, 4.01, NULL, 'USD',
             'tcgplayer', 1, '[]', 'English', 'Sol Ring', 123, '2026-01-01 00:00:00', NULL, NULL,
             NULL, NULL, NULL, '{\"a\":1}', NULL, 0),
            ('cc2', 'Plains', 'Basic Land', '[\"Land\"]', '[\"Land\"]', 'Common', '3ed', 'The Dark',
             '2', NULL, NULL, NULL, NULL, 'USD', NULL, 0, '[]', 'English', NULL, NULL,
             '2026-01-01 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0),
            ('cc6', 'Sol Ring', 'Artifact', '[\"Artifact\"]', '[]', 'Uncommon', 'lea', 'Alpha', '1',
             NULL, 2.5, NULL, 3.5, 'USD', NULL, 1.0, '[]', 'English', NULL, NULL,
             '2026-01-01 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0),
            ('cc7', 'SOL RING ', 'Artifact', '', '[]', 'Uncommon', '2ed', 'Unlimited', '1', NULL,
             NULL, NULL, NULL, 'USD', NULL, 1, '[]', 'English', NULL, NULL,
             '2026-01-01 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0),
            ('cc-ja', 'ソール・リング', 'Artifact', '[\"Artifact\"]', '[]', 'Uncommon', 'jmp',
             'Jumpstart', '1', NULL, 3.0, NULL, NULL, 'USD', NULL, 1, '[]', 'Japanese', 'Sol Ring',
             NULL, '2026-01-01 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0);
        INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price,
                                added_at, notes, is_trade, user_id, is_proxy)
          VALUES (1, 'cc1', 2, 'Near Mint', 'Normal', 'English', 0.0, '2024-01-05 00:00:00', '', 0, 1, 0),
                 (2, 'cc6', 1, 'Near Mint', 'Normal', 'English', NULL, '2024-01-04 00:00:00', '', 0, 1, 0),
                 (3, 'cc7', 1, 'Near Mint', 'Normal', 'English', NULL, '2024-01-03 00:00:00', '', 0, 1, 0),
                 (4, 'cc1', 5, 'Near Mint', 'Normal', 'English', NULL, '2024-01-06 00:00:00', '', 0, 2, 0);
        INSERT INTO collection_read_revision (id, revision) VALUES (1, 1);
        ",
    )
    .unwrap();
    path
}

/// Fake Node: echoes so proxy-fallback assertions can see the hop happened.
async fn spawn_fake_node() -> SocketAddr {
    let echo = any(|req: Request| async move {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let mut out = HeaderMap::new();
        out.insert(HeaderName::from_static("x-node"), HeaderValue::from_static("1"));
        out.insert(HeaderName::from_static("x-source"), HeaderValue::from_static("live"));
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
    let health = axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({"status":"ok"}))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

fn router_for(db_path: PathBuf, routes: &[&str], upstream: &str) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    // Keep the suite hermetic: the stale background refresh POSTs to
    // st.scry's base (api.scryfall.com by default). The dedicated refresh
    // case re-enables it against a mock Scry.
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    app(st)
}

/// router_for pinned to sidecar mode (LOCAL_SCRYFALL parity): the collq
/// walk/durable legs answer, and the iter-14 upstream-mode SQLite answer legs
/// (answerLocally / Oracle-Tag-local) stand down — they are Node-parity arms
/// for the mode WITHOUT a sidecar. Collq tests that pin walk behaviour
/// (durable headers, out-of-subset proxy) use this.
fn router_for_sidecar_mode(db_path: PathBuf, routes: &[&str], upstream: &str) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    st.local_scrayfall = true;
    app(st)
}

/// Like router_for, but pins Scry to a mock — the iter-10 live merge leg
/// talks to st.scry and must never touch api.scryfall.com from the suite.
fn router_for_live(db_path: PathBuf, routes: &[&str], upstream: &str, scry: scrybox_server::scryfall::Scry) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    st.scry = Arc::new(scry);
    app(st)
}

async fn get(r: &Router, path: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder()
        .method("GET")
        .uri(path)
        .extension(ConnectInfo(next_peer()))
        .body(Body::empty())
        .unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(
            header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    let res = r.clone().oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    (parts.status, parts.headers, axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec())
}

fn auth() -> Vec<(&'static str, &'static str)> {
    vec![("authorization", "Bearer tok-session")]
}

async fn get_json(r: &Router, path: &str) -> (StatusCode, HeaderMap, Value) {
    let (s, h, b) = get(r, path, &auth()).await;
    (s, h, serde_json::from_slice(&b).unwrap())
}

/// The handlers take `ConnectInfo` (searchLimiter keys by client IP since
/// lane-B iter-5's 6e24334); oneshot requests must carry it explicitly
/// (scan_match_route.rs precedent). A fresh address per request also gives
/// every request its own process-global searchLimiter bucket, so tests
/// never race each other's 300/15min window.
fn next_peer() -> SocketAddr {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    SocketAddr::from((
        std::net::Ipv4Addr::new(10, 199, (n / 250) as u8, (n % 250) as u8 + 1),
        41000,
    ))
}

async fn post(
    r: &Router,
    path: &str,
    headers: &[(&str, &str)],
    body: &'static str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .extension(ConnectInfo(next_peer()))
        .body(Body::from(body))
        .unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(
            header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    let res = r.clone().oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    (parts.status, parts.headers, axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec())
}

fn json_ct() -> Vec<(&'static str, &'static str)> {
    let mut h = auth();
    h.push(("content-type", "application/json"));
    h
}

// --- native path ---------------------------------------------------------

#[tokio::test]
async fn db_name_search_shape_and_headers() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let (s, h, body) = get_json(&r, "/api/search?scope=database&name=Sol%20Ring").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
        "application/json; charset=utf-8"
    );
    assert_eq!(h.get("x-source").unwrap().to_str().unwrap(), "cache");
    assert_eq!(
        h.get("access-control-expose-headers").unwrap().to_str().unwrap(),
        "X-Total-Count, X-Source"
    );
    assert!(h.get("x-node").is_none(), "must not hop to Node");
    assert!(h.get(header::ETAG).is_some());
    assert!(h.get("x-total-count").is_none(), "field-filter path never sets meta.total");

    // Bare JSON array, not {cards,total,source}.
    let cards = body.as_array().expect("body is an array");
    assert_eq!(cards.len(), 3, "cc1, cc6, cc7 (LIKE is ASCII-case-insensitive)");
    // Column order from SELECT *, arrays parsed in place, owned_qty appended.
    let keys: Vec<&String> = cards[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, card_keys());
    assert_eq!(cards[0]["id"], json!("cc1"));
    assert_eq!(cards[0]["subtypes"], json!(["Artifact"]));
    assert_eq!(cards[0]["types"], json!([]));
    // Owned by user 1: 2 (cc1) + 1 (cc6) + 1 (cc7 'SOL RING ' — LOWER(TRIM) key).
    for c in cards {
        assert_eq!(c["owned_qty"], json!(4), "owned count across printings for {}", c["name"]);
    }
}

#[tokio::test]
async fn number_and_json_float_formatting() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let (s, _, raw) = get(&r, "/api/search?name=Sol%20Ring&number=%23160%2Fbrc", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(raw).unwrap();
    let body: Value = serde_json::from_str(&text).unwrap();
    let cards = body.as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], json!("cc1"));
    // JS JSON.stringify: integral REAL prints without a fraction; 4.01 stays.
    assert!(text.contains("\"cmc\":1"), "cmc integral REAL must print as 1: {text}");
    assert!(text.contains("\"price_trend\":4.01"));
    assert!(text.contains("\"price_holofoil\":null"));
}

#[tokio::test]
async fn set_and_lang_and_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // set=lea narrows to cc6.
    let (_, _, body) = get_json(&r, "/api/search?name=Sol%20Ring&set=lea").await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], json!("cc6"));

    // lang by display name and by code resolve to the same filter.
    for q in ["lang=Japanese", "lang=ja", "lang=JAPANESE"] {
        let (_, _, body) = get_json(&r, &format!("/api/search?name=%E3%82%BD%E3%83%BC%E3%83%AB&{q}")).await;
        let cards = body.as_array().unwrap();
        assert_eq!(cards.len(), 1, "{q}");
        assert_eq!(cards[0]["id"], json!("cc-ja"));
    }
    // Unknown lang falls back to English rows.
    let (_, _, body) = get_json(&r, "/api/search?name=Sol%20Ring&lang=xx").await;
    assert_eq!(body.as_array().unwrap().len(), 3);

    // page/limit window the ordered result set.
    let (_, _, p1) = get_json(&r, "/api/search?name=Sol%20Ring&limit=2&page=1").await;
    let (_, _, p2) = get_json(&r, "/api/search?name=Sol%20Ring&limit=2&page=2").await;
    let p1 = p1.as_array().unwrap();
    let p2 = p2.as_array().unwrap();
    assert_eq!(p1.len(), 2);
    assert_eq!(p2.len(), 1);
    assert_eq!(p1[0]["id"], json!("cc1"));
    assert_eq!(p2[0]["id"], json!("cc7"));

    // A blank q is trimmed to '' by Node and behaves as absent — native.
    let (s, h, _) = get(&r, "/api/search?name=Sol%20Ring&q=%20", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-source").unwrap().to_str().unwrap(), "cache");
    assert!(h.get("x-node").is_none());
}

#[tokio::test]
async fn etag_is_express_body_tag_and_replays_304() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let (_, h, body) = get(&r, "/api/search?name=Sol%20Ring", &auth()).await;
    let tag = h.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let etag = scrybox_server::collection::body_etag(&body);
    assert_eq!(tag, etag, "route tag must be the Express default body tag");

    let (s, h, b) = get(&r, "/api/search?name=Sol%20Ring", &[("authorization", "Bearer tok-session"), ("if-none-match", tag.as_str())]).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    assert!(b.is_empty());
    assert_eq!(h.get(header::ETAG).unwrap().to_str().unwrap(), tag);
}

#[tokio::test]
async fn cache_invalidated_by_revision_bump() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    let upstream = spawn_fake_node().await;
    let r = router_for(db.clone(), &["search"], &format!("http://{upstream}"));

    let (_, _, body) = get_json(&r, "/api/search?name=Sol%20Ring").await;
    assert_eq!(body.as_array().unwrap().len(), 3);

    // Node writes bump collection_read_revision via the schema triggers
    // (db.js; the fixture omits the card_cache leg, so bump it here the way
    // the trigger would). The cache must not keep serving the pre-write entry.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "INSERT INTO card_cache (id, name, subtypes, types, color_identity)
             VALUES ('cc8', 'Sol Rings', '[]', '[]', '[]');
             UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1;",
        )
        .unwrap();
    }
    let (_, _, body) = get_json(&r, "/api/search?name=Sol%20Ring").await;
    assert_eq!(body.as_array().unwrap().len(), 4, "revision bump must invalidate the cached body");
}

// --- proxy fallback ------------------------------------------------------

#[tokio::test]
async fn zero_rows_live_merge_answers_empty() {
    // Node's runSearch answers a zero-row field-filter search itself from
    // the specific/broad/fallback merge — with every leg empty that is
    // `res.json([])` with X-Source: cache, no proxy hop (iter-10 port).
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    let r = router_for_live(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"), mock_scryfall::scry_for(mock, 0.0));

    let (s, h, body) = get(&r, "/api/search?name=NoCardNamedThis", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node"), None, "must NOT have gone through Node");
    let cards: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(cards.as_array().unwrap().len(), 0);
    assert_eq!(h.get("x-source").unwrap().to_str().unwrap(), "cache", "empty merge = cache source");
    assert_eq!(h.get("x-total-count"), None, "no meta on the empty answer");
    // The broad leg was actually asked, with Node's exact suffix.
    let asked = ms.hits_for("GET /cards/search").iter().any(|(k, _)| {
        k.contains("q=NoCardNamedThis%20unique%3Aprints%20-is%3Adigital")
    });
    assert!(asked, "broad query must reach the live client");
}

#[tokio::test]
async fn zero_rows_upstream_error_proxies() {
    // Upstream errors stay Node's problem: a search whose merge legs all
    // fail must proxy verbatim (Node's own catch → next(err)), never a
    // half-baked native 500.
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    ms.statuses
        .lock()
        .unwrap()
        .insert("GET /cards/search".into(), vec![500]);
    let r = router_for_live(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"), mock_scryfall::scry_for(mock, 0.0));
    let (s, h, _b) = get(&r, "/api/search?scope=database&name=ZzqxGone&page=1&limit=20", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1", "merge failure must proxy to Node");
}

#[tokio::test]
async fn gated_queries_proxy_to_node() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    for q in [
        "/api/search?q=t%3Acreature%20c%3Ag",          // raw query → parser/sidecar
        // scope=collection + q is NO LONGER a proxy case in upstream mode:
        // Node answers data-backed queries from the owned rows (answerLocally)
        // and Rust now does too — pinned natively below and in
        // tests/collq_local.rs.
        // scope=internet no longer proxies: the iter-10 live merge leg
        // answers it natively — see the live_merge_* tests below.
        "/api/search?name=Sol%20Ring&prints=1",        // allPrints walk
        "/api/search?name=Sol%20Ring&image=abc",       // cvScan ranking
        "/api/search?name=a&name=b",                   // qs array shape
        "/api/search?scope=database&name=Sol%20Ring&page=1&page=2", // repeated page
    ] {
        let (s, h, _) = get(&r, q, &auth()).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1", "{q} must proxy");
    }
    // scope=collection + name NO LONGER proxies: it is the native
    // cardSearchSql.collectionQuery leg (F8-TC5) — see the
    // collection_scope_* tests below for its contract.
    let (s, h, _) = get(&r, "/api/search?scope=collection&name=Sol%20Ring", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none(), "collection field-filter must answer natively");
}

// --- POST /api/search (bigJson + {...query, ...body} merge) ---------------

#[tokio::test]
async fn post_body_name_served_natively() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let (s, h, body) = post(&r, "/api/search", &json_ct(), r#"{"name":"Sol Ring"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-source").unwrap().to_str().unwrap(), "cache");
    assert!(h.get("x-node").is_none(), "must not hop to Node");
    assert!(h.get(header::ETAG).is_some(), "res.json sets the body ETag on POST too");
    assert_eq!(h.get("ratelimit-limit").unwrap().to_str().unwrap(), "300");
    let cards = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(cards.as_array().unwrap().len(), 3);
    // Byte-identical to the GET serving the same merged params (shared cache).
    let (_, _, gbody) = get(&r, "/api/search?name=Sol%20Ring", &auth()).await;
    assert_eq!(body, gbody);
}

#[tokio::test]
async fn post_body_overrides_query_params() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // Body wins over the query for the same key (spread order).
    let (s, h, body) = post(
        &r,
        "/api/search?name=__definitely_no_such_card__&limit=1&page=1",
        &json_ct(),
        r#"{"name":"Sol Ring"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap().as_array().unwrap().len(), 1);

    // A body property also neutralises repeated query keys (spread replaces
    // the whole key, qs array shape never reaches the handler).
    let (s, h, body) = post(&r, "/api/search?name=a&name=b", &json_ct(), r#"{"name":"Sol Ring"}"#)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap().as_array().unwrap().len(), 3);

    // Without the body override the duplicate stays exotic → proxy.
    let req_dup = post(&r, "/api/search?name=a&name=b", &json_ct(), "{}").await;
    assert_eq!(req_dup.1.get("x-node").unwrap().to_str().unwrap(), "1");
}

#[tokio::test]
async fn post_gated_bodies_proxy_with_body_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    for (body, why) in [
        (r#"{"q":"sol ring"}"#, "q-leg pinned to GET"),
        (r#"{"name":"Sol Ring","scope":"collection"}"#, "collection scope"),
        (r#"{"name":"Sol Ring","scope":"internet"}"#, "internet scope"),
        (r#"{"name":"Sol Ring","prints":"1"}"#, "allPrints walk"),
        (r#"{"name":"Sol Ring","image":"abc"}"#, "cvScan ranking leg"),
        (r#"{"name":{"a":1}}"#, "object on stringified key"),
        (r#"{"name":"Sol Ring","limit":[5]}"#, "array on parseInt key"),
        (r#"{"name":"Sol Ring","lang":{"a":1}}"#, "object on lang"),
    ] {
        let (s, h, echo) = post(&r, "/api/search", &json_ct(), body).await;
        assert_eq!(s, StatusCode::OK, "{why}");
        assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1", "{why} must proxy");
        // The consumed body must arrive at Node byte-exact.
        let e: Value = serde_json::from_slice(&echo).unwrap();
        assert_eq!(e["body"].as_str().unwrap(), body, "{why} body must survive rebuild");
    }
}

#[tokio::test]
async fn post_content_type_and_body_shape_ladder() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // Non-JSON content-type: body-parser SKIPS the body (no keys, no size
    // check) — the query alone drives the native answer.
    let (s, h, body) = post(
        &r,
        "/api/search?name=Sol%20Ring",
        &[("authorization", "Bearer tok-session"), ("content-type", "text/plain")],
        "not json at all",
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap().as_array().unwrap().len(), 3);

    // No content-type at all, empty body: same merge ({} contributes keys).
    let (s, h, _) = post(&r, "/api/search?name=Sol%20Ring", &auth(), "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());

    // application/json body {} and [] contribute no named keys.
    for body in ["{}", "[]"] {
        let (s, h, body_) = post(&r, "/api/search?name=Sol%20Ring", &json_ct(), body).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert!(h.get("x-node").is_none(), "{body} must stay native");
        assert_eq!(serde_json::from_slice::<Value>(&body_).unwrap().as_array().unwrap().len(), 3);
    }

    // +json suffix mimes are parsed too (type-is 'json').
    let (s, _, _) = post(
        &r,
        "/api/search",
        &[("authorization", "Bearer tok-session"), ("content-type", "application/vnd.api+json")],
        r#"{"name":"NoCardNamedThis"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "zero-row +json body proxies to Node untouched");
}

#[tokio::test]
async fn post_malformed_json_answers_global_500() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // entity.parse.failed AND strict-mode scalars → global handler 500.
    for body in ["{oops", "5", "null", "\"a string\"", "  "] {
        let (s, _, b) = post(&r, "/api/search", &json_ct(), body).await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{body:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&b).unwrap(),
            json!({ "error": "Internal server error" })
        );
    }
}

#[tokio::test]
async fn post_auth_gate_before_body_parse() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // authenticateToken (server.js:454) runs before bigJson (462): a garbage
    // body must never outrun the 401/403.
    let (s, _, b) = post(&r, "/api/search", &[("content-type", "application/json")], "{oops").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap(),
        json!({ "error": "Access denied. No token provided." })
    );

    // API keys are GET-only reads → 403 before any parse.
    let (s, _, b) = post(
        &r,
        "/api/search",
        &[("authorization", "Bearer tok-api"), ("content-type", "application/json")],
        "not-even-close",
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap(),
        json!({ "error": "API keys are read-only. Use a login session for this request." })
    );

    // A valid session + malformed body reaches the parse → 500, NOT the
    // proxy (and Node's rate bucket is only advanced by requests that pass
    // the parser — parse failure never reaches searchLimiter, so there is
    // deliberately no limiter-header assertion here: the shared in-process
    // bucket is shared across all tests in this binary).
    let (s, h, _) = post(&r, "/api/search", &json_ct(), "{oops").await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(h.get("ratelimit-limit").is_none(), "limiter never saw this request");
    // And the limiter headers on a good request prove it ran after parse.
    let (s, h, _) = post(&r, "/api/search", &json_ct(), r#"{"name":"Sol Ring"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("ratelimit-limit").unwrap().to_str().unwrap(), "300");
}

#[tokio::test]
async fn post_too_large_body_answers_413() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let big = format!(r#"{{"name":"{}"}}"#, "x".repeat(16 * 1024 * 1024));
    let req = Request::builder()
        .method("POST")
        .uri("/api/search")
        .extension(axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:55555".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .header("authorization", "Bearer tok-session")
        .header("content-type", "application/json")
        .extension(ConnectInfo(next_peer()))
        .body(Body::from(big))
        .unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    assert_eq!(parts.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        serde_json::from_slice::<Value>(&axum::body::to_bytes(body, usize::MAX).await.unwrap())
            .unwrap(),
        json!({ "error": "Upload too large. Try exporting/importing in smaller batches." })
    );

    // A NON-JSON content-type body of the same size is SKIPPED, not 413'd
    // (body-parser type check precedes the limit check).
    let (s, h, body) = post(
        &r,
        "/api/search?name=Sol%20Ring",
        &[("authorization", "Bearer tok-session"), ("content-type", "text/plain")],
        Box::leak("x".repeat(16 * 1024 * 1024).into_boxed_str()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none(), "query-only native answer");
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap().as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn post_number_and_falsy_coercions() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // Numbers stringify JS-style before the parseInt digit scan: 2 → page 2,
    // 1.5 → parseInt("1.5") = 1, falsy 0 → ||-default.
    let (_, _, a) = post(&r, "/api/search", &json_ct(), r#"{"name":"Sol Ring","page":2,"limit":1}"#).await;
    let (_, _, b) = get(&r, "/api/search?name=Sol%20Ring&page=2&limit=1", &auth()).await;
    assert_eq!(a, b, "number page/limit must match the query-string window");
    let (_, _, c) = post(&r, "/api/search", &json_ct(), r#"{"name":"Sol Ring","page":1.5,"limit":0}"#).await;
    let (_, _, d) = get(&r, "/api/search?name=Sol%20Ring", &auth()).await;
    assert_eq!(c, d, "1.5 → 1 and 0 → 60 defaults");

    // Falsy body values never open the `if (x)` gates: number:null,
    // set:false, lang:0 all behave exactly like an absent key.
    let (_, _, e) = post(
        &r,
        "/api/search",
        &json_ct(),
        r#"{"name":"Sol Ring","number":null,"set":false,"lang":0}"#,
    )
    .await;
    assert_eq!(e, d);

    // Truthy non-strings on the LIKE keys are Node-crash shapes: runSearch's
    // `(x || '').trim()` throws `.trim is not a function` and the route's
    // catch answers 500 {"error":"Search failed"} (probed live:
    // {"name":160}/{"number":160}/{"set":160}/{"name":true} all 500 on :3312).
    // Rust never answers those natively — the whole request forwards so
    // Node's own 500 rides verbatim. parity ps_post_number_numeric compares
    // both sides live. Falsy non-strings (0/-0/false/null) stay native.
    for body in [
        r#"{"name":5,"number":true}"#,
        r#"{"name":"Sol Ring","number":160}"#,
        r#"{"name":"Sol Ring","set":160}"#,
        r#"{"name":true}"#,
        r#"{"name":"Sol Ring","set":{"a":1}}"#,
    ] {
        let (_, h, _) = post(&r, "/api/search", &json_ct(), body).await;
        assert_eq!(
            h.get("x-node").unwrap().to_str().unwrap(), "1",
            "truthy non-string must forward whole-request: {body}"
        );
    }
    // -0 stays falsy (JS `-0 || ''`), so it must NOT forward.
    let (_, h, _) = post(&r, "/api/search", &json_ct(), r#"{"name":-0,"number":"160/b"}"#).await;
    assert!(h.get("x-node").is_none(), "-0 name is falsy — native leg");
}

#[tokio::test]
async fn post_never_replays_304_but_gets_the_same_etag() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let (_, gh, _) = get(&r, "/api/search?name=Sol%20Ring", &auth()).await;
    let etag = gh.get(header::ETAG).unwrap().to_str().unwrap().to_string();

    let mut hs = json_ct();
    hs.push(("if-none-match", Box::leak(etag.clone().into_boxed_str())));
    let (s, h, body) = post(&r, "/api/search?name=Sol%20Ring", &hs, "{}").await;
    assert_eq!(s, StatusCode::OK, "conditional revalidation is GET/HEAD-only");
    assert_eq!(h.get(header::ETAG).unwrap().to_str().unwrap(), etag);
    assert!(!body.is_empty());

    // The GET twin DOES replay 304 with the same tag.
    let (s, _, _) = get(&r, "/api/search?name=Sol%20Ring", &[("authorization", "Bearer tok-session"), ("if-none-match", etag.as_str())]).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn post_kill_switch_off_proxies_everything() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &[], &format!("http://{upstream}"));

    let (s, h, echo) = post(&r, "/api/search", &json_ct(), r#"{"name":"Sol Ring"}"#).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1");
    let e: Value = serde_json::from_slice(&echo).unwrap();
    assert_eq!(e["body"].as_str().unwrap(), r#"{"name":"Sol Ring"}"#);
}

#[tokio::test]
async fn post_other_methods_still_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    for m in ["PUT", "DELETE", "PATCH", "HEAD"] {
        let req = Request::builder()
            .method(m)
            .uri("/api/search")
            .header("authorization", "Bearer tok-session")
            .extension(ConnectInfo(next_peer()))
            .body(Body::empty())
            .unwrap();
        let res = r.clone().oneshot(req).await.unwrap();
        assert_eq!(res.headers().get("x-node").unwrap().to_str().unwrap(), "1", "{m}");
    }
}

#[tokio::test]
async fn kill_switch_off_proxies_everything() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &[], &format!("http://{upstream}"));

    let (_, h, _) = get(&r, "/api/search?name=Sol%20Ring", &auth()).await;
    assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1");
}

#[tokio::test]
async fn auth_matches_node() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let (s, _, b) = get(&r, "/api/search?name=Sol%20Ring", &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap(),
        json!({ "error": "Access denied. No token provided." })
    );

    let (s, _, b) = get(&r, "/api/search?name=x", &[("authorization", "Bearer nope")]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap(),
        json!({ "error": "Invalid or expired session token." })
    );

    // API keys may read searches (GET).
    let (s, _, _) = get(&r, "/api/search?name=Sol%20Ring", &[("authorization", "Bearer tok-api")]).await;
    assert_eq!(s, StatusCode::OK);
}

// --- name index (name_index.rs) -------------------------------------------

use scrybox_server::{db::DbPool, name_index, name_index::{NameIndex, OwnedCache}};

/// Add distinctive rows to the seeded DB and bump the revision past every
/// earlier entry so both caches and the name index treat it as a fresh state.
fn seed_extra(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "
        INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name,
                                number, language, printed_name, price_estimated)
          VALUES ('cc8',  'Zz Zorv',           'Artifact', '[]', '[]', 'Rare', 'brc', 'Extra', '9',
                  'English', 'Sol Ring', 0),
                 ('cc9',  'Sol Ringg Variant', 'Artifact', '[]', '[]', 'Rare', 'brc', 'Extra', '8',
                  'English', NULL,       0),
                 ('cc10', 'Plainville',        'Land',     '[]', '[]', 'Rare', 'brc', 'Extra', '7',
                  'English', 'the PLAINS of paper', 0),
                 ('cc11', '100_Card',          'Land',     '[]', '[]', 'Rare', 'brc', 'Extra', '6',
                  'English', NULL,       0),
                 ('cc12', '100XCard',          'Land',     '[]', '[]', 'Rare', 'brc', 'Extra', '5',
                  'English', NULL,       0),
                 ('cc13', 'ソールテスト',      'Artifact', '[]', '[]', 'Rare', 'brc', 'Extra', '4',
                  'English', NULL,       0);
        UPDATE collection_read_revision SET revision = 10 WHERE id = 1;
        ",
    )
    .unwrap();
}

fn router_for_idx(db_path: PathBuf, routes: &[&str], upstream: &str, idx: NameIndex) -> Router {
    router_for_idx_owned(db_path, routes, upstream, idx, OwnedCache::new())
}

fn router_for_idx_owned(
    db_path: PathBuf,
    routes: &[&str],
    upstream: &str,
    idx: NameIndex,
    owned: OwnedCache,
) -> Router {
    router_for_idx_owned_sidecar(db_path, routes, upstream, idx, owned, None)
}

fn router_for_idx_owned_sidecar(
    db_path: PathBuf,
    routes: &[&str],
    upstream: &str,
    idx: NameIndex,
    owned: OwnedCache,
    sidecar_url: Option<String>,
) -> Router {
    let is_sidecar = sidecar_url.is_some();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url,
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    // Sidecar URL set = the parity harness's sidecar mode (LOCAL_SCRYFALL on
    // both sides): the engine answers EVERY collection-q, card_cache SQL is
    // outage-fallback only. Upstream mode (no sidecar) gets the SQLite answer
    // legs (collq local rows / Oracle-Tag-local).
    st.local_scrayfall = is_sidecar;
    st.name_index = Arc::new(idx);
    st.owned_index = Arc::new(owned);
    app(st)
}

/// The candidate set must equal — rowid-order and all — what SQLite's
/// ASCII-case-insensitive LIKE matches, for every indexable pattern shape.
#[test]
fn index_candidates_equal_like_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    seed_extra(&path);
    let pool = DbPool::open(&path, 2).unwrap();
    let idx = NameIndex::new();
    idx.ensure_fresh(&pool, 10);
    let snap = idx.snapshot_for(10).expect("snapshot at seeded rev");
    let other = Connection::open(&path).unwrap();

    for raw in [
        "sol ring", "SOL RING", "Sol Rin", "l ring", "plains", "PLAINS", "the PLAINS of paper",
        "ソール", "100", "zzz missing", "Zz Zorv", "ringg",
    ] {
        let pat = name_index::indexable_pattern(raw).unwrap();
        let like = format!("%{raw}%");
        let want: Vec<i64> = other
            .prepare_cached("SELECT rowid FROM card_cache WHERE name LIKE ? OR printed_name LIKE ? ORDER BY rowid")
            .unwrap()
            .query_map(rusqlite::params![like.clone(), like], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(snap.candidate_rowids(&pat), want, "pattern {raw:?}");
    }
    // Metacharacters and empty patterns never take the index.
    assert!(name_index::indexable_pattern("").is_none());
    assert!(name_index::indexable_pattern("100_card").is_none());
    assert!(name_index::indexable_pattern("a%b").is_none());
}

/// Byte-for-byte: the index path must serialize exactly what the plain scan
/// serializes — same rows, same rowid order, same owned_qty, same ETag.
#[tokio::test]
async fn index_path_body_bytes_equal_plain_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    seed_extra(&path);
    let upstream = format!("http://{}", spawn_fake_node().await);

    let plain = router_for_idx_owned(path.clone(), &["search"], &upstream, NameIndex::disabled(), OwnedCache::disabled());
    let indexed = router_for_idx(path.clone(), &["search"], &upstream, NameIndex::new());

    for url in [
        "/api/search?scope=database&name=Sol%20Ring",
        "/api/search?scope=database&name=Sol&page=2&limit=1",
        "/api/search?scope=database&name=plains&page=1&limit=1",
        "/api/search?scope=database&name=100",
        "/api/search?name=the%20plains%20of%20paper",
        "/api/search?scope=database&name=ソール",
    ] {
        let (s1, h1, b1) = get(&plain, url, &auth()).await;
        let (s2, h2, b2) = get(&indexed, url, &auth()).await;
        assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{url}");
        assert_eq!(h1.get("x-node"), None, "{url} plain hit Node");
        assert_eq!(h2.get("x-node"), None, "{url} indexed hit Node");
        assert_eq!(b1, b2, "body mismatch for {url}");
        assert_eq!(
            h1.get(header::ETAG).unwrap(),
            h2.get(header::ETAG).unwrap(),
            "etag mismatch for {url}"
        );
        assert!(!b1.is_empty());
    }
}

/// A row written after the index exists must be visible once the revision
/// bumps — i.e. the rebuild actually sees new rows (the DB-copy write-mix
/// pattern of the honest bench).
#[tokio::test]
async fn index_serves_rows_written_after_invalidations() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    // iter-10: zero-row names now reach the live merge leg — pin it to a
    // dead mock so "no local rows" keeps the proxy verdict, hermetically.
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    ms.statuses
        .lock()
        .unwrap()
        .insert("GET /cards/search".into(), vec![500]);
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: ["search".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(path.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    st.name_index = Arc::new(NameIndex::new());
    st.owned_index = Arc::new(OwnedCache::new());
    st.scry = Arc::new(mock_scryfall::scry_for(mock, 0.0));
    let r = app(st);

    // Cold: zero rows → proxy, exactly like the plain path.
    let (s, h, _) = get(&r, "/api/search?name=Newthing", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1", "zero rows must proxy");

    // Node-style write + trigger bump, then the entry must go native.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id,
                                     set_name, number, language, price_estimated)
             VALUES ('cc13', 'Newthing', 'Land', '[]', '[]', 'Rare', 'brc', 'Extra', '3', 'English', 0);
             UPDATE collection_read_revision SET revision = 11 WHERE id = 1;",
        )
        .unwrap();
    }
    let (s, h, b) = get(&r, "/api/search?name=Newthing", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node"), None, "must be answered natively after invalidation");
    let cards = serde_json::from_slice::<Value>(&b).unwrap();
    assert_eq!(cards.as_array().unwrap().len(), 1);
    assert_eq!(cards[0]["id"], json!("cc13"));

    // …and a deletion is equally visible (the trigger bumps the same counter).
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DELETE FROM card_cache WHERE id = 'cc13';
             UPDATE collection_read_revision SET revision = 12 WHERE id = 1;",
        )
        .unwrap();
    }
    let (s, h, _) = get(&r, "/api/search?name=Newthing", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1", "deleted row must fall back to proxy");
}

/// `_` is LIKE's single-char wildcard — patterns containing it must keep the
/// plain-scan semantics (both literal `_` and wildcard hits come back).
#[tokio::test]
async fn underscore_pattern_keeps_plain_scan_like_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    seed_extra(&path);
    let upstream = format!("http://{}", spawn_fake_node().await);
    let r = router_for_idx(path, &["search"], &upstream, NameIndex::new());

    let (s, h, b) = get(&r, "/api/search?name=100_card", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node"), None);
    let cards = serde_json::from_slice::<Value>(&b).unwrap();
    let ids: Vec<&str> = cards
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["cc11", "cc12"], "LIKE '_' wildcard must still match cc12 via plain scan");
}

/// Repeated same-revision builds must not be re-scanned; and stale snapshots
/// (wrong rev) must never serve.
#[test]
fn index_snapshot_is_rev_gated() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    seed_extra(&path);
    let pool = DbPool::open(&path, 2).unwrap();
    let idx = NameIndex::new();
    assert!(idx.snapshot_for(10).is_none(), "cold index serves nothing");
    idx.ensure_fresh(&pool, 10);
    assert!(idx.snapshot_for(10).is_some());
    assert!(idx.snapshot_for(11).is_none(), "rev mismatch must fall back to plain scan");
    assert!(idx.snapshot_for(9).is_none());
}

/// SQLite TRIM strips spaces only and LOWER folds ASCII only — the Rust
/// attach must agree (tab-prefixed names are NOT trimmed by either).
#[test]
fn owned_snapshot_attach_mirrors_sql_trim_lower() {
    use scrybox_server::name_index::attach_owned_from_snapshot;
    let mut map = std::collections::HashMap::new();
    map.insert("sol ring".to_string(), 7i64);
    map.insert("\tx".to_string(), 3i64);
    let mut cards = vec![
        json!({"id": "a", "name": "  SOL Ring  "}), // spaces trimmed by both
        json!({"id": "b", "name": "\tX"}),           // tab kept by both (key "\tx")
        json!({"id": "c", "name": "\u{a0}X"}),       // NBSP kept by both (no match)
        json!({"id": "d", "name": "   "}),           // trims to empty → 0
        json!({"id": "e", "name": null}),            // NULL name → empty key → 0
    ];
    assert!(attach_owned_from_snapshot(&mut cards, &map));
    let qty: Vec<i64> = cards.iter().map(|c| c["owned_qty"].as_i64().unwrap()).collect();
    assert_eq!(qty, vec![7, 3, 0, 0, 0]);
    // Key order preserved (owned_qty still appended last).
    let keys: Vec<&String> = cards[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["id", "name", "owned_qty"]);
    // A foreign card_id (not the row's own id) must bounce to the SQL path.
    let mut odd = vec![json!({"id": "a", "card_id": "zz", "name": "Sol Ring"})];
    assert!(!attach_owned_from_snapshot(&mut odd, &map));
}

// --- sidecar walk (iter 7) --------------------------------------------------
//
// The live leg port: zero-row field-filter searches page the scryfall-local
// sidecar exactly like Node's runSearch (broad query, first-word fallback,
// 175-window math, normalizeCard shaping, X-Source: scryfall / cache with
// the broad query's total). A 500 on the sidecar must proxy to Node verbatim.

use std::sync::Mutex;

/// Scryfall-shaped raw card (what the sidecar serves: no `prices`, no `lang`,
/// no `supertype` — the normalizeCard null-paths are the point).
fn raw_card(id: &str, name: &str, collector: &str, n: i64) -> Value {
    json!({
        "object": "card", "id": id, "oracle_id": format!("or-{id}"), "layout": "normal",
        "name": name, "manaid": null, "cmc": n, "colors": ["G"], "color_identity": ["G"],
        "keywords": ["Reach"], "type_line": "Legendary Creature — Spirit",
        "oracle_text": "Reach\nModified creatures you control have trample.",
        "power": "3", "toughness": "3", "mana_cost": "{2}{G}", "collector_number": collector,
        "released_at": "2022-02-18", "rarity": "mythic", "set": "neo", "set_name": "Kamigawa: Neon Dynasty",
        "image_uris": {"small": "https://x/small.jpg", "normal": "https://x/normal.jpg", "large": "https://x/large.jpg"},
        "scryfall_uri": "https://scryfall/card",
    })
}

/// Fake scryfall-local. Pages at 75 like the real sidecar (the walk's
/// 175-window math must ride whatever the sidecar does). Every received
/// (q, page) pair is recorded for the pagination assertions.
async fn spawn_fake_sidecar() -> (String, Arc<Mutex<Vec<(String, i64)>>>) {
    let seen: Arc<Mutex<Vec<(String, i64)>>> = Arc::new(Mutex::new(vec![]));
    let seen2 = seen.clone();
    let search = axum::routing::get(move |Query(params): Query<std::collections::HashMap<String, String>>| {
        let seen = seen2.clone();
        async move {
            let q = params.get("q").cloned().unwrap_or_default();
            let page: i64 = params.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
            seen.lock().unwrap().push((q.clone(), page));
            let (total, size): (u64, u64) = if q.starts_with("Kodama of the West Tree") {
                (7, 7)
            } else if q.starts_with("Kodama unique") {
                (41, 41) // the real sidecar's total for the first-word fallback
            } else if q.starts_with("Elvish Mystic") {
                (200, 75)
            } else if q.starts_with("sol ring") {
                (4, 4) // collection walk: 4 unique:cards printings, one name
            } else if q.starts_with("t:creature c:g") || q.starts_with("t:Creature c:G") {
                (4, 4) // operator-conjunction walk: green printings of an owned name
            } else if q.starts_with("fire // ice") {
                (1, 1) // one DFC — the walk must add full + both faces
            } else if q.starts_with("dfcwalk") {
                (1, 1) // match list containing a DFC, reached by a gated name query
            } else if q.contains("boom") {
                return (StatusCode::INTERNAL_SERVER_ERROR, "").into_response();
            } else if q.contains("invalidq") {
                // The engine's own syntax verdict → INVALID_QUERY 400, uncached.
                return (StatusCode::UNPROCESSABLE_ENTITY, "").into_response();
            } else if q.contains("nomatch") {
                // Scryfall "no cards found" family — tryQuery's 404/422 catch.
                return (StatusCode::NOT_FOUND, "").into_response();
            } else {
                (0, 0)
            };
            let start = (page as u64 - 1) * size;
            let n = total.saturating_sub(start).min(size);
            let data: Vec<Value> = (0..n)
                .map(|i| {
                    let idx = start + i;
                    let card_name = if q.starts_with("sol ring")
                        || q.starts_with("t:Creature c:G")
                        || q.starts_with("t:creature c:g")
                    {
                        "Sol Ring".to_string()
                    } else if q.starts_with("fire // ice") || q.starts_with("dfcwalk") {
                        "Fire // Ice".to_string()
                    } else {
                        q.split_whitespace().next().unwrap_or("Card").to_string()
                    };
                    raw_card(
                        &format!("sid-{q}-p{page}-{i}"),
                        &card_name,
                        &format!("{}", idx + 1),
                        (idx % 8) as i64,
                    )
                })
                .collect();
            let has_more = start + n < total;
            axum::Json(json!({
                "object": "list", "type": "card", "context": "search",
                "data": data, "cards": data, "total_cards": total, "total": total,
                "page": page, "page_size": size, "per_page": size, "offset": start,
                "has_more": has_more,
            }))
            .into_response()
        }
    });
    let r = Router::new().route("/cards/search", search);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn sidecar_router(path: PathBuf, upstream: &str, sidecar: &str) -> Router {
    router_for_idx_owned_sidecar(
        path,
        &["search", "search_sidecar"],
        upstream,
        NameIndex::new(),
        OwnedCache::new(),
        Some(sidecar.to_string()),
    )
}

/// Zero local rows → native walk: X-Source: scryfall, X-Total-Count from the
/// broad query, and Node's normalizeCard shape (key order included) with
/// owned_qty appended last.
#[tokio::test]
async fn sidecar_walk_serves_zero_row_name_with_node_contract() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let r = sidecar_router(path, &upstream, &sidecar);

    let (s, h, b) = get(&r, "/api/search?scope=database&name=Kodama%20of%20the%20West%20Tree&page=1&limit=20", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-source").unwrap(), "scryfall");
    assert_eq!(h.get("x-total-count").unwrap(), "7");
    assert_eq!(h.get("access-control-expose-headers").unwrap(), "X-Total-Count, X-Source");
    let cards: Vec<Value> = serde_json::from_slice(&b).unwrap();
    assert_eq!(cards.len(), 7);
    assert_eq!(
        cards[0].as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        vec![
            "id", "oracle_id", "scryfall_search_eligible", "name", "supertype", "subtypes",
            "types", "rarity", "set_id", "set_name", "number", "image_url", "price_trend",
            "price_normal", "price_holofoil", "price_etched", "cmc", "color_identity",
            "language", "printed_name", "tcgplayer_url", "cardmarket_url",
            "tcgplayer_product_id", "price_source", "price_currency", "released_at",
            "oracle_text", "oracle_faces", "owned_qty",
        ],
        "normalized key order must match the Node object literal (finishes omitted)"
    );
    assert_eq!(cards[0]["types"], json!(["Green"]));
    assert_eq!(cards[0]["subtypes"], json!(["Legendary", "Creature", "Spirit"]));
    assert_eq!(cards[0]["rarity"], "Mythic");
    assert_eq!(cards[0]["price_normal"], Value::Null, "no prices object → all-null money");
    assert_eq!(cards[0]["price_source"], Value::Null);
    assert_eq!(cards[0]["price_currency"], "USD");
    assert_eq!(cards[0]["language"], "English");
    assert_eq!(cards[0]["scryfall_search_eligible"], 1);
    assert_eq!(cards[0]["oracle_faces"], Value::Null);
    assert_eq!(cards[0]["image_url"], "https://x/normal.jpg");
    assert_eq!(cards[0]["released_at"], "2022-02-18");
    assert_eq!(cards[0]["owned_qty"], 0);
    // Only the broad query ran (it produced cards → no fallback).
    let got = seen.lock().unwrap().clone();
    assert_eq!(got, vec![("Kodama of the West Tree unique:prints -is:digital".to_string(), 1)]);
}

/// offset 200 past a 7-result broad query: empty window → the first-word
/// fallback (also empty there) → Node's `[] + X-Source: cache` WITH the
/// broad query's X-Total-Count, and no upsert side effects.
#[tokio::test]
async fn sidecar_walk_empty_returns_cache_source_with_total() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let r = sidecar_router(path, &upstream, &sidecar);

    let (s, h, b) = get(&r, "/api/search?scope=database&name=Kodama%20of%20the%20West%20Tree&page=3&limit=100", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-source").unwrap(), "cache", "empty merge → Node labels it cache");
    assert_eq!(h.get("x-total-count").unwrap(), "7", "broad query's total survives");
    assert_eq!(b, b"[]");
    let got = seen.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![
            ("Kodama of the West Tree unique:prints -is:digital".to_string(), 2),
            ("Kodama unique:prints -is:digital".to_string(), 2),
        ],
        "offset 200 → window page 2 (skip 25) of the 175 math; fallback runs only after empty broad"
    );
}

/// Multi-page window math: offset 200, limit 100 → fetch page 2 (skip 25),
/// then page 3 while short of the limit; stable sort by printed number.
#[tokio::test]
async fn sidecar_walk_multi_page_window_math() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let r = sidecar_router(path, &upstream, &sidecar);

    let (s, h, b) = get(&r, "/api/search?scope=database&name=Elvish%20Mystic&page=3&limit=100", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-source").unwrap(), "scryfall");
    assert_eq!(h.get("x-total-count").unwrap(), "200");
    let cards: Vec<Value> = serde_json::from_slice(&b).unwrap();
    assert_eq!(cards.len(), 100);
    let nums: Vec<i64> = cards.iter().map(|c| c["number"].as_str().unwrap().parse().unwrap()).collect();
    let mut sorted = nums.clone();
    sorted.sort();
    assert_eq!(nums, sorted, "stable parseInt(number)||0 ascending sort");
    assert_eq!(nums.first().copied(), Some(101), "window page 2 (sidecar pages at 75) skip 25 → item 101");
    let pages: Vec<i64> = seen.lock().unwrap().iter().map(|(_, p)| *p).collect();
    assert_eq!(pages, vec![2, 3], "175-window pagination, continues while short of limit");
}

/// A sidecar 500 must not 500 or fabricate: the request proxies and Node
/// answers verbatim (its own retries/503/upsert semantics preserved).
#[tokio::test]
async fn sidecar_failure_proxies_to_node() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, _seen) = spawn_fake_sidecar().await;
    // Walk fails (boom card) AND the iter-10 live merge fails (scripted
    // 500) — the request then keeps Node's problem, exactly like Node's
    // own catch → next(err).
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    ms.statuses
        .lock()
        .unwrap()
        .insert("GET /cards/search".into(), vec![500]);
    let mut st = sidecar_state(path, &upstream, &sidecar);
    st.scry = Arc::new(mock_scryfall::scry_for(mock, 0.0));
    let r = app(st);

    let (s, h, _b) = get(&r, "/api/search?scope=database&name=boom%20card&page=1&limit=20", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1", "must have gone through Node");
}

/// Walk failure alone must NOT sink the request: the merge leg answers
/// from the live client while the sidecar is down (iter-10 fall-through).
#[tokio::test]
async fn sidecar_failure_falls_through_to_live_merge() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, _seen) = spawn_fake_sidecar().await;
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    *ms.search.lock().unwrap() = json!({
        "data": [mock_scryfall::raw_card("zboom-a1", "Boom Card Alpha", "zz1", "1", Some(1.0))],
        "total_cards": 1, "has_more": false,
    });
    let mut st = sidecar_state(path, &upstream, &sidecar);
    st.scry = Arc::new(mock_scryfall::scry_for(mock, 0.0));
    let r = app(st);

    // Distinct name from the proxy test above: the process-wide rawPageCache
    // would otherwise serve this merged answer to that one (and vice versa).
    let (s, h, b) = get(&r, "/api/search?scope=database&name=boom%20card%20alpha&page=1&limit=20", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node"), None, "merge leg answers without Node");
    let body: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], json!("mtg-zboom-a1"), "normalizeCard mtg-id prefix");
    assert_eq!(h.get("x-source").unwrap(), "scryfall");
}

/// With search_sidecar off the kill switch the walk is skipped, but the
/// merge leg (iter-10) still answers natively — the sidecar stays untouched.
#[tokio::test]
async fn sidecar_kill_switch_proxies() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    *ms.search.lock().unwrap() = json!({
        "data": [mock_scryfall::raw_card("zswitch-1", "Kodama of the West Tree", "neo", "1", Some(1.0))],
        "total_cards": 1, "has_more": false,
    });
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: ["search".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(path),
        sidecar_url: Some(sidecar.clone()),
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    st.name_index = Arc::new(NameIndex::new());
    st.owned_index = Arc::new(OwnedCache::new());
    st.scry = Arc::new(mock_scryfall::scry_for(mock, 0.0));
    let r = app(st);
    let (s, h, b) = get(&r, "/api/search?scope=database&name=Kodama%20of%20the%20West%20Tree&page=1&limit=20", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node"), None, "merge leg answers without Node");
    let body: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 1, "merge leg answers the search");
    assert!(seen.lock().unwrap().is_empty(), "sidecar must not be touched");
}

/// Walked entries are cached per (params, rev) like cache-path entries:
/// repeat hits must not re-page the sidecar, and a revision bump must.
#[tokio::test]
async fn sidecar_walk_entry_caches_and_invalidates_with_rev() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let r = sidecar_router(path.clone(), &upstream, &sidecar);
    let url = "/api/search?scope=database&name=Kodama%20of%20the%20West%20Tree&page=1&limit=20";

    let (_, _, b1) = get(&r, url, &auth()).await;
    let (_, _, b2) = get(&r, url, &auth()).await;
    assert_eq!(b1, b2);
    assert_eq!(seen.lock().unwrap().len(), 1, "second request served from cache");

    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1;").unwrap();
    drop(conn);
    let (_, _, b3) = get(&r, url, &auth()).await;
    assert_eq!(b3, b1, "same body after rebuild");
    assert_eq!(seen.lock().unwrap().len(), 2, "rev bump forced a re-walk");
}

// --- single-flight (iter 8) ------------------------------------------------

fn sidecar_state(path: PathBuf, upstream: &str, sidecar: &str) -> AppState {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: ["search".to_string(), "search_sidecar".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        db_path: Some(path),
        sidecar_url: Some(sidecar.to_string()),
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    // A wired sidecar IS sidecar mode (LOCAL_SCRYFALL parity): the walk legs
    // answer; the iter-14 upstream-mode SQLite answer legs must stand down.
    st.local_scrayfall = true;
    st
}

async fn burst(r: &Router, url: &str) -> Vec<(StatusCode, HeaderMap, Vec<u8>)> {
    let mut handles = Vec::new();
    for _ in 0..8 {
        let r = r.clone();
        let u = url.to_string();
        handles.push(tokio::spawn(async move {
            let req = Request::builder()
                .method("GET")
                .uri(u)
                .extension(axum::extract::connect_info::ConnectInfo(
                    "127.0.0.1:55555".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .header("authorization", "Bearer tok-session")
                .extension(ConnectInfo(next_peer()))
                .body(Body::empty())
                .unwrap();
            let res = r.oneshot(req).await.unwrap();
            let (parts, body) = res.into_parts();
            (
                parts.status,
                parts.headers,
                axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec(),
            )
        }));
    }
    let mut hits = Vec::new();
    for h in handles {
        hits.push(h.await.unwrap());
    }
    hits
}

/// The write-mix moment: a burst of concurrent requests hits one cold key.
/// Without single-flight each request runs its own build — and, on the walk
/// leg, pages the sidecar for itself (the iter-7 realistic p99 tail). With
/// it, exactly ONE computation serves the burst (mirrors the collection and
/// stats tests; the sidecar `seen` log is the walk-path computes()).
#[tokio::test]
async fn concurrent_misses_coalesce_into_one_sidecar_walk() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let r = app(sidecar_state(path, &upstream, &sidecar));

    let hits = burst(&r, "/api/search?scope=database&name=Kodama%20of%20the%20West%20Tree&page=1&limit=20").await;
    for (s, h, b) in &hits {
        assert_eq!(*s, StatusCode::OK);
        assert_eq!(h.get("x-source").unwrap(), "scryfall");
        assert!(h.get("x-node").is_none(), "burst must not fall through to Node");
        assert_eq!(b, &hits[0].2, "every waiter in the burst gets the same body");
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "burst of 8 cold requests must page the sidecar exactly once, got {seen:?}");
}

#[tokio::test]
async fn concurrent_misses_coalesce_into_one_build() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream,
        static_dir: None,
        rust_routes: ["search".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(path),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    let cache = Arc::clone(&st.search_cache);
    let r = app(st);

    let hits = burst(&r, "/api/search?scope=database&name=Sol%20Ring").await;
    for (s, _, b) in &hits {
        assert_eq!(*s, StatusCode::OK);
        assert_eq!(b, &hits[0].2, "every waiter in the burst gets the same body");
    }
    assert_eq!(cache.computes(), 1, "burst of 8 cold requests must compute the body exactly once");
}

/// Gate D iter-25: distinct cold keys no longer spawn unbounded parallel
/// builds — every CPU leg queues on the shared build budget (twin of the
/// collection cache's `builds`, iter 15). Saturate the budget by hand,
/// start one cold request, and it must sit queued until a permit frees.
/// The unbounded (pre-iter-25) code completes this request during the
/// sleep, so the assertion below fails without the gate — it guards the
/// gate itself, not the fixture.
#[tokio::test]
async fn cold_build_waits_for_the_build_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream,
        static_dir: None,
        rust_routes: ["search".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(path),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = false;
    let cache = Arc::clone(&st.search_cache);
    let r = app(st);
    assert_eq!(cache.build_permits_available(), 2, "search build budget is 2 (collection parity)");

    let p1 = cache.build_permit().await;
    let p2 = cache.build_permit().await;
    assert_eq!(cache.build_permits_available(), 0);

    let h = {
        let r = r.clone();
        tokio::spawn(async move {
            let req = Request::builder()
                .method("GET")
                .uri("/api/search?scope=database&name=Sol%20Ring")
                .header("authorization", "Bearer tok-session")
                .extension(ConnectInfo(next_peer()))
                .body(Body::empty())
                .unwrap();
            r.oneshot(req).await.unwrap().status()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!h.is_finished(), "cold build must queue while the build budget is saturated");
    drop((p1, p2));
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), h)
        .await
        .expect("request must complete once the budget frees")
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache.computes(), 1, "the queued request computed exactly one body");
}

// --- collection-scope q leg (iter 9) ---------------------------------------
//
// Node answers `scope=collection&q=…` via resolveCollectionQuery: a match
// list from the durable collection_query_cache (walked through the sidecar
// on miss), intersected with the LIVE collection via ownedByNames. Rust
// serves ONLY the name-only free-text branch natively; every structured
// query stays on Node, and the deterministic engine verdicts (404 empty,
// 400/422 invalid) reproduce Node's contract byte-for-byte.

/// Seed one durable collection_query_cache row (Node's own upsert shape).
fn seed_cqc(path: &std::path::Path, query: &str, lang: &str, names: &str, total: i64, complete: i64, resolved_at: i64) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_query_cache (
           query TEXT NOT NULL, lang TEXT NOT NULL DEFAULT '', names TEXT NOT NULL,
           upstream_total INTEGER, fetched_count INTEGER NOT NULL DEFAULT 0,
           complete INTEGER NOT NULL DEFAULT 1, resolved_at INTEGER NOT NULL,
           expires_at INTEGER NOT NULL, PRIMARY KEY (query, lang));",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO collection_query_cache (query, lang, names, upstream_total, fetched_count, complete, resolved_at, expires_at)
         VALUES (?, ?, ?, ?, 3, ?, ?, ?)",
        rusqlite::params![query, lang, names, total, complete, resolved_at, resolved_at + 7 * 24 * 3600 * 1000],
    )
    .unwrap();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn cqc_row(path: &std::path::Path, query: &str, lang: &str) -> Option<(String, Option<i64>, i64)> {
    let conn = Connection::open(path).unwrap();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS collection_query_cache (
           query TEXT NOT NULL, lang TEXT NOT NULL DEFAULT '', names TEXT NOT NULL,
           upstream_total INTEGER, fetched_count INTEGER NOT NULL DEFAULT 0,
           complete INTEGER NOT NULL DEFAULT 1, resolved_at INTEGER NOT NULL,
           expires_at INTEGER NOT NULL, PRIMARY KEY (query, lang));",
        [],
    )
    .unwrap();
    let mut stmt = conn
        .prepare("SELECT names, upstream_total, complete FROM collection_query_cache WHERE query = ? AND lang = ?")
        .unwrap();
    let mut rows = stmt.query(rusqlite::params![query, lang]).unwrap();
    rows.next().unwrap().map(|r| {
        (
            r.get::<_, String>(0).unwrap(),
            r.get::<_, Option<i64>>(1).unwrap(),
            r.get::<_, i64>(2).unwrap(),
        )
    })
}

/// The exact response key order of the q leg (ownedByNames projection +
/// base_rarity). oracle_id/price_etched distinguish it from /api/collection.
fn q_row_keys() -> Vec<&'static str> {
    vec![
        "entry_id", "card_id", "quantity", "condition", "printing", "language", "purchase_price",
        "added_at", "is_trade", "notes", "is_proxy", "oracle_id", "name", "printed_name",
        "supertype", "subtypes", "types", "cmc", "color_identity", "rarity", "set_id", "set_name",
        "number", "image_url", "price_trend", "price_normal", "price_holofoil", "price_etched",
        "price_currency", "price_source", "price_estimated", "tcgplayer_url", "cardmarket_url",
        "tcgplayer_product_id", "base_rarity",
    ]
}

#[tokio::test]
async fn collq_durable_hit_serves_native_shape_and_headers() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    // Durable entry: the match list Node's walk would have produced. No
    // sidecar is wired at all — a walk attempt would show as an x-node hop.
    seed_cqc(path.as_path(), "sol ring", "en", r#"["sol ring"]"#, 4, 1, now_ms());
    let upstream = spawn_fake_node().await;
    // Sidecar mode: the durable/walk contract is the LOCAL_SCRYFALL parity
    // arm — upstream mode answers this query via the iter-14 SQLite legs.
    let r = router_for_sidecar_mode(path.clone(), &["search"], &format!("http://{upstream}"));
    // Whitespace/case normalization must land on the durable key 'sol ring'.
    let (s, h, b) = get_json(&r, "/api/search?scope=collection&q=%20SOL%20%20Ring%20&limit=60&page=1").await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none(), "durable hit must not walk or proxy");
    assert_eq!(h["x-total-count"], "3", "user 1 owns three Sol Ring printings");
    assert_eq!(h["x-catalog-complete"], "1");
    assert_eq!(h["x-catalog-upstream-total"], "4");
    assert_eq!(h["x-catalog-cache"], "fresh");
    assert_eq!(h["x-catalog-snapshot"], "remote:29e29f45850c995b7b8a:4", "membershipVersion must match Node's sha256");
    // resolveCollectionQuery answers with LIVE_SOURCE on every sidecar path;
    // only X-Catalog-Cache carries the age.
    assert_eq!(h["x-source"], "sidecar");
    assert_eq!(
        h["access-control-expose-headers"],
        "X-Total-Count, X-Catalog-Complete, X-Catalog-Upstream-Total, X-Catalog-Cache, X-Catalog-Snapshot, X-Source"
    );
    let rows = b.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    // ORDER BY added_at DESC, id DESC → entry 3? (2024-01-05 > 04 > 03) → 1,2,3.
    assert_eq!(rows[0]["entry_id"], 1);
    assert_eq!(rows[1]["entry_id"], 2);
    assert_eq!(rows[2]["entry_id"], 3);
    assert_eq!(
        rows[0].as_object().unwrap().keys().map(|s| s.as_str()).collect::<Vec<_>>(),
        q_row_keys()
    );
    // price_trend override: cc1 price_trend 4.01 (normal path of resolveCardPrice).
    assert_eq!(rows[0]["price_trend"], serde_json::json!(4.01));
    assert_eq!(rows[0]["subtypes"], serde_json::json!(["Artifact"]));
    assert_eq!(rows[0]["base_rarity"], "Uncommon");

    // count=0 suppresses X-Total-Count only (Node: includeTotal false).
    let (s, h, _) = get_json(&r, "/api/search?scope=collection&q=sol%20ring&limit=60&page=1&count=0").await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-total-count").is_none());
}

/// iter-16: the walk leg's `readCatalogSnapshot` expectedSnapshot contract.
/// Node verifies the caller's token against the CURRENT
/// `remote:{membershipVersion}:{revision}` on EVERY request (fresh build or
/// cached replay — the token is not part of the response-cache key) and
/// throws CATALOG_SNAPSHOT_CHANGED on mismatch; the route answers
/// 409 `{error: 'CATALOG_SNAPSHOT_CHANGED'}` (collection.js:221). Falsy
/// `snapshot` (route line 140: typeof !== 'string' for a duplicated key →
/// null) skips the check and never proxies.
#[tokio::test]
async fn collq_walk_snapshot_contract() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    seed_cqc(path.as_path(), "sol ring", "en", r#"["sol ring"]"#, 4, 1, now_ms());
    let upstream = spawn_fake_node().await;
    let r = router_for_sidecar_mode(path.clone(), &["search"], &format!("http://{upstream}"));

    let good = "remote:29e29f45850c995b7b8a:4";
    let base = "/api/search?scope=collection&q=sol%20ring&limit=60&page=1";

    // Cold build, no snapshot: the token the caller would echo next.
    let (s, h, _) = get_json(&r, base).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-catalog-snapshot"], good);

    // Matching token (warm replay path — same response-cache key) answers
    // natively with the full walk-leg contract.
    let (s, h, b) = get_json(&r, &format!("{base}&snapshot={good}")).await;
    assert_eq!(s, StatusCode::OK, "matching snapshot must answer natively");
    assert!(h.get("x-node").is_none(), "snapshot match must not proxy");
    assert_eq!(h["x-catalog-snapshot"], good);
    assert_eq!(h["x-total-count"], "3");
    assert_eq!(h["x-source"], "sidecar");
    assert_eq!(b.as_array().unwrap().len(), 3);

    // Mismatch → 409 with Node's exact body, answered natively.
    let (s, h, b) = get_json(&r, &format!("{base}&snapshot=x")).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(h.get("x-node").is_none(), "409 is answered natively, not proxied");
    assert_eq!(b, serde_json::json!({ "error": "CATALOG_SNAPSHOT_CHANGED" }));
    // ...and the check never poisons the shared cache entry.
    let (s, _, _) = get_json(&r, base).await;
    assert_eq!(s, StatusCode::OK, "a 409 must not evict the cached body");

    // Falsy snapshot skips the check (Node `if (expectedSnapshot && ...)`).
    let (s, h, _) = get_json(&r, &format!("{base}&snapshot=")).await;
    assert_eq!(s, StatusCode::OK, "empty snapshot is falsy → no check");
    assert!(h.get("x-node").is_none());

    // Duplicated key parses as a qs array: typeof !== 'string' → Node passes
    // expectedSnapshot null — no check anywhere, no proxy.
    let (s, h, _) = get_json(&r, &format!("{base}&snapshot=x&snapshot=y")).await;
    assert_eq!(s, StatusCode::OK, "array snapshot is null in Node → no check");
    assert!(h.get("x-node").is_none());

    // A real collection mutation bumps BOTH revision counters via triggers:
    // the pre-bump token must now 409, and the fresh token mint at the new
    // revision must 200 (header + check agree, like Node's single txn).
    {
        let conn = Connection::open(path.as_path()).unwrap();
        conn.execute(
            "INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price, added_at, notes, is_trade, user_id, is_proxy)
             VALUES (9, 'cc1', 1, 'Near Mint', 'Normal', 'English', NULL, '2024-02-01 00:00:00', '', 0, 1, 0)",
            [],
        )
        .unwrap();
        let rev: i64 = conn
            .query_row("SELECT revision FROM collection_snapshot WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rev, 5, "collection trigger bumps the catalog revision");
    }
    let (s, h, b) = get_json(&r, &format!("{base}&snapshot={good}")).await;
    assert_eq!(s, StatusCode::CONFLICT, "token minted at the old revision is stale");
    assert_eq!(b, serde_json::json!({ "error": "CATALOG_SNAPSHOT_CHANGED" }));
    let fresh = "remote:29e29f45850c995b7b8a:5";
    let (s, h, b) = get_json(&r, &format!("{base}&snapshot={fresh}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());
    assert_eq!(h["x-catalog-snapshot"], fresh);
    assert_eq!(b.as_array().unwrap().len(), 4, "the new owned row is in the answer");
}

#[tokio::test]
async fn collq_structured_queries_and_params_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    seed_cqc(path.as_path(), "sol ring", "en", r#"["sol ring"]"#, 4, 1, now_ms());
    let upstream = spawn_fake_node().await;
    // Sidecar mode (LOCAL_SCRYFALL parity): anything outside the proven walk
    // subset proxies. In upstream mode several of these are answered by the
    // iter-14 SQLite legs instead (pinned in gated_queries / collq_local).
    let r = router_for_sidecar_mode(path.clone(), &["search", "search_sidecar"], &format!("http://{upstream}"));
    for url in [
        "/api/search?scope=collection&q=t%3Acreature%20and%20and%20c%3Ag&limit=60", // doubled 'and' → Node throws → raw-passthrough key
        "/api/search?scope=collection&q=colorless",                                  // Node rewrites bare colorless to c:c
        "/api/search?scope=collection&q=t%3Acreature%20c%3Axyz&limit=60",            // invalid color → Node throws → raw-passthrough key
        "/api/search?scope=collection&q=%21sol%20ring",                  // exact name
        "/api/search?scope=collection&q=sol%20ring%20or%20plains",       // keyword token
        "/api/search?scope=collection&q=sol%20ring%20not",               // keyword token
        "/api/search?scope=collection&q=sol%20%28ring%29",               // grouping
        "/api/search?scope=collection&q=%22sol%20ring%22",               // quoted
        "/api/search?scope=collection&q=t%3A%22big%20creature%22",       // quoted operator value
        "/api/search?scope=collection&q=t%3Dcreature",                   // '=' comparator
        "/api/search?scope=collection&q=unique%3Acards",                 // smuggled sidecar param
        // (iter-16: `&snapshot=…` is no longer a proxy pin — the walk-leg
        // readCatalogSnapshot check is native; pinned in
        // collq_walk_snapshot_contract.)
        "/api/search?scope=collection&q=sol%20ring&count=0&count=1",     // qs array
        "/api/search?scope=database&q=sol%20ring",                       // other scope
    ] {
        let (s, h, _) = get_json(&r, url).await;
        assert_eq!(s, StatusCode::OK, "{url}");
        assert_eq!(h["x-node"], "1", "{url} must proxy to Node");
    }
}

#[tokio::test]
async fn collq_walk_resolves_persists_and_joins_single_flight() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = spawn_fake_node().await;
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let st = sidecar_state(path.clone(), &format!("http://{upstream}"), &sidecar);
    let r = app(st.clone());

    let hits = burst(&r, "/api/search?scope=collection&q=Sol%20Ring&limit=60&page=1").await;
    for (s, h, b) in &hits {
        assert_eq!(*s, StatusCode::OK);
        // The requests that performed the walk report resolved/sidecar —
        // Node's contract for the resolving request.
        assert_eq!(h["x-catalog-cache"], "resolved");
        assert_eq!(h["x-source"], "sidecar");
        assert_eq!(h["x-total-count"], "3");
        let v: Value = serde_json::from_slice(b).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 3);
    }
    let g = seen.lock().unwrap();
    let walked: Vec<_> = g.iter().filter(|(q, _)| q == "sol ring unique:cards").collect();
    assert_eq!(walked.len(), 1, "burst of 8 must pay exactly ONE sidecar walk");
    drop(g);
    // Durable upsert with Node's row shape.
    let (names, total, complete) = cqc_row(&path, "sol ring", "en").expect("row persisted");
    assert_eq!(names, r#"["sol ring"]"#);
    assert_eq!(total, Some(4));
    assert_eq!(complete, 1);
    assert_eq!(st.collq_cache.walks(), 1);

    // Second burst: in-memory entry, zero sidecar traffic, SearchCache hit.
    // X-Catalog-Cache ages per-request (Node semantics); X-Source stays
    // 'sidecar' — LIVE_SOURCE on every resolveCollectionQuery path.
    let hits2 = burst(&r, "/api/search?scope=collection&q=Sol%20Ring&limit=60&page=1").await;
    assert_eq!(hits2[0].1["x-catalog-cache"], "fresh");
    assert_eq!(hits2[0].1["x-source"], "sidecar");
    let g2 = seen.lock().unwrap();
    let walked2: Vec<_> = g2.iter().filter(|(q, _)| q == "sol ring unique:cards").collect();
    assert_eq!(walked2.len(), 1, "warm entries must not re-walk");
    drop(g2);
}

// --- iter 10: operator-conjunction subset of the q leg ---------------------

/// Pure gate battery. Every ACCEPT expectation is pinned against the real
/// `shared/scryfallQuery.js` (`analyze` → `toScryfall(stripDirectives(…))`),
/// run as ground truth — if the grammar's re-serialization changes, this
/// test must change with it, and it should.
#[test]
fn collq_walk_base_operator_battery() {
    const ACCEPT: &[(&str, &str)] = &[
        // The bench route's query — byte-identical echo.
        ("t:creature c:g", "t:creature c:g"),
        // Keyword lowercased (tokenizer toLowerCase), value verbatim.
        ("T:Creature C:G", "t:Creature c:G"),
        ("C:WU", "c:WU"),
        // Whitespace collapses; JS trim drops edges.
        ("  t:creature   c:g ", "t:creature c:g"),
        ("t:creature\tand c:g", "t:creature c:g"),
        // 'and' strictly between terms is consumed by parseAnd.
        ("t:creature and c:g", "t:creature c:g"),
        ("rust and gold", "rust gold"),
        // Name + operator mixtures (all game-card terms → ownedByNames leg).
        ("sol ring t:creature", "sol ring t:creature"),
        ("t:cre aure", "t:cre aure"),
        // Keyword spellings ride through as typed (raw preserved).
        ("type:creature colour:green", "type:creature colour:green"),
        ("c:colorless", "c:colorless"),
        ("c:m t:creature", "c:m t:creature"),
        ("c:multicolor t:instant", "c:multicolor t:instant"),
        // Digit counts and letter runs pass the color classifier.
        ("c:5", "c:5"),
        // Values keep any interior punctuation Node's readBare keeps.
        ("t:a-b", "t:a-b"),
        ("t:>x", "t:>x"),
        ("t:creaturec:g", "t:creaturec:g"),
        // Unicode names keep iter-9 semantics.
        ("t:creature naïve", "t:creature naïve"),
        // Plain name-only queries keep the exact iter-9 output.
        ("Sol Ring", "sol ring"),
        // `is:` Scrybox extension: a TYPE_WORDS value is rewritten to the
        // canonical t: leaf by Node's own leafToScryfall (verified against
        // shared/scryfallQuery.js); hyphenated words gain a space and are
        // QUOTED; anything else is the isflag leaf, echoed verbatim.
        ("is:land", "t:land"),
        ("is:Land", "t:land"),
        ("is:world", "t:world"),
        ("is:basic-land", "t:\"basic land\""),
        ("is:Foo", "is:Foo"),
        ("is:land t:creature", "t:land t:creature"),
    ];
    const REJECT: &[&str] = &[
        // Node's parser THROWS on these → sidecar mode walks the raw string
        // verbatim: a different durable key, so the gate must not accept.
        "c:xyz",
        "t:creature c:mono-b",
        "t:",
        "and",
        "and t:creature",
        "t:creature and",
        "t:a and and t:c", // Node elides both — proxy is the safe subset
        // Grammar structures outside the subset.
        "or",
        "sol ring or plains",
        "sol ring not",
        "unique:cards",
        "oracle:fire",
        "t=creature",
        "c!=g",
        "t:creature|instant",
        "\"sol ring\"",
        "sol (ring)",
        "!sol ring",
        "-t:artifact",
        "t:\"big creature\"",
        "t:/gob/",
        "/gob/",
        // The iter-9 parity hole: makeLeaf rewrites bare `colorless` to the
        // color leaf c:c — echoing it would walk a query Node never sends.
        "colorless",
        "COLORLESS goblin",
        "t:creature colorless",
    ];
    for (q, want) in ACCEPT {
        assert_eq!(scrybox_server::collq::walk_base(q).as_deref(), Some(*want), "accept {q:?}");
    }
    for q in REJECT {
        assert_eq!(scrybox_server::collq::walk_base(q), None, "reject {q:?}");
    }
}

#[tokio::test]
async fn collq_operator_conjunction_walks_persists_and_serves_natively() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = spawn_fake_node().await;
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let st = sidecar_state(path.clone(), &format!("http://{upstream}"), &sidecar);
    let r = app(st.clone());

    // Cold burst on the operator query: served NATIVELY (no x-node hop), one
    // single-flight walk, on the byte-exact toScryfall spelling Node uses —
    // keyword lowercased, value verbatim, ' unique:cards' scope appended.
    let hits = burst(&r, "/api/search?scope=collection&q=T%3ACreature%20c%3AG&limit=60&page=1").await;
    for (s, h, b) in &hits {
        assert_eq!(*s, StatusCode::OK);
        assert!(h.get("x-node").is_none(), "operator conjunction must not proxy");
        assert_eq!(h["x-catalog-cache"], "resolved");
        assert_eq!(h["x-source"], "sidecar");
        assert_eq!(h["x-total-count"], "3");
        let v: Value = serde_json::from_slice(b).unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["name"], "Sol Ring");
        assert_eq!(
            rows[0].as_object().unwrap().keys().map(|s| s.as_str()).collect::<Vec<_>>(),
            q_row_keys()
        );
    }
    let g = seen.lock().unwrap();
    assert_eq!(
        g.iter().filter(|(q, _)| q == "t:Creature c:G unique:cards").count(),
        1,
        "burst pays exactly ONE walk, on the Node-identical query"
    );
    drop(g);
    // Durable row lands under Node's own key (same bytes → shared answer).
    let (names, total, complete) = cqc_row(&path, "t:Creature c:G", "en").expect("row persisted");
    assert_eq!(names, r#"["sol ring"]"#);
    assert_eq!(total, Some(4));
    assert_eq!(complete, 1);
    assert_eq!(st.collq_cache.walks(), 1);

    // Warm burst: memory hit, zero new sidecar traffic.
    let hits2 = burst(&r, "/api/search?scope=collection&q=T%3ACreature%20c%3AG&limit=60&page=1").await;
    assert_eq!(hits2[0].1["x-catalog-cache"], "fresh");
    assert_eq!(hits2[0].1["x-source"], "sidecar");
    let g2 = seen.lock().unwrap();
    assert_eq!(g2.iter().filter(|(q, _)| q == "t:Creature c:G unique:cards").count(), 1);
    drop(g2);

    // The 'and'-elision spelling normalizes the keyword away; values keep
    // their typed case (Node's leafToScryfall is verbatim on values), so
    // this lowercase spelling is a SEPARATE durable key that pays its own
    // single walk — and the walked string proves 'and' never reaches the
    // sidecar.
    let (s, h, b) = get_json(&r, "/api/search?scope=collection&q=t%3Acreature%20and%20c%3Ag&limit=60&page=1").await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());
    assert_eq!(h["x-catalog-cache"], "resolved");
    assert_eq!(h["x-total-count"], "3");
    assert_eq!(b.as_array().unwrap().len(), 3);
    let g3 = seen.lock().unwrap();
    assert_eq!(g3.iter().filter(|(q, _)| q.starts_with("t:creature and")).count(), 0);
    assert_eq!(g3.iter().filter(|(q, _)| q == "t:creature c:g unique:cards").count(), 1);
    drop(g3);
    // And it lands its own Node-shaped durable row under that key.
    let (names, total, complete) = cqc_row(&path, "t:creature c:g", "en").expect("row persisted");
    assert_eq!(names, r#"["sol ring"]"#);
    assert_eq!(total, Some(4));
    assert_eq!(complete, 1);
}

#[tokio::test]
async fn collq_404_is_durable_empty_answer() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = spawn_fake_node().await;
    let (sidecar, _seen) = spawn_fake_sidecar().await;
    let r = sidecar_router(path.clone(), &format!("http://{upstream}"), &sidecar);
    let (s, h, b) = get_json(&r, "/api/search?scope=collection&q=nomatch%20card&limit=20&page=1").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-catalog-cache"], "resolved");
    assert_eq!(h["x-catalog-complete"], "1");
    assert_eq!(h["x-catalog-upstream-total"], "0");
    assert_eq!(h["x-total-count"], "0");
    assert_eq!(h["x-source"], "sidecar");
    assert_eq!(b, json!([]));
    let (names, total, complete) = cqc_row(&path, "nomatch card", "en").expect("404 answer persisted");
    assert_eq!(names, "[]");
    assert_eq!(total, Some(0));
    assert_eq!(complete, 1);
}

#[tokio::test]
async fn collq_engine_422_is_400_invalid_query_and_stays_uncached() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = spawn_fake_node().await;
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let r = sidecar_router(path.clone(), &format!("http://{upstream}"), &sidecar);
    let (s, _h, b) = get_json(&r, "/api/search?scope=collection&q=invalidq%20name").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b, json!({ "error": "INVALID_QUERY" }));
    assert!(cqc_row(&path, "invalidq name", "en").is_none(), "400 must not persist an entry");
    // Node re-walks on every retry (its catch caches nothing) — so must we.
    let _ = get_json(&r, "/api/search?scope=collection&q=invalidq%20name").await;
    let n = seen.lock().unwrap().iter().filter(|(q, _)| q.starts_with("invalidq name")).count();
    assert_eq!(n, 2, "invalid answers are never cached");
}

#[tokio::test]
async fn collq_transport_failure_and_missing_sidecar_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = spawn_fake_node().await;
    let (sidecar, _seen) = spawn_fake_sidecar().await;
    let r = sidecar_router(path.clone(), &format!("http://{upstream}"), &sidecar);
    let (s, h, _) = get_json(&r, "/api/search?scope=collection&q=boom%20thing").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-node"], "1", "sidecar 500 → Node answers (local-fallback/503 is Node's)");
    assert_eq!(h["x-source"], "live");

    // Kill switch off (no search_sidecar): a cold key with no durable row
    // proxies instead of walking. Sidecar mode — upstream mode would answer
    // this name query natively via the iter-14 SQLite leg.
    let r2 = router_for_idx_owned_sidecar(
        path.clone(),
        &["search"],
        &format!("http://{upstream}"),
        NameIndex::new(),
        OwnedCache::new(),
        Some(sidecar.clone()),
    );
    let (_s, h2, _) = get_json(&r2, "/api/search?scope=collection&q=some%20cold%20name").await;
    assert_eq!(h2["x-node"], "1");
}

#[tokio::test]
async fn collq_window_semantics_and_stale_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    seed_cqc(path.as_path(), "sol ring", "en", r#"["sol ring"]"#, 4, 1, now_ms());
    let upstream = spawn_fake_node().await;
    let r = router_for(path.clone(), &["search"], &format!("http://{upstream}"));

    // No page/limit keys → the COMPLETE owned result (no window), total set.
    let (s, h, b) = get_json(&r, "/api/search?scope=collection&q=sol%20ring").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 3);
    assert_eq!(h["x-total-count"], "3");
    // Explicit page=2&limit=2 → window (third entry), total unaffected.
    let (_, _h, b2) = get_json(&r, "/api/search?scope=collection&q=sol%20ring&page=2&limit=2").await;
    let rows = b2.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["entry_id"], 3);
    // Empty-string page key still counts as "explicit pagination" (qs != null).
    let (_, _h, b3) = get_json(&r, "/api/search?scope=collection&q=sol%20ring&page=").await;
    assert_eq!(b3.as_array().unwrap().len(), 3, "page='' → parseInt NaN → 1 → full window fits");

    // Incomplete + older than the stale bound (7d) → re-walk required.
    seed_cqc(path.as_path(), "plains", "en", "[]", 1, 0, now_ms() - 8 * 24 * 3600 * 1000);
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let r2 = sidecar_router(path.clone(), &format!("http://{upstream}"), &sidecar);
    let (s, _h, _b) = get_json(&r2, "/api/search?scope=collection&q=plains").await;
    assert_eq!(s, StatusCode::OK);
    assert!(seen.lock().unwrap().iter().any(|(q, _)| q == "plains unique:cards"), "expired-incomplete entry must re-walk");

    // Incomplete but within the stale bound → served immediately ('stale').
    seed_cqc(path.as_path(), "swamp", "en", r#"["swamp"]"#, 1, 0, now_ms() - 8 * 3600 * 1000);
    let (s2, h2, _b2) = get_json(&r2, "/api/search?scope=collection&q=swamp").await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(h2["x-catalog-cache"], "stale");
    assert!(!seen.lock().unwrap().iter().any(|(q, _)| q == "swamp unique:cards"), "stale-usable entry must NOT walk");
}

#[tokio::test]
async fn collq_dfc_walk_stores_full_and_face_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = spawn_fake_node().await;
    let (sidecar, _seen) = spawn_fake_sidecar().await;
    let r = sidecar_router(path.clone(), &format!("http://{upstream}"), &sidecar);
    // A gated name query whose MATCH LIST contains a double-faced card: the
    // walk must persist the full "A // B" plus both faces (Node's Set).
    let (s, h, b) = get_json(&r, "/api/search?scope=collection&q=dfcwalk&limit=20&page=1").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-catalog-cache"], "resolved");
    assert_eq!(h["x-catalog-upstream-total"], "1");
    assert_eq!(b, json!([]), "fixture owns no Fire // Ice printings");
    let (names, _t, _c) = cqc_row(&path, "dfcwalk", "en").expect("persisted");
    let parsed: Vec<String> = serde_json::from_str(&names).unwrap();
    assert_eq!(parsed, vec!["fire // ice", "fire", "ice"], "walk stores full + faces, insertion order");
}

// --- collection-scope field-filter leg (cardSearchSql.collectionQuery) ---

/// Extra rows the collection leg exercises: a Japanese printing that matches
/// ONLY via printed_name (the leg must ignore `lang` — owners see every
/// language they own), a promo pair whose collector numbers both CAST to 0
/// (the numberClause guard from the #regression), and a zero-quantity row.
fn owned_extra_db(dir: &std::path::Path) -> PathBuf {
    let path = seeded_db(dir);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "
        INSERT INTO collection (id, card_id, quantity, user_id) VALUES (10, 'cc-ja', 2, 1);
        INSERT INTO collection (id, card_id, quantity, user_id) VALUES (11, 'cc2', 3, 1);
        INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number,
                                language, price_estimated)
          VALUES ('cc-promo1', 'Promo X', 'Land', '[]', '[]', 'Common', 'p1', 'Promo One', 'TG12', 'English', 0),
                 ('cc-promo2', 'Promo X', 'Land', '[]', '[]', 'Common', 'p2', 'Promo Two', 'SV49', 'English', 0);
        INSERT INTO collection (id, card_id, quantity, user_id) VALUES (12, 'cc-promo1', 1, 1), (13, 'cc-promo2', 1, 1);
        INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number,
                                language, price_estimated)
          VALUES ('cc-dead', 'Zerocard', 'Land', '[]', '[]', 'Common', 'p3', 'Zero Set', '1', 'English', 0);
        INSERT INTO collection (id, card_id, quantity, user_id) VALUES (14, 'cc-dead', 0, 1);
        INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-u2', 2, '2099-01-01 00:00:00');
        ",
    )
    .unwrap();
    path
}

fn card_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn collection_scope_shape_headers_and_cross_language() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(owned_extra_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let (s, h, body) = get_json(&r, "/api/search?scope=collection&name=Sol%20Ring").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
        "application/json; charset=utf-8"
    );
    // Node's route sets NO X-Source / Access-Control-Expose-Headers /
    // X-Total-Count on this leg (meta.source unset, meta.total null).
    assert!(h.get("x-source").is_none(), "collection leg never sets X-Source");
    assert!(h.get("access-control-expose-headers").is_none());
    assert!(h.get("x-total-count").is_none());
    assert!(h.get(header::ETAG).is_some());
    assert!(h.get("x-node").is_none(), "must not hop to Node");

    // Card id ascending (matched ORDER BY card_id): the Japanese printing
    // sorts first ('-' 0x2D < '1'), matched via printed_name.
    assert_eq!(card_ids(&body), ["cc-ja", "cc1", "cc6", "cc7"]);
    // Column order = card_cache table order + appended owned_qty.
    let keys: Vec<&String> = body[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, card_keys());
    // Logical totals: 'sol ring' group carries 2+1+1; the Japanese row is its
    // own card_key (ソール・リング) with its own 2.
    for c in body.as_array().unwrap() {
        let want = if c["id"] == json!("cc-ja") { 2 } else { 4 };
        assert_eq!(c["owned_qty"], json!(want), "owned_qty for {}", c["id"]);
    }

    // ETag replays as 304, WITHOUT the X-Source/expose headers.
    let etag = h.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let (s304, h304, b304) = get(
        &r,
        "/api/search?scope=collection&name=Sol%20Ring",
        &[("authorization", "Bearer tok-session"), ("if-none-match", etag.as_str())],
    )
    .await;
    assert_eq!(s304, StatusCode::NOT_MODIFIED);
    assert!(h304.get("x-source").is_none());
    assert!(h304.get("access-control-expose-headers").is_none());
    assert!(b304.is_empty());
}

#[tokio::test]
async fn collection_scope_number_cast_and_promo_guard() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(owned_extra_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // "002" finds the stored "2" through the numeric CAST term.
    let (_, _, body) = get_json(&r, "/api/search?scope=collection&number=002").await;
    assert_eq!(card_ids(&body), ["cc2"]);

    // The un-anchored-CAST over-match guard: TG12 and SV49 both CAST to 0,
    // so only exact matching may apply — SV49 must not surface TG12.
    let (_, _, body) = get_json(&r, "/api/search?scope=collection&number=SV49").await;
    assert_eq!(card_ids(&body), ["cc-promo2"]);

    // Route-level number cleaning (# prefix, /b suffix) before numberClause.
    let (_, _, body) = get_json(&r, "/api/search?scope=collection&name=Sol%20Ring&number=%23160%2Fbrc").await;
    assert_eq!(card_ids(&body), ["cc1"]);
    assert_eq!(body[0]["owned_qty"], json!(4));
}

#[tokio::test]
async fn collection_scope_sets_pagination_and_empties() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(owned_extra_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // set=lea: the 'sol ring' logical group collapses to the single printing
    // IN that set, while owned_qty stays the logical total.
    let (_, _, body) = get_json(&r, "/api/search?scope=collection&name=Sol%20Ring&set=lea").await;
    assert_eq!(card_ids(&body), ["cc6"]);
    assert_eq!(body[0]["owned_qty"], json!(4));

    // Multi-set "lea, 2ed" (comma + space) spans both printings.
    let (_, _, body) = get_json(&r, "/api/search?scope=collection&name=Sol%20Ring&set=lea%2C%202ed").await;
    assert_eq!(card_ids(&body), ["cc6", "cc7"]);

    // Window over the card_id-ordered rows: page 2 of 2.
    let (_, _, body) = get_json(&r, "/api/search?scope=collection&name=Sol%20Ring&page=2&limit=2").await;
    assert_eq!(card_ids(&body), ["cc6", "cc7"]);

    // Zero-quantity ownership is NOT an owned search result — and an empty
    // answer is served natively (no fall-through to Node, no 502 standalone).
    let (s, h, body) = get_json(&r, "/api/search?scope=collection&name=Zerocard").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, json!([]));
    assert!(h.get("x-node").is_none(), "empty must not proxy on this leg");

    // The other user's window is their OWN collection only (user 2 owns cc1 ×5).
    let (s, _h, body) = get(
        &r,
        "/api/search?scope=collection&name=Sol%20Ring",
        &[("authorization", "Bearer tok-u2")],
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(s, StatusCode::OK);
    assert_eq!(card_ids(&body), ["cc1"]);
    assert_eq!(body[0]["owned_qty"], json!(5));
}

#[tokio::test]
async fn collection_scope_prints_inert_and_image_proxies() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(owned_extra_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // prints=1 is inert for collection scope (Node's allPrints branch is
    // gated scope !== 'collection') → still answered natively.
    let (_, h, _b) = get_json(&r, "/api/search?scope=collection&name=Plains&prints=1").await;
    assert!(h.get("x-node").is_none(), "prints must not force the proxy here");

    // image is NOT inert: the route cvScan-ranks the rows afterwards, so the
    // request belongs to Node.
    let (_, h, _b) = get_json(
        &r,
        "/api/search?scope=collection&name=Plains&image=Zm9v",
    )
    .await;
    assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1", "image must hop to Node");
}

// --- searchLimiter twin (F3-TC4) -------------------------------------------

/// Node mounts express-rate-limit's searchLimiter (300/15min per client IP,
/// middleware/auth.js) ahead of searchCards, AFTER authenticateToken. The
/// e2e F3-TC4 burst is the integration proof; this is the unit regression for
/// the bucket advancing on EVERY authenticated request — including ones the
/// native path serves from cache (the iter-7 bug: cache hits skipped the
/// bucket, so 350-request bursts never saw a 429). Unique client IP so the
/// process-global limiter cannot collide with the other tests' 127.0.0.1.
#[tokio::test]
async fn search_limiter_blocks_after_300_per_ip_in_window() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let hit = |i: u32| {
        let r = r.clone();
        async move {
            let req = Request::builder()
                .method("GET")
                .uri("/api/search?game=mtg&name=Sol%20Ring")
                .extension(axum::extract::connect_info::ConnectInfo(
                    format!("203.0.113.9:{}", 40000 + i).parse::<SocketAddr>().unwrap(),
                ))
                .header("authorization", "Bearer tok-session")
                .body(Body::empty())
                .unwrap();
            let res = r.oneshot(req).await.unwrap();
            let status = res.status();
            let headers = res.headers().clone();
            let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec();
            (status, headers, body)
        }
    };

    for i in 1..=300 {
        let (s, h, _) = hit(i).await;
        assert_ne!(s, StatusCode::TOO_MANY_REQUESTS, "request {i} must not be limited yet");
        assert_eq!(
            h.get("ratelimit-limit").map(|v| v.to_str().unwrap()),
            Some("300"),
            "every authenticated search response carries the limiter headers (request {i})"
        );
        assert_eq!(
            h.get("ratelimit-remaining").map(|v| v.to_str().unwrap()),
            Some((300 - i).to_string().as_str()),
            "bucket advances on every request, including cache hits (request {i})"
        );
        assert_eq!(h.get("ratelimit-policy").map(|v| v.to_str().unwrap()), Some("300;w=900"));
    }

    let (s, h, b) = hit(301).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap(),
        json!({ "error": "Too many searches. Please slow down and try again shortly." })
    );
    assert_eq!(h.get("ratelimit-remaining").map(|v| v.to_str().unwrap()), Some("0"));
    let retry_after: Option<String> = h.get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
    let ratelimit_reset: Option<String> = h.get("ratelimit-reset").and_then(|v| v.to_str().ok()).map(str::to_string);
    assert_eq!(retry_after, ratelimit_reset, "retry-after mirrors ratelimit-reset");

    // A different client IP has its own bucket (per-IP key, not global).
    let req = Request::builder()
        .method("GET")
        .uri("/api/search?game=mtg&name=Sol%20Ring")
        .extension(axum::extract::connect_info::ConnectInfo(
            "203.0.113.10:40000".parse::<SocketAddr>().unwrap(),
        ))
        .header("authorization", "Bearer tok-session")
        .body(Body::empty())
        .unwrap();
    let res = r.oneshot(req).await.unwrap();
    assert_ne!(res.status(), StatusCode::TOO_MANY_REQUESTS, "bucket is per client IP");
}

// --- stale background refresh (F3-TC8) --------------------------------------

mod mock_scryfall;

/// Node's queryLocal stale leg (scryfallApi.js:756): rows older than
/// CACHE_AGE_LIMIT_MS (3 days) are answered from cache immediately AND
/// re-fetched fire-and-forget through bulkFetchByIdentifier → cacheCards.
/// F3-TC8 drives the same contract over HTTP; this pins the internals: the
/// batch shape (identifier pick), the cache write, and that fresh rows are
/// never collected.
#[tokio::test]
async fn stale_rows_trigger_background_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    {
        let conn = Connection::open(&db).unwrap();
        // F3-TC8 shape: the Sol Ring printings are 4 days old; cc2 (Plains)
        // is reset to CURRENT_TIMESTAMP as the never-collected control
        // (the fixture stamps every row '2026-01-01', which is >3 days ago).
        conn.execute(
            "UPDATE card_cache SET last_updated = '2020-01-01T00:00:00.000Z' WHERE id IN ('cc1','cc6','cc7')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE card_cache SET last_updated = CURRENT_TIMESTAMP WHERE id = 'cc2'",
            [],
        )
        .unwrap();
    }

    let upstream = format!("http://{}", spawn_fake_node().await);
    let (scry_addr, scry) = mock_scryfall::spawn_mock().await;
    scry.collection_batches.lock().unwrap().push(json!({
        "object": "list",
        "data": [mock_scryfall::raw_card("stale1", "Sol Ring", "brc", "160", Some(3.5))],
        "not_found": []
    }));

    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream,
        static_dir: None,
        rust_routes: ["search".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(db.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.stale_refresh = true;
    st.scry = Arc::new(mock_scryfall::scry_for(scry_addr, 0.0));
    let r = app(st);

    // The cached (stale) rows go out untouched, source cache, no Node hop.
    let (s, h, body) = get(&r, "/api/search?name=Sol%20Ring", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-source").unwrap(), "cache");
    assert!(h.get("x-node").is_none());
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 3, "cached answer unchanged by the stale leg (cc1/cc6/cc7; cc-ja is language-filtered)");

    // The background refresh lands: cacheCards wrote the mock's normalized
    // card (normalizeCard prefixes 'mtg-').
    let mut landed = false;
    for _ in 0..100 {
        let conn = Connection::open(&db).unwrap();
        landed = conn
            .query_row("SELECT last_updated FROM card_cache WHERE id = 'mtg-stale1'", [], |r| {
                r.get::<_, String>(0)
            })
            .is_ok();
        if landed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(landed, "background refresh must cacheCards the stale rows");

    let hits = scry.hits_for("POST /cards/collection");
    assert_eq!(hits.len(), 1, "one batch for ≤75 stale rows");
    let sent: Value = serde_json::from_str(&hits[0].1).unwrap();
    let idents = sent["identifiers"].as_array().unwrap();
    // Identifier pick (bulkFetchByIdentifier): cc1/cc6/cc7 all carry
    // set_id+number → {set, collector_number} (lowercased), never {name}.
    assert_eq!(idents.len(), 3, "only the three stale rows are collected");
    assert!(idents.iter().all(|i| i.get("set").is_some() && i.get("collector_number").is_some()));
    assert!(idents.iter().any(|i| i["set"] == json!("brc") && i["collector_number"] == json!("160")));
    assert!(idents.iter().any(|i| i["set"] == json!("lea") && i["collector_number"] == json!("1")));

    // Fresh rows are never collected: the Plains search (cc2, CURRENT_TIMESTAMP)
    // adds no new batch.
    let (s, _h, _body) = get(&r, "/api/search?name=Plains", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(scry.hits_for("POST /cards/collection").len(), 1, "fresh rows trigger no refresh");
}

// --- live specific/broad/fallback merge (iter 10) --------------------------
//
// runSearch's merge block (scryfallApi.js:776-881) ported natively: the
// field-filter legs after queryLocal comes back empty (scope=database) or
// is skipped (scope=internet). These are the Rust twins of the Node e2e
// cases F5-TC3 (set+number pin) and F3-TC9 (internet multilingual), plus
// mergeSearchPrices' cache/upstream price merge.

#[tokio::test]
async fn live_merge_set_number_pinned_skips_broad() {
    // F5-TC3 twin: set+number lands on the specific query; the exact hit
    // (numberPinnedIt) short-circuits the broad leg entirely, X-Total-Count
    // is the exact count (1), X-Source: scryfall, and the awaited
    // cacheCards write lands in card_cache.
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    *ms.search.lock().unwrap() = json!({
        "data": [mock_scryfall::raw_card("zz1-232", "Zzland", "zz1", "232", Some(2.5))],
        "total_cards": 7, "has_more": false,
    });
    let r = router_for_live(path.clone(), &["search"], &format!("http://{upstream}"), mock_scryfall::scry_for(mock, 0.0));

    let (s, h, b) = get(&r, "/api/search?set=zz1&number=232", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node"), None);
    let body: Value = serde_json::from_slice(&b).unwrap();
    let cards = body.as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], json!("mtg-zz1-232"), "normalizeCard mtg-id prefix");
    assert_eq!(h.get("x-total-count").unwrap(), "1", "numberPinnedIt → exact count, not total_cards");
    assert_eq!(h.get("x-source").unwrap(), "scryfall");
    // Only the specific query went out — the broad leg was skipped.
    let search_hits = ms.hits_for("GET /cards/search");
    assert_eq!(search_hits.len(), 1, "broad leg skipped after number-pinned exact");
    assert!(search_hits[0].0.contains("q=set%3Azz1%20cn%3A232"), "{}", search_hits[0].0);
    // cacheCards was awaited before the response: the row is queryable.
    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM card_cache WHERE id = 'mtg-zz1-232'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "merge response awaited the cache write");
}

#[tokio::test]
async fn live_merge_internet_ja_lang_search() {
    // F3-TC9 twin: scope=internet skips queryLocal and answers from the
    // merge; a non-English lang feeds langSearch's resolve so the broad
    // query carries lang:ja + include_multilingual, and the sidecar-shaped
    // raw card comes back normalizeCard'd (language display name,
    // printed_name kept, owned_qty attached).
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    *ms.search.lock().unwrap() = json!({
        "data": [{
            "id": "jp123", "oracle_id": "oj", "name": "Black Lotus",
            "printed_name": "\u{9ed2}\u{304d}\u{8472}", "lang": "ja",
            "layout": "normal", "rarity": "uncommon", "type_line": "Artifact",
            "collector_number": "1", "set": "zzj", "set_name": "Zz Japan",
            "cmc": 0, "colors": [], "color_identity": [],
        }],
        "total_cards": 1, "has_more": false,
    });
    let r = router_for_live(path, &["search"], &format!("http://{upstream}"), mock_scryfall::scry_for(mock, 0.0));

    let (s, h, b) = get(&r, "/api/search?scope=internet&name=Black%20Lotus&lang=ja", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node"), None, "internet scope answers natively now");
    let body: Value = serde_json::from_slice(&b).unwrap();
    let cards = body.as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], json!("mtg-jp123"));
    assert_eq!(cards[0]["language"], json!("Japanese"), "langSearch language survives");
    assert_eq!(cards[0]["printed_name"], json!("\u{9ed2}\u{304d}\u{8472}"));
    assert_eq!(cards[0]["name"], json!("Black Lotus"), "canonical name kept");
    assert_eq!(cards[0]["owned_qty"], json!(0), "internet rows get owned_qty too");
    assert_eq!(h.get("x-source").unwrap(), "scryfall");
    assert_eq!(h.get("x-total-count").unwrap(), "1", "broad meta total rides through");
    let search_hits = ms.hits_for("GET /cards/search");
    assert!(search_hits.iter().any(|(k, _)| k.contains("lang%3Aja") && k.contains("include_multilingual=true")),
        "ja leg: {:?}", search_hits.iter().map(|(k, _)| k).collect::<Vec<_>>());
    assert!(search_hits.iter().all(|(k, _)| k.contains("lang%3Aja")),
        "no English leg for a non-English lang");
}

#[tokio::test]
async fn live_merge_first_word_fallback_queried() {
    // Multi-word name: broad empty → the first-word fallback query goes
    // out with the same suffix (Node's fallback leg).
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (mock, ms) = mock_scryfall::spawn_mock().await; // default search: data []
    let r = router_for_live(path, &["search"], &format!("http://{upstream}"), mock_scryfall::scry_for(mock, 0.0));

    let (s, _h, b) = get(&r, "/api/search?name=Zzadamant%20Will", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap().as_array().unwrap().len(), 0);
    let asked: Vec<String> = ms.hits_for("GET /cards/search").into_iter().map(|(k, _)| k).collect();
    assert!(asked.iter().any(|k| k.contains("q=Zzadamant%20Will%20unique%3Aprints")), "broad asked: {asked:?}");
    assert!(asked.iter().any(|k| k.contains("q=Zzadamant%20unique%3Aprints")), "fallback asked: {asked:?}");
}

#[tokio::test]
async fn live_merge_internet_upstream_dead_serves_local_rows() {
    // Node's merge catch under scope=internet still answers from the
    // already-resolved queryLocal rows when it has them — here upstream
    // is scripted dead, so the cached row must come back, not a proxy hop.
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO card_cache (id, name, language, last_updated) VALUES ('zzq1', 'Zzqx Oracle', 'English', '2026-01-01 00:00:00')",
            [],
        )
        .unwrap();
    }
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    ms.statuses
        .lock()
        .unwrap()
        .insert("GET /cards/search".into(), vec![500]);
    let r = router_for_live(path, &["search"], &format!("http://{upstream}"), mock_scryfall::scry_for(mock, 0.0));

    let (s, h, b) = get(&r, "/api/search?scope=internet&name=Zzqx%20Oracle", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node"), None, "catch answers from local rows, no proxy");
    let body: Value = serde_json::from_slice(&b).unwrap();
    let cards = body.as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], json!("zzq1"));
    assert_eq!(h.get("x-source").unwrap(), "cache");
}

#[tokio::test]
async fn live_merge_prices_from_cache_without_upstream() {
    // mergeSearchPrices twin, cached-quote branch: a sidecar-shaped raw
    // card (no prices) gets its quote copied from card_cache — zero
    // /cards/collection traffic.
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO card_cache (id, name, language, price_source, price_normal, last_updated)
             VALUES ('mtg-zprice1', 'Zprice Card', 'English', 'tcgplayer', 4.5, '2026-01-01 00:00:00')",
            [],
        )
        .unwrap();
    }
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    *ms.search.lock().unwrap() = json!({
        "data": [mock_scryfall::raw_card("zprice1", "Zprice Card", "zz1", "1", None)],
        "total_cards": 1, "has_more": false,
    });
    let r = router_for_live(path, &["search"], &format!("http://{upstream}"), mock_scryfall::scry_for(mock, 0.0));

    let (s, _h, b) = get(&r, "/api/search?scope=internet&name=Zprice%20Card", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    let body: Value = serde_json::from_slice(&b).unwrap();
    let cards = body.as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["price_normal"], json!(4.5), "quote merged from card_cache");
    assert_eq!(cards[0]["price_source"], json!("tcgplayer"));
    assert!(ms.hits_for("POST /cards/collection").is_empty(), "cached quote needs no bulk fetch");
}

#[tokio::test]
async fn live_merge_prices_never_checked_fetches_upstream() {
    // mergeSearchPrices twin, never-checked branch: a price-less card with
    // no card_cache row at all pins an upstream /cards/collection fetch
    // (upstream: true — TCGplayer quotes do not exist on the sidecar) and
    // the fresh quote rides the same response.
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (mock, ms) = mock_scryfall::spawn_mock().await;
    *ms.search.lock().unwrap() = json!({
        "data": [mock_scryfall::raw_card("zprice2", "Zprice Two", "zz1", "2", None)],
        "total_cards": 1, "has_more": false,
    });
    *ms.collection_batches.lock().unwrap() = vec![json!({
        "data": [mock_scryfall::raw_card("zprice2", "Zprice Two", "zz1", "2", Some(3.25))],
        "not_found": [],
    })];
    let r = router_for_live(path, &["search"], &format!("http://{upstream}"), mock_scryfall::scry_for(mock, 0.0));

    let (s, _h, b) = get(&r, "/api/search?scope=internet&name=Zprice%20Two", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    let body: Value = serde_json::from_slice(&b).unwrap();
    let cards = body.as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["price_normal"], json!(3.25), "fresh upstream quote merged in");
    let bulk = ms.hits_for("POST /cards/collection");
    assert_eq!(bulk.len(), 1, "never-checked card triggered the bulk fetch");
    let idents: Vec<String> = serde_json::from_str::<Value>(&bulk[0].1)
        .unwrap()["identifiers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| format!("{}:{}", v["set"].as_str().unwrap_or_default(), v["collector_number"].as_str().unwrap_or_default()))
        .collect();
    assert!(idents.contains(&"zz1:2".to_string()), "bulk asked by set+number: {idents:?}");
}


