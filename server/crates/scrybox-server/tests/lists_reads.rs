//! Path-level tests for the iter-31 group: `lists_reads`
//! (GET /api/lists, GET /api/lists/card-matches, GET /api/lists/:id,
//! GET /api/lists/:id/cardlist).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (lr_* cases). These tests
//! pin the contract locally: kill-switch proxying on all four shapes, the
//! auth gate with parse-before-auth order, the never-405 dispatcher (PUT/
//! DELETE on the /:id shape go to the proxy, not 405), the collection fold
//! (sort_order ordering, totals, minimum-value maths), the card-matches
//! object (short-q {}, LIKE-escape, numeric-key ascending order), the detail
//! 404/422/200 paths (owned_qty, current vs minimum totals, parseCardRow
//! arrays), and the cardlist text twin (plain/detailed/missing=1, ETag/304).

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
        "CREATE TABLE card_lists (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            description TEXT,
            accent_color TEXT DEFAULT '#10b981',
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP, source TEXT, icon TEXT, sort_order INTEGER
         );
         CREATE TABLE list_cards (
            list_id INTEGER NOT NULL,
            card_id TEXT NOT NULL,
            quantity INTEGER DEFAULT 1,
            PRIMARY KEY(list_id, card_id)
         );
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL),
                  (2,'u2','x','member','tok-2',0,'k59');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id,name,supertype,subtypes,types,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency)
           VALUES
             ('z-alpha','Alpha Card','MTG','[\"Creature\"]','[\"White\"]','Rare','lrs','LR Set','1','img/a',1.0,1.0,0,0,0,2,'[\"W\"]','USD'),
             ('z-alpha-b','Alpha Card ','MTG','[\"Creature\"]','[\"White\"]','Rare','lrb','LR Set B','2','img/ab',0.2,0.2,0,0,0,2,'[\"W\"]','USD'),
             ('z-beta','Beta Card','MTG','[\"Instant\"]','[\"Blue\"]','Common','lrp','LR Promos','42',null,0.5,0.5,0,0,0,1,'[\"U\"]','USD'),
             ('z-est','Est Card','MTG','[\"Artifact\"]','[]','Common','lrs','LR Set','7',null,4.0,4.0,0,0,1,0,'[]','USD'),
             ('z-eur','Eur Card','MTG','[\"Artifact\"]','[]','Common','lrs','LR Set','8',null,9.0,9.0,0,0,0,0,'[]','EUR');

         INSERT INTO card_lists (id,user_id,name,description,accent_color,created_at,source,icon,sort_order) VALUES
           (201,1,'Wishlist','w','#10b981','2026-01-01 00:00:05','web',null,NULL),
           (202,1,'Owned Fold','o','#10b981','2026-01-01 00:00:04','web',null,0),
           (203,1,'Broken List','b','#10b981','2026-01-01 00:00:03','web',null,NULL),
           (204,2,'Foreign List','f','#10b981','2026-01-01 00:00:02','web',null,NULL);
         INSERT INTO list_cards (list_id,card_id,quantity) VALUES
           (201,'z-alpha',1),
           (201,'z-alpha-b',1),
           (201,'z-beta',2),
           (201,'z-eur',1),
           (202,'z-alpha',4),
           (202,'z-beta',2),
           (202,'z-est',1),
           (203,'zzz-nope',1);
         INSERT INTO collection (card_id,quantity,user_id) VALUES
           ('z-beta',2,1),
           ('z-alpha-b',1,1);",
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

// ---------------------------------------------------------------------------
// kill switch + auth gate + never-405 dispatcher
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_proxies_all_four_reads() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "ks.db"), &[], &format!("http://{upstream}"));
    for path in [
        "/api/lists",
        "/api/lists/card-matches?q=alpha",
        "/api/lists/201",
        "/api/lists/201/cardlist",
    ] {
        let (s, h, _) = request(&r, "GET", path, &AUTH, None).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert_eq!(h.get("x-node").unwrap(), "1", "{path}");
    }
}

#[tokio::test]
async fn unauthenticated_reads_get_node_401() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "auth.db"), &["lists_reads"], &format!("http://{upstream}"));
    for path in ["/api/lists", "/api/lists/card-matches?q=ab", "/api/lists/201", "/api/lists/201/cardlist"] {
        let (s, _, b) = request(&r, "GET", path, &[], None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(json_body(&b), json!({ "error": "Access denied. No token provided." }), "{path}");
    }
}

#[tokio::test]
async fn bad_json_body_500s_before_auth() {
    // express.json runs ahead of the auth gate (iter-28 pin, shared rule).
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "badbody.db"), &["lists_reads"], &format!("http://{upstream}"));
    let h = [("content-type", "application/json")];
    let (s, _, b) = request(&r, "GET", "/api/lists", &h, Some("{nope")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({ "error": "Internal server error" }));
}

#[tokio::test]
async fn writes_on_owned_shapes_never_405() {
    // PUT/DELETE land on the /api/lists/{id} dispatcher — they must proxy to
    // Node, never 405 from Rust (kill-switch semantics for the write group).
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "w.db"), &["lists_reads"], &format!("http://{upstream}"));
    let j = [("content-type", "application/json")];
    let (s, h, b) = request(&r, "PUT", "/api/lists/201", &j, Some("{\"name\":\"x\"}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["method"], json!("PUT"));
    assert_eq!(json_body(&b)["pq"], json!("/api/lists/201"));
    let (s, h, _) = request(&r, "DELETE", "/api/lists/201", &[], None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    // Bulk-ish shapes are not mounted at all -> proxy fallback.
    let (s, h, _) = request(&r, "POST", "/api/lists/201/cards", &j, Some("{}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    // /api/lists/order must not collide with /:id — PUT proxies.
    let (s, h, b) = request(&r, "PUT", "/api/lists/order", &j, Some("{\"ids\":[201]}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["pq"], json!("/api/lists/order"));
}

// ---------------------------------------------------------------------------
// GET /api/lists
// ---------------------------------------------------------------------------

#[tokio::test]
async fn collection_shape_totals_minimum_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "c.db"), &["lists_reads"], &format!("http://{upstream}"));
    let (s, h, b) = request(&r, "GET", "/api/lists", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "application/json; charset=utf-8");
    assert!(h.get(header::ETAG).is_some());
    let list = json_body(&b);
    let arr = list.as_array().unwrap();
    // user 1 only; (sort_order IS NULL) DESC, sort_order ASC, created_at DESC
    // -> NULLs first (201, 203 by created_at DESC), then sorted 202.
    assert_eq!(arr.iter().map(|l| l["id"].as_i64().unwrap()).collect::<Vec<_>>(), vec![201, 203, 202]);
    let l201 = &arr[0];
    assert_eq!(l201["total_card_types"], json!(3), "alpha fold counts as one logical card");
    assert_eq!(l201["total_cards"], json!(5));
    assert_eq!(l201["unresolved_card_types"], json!(0));
    // alpha fold 2 x cheapest(0.2) + beta 2 x 0.5 = 1.4; EUR unpriced.
    assert_eq!(l201["minimum_value"], json!(1.4));
    assert_eq!(l201["minimum_value_currency"], json!("USD"));
    assert_eq!(l201["unpriced_cards"], json!(1));
    assert_eq!(l201["unpriced_card_types"], json!(1));
    let l203 = &arr[1];
    assert_eq!(l203["unresolved_card_types"], json!(1));
    assert_eq!(l203["total_card_types"], json!(1), "missing rows count as 'missing:'||card_id");
    assert_eq!(l203["total_cards"], json!(1));
    assert_eq!(l203["minimum_value"], json!(0));
    // key order: row columns then minimums (express spread order).
    let keys: Vec<&String> = l201.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec!["id", "name", "description", "accent_color", "icon", "created_at",
             "total_card_types", "total_cards", "unresolved_card_types",
             "minimum_value", "minimum_value_currency", "unpriced_cards", "unpriced_card_types"]
    );
}

#[tokio::test]
async fn collection_etag_304_and_no_cache_gate() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "etag.db"), &["lists_reads"], &format!("http://{upstream}"));
    let (_, h, _) = request(&r, "GET", "/api/lists", &AUTH, None).await;
    let tag = h.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let inm = [("authorization", "Bearer sess-1"), ("if-none-match", tag.as_str())];
    let (s, _, b) = request(&r, "GET", "/api/lists", &inm, None).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    assert!(b.is_empty());
    let nc = [
        ("authorization", "Bearer sess-1"),
        ("if-none-match", tag.as_str()),
        ("cache-control", "no-cache"),
    ];
    let (s, _, b) = request(&r, "GET", "/api/lists", &nc, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!b.is_empty());
}

// ---------------------------------------------------------------------------
// GET /api/lists/card-matches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn card_matches_short_q_answers_empty_object() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "cm1.db"), &["lists_reads"], &format!("http://{upstream}"));
    for q in ["", "a"] {
        let (s, _, b) = request(&r, "GET", &format!("/api/lists/card-matches?q={q}"), &AUTH, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(json_body(&b), json!({}));
    }
    let (s, _, b) = request(&r, "GET", "/api/lists/card-matches", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({}));
}

#[tokio::test]
async fn card_matches_groups_by_list_numeric_key_order() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "cm2.db"), &["lists_reads"], &format!("http://{upstream}"));
    // 'Card' matches alpha (201,202), beta (201,202), est (202), eur (201).
    let (s, _, b) = request(&r, "GET", "/api/lists/card-matches?q=Card", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    let v = json_body(&b);
    assert_eq!(v["201"], json!(["Alpha Card", "Alpha Card ", "Beta Card", "Eur Card"]),
               "DISTINCT over raw names — the trailing-space printing is its own name");
    assert_eq!(v["202"], json!(["Alpha Card", "Beta Card", "Est Card"]));
    // JS object integer-key order: 201 before 202 (ascending numeric).
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["201", "202"]);
    // LIKE metacharacters are literal, not wildcards.
    let (s, _, b) = request(&r, "GET", "/api/lists/card-matches?q=%25", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({}));
    // foreign user's list name never matches for user 1.
    let (s, _, b) = request(&r, "GET", "/api/lists/card-matches?q=Foreign", &AUTH, None).await;
    assert_eq!(json_body(&b), json!({}));
}

// ---------------------------------------------------------------------------
// GET /api/lists/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detail_fold_prices_and_owned_qty() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "d.db"), &["lists_reads"], &format!("http://{upstream}"));
    let (s, h, b) = request(&r, "GET", "/api/lists/201", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "application/json; charset=utf-8");
    let v = json_body(&b);
    assert_eq!(v["id"], json!(201));
    assert_eq!(v["minimum_value"], json!(1.4));
    assert_eq!(v["unpriced_cards"], json!(1));
    // current axis is PER list_cards row (unfolded): 1x1.0 + 1x0.2 + 2x0.5 = 2.2.
    assert_eq!(v["current_printing_value"], json!(2.2));
    assert_eq!(v["current_unpriced_cards"], json!(1));
    assert_eq!(v["current_estimated_cards"], json!(0));
    let cards = v["cards"].as_array().unwrap();
    // fold: alpha(+b) one row qty 2 rep=MIN(id)='z-alpha'; ORDER BY cc.name.
    assert_eq!(cards.len(), 3);
    assert_eq!(cards[0]["name"], json!("Alpha Card"));
    assert_eq!(cards[0]["quantity"], json!(2));
    assert_eq!(cards[0]["id"], json!("z-alpha"), "MIN(card_id) representative");
    assert_eq!(cards[0]["subtypes"], json!(["Creature"]), "parseCardRow JSON columns");
    assert_eq!(cards[0]["color_identity"], json!([]), "SELECT omits color_identity -> parseCardRow []");
    assert_eq!(cards[0]["owned_qty"], json!(1), "user owns z-alpha-b 1 -> same logical card");
    assert_eq!(cards[1]["name"], json!("Beta Card"));
    assert_eq!(cards[1]["owned_qty"], json!(2));
    assert_eq!(cards[2]["name"], json!("Eur Card"));
    assert_eq!(cards[2]["owned_qty"], json!(0));
    // key order: SELECT columns then parseCardRow tail (color_identity last).
    let keys: Vec<&String> = cards[0].as_object().unwrap().keys().collect();
    assert_eq!(keys.last().unwrap().as_str(), "color_identity");
}

#[tokio::test]
async fn detail_estimated_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "d2.db"), &["lists_reads"], &format!("http://{upstream}"));
    let (s, _, b) = request(&r, "GET", "/api/lists/202", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    let v = json_body(&b);
    // est card: currentPrintingPrice has NO estimated gate (the estimated flag
    // only marks the total '~' in deckCurrentPrintValues): 4x1.0 + 2x0.5 +
    // 1x4.0 = 9.0 with 1 estimated copy.
    assert_eq!(v["current_printing_value"], json!(9));
    assert_eq!(v["current_unpriced_cards"], json!(0));
    assert_eq!(v["current_estimated_cards"], json!(1));
    // foreign -> 404 with Node's wording.
    let (s, _, b) = request(&r, "GET", "/api/lists/204", &AUTH, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "List not found" }));
    // non-numeric id -> no row -> 404 (no parseInt gate in Node).
    let (s, _, _) = request(&r, "GET", "/api/lists/abc", &AUTH, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // unresolved -> 422 with the LIST wording.
    let (s, _, b) = request(&r, "GET", "/api/lists/203", &AUTH, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(&b),
        json!({ "error": "This list contains cards whose details are unavailable. Remove or re-import them before continuing." })
    );
}

// ---------------------------------------------------------------------------
// GET /api/lists/:id/cardlist
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cardlist_plain_detailed_missing_and_304() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "cl.db"), &["lists_reads"], &format!("http://{upstream}"));
    let (s, h, b) = request(&r, "GET", "/api/lists/201/cardlist", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "text/plain; charset=utf-8");
    let tag = h.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    assert_eq!(
        String::from_utf8(b).unwrap(),
        "2 Alpha Card\n2 Beta Card\n1 Eur Card",
        "folded quantities, ORDER BY cc.name"
    );
    let (s, _, b) = request(&r, "GET", "/api/lists/201/cardlist?style=detailed", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        String::from_utf8(b).unwrap(),
        "2 Alpha Card (LRS) 1\n2 Beta Card (LRP) 42\n1 Eur Card (LRS) 8",
        "detailed = qty Name (SET upper) num"
    );
    // missing=1: beta fully owned (2/2) drops, alpha 2-1=1.
    let (s, _, b) = request(&r, "GET", "/api/lists/201/cardlist?missing=1", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(String::from_utf8(b).unwrap(), "1 Alpha Card\n1 Eur Card");
    // ETag/304 round trip keeps the text content-type gate.
    let inm = [("authorization", "Bearer sess-1"), ("if-none-match", tag.as_str())];
    let (s, h, b) = request(&r, "GET", "/api/lists/201/cardlist", &inm, None).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    assert!(b.is_empty());
    assert_eq!(h.get(header::ETAG).unwrap().to_str().unwrap(), tag);
    // 404/422 wordings.
    let (s, _, b) = request(&r, "GET", "/api/lists/204/cardlist", &AUTH, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "List not found" }));
    let (s, _, b) = request(&r, "GET", "/api/lists/203/cardlist", &AUTH, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(&b),
        json!({ "error": "This list contains cards whose details are unavailable. Remove or re-import them before exporting." })
    );
}
