//! Ported `GET /api/stats` tests.
//!
//! The expected bodies (expected_stats_user{1,2}.json) are the byte-exact
//! `JSON.stringify` output of the REAL Node handler logic over the same
//! stats_seed.sql fixture. They were produced by an ad-hoc Node harness
//! ("stats-oracle", not checked in) that extracted the SQL template literals
//! verbatim from backend/src/routes/stats.js — the same extraction
//! scripts/gen_stats_sql.py performs for the Rust constants — ran them over
//! node:sqlite, and called the real utils/priceHelpers.js (applyProxyRarity,
//! resolveCardPrice, isVintageSet) and utils/deckOwnership.js. The fixture
//! pins every timestamp far in the past or the future, so the rolling 7d/30d
//! windows classify identically forever; bench/parity.mjs additionally
//! re-validates these bodies against the live Node handler every iteration.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, collection::body_etag, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

const EXPECTED_USER1: &str = include_str!("expected_stats_user1.json");
const EXPECTED_USER2: &str = include_str!("expected_stats_user2.json");

fn seeded_stats_db(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    // The fixture's collection triggers reference collection_snapshot (a live
    // table in the real schema); stub it like the collection/search tests.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(include_str!("stats_seed.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO collection_read_revision (id, revision) VALUES (1, 1);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);
         INSERT OR IGNORE INTO stats_aux_revision (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    path
}

/// Fake Node: echoes so proxy-path assertions can see the hop happened.
async fn spawn_fake_node() -> SocketAddr {
    let echo = any(|req: Request| async move {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let mut out = HeaderMap::new();
        out.insert(HeaderName::from_static("x-node"), HeaderValue::from_static("1"));
        (
            StatusCode::OK,
            out,
            axum::Json(json!({
                "method": parts.method.as_str(),
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
    app(AppState::new(cfg))
}

async fn request(r: &Router, method: &str, path: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(path).body(Body::empty()).unwrap();
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
    vec![("authorization", "Bearer tok-stats-rich")]
}

async fn get_json(r: &Router, path: &str) -> (StatusCode, HeaderMap, Value) {
    let (s, h, b) = request(r, "GET", path, &auth()).await;
    (s, h, serde_json::from_slice(&b).unwrap())
}

// --- native path: byte parity against the Node oracle ---------------------

#[tokio::test]
async fn full_body_matches_node_oracle_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "parity.db"), &["stats"], &format!("http://{upstream}"));

    let (s, h, body) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
        "application/json; charset=utf-8"
    );
    assert!(h.get("x-node").is_none(), "must not hop to Node");
    assert!(h.get("x-source").is_none(), "/stats sets no X-Source");
    assert_eq!(h.get("x-stats-cache").unwrap().to_str().unwrap(), "miss");
    // The Express default ETag over the exact Node-produced bytes.
    assert_eq!(h.get(header::ETAG).unwrap().to_str().unwrap(), body_etag(EXPECTED_USER1.trim_end().as_bytes()));
    assert_eq!(
        String::from_utf8(body.clone()).unwrap(),
        EXPECTED_USER1.trim_end(),
        "response body must be byte-identical to the Node handler's JSON.stringify output"
    );
}

#[tokio::test]
async fn nested_key_orders_match_node() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "keys.db"), &["stats"], &format!("http://{upstream}"));
    let (s, _, value) = get_json(&r, "/api/stats").await;
    assert_eq!(s, StatusCode::OK);

    let top: Vec<&String> = value.as_object().unwrap().keys().collect();
    assert_eq!(
        top,
        vec![
            "summary", "types", "colorValues", "rarities", "sets", "topValuable", "recentAdditions", "setProgress", "decks"
        ]
    );
    let summary: Vec<&String> = value["summary"].as_object().unwrap().keys().collect();
    assert_eq!(
        summary,
        vec![
            "totalCards", "uniqueCards", "totalValue", "estimatedCards", "totalSpent", "roi", "avgCardValue",
            "duplicateCopies", "mintRate", "vintageRatio", "change7d", "change30d", "change1y", "change5y"
        ]
    );
    let roi: Vec<&String> = value["summary"]["roi"].as_object().unwrap().keys().collect();
    assert_eq!(roi, vec!["abs", "pct"]);
    let c7: Vec<&String> = value["summary"]["change7d"].as_object().unwrap().keys().collect();
    assert_eq!(c7, vec!["available", "abs", "pct"]);

    // applyProxyRarity + price_trend overwrite semantics: existing columns
    // keep their SELECT position, base_rarity appends last.
    let row: Vec<&String> = value["topValuable"][0].as_object().unwrap().keys().collect();
    let mut expect_row: Vec<&str> = vec![
        "entry_id", "quantity", "condition", "printing", "language", "purchase_price", "is_trade", "is_proxy",
        "card_id", "name", "printed_name", "rarity", "set_name", "set_id", "number", "image_url", "supertype",
        "subtypes", "types", "cmc", "color_identity", "price_trend", "price_normal", "price_holofoil",
        "price_estimated",
    ];
    expect_row.push("base_rarity");
    assert_eq!(row, expect_row.iter().collect::<Vec<_>>());
    // Proxy row: rarity flipped in place, base_rarity carries the original.
    let proxy = value["topValuable"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["is_proxy"] == json!(1))
        .expect("proxy row");
    assert_eq!(proxy["rarity"], json!("Proxy"));
    assert_eq!(proxy["base_rarity"], json!("Rare"));
    assert_eq!(proxy["price_trend"], json!(0), "proxy price resolves to 0");
    // resolveCardPrice picked the holofoil for the Holofoil printing.
    assert_eq!(value["topValuable"][0]["price_trend"], json!(12.34));

    let decks: Vec<&String> = value["decks"].as_object().unwrap().keys().collect();
    assert_eq!(decks, vec!["count", "built", "needCards"]);
}

#[tokio::test]
async fn empty_collection_shape_matches_node() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "empty.db"), &["stats"], &format!("http://{upstream}"));
    let (s, h, body) = request(&r, "GET", "/api/stats", &[("authorization", "Bearer tok-stats-empty")]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(String::from_utf8(body).unwrap(), EXPECTED_USER2.trim_end());
    assert!(h.get(header::ETAG).is_some());
    // Sanity-pin the zero/empty contract explicitly too (roi null pct etc.).
    let v: Value = serde_json::from_str(EXPECTED_USER2.trim_end()).unwrap();
    assert_eq!(v["summary"]["roi"], json!({"abs": 0, "pct": null}));
    assert_eq!(v["summary"]["change7d"], json!({"available": false, "abs": null, "pct": null}));
    assert_eq!(v["decks"], json!({"count": 0, "built": 0, "needCards": 0}));
    assert_eq!(v["types"], json!([]));
    assert_eq!(v["sets"], json!([]));
}

#[tokio::test]
async fn decks_and_set_progress_math() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "decks.db"), &["stats"], &format!("http://{upstream}"));
    let (_, _, value) = get_json(&r, "/api/stats").await;
    // built: checked_out 1; needCards: only the deck missing non-basic copies
    // (basic-lands-only deck exempt, NULL checked_out counts as not built,
    // user 3's deck is scoped out).
    assert_eq!(value["decks"], json!({"count": 3, "built": 1, "needCards": 1}));
    let sp = value["setProgress"].as_array().unwrap();
    assert_eq!(sp.len(), 3, "sets with size, top-4");
    assert_eq!(sp[0]["setId"], json!("lea"));
    assert_eq!(sp[0]["percent"], json!(1), "round(3/364*100) = 1");
    assert_eq!(sp[1]["setId"], json!("2ed"), "percent tie broken by setId ASC");
    assert_eq!(sp[2]["setId"], json!("m11"));
    // xyz has printed_total 0/total 0 → size NULL → excluded from setProgress
    // but present in the value-sorted sets list.
    let sets: Vec<&str> = value["sets"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(sets, vec!["lea", "2ed", "m11", "xyz"]);
    // Change windows flipped by the far-future price point (7.5 → 9.0).
    assert_eq!(value["summary"]["change7d"], json!({"available": true, "abs": 1.5, "pct": 20}));
    assert_eq!(value["summary"]["change30d"], json!({"available": true, "abs": 1.5, "pct": 20}));
    // toFixed(1) half-away-from-zero cases land exactly (58.823529... → 58.8).
    assert_eq!(value["summary"]["mintRate"], json!(58.8));
    assert_eq!(value["summary"]["vintageRatio"], json!(58.8));
    // Integral f64 must serialize without '.0' (Node JSON.stringify does not).
    let raw = String::from_utf8(
        request(&r, "GET", "/api/stats", &auth()).await.2,
    )
    .unwrap();
    assert!(raw.contains("\"count\":9"), "integer-valued floats render as integers: {raw}");
    assert!(!raw.contains("\"value\":32.180000000000"), "shortest round-trip floats");
}

// --- cache behaviour (statsCache.js mirror) --------------------------------

#[tokio::test]
async fn cache_miss_hit_and_304_replay() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "cache.db"), &["stats"], &format!("http://{upstream}"));

    let (s1, h1, b1) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(h1["x-stats-cache"], "miss");
    let (s2, h2, b2) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(h2["x-stats-cache"], "hit");
    assert_eq!(b1, b2, "cached replay is byte-stable");
    assert_eq!(h1[header::ETAG], h2[header::ETAG]);

    let etag = h2[header::ETAG].to_str().unwrap().to_string();
    let (s3, h3, b3) = request(&r, "GET", "/api/stats", &[("authorization", "Bearer tok-stats-rich"), ("if-none-match", etag.as_str())]).await;
    assert_eq!(s3, StatusCode::NOT_MODIFIED);
    assert!(b3.is_empty(), "304 carries no body");
    assert_eq!(h3[header::ETAG], etag.as_str());
    assert_eq!(h3["x-stats-cache"], "hit", "Node sets X-Stats-Cache before the freshness check");

    // '*' also matches (Node's etag_matches).
    let (s4, _, _) = request(&r, "GET", "/api/stats", &[("authorization", "Bearer tok-stats-rich"), ("if-none-match", "*")]).await;
    assert_eq!(s4, StatusCode::NOT_MODIFIED);

    // A different user has their own cache slot but the same data for
    // user-independent shapes — user 2's body differs, proving per-user keys.
    let (s5, h5, _) = request(&r, "GET", "/api/stats", &[("authorization", "Bearer tok-stats-empty")]).await;
    assert_eq!(s5, StatusCode::OK);
    assert_eq!(h5["x-stats-cache"], "miss");
}

#[tokio::test]
async fn revision_bumps_invalidate_all_three_counters() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = seeded_stats_db(dir.path(), "rev.db");
    let upstream = spawn_fake_node().await;
    let r = router_for(db_path.clone(), &["stats"], &format!("http://{upstream}"));

    let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(h["x-stats-cache"], "miss");
    let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(h["x-stats-cache"], "hit");

    for table in ["collection_read_revision", "deck_list_revision", "stats_aux_revision"] {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(&format!("UPDATE {table} SET revision = revision + 1 WHERE id = 1;")).unwrap();
        drop(conn);
        let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
        assert_eq!(h["x-stats-cache"], "miss", "{table} bump must invalidate");
        let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
        assert_eq!(h["x-stats-cache"], "hit", "and the fresh body re-caches");
    }
}

#[tokio::test]
async fn price_history_write_bumps_aux_trigger_and_freshens_body() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = seeded_stats_db(dir.path(), "aux.db");
    let upstream = spawn_fake_node().await;
    let r = router_for(db_path.clone(), &["stats"], &format!("http://{upstream}"));

    let (_, _, old) = get_json(&r, "/api/stats").await;
    assert_eq!(old["summary"]["change7d"]["abs"], json!(1.5));

    // Node-side write simulation: a NEW price point for an owned card. This
    // rides through the fixture's stats_aux trigger (db.js does the same on
    // the live DB), so the cached body must be dropped AND the body must
    // reflect the new history (change7d abs re-computed).
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "INSERT INTO price_history (card_id, price, recorded_at) VALUES ('c-sol', 4.0, '2099-06-01 00:00:00');",
    )
    .unwrap();
    let aux: i64 = conn
        .query_row("SELECT revision FROM stats_aux_revision WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert!(aux >= 1, "trigger must have bumped stats_aux_revision");
    drop(conn);

    let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(h["x-stats-cache"], "miss", "trigger-bumped revision invalidates");
    let (_, _, fresh) = get_json(&r, "/api/stats").await;
    assert_ne!(fresh, old, "body must reflect the new price point");
    assert_eq!(fresh["summary"]["change7d"]["abs"], json!(4.5), "9.0→12.0 on qty 3");
}

#[tokio::test]
async fn proxied_write_bumps_user_generation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = seeded_stats_db(dir.path(), "gen.db");
    let upstream = spawn_fake_node().await;
    let r = router_for(db_path.clone(), &["stats"], &format!("http://{upstream}"));

    let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(h["x-stats-cache"], "miss");
    let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(h["x-stats-cache"], "hit");

    // Authenticated non-GET /api write, proxied → invalidateOnWrite twin.
    let (s, _, _) = request(&r, "POST", "/api/collection", &auth()).await;
    assert_eq!(s, StatusCode::OK, "write rides the proxy to the echo upstream");
    // The bump is fire-and-forget; give the spawned task a moment.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(h["x-stats-cache"], "miss", "proxied write must invalidate the user's cache");

    // HEAD and unauthenticated writes must NOT bump (Node's middleware skips
    // HEAD and never reaches a user for unauth).
    let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(h["x-stats-cache"], "hit");
    let _ = request(&r, "POST", "/api/collection", &[("authorization", "Bearer bogus")]).await;
    let _ = request(&r, "HEAD", "/api/collection", &auth()).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, h, _) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(h["x-stats-cache"], "hit", "unauth/HEAD writes never bump");
}

// --- encoding / ETag --------------------------------------------------------

#[tokio::test]
async fn gzip_served_when_accepted_and_decompressed_matches() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "gz.db"), &["stats"], &format!("http://{upstream}"));
    let (s, h, body) =
        request(&r, "GET", "/api/stats", &[("authorization", "Bearer tok-stats-rich"), ("accept-encoding", "gzip")])
            .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_ENCODING], "gzip");
    assert_eq!(h[header::VARY], "Accept-Encoding");
    // inflate and compare to the oracle bytes.
    let mut d = flate2::read::GzDecoder::new(&body[..]);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut d, &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), EXPECTED_USER1.trim_end());
}

// --- proxy paths (anything Node answers natively that we do not) ------------

#[tokio::test]
async fn kill_switch_off_proxies_everything() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "off.db"), &["collection", "search"], &format!("http://{upstream}"));
    let (s, h, body) = request(&r, "GET", "/api/stats", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-node"], "1", "kill switch off → Node");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["pq"], json!("/api/stats"));
    assert!(h.get("x-stats-cache").is_none());
}

#[tokio::test]
async fn post_and_head_proxies_even_authed() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "post.db"), &["stats"], &format!("http://{upstream}"));

    let (s, h, body) = request(&r, "POST", "/api/stats", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-node"], "1", "POST is Node's business");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["method"], "POST");
    assert_eq!(v["pq"], "/api/stats");

    let (s, h, _) = request(&r, "HEAD", "/api/stats", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-node"], "1", "HEAD goes to Node (Express answers it natively)");
}

#[tokio::test]
async fn trailing_slash_and_subroutes_and_query_paths() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "paths.db"), &["stats"], &format!("http://{upstream}"));

    // Query string is ignored by Node's route → Rust answers natively.
    let (s, h, body) = request(&r, "GET", "/api/stats?ignored=1&foo=bar", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());
    assert_eq!(String::from_utf8(body).unwrap(), EXPECTED_USER1.trim_end());

    // Trailing slash: Express matches it, but the Rust gate is exact — so it
    // proxies and Node's answer comes back untouched (byte-perfect by
    // construction against the same handler).
    let (s, h, body) = request(&r, "GET", "/api/stats/", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-node"], "1");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["pq"], "/api/stats/");

    for p in ["/api/stats/history?period=30d", "/api/stats/networth", "/api/stats/top-mover", "/api/stats/commander-of-the-day"] {
        let (s, h, _) = request(&r, "GET", p, &auth()).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        assert_eq!(h["x-node"], "1", "{p} must proxy (not in scope)");
    }
}

// --- auth contract (same bodies as auth.rs / Node middleware) ---------------

#[tokio::test]
async fn auth_failures_match_node_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_stats_db(dir.path(), "auth.db"), &["stats"], &format!("http://{upstream}"));

    let (s, h, body) = request(&r, "GET", "/api/stats", &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(h[header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({"error": "Access denied. No token provided."}));

    let (s, _, body) = request(&r, "GET", "/api/stats", &[("authorization", "Bearer bogus-token")]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({"error": "Invalid or expired session token."}));

    // API keys are GET-only: GET works natively...
    let (s, h, _) = request(&r, "GET", "/api/stats", &[("authorization", "Bearer tok-api-stats")]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());
    // ...POST with an API key goes to Node, whose middleware returns the
    // 403; in this harness the echo upstream stands in for it (the parity
    // bench checks the live body).
    let (s, h, v) = request(&r, "POST", "/api/stats", &[("authorization", "Bearer tok-api-stats")]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-node"], "1", "API-key POST proxied to Node (403 comes from Node)");
    let _: Value = serde_json::from_slice(&v).unwrap();
}

#[tokio::test]
async fn concurrent_misses_coalesce_into_one_computation() {
    // The write-mix moment: a burst of concurrent requests hits a cold cache
    // (right after an invalidation). Without single-flight every one of them
    // runs the full build_body (Node does exactly this); with it, exactly
    // ONE computation serves the whole burst. computes()==1 is the contract.
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_stats_db(dir.path(), "coalesce.db");
    let upstream = spawn_fake_node().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{upstream}"),
        static_dir: None,
        rust_routes: ["stats".to_string()].into_iter().collect(),
        db_path: Some(db),
        sidecar_url: None,
    };
    let state = AppState::new(cfg);
    let cache = std::sync::Arc::clone(&state.stats_cache);
    let r = app(state);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let r = r.clone();
        handles.push(tokio::spawn(async move {
            let req = Request::builder()
                .method("GET")
                .uri("/api/stats")
                .header("authorization", "Bearer tok-stats-rich")
                .body(Body::empty())
                .unwrap();
            let res = r.oneshot(req).await.unwrap();
            let (parts, body) = res.into_parts();
            (parts.status, axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec())
        }));
    }
    let mut hits = Vec::new();
    for h in handles {
        hits.push(h.await.unwrap());
    }
    for (s, b) in &hits {
        assert_eq!(*s, StatusCode::OK);
        assert_eq!(b, &hits[0].1, "every waiter in the burst gets the same body");
    }
    assert_eq!(cache.computes(), 1, "burst of 8 cold requests must compute the body exactly once");
}
