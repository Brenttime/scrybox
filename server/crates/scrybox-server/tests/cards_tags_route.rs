//! Native /api/cards oracle + tag views (Lane B unit 2a): auth gate,
//! closure-fold shape of /oracle-tags (direct + inherited, via lists),
//! role-tags grouping/validation, tag-stats noise filter + ranking, and the
//! getOracleText cached/fetch legs incl. rules-text-only backfill.

use std::{collections::BTreeSet, net::SocketAddr, sync::Arc};

use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    response::IntoResponse,
    Router,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tower::ServiceExt;

use scrybox_server::{
    app,
    scryfall::{Scry, ScryEnv},
    AppState, Config,
};

async fn spawn_fake_node() -> SocketAddr {
    let echo = axum::routing::any(|req: Request| async move {
        let parts = req.into_parts().0;
        (
            StatusCode::OK,
            [("x-node", "1")],
            axum::Json(json!({"pq": parts.uri.path_and_query().unwrap().as_str()})),
        )
    });
    let health =
        axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({"status":"ok"}))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

fn seeded_db(dir: &std::path::Path, name: &str) -> Connection {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(include_str!("scry_schema.sql")).unwrap();
    conn.execute_batch(include_str!("oracle_tag_schema.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, api_key, share_token) VALUES (1,'u1','x','admin','tok-api','share-1');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-session', 1, '2099-01-01 00:00:00');",
    )
    .unwrap();
    conn
}

async fn router_for(db_path: std::path::PathBuf, routes: &[&str], upstream: &str) -> Router {
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

async fn router_for_scry(
    db_path: std::path::PathBuf,
    routes: &[&str],
    upstream: &str,
    scry: Scry,
) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.scry = Arc::new(scry);
    app(st)
}

fn get_router(_router: &Router, uri: &str, auth: Option<&str>) -> Request {
    let mut b = Request::get(uri).body(Body::empty()).unwrap();
    if let Some(t) = auth {
        b.headers_mut()
            .insert("authorization", t.parse().unwrap());
    }
    b
}

fn post_router(_router: &Router, uri: &str, auth: Option<&str>, body: Value) -> Request {
    let mut req = Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    if let Some(t) = auth {
        req.headers_mut()
            .insert("authorization", t.parse().unwrap());
    }
    req
}

async fn body_str(res: axum::response::Response) -> String {
    String::from_utf8(
        axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

const SESS: Option<&str> = Some("Bearer tok-session");

// ---------------------------------------------------------------------------
// Tag fixtures
// ---------------------------------------------------------------------------

fn seed_tag_data(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO oracle_tag_generations (id, source_updated_at, imported_at, active)
         VALUES (7,'2026-01-01',1,1),(8,'2026-01-02',2,0);
         INSERT INTO oracle_tags (generation_id, tag_id, slug, label, description) VALUES
           (7,'t-parent','has-land','Has Land',NULL),
           (7,'t-child','land-fallback','Land Fallback','desc'),
           (7,'t-grand','land-type','Land Type',NULL),
           (7,'t-noise','has-rambly','Has Rambly','noise'),
           (7,'t-untagged','pun-solo','Pun Solo',NULL),
           (8,'t-old','stale-tag','Stale',NULL);
         INSERT INTO oracle_tag_closure (generation_id, ancestor_tag_id, descendant_tag_id) VALUES
           (7,'t-parent','t-parent'),(7,'t-child','t-child'),(7,'t-grand','t-grand'),
           (7,'t-grand','t-parent'),(7,'t-grand','t-child'),(7,'t-parent','t-child'),
           (7,'t-noise','t-noise'),(7,'t-untagged','t-untagged');
         INSERT INTO oracle_tag_assignments (generation_id, tag_id, oracle_id) VALUES
           (7,'t-child','oid-1'),(7,'t-parent','oid-1'),
           (7,'t-child','oid-2'),
           (7,'t-noise','oid-1'),(7,'t-noise','oid-2'),
           (7,'t-untagged','oid-3'),
           (8,'t-old','oid-1');
         INSERT INTO card_cache (id, name) VALUES
           ('mtg-aaaa1111-1111-1111-1111-111111111111','Card One'),
           ('mtg-bbbb2222-2222-2222-2222-222222222222','Card Two'),
           ('mtg-cccc3333-3333-3333-3333-333333333333','Card Three');
         UPDATE card_cache SET oracle_id='oid-1' WHERE id LIKE 'mtg-aaaa%';
         UPDATE card_cache SET oracle_id='oid-2' WHERE id LIKE 'mtg-bbbb%';
         UPDATE card_cache SET oracle_id='oid-3' WHERE id LIKE 'mtg-cccc%';",
    )
    .unwrap();
}

const C1: &str = "mtg-aaaa1111-1111-1111-1111-111111111111";
const C2: &str = "mtg-bbbb2222-2222-2222-2222-222222222222";
const C3: &str = "mtg-cccc3333-3333-3333-3333-333333333333";

// ---------------------------------------------------------------------------
// GET /api/cards/:id/oracle-tags
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oracle_tags_direct_inherited_and_via() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    seed_tag_data(&conn);
    drop(conn);
    let node = spawn_fake_node().await;
    let router = router_for(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;

    // oid-1: direct child+parent; grand is inherited via both, deduped rows
    // grouped with via list in (anc.slug, via.slug) order.
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            &format!("/api/cards/{C1}/oracle-tags"),
            SESS,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/json; charset=utf-8");
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v["ready"], json!(true));
    assert_eq!(v["oracle_id"], json!("oid-1"));
    // Direct tags ordered by slug: has-land, has-rambly, land-fallback.
    // tag_id stripped. NOTE: /oracle-tags has NO noise filter (TAG_NOISE only
    // gates tag-stats), so the fixture's t-noise/has-rambly assignment shows
    // here exactly like Node's cardOracleTags returns it.
    assert_eq!(
        v["tags"],
        json!([
            {"slug":"has-land","label":"Has Land","description":null},
            {"slug":"has-rambly","label":"Has Rambly","description":"noise"},
            {"slug":"land-fallback","label":"Land Fallback","description":"desc"}
        ])
    );
    // Inherited: grand only (parent is direct -> skipped; self rows excluded).
    assert_eq!(
        v["inherited"],
        json!([
            {"slug":"land-type","label":"Land Type","description":null,"via":["has-land","land-fallback"]}
        ])
    );

    // oid-2: direct child only -> parent AND grand inherited with via lists.
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            &format!("/api/cards/{C2}/oracle-tags"),
            SESS,
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v["tags"], json!([{"slug":"has-rambly","label":"Has Rambly","description":"noise"},{"slug":"land-fallback","label":"Land Fallback","description":"desc"}]));
    assert_eq!(
        v["inherited"],
        json!([
            {"slug":"has-land","label":"Has Land","description":null,"via":["land-fallback"]},
            {"slug":"land-type","label":"Land Type","description":null,"via":["land-fallback"]}
        ])
    );
}

#[tokio::test]
async fn oracle_tags_no_gen_no_oracle_and_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    seed_tag_data(&conn);
    conn.execute("UPDATE card_cache SET oracle_id=NULL WHERE id LIKE 'mtg-cccc%'", [])
        .unwrap();
    drop(conn);
    let node = spawn_fake_node().await;
    let router = router_for(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;

    // Card without oracle_id -> ready:true, nulls.
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            &format!("/api/cards/{C3}/oracle-tags"),
            SESS,
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v, json!({"ready":true,"oracle_id":null,"tags":[],"inherited":[]}));

    // ?oracle_id= fallback answers for the same card (query-string decoded).
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            &format!("/api/cards/{C3}/oracle-tags?oracle_id=oid-1&x=1"),
            SESS,
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v["oracle_id"], json!("oid-1"));
    assert_eq!(v["tags"].as_array().unwrap().len(), 3); // incl. t-noise (no filter here)

    // Unknown card + no fallback -> ready:true nulls (row absent === no id).
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            "/api/cards/mtg-nope/oracle-tags",
            SESS,
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v, json!({"ready":true,"oracle_id":null,"tags":[],"inherited":[]}));

    // Inactive-only generation -> ready:false.
    let dir2 = tempfile::tempdir().unwrap();
    let conn2 = seeded_db(dir2.path(), "ct.db");
    seed_tag_data(&conn2);
    conn2
        .execute("UPDATE oracle_tag_generations SET active = 0", [])
        .unwrap();
    drop(conn2);
    let router2 = router_for(
        dir2.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;
    let res = router2
        .clone()
        .oneshot(get_router(
            &router2,
            &format!("/api/cards/{C1}/oracle-tags"),
            SESS,
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v, json!({"ready":false,"oracle_id":null,"tags":[],"inherited":[]}));
}

#[tokio::test]
async fn oracle_tags_auth_gate_and_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    seed_tag_data(&conn);
    drop(conn);
    let node = spawn_fake_node().await;
    let router = router_for(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;

    // No token -> 401, Node's exact body.
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            &format!("/api/cards/{C1}/oracle-tags"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_str(res).await,
        "{\"error\":\"Access denied. No token provided.\"}"
    );

    // Bad token -> 401 invalid.
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            &format!("/api/cards/{C1}/oracle-tags"),
            Some("Bearer nope"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // API keys answer GETs (authenticateToken parity).
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            &format!("/api/cards/{C1}/oracle-tags"),
            Some("Bearer tok-api"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // GET on the POST-only paths is NOT ours -> proxied untouched.
    let res = router
        .clone()
        .oneshot(get_router(&router, "/api/cards/role-tags", SESS))
        .await
        .unwrap();
    assert_eq!(res.headers()["x-node"], "1");
    let res = router
        .clone()
        .oneshot(get_router(&router, "/api/cards/tag-stats", SESS))
        .await
        .unwrap();
    assert_eq!(res.headers()["x-node"], "1");

    // Kill switch: group not owned -> every path (incl. ours) proxies.
    let off = router_for(
        dir.path().join("ct.db"),
        &[],
        &format!("http://{node}"),
    )
    .await;
    let res = off
        .clone()
        .oneshot(get_router(&off, &format!("/api/cards/{C1}/oracle-tags"), SESS))
        .await
        .unwrap();
    assert_eq!(res.headers()["x-node"], "1");
}

// ---------------------------------------------------------------------------
// POST /api/cards/role-tags
// ---------------------------------------------------------------------------

#[tokio::test]
async fn role_tags_groups_by_card() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    seed_tag_data(&conn);
    drop(conn);
    let node = spawn_fake_node().await;
    let router = router_for(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;

    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/role-tags",
            SESS,
            json!({"ids":[C1,C2,C1,C3,7,"  "],"slugs":["land-type","land-fallback","land-type"]}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // c1 carries both slugs (direct parent/child + inherited grand);
    // c2 carries both too (child direct, parent+grand inherited);
    // c3 (pun-solo only) has neither -> absent from the map entirely.
    // Distinct rows come in card_cache id order (cc.id drives the plan).
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v["ready"], json!(true));
    let tags = v["tags"].as_object().unwrap();
    let c1: Vec<&str> = tags[C1].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    let c2: Vec<&str> = tags[C2].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert!(c1.contains(&"land-type") && c1.contains(&"land-fallback"), "{c1:?}");
    assert!(c2.contains(&"land-type") && c2.contains(&"land-fallback"), "{c2:?}");
    assert!(!tags.contains_key(C3));
}

#[tokio::test]
async fn role_tags_validation_and_ready_false() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    seed_tag_data(&conn);
    drop(conn);
    let node = spawn_fake_node().await;
    let router = router_for(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;

    let bad = |body: Value| {
        let router = router.clone();
        async move {
            let req = post_router(&router, "/api/cards/role-tags", SESS, body);
            let res = router.oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                body_str(res).await,
                "{\"error\":\"ids (1-1000) and slugs (1-100) are required\"}"
            );
        }
    };
    bad(json!({"ids":[],"slugs":["a"]})).await;
    bad(json!({"ids":[C1],"slugs":[]})).await;
    bad(json!({"ids":[7,8],"slugs":["a"]})).await; // non-strings filtered => empty
    bad(json!({"ids":[C1],"slugs":(0..101).map(|i| format!("s{i}")).collect::<Vec<_>>()})).await;
    bad(json!({"ids":(0..1001).map(|i| format!("id{i}")).collect::<Vec<_>>(),"slugs":["a"]})).await;
    bad(json!({})).await;

    // The GLOBAL app.use('/api', authenticateToken) gate runs before the
    // route body (server.js:430): a bad-shape request without a token
    // answers 401, not 400. (Parse failures still outrank auth — that is the
    // express.json middleware ahead of the gate, covered by the Invalid arm.)
    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/role-tags",
            None,
            json!({"ids":[],"slugs":[]}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_str(res).await,
        "{\"error\":\"Access denied. No token provided.\"}"
    );

    // Valid shape, no token -> 401.
    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/role-tags",
            None,
            json!({"ids":[C1],"slugs":["land-type"]}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_str(res).await,
        "{\"error\":\"Access denied. No token provided.\"}"
    );

    // API keys are GET-only: middleware/auth.js answers 403 with its own
    // message (NOT the 401 a bad token gets).
    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/role-tags",
            Some("Bearer tok-api"),
            json!({"ids":[C1],"slugs":["land-type"]}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_str(res).await,
        "{\"error\":\"API keys are read-only. Use a login session for this request.\"}"
    );

    // No active generation -> {ready:false, tags:{}}.
    let dir2 = tempfile::tempdir().unwrap();
    let conn2 = seeded_db(dir2.path(), "ct.db");
    seed_tag_data(&conn2);
    conn2.execute("DELETE FROM oracle_tag_generations", []).unwrap();
    drop(conn2);
    let router2 = router_for(
        dir2.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;
    let res = router2
        .clone()
        .oneshot(post_router(
            &router2,
            "/api/cards/role-tags",
            SESS,
            json!({"ids":[C1],"slugs":["land-type"]}),
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v, json!({"ready":false,"tags":{}}));
}

// ---------------------------------------------------------------------------
// POST /api/cards/tag-stats
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tag_stats_filters_ranks_and_caps() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    seed_tag_data(&conn);
    drop(conn);
    let node = spawn_fake_node().await;
    let router = router_for(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;

    // c1+c2 share has-land (noise: has-) and land-fallback (direct on both);
    // land-type (grand) is inherited for both; has-rambly noise; pun-solo
    // only c3 (<2 cards). Expected: land-fallback and land-type, both 2
    // cards, slug-ordered tie-break; noise dropped.
    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/tag-stats",
            SESS,
            json!({"ids":[C1,C2,C3]}),
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v["ready"], json!(true));
    let tags = v["tags"].as_array().unwrap();
    let slugs: Vec<&str> = tags.iter().map(|t| t["slug"].as_str().unwrap()).collect();
    assert_eq!(slugs, vec!["land-fallback", "land-type"]);
    assert_eq!(tags[0]["label"], json!("Land Fallback"));
    assert_eq!(tags[0]["cards"].as_array().unwrap().len(), 2);

    // Single card -> every tag under the >=2 bar.
    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/tag-stats",
            SESS,
            json!({"ids":[C1]}),
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v["tags"], json!([]));

    // Validation + auth ordering mirrors role-tags: the global /api gate
    // answers 401 before the route's 400 shape check.
    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/tag-stats",
            None,
            json!({"ids":[]}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_str(res).await,
        "{\"error\":\"Access denied. No token provided.\"}"
    );
    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/tag-stats",
            None,
            json!({"ids":[C1]}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tag_stats_null_label_falls_back_to_slug() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    seed_tag_data(&conn);
    // A shared, non-noise tag with NULL label: only land-type has NULL label
    // already; assert its emitted label equals its slug ('Land Type' set, so
    // null out the other shared tag instead).
    conn.execute(
        "UPDATE oracle_tags SET label = NULL WHERE slug = 'land-fallback'",
        [],
    )
    .unwrap();
    drop(conn);
    let node = spawn_fake_node().await;
    let router = router_for(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;
    let res = router
        .clone()
        .oneshot(post_router(
            &router,
            "/api/cards/tag-stats",
            SESS,
            json!({"ids":[C1,C2]}),
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    let tags = v["tags"].as_array().unwrap();
    let lf = tags.iter().find(|t| t["slug"] == json!("land-fallback")).unwrap();
    assert_eq!(lf["label"], json!("land-fallback"), "label || slug parity");
}

// ---------------------------------------------------------------------------
// GET /api/cards/:id/oracle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oracle_cached_row_paths() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    conn.execute(
        "INSERT INTO card_cache (id, name, oracle_text, oracle_faces) VALUES
         ('mtg-dddd4444-4444-4444-4444-444444444444','Texted','hello world','[{\"name\":\"A\",\"mana_cost\":\"\",\"type_line\":\"T\",\"oracle_text\":\"x\"}]'),
         ('mtg-eeee5555-5555-5555-5555-555555555555','BrokenFaces','text','{not json'),
         ('mtg-ffff6666-6666-6666-6666-666666666666','NullText',NULL,NULL)",
        [],
    )
    .unwrap();
    // A non-mtg id row (local/custom card): never fetches.
    conn.execute(
        "INSERT INTO card_cache (id, name, oracle_text) VALUES ('custom-1','Custom',NULL)",
        [],
    )
    .unwrap();
    drop(conn);
    let node = spawn_fake_node().await;
    let router = router_for(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
    )
    .await;

    // Cached text + faces passthrough (parsed JSON), byte-exact key order.
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            "/api/cards/mtg-dddd4444-4444-4444-4444-444444444444/oracle",
            SESS,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        body_str(res).await,
        "{\"oracle_text\":\"hello world\",\"faces\":[{\"name\":\"A\",\"mana_cost\":\"\",\"type_line\":\"T\",\"oracle_text\":\"x\"}]}"
    );

    // Unparseable faces JSON -> faces null (Node try/catch).
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            "/api/cards/mtg-eeee5555-5555-5555-5555-555555555555/oracle",
            SESS,
        ))
        .await
        .unwrap();
    assert_eq!(
        body_str(res).await,
        "{\"oracle_text\":\"text\",\"faces\":null}"
    );

    // Null-text mtg row would fetch; but here verify non-mtg row answers the
    // empty shape without touching the wire, and unknown ids 404.
    let res = router
        .clone()
        .oneshot(get_router(&router, "/api/cards/custom-1/oracle", SESS))
        .await
        .unwrap();
    assert_eq!(body_str(res).await, "{\"oracle_text\":\"\",\"faces\":null}");

    let res = router
        .clone()
        .oneshot(get_router(&router, "/api/cards/custom-missing/oracle", SESS))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_str(res).await, "{\"error\":\"Card not found\"}");

    // Auth gate.
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            "/api/cards/mtg-dddd4444-4444-4444-4444-444444444444/oracle",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

struct FetchMock {
    addr: SocketAddr,
    hits: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn spawn_card_mock(card: Option<Value>, status: Option<u16>) -> FetchMock {
    let hits: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hits2 = hits.clone();
    let st: Arc<std::sync::Mutex<Option<u16>>> = Arc::new(std::sync::Mutex::new(status));
    let cd: Arc<std::sync::Mutex<Option<Value>>> = Arc::new(std::sync::Mutex::new(card));
    let router = Router::new().fallback(
        move |req: Request| {
            let hits = hits2.clone();
            let st = st.clone();
            let cd = cd.clone();
            async move {
                let path = req.uri().path().to_string();
                hits.lock().unwrap().push(format!("{} {path}", req.method()));
                let code = st.lock().unwrap().unwrap_or(200);
                let body = cd.lock().unwrap().clone();
                let json: Option<Value> = body;
                if let Some(j) = json {
                    return (StatusCode::from_u16(code).unwrap(), axum::Json(j)).into_response();
                }
                (StatusCode::from_u16(code).unwrap(), axum::Json(json!({"object":"error"})))
                    .into_response()
            }
        },
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    FetchMock { addr, hits }
}

#[tokio::test]
async fn oracle_legacy_row_fetch_backfills_rules_columns() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    conn.execute(
        "INSERT INTO card_cache (id, name, oracle_text, oracle_faces, price_normal) VALUES
         ('mtg-99999999-9999-9999-9999-999999999999','Legacy',NULL,NULL,1.5)",
        [],
    )
    .unwrap();
    drop(conn);
    let node = spawn_fake_node().await;
    let mock = spawn_card_mock(
        Some(json!({
            "id": "99999999-9999-9999-9999-999999999999",
            "name": "Legacy",
            "oracle_text": "Draw a card.",
            "card_faces": [
                {"name":"A","mana_cost":"{W}","type_line":"Creature","oracle_text":"first"},
                {"name":"B","mana_cost":"","type_line":"Instant","oracle_text":"second"}
            ]
        })),
        None,
    )
    .await;
    let scry = Scry::new(ScryEnv {
        local: false,
        local_url: format!("http://{}", mock.addr),
        base_url: format!("http://{}", mock.addr),
        gap_scale: 0.0,
    });
    let router = router_for_scry(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
        scry,
    )
    .await;

    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            "/api/cards/mtg-99999999-9999-9999-9999-999999999999/oracle",
            SESS,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v: Value = serde_json::from_str(&body_str(res).await).unwrap();
    assert_eq!(v["oracle_text"], json!("Draw a card."));
    // oracleFields join fallback: top-level text wins (raw oracle_text set).
    // faces come from the >1 card_faces projection.
    let faces = v["faces"].as_array().unwrap();
    assert_eq!(faces.len(), 2);
    assert_eq!(faces[1]["oracle_text"], json!("second"));

    // Wire: exactly one GET to /cards/<uuid>.
    let hits = mock.hits.lock().unwrap().clone();
    assert_eq!(hits, vec!["GET /cards/99999999-9999-9999-9999-999999999999"]);

    // Rules-text-only backfill; prices untouched.
    let conn = Connection::open(dir.path().join("ct.db")).unwrap();
    let (text, faces_json, price): (Option<String>, Option<String>, Option<f64>) = conn
        .query_row(
            "SELECT oracle_text, oracle_faces, price_normal FROM card_cache WHERE id LIKE 'mtg-9999%'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(text.as_deref(), Some("Draw a card."));
    assert!(faces_json.unwrap().contains("second"));
    assert_eq!(price, Some(1.5));
}

#[tokio::test]
async fn oracle_fetch_404_and_error_ladder() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "ct.db");
    conn.execute(
        "INSERT INTO card_cache (id, name, oracle_text) VALUES
         ('mtg-88888888-8888-8888-8888-888888888888','Gone',NULL)",
        [],
    )
    .unwrap();
    drop(conn);
    let node = spawn_fake_node().await;

    // 404 from Scryfall -> 404 Card not found.
    let mock = spawn_card_mock(None, Some(404)).await;
    let scry = Scry::new(ScryEnv {
        local: false,
        local_url: format!("http://{}", mock.addr),
        base_url: format!("http://{}", mock.addr),
        gap_scale: 0.0,
    });
    let router = router_for_scry(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
        scry,
    )
    .await;
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            "/api/cards/mtg-88888888-8888-8888-8888-888888888888/oracle",
            SESS,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_str(res).await, "{\"error\":\"Card not found\"}");

    // 5xx -> 502 Could not load rules text.
    let mock = spawn_card_mock(None, Some(500)).await;
    let scry = Scry::new(ScryEnv {
        local: false,
        local_url: format!("http://{}", mock.addr),
        base_url: format!("http://{}", mock.addr),
        gap_scale: 0.0,
    });
    let router = router_for_scry(
        dir.path().join("ct.db"),
        &["cards_tags"],
        &format!("http://{node}"),
        scry,
    )
    .await;
    let res = router
        .clone()
        .oneshot(get_router(
            &router,
            "/api/cards/mtg-88888888-8888-8888-8888-888888888888/oracle",
            SESS,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        body_str(res).await,
        "{\"error\":\"Could not load rules text\"}"
    );
}
