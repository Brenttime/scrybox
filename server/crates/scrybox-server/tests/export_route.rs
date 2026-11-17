//! LANE A — iter-38 route tests for the export group (export.rs).
//!
//! Pinned: attachment bytes + Content-Type/Disposition + Express default
//! body ETag (+304) on the two export reads, the /import JSON contract
//! (key ORDER included), every rejection list verbatim, and the whole-
//! request forward policy (fake Node answers with x-node: 1).
//!
//! The seeded shape mirrors what parity.mjs pins against the live Node
//! container: a proxy row (market_price = Number(price_trend||0)||0) and a
//! non-proxy row (market_price key ABSENT from JSON, 0 in CSV).

mod mock_scryfall;

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{any, get},
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
        out.insert(
            axum::http::HeaderName::from_static("x-node"),
            axum::http::HeaderValue::from_static("1"),
        );
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
    let health = get(|| async { (StatusCode::OK, axum::Json(json!({"status":"ok"}))) });
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
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'adm','x','admin','tok-a',0,NULL),
                  (2,'mem','x','member','tok-m',0,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES
           ('sess-admin', 1, '2099-01-01 00:00:00'),
           ('sess-member', 2, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id, name, supertype, types, rarity, set_id, set_name, number, image_url, price_trend, price_normal, price_holofoil)
           VALUES ('mtg-aaa1','Test Card','Creature','Creature','Rare','10e','Tenth Edition','1','http://img/1',2.5,2.0,3.0),
                  ('mtg-aaa2','Proxy Card','Creature','Creature','Common','10e','Tenth Edition','2','http://img/2',1.25,1.0,1.5),
                  ('not-mtg','Not An MTG Row',NULL,'Creature','Common','10e','Tenth Edition','3',NULL,NULL,NULL,NULL);
         INSERT INTO collection (id, card_id, quantity, condition, printing, language, purchase_price, added_at, user_id, is_proxy)
           VALUES (900001,'mtg-aaa1',2,'Near Mint','Normal','English',1.5,'2026-01-03 10:00:00',2,0),
                  (900002,'mtg-aaa2',1,'Damaged','Holofoil',NULL,NULL,'2026-01-02 10:00:00',2,1);",
    )
    .unwrap();
    path
}

struct Fix {
    router: Router,
    scry: Arc<mock_scryfall::MockState>,
    db_path: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture(routes: &[&str]) -> Fix {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), "export.db");
    let node = spawn_fake_node().await;
    let (scry_addr, scry) = mock_scryfall::spawn_mock().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.scry = Arc::new(mock_scryfall::scry_for(scry_addr, 0.0));
    Fix { router: app(st), scry, db_path: db, _tmp: tmp }
}

async fn request(
    r: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
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

const MEMBER: [(&str, &str); 1] = [("authorization", "Bearer sess-member")];
const MEMBER_JSON: [(&str, &str); 2] =
    [("authorization", "Bearer sess-member"), ("content-type", "application/json")];

// ---------------------------------------------------------------------------
// GET /api/export
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_export_csv_internal_bytes_and_headers() {
    let f = fixture(&["export"]).await;
    let (st, hd, body) = request(&f.router, "GET", "/api/export", &MEMBER, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["content-type"], "text/csv; charset=utf-8");
    assert_eq!(hd["content-disposition"], "attachment; filename=scrybox_collection_internal.csv");
    let etag = hd["etag"].to_str().unwrap().to_string();
    assert!(etag.starts_with("W/\""), "express default body tag, got {etag}");
    let text = String::from_utf8(body).unwrap();
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(
        lines[0],
        "Card ID,Name,Set Name,Set ID,Card Number,Rarity,Quantity,Condition,Printing,Language,Purchase Price,Market Price,Added At"
    );
    // row order = plain SELECT (no ORDER BY) — rowid order for this fixture.
    assert_eq!(
        lines[1],
        "\"mtg-aaa1\",\"Test Card\",\"Tenth Edition\",\"10e\",\"1\",\"Rare\",\"2\",\"Near Mint\",\"Normal\",\"English\",\"1.5\",\"2\",\"2026-01-03 10:00:00\""
    );
    // price columns: strategy reads price_trend||market_price||0 on the
    // exportRows output (price_* destructured AWAY → market_price wins):
    // mtg-aaa1 Normal + price_normal 2.0 → 2; proxy row → resolveCardPrice
    // proxy branch → 0. null language → 'English', null purchase_price → 0.
    assert_eq!(
        lines[2],
        "\"mtg-aaa2\",\"Proxy Card\",\"Tenth Edition\",\"10e\",\"2\",\"Common\",\"1\",\"Damaged\",\"Holofoil\",\"English\",\"0\",\"0\",\"2026-01-02 10:00:00\""
    );
    // 304 replay of our own tag: no body, keeps disposition + etag, no CT.
    let inm: Vec<(&str, &str)> =
        MEMBER.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>();
    let (st2, hd2, body2) = {
        let mut rb = Request::builder().method("GET").uri("/api/export");
        for &(k, v) in &inm {
            rb = rb.header(k, v);
        }
        rb = rb.header("if-none-match", etag.as_str());
        let req = rb.body(Body::empty()).unwrap();
        let resp = f.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
        (status, headers, bytes)
    };
    assert_eq!(st2, StatusCode::NOT_MODIFIED);
    assert!(body2.is_empty());
    assert_eq!(hd2["etag"], etag.as_str());
    assert_eq!(hd2["content-disposition"], "attachment; filename=scrybox_collection_internal.csv");
    assert!(hd2.get("content-type").is_none());
}

#[tokio::test]
async fn get_export_json_key_order_and_market_price_omission() {
    let f = fixture(&["export"]).await;
    let (st, hd, body) =
        request(&f.router, "GET", "/api/export?format=json", &MEMBER, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["content-type"], "application/json; charset=utf-8");
    assert_eq!(hd["content-disposition"], "attachment; filename=scrybox_collection_internal.json");
    let v = json_body(&body);
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // destructure order: SELECT columns minus entry_id/price_* + market_price
    // (resolveCardPrice ALWAYS returns → the key is always present).
    let keys: Vec<&str> = rows[0].as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "quantity",
            "condition",
            "printing",
            "language",
            "purchase_price",
            "added_at",
            "is_proxy",
            "card_id",
            "name",
            "supertype",
            "types",
            "rarity",
            "set_code",
            "set_name",
            "collector_number",
            "image_url",
            "market_price"
        ]
    );
    // mtg-aaa1: Normal + price_normal 2.0 → 2 (price_trend 2.5 NOT used).
    assert_eq!(rows[0]["market_price"], json!(2));
    // proxy row: is_proxy truthy → 0 whatever the prices.
    assert_eq!(rows[1]["market_price"], json!(0));
    assert_eq!(rows[1]["is_proxy"], json!(1));
    // entry_id must be gone from BOTH formats.
    assert!(rows[0].get("entry_id").is_none());
}

#[tokio::test]
async fn get_export_ecosystem_selects_target_format() {
    let f = fixture(&["export"]).await;
    // ecosystem drives targetFormat (and the filename), format drives the
    // branch: format=csv + ecosystem=json → CSV bytes named .json? No —
    // sendExport picks the branch by FORMAT, so this is CSV with the csv
    // name; ecosystem=json only renames CSV content.
    let (st, hd, body) =
        request(&f.router, "GET", "/api/export?ecosystem=json&format=csv", &MEMBER, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["content-disposition"], "attachment; filename=scrybox_collection_json.csv");
    assert!(String::from_utf8_lossy(&body).starts_with("Card ID,"));
    // ecosystem=manabox renames + reverse-maps the condition column.
    let (_, hd, body) =
        request(&f.router, "GET", "/api/export?ecosystem=MANABOX", &MEMBER, None).await;
    assert_eq!(hd["content-disposition"], "attachment; filename=scrybox_collection_manabox.csv");
    let text = String::from_utf8_lossy(&body);
    assert!(text.starts_with("Name,Set code,Card number,Quantity,Condition,Foil\n"));
    assert!(text.contains("\"near_mint\""), "{text}");
    // empty ecosystem falls through to format; empty format → 'internal'.
    let (_, hd, _) =
        request(&f.router, "GET", "/api/export?ecosystem=&format=", &MEMBER, None).await;
    assert_eq!(hd["content-disposition"], "attachment; filename=scrybox_collection_internal.csv");
}

#[tokio::test]
async fn get_export_requires_auth_and_proxies_other_methods() {
    let f = fixture(&["export"]).await;
    let (st, _, body) = request(&f.router, "GET", "/api/export", &[], None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(&body)["error"], json!("Access denied. No token provided."));
    // DELETE on an owned path: handler self-checks, whole request proxies.
    let (st, hd, _) = request(&f.router, "DELETE", "/api/export", &MEMBER, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
}

// ---------------------------------------------------------------------------
// POST /api/export
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_export_view_order_dedup_and_validation() {
    let f = fixture(&["export"]).await;
    // reversed order + duplicate id → dedup, request order kept.
    let (st, hd, body) = request(
        &f.router,
        "POST",
        "/api/export",
        &MEMBER_JSON,
        Some(r#"{"entry_ids":[900002,900001,900002]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["content-disposition"], "attachment; filename=scrybox_view_internal.csv");
    let text = String::from_utf8_lossy(&body);
    let lines: Vec<&str> = text.split('\n').collect();
    assert!(lines[1].starts_with("\"mtg-aaa2\""), "reversed order: {text}");
    assert!(lines[2].starts_with("\"mtg-aaa1\""));
    assert_eq!(lines.len(), 3);

    // string ids → Number() coercion keeps '900001'.
    let (_, _, body) = request(
        &f.router,
        "POST",
        "/api/export",
        &MEMBER_JSON,
        Some(r#"{"entry_ids":["900001", 999999]}"#),
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(text.lines().count(), 2, "only 900001 survives: {text}");

    // non-array → 400 non-empty array
    let (st, _, body) =
        request(&f.router, "POST", "/api/export", &MEMBER_JSON, Some(r#"{}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], json!("entry_ids must be a non-empty array"));
    // Node FILTERS non-integers (map Number, filter isInteger): null→0 and
    // true→1 SURVIVE (no rows match → empty CSV), {} drops. 400 only when
    // the filtered list is EMPTY.
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/export",
        &MEMBER_JSON,
        Some(r#"{"entry_ids":["a",null,true,{}]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.is_empty(), "rows [] → res.send(''): {body:?}");
    let (st2, _, body2) = request(
        &f.router,
        "POST",
        "/api/export",
        &MEMBER_JSON,
        Some(r#"{"entry_ids":["a",{}]}"#),
    )
    .await;
    assert_eq!(st2, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body2)["error"], json!("entry_ids must be integers"));

    // format:json branch keyed by body.format; ecosystem String() coercion.
    let (st, hd, body) = request(
        &f.router,
        "POST",
        "/api/export",
        &MEMBER_JSON,
        Some(r#"{"entry_ids":[900001],"format":"JSON","ecosystem":5}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["content-type"], "application/json; charset=utf-8");
    assert_eq!(hd["content-disposition"], "attachment; filename=scrybox_view_5.json");
    assert_eq!(json_body(&body).as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// POST /api/import
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_validation_rejections() {
    let f = fixture(&["export"]).await;
    // no token → 401 BEFORE the big body parse (gate order, server.js).
    let (st, _, _) = request(
        &f.router,
        "POST",
        "/api/import",
        &[("content-type", "application/json")],
        Some(r#"{"data":[]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // no data
    let (st, _, body) =
        request(&f.router, "POST", "/api/import", &MEMBER_JSON, Some("{}")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], json!("No data provided"));
    let (st, _, body) =
        request(&f.router, "POST", "/api/import", &MEMBER_JSON, Some(r#"{"data":""}"#))
            .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], json!("No data provided"));

    // json mode, non-array payload
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":{}}"#),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], json!("Invalid data payload"));

    // csv with one line → empty
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"tcgplayer","data":"Card Name\n"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], json!("CSV file is empty or missing headers"));

    // csv: rows go through the strategy ONCE, then the Scryfall gate —
    // unrecognized names all rejected, verbatim reason.
    *f.scry.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": 1})];
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(
            r#"{"format":"tcgplayer","data":"Card Name,Quantity\nLightning Bolt,2\n\"Delver of Secrets\",1"}"#,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let v = json_body(&body);
    assert_eq!(
        v["error"],
        json!("Import rejected: 2 of 2 row(s) are not valid MTG printings. No rows were imported.")
    );
    assert_eq!(
        v["rejected"][0],
        json!({"index":0,"reason":"card was not recognized by Scryfall as an MTG printing"})
    );
    assert_eq!(
        v["rejected"][1],
        json!({"index":1,"reason":"card was not recognized by Scryfall as an MTG printing"})
    );

    // json: null item → the V8 property-access message, verbatim.
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":[null]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    let v = json_body(&body);
    assert_eq!(v["error"], json!("Import failed"));
    assert_eq!(v["message"], json!("Cannot read properties of null (reading 'game')"));

    // format null / non-string → pinned toLowerCase throws.
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":null,"data":"a,b\nc,d"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&body)["message"], json!("Cannot read properties of null (reading 'toLowerCase')"));
    let (_, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":5,"data":"a,b\nc,d"}"#),
    )
    .await;
    assert_eq!(json_body(&body)["message"], json!("format.toLowerCase is not a function"));

    // json mode with a STRING data that fails JSON.parse → forwarded (V8
    // parse-error text is unreproducible).
    let (st, hd, _) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":"zzz"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
}

#[tokio::test]
async fn import_rejects_bad_game_and_printing_shapes() {
    let f = fixture(&["export"]).await;
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(
            r#"{"format":"json","data":[{"game":"Pokemon","name":"X"},{"name":"Y","printing":"Foil-ish"},{"printing":"null","name":"Z"}]}"#,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let v = json_body(&body);
    assert_eq!(v["error"], json!("Import rejected: 3 of 3 row(s) are not valid MTG printings. No rows were imported."));
    assert_eq!(
        v["rejected"][0]["reason"],
        json!("game 'Pokemon' is not supported (this install is MTG-only)")
    );
    assert_eq!(v["rejected"][1]["reason"], json!("printing 'Foil-ish' is not supported"));
    assert_eq!(v["rejected"][2]["reason"], json!("printing 'null' is not supported"));

    // no identifier at all (name '' + no id/set/number) → identifier gate.
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":[{"game":null,"name":""}]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&body)["rejected"][0]["reason"],
        json!("card has no MTG identifier Scryfall can validate")
    );
}

#[tokio::test]
async fn import_success_cache_hit_writes_and_replies() {
    let f = fixture(&["export"]).await;
    let (st, hd, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(
            r#"{"format":"json","data":[{"game":"mtg","name":"Test Card","quantity":3,"condition":"Lightly Played","printing":"Normal","language":"English","purchase_price":0.5,"card_id":"mtg-aaa1"}]}"#,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v = json_body(&body);
    // byte shape: key order success → count → message.
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(keys, vec!["success", "count", "message"]);
    assert_eq!(v["success"], json!(true));
    assert_eq!(v["count"], json!(1));
    assert_eq!(v["message"], json!("Successfully imported 1 items."));
    assert!(hd["etag"].to_str().unwrap().starts_with("W/\""));
    assert_eq!(hd["content-type"], "application/json; charset=utf-8");

    // raw-item binds, not the mapped cardData: quantity 3, condition as given.
    let conn = Connection::open(&f.db_path).unwrap();
    let (q, c, pp, added): (i64, String, f64, String) = conn
        .query_row(
            "SELECT quantity, condition, purchase_price, added_at FROM collection WHERE card_id='mtg-aaa1' AND quantity=3",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((q, c.as_str(), pp), (3, "Lightly Played", 0.5));
    assert!(!added.is_empty(), "CURRENT_TIMESTAMP must be set");
    // no Scryfall traffic for cache hits
    assert!(f.scry.hits_for("POST /cards/collection").is_empty());
    // the cached card_cache row is used VERBATIM — never rewritten by
    // cacheCards (Node only caches bulk-resolved matches).
    let name: String = conn
        .query_row("SELECT name FROM card_cache WHERE id='mtg-aaa1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "Test Card");
}

#[tokio::test]
async fn import_success_uncached_goes_through_shared_bulk_resolver() {
    let f = fixture(&["export"]).await;
    *f.scry.collection_batches.lock().unwrap() = vec![json!({
        "data": [mock_scryfall::raw_card("bbb-1", "Fresh Card", "10e", "5", Some(1.0))],
        "not_found": 0,
    })];
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":[{"game":"mtg","name":"Fresh Card","set_code":"10e","number":"5"}]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&body)["count"], json!(1));
    // one bulk call per uncached row, sn: key from set_code/number
    let hits = f.scry.hits_for("POST /cards/collection");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].1.contains("\"set\":\"10e\"") && hits[0].1.contains("\"collector_number\":\"5\""), "{}", hits[0].1);
    // cacheCards wrote the normalized card (mtg- prefixed) + the entry row.
    let conn = Connection::open(&f.db_path).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM card_cache WHERE id='mtg-bbb-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM collection WHERE card_id='mtg-bbb-1' AND user_id=2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn import_rejects_unrecognized_and_non_mtg_cached() {
    let f = fixture(&["export"]).await;
    // unrecognized (empty collection response)
    *f.scry.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": 1})];
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":[{"game":"mtg","name":"Nope Card"}]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&body)["rejected"][0]["reason"],
        json!("card was not recognized by Scryfall as an MTG printing")
    );

    // cached row whose id is not mtg- prefixed
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":[{"game":"mtg","name":"X","card_id":"not-mtg"}]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&body)["rejected"][0]["reason"],
        json!("cached card id is not an MTG printing")
    );

    // 'Foil' is NOT on the allowed list (['Normal','Holofoil'] only) →
    // gate rejection, not a CHECK violation — nothing reaches the DB.
    let (st, _, body) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":[{"game":"mtg","name":"Foil Card","printing":"Foil"}]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&body)["rejected"][0]["reason"],
        json!("printing 'Foil' is not supported")
    );
    let conn = Connection::open(&f.db_path).unwrap();
    let n: i64 =
        conn.query_row("SELECT COUNT(*) FROM collection WHERE card_id='mtg-ccc-1'", [], |r| r.get(0))
            .unwrap();
    assert_eq!(n, 0, "rejected rows never reach the DB");
}

#[tokio::test]
async fn import_bulk_client_error_forwards_whole_request() {
    let f = fixture(&["export"]).await;
    *f.scry.statuses.lock().unwrap() =
        std::collections::HashMap::from([("POST /cards/collection".to_string(), vec![500u16])]);
    let (st, hd, _) = request(
        &f.router,
        "POST",
        "/api/import",
        &MEMBER_JSON,
        Some(r#"{"format":"json","data":[{"game":"mtg","name":"Retry Card"}]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
}

#[tokio::test]
async fn off_switch_proxies_everything() {
    let f = fixture(&[]).await;
    let (st, hd, _) = request(&f.router, "GET", "/api/export", &MEMBER, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
    let (st, hd, _) =
        request(&f.router, "POST", "/api/import", &MEMBER_JSON, Some("{}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
}
