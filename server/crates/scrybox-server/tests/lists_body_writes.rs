//! LANE A iteration 33 — card-list CARD-BODY writes.
//!
//! The seven /api/lists/:id card-body write routes from
//! backend/src/routes/lists.js (single add, bulk add, bulk remove,
//! remove-from-collection, cheapest printings, change printing, remove card).
//! Node-oracle behaviours pinned here:
//!   * single add SETS the quantity — CASE/ELSE-0 fold onto the effective
//!     printing then a quantity<=0 purge (bulk add INCREMENTs instead);
//!   * Math.max(1, parseInt(q,10) || 1) quantity clamping;
//!   * card_cache miss + string id -> the WHOLE request is forwarded to the
//!     upstream untouched, before any local statement (bulk: a read-only
//!     pre-pass over every entry, never a half-applied replay);
//!   * bulk-remove has no per-entry catch (the route 500s whole) and counts
//!     Math.min(qty, row.quantity);
//!   * remove-from-collection budgets owned-locked per logical card across
//!     rows, dry_run leaves every byte alone, DELETE/UPDATE are user-scoped;
//!   * cheapest-printings skips demands that are ALREADY aligned
//!     (`if (!misaligned) continue;`) — the move only fires when the list
//!     holds a non-cheapest row for the key.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
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
             ('z-alpha','Alpha Card','MTG','[\"Creature\"]','[\"White\"]','Rare','lrs','LR Set','1','img/a',1.0,5.0,0,0,0,2,'[\"W\"]','USD'),
             ('z-alpha2','Alpha Card','MTG','[\"Creature\"]','[\"White\"]','Rare','lrp','LR Promos','2',null,0,5.0,0,0,0,2,'[\"W\"]','USD'),
             ('z-alpha-cheap','Alpha Card','MTG','[\"Creature\"]','[\"White\"]','Common','lrq','LR Bundle','3',null,0,0.2,0,0,0,2,'[\"W\"]','USD'),
             ('z-beta','Beta Card','MTG','[\"Instant\"]','[\"Blue\"]','Common','lrp','LR Promos','42',null,0.5,0,0,0,0,1,'[\"U\"]','USD'),
             ('z-gamma','Gamma Card','MTG','[\"Land\"]','[\"\"]','Common','lrs','LR Set','9',null,NULL,NULL,0,0,0,0,'[]',NULL);
         INSERT INTO card_lists (id,user_id,name,description,accent_color,created_at,source,icon,sort_order) VALUES
           (201,1,'Wishlist','w','#10b981','2026-01-01 00:00:05','web',null,NULL),
           (202,1,'Owned Fold','o','#10b981','2026-01-01 00:00:04','web',null,0),
           (204,2,'Foreign List','f','#10b981','2026-01-01 00:00:02','web',null,NULL);
         INSERT INTO list_cards (list_id,card_id,quantity) VALUES
           (201,'z-alpha',1),
           (201,'z-beta',2),
           (202,'z-beta',1);",
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
const JSON_CT: [(&str, &str); 2] = [("authorization", "Bearer sess-1"), ("content-type", "application/json")];
const JSON_ONLY: [(&str, &str); 1] = [("content-type", "application/json")];

fn open(db: &PathBuf) -> Connection {
    Connection::open(db).unwrap()
}

/// (card_id, quantity) rows of one list, ordered by card_id.
fn lc(conn: &Connection, list_id: i64) -> Vec<(String, i64)> {
    let mut stmt = conn
        .prepare("SELECT card_id, quantity FROM list_cards WHERE list_id = ?1 ORDER BY card_id")
        .unwrap();
    let mut rows = stmt.query(rusqlite::params![list_id]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        out.push((r.get(0).unwrap(), r.get(1).unwrap_or(0)));
    }
    out
}

struct Ctx {
    router: Router,
    db: PathBuf,
}

fn url_of(a: &SocketAddr) -> String {
    format!("http://{a}")
}

async fn ctx(name: &str, routes: &[&str]) -> Ctx {
    let dir = tempfile::tempdir().unwrap().keep();
    let db = seeded_db(&dir, name);
    let upstream = spawn_fake_node().await;
    Ctx { router: router_for(db.clone(), routes, &url_of(&upstream)), db }
}

impl Ctx {
    async fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> (StatusCode, HeaderMap, Value) {
        let raw = body.map(|b| serde_json::to_string(&b).unwrap());
        let (s, h, b) = request(&self.router, method, path, headers, raw.as_deref()).await;
        (s, h, json_body(&b))
    }
    fn conn(&self) -> Connection {
        open(&self.db)
    }
}

// ---------------------------------------------------------------------------
// POST /:id/cards — SET semantics with the CASE/ELSE-0 fold + purge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_card_sets_quantity_and_folds_equivalents() {
    let c = ctx("lb-add.db", &["lists_writes"]).await;
    // z-alpha2 is an alternate printing of 'Alpha Card'; the equivalent-row
// query (ORDER BY (lc.card_id = ?) DESC, lc.card_id) resolves the key to the
// EXISTING z-alpha row, which is SET to 2 (not incremented, no new row).
    let (s, h, body) = c.call("POST", "/api/lists/201/cards", &JSON_CT, Some(json!({"card_id": "z-alpha2", "quantity": "2x"}))).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["message"], "Card added to list");
    assert!(h.get("x-node").is_none(), "must not proxy");
    assert_eq!(
        lc(&c.conn(), 201),
        vec![("z-alpha".into(), 2), ("z-beta".into(), 2)],
        "fold keeps exactly one row per logical card"
    );
}

#[tokio::test]
async fn add_card_new_row_and_clamps() {
    let c = ctx("lb-add2.db", &["lists_writes"]).await;
    // quantity 0 -> parseInt 0 -> || 1 -> 1; '7abc' -> 7; SET semantics.
    let (s, _, body) = c.call("POST", "/api/lists/201/cards", &JSON_CT, Some(json!({"card_id": "z-gamma", "quantity": 0}))).await;
    assert_eq!(s, 200, "{body}");
    let (s, _, _) = c.call("POST", "/api/lists/201/cards", &JSON_CT, Some(json!({"card_id": "z-alpha", "quantity": "7abc"}))).await;
    assert_eq!(s, 200);
    let got = lc(&c.conn(), 201);
    assert!(got.iter().any(|(card, q)| card == "z-gamma" && *q == 1), "{got:?}");
    assert_eq!(got.iter().find(|(card, _)| card == "z-alpha").unwrap().1, 7);
    // Missing quantity -> destructuring default 1.
    let (s, _, _) = c.call("POST", "/api/lists/202/cards", &JSON_CT, Some(json!({"card_id": "z-alpha"}))).await;
    assert_eq!(s, 200);
    assert_eq!(lc(&c.conn(), 202), vec![("z-alpha".into(), 1), ("z-beta".into(), 1)]);
}

#[tokio::test]
async fn add_card_error_gates_and_ordering() {
    let c = ctx("lb-add-gates.db", &["lists_writes"]).await;
    let (s, _, body) = c.call("POST", "/api/lists/201/cards", &JSON_CT, Some(json!({}))).await;
    assert_eq!(s, 400);
    assert_eq!(body["error"], "card_id is required");
    // Unauthenticated with a JSON body: auth runs before the route body.
    let (s, _, _) = c.call("POST", "/api/lists/201/cards", &JSON_ONLY, Some(json!({"card_id": "z-gamma"}))).await;
    assert_eq!(s, 401);
    // Someone else's list -> 404, nothing written.
    let (s, _, body) = c.call("POST", "/api/lists/204/cards", &JSON_CT, Some(json!({"card_id": "z-gamma"}))).await;
    assert_eq!(s, 404);
    assert_eq!(body["error"], "List not found or unauthorized");
    assert_eq!(lc(&c.conn(), 204), Vec::<(String, i64)>::new());
}

#[tokio::test]
async fn add_card_hydration_forward_and_type_error() {
    let c = ctx("lb-add-fwd.db", &["lists_writes"]).await;
    // String id, cache miss -> whole request forwarded untouched, no local row.
    let (s, h, body) = c.call("POST", "/api/lists/201/cards", &JSON_CT, Some(json!({"card_id": "z-uncached-9", "quantity": 4}))).await;
    assert_eq!(s, 200);
    assert_eq!(h.get("x-node").map(|v| v.to_str().unwrap()), Some("1"));
    assert_eq!(body["pq"], "/api/lists/201/cards");
    assert_eq!(body["method"], "POST");
    let inner: Value = serde_json::from_str(body["body"].as_str().unwrap()).unwrap();
    assert_eq!(inner["quantity"], 4);
    let n: i64 = c
        .conn()
        .query_row("SELECT COUNT(*) FROM list_cards WHERE card_id = 'z-uncached-9'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "forward path writes nothing before deciding");
    // Non-string id on a cache miss: Node's getCardById TypeError -> route 500.
    let (s, h, body) = c.call("POST", "/api/lists/201/cards", &JSON_CT, Some(json!({"card_id": 931211931}))).await;
    assert_eq!(s, 500);
    assert_eq!(body["error"], "Failed to add card to list");
    assert!(h.get("x-node").is_none());
}

// ---------------------------------------------------------------------------
// POST /:id/cards/bulk — INCREMENT semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_add_gates() {
    let c = ctx("lb-bulk-gates.db", &["lists_writes"]).await;
    let oversize: Vec<Value> = (0..251).map(|i| json!({"card_id": format!("z-c{i}")})).collect();
    for bad in [json!({}), json!({"cards": "z-beta"}), json!({"cards": []}), json!({"cards": oversize})] {
        let (s, h, body) = c.call("POST", "/api/lists/201/cards/bulk", &JSON_CT, Some(bad.clone())).await;
        assert_eq!(s, 400, "{body} for {bad}");
        assert_eq!(body["error"], "cards must be 1-250 entries");
        assert!(h.get("x-node").is_none());
    }
    let (s, _, _) = c.call("POST", "/api/lists/201/cards/bulk", &JSON_ONLY, Some(json!({"cards": []}))).await;
    assert_eq!(s, 401, "entry gate is inside the router, auth precedes");
}

#[tokio::test]
async fn bulk_add_increments_and_skips_falsy() {
    let c = ctx("lb-bulk.db", &["lists_writes"]).await;
    let (s, h, body) = c
        .call(
            "POST",
            "/api/lists/201/cards/bulk",
            &JSON_CT,
            Some(json!({"cards": [
                {"card_id": "z-beta", "quantity": 3},          // increment existing
                {"card_id": "z-alpha2", "quantity": "2x"},     // fold key onto existing row
                {"quantity": 5},                                // falsy card_id -> skipped
                {"card_id": null},                              // skipped
                {"card_id": 424242}                             // non-string cache miss -> failed
            ]})),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    assert!(h.get("x-node").is_none());
    assert_eq!(body["added"], 5); // 3 + 2; skipped/failed contribute nothing
    assert_eq!(body["failed"], json!([424242]));
    let got = lc(&c.conn(), 201);
    // z-beta 2+3=5; the alpha key increments the existing z-alpha row (self
    // preferred by ORDER BY), never creating a parallel z-alpha2 row.
    assert!(got.iter().any(|(card, q)| card == "z-beta" && *q == 5), "{got:?}");
    assert!(got.iter().any(|(card, q)| card == "z-alpha" && *q == 3), "{got:?}");
    assert!(!got.iter().any(|(card, _)| card == "z-alpha2"), "no duplicate key rows {got:?}");
}

#[tokio::test]
async fn bulk_add_hydration_forwards_whole_request() {
    let c = ctx("lb-bulk-fwd.db", &["lists_writes"]).await;
    let (s, h, _body) = c
        .call(
            "POST",
            "/api/lists/201/cards/bulk",
            &JSON_CT,
            Some(json!({"cards": [{"card_id": "z-beta", "quantity": 4}, {"card_id": "z-uncached-9", "quantity": 1}]})),
        )
        .await;
    assert_eq!(h.get("x-node").map(|v| v.to_str().unwrap()), Some("1"));
    assert_eq!(s, 200);
    // The read-only pre-pass wrote nothing: Node's replay sees a pristine list.
    let q: i64 = c
        .conn()
        .query_row("SELECT quantity FROM list_cards WHERE list_id = 201 AND card_id = 'z-beta'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(q, 2);
}

#[tokio::test]
async fn bulk_add_all_failed_500() {
    let c = ctx("lb-bulk-fail.db", &["lists_writes"]).await;
    let (s, _, body) = c
        .call("POST", "/api/lists/201/cards/bulk", &JSON_CT, Some(json!({"cards": [{"card_id": 111}, {"card_id": false}]})))
        .await;
    // false is falsy -> skipped (not failed); only 111 fails, added 0 -> 500.
    assert_eq!(s, 500);
    assert_eq!(body["error"], "Failed to add cards to list");
}

// ---------------------------------------------------------------------------
// POST /:id/cards/bulk-remove
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_remove_decrements_and_silently_skips() {
    let c = ctx("lb-bulkrm.db", &["lists_writes"]).await;
    let (s, h, body) = c
        .call(
            "POST",
            "/api/lists/201/cards/bulk-remove",
            &JSON_CT,
            Some(json!({"cards": [
                {"card_id": "z-alpha2", "quantity": 1},   // equivalent row z-alpha qty1 -> DELETE, +1
                {"card_id": "z-beta", "quantity": 1},     // qty 2 -> UPDATE to 1, +1
                {"card_id": "z-gamma"},                    // not in list -> silently skipped
                {"quantity": 9}                            // falsy -> skipped
            ]})),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    assert!(h.get("x-node").is_none());
    assert_eq!(body["removed"], 2);
    assert!(body.get("failed").is_none(), "Node's bulk-remove has no failed array");
    assert_eq!(lc(&c.conn(), 201), vec![("z-beta".into(), 1)]);
}

#[tokio::test]
async fn bulk_remove_gates_and_foreign() {
    let c = ctx("lb-bulkrm-g.db", &["lists_writes"]).await;
    let (s, _, body) = c.call("POST", "/api/lists/201/cards/bulk-remove", &JSON_CT, Some(json!({"cards": []}))).await;
    assert_eq!(s, 400);
    assert_eq!(body["error"], "cards must be 1-250 entries");
    let (s, _, _) = c.call("POST", "/api/lists/204/cards/bulk-remove", &JSON_CT, Some(json!({"cards": [{"card_id": "z-beta"}]}))).await;
    assert_eq!(s, 404);
}

// ---------------------------------------------------------------------------
// POST /:id/remove-from-collection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_from_collection_dry_run_then_real() {
    let c = ctx("lb-rfc.db", &["lists_writes"]).await;
    c.conn().execute_batch(
        "INSERT INTO collection (card_id, quantity, user_id) VALUES
           ('z-alpha', 2, 1), ('z-alpha2', 1, 1), ('z-alpha', 1, 2);",
    )
    .unwrap();
    // list 201 wants 1x z-alpha (own 3 across printings) + 2x z-beta (own 0).
    let (s, h, body) = c.call("POST", "/api/lists/201/remove-from-collection", &JSON_CT, Some(json!({"dry_run": true}))).await;
    assert_eq!(s, 200, "{body}");
    assert!(h.get("x-node").is_none());
    assert_eq!(body["dry_run"], true);
    assert_eq!(body["requested"], 3);
    assert_eq!(body["removed"], 1);
    assert_eq!(
        body["short"],
        json!([{"card_id": "z-beta", "name": "Beta Card", "wanted": 2, "removed": 0}])
    );
    let n: i64 = c.conn().query_row("SELECT COUNT(*) FROM collection WHERE user_id = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "dry_run leaves the collection byte-identical");

    let (s, _, body) = c.call("POST", "/api/lists/201/remove-from-collection", &JSON_CT, Some(json!({}))).await;
    assert_eq!(s, 200);
    assert_eq!(body["dry_run"], false);
    assert_eq!(body["removed"], 1);
    // Exact-printing-first, newest-first: the z-alpha row (q2) decremented to 1.
    let binding = c.conn();
    let mut stmt = binding.prepare("SELECT id, quantity FROM collection WHERE user_id = 1 ORDER BY id").unwrap();
    let got: Vec<(i64, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(got, vec![(1, 1), (2, 1)]);
    // Other user's rows untouched.
    let n: i64 = c.conn().query_row("SELECT COUNT(*) FROM collection WHERE user_id = 2", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn remove_from_collection_deletes_whole_row_when_take_equals_qty() {
    let c = ctx("lb-rfc-del.db", &["lists_writes"]).await;
    c.conn().execute("INSERT INTO collection (card_id, quantity, user_id) VALUES ('z-alpha', 1, 1)", []).unwrap();
    let (s, _, body) = c.call("POST", "/api/lists/201/remove-from-collection", &JSON_CT, Some(json!({"dry_run": "truthy"}))).await;
    assert_eq!(s, 200);
    assert_eq!(body["dry_run"], true, "any truthy value counts");
    let (s, _, _) = c.call("POST", "/api/lists/201/remove-from-collection", &JSON_CT, Some(json!({}))).await;
    assert_eq!(s, 200);
    let n: i64 = c.conn().query_row("SELECT COUNT(*) FROM collection WHERE user_id = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "take >= entry.quantity deletes the row");
}

#[tokio::test]
async fn remove_from_collection_gates() {
    let c = ctx("lb-rfc-gates.db", &["lists_writes"]).await;
    let (s, _, body) = c.call("POST", "/api/lists/204/remove-from-collection", &JSON_CT, Some(json!({"dry_run": true}))).await;
    assert_eq!(s, 404);
    assert_eq!(body["error"], "List not found or unauthorized");
    let (s, _, _) = c.call("POST", "/api/lists/201/remove-from-collection", &JSON_ONLY, None).await;
    assert_eq!(s, 401);
}

// ---------------------------------------------------------------------------
// PUT /:id/cheapest-printings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cheapest_printings_moves_only_misaligned_demands() {
    let c = ctx("lb-cheap.db", &["lists_writes"]).await;
    let (s, h, body) = c.call("PUT", "/api/lists/201/cheapest-printings", &JSON_CT, Some(json!({}))).await;
    assert_eq!(s, 200, "{body}");
    assert!(h.get("x-node").is_none());
    assert_eq!(body["message"], "Cards moved to their cheapest printings");
    // alpha: z-alpha(5.0) -> z-alpha-cheap(0.2). beta: already cheapest (0.5
    // trend) -> `if (!misaligned) continue;` skips it.
    assert_eq!(body["moved"], 1);
    let got = lc(&c.conn(), 201);
    assert_eq!(got.len(), 2);
    assert!(got.iter().any(|(card, q)| card == "z-alpha-cheap" && *q == 1), "{got:?}");
    assert!(got.iter().any(|(card, q)| card == "z-beta" && *q == 2), "{got:?}");
    // Idempotent rerun.
    let (_, _, body) = c.call("PUT", "/api/lists/201/cheapest-printings", &JSON_CT, Some(json!({}))).await;
    assert_eq!(body["moved"], 0);
}

#[tokio::test]
async fn cheapest_printings_unpriced_and_foreign() {
    let c = ctx("lb-cheap2.db", &["lists_writes"]).await;
    // z-gamma has no priced printing: a demand without a target is left alone.
    c.conn().execute("INSERT INTO list_cards (list_id, card_id, quantity) VALUES (201, 'z-gamma', 4)", []).unwrap();
    let (_, _, body) = c.call("PUT", "/api/lists/201/cheapest-printings", &JSON_CT, Some(json!({}))).await;
    assert_eq!(body["moved"], 1);
    let q: i64 = c
        .conn()
        .query_row("SELECT quantity FROM list_cards WHERE list_id = 201 AND card_id = 'z-gamma'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(q, 4);
    let (s, _, body) = c.call("PUT", "/api/lists/204/cheapest-printings", &JSON_CT, Some(json!({}))).await;
    assert_eq!(s, 404);
    assert_eq!(body["error"], "List not found or unauthorized");
}

// ---------------------------------------------------------------------------
// PUT /:id/cards/:card_id/printing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn printing_gates_and_moves() {
    let c = ctx("lb-print.db", &["lists_writes"]).await;
    let (s, _, body) = c.call("PUT", "/api/lists/201/cards/z-alpha/printing", &JSON_CT, Some(json!({}))).await;
    assert_eq!(s, 400);
    assert_eq!(body["error"], "card_id is required");
    let (s, _, body) = c.call("PUT", "/api/lists/201/cards/z-nope/printing", &JSON_CT, Some(json!({"card_id": "z-alpha2"}))).await;
    assert_eq!(s, 404);
    assert_eq!(body["error"], "Card not in this list");
    let (s, _, body) = c.call("PUT", "/api/lists/201/cards/z-alpha/printing", &JSON_CT, Some(json!({"card_id": "z-alpha"}))).await;
    assert_eq!(s, 200);
    assert_eq!(body["message"], "Printing unchanged");
    assert_eq!(body["card_id"], "z-alpha");
    let (s, _, body) = c.call("PUT", "/api/lists/201/cards/z-alpha/printing", &JSON_CT, Some(json!({"card_id": "z-beta"}))).await;
    assert_eq!(s, 400);
    assert_eq!(body["error"], "Pick a printing of the same card");
    // Number body value: falsy gate first, then String() coercion -> "99", a
    // string id not in cache -> hydration forward.
    let (s, h, _) = c.call("PUT", "/api/lists/201/cards/z-alpha/printing", &JSON_CT, Some(json!({"card_id": 99}))).await;
    assert_eq!(h.get("x-node").map(|v| v.to_str().unwrap()), Some("1"));
    assert_eq!(s, 200);
    // The real move.
    let (s, _, body) = c.call("PUT", "/api/lists/201/cards/z-alpha/printing", &JSON_CT, Some(json!({"card_id": "z-alpha2"}))).await;
    assert_eq!(s, 200);
    assert_eq!(body["message"], "Printing changed");
    assert_eq!(body["card_id"], "z-alpha2");
    let got = lc(&c.conn(), 201);
    assert_eq!(got.len(), 2);
    assert!(got.iter().any(|(card, q)| card == "z-alpha2" && *q == 1), "{got:?}");
    assert!(!got.iter().any(|(card, _)| card == "z-alpha"), "old row deleted {got:?}");
}

#[tokio::test]
async fn printing_merges_onto_existing_target_row() {
    let c = ctx("lb-print-merge.db", &["lists_writes"]).await;
    c.conn().execute("INSERT INTO list_cards (list_id, card_id, quantity) VALUES (201, 'z-alpha2', 3)", []).unwrap();
    let (s, _, body) = c.call("PUT", "/api/lists/201/cards/z-alpha/printing", &JSON_CT, Some(json!({"card_id": "z-alpha2"}))).await;
    assert_eq!(s, 200);
    let got = lc(&c.conn(), 201);
    // ON CONFLICT merge: 3 + 1, and the from-row is gone.
    assert!(got.iter().any(|(card, q)| card == "z-alpha2" && *q == 4), "{got:?}");
    assert!(!got.iter().any(|(card, _)| card == "z-alpha"), "{got:?}");
    let _ = body;
}

// ---------------------------------------------------------------------------
// DELETE /:id/cards/:card_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_card_sweeps_equivalent_rows() {
    let c = ctx("lb-del.db", &["lists_writes"]).await;
    c.conn().execute_batch(
        "INSERT INTO list_cards (list_id, card_id, quantity) VALUES (201, 'z-alpha2', 3), (202, 'z-alpha', 5);",
    )
    .unwrap();
    let (s, h, body) = c.call("DELETE", "/api/lists/201/cards/z-alpha", &AUTH, None).await;
    assert_eq!(s, 200, "{body}");
    assert!(h.get("x-node").is_none());
    assert_eq!(body["message"], "Card removed from list");
    assert_eq!(lc(&c.conn(), 201), vec![("z-beta".into(), 2)], "every same-key row in THIS list is gone");
    let n: i64 = c
        .conn()
        .query_row("SELECT COUNT(*) FROM list_cards WHERE list_id = 202 AND card_id = 'z-alpha'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "sibling list untouched");
}

#[tokio::test]
async fn remove_card_gates() {
    let c = ctx("lb-del-g.db", &["lists_writes"]).await;
    let (s, _, body) = c.call("DELETE", "/api/lists/204/cards/z-beta", &AUTH, None).await;
    assert_eq!(s, 404);
    assert_eq!(body["error"], "List not found or unauthorized");
    let (s, _, _) = c.call("DELETE", "/api/lists/201/cards/z-beta", &[], None).await;
    assert_eq!(s, 401);
    // Uncached card id: the equivalent sweep degenerates to the exact row.
    let (s, _, _) = c.call("DELETE", "/api/lists/201/cards/z-uncached-9", &AUTH, None).await;
    assert_eq!(s, 200);
    assert_eq!(lc(&c.conn(), 201).len(), 2, "nothing removed, still 200");
}

// ---------------------------------------------------------------------------
// Kill switch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_and_shape_guards() {
    let dir = tempfile::tempdir().unwrap().keep();
    let db_path = seeded_db(&dir, "lb-kill.db");
    let upstream = spawn_fake_node().await;

    // Group OFF: every card-body shape goes to Node — never a bare 405.
    let off = router_for(db_path.clone(), &["lists_reads"], &url_of(&upstream));
    let shapes: [(&str, &str, Option<Value>); 7] = [
        ("POST", "/api/lists/201/cards", Some(json!({"card_id": "z-gamma"}))),
        ("POST", "/api/lists/201/cards/bulk", Some(json!({"cards": [{"card_id": "z-gamma"}]}))),
        ("POST", "/api/lists/201/cards/bulk-remove", Some(json!({"cards": [{"card_id": "z-gamma"}]}))),
        ("POST", "/api/lists/201/remove-from-collection", Some(json!({"dry_run": true}))),
        ("PUT", "/api/lists/201/cheapest-printings", Some(json!({}))),
        ("PUT", "/api/lists/201/cards/z-alpha/printing", Some(json!({"card_id": "z-alpha2"}))),
        ("DELETE", "/api/lists/201/cards/z-alpha", None),
    ];
    for (method, path, body) in shapes {
        let raw = body.as_ref().map(|b| serde_json::to_string(b).unwrap());
        let (s, h, bytes) = request(&off, method, path, &JSON_CT, raw.as_deref()).await;
        let echoed = json_body(&bytes);
        assert_eq!(s, 200, "{method} {path}: {echoed}");
        assert_eq!(h.get("x-node").map(|v| v.to_str().unwrap()), Some("1"), "{method} {path} must proxy");
        assert_eq!(echoed["method"], method);
        assert_eq!(echoed["pq"], path);
    }

    // Group ON but a method Node's router also lacks on the path
    // (GET on a POST-only path) must forward, never 405/404 from Rust.
    let on = router_for(db_path, &["lists_writes", "lists_reads"], &url_of(&upstream));
    let (s, h, bytes) = request(&on, "GET", "/api/lists/201/cards", &AUTH, None).await;
    assert_eq!(h.get("x-node").map(|v| v.to_str().unwrap()), Some("1"));
    assert_eq!(s, 200, "{:?}", String::from_utf8_lossy(&bytes));
}
