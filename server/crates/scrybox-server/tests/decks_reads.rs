//! Path-level tests for the iter-29 group: `decks_reads`
//! (GET /api/decks, GET /api/decks/:id, GET /api/decks/:id/locations).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (dr_* cases). These tests
//! pin the contract locally: kill-switch proxying, the auth gate with
//! parse-before-auth order, the deck-list cache header + revision
//! invalidation, the Express body-ETag/304 path (incl. the no-cache gate),
//! the minimum-value/ownership/commander maths on the list, the detail
//! shape (owned-printing swap incl. deck_list_card_id key order,
//! current/cheapest prices, maybeboard), the 404/422 error paths, and the
//! coverage rows (locked exclusion, basic-land flag, missing-desc sort).

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
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

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

fn seeded_db(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "CREATE TABLE deck_maybe_cards (
            deck_id INTEGER NOT NULL,
            card_id TEXT NOT NULL,
            quantity INTEGER DEFAULT 1,
            PRIMARY KEY(deck_id, card_id)
         );
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL),
                  (2,'u2','x','member','tok-2',0,'k59');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id,name,supertype,subtypes,types,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency)
           VALUES
             ('z-etali','Etali, Primal Storm','MTG','[\"Legendary\",\"Creature\",\"Dragon\"]','[\"Red\",\"Green\"]','Rare','one','Outlaws','1','img/etali-a',1.0,1.0,0,0,0,6,'[\"R\",\"G\"]','USD'),
             ('z-etali-b','Etali, Primal Storm ','MTG','[\"Legendary\",\"Creature\",\"Dragon\"]','[\"Red\",\"Green\"]','Rare','two','Strixhaven','2','img/etali-b',0.5,0.5,0,0,0,6,'[\"R\",\"G\"]','USD'),
             ('a-vial','Aether Vial','MTG','[\"Artifact\"]','[]','Uncommon','dmu','Dominaria','1','img/vial-a',0.1,0.1,0,0,0,1,'[]','USD'),
             ('a-vial-2','Aether Vial ','MTG','[\"Artifact\"]','[]','Uncommon','who','Doctor Who','2','img/vial-b',0.05,0.05,0,0,0,1,'[]','USD'),
             ('plains-x','Plains','MTG','[\"Basic\",\"Land\",\"Plains\"]','[]','Common','dmu','Dominaria','261',null,0.01,0.01,0,0,0,0,'[]','USD'),
             ('eur-card','Euro Thing','MTG','[\"Artifact\"]','[]','Common','dmu','Dominaria','9',null,2.0,2.0,0,0,0,1,'[]','EUR');

         INSERT INTO decks (id,user_id,name,description,checked_out,created_at) VALUES
             (101,1,'Commander Deck','cd',0,'2026-01-01 00:00:05'),
             (102,1,'Coverage Deck','cv',0,'2026-01-01 00:00:04'),
             (103,1,'Locked Deck','lk',1,'2026-01-01 00:00:03'),
             (104,1,'Broken Deck','bd',0,'2026-01-01 00:00:02'),
             (105,2,'Foreign Deck','fd',0,'2026-01-01 00:00:01');
         INSERT INTO deck_cards (deck_id,card_id,quantity,is_commander) VALUES
             (101,'z-etali',1,0),
             (101,'a-vial',4,0),
             (102,'plains-x',3,0),
             (102,'a-vial',1,0),
             (103,'plains-x',3,0),
             (104,'zzz-nope',1,0);
         INSERT INTO deck_maybe_cards (deck_id,card_id,quantity) VALUES (101,'z-etali-b',2);
         INSERT INTO collection (card_id,quantity,user_id) VALUES
             ('a-vial-2',2,1),
             ('plains-x',2,1);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);
         INSERT OR IGNORE INTO collection_read_revision (id, revision) VALUES (1, 1);
         INSERT OR IGNORE INTO stats_aux_revision (id, revision) VALUES (1, 0);",
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

async fn request(r: &Router, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut rb = Request::builder().method(method).uri(path);
    for &(k, v) in headers {
        rb = rb.header(k, v);
    }
    let req = rb.body(Body::from(body.unwrap_or("").to_string())).unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, headers, bytes)
}

fn json_body(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap()
}

const AUTH: [(&str, &str); 1] = [("authorization", "Bearer sess-1")];

fn open(db: &PathBuf) -> Connection {
    Connection::open(db).unwrap()
}

// ---------------------------------------------------------------------------
// kill switch + auth gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_proxies_all_three_reads() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "ks.db"), &[], &format!("http://{upstream}"));
    for path in ["/api/decks", "/api/decks/101", "/api/decks/101/locations"] {
        let (s, h, _) = request(&r, "GET", path, &AUTH, None).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert_eq!(h.get("x-node").unwrap(), "1", "{path}");
    }
}

#[tokio::test]
async fn unauthenticated_reads_get_node_401() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "auth.db"), &["decks_reads"], &format!("http://{upstream}"));
    let (s, _, b) = request(&r, "GET", "/api/decks", &[], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(&b), json!({ "error": "Access denied. No token provided." }));
    let (s, _, _) = request(&r, "GET", "/api/decks/101", &[], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bad_json_body_500s_before_auth() {
    // express.json runs ahead of the auth gate: an authenticated-looking GET
    // with an unparseable body and no token answers 500, not 401 (iter-28 pin).
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "badbody.db"), &["decks_reads"], &format!("http://{upstream}"));
    let h = [("content-type", "application/json")];
    let (s, _, b) = request(&r, "GET", "/api/decks", &h, Some("{nope")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({ "error": "Internal server error" }));
}

// ---------------------------------------------------------------------------
// GET /api/decks
// ---------------------------------------------------------------------------

fn deck_by_id<'a>(list: &'a [Value], id: i64) -> &'a Value {
    list.iter().find(|d| d["id"] == json!(id)).unwrap_or_else(|| panic!("deck {id} missing"))
}

#[tokio::test]
async fn list_shape_values_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "list.db"), &["decks_reads"], &format!("http://{upstream}"));
    let (s, h, b) = request(&r, "GET", "/api/decks", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "application/json; charset=utf-8");
    assert_eq!(h.get("x-deck-list-cache").unwrap(), "miss");
    assert!(h.get(header::ETAG).is_some());
    let list = json_body(&b);
    let list = list.as_array().unwrap();
    // created_at DESC (pinned vs live Node :3421 on this fixture).
    assert_eq!(list[0]["id"], json!(101));
    assert_eq!(list.last().unwrap()["id"], json!(104));
    assert_eq!(list.len(), 4, "user 1 only");

    let one = deck_by_id(list, 101);
    assert_eq!(one["total_card_types"], json!(2));
    assert_eq!(one["total_cards"], json!(5));
    assert_eq!(one["unresolved_card_types"], json!(0));
    // cheapest printing across ALL printings: Etali 0.5 + Vial 0.05 * 4
    assert_eq!(one["minimum_value"], json!(0.7));
    assert_eq!(one["minimum_value_currency"], json!("USD"));
    assert_eq!(one["unpriced_cards"], json!(0));
    assert_eq!(one["unpriced_card_types"], json!(0));
    // ownership (pinned): Etali 0/1 AND Vial 2/4 both short -> 2 types, 3
    // copies; basic lands are exempt, legends are not.
    assert_eq!(one["missing_card_types"], json!(2));
    assert_eq!(one["missing_copies"], json!(3));
    // legendary creature candidate wins without the flag.
    assert_eq!(one["commander_name"], json!("Etali, Primal Storm"));
    assert_eq!(one["commander_card_id"], json!("z-etali"));
    assert_eq!(one["commander_image_url"], json!("img/etali-a"));

    // basic lands are exempt from ownership; coverage deck is complete.
    let cov = deck_by_id(list, 102);
    assert_eq!(cov["missing_card_types"], json!(0));
    assert_eq!(cov["missing_copies"], json!(0));
    assert_eq!(cov["commander_name"], Value::Null);

    // broken deck surfaces unresolved counts instead of erroring.
    let broken = deck_by_id(list, 104);
    assert_eq!(broken["unresolved_card_types"], json!(1));

    // EUR printings never feed minimum_value (USD gate): Euro Thing has no
    // deck, but if the gate were missing, nothing here changes — instead
    // assert the Vial floor used USD rows: minimum 0.7 not 0 or 2.0-scaled.
    assert_eq!(one["minimum_value"], json!(0.7));
}

#[tokio::test]
async fn list_cache_hit_and_revision_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "cache.db");
    let r = router_for(db.clone(), &["decks_reads"], &format!("http://{upstream}"));
    let (s, h, b1) = request(&r, "GET", "/api/decks", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-deck-list-cache").unwrap(), "miss");
    let (_, h, b2) = request(&r, "GET", "/api/decks", &AUTH, None).await;
    assert_eq!(h.get("x-deck-list-cache").unwrap(), "hit");
    assert_eq!(b1, b2);

    // Any deck_cards write bumps deck_list_revision via the schema triggers;
    // the next request must miss and reflect the new deck.
    let conn = open(&db);
    conn.execute("INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES (102,'eur-card',1)", []).unwrap();
    drop(conn);
    let (s, h, b3) = request(&r, "GET", "/api/decks", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-deck-list-cache").unwrap(), "miss");
    assert_ne!(b1, b3);
    let list = json_body(&b3);
    let cov = deck_by_id(list.as_array().unwrap(), 102);
    // pinned: minimum_value stays 3x0.01 Plains + 1x0.05 Vial (eur-card has
    // no USD printing -> unpriced, never priced).
    assert_eq!(cov["unpriced_cards"], json!(1));
    assert_eq!(cov["minimum_value"], json!(0.08));
}

#[tokio::test]
async fn list_etag_304_and_no_cache_gate() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "etag.db"), &["decks_reads"], &format!("http://{upstream}"));
    let (_, h, _) = request(&r, "GET", "/api/decks", &AUTH, None).await;
    let tag = h.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    assert!(tag.starts_with("W/"), "express default etag is weak: {tag}");

    let with_tag = [("authorization", "Bearer sess-1"), ("if-none-match", tag.as_str())];
    let (s, h, b) = request(&r, "GET", "/api/decks", &with_tag, None).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    assert!(b.is_empty());
    assert_eq!(h.get(header::ETAG).unwrap(), tag.as_str());
    assert!(h.get(header::CONTENT_TYPE).is_none());
    assert_eq!(h.get("x-deck-list-cache").unwrap(), "hit");

    // fresh@0.5.2: a standalone `no-cache` directive disables the 304.
    let nc = [
        ("authorization", "Bearer sess-1"),
        ("if-none-match", tag.as_str()),
        ("cache-control", "no-cache"),
    ];
    let (s, _, b) = request(&r, "GET", "/api/decks", &nc, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!b.is_empty());
}

#[tokio::test]
async fn list_gzip_when_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "gz.db"), &["decks_reads"], &format!("http://{upstream}"));
    let gz = [("authorization", "Bearer sess-1"), ("accept-encoding", "gzip")];
    let (s, h, b) = request(&r, "GET", "/api/decks", &gz, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    // unit-5a security-header stack: the COEP middleware adds Vary User-Agent
    // and cors adds Origin ahead of compression's Accept-Encoding — oracle
    // is the Node instance (curl -D shows "User-Agent, Origin,
    // Accept-Encoding"). Header order/value parity is owned by
    // secheaders.rs tests; here we pin the compression leg only.
    assert_eq!(h.get(header::VARY).unwrap(), "User-Agent, Origin, Accept-Encoding");
    // body must be real gzip: decode and compare to the identity response.
    let mut d = flate2::read::GzDecoder::new(&b[..]);
    use std::io::Read;
    let mut plain = Vec::new();
    d.read_to_end(&mut plain).unwrap();
    let (_, _, raw) = request(&r, "GET", "/api/decks", &AUTH, None).await;
    assert_eq!(plain, raw);
}

// ---------------------------------------------------------------------------
// GET /api/decks/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detail_shape_prices_and_printing_swap() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "detail.db"), &["decks_reads"], &format!("http://{upstream}"));
    let (s, h, b) = request(&r, "GET", "/api/decks/101", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get(header::ETAG).is_some());
    let d = json_body(&b);
    assert_eq!(d["name"], json!("Commander Deck"));
    assert_eq!(d["minimum_value"], json!(0.7));
    // pinned: current-printing values are priced on the DISPLAY printings —
    // vial swapped to owned a-vial-2 -> etali 1x1.0 + vial 4x0.05 = 1.2.
    assert_eq!(d["current_printing_value"], json!(1.2));
    assert_eq!(d["current_unpriced_cards"], json!(0));
    assert_eq!(d["current_estimated_cards"], json!(0));

    let cards = d["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 2);
    // ORDER BY cc.name (SQL): Aether Vial first.
    assert_eq!(cards[0]["name"], json!("Aether Vial"));
    // Owned printing swap: user owns a-vial-2 (same logical name) and the
    // deck's own printing is NOT in the owned set.
    assert_eq!(cards[0]["id"], json!("a-vial-2"));
    assert_eq!(cards[0]["deck_list_card_id"], json!("a-vial"));
    assert_eq!(cards[0]["set_name"], json!("Doctor Who"));
    assert_eq!(cards[0]["current_price"], json!(0.05));
    assert_eq!(cards[0]["cheapest_price"], json!(0.05));
    // key order: deck_list_card_id (appended by the swap) precedes the
    // route-level current_price/cheapest_price inserts.
    let keys: Vec<&String> = cards[0].as_object().unwrap().keys().collect();
    let dlc = keys.iter().position(|k| *k == "deck_list_card_id").unwrap();
    let cur = keys.iter().position(|k| *k == "current_price").unwrap();
    let cheap = keys.iter().position(|k| *k == "cheapest_price").unwrap();
    assert!(dlc < cur && cur < cheap, "keys: {keys:?}");

    // Deck's own printing already owned for Etali? user owns none: the deck
    // row IS the representative and not owned -> swap to cheapest owned? no
    // owned Etali at all -> row untouched, still z-etali at deck printings.
    assert_eq!(cards[1]["name"], json!("Etali, Primal Storm"));
    assert_eq!(cards[1]["id"], json!("z-etali"));
    assert_eq!(cards[1]["current_price"], json!(1));
    assert_eq!(cards[1]["cheapest_price"], json!(0.5));
    assert!(cards[1].get("deck_list_card_id").is_none());
    // parseCardRow: JSON columns parsed, types array (live row shape: types
    // = colour words, subtypes = type words split).
    assert_eq!(cards[1]["types"], json!(["Red", "Green"]));
    assert_eq!(cards[1]["color_identity"], json!(["R", "G"]));
    assert_eq!(cards[1]["subtypes"], json!(["Legendary", "Creature", "Dragon"]));

    let maybe = d["maybeboard"].as_array().unwrap();
    assert_eq!(maybe.len(), 1);
    assert_eq!(maybe[0]["id"], json!("z-etali-b"));
    assert_eq!(maybe[0]["current_price"], json!(0.5));
}

#[tokio::test]
async fn detail_404_foreign_and_422_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "detail404.db"), &["decks_reads"], &format!("http://{upstream}"));
    let (s, _, b) = request(&r, "GET", "/api/decks/105", &AUTH, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "Deck not found" }));

    let (s, _, b) = request(&r, "GET", "/api/decks/104", &AUTH, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(&b),
        json!({ "error": "This deck contains cards whose details are unavailable. Remove or re-import them before continuing." })
    );
}

#[tokio::test]
async fn detail_never_405s_shared_shape_to_writes() {
    // With decks_writes on and decks_reads off, GET on the shared shape must
    // proxy (the write group only owns PUT/DELETE); never 405/404-from-us.
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(
        seeded_db(dir.path(), "shared.db"),
        &["decks_writes"],
        &format!("http://{upstream}"),
    );
    let (s, h, _) = request(&r, "GET", "/api/decks/101", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    // And a shape neither group owns still proxies with both on.
    let dir2 = tempfile::tempdir().unwrap();
    let r2 = router_for(
        seeded_db(dir2.path(), "shared2.db"),
        &["decks_reads", "decks_writes"],
        &format!("http://{upstream}"),
    );
    let (s, h, _) = request(&r2, "GET", "/api/decks/101/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    // GET on the shared shape with both on -> native reads.
    let (s, h, _) = request(&r2, "GET", "/api/decks/101", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none());
}

// ---------------------------------------------------------------------------
// GET /api/decks/:id/locations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn locations_coverage_math_and_sort() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "loc.db"), &["decks_reads"], &format!("http://{upstream}"));
    let (s, _, b) = request(&r, "GET", "/api/decks/102/locations", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    let rows = json_body(&b);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // missing DESC (pinned): Plains required 3, owned 2, locked 3 (deck 103)
    // -> available = owned - locked = -1, missing = required - available = 4.
    assert_eq!(rows[0]["name"], json!("Plains"));
    assert_eq!(rows[0]["required"], json!(3));
    assert_eq!(rows[0]["owned"], json!(2));
    assert_eq!(rows[0]["in_use"], json!(3));
    assert_eq!(rows[0]["available"], json!(0));
    assert_eq!(rows[0]["missing"], json!(4));
    assert_eq!(rows[0]["covered"], json!(false));
    assert_eq!(rows[0]["is_basic_land"], json!(true));
    // Vial: required 1, owned 2 (other printing), nothing locked -> covered.
    assert_eq!(rows[1]["name"], json!("Aether Vial"));
    assert_eq!(rows[1]["owned"], json!(2));
    assert_eq!(rows[1]["missing"], json!(0));
    assert_eq!(rows[1]["covered"], json!(true));
    assert_eq!(rows[1]["is_basic_land"], json!(false));
    assert_eq!(rows[1]["card_name"], json!("Aether Vial"));
}

#[tokio::test]
async fn locations_404_and_422() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "loc404.db"), &["decks_reads"], &format!("http://{upstream}"));
    let (s, _, b) = request(&r, "GET", "/api/decks/105/locations", &AUTH, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "Deck not found" }));
    let (s, _, b) = request(&r, "GET", "/api/decks/104/locations", &AUTH, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_body(&b), json!({ "error": "Deck contains cards with missing metadata" }));
}

#[tokio::test]
async fn locations_ignores_other_users_locked_decks() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "loclock.db");
    let conn = Connection::open(&db).unwrap();
    // Another user's checked-out deck holding Plains must NOT lock ours.
    conn.execute(
        "INSERT INTO decks (id,user_id,name,checked_out,created_at) VALUES (106,2,'Their locked',1,'2026-01-01 00:00:00')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO deck_cards (deck_id,card_id,quantity) VALUES (106,'plains-x',9)", []).unwrap();
    drop(conn);
    let r = router_for(db, &["decks_reads"], &format!("http://{upstream}"));
    let (s, _, b) = request(&r, "GET", "/api/decks/102/locations", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    let rows = json_body(&b);
    let plains = rows.as_array().unwrap().iter().find(|x| x["name"] == json!("Plains")).unwrap();
    // only our own deck 103's 3 copies stay locked out (pinned: same -1
    // available math -> missing 4).
    assert_eq!(plains["in_use"], json!(3));
    assert_eq!(plains["missing"], json!(4));
}
