//! LANE-B unit 4 (iter-12): native POST /api/admin/seed-cards
//! (RUST_ROUTES=seed_cards). Auth ladder on the POST shape (API key hits
//! authenticateToken's read-only gate, NOT requireAdmin's admin-key 403),
//! kill-switch + method + body-parser forwards, happy path (208 rows,
//! domains, message/Σqty agreement, card_cache upsert, exactly two
//! /cards/search legs at set:lea / set:mh3), cache fallback, 502 empty,
//! 409 allocation conflict with rollback, and the 429 retry leg.

use std::{collections::BTreeSet, net::SocketAddr, sync::Arc};

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use scrybox_server::{app, AppState, Config};

mod mock_scryfall;

const SESS: Option<&str> = Some("Bearer tok-session");
const URI: &str = "/api/admin/seed-cards";

async fn spawn_fake_node() -> SocketAddr {
    let echo = axum::routing::any(|req: Request| async move {
        let parts = req.into_parts().0;
        (
            StatusCode::OK,
            [("x-node", "1")],
            axum::Json(json!({"pq": parts.uri.path_and_query().unwrap().as_str()})),
        )
    });
    let health = axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({"status":"ok"}))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

fn seeded_db(dir: &std::path::Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, api_key, share_token) VALUES (1,'u1','x','admin','tok-api','share-1');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-session', 1, '2099-01-01 00:00:00');
         INSERT INTO users (id, username, password_hash, role, share_token) VALUES (2,'u2','x','member','share-2');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-member', 2, '2099-01-01 00:00:00');",
    )
    .unwrap();
    conn
}

async fn router_for(dir: &std::path::Path, routes: &[&str], upstream: &str, scry: Option<SocketAddr>) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(dir.join("db.sqlite")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    if let Some(addr) = scry {
        st.scry = Arc::new(mock_scryfall::scry_for(addr, 0.0));
    }
    app(st)
}

fn post_json(uri: &str, auth: Option<&str>, body: &str) -> Request {
    let mut b = Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    if let Some(t) = auth {
        b.headers_mut().insert("authorization", t.parse().unwrap());
    }
    b
}

async fn body_val(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Raw Scryfall card for the mock's /cards/search `data` (normalizeCard's
/// input shape, not parseCardRow's row shape).
fn raw_card(id: &str, name: &str, set: &str, usd: Option<&str>, foil: Option<&str>) -> Value {
    json!({
        "id": id,
        "name": name,
        "set": set,
        "lang": "en",
        "type_line": "Creature — Human",
        "colors": ["W"],
        "color_identity": ["W"],
        "cmc": 2.0,
        "image_uris": {"normal": format!("https://cards.example/{id}.png")},
        "prices": {"usd": usd, "usd_foil": foil, "usd_etched": null},
    })
}

fn default_cards() -> Value {
    json!({ "data": [
        raw_card("s-normal", "Test Priced", "lea", Some("3.50"), Some("5.00")),
        raw_card("s-holo-only", "Test Foil Only", "lea", None, Some("2.10")),
        raw_card("s-cheap", "Test Freebie", "lea", Some("0"), None),
    ]})
}

fn rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let ncols = stmt.column_count();
    stmt.query_map([], |r| {
        Ok((0..ncols)
            .map(|i| match r.get::<_, rusqlite::types::Value>(i).unwrap() {
                rusqlite::types::Value::Null => Value::Null,
                rusqlite::types::Value::Integer(i) => json!(i),
                rusqlite::types::Value::Real(f) => json!(f),
                rusqlite::types::Value::Text(s) => json!(s),
                rusqlite::types::Value::Blob(_) => json!("<blob>"),
            })
            .collect::<Vec<_>>())
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_off_proxies() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &[], &format!("http://{node}"), None).await;
    let res = r.oneshot(post_json(URI, SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn non_post_forwards_to_node() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), None).await;
    let mut req = Request::get(URI).body(Body::empty()).unwrap();
    req.headers_mut().insert("authorization", SESS.unwrap().parse().unwrap());
    let res = r.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn malformed_json_body_forwards() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), None).await;
    // express.json (strict) runs BEFORE the admin gate: a strict-parse
    // failure is Node's 400 page to render, auth never runs here.
    let res = r.clone().oneshot(post_json(URI, SESS, "{oops")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
    // Without a token AND with a bad body the parser still wins (Forward).
    let res = r.oneshot(post_json(URI, None, "{oops")).await.unwrap();
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn auth_ladder_post_shape() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), None).await;

    let res = r.clone().oneshot(post_json(URI, None, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_val(res).await, json!({"error": "Access denied. No token provided."}));

    let res = r.clone().oneshot(post_json(URI, Some("Bearer nope"), "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_val(res).await, json!({"error": "Invalid or expired session token."}));

    // POST + API key: authenticateToken's read-only 403 fires BEFORE
    // requireAdmin's admin-key message (middleware/auth.js:30-33).
    let res = r.clone().oneshot(post_json(URI, Some("Bearer tok-api"), "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_val(res).await,
        json!({"error": "API keys are read-only. Use a login session for this request."})
    );

    let res = r.clone().oneshot(post_json(URI, Some("Bearer tok-member"), "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Access denied. Administrator privileges required."})
    );
}

#[tokio::test]
async fn happy_path_seeds_208_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path());
    let (saddr, mock) = mock_scryfall::spawn_mock().await;
    *mock.search.lock().unwrap() = default_cards();
        let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), Some(saddr)).await;

    // Pre-existing state: an admin row in a seed set (must be purged) and
    // one in a foreign set (must survive).
    conn.execute_batch(
        "INSERT INTO card_cache (id, name, set_id, price_normal) VALUES ('old-lea','Old Lea','lea',1.0);
         INSERT INTO card_cache (id, name, set_id, price_normal) VALUES ('keep-x','Keeper','xyz',1.0);
         INSERT INTO collection (card_id, quantity, user_id) VALUES ('old-lea', 5, 1);
         INSERT INTO collection (card_id, quantity, user_id) VALUES ('keep-x', 3, 1);",
    )
    .unwrap();

    let res = r.oneshot(post_json(URI, SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/json; charset=utf-8"
    );
    let v = body_val(res).await;
    let msg = v["message"].as_str().unwrap();
    // 'Successfully seeded a large test collection: N cards for admin user.'
    let caps: Vec<&str> = msg.split(' ').collect();
    assert_eq!(caps.len(), 11, "unexpected message: {msg}");
    assert_eq!(&caps[..6], &["Successfully", "seeded", "a", "large", "test", "collection:"]);
    assert_eq!(&caps[7..], &["cards", "for", "admin", "user."]);
    let added: i64 = caps[6].parse().unwrap();
    assert!((208..=416).contains(&added), "addedCount out of range: {added}");
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["message"]);

    // Exactly one /cards/search leg per seed set, in order.
    let hits = mock.hits.lock().unwrap();
    assert_eq!(hits.len(), 2, "hits: {hits:?}");
    assert!(hits[0].0.contains("q=set%3Alea"), "hit0: {}", hits[0].0);
    assert!(hits[1].0.contains("q=set%3Amh3"), "hit1: {}", hits[1].0);
    drop(hits);

    // The seed purged the user's seed-set rows, kept the foreign one, and
    // wrote exactly 168+40 entries.
    let left = rows(&conn, "SELECT COUNT(*) FROM collection WHERE user_id=1 AND card_id='old-lea'");
    assert_eq!(left[0][0], json!(0), "seed-set row survived the purge");
    let kept = rows(&conn, "SELECT quantity FROM collection WHERE user_id=1 AND card_id='keep-x'");
    assert_eq!(kept[0][0], json!(3), "foreign-set row must survive");
    let agg = rows(
        &conn,
        "SELECT COUNT(*), SUM(quantity) FROM collection WHERE user_id=1 AND card_id != 'keep-x'",
    );
    assert_eq!(agg[0][0], json!(208), "one row per insertSeedEntry call");
    assert_eq!(agg[0][1], json!(added), "message count must equal Σ quantity");

    // Value domains per randomEntry / printsForCard.
    let bad = rows(
        &conn,
        "SELECT COUNT(*) FROM collection WHERE user_id=1 AND card_id != 'keep-x'
           AND (quantity NOT IN (1,2)
                OR condition NOT IN ('Near Mint','Lightly Played','Moderately Played','Heavily Played')
                OR printing NOT IN ('Normal','Holofoil')
                OR language NOT IN ('English','Japanese')
                OR purchase_price IS NULL OR purchase_price < 0 OR purchase_price > 10
                OR ABS(ROUND(purchase_price * 100) - purchase_price * 100) > 1e-6)",
    );
    assert_eq!(bad[0][0], json!(0), "row outside the randomEntry domains");
    // Unpriced card → printsForCard = ['Normal']; foil-only → ['Holofoil'].
    let wrong = rows(
        &conn,
        "SELECT COUNT(*) FROM collection WHERE user_id=1 AND card_id='mtg-s-cheap' AND printing != 'Normal'",
    );
    assert_eq!(wrong[0][0], json!(0));
    let wrong = rows(
        &conn,
        "SELECT COUNT(*) FROM collection WHERE user_id=1 AND card_id='mtg-s-holo-only' AND printing != 'Holofoil'",
    );
    assert_eq!(wrong[0][0], json!(0));
    let priced_seen = rows(
        &conn,
        "SELECT COUNT(*) FROM collection WHERE user_id=1 AND card_id='mtg-s-normal'",
    );
    assert!(priced_seen[0][0].as_i64().unwrap() > 0, "priced card never drawn in 208?");

    // cacheCards upserted the normalized fetch rows.
    let cached = rows(
        &conn,
        "SELECT name, set_id, language, price_normal, price_holofoil FROM card_cache WHERE id='mtg-s-normal'",
    );
    assert_eq!(cached[0], vec![json!("Test Priced"), json!("lea"), json!("English"), json!(3.5), json!(5.0)]);
}

#[tokio::test]
async fn fallback_to_cache_when_apis_empty() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path());
    let (saddr, mock) = mock_scryfall::spawn_mock().await;
    *mock.search.lock().unwrap() = json!({"data": []});
        let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), Some(saddr)).await;

    conn.execute_batch(
        "INSERT INTO card_cache (id, name, set_id, price_normal) VALUES ('c1','Card One','xyz',1.5);
         INSERT INTO card_cache (id, name, set_id, price_normal) VALUES ('c2','Card Two','xyz',0);
         INSERT INTO card_cache (id, name, set_id, price_normal) VALUES ('c3','Card Three','xyz',2.5);",
    )
    .unwrap();

    let res = r.oneshot(post_json(URI, SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_val(res).await;
    let msg = v["message"].as_str().unwrap();
    let added: i64 = msg.split(' ').nth(6).unwrap().parse().unwrap();
    // Both sets answered [] — the legs still ran (2 hits), then the cache
    // fallback drove the seed; rows only from the cached ids.
    assert_eq!(mock.hits.lock().unwrap().len(), 2);
    let only_cached = rows(
        &conn,
        "SELECT COUNT(*) FROM collection WHERE user_id=1 AND card_id NOT IN ('c1','c2','c3')",
    );
    assert_eq!(only_cached[0][0], json!(0), "fallback pool leaked non-cached ids");
    let agg = rows(&conn, "SELECT COUNT(*), SUM(quantity) FROM collection WHERE user_id=1");
    assert_eq!(agg[0][0], json!(208));
    assert_eq!(agg[0][1], json!(added));
    // c2 is unpriced → Normal only.
    let wrong = rows(
        &conn,
        "SELECT COUNT(*) FROM collection WHERE card_id='c2' AND printing != 'Normal'",
    );
    assert_eq!(wrong[0][0], json!(0));
}

#[tokio::test]
async fn empty_pool_and_empty_cache_502() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let (saddr, mock) = mock_scryfall::spawn_mock().await;
    *mock.search.lock().unwrap() = json!({"data": []});
        let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), Some(saddr)).await;

    let res = r.oneshot(post_json(URI, SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Could not fetch seed card data from the card APIs, and local cache is empty. Try again shortly."})
    );
    assert_eq!(mock.hits.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn allocation_conflict_409_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path());
    let (saddr, mock) = mock_scryfall::spawn_mock().await;
    *mock.search.lock().unwrap() = default_cards();
        let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), Some(saddr)).await;

    // Owns 2 copies of a seed-set card; a checked-out deck needs 3 of the
    // same logical card → owned(2) − remove(2) < locked(3) → conflict.
    conn.execute_batch(
        "INSERT INTO card_cache (id, name, set_id, supertype, price_normal) VALUES ('z1','Zuran Orb','lea','Artifact',1.0);
         INSERT INTO collection (card_id, quantity, user_id) VALUES ('z1', 2, 1);
         INSERT INTO decks (id, user_id, name, checked_out) VALUES (1, 1, 'Draft', 1);
         INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (1, 'z1', 3);",
    )
    .unwrap();

    let res = r.oneshot(post_json(URI, SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Check in decks using these cards before replacing the seed collection."})
    );
    // Transaction rolled back: the owned row is untouched, nothing inserted.
    let agg = rows(&conn, "SELECT COUNT(*), MAX(quantity) FROM collection WHERE user_id=1");
    assert_eq!(agg[0], vec![json!(1), json!(2)], "409 must leave the collection byte-intact");
}

#[tokio::test]
async fn checked_in_deck_does_not_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path());
    let (saddr, mock) = mock_scryfall::spawn_mock().await;
    *mock.search.lock().unwrap() = default_cards();
        let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), Some(saddr)).await;

    // Same deck, but checked_out = 0 → locked_qty 0 → no conflict.
    conn.execute_batch(
        "INSERT INTO card_cache (id, name, set_id, supertype, price_normal) VALUES ('z1','Zuran Orb','lea','Artifact',1.0);
         INSERT INTO collection (card_id, quantity, user_id) VALUES ('z1', 2, 1);
         INSERT INTO decks (id, user_id, name, checked_out) VALUES (1, 1, 'Draft', 0);
         INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (1, 'z1', 3);",
    )
    .unwrap();

    let res = r.oneshot(post_json(URI, SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let agg = rows(&conn, "SELECT COUNT(*) FROM collection WHERE user_id=1");
    assert_eq!(agg[0][0], json!(208));
}

#[tokio::test]
async fn first_429_retries_then_seeds() {
    let dir = tempfile::tempdir().unwrap();
    let _conn = seeded_db(dir.path());
    let (saddr, mock) = mock_scryfall::spawn_mock().await;
    *mock.search.lock().unwrap() = default_cards();
    // lea: 429 then 200 (fetchFromScryfall's own retry — the shared
    // cooldown rides Retry-After: 0 here, so it does not stall the suite);
    // mh3 takes the remaining scripted 200.
    mock.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![429, 200, 200]);
    *mock.retry_after.lock().unwrap() = Some("0".into());
        let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), Some(saddr)).await;

    let res = r.oneshot(post_json(URI, SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let hits = mock.hits.lock().unwrap();
    assert_eq!(hits.len(), 3, "expected one retry on lea: {hits:?}");
    assert!(hits[0].0.contains("set%3Alea") && hits[1].0.contains("set%3Alea"));
    assert!(hits[2].0.contains("set%3Amh3"));
}

#[tokio::test]
async fn all_429_falls_to_cache_or_502() {
    let dir = tempfile::tempdir().unwrap();
    seeded_db(dir.path());
    let (saddr, mock) = mock_scryfall::spawn_mock().await;
    // Every leg 429 → fetchFromScryfall throws after 3 tries →
    // getCardsBySet answers [] → empty pool + empty cache → 502.
    mock.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![429]);
    *mock.retry_after.lock().unwrap() = Some("0".into());
        let node = spawn_fake_node().await;
    let r = router_for(dir.path(), &["seed_cards"], &format!("http://{node}"), Some(saddr)).await;

    let res = r.oneshot(post_json(URI, SESS, "{}")).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    // 3 attempts × 2 sets.
    assert_eq!(mock.hits.lock().unwrap().len(), 6);
}
