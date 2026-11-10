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
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url,
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
    let r = sidecar_router(path, &upstream, &sidecar);

    let (s, h, _b) = get(&r, "/api/search?scope=database&name=boom%20card&page=1&limit=20", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1", "must have gone through Node");
}

/// With search_sidecar off the kill switch, zero-row requests still proxy.
#[tokio::test]
async fn sidecar_kill_switch_proxies() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let upstream = format!("http://{}", spawn_fake_node().await);
    let (sidecar, seen) = spawn_fake_sidecar().await;
    let r = router_for_idx_owned_sidecar(
        path,
        &["search"], // no search_sidecar
        &upstream,
        NameIndex::new(),
        OwnedCache::new(),
        Some(sidecar.clone()),
    );
    let (s, h, _b) = get(&r, "/api/search?scope=database&name=Kodama%20of%20the%20West%20Tree&page=1&limit=20", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
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
    AppState::new(cfg)
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
                .header("authorization", "Bearer tok-session")
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
    let st = AppState::new(cfg);
    let cache = Arc::clone(&st.search_cache);
    let r = app(st);

    let hits = burst(&r, "/api/search?scope=database&name=Sol%20Ring").await;
    for (s, _, b) in &hits {
        assert_eq!(*s, StatusCode::OK);
        assert_eq!(b, &hits[0].2, "every waiter in the burst gets the same body");
    }
    assert_eq!(cache.computes(), 1, "burst of 8 cold requests must compute the body exactly once");
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
    let r = router_for(path.clone(), &["search"], &format!("http://{upstream}"));
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

#[tokio::test]
async fn collq_structured_queries_and_params_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    seed_cqc(path.as_path(), "sol ring", "en", r#"["sol ring"]"#, 4, 1, now_ms());
    let upstream = spawn_fake_node().await;
    let r = router_for(path.clone(), &["search", "search_sidecar"], &format!("http://{upstream}"));
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
        "/api/search?scope=collection&q=sol%20ring&snapshot=x",          // expectedSnapshot contract
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
        "is:land",
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
    // proxies instead of walking.
    let r2 = router_for(path.clone(), &["search"], &format!("http://{upstream}"));
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
