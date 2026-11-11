//! Path-level tests for the iter-32 group: `lists_writes`
//! (POST /api/lists, PUT /api/lists/order, PUT /api/lists/:id,
//! DELETE /api/lists/:id).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (lw_* cases). These tests
//! pin the contract locally: the parseListLine/parseListText twins against
//! the Node-oracle fixture table generated from the real utils/listImport.js;
//! create (defaults, icon clean, qty stacking, name coercion shapes);
//! the Scryfall-hydration forward (uncached name -> whole request to Node,
//! ZERO local rows); update (binding shapes, accent TypeError-500 twin,
//! changes==0 -> 404); delete (cascade-twin removes list_cards);
//! order (Number-filter survival table, post-filter index, 400 gates);
//! kill-switch proxying and the never-405 dispatcher.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use rusqlite::{types::Value as SqlValue, Connection};
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
             ('z-beta','Beta Card','MTG','[\"Instant\"]','[\"Blue\"]','Common','lrp','LR Promos','42',null,0.5,0.5,0,0,0,1,'[\"U\"]','USD');
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

// ---------------------------------------------------------------------------
// parseListLine / parseListText — Node-oracle table
// ---------------------------------------------------------------------------

#[test]
fn parse_list_line_matches_node_oracle() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/list_import_oracle.json")).unwrap();
    for case in fixture["lines"].as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        let parsed = scrybox_server::lists_writes::parse_list_line(input);
        match &case["out"] {
            Value::Null => assert!(parsed.is_none(), "expected null for {input:?}, got {parsed:?}"),
            out => {
                let p = parsed.unwrap_or_else(|| panic!("expected Some for {input:?}"));
                assert_eq!(p.name, out["name"].as_str().unwrap(), "name for {input:?}");
                let want = out["qty"].as_f64().unwrap();
                let got = match &p.qty {
                    SqlValue::Integer(i) => *i as f64,
                    SqlValue::Real(f) => *f,
                    other => panic!("bad qty binding {other:?} for {input:?}"),
                };
                assert_eq!(got, want, "qty for {input:?}");
                assert_eq!(p.set_ref, out["setRef"].as_str().unwrap(), "setRef for {input:?}");
                assert_eq!(p.set, out["set"].as_str().unwrap(), "set for {input:?}");
                assert_eq!(p.number, out["number"].as_str().unwrap(), "number for {input:?}");
            }
        }
    }
}

#[test]
fn parse_list_text_matches_node_oracle() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/list_import_oracle.json")).unwrap();
    for case in fixture["text"].as_array().unwrap() {
        if case["in"].is_null() {
            // parseListText(null): String(text || '') -> '' -> no lines.
            assert!(scrybox_server::lists_writes::parse_list_text("").is_empty());
            assert_eq!(case["out"].as_array().unwrap().len(), 0);
            continue;
        }
        let input = case["in"].as_str().unwrap();
        let parsed = scrybox_server::lists_writes::parse_list_text(input);
        let out = case["out"].as_array().unwrap();
        assert_eq!(parsed.len(), out.len(), "line count for {input:?}");
        for (p, want) in parsed.iter().zip(out) {
            assert_eq!(p.name, want["name"].as_str().unwrap(), "name in {input:?}");
            let w = want["qty"].as_f64().unwrap();
            let g = match &p.qty {
                SqlValue::Integer(i) => *i as f64,
                SqlValue::Real(f) => *f,
                _ => f64::NAN,
            };
            assert_eq!(g, w, "qty in {input:?}");
            assert_eq!(p.set_ref, want["setRef"].as_str().unwrap(), "setRef in {input:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/lists
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_basic_cached_defaults_and_stacking() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "create.db");
    let upstream = spawn_fake_node().await;
    let r = router_for(db.clone(), &["lists_writes"], &format!("http://{upstream}"));
    let body = json!({
        "name": "  Buylist  ",
        "list_text": "2 Alpha Card\n3 Alpha Card\n1 Beta Card (lrp) 42\nnot-a-real-card-xyz"
    });
    // 'not-a-real-card-xyz' is uncached -> the WHOLE request must forward
    // and no row may be created locally.
    let (s, h, b) = request(&r, "POST", "/api/lists", &JSON_CT, Some(&body.to_string())).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["body"], body.to_string(), "forwarded body must be verbatim");
    let conn = open(&db);
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM card_lists WHERE name='Buylist'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "forwarded create must not insert locally");

    // All-cached variant answers natively with the Node contract.
    let body = json!({
        "name": " Buylist ",
        "list_text": "2 Alpha Card\n3 Alpha Card\n1 Beta Card (lrp) 42",
        "icon": "Bad Icon!",
        "accent_color": "nope"
    });
    let (s, _h, b) = request(&r, "POST", "/api/lists", &JSON_CT, Some(&body.to_string())).await;
    assert_eq!(s, StatusCode::CREATED);
    let j = json_body(&b);
    assert_eq!(j["message"], "List created");
    assert_eq!(j["matched"], 3, "matched counts LINES, not distinct cards (Node matched++)");
    assert_eq!(j["lines"], 3);
    assert_eq!(j["unmatched"], json!([]));
    let id = j["id"].as_i64().expect("numeric id");
    let row: (String, String, String, Option<String>) = conn
        .query_row(
            "SELECT name, description, accent_color, icon FROM card_lists WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.0, "Buylist", "String(name).trim()");
    assert_eq!(row.1, "", "description || '' default");
    assert_eq!(row.2, "#10b981", "non-# accent falls back to default");
    assert_eq!(row.3, None, "cleanIcon rejects 'Bad Icon!'");
    let qty: i64 = conn
        .query_row("SELECT quantity FROM list_cards WHERE list_id=?1 AND card_id='z-alpha'", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(qty, 5, "ON CONFLICT stacks 2+3 on the same card");
    let beta: i64 = conn
        .query_row("SELECT quantity FROM list_cards WHERE list_id=?1 AND card_id='z-beta'", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(beta, 1, "setRef tail strips '(lrp) 42' off the name");

    // icon kept when it matches ^[a-z0-9-]{1,32}$; explicit accent kept.
    let body = json!({"name": "X", "icon": "good-icon-1", "accent_color": "#abcdef"});
    let (s, _, b) = request(&r, "POST", "/api/lists", &JSON_CT, Some(&body.to_string())).await;
    assert_eq!(s, StatusCode::CREATED);
    let id = json_body(&b)["id"].as_i64().unwrap();
    let (icon, accent): (Option<String>, String) = conn
        .query_row("SELECT icon, accent_color FROM card_lists WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(icon.as_deref(), Some("good-icon-1"));
    assert_eq!(accent, "#abcdef");

    // name coercions: 42 -> '42' and {} -> '[object Object]' are truthy
    // non-empty after String() — Node creates the row, not 400.
    let (s, _, b) = request(&r, "POST", "/api/lists", &JSON_CT, Some("{\"name\":42}")).await;
    assert_eq!(s, StatusCode::CREATED);
    let id = json_body(&b)["id"].as_i64().unwrap();
    let name: String = conn
        .query_row("SELECT name FROM card_lists WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "42");
}

#[tokio::test]
async fn create_errors_auth_and_parse_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "create-err.db");
    let upstream = spawn_fake_node().await;
    let r = router_for(db, &["lists_writes"], &format!("http://{upstream}"));
    for bad in ["{}", "{\"name\":\"\"}", "{\"name\":\"   \"}", "{\"name\":false}", "{\"name\":null}"] {
        let (s, _h, b) = request(&r, "POST", "/api/lists", &JSON_CT, Some(bad)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(json_body(&b)["error"], "List name is required");
    }
    // auth outranks the name gate; malformed JSON outranks auth (express.json
    // only engages with a JSON content-type — live-pinned on the decks twin).
    let (s, _, _) = request(&r, "POST", "/api/lists", &[], Some("{}")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = request(&r, "POST", "/api/lists", &JSON_ONLY, Some("{oops")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
}

// ---------------------------------------------------------------------------
// PUT /api/lists/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_binds_like_node_and_gates() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "update.db");
    let upstream = spawn_fake_node().await;
    let r = router_for(db.clone(), &["lists_writes"], &format!("http://{upstream}"));
    let (s, _h, b) = request(
        &r,
        "PUT",
        "/api/lists/201",
        &JSON_CT,
        Some("{\"name\":\"Renamed \",\"description\":5,\"accent_color\":\"#0f0\",\"icon\":\"ab-1\"}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({"message": "List updated"}));
    let conn = open(&db);
    let row: (String, String, String, Option<String>) = conn
        .query_row(
            "SELECT name, description, accent_color, icon FROM card_lists WHERE id=201",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.0, "Renamed");
    assert_eq!(row.1, "5", "raw 5 binds through TEXT affinity like node-sqlite3");
    assert_eq!(row.2, "#0f0");
    assert_eq!(row.3.as_deref(), Some("ab-1"));

    // accent_color != null && startsWith('#'): present non-string throws
    // TypeError in Node -> 500 with the route message. null -> NULL column.
    let (s, _, b) = request(&r, "PUT", "/api/lists/201", &JSON_CT, Some("{\"name\":\"R\",\"accent_color\":5}")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b)["error"], "Failed to update list");
    let (s, _, _) = request(&r, "PUT", "/api/lists/201", &JSON_CT, Some("{\"name\":\"R\",\"accent_color\":null}")).await;
    assert_eq!(s, StatusCode::OK);
    let accent: Option<String> = conn
        .query_row("SELECT accent_color FROM card_lists WHERE id=201", [], |r| r.get(0))
        .unwrap();
    assert_eq!(accent, None, "!= null guard -> NULL column");
    // description null/undefined -> ''; falsy-but-not-null (0) binds raw.
    let (s, _, _) = request(&r, "PUT", "/api/lists/201", &JSON_CT, Some("{\"name\":\"R\",\"description\":0}")).await;
    assert_eq!(s, StatusCode::OK);
    let desc: String = conn.query_row("SELECT description FROM card_lists WHERE id=201", [], |r| r.get(0)).unwrap();
    assert_eq!(desc, "0");

    let (s, _, b) = request(&r, "PUT", "/api/lists/204", &JSON_CT, Some("{\"name\":\"steal\"}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "changes==0 -> 404");
    assert_eq!(json_body(&b)["error"], "List not found or unauthorized");
    let (s, _, _) = request(&r, "PUT", "/api/lists/201", &JSON_CT, Some("{\"name\":\"\"}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, _) = request(&r, "PUT", "/api/lists/201", &JSON_ONLY, Some("{oops")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "parse outranks auth");
    let (s, _, _) = request(&r, "PUT", "/api/lists/201", &[], Some("{}")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// DELETE /api/lists/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_cascades_list_cards_twin() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "delete.db");
    let upstream = spawn_fake_node().await;
    let r = router_for(db.clone(), &["lists_writes"], &format!("http://{upstream}"));
    let (s, _h, b) = request(&r, "DELETE", "/api/lists/201", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({"message": "List deleted"}));
    let conn = open(&db);
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM card_lists WHERE id=201", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM list_cards WHERE list_id=201", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "Node's FK cascade must be mirrored explicitly");
    let (s, _, b) = request(&r, "DELETE", "/api/lists/204", &AUTH, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b)["error"], "List not found or unauthorized");
    let (s, _, _) = request(&r, "DELETE", "/api/lists/202", &[], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = request(&r, "DELETE", "/api/lists/202", &JSON_ONLY, Some("{oops")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "express.json precedes auth even for DELETE");
}

// ---------------------------------------------------------------------------
// PUT /api/lists/order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn order_number_filter_and_index() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "order.db");
    let upstream = spawn_fake_node().await;
    let r = router_for(db.clone(), &["lists_writes"], &format!("http://{upstream}"));
    // Number() survival: '202'->202 kept, 4.5 dies, null->0 KEPT, true->1
    // KEPT, {} NaN dies, [7]->'7'->7 kept. sort_order = POST-FILTER index.
    let (s, _h, b) = request(
        &r,
        "PUT",
        "/api/lists/order",
        &JSON_CT,
        Some("{\"ids\":[\"202\",4.5,null,true,{ },[7],\"201\"]}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({"ok": true}));
    let conn = open(&db);
    let so202: i64 = conn.query_row("SELECT sort_order FROM card_lists WHERE id=202", [], |r| r.get(0)).unwrap();
    let so201: i64 = conn.query_row("SELECT sort_order FROM card_lists WHERE id=201", [], |r| r.get(0)).unwrap();
    assert_eq!(so202, 0, "202 kept at post-filter index 0");
    assert_eq!(so201, 4, "201 at index 4: survivors 202, null->0, true->1, [7]->7, 201; 4.5 and obj dropped");

    for bad in ["{}", "{\"ids\":[]}", "{\"ids\":\"x\"}", "{\"ids\":{}}", "{\"ids\":[4.5]}"] {
        let (s, _h, b) = request(&r, "PUT", "/api/lists/order", &JSON_CT, Some(bad)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(json_body(&b)["error"], "ids must be 1-1000 list ids");
    }
    let many: Vec<i64> = (1..=1001).collect();
    let body = json!({"ids": many}).to_string();
    let (s, _, _) = request(&r, "PUT", "/api/lists/order", &JSON_CT, Some(&body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, ">1000 ids");
    let (s, _, _) = request(&r, "PUT", "/api/lists/order", &[], Some("{}")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = request(&r, "PUT", "/api/lists/order", &JSON_ONLY, Some("{oops")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
}

// ---------------------------------------------------------------------------
// Routing / kill-switch / never-405
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_and_shape_guards() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    // Group off: everything proxies untouched.
    let r = router_for(seeded_db(dir.path(), "ks.db"), &[], &format!("http://{upstream}"));
    for (m, p, b) in [
        ("POST", "/api/lists", Some("{\"name\":\"x\"}")),
        ("PUT", "/api/lists/201", Some("{\"name\":\"x\"}")),
        ("PUT", "/api/lists/order", Some("{\"ids\":[201]}")),
        ("DELETE", "/api/lists/201", None),
    ] {
        let (_s, h, _) = request(&r, m, p, &JSON_CT, b).await;
        assert_eq!(h.get("x-node").unwrap(), "1", "{m} {p}");
    }
    // Writes on, reads OFF: GETs must proxy (native GET only with lists_reads).
    // LANE-A iter-33: the card-body write shapes below moved from proxy to
    // native with lists_body_writes (see tests/lists_body_writes.rs).
    let r = router_for(seeded_db(dir.path(), "w-only.db"), &["lists_writes"], &format!("http://{upstream}"));
    for (m, p) in [
        ("GET", "/api/lists"),
        ("GET", "/api/lists/201"),
        ("GET", "/api/lists/order"),
        ("GET", "/api/lists/card-matches?q=sol"),
        ("PATCH", "/api/lists/201"),
        ("POST", "/api/lists/201"),
        ("POST", "/api/lists/order"),
    ] {
        let (_s, h, _) = request(&r, m, p, &JSON_CT, Some("{}")).await;
        assert_eq!(h.get("x-node").unwrap(), "1", "{m} {p} must proxy, never 405/404 here");
    }
    // Card-body writes are native as soon as lists_writes is on.
    for (m, p) in [
        ("POST", "/api/lists/201/cards"),
        ("PUT", "/api/lists/201/cheapest-printings"),
        ("DELETE", "/api/lists/201/cards/z-alpha"),
    ] {
        let (_s, h, _) = request(&r, m, p, &JSON_CT, Some("{}")).await;
        assert!(h.get("x-node").is_none(), "{m} {p} must be native under lists_writes");
    }
    // Both on: GET answers natively (no x-node), unowned sub-paths still proxy.
    let r = router_for(
        seeded_db(dir.path(), "both.db"),
        &["lists_reads", "lists_writes"],
        &format!("http://{upstream}"),
    );
    let (s, h, _) = request(&r, "GET", "/api/lists", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-node").is_none(), "GET collection must be native when both groups on");
    let (_s, h, _) = request(&r, "PUT", "/api/lists/201/cheapest-printings", &JSON_CT, Some("{}")).await;
    assert!(h.get("x-node").is_none(), "cheapest-printings is native under lists_writes");
}
