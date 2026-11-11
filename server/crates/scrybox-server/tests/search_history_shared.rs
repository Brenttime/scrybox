//! Path-level tests for the iter-27 groups: `search_history`
//! (GET/POST/DELETE /api/search-history) and `shared`
//! (public GET /api/shared/:token).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (shx_*/shrd_* cases,
//! including the cross-side deflate-raw read-back). These tests pin the
//! contract locally: kill-switch proxying, the auth gate (401/403), the
//! parse-before-auth ORDER (malformed JSON POST answers 500 without a
//! token), the merge/dedupe/sort rules, blob persistence readable
//! independently, Express default ETag 304 replay, and the shared route's
//! key order / proxy-rarity / stats fold / qs quirks on a deterministic
//! fixture.

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
        "CREATE TABLE search_history (
            user_id INTEGER PRIMARY KEY,
            data BLOB NOT NULL,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
         );
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-public',1,NULL),
                  (2,'u2','x','member','tok-off',0,'k59'),
                  (3,'u3','x','member','tok-empty',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id,name,supertype,subtypes,types,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_estimated)
           VALUES ('c-holo','Holo Card','Creature','[\"Bird\"]','[\"Creature\",\"Bird\"]','Rare','who','Doctor Who','57',null,0.32,0.32,3.91,0),
                  ('c-norm','Norm Card','Creature','[\"Cat\"]','[\"Creature\",\"Cat\"]','Common','dmu','Dominaria','43',null,0.12,0.12,0.10,0),
                  ('c-null','Null Card',null,null,null,null,null,null,null,null,null,null,null,0);
         -- added_at DESC order: holo(2x, trade=0), norm(1x trade=1), norm-proxy(3x), null-price
         INSERT INTO collection (id,card_id,quantity,condition,printing,language,added_at,is_trade,user_id,is_proxy)
           VALUES (1,'c-holo',2,'Near Mint','Holofoil','English','2026-01-04 10:00:00',0,1,0),
                  (2,'c-norm',1,'Lightly Played','Normal','English','2026-01-03 10:00:00',1,1,0),
                  (3,'c-norm',3,'Near Mint','Normal','Japanese','2026-01-02 10:00:00',0,1,1),
                  (4,'c-null',4,'Near Mint','Normal','English','2026-01-01 10:00:00',0,1,0);
         INSERT INTO collection_read_revision (id, revision) VALUES (1, 1);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);
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
const JSON_CT: [(&str, &str); 2] = [("authorization", "Bearer sess-1"), ("content-type", "application/json")];

/// Node's serialize, independently reproduced for blob verification.
fn raw_inflate(blob: &[u8]) -> String {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut d = Decompress::new(false);
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let mut pos = 0usize;
    loop {
        let (in0, out0) = (d.total_in() as usize, d.total_out() as usize);
        let status = d.decompress(&blob[pos..], &mut buf, FlushDecompress::Finish).unwrap();
        let (in1, out1) = (d.total_in() as usize, d.total_out() as usize);
        out.extend_from_slice(&buf[..out1 - out0]);
        pos += in1 - in0;
        if status == Status::StreamEnd {
            break;
        }
        assert!(!(in1 == in0 && out1 == out0), "stalled");
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn kill_switch_proxies_everything_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "ks.db"), &[], &format!("http://{upstream}"));
    let (s, h, b) = request(&r, "GET", "/api/search-history", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["pq"], "/api/search-history");
    let (s, h, _) = request(&r, "GET", "/api/shared/tok-public", &[], None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn auth_gate_and_parse_order() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "auth.db"), &["search_history"], &format!("http://{upstream}"));

    // No token -> 401 body from middleware/auth.js twin.
    let (s, _, b) = request(&r, "GET", "/api/search-history", &[], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(&b), json!({ "error": "Access denied. No token provided." }));

    // Malformed JSON POST WITHOUT a token still answers 500 (body-parser
    // threw above the gate) — proves the parse/auth order.
    let (s, _, b) = request(&r, "POST", "/api/search-history", &[("content-type", "application/json")], Some("{oops")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({ "error": "Internal server error" }));

    // API key (GET-only credential) on POST -> 403.
    let (s, _, b) = request(
        &r,
        "POST",
        "/api/search-history",
        &[("authorization", "Bearer k59"), ("content-type", "application/json")],
        Some("{\"q\":\"x\"}"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(json_body(&b), json!({ "error": "API keys are read-only. Use a login session for this request." }));

    // Trailing slash is NOT owned -> proxied (Express loose-match policy).
    let (s, h, _) = request(&r, "GET", "/api/search-history/", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");

    // PUT is not routed -> proxied.
    let (_s, h, _) = request(&r, "PUT", "/api/search-history", &AUTH, None).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn post_merge_persists_and_replays_via_get() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "post.db");
    let r = router_for(db.clone(), &["search_history"], &format!("http://{upstream}"));

    // 400: nothing survives cleaning.
    let (s, _, b) = request(&r, "POST", "/api/search-history", &JSON_CT, Some("{\"items\":[{\"q\":\"  \"},{}]}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({ "error": "q is required" }));

    // Deterministic import: dedupe max-at wins, trim, empty filtered,
    // string at via Number(), stable sort desc.
    let items = json!({"items":[
        {"q":"delta force","at":1760000000000.0},
        {"q":"  spaced  ","at":1760000001000.0},
        {"q":"delta force","at":1759999999000.0},
        {"q":"   "},
        {"q":"emoji 🧪","at":"1760000002500"}
    ]});
    let (s, h, b) = request(&r, "POST", "/api/search-history", &JSON_CT, Some(&items.to_string())).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        String::from_utf8_lossy(&b),
        r#"[{"q":"emoji 🧪","at":1760000002000},{"q":"spaced","at":1760000001000},{"q":"delta force","at":1760000000000}]"#
    );
    // Express default body ETag on the write response.
    assert_eq!(h.get(header::ETAG).unwrap().to_str().unwrap(), body_etag(&b));

    // The persisted blob must be raw deflate (no zlib header) holding the
    // [[q, at]] JSON, decodable independently — Node's inflateRaw twin.
    let conn = Connection::open(&db).unwrap();
    let blob: Vec<u8> = conn
        .query_row("SELECT data FROM search_history WHERE user_id = 1", [], |r| r.get(0))
        .unwrap();
    assert_ne!(blob[0], 0x78, "zlib header on a deflateRaw blob");
    assert_eq!(
        raw_inflate(&blob),
        r#"[["emoji 🧪",1760000002],["spaced",1760000001],["delta force",1760000000]]"#
    );

    // GET mirrors it; ETag replay answers a bodyless 304.
    let (s, h, b) = request(&r, "GET", "/api/search-history", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        String::from_utf8_lossy(&b),
        r#"[{"q":"emoji 🧪","at":1760000002000},{"q":"spaced","at":1760000001000},{"q":"delta force","at":1760000000000}]"#
    );
    let tag = h.get(header::ETAG).unwrap().clone();
    let (s, _, b) = request(&r, "GET", "/api/search-history", &[("authorization", "Bearer sess-1"), ("if-none-match", tag.to_str().unwrap())], None).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    assert!(b.is_empty());
    // npm-fresh: request Cache-Control 'no-cache' disables the 304
    // (Node answers the full 200 — verified against fresh@0.5.2 regex).
    let (s, _, b) = request(
        &r,
        "GET",
        "/api/search-history",
        &[
            ("authorization", "Bearer sess-1"),
            ("if-none-match", tag.to_str().unwrap()),
            ("cache-control", "no-cache, must-revalidate"),
        ],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(!b.is_empty());

    // A second add merges through the persisted blob (dedupe keeps the
    // newer at; sort stays descending).
    let (s, _, b) = request(&r, "POST", "/api/search-history", &JSON_CT, Some("{\"q\":\" delta force \"}")).await;
    assert_eq!(s, StatusCode::OK);
    let v: Vec<Value> = serde_json::from_slice(&b).unwrap();
    assert_eq!(v[0]["q"], "delta force"); // moved to top (at = now)
    assert_eq!(v.len(), 3);

    // DELETE one, then clear.
    let (s, _, b) = request(&r, "DELETE", "/api/search-history", &JSON_CT, Some("{\"q\":\"spaced\"}")).await;
    assert_eq!(s, StatusCode::OK);
    let v: Vec<Value> = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.iter().filter(|e| e["q"] == "spaced").count(), 0);
    assert_eq!(v.len(), 2);
    let (s, _, b) = request(&r, "DELETE", "/api/search-history", &JSON_CT, Some("{}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(&b[..], b"[]");
    let (s, _, b) = request(&r, "DELETE", "/api/search-history", &AUTH, None).await; // no body -> {} -> clear
    assert_eq!(s, StatusCode::OK);
    assert_eq!(&b[..], b"[]");
}

#[tokio::test]
async fn shared_public_read() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "sh.db"), &["shared"], &format!("http://{upstream}"));

    // Unknown token -> 404; disabled share -> 404. No session needed.
    let (s, _, b) = request(&r, "GET", "/api/shared/nope", &[], None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&b), json!({ "error": "This card collection is private or does not exist." }));
    let (s, _, _) = request(&r, "GET", "/api/shared/tok-off", &[], None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, h, b) = request(&r, "GET", "/api/shared/tok-public", &[], None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get(header::ETAG).unwrap().to_str().unwrap(), body_etag(&b));
    let v = json_body(&b);
    assert_eq!(v["owner"], "u1");

    // Key order on a formatted row: SELECT order, base_rarity appended at
    // apply time, color_identity last; price_trend overwritten in place.
    let keys: Vec<&String> = v["collection"][0].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec![
            "entry_id", "card_id", "quantity", "condition", "printing", "language", "added_at",
            "is_trade", "is_proxy", "name", "printed_name", "supertype", "subtypes", "types",
            "rarity", "set_id", "set_name", "number", "image_url", "price_trend", "price_normal",
            "price_holofoil", "price_estimated", "base_rarity", "color_identity"
        ]
    );
    assert_eq!(v["collection"][0]["price_trend"], 3.91); // Holofoil branch
    assert_eq!(v["collection"][1]["printing"], "Normal");
    assert_eq!(v["collection"][2]["rarity"], "Proxy"); // proxy copy
    assert_eq!(v["collection"][2]["base_rarity"], "Common");
    assert_eq!(v["collection"][2]["price_trend"], 0); // proxy worth nothing
    assert_eq!(v["collection"][3]["price_trend"], 0); // all-null prices
    assert_eq!(v["collection"][0]["subtypes"], json!(["Bird"]));
    assert_eq!(v["collection"][0]["color_identity"], json!([]));

    // Stats fold over rows in added_at DESC order, insertion-ordered keys.
    // totalCards 2+1+3+4=10; value 2*3.91 + 1*0.12 + 3*0 + 4*0 = 7.94.
    let st = &v["stats"];
    assert_eq!(st["summary"]["totalCards"], 10);
    assert_eq!(st["summary"]["uniqueCards"], 4);
    assert_eq!(st["summary"]["totalValue"], 7.94);
    assert_eq!(st["summary"]["estimatedCards"], 0);
    assert_eq!(st["types"][0], json!({"name":"Creature","value":6})); // bird 2 + cat 1+3
    assert_eq!(st["types"][1], json!({"name":"Bird","value":2}));
    assert_eq!(st["types"][2], json!({"name":"Cat","value":4})); // 1+3
    assert_eq!(st["types"][3], json!({"name":"Colorless","value":4})); // null types row
    assert_eq!(st["rarities"][0], json!({"name":"Rare","value":2}));
    assert_eq!(st["rarities"][1], json!({"name":"Common","value":1}));
    assert_eq!(st["rarities"][2], json!({"name":"Proxy","value":3}));
    assert_eq!(st["rarities"][3], json!({"name":"Unknown","value":4})); // null rarity
    // sets sorted by RAW value desc: who(7.82) dmu(0.12) null(0).
    assert_eq!(st["sets"][0]["id"], "who");
    assert_eq!(st["sets"][0]["value"], 7.82);
    assert_eq!(st["sets"][1]["id"], "dmu");
    assert_eq!(st["sets"][2]["id"], "null"); // JS key for null set_id

    // Trade filter: only the is_trade=1 row; entry_id keeps position.
    let (s, _, b) = request(&r, "GET", "/api/shared/tok-public?list=trade", &[], None).await;
    assert_eq!(s, StatusCode::OK);
    let v = json_body(&b);
    assert_eq!(v["collection"].as_array().unwrap().len(), 1);
    assert_eq!(v["collection"][0]["card_id"], "c-norm");
    assert_eq!(v["stats"]["summary"]["totalCards"], 1);

    // Repeated list= is an ARRAY in qs -> collection branch.
    let (_s, _, b) = request(&r, "GET", "/api/shared/tok-public?list=trade&list=trade", &[], None).await;
    assert_eq!(json_body(&b)["collection"].as_array().unwrap().len(), 4);

    // Percent-decoded token miss -> 404; invalid escape -> routing throw 500.
    let (s, _, _) = request(&r, "GET", "/api/shared/abc%2Fdef", &[], None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, b) = request(&r, "GET", "/api/shared/a%ZZb", &[], None).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({ "error": "Internal server error" }));

    // Empty-collection owner shape.
    let (s, _, b) = request(&r, "GET", "/api/shared/tok-empty", &[], None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b)["collection"], json!([]));

    // HEAD and other methods are not owned -> proxied untouched.
    let (s, h, _) = request(&r, "HEAD", "/api/shared/tok-public", &[], None).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(s, StatusCode::OK);

    // 304 replay of the route's own body ETag.
    let (s, h, _) = request(&r, "GET", "/api/shared/tok-empty", &[], None).await;
    assert_eq!(s, StatusCode::OK);
    let tag = h.get(header::ETAG).unwrap().clone();
    let (s, _, b) = request(&r, "GET", "/api/shared/tok-empty", &[("if-none-match", tag.to_str().unwrap())], None).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    assert!(b.is_empty());
    // no-cache on the request kills the 304 (npm-fresh parity, live-pinned).
    let (s, _, _) = request(
        &r,
        "GET",
        "/api/shared/tok-empty",
        &[("if-none-match", tag.to_str().unwrap()), ("cache-control", "NO-CACHE")],
        None,
    )
    .await;
    // Live-pinned vs Node :3312: NO-CACHE -> 304 (fresh@0.5.2 regex is
    // case-sensitive, so the directive does NOT disable the ETag replay).
    assert_eq!(s, StatusCode::NOT_MODIFIED); // case-sensitive: NO-CACHE does NOT match
    let (s, _, _) = request(
        &r,
        "GET",
        "/api/shared/tok-empty",
        &[("if-none-match", tag.to_str().unwrap()), ("cache-control", "no-store")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_MODIFIED); // no-store is NOT the fresh directive
    let (s, _, _) = request(
        &r,
        "GET",
        "/api/shared/tok-empty",
        &[("if-none-match", tag.to_str().unwrap()), ("cache-control", "no-cache")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn shared_bad_array_column_is_500_with_route_body() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let db = seeded_db(dir.path(), "sh500.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute("UPDATE card_cache SET types = 'oops' WHERE id = 'c-null'", []).unwrap();
    }
    let r = router_for(db, &["shared"], &format!("http://{upstream}"));
    let (s, _, b) = request(&r, "GET", "/api/shared/tok-public", &[], None).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({ "error": "Failed to retrieve shared collection" }));
}

#[tokio::test]
async fn oversized_body_answers_the_413_shape() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let r = router_for(seeded_db(dir.path(), "big.db"), &["search_history"], &format!("http://{upstream}"));
    let big = format!("{{\"q\":\"{}\"}}", "x".repeat(1024 * 1024 + 16));
    let (s, _, b) = request(&r, "POST", "/api/search-history", &JSON_CT, Some(&big)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json_body(&b), json!({ "error": "Upload too large. Try exporting/importing in smaller batches." }));
}
