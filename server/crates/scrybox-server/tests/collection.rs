//! Ported `GET /api/collection` tests. These encode the Node parity contract
//! (status, exact bodies, key order, ETag/304, X-Total-Count, auth errors)
//! against a temp DB built from the prod schema fixture.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, StatusCode},
    routing::any,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "tok-session";
const API_KEY: &str = "tok-api";

/// The 32 selected columns in Node's order, plus the appended keys.
fn base_keys() -> Vec<&'static str> {
    vec![
        "entry_id", "card_id", "quantity", "condition", "printing", "language", "purchase_price",
        "added_at", "is_trade", "notes", "is_proxy", "name", "printed_name", "supertype", "subtypes",
        "types", "cmc", "color_identity", "rarity", "set_id", "set_name", "number", "image_url",
        "price_trend", "price_normal", "price_holofoil", "price_currency", "price_source",
        "price_estimated", "tcgplayer_url", "cardmarket_url", "tcgplayer_product_id", "base_rarity",
    ]
}

fn seeded_db(dir: &std::path::Path) -> PathBuf {
    seeded_db_named(dir, "bindarr-test.db")
}

fn seeded_db_named(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    // Trigger bodies in the prod schema also touch tables this port does not
    // model; stub them so the revision triggers stay live and writes do not
    // throw.
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
          VALUES ('tok-session', 1, '2099-01-01 00:00:00'),
                 ('tok-expired', 1, '2000-01-01 00:00:00');
        INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number,
                                price_trend, price_normal, price_holofoil, cmc, color_identity, tcgplayer_product_id)
          VALUES ('cc1', 'Sol Ring', 'Card', '[]', '[\"Artifact\"]', 'Common', '3ed', 'The Dark', '1',
                  2.5, 3.0, 4.5, 1, '[]', 123),
                 ('cc2', 'Plains', 'Basic', '[\"Land\"]', '[\"Land\"]', 'Common', '3ed', 'The Dark', '2',
                  NULL, NULL, NULL, 0, NULL, NULL),
                 ('cc3', 'Proxy Card', 'Card', '[\"Human\"]', '[\"Creature\"]', 'Uncommon', '3ed', 'The Dark', '3',
                  9.0, 0, 0, 2, '[\"W\"]', NULL),
                 ('cc4', 'Holo Only', 'Card', '', '[]', 'Rare', '3ed', 'The Dark', '4',
                  NULL, NULL, 7.5, 3, '[]', NULL),
                 ('cc5', 'Tundra', 'Card', '[\"Plains\",\"Island\"]', '[\"Land\"]', 'Rare', '3ed', 'The Dark', '5',
                  1.25, 1.25, NULL, 0, '[\"W\",\"U\"]', NULL);
        INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price,
                                added_at, notes, is_trade, user_id, is_proxy)
          VALUES (1, 'cc1', 2, 'Near Mint', 'Normal', 'English', 0.0, '2024-01-05 00:00:00', '', 0, 1, 0),
                 (2, 'cc2', 4, 'Near Mint', 'Normal', 'English', NULL, '2024-01-04 00:00:00', '', 0, 1, 0),
                 (3, 'cc3', 1, 'Near Mint', 'Normal', 'English', NULL, '2024-01-03 00:00:00', '', 0, 1, 1),
                 (4, 'cc4', 1, 'Near Mint', 'Holofoil', 'English', NULL, '2024-01-02 00:00:00', '', 0, 1, 0),
                 (5, 'cc5', 1, 'Near Mint', 'Normal', 'English', NULL, '2024-01-01 00:00:00', '', 1, 1, 0),
                 (6, 'cc1', 1, 'Near Mint', 'Normal', 'English', NULL, '2024-01-06 00:00:00', '', 0, 2, 0);
        INSERT INTO collection_read_revision (id, revision) VALUES (1, 1);
        INSERT INTO decks (id, user_id, name, checked_out) VALUES (1, 1, 'd1', 1);
        INSERT INTO deck_cards (deck_id, card_id, quantity)
          VALUES (1, 'cc1', 2), (1, 'cc2', 99), (1, 'cc5', 1);
        ",
    )
    .unwrap();
    path
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

/// A stand-in Node that just answers "NODE" — used to prove the proxy fallback.
async fn fake_node() -> String {
    let r = Router::new().fallback(any(|| async { (StatusCode::OK, "NODE") }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    format!("http://{addr}")
}

// --- auth parity -------------------------------------------------------------

#[tokio::test]
async fn no_token_401_exact_body() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (s, h, b) = get(&r, "/api/collection", &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap(),
        json!({ "error": "Access denied. No token provided." })
    );
    assert!(h[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/json"));
}

#[tokio::test]
async fn expired_and_unknown_token_401() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    for tok in ["tok-expired", "nope"] {
        let (s, _, b) = get(&r, "/api/collection", &[("authorization", &format!("Bearer {tok}"))]).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{tok}");
        assert_eq!(
            serde_json::from_slice::<Value>(&b).unwrap(),
            json!({ "error": "Invalid or expired session token." })
        );
    }
}

#[tokio::test]
async fn api_key_get_is_authorized() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (s, _, _) = get(&r, "/api/collection?limit=10", &[("authorization", "Bearer tok-api")]).await;
    assert_eq!(s, StatusCode::OK);
}

// --- response shape parity ----------------------------------------------------

#[tokio::test]
async fn paginated_shape_headers_and_key_order() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (s, h, v) = get_json(&r, "/api/collection?limit=100&offset=0").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-total-count"], "5"); // user 1 only
    assert_eq!(h["access-control-expose-headers"], "X-Total-Count");
    let etag = h[header::ETAG].to_str().unwrap().to_string();
    assert!(etag.starts_with("W/\"col-") && etag.ends_with('"'), "etag: {etag}");
    assert_eq!(h[header::CACHE_CONTROL], "private, no-cache");
    assert_eq!(h[header::CONTENT_TYPE], "application/json; charset=utf-8");

    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 5);
    // Newest first by added_at DESC, id DESC.
    let ids: Vec<i64> = rows.iter().map(|r| r["entry_id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    // Key order: 32 SELECT keys then appended base_rarity; no checked_out_qty.
    let keys: Vec<&str> = rows[0].as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, base_keys());
}

#[tokio::test]
async fn row_semantics_proxy_rarity_prices_and_arrays() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (_, _, v) = get_json(&r, "/api/collection?limit=100").await;
    let rows = v.as_array().unwrap();
    let by_id = |id: i64| rows.iter().find(|r| r["entry_id"] == json!(id)).unwrap().clone();

    let e1 = by_id(1); // Normal printing, price_normal 3.0 > 0
    assert_eq!(e1["price_trend"], json!(3));
    assert_eq!(e1["rarity"], json!("Common"));
    assert_eq!(e1["base_rarity"], json!("Common"));
    assert_eq!(e1["is_proxy"], json!(0));
    assert_eq!(e1["types"], json!(["Artifact"]));
    assert_eq!(e1["subtypes"], json!([]));
    assert_eq!(e1["color_identity"], json!([]));
    assert_eq!(e1["name"], json!("Sol Ring"));
    assert_eq!(e1["quantity"], json!(2));

    let e2 = by_id(2); // NULL color_identity -> []
    assert_eq!(e2["color_identity"], json!([]));
    assert_eq!(e2["types"], json!(["Land"]));

    let e3 = by_id(3); // is_proxy row: proxy price/rarity rules
    assert_eq!(e3["price_trend"], json!(0));
    assert_eq!(e3["rarity"], json!("Proxy"));
    assert_eq!(e3["base_rarity"], json!("Uncommon"));
    assert_eq!(e3["is_proxy"], json!(1));

    let e4 = by_id(4); // Holofoil with only price_holofoil set
    assert_eq!(e4["price_trend"], json!(7.5));
    assert_eq!(e4["subtypes"], json!([])); // stored '' -> []

    let e5 = by_id(5); // price_trend 1.25 pass-through (Normal, price_normal 1.25)
    assert_eq!(e5["price_trend"], json!(1.25));
}

#[tokio::test]
async fn full_response_appends_checked_out_qty() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (s, h, v) = get_json(&r, "/api/collection").await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-total-count").is_none(), "no total header when unpaginated");
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 5);
    for row in rows {
        let mut keys: Vec<&str> = row.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        let last = keys.pop().unwrap();
        assert_eq!(last, "checked_out_qty");
        keys.pop().unwrap();
        assert_eq!(keys, base_keys()[..32]);
    }
    let qty = |id: i64| rows.iter().find(|r| r["entry_id"] == json!(id)).unwrap()["checked_out_qty"].clone();
    assert_eq!(qty(1), json!(2)); // greedy: deck needs 2x cc1, newest entry takes all
    assert_eq!(qty(5), json!(1)); // dual land is not basic -> allocated
    assert_eq!(qty(2), json!(0)); // Plains excluded via basic-land rule
    assert_eq!(qty(3), json!(0));
    assert_eq!(qty(4), json!(0));
}

#[tokio::test]
async fn include_allocation_on_paginated() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (s, h, v) = get_json(&r, "/api/collection?include_allocation=1&limit=10").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-total-count"], "5");
    // Node skips the *route* ETag branch exactly when include_allocation === '1'
    // (routes/collection.js:389), but Express's res.json() still stamps its
    // default body tag — so the tag here must be the body form, not `col-`.
    let etag = h[header::ETAG].to_str().unwrap().to_string();
    assert!(!etag.starts_with("W/\"col-"), "alloc read must not carry the route tag: {etag}");
    assert!(etag.starts_with("W/\"") && etag.len() > 4, "expected Express body tag: {etag}");
    assert!(h.get(header::CACHE_CONTROL).is_none());
    let rows = v.as_array().unwrap();
    assert_eq!(rows[0]["checked_out_qty"], json!(2));
}

#[tokio::test]
async fn count_zero_suppresses_total_and_pagination_clamps() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (s, h, v) = get_json(&r, "/api/collection?count=0&limit=10").await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-total-count").is_none());
    assert_eq!(v.as_array().unwrap().len(), 5);

    // limit=-5 -> Math.max(1, -5) = 1; limit=99999 -> 2000.
    let (_, _, v) = get_json(&r, "/api/collection?limit=-5").await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    let (_, _, v) = get_json(&r, "/api/collection?limit=99999").await;
    assert_eq!(v.as_array().unwrap().len(), 5);
    // limit=0 / limit=abc -> 100.
    let (_, _, v) = get_json(&r, "/api/collection?limit=abc").await;
    assert_eq!(v.as_array().unwrap().len(), 5);
    // offset beyond the end -> empty; negative -> 0.
    let (_, _, v) = get_json(&r, "/api/collection?limit=10&offset=9").await;
    assert_eq!(v.as_array().unwrap().len(), 0);
    let (_, _, v) = get_json(&r, "/api/collection?limit=10&offset=-2").await;
    assert_eq!(v.as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn is_trade_and_card_id_filters() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (s, h, v) = get_json(&r, "/api/collection?is_trade=true&limit=10").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-total-count"], "1");
    assert_eq!(v.as_array().unwrap()[0]["entry_id"], json!(5));

    let (_, h, v) = get_json(&r, "/api/collection?is_trade=0&limit=10").await;
    assert_eq!(h["x-total-count"], "4");
    assert_eq!(v.as_array().unwrap().len(), 4);

    // Non-'true'/'1' values coerce to 0.
    let (_, h, _) = get_json(&r, "/api/collection?is_trade=maybe&limit=10").await;
    assert_eq!(h["x-total-count"], "4");

    let (_, h, v) = get_json(&r, "/api/collection?card_id=cc1&limit=10").await;
    assert_eq!(h["x-total-count"], "1");
    assert_eq!(v.as_array().unwrap()[0]["entry_id"], json!(1));

    // card_id='' is falsy in Node -> no filter.
    let (_, h, _) = get_json(&r, "/api/collection?card_id=&limit=10").await;
    assert_eq!(h["x-total-count"], "5");
}

// --- ETag / cache invalidation -------------------------------------------------

#[tokio::test]
async fn etag_304_and_revision_invalidates_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let r = router_for(path.clone(), &["collection"], "http://127.0.0.1:1");

    let (s, h, _) = get_json(&r, "/api/collection?limit=2&offset=0").await;
    assert_eq!(s, StatusCode::OK);
    let etag = h[header::ETAG].to_str().unwrap().to_string();

    // Replay -> 304 with empty body.
    let (s, _, b) = get(&r, "/api/collection?limit=2&offset=0", &[("authorization", &format!("Bearer {TOKEN}")), ("if-none-match", &etag)]).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    assert!(b.is_empty());

    // Node-side write (via trigger-bumped revision) must invalidate both the
    // ETag and the cached bytes.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE collection SET notes = 'sold copy' WHERE id = 1", []).unwrap();
    }
    let (s, h2, v) = get_json(&r, "/api/collection?limit=2&offset=0").await;
    assert_eq!(s, StatusCode::OK);
    let etag2 = h2[header::ETAG].to_str().unwrap().to_string();
    assert_ne!(etag, etag2, "etag must follow the revision counter");
    assert_eq!(v.as_array().unwrap()[0]["notes"], json!("sold copy"));

    // Old tag no longer matches.
    let (s, _, _) = get(&r, "/api/collection?limit=2&offset=0", &[("authorization", &format!("Bearer {TOKEN}")), ("if-none-match", &etag)]).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn card_cache_price_update_invalidates_full_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let r = router_for(path.clone(), &["collection"], "http://127.0.0.1:1");

    let (_, _, v) = get_json(&r, "/api/collection").await;
    let rows = v.as_array().unwrap();
    assert_eq!(rows[0]["price_trend"], json!(3));

    {
        let conn = Connection::open(&path).unwrap();
        // Holofoil price movement through card_cache (trigger bumps revision).
        conn.execute("UPDATE card_cache SET price_holofoil = 10.0 WHERE id = 'cc1'", []).unwrap();
        conn.execute("UPDATE collection SET printing = 'Holofoil' WHERE id = 1", []).unwrap();
    }
    let (_, _, v) = get_json(&r, "/api/collection").await;
    let rows = v.as_array().unwrap();
    assert_eq!(rows[0]["price_trend"], json!(10));
}

// --- review 2026-11-13 C1: allocation shapes keyed on deck_list_revision ------

/// The full and include_allocation=1 shapes embed checked_out_qty, which is
/// computed from decks/deck_cards — tables whose triggers bump
/// deck_list_revision, NOT collection_read_revision. The cache must key
/// those shapes on BOTH counters or a deck edit serves stale allocations
/// forever (Node computes them fresh every request).
#[tokio::test]
async fn deck_edit_invalidates_checked_out_qty_in_alloc_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    let r = router_for(path.clone(), &["collection"], "http://127.0.0.1:1");

    let alloc_paths = ["/api/collection", "/api/collection?include_allocation=1&limit=10"];

    // Prime both allocation caches: deck d1 (user 1) is checked out with
    // 2x cc1, so entry_id 1 carries checked_out_qty = 2.
    for path in alloc_paths {
        let (s, _, v) = get_json(&r, path).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        let rows = v.as_array().cloned().unwrap_or_else(|| v["data"].as_array().unwrap().clone());
        let e1 = rows.iter().find(|e| e["entry_id"] == json!(1)).unwrap();
        assert_eq!(e1["checked_out_qty"], json!(2), "seed state: {path}");
    }

    // Deck-side edit: check the deck back in. The decks UPDATE trigger bumps
    // deck_list_revision ONLY — collection_read_revision must not move, or
    // this test would not isolate the C1 failure mode. (Counters are read
    // relative: the seed's own INSERTs already fired the triggers.)
    let (dl_before, cr_before) = {
        let conn = Connection::open(&path).unwrap();
        (
            conn.query_row("SELECT revision FROM deck_list_revision WHERE id = 1", [], |r| r.get::<_, i64>(0)).unwrap(),
            conn.query_row("SELECT revision FROM collection_read_revision WHERE id = 1", [], |r| r.get::<_, i64>(0)).unwrap(),
        )
    };
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE decks SET checked_out = 0 WHERE id = 1", []).unwrap();
        let dl: i64 = conn
            .query_row("SELECT revision FROM deck_list_revision WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        let cr: i64 = conn
            .query_row("SELECT revision FROM collection_read_revision WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dl, dl_before + 1, "deck trigger bumped deck_list_revision");
        assert_eq!(cr, cr_before, "collection counter must be untouched for this test");
    }

    for path in alloc_paths {
        let (s, _, v) = get_json(&r, path).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        let rows = v.as_array().cloned().unwrap_or_else(|| v["data"].as_array().unwrap().clone());
        let e1 = rows.iter().find(|e| e["entry_id"] == json!(1)).unwrap();
        assert_eq!(
            e1["checked_out_qty"],
            json!(0),
            "C1: cached allocation body survived a deck edit on {path}"
        );
    }
}

// --- compression ---------------------------------------------------------------

#[tokio::test]
async fn gzip_served_when_accepted_and_above_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_db(dir.path());
    // Push one row's notes past the 1 KB compression threshold.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE collection SET notes = ?1 WHERE id = 1", [ "z".repeat(1500) ]).unwrap();
    }
    let r = router_for(path, &["collection"], "http://127.0.0.1:1");

    let (s, h, gz) = get(&r, "/api/collection?limit=100", &[("authorization", &format!("Bearer {TOKEN}")), ("accept-encoding", "gzip, deflate, br")]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_ENCODING], "gzip");
    let mut dec = flate2::read::GzDecoder::new(&gz[..]);
    let mut plain = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut plain).unwrap();

    let (_, _, raw) = get(&r, "/api/collection?limit=100", &auth()).await;
    assert_eq!(plain, raw);

    // Small bodies stay identity even when gzip is offered.
    let (_, h, _) = get(&r, "/api/collection?limit=1&offset=1", &[("authorization", &format!("Bearer {TOKEN}")), ("accept-encoding", "gzip")]).await;
    assert!(h.get(header::CONTENT_ENCODING).is_none());
}

// --- Express-default body ETag (untagged paths) --------------------------------

/// Vectors from the `etag` npm package that Express's `res.json()` uses
/// (`base64(sha1(body))` truncated to 27 chars, byte length in hex), verified
/// against openssl and the copy installed in backend/node_modules/etag.
#[test]
fn body_etag_matches_the_etag_package() {
    assert_eq!(scrybox_server::collection::body_etag(b""), "W/\"0-2jmj7l5rSw0yVb/vlWAYkK/YBwk\"");
    assert_eq!(scrybox_server::collection::body_etag(b"abc"), "W/\"3-qZk+NkcGgWq6PiVxeFDCbJzQ2J0\"");
    assert_eq!(scrybox_server::collection::body_etag(b"hello"), "W/\"5-qvTGHdzF6KLavt4PO0gs2a6pQ00\"");
    // Multi-byte characters hash as UTF-8 bytes (2 bytes for "é"), matching
    // Buffer.byteLength(entity, 'utf8').
    assert_eq!(scrybox_server::collection::body_etag("é".as_bytes()), "W/\"2-vxW+cXrBsIC08cRWaSgliR/1Bz0\"");
}

/// Unpaginated and allocation-carrying reads get no route tag, so they must
/// fall back to the body tag and answer a conditional GET with an empty 304.
#[tokio::test]
async fn untagged_paths_get_body_etag_and_answer_304() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    for path in ["/api/collection", "/api/collection?include_allocation=1&limit=50"] {
        let (s, h, body) = get(&r, path, &auth()).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert!(h.get(header::CACHE_CONTROL).is_none(), "{path}: body tag carries no Cache-Control");
        let etag = h[header::ETAG].to_str().unwrap().to_string();
        assert_eq!(etag, scrybox_server::collection::body_etag(&body), "{path}");

        let (s, h2, b2) = get(&r, path, &[("authorization", "Bearer tok-session"), ("if-none-match", &etag)]).await;
        assert_eq!(s, StatusCode::NOT_MODIFIED, "{path}");
        assert!(b2.is_empty(), "{path}: 304 carried a body");
        assert_eq!(h2[header::ETAG].to_str().unwrap(), etag);

        // A stale tag must not short-circuit the read.
        let (s, _, b3) = get(&r, path, &[("authorization", "Bearer tok-session"), ("if-none-match", "\"dead-beef\"")]).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert_eq!(b3, body);
    }
}

/// The route's revision tag wins where Node emits it; the body tag must not
/// shadow it (a body tag would force a full read before any 304).
#[tokio::test]
async fn route_etag_is_not_shadowed_by_body_etag() {
    let dir = tempfile::tempdir().unwrap();
    let r = router_for(seeded_db(dir.path()), &["collection"], "http://127.0.0.1:1");
    let (s, h, body) = get(&r, "/api/collection?limit=100&offset=0", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    let etag = h[header::ETAG].to_str().unwrap().to_string();
    assert!(etag.starts_with("W/\"col-"), "etag: {etag}");
    assert_ne!(etag, scrybox_server::collection::body_etag(&body));
    assert_eq!(h[header::CACHE_CONTROL], "private, no-cache");
}

// --- kill switch ---------------------------------------------------------------

#[tokio::test]
async fn unowned_routes_and_non_get_still_proxy_to_node() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = fake_node().await;
    // Route disabled -> everything on the path proxies.
    let r = router_for(seeded_db(dir.path()), &[], &upstream);
    let (s, _, b) = get(&r, "/api/collection", &auth()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(&b, b"NODE");

    // Route enabled but method POST -> still Node's.
    let r = router_for(seeded_db_named(dir.path(), "bindarr-post.db"), &["collection"], &upstream);
    let req = Request::builder().method("POST").uri("/api/collection")
        .header("authorization", "Bearer tok-session")
        .body(Body::empty()).unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&b[..], b"NODE");
}

// ---------------------------------------------------------------------------
// Iter 5: parallel build fast path — oracle byte-identity, lazy gzip,
// single-flight.
// ---------------------------------------------------------------------------

/// Bulk variant of `seeded_db_named`: enough rows (and enough edge-case
/// variety — unicode names, quotes, nulls, empty-string JSON columns,
/// proxies, holofoil printings, float prices) to push the parallel shaper
/// past its 4000-row threshold and cover every shaping branch.
fn seeded_bulk_db(dir: &std::path::Path, name: &str, rows: usize) -> PathBuf {
    let path = seeded_db_named(dir, name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 0..300 {
        let (subtype, types, rarity, pt, pn, ph, ci): (&str, &str, &str, Option<f64>, Option<f64>, Option<f64>, Option<&str>) =
            match i % 6 {
                0 => ("[]", "[\"Artifact\"]", "Common", Some(2.5), Some(3.0), Some(4.5), Some("[]")),
                1 => ("", "[]", "Uncommon", None, None, None, None),
                2 => ("[\"Human\",\"Wizard\"]", "[\"Creature\"]", "Rare", Some(0.0), Some(0.0), None, Some("[\"U\",\"B\"]")),
                3 => ("[\"Land\"]", "[\"Land\"]", "Basic Land", None, Some(0.01), None, Some("[]")),
                4 => ("[\"Elf\"]", "[\"Creature\",\"Elf\"]", "Mythic Rare", Some(1234.5678), Some(1000.0), Some(1500.0), Some("[\"G\"]")),
                _ => ("[\"Weird \\\"Quoted\\\" Token\"]", "[\"Creature\"]", "", Some(-1.5), None, Some(0.0), Some("[\"R\"]")),
            };
        conn.execute(
            "INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number,
                                      price_trend, price_normal, price_holofoil, cmc, color_identity, image_url)
             VALUES (?1, ?2, 'Card', ?3, ?4, ?5, 'wk1', 'Wéird Set \"Q\"', ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                format!("bulk{i}"),
                format!("Ünïcode & \"quoted\" <card> {i} — 東京"),
                subtype, types, rarity, format!("{i}½"), pt, pn, ph,
                rusqlite::types::Value::Real((i % 9) as f64 + 0.125), ci,
                if i % 7 == 0 { None } else { Some(format!("https://img/{i}.jpg")) },
            ],
        )
        .unwrap();
    }
    for j in 0..rows {
        conn.execute(
            "INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price,
                                      added_at, notes, is_trade, user_id, is_proxy)
             VALUES (?1, ?2, ?3, ?4, ?5, 'English', ?6, ?7, ?8, ?9, 1, ?10)",
            rusqlite::params![
                1000 + j as i64,
                format!("bulk{}", j % 300),
                (j % 4) as i64 + 1,
                ["Near Mint", "Lightly Played", "Damaged"][j % 3],
                if j % 3 == 0 { "Holofoil" } else { "Normal" },
                if j % 5 == 0 { None } else { Some(j as f64 * 0.05) },
                format!("2024-{:02}-{:02} 00:00:00", j % 12 + 1, j % 28 + 1),
                if j % 11 == 0 { "ñotes \"x\"" } else { "" },
                (j % 2) as i64,
                (j % 9 == 0) as i64,
            ],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    path
}

#[test]
fn parallel_build_matches_sequential_oracle_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_bulk_db(dir.path(), "bulk-identity.db", 4500);
    let pool = std::sync::Arc::new(scrybox_server::db::DbPool::open(&path, 4).unwrap());
    let cases = [
        None,
        Some(""),
        Some("is_trade=1"),
        Some("is_trade=0"),
        Some("limit=1000&offset=0"),
        Some("limit=2000&offset=1000"),
        Some("limit=4499&offset=0"),
        Some("include_allocation=1"),
        Some("count=0&limit=10"),
        Some("card_id=bulk7"),
        Some("include_allocation=1&count=0&limit=2000&offset=500"),
    ];
    for qs in cases {
        let p = scrybox_server::collection::parse_params(1, qs);
        let seq = scrybox_server::collection::build_body(&pool, &p).expect("sequential build");
        let par = scrybox_server::collection::build_body_parallel(&pool, &p).expect("parallel build");
        assert_eq!(seq.1, par.1, "x-total-count mismatch for {qs:?}");
        assert_eq!(seq.0, par.0, "body bytes diverged for {qs:?}");
    }
}

#[tokio::test]
async fn lazy_gzip_body_decompresses_to_the_raw_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_bulk_db(dir.path(), "lazy-gz.db", 4500);
    let r = router_for(path, &["collection"], "http://127.0.0.1:1");
    let (s1, _, raw) = get(&r, "/api/collection", &auth()).await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, h2, gzipped) = get(&r, "/api/collection", &[("authorization", "Bearer tok-session"), ("accept-encoding", "gzip")]).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(h2[header::CONTENT_ENCODING], "gzip");
    use std::io::Read;
    let mut dec = flate2::read::GzDecoder::new(&gzipped[..]);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).unwrap();
    assert_eq!(out, raw, "lazy multi-member gzip must decompress to the exact raw body");
}

#[tokio::test]
async fn concurrent_misses_coalesce_into_one_computation() {
    // The write-mix moment: a burst of concurrent requests hits a cold cache.
    // Without single-flight every one of them runs the full ~26 MB-shape
    // build (what Node does); with it, exactly ONE computation serves the
    // burst. computes()==1 is the contract (mirrors the stats.rs test).
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_bulk_db(dir.path(), "coalesce.db", 4500);
    let upstream = fake_node().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.clone(),
        static_dir: None,
        rust_routes: ["collection".to_string()].into_iter().collect(),
        db_path: Some(path),
        sidecar_url: None,
    };
    let state = AppState::new(cfg);
    let cache = std::sync::Arc::clone(&state.collection_cache);
    let r = app(state);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let r = r.clone();
        handles.push(tokio::spawn(async move {
            let req = Request::builder()
                .method("GET")
                .uri("/api/collection")
                .header("authorization", "Bearer tok-session")
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

// --- background pre-warm refresher (iter 14) ---------------------------------

/// Fire N writes the way the write-mix does, then poll until the refresher
/// has had ≥2 settled ticks (500 ms) plus build time.
async fn wait_refresher_window(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[tokio::test]
async fn refresher_prebuilds_post_bump_body_without_requests() {
    // Owner hint "collection_full pre-warm after invalidation": after a
    // revision bump, the poller must land the current body on its own — a
    // request arriving afterwards is served from the store (computes does
    // not grow).
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_bulk_db(dir.path(), "refresher.db", 4500);
    let upstream = fake_node().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.clone(),
        static_dir: None,
        rust_routes: ["collection".to_string()].into_iter().collect(),
        db_path: Some(path.clone()),
        sidecar_url: None,
    };
    let mut state = AppState::new(cfg);
    // Fixture builds take <1 ms; the iter-15 cost gate would (correctly)
    // never fan them out — disable it to test the fan-out mechanics.
    state.prewarm_min_cost_ms = 0;
    let cache = std::sync::Arc::clone(&state.collection_cache);
    let r = app(state);

    let (s, _, first) = get_json(&r, "/api/collection").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cache.computes(), 1, "cold request computes once");

    // Node-side write: bump the counter exactly like the schema trigger does.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("UPDATE collection_read_revision SET revision = revision + 1 WHERE id = 1;")
            .unwrap();
    }

    // No requests in flight: poll for the refresher's own compute.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while cache.computes() < 2 && std::time::Instant::now() < deadline {
        wait_refresher_window(50).await;
    }
    assert_eq!(cache.computes(), 2, "refresher must pre-build the post-bump body with no requests in flight");

    // The next request is served from the store — no new compute — and the
    // body is byte-identical (data unchanged; only the counter moved).
    let (s, _, second) = get_json(&r, "/api/collection").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(second, first, "post-bump body must equal the pre-bump body (data unchanged)");
    assert_eq!(cache.computes(), 2, "request after the pre-build must be a store hit");
}

#[tokio::test]
async fn refresher_does_no_unrequested_work_or_spurious_builds() {
    // Costs are bounded like Node's: a shape nobody requests is never built,
    // and a current entry is never rebuilt while the revision is stable
    // (protects the burst computes()==1 contract and idle-server cost).
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_bulk_db(dir.path(), "refresher-idle.db", 4500);
    let upstream = fake_node().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.clone(),
        static_dir: None,
        rust_routes: ["collection".to_string()].into_iter().collect(),
        db_path: Some(path.clone()),
        sidecar_url: None,
    };
    let mut state = AppState::new(cfg);
    state.prewarm_min_cost_ms = 0; // see note in the sibling refresher test
    let cache = std::sync::Arc::clone(&state.collection_cache);
    let r = app(state);

    // Idle server: many ticks, zero requests → zero builds.
    wait_refresher_window(1200).await;
    assert_eq!(cache.computes(), 0, "no requests must mean no background builds");

    let (s, _, _) = get_json(&r, "/api/collection?limit=50&offset=100").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cache.computes(), 1);

    // Revision stable: the poller must skip (is_current) — no spurious compute.
    wait_refresher_window(1500).await;
    assert_eq!(cache.computes(), 1, "a current entry at a stable revision must not be rebuilt");
}
