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
    body::Body,
    extract::Request,
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
    };
    app(AppState::new(cfg))
}

async fn get(r: &Router, path: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method("GET").uri(path).body(Body::empty()).unwrap();
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
async fn zero_rows_proxies_to_node() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    // Node would fall through to the live Scryfall/sidecar path — so must Rust.
    let (s, h, body) = get(&r, "/api/search?name=NoCardNamedThis", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1");
    let echo: Value = serde_json::from_slice(&body).unwrap();
    assert!(echo["pq"].as_str().unwrap().starts_with("/api/search?"));
    assert_eq!(h.get("x-source").unwrap().to_str().unwrap(), "live");
}

#[tokio::test]
async fn gated_queries_proxy_to_node() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    for q in [
        "/api/search?q=t%3Acreature%20c%3Ag",          // raw query → parser/sidecar
        "/api/search?scope=collection&q=sol%20ring",   // collection catalog path
        "/api/search?scope=collection&name=Sol%20Ring", // collection userFilter path
        "/api/search?scope=internet&name=Sol%20Ring",  // internet scope is authoritative
        "/api/search?name=Sol%20Ring&prints=1",        // allPrints walk
        "/api/search?name=Sol%20Ring&image=abc",       // cvScan ranking
        "/api/search?name=a&name=b",                   // qs array shape
        "/api/search?scope=database&name=Sol%20Ring&page=1&page=2", // repeated page
    ] {
        let (s, h, _) = get(&r, q, &auth()).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        assert_eq!(h.get("x-node").unwrap().to_str().unwrap(), "1", "{q} must proxy");
    }
}

#[tokio::test]
async fn post_proxies_even_when_owned() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path()), &["search"], &format!("http://{upstream}"));

    let req = Request::builder()
        .method("POST")
        .uri("/api/search")
        .header("authorization", "Bearer tok-session")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"Sol Ring"}"#))
        .unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(parts.headers.get("x-node").unwrap().to_str().unwrap(), "1");
    let echo: Value = serde_json::from_slice(&axum::body::to_bytes(body, usize::MAX).await.unwrap()).unwrap();
    assert_eq!(echo["body"].as_str().unwrap(), r#"{"name":"Sol Ring"}"#);
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
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
    };
    let mut st = AppState::new(cfg);
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
    let r = router_for_idx(path.clone(), &["search"], &upstream, NameIndex::new());

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
