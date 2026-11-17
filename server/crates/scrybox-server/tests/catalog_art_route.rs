//! Native card-art reads + stateless catalog reads (Lane B unit 2b):
//! /api/card-art/index ids fold (sorted readdir, bundled-then-user, dedup),
//! /api/card-art/:id.png serving + validation, /api/scan-sets set counts /
//! local / published / builtLangs, and /api/admin/catalogs/languages auth
//! ladder + per-language counts. Mutating card-art routes and the catalog
//! build controls must fall through to the proxy.

use std::{collections::BTreeSet, net::SocketAddr};

use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    Router,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tower::ServiceExt;

use scrybox_server::{app, AppState, Config};

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
    conn.execute_batch(include_str!("schema_fixture.sql"))
        .unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, api_key, share_token) VALUES (1,'u1','x','admin','tok-api','share-1');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-session', 1, '2099-01-01 00:00:00');
         INSERT INTO users (id, username, password_hash, role, share_token) VALUES (2,'u2','x','member','share-2');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-member', 2, '2099-01-01 00:00:00');",
    )
    .unwrap();
    conn
}

/// shared/ fixture: languages.json (same shape as the repo file) + card-art/
/// layer dirs created by the caller under it.
fn write_shared(dir: &std::path::Path) -> std::path::PathBuf {
    let shared = dir.join("shared");
    std::fs::create_dir_all(shared.join("card-art")).unwrap();
    std::fs::write(
        shared.join("languages.json"),
        serde_json::to_vec(&json!([
            {"code": "en", "name": "English", "scryfall": "en"},
            {"code": "ja", "name": "Japanese", "scryfall": "ja"},
            {"code": "de", "name": "German", "scryfall": "de"},
            {"code": "es", "name": "Spanish", "scryfall": "es"}
        ]))
        .unwrap(),
    )
    .unwrap();
    shared
}

fn write_png(path: &std::path::Path, marker: &[u8]) {
    std::fs::write(
        path,
        [b"\x89PNG\r\n\x1a\n".to_vec(), marker.to_vec()].concat(),
    )
    .unwrap();
}

async fn router_for(
    db_path: std::path::PathBuf,
    routes: &[&str],
    upstream: &str,
    shared: Option<std::path::PathBuf>,
    model: Option<std::path::PathBuf>,
) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: routes
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.shared_dir = shared;
    st.model_dir = model;
    app(st)
}

fn get_uri(uri: &str, auth: Option<&str>) -> Request {
    let mut b = Request::get(uri).body(Body::empty()).unwrap();
    if let Some(t) = auth {
        b.headers_mut().insert("authorization", t.parse().unwrap());
    }
    b
}

async fn body_val(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn body_bytes(res: axum::response::Response) -> (axum::response::Response, Vec<u8>) {
    let (parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (
        axum::response::Response::from_parts(parts, Body::empty()),
        bytes,
    )
}

fn card(conn: &Connection, id: &str, set: &str, lang: &str, image: Option<&str>) {
    conn.execute(
        "INSERT INTO card_cache (id, name, set_id, language, image_url) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![id, id, set, lang, image],
    )
    .unwrap();
}

const SESS: Option<&str> = Some("Bearer tok-session");

// ---------------------------------------------------------------------------
// card-art index
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_folds_layers_sorted_deduped() {
    let dir = tempfile::tempdir().unwrap();
    let shared = write_shared(dir.path());
    let bundled = shared.join("card-art");
    let user = dir.path().join("card-art"); // dirname(db)/card-art
    std::fs::create_dir_all(&user).unwrap();
    write_png(&bundled.join("zz.png"), b"B");
    write_png(&bundled.join("a.png"), b"A");
    write_png(&bundled.join("a!.png"), b"A2");
    write_png(&bundled.join("note.txt"), b"x");
    write_png(&bundled.join("UPPER.PNG"), b"U");
    write_png(&user.join("mm.png"), b"M");
    write_png(&user.join("a.png"), b"A-user"); // dup of bundled a.png

    let conn = seeded_db(dir.path(), "db.sqlite");
    drop(conn);
    let node = spawn_fake_node().await;
    let r = router_for(
        dir.path().join("db.sqlite"),
        &["catalog_art"],
        &format!("http://{node}"),
        Some(shared),
        None,
    )
    .await;
    let res = r
        .oneshot(get_uri("/api/card-art/index", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "public, max-age=0, must-revalidate"
    );
    // Code-unit readdir order WITHIN each layer; bundled layer first:
    // UPPER.PNG (U=85 < 'a'=97), 'a!.png' before 'a.png' ('!' 33 < '.' 46),
    // then bundled zz; user-layer mm last. Dedup keeps first-seen (bundled a
    // before user a).
    let v = body_val(res).await;
    assert_eq!(
        v["ids"],
        json!(["UPPER", "a!", "a", "zz", "mm"]),
        "ids must be readdirSync-sorted (full names) with first-seen dedup"
    );
}

#[tokio::test]
async fn index_absent_dirs_answer_empty() {
    let dir = tempfile::tempdir().unwrap();
    let shared = write_shared(dir.path());
    std::fs::remove_dir(shared.join("card-art")).unwrap(); // absent bundled layer
    let conn = seeded_db(dir.path(), "db.sqlite");
    drop(conn);
    let node = spawn_fake_node().await;
    let r = router_for(
        dir.path().join("db.sqlite"),
        &["catalog_art"],
        &format!("http://{node}"),
        Some(shared),
        None,
    )
    .await;
    // No user dir either (db parent has no card-art) => ids [].
    let res = r
        .oneshot(get_uri("/api/card-art/index", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_val(res).await, json!({"ids": []}));
}

// ---------------------------------------------------------------------------
// card-art png
// ---------------------------------------------------------------------------

#[tokio::test]
async fn png_serves_user_layer_over_bundled() {
    let dir = tempfile::tempdir().unwrap();
    let shared = write_shared(dir.path());
    let bundled = shared.join("card-art");
    let user = dir.path().join("card-art");
    std::fs::create_dir_all(&user).unwrap();
    write_png(&bundled.join("mtg-abc.png"), b"BUNDLED");
    write_png(&user.join("mtg-abc.png"), b"USER");

    let conn = seeded_db(dir.path(), "db.sqlite");
    drop(conn);
    let node = spawn_fake_node().await;
    let r = router_for(
        dir.path().join("db.sqlite"),
        &["catalog_art"],
        &format!("http://{node}"),
        Some(shared),
        None,
    )
    .await;
    let res = r
        .oneshot(get_uri("/api/card-art/mtg-abc.png", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    assert_eq!(
        res.headers()
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "public, max-age=0, must-revalidate"
    );
    let (res, bytes) = body_bytes(res).await;
    assert_eq!(
        res.headers()
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap(),
        bytes.len().to_string()
    );
    assert_eq!(&bytes[8..], b"USER"); // user layer wins, bundled untouched
}

#[tokio::test]
async fn png_404_unknown_invalid_ids_and_proxies_other_paths() {
    let dir = tempfile::tempdir().unwrap();
    let shared = write_shared(dir.path());
    let conn = seeded_db(dir.path(), "db.sqlite");
    drop(conn);
    let node = spawn_fake_node().await;
    let r = router_for(
        dir.path().join("db.sqlite"),
        &["catalog_art"],
        &format!("http://{node}"),
        Some(shared),
        None,
    )
    .await;
    // Unknown but valid id.
    let res = r
        .clone()
        .oneshot(get_uri("/api/card-art/no-such-card.png", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_val(res).await,
        json!({"error": "No art for this card"})
    );
    // ID_RE rejects '.', '..', '_', leading non-alnum, embedded space.
    for uri in [
        "/api/card-art/...png",
        "/api/card-art/a..b.png",
        "/api/card-art/_.png",
        "/api/card-art/%20x.png",
    ] {
        let res = r.clone().oneshot(get_uri(uri, None)).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "{uri} must 404 like isValidId"
        );
        assert_eq!(
            body_val(res).await,
            json!({"error": "No art for this card"})
        );
    }
    // Undecodable % escape => Express URIError => 500 shape.
    let res = r
        .clone()
        .oneshot(get_uri("/api/card-art/%ZZ.png", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Internal server error"})
    );
    // Unit 6d: the mutating legs are NATIVE — no token answers Node's 401
    // shape locally (they only ride the proxy when the group is off, see
    // write_routes_proxied_when_group_off).
    let res = r
        .clone()
        .oneshot(
            Request::post("/api/card-art/mtg-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_val(res).await["error"],
        "Access denied. No token provided."
    );
    let res = r
        .clone()
        .oneshot(
            Request::delete("/api/card-art/mtg-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_val(res).await["error"],
        "Access denied. No token provided."
    );
    let res = r
        .oneshot(get_uri("/api/admin/catalogs", None))
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").unwrap(), "1"); // build-control list stays on Node
}

// ---------------------------------------------------------------------------
// GET /api/scan-sets
// ---------------------------------------------------------------------------

async fn scan_sets_router(dir: &std::path::Path, model: std::path::PathBuf) -> Router {
    let shared = write_shared(dir);
    let conn = seeded_db(dir, "db.sqlite");
    card(&conn, "e1", "brc", "English", Some("https://x/e1.jpg"));
    card(&conn, "e2", "BRC", "English", Some("https://x/e2.jpg")); // case-folds
    card(&conn, "e3", "one", "English", None); // no art => excluded
    card(&conn, "j1", "brc", "Japanese", Some("https://x/j1.jpg"));
    card(&conn, "s1", "all", "Spanish", Some("https://x/s1.jpg"));
    drop(conn);
    let node = spawn_fake_node().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: ["catalog_art".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        db_path: Some(dir.join("db.sqlite")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.shared_dir = Some(shared);
    st.model_dir = Some(model);
    app(st)
}

#[tokio::test]
async fn scan_sets_counts_and_gates() {
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("models");
    std::fs::create_dir_all(&model).unwrap();
    let r = scan_sets_router(dir.path(), model.clone()).await;

    // No token => the global /api gate answer.
    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Access denied. No token provided."})
    );
    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets", Some("Bearer nope")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Invalid or expired session token."})
    );
    // API key is a GET credential — allowed here (not admin surface).
    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets", Some("Bearer tok-api")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets", SESS))
        .await
        .unwrap();
    let v = body_val(res).await;
    assert_eq!(
        v,
        json!({
            "game": "mtg", "lang": "English",
            "sets": {"brc": {"cached": 2, "embedded": 0}},
            "local": false, "published": false, "builtLangs": []
        })
    );
    // lang resolves through languages.js semantics (code, name, scryfall, junk).
    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets?lang=ja", SESS))
        .await
        .unwrap();
    let v = body_val(res).await;
    assert_eq!(v["lang"], "Japanese");
    assert_eq!(v["sets"], json!({"brc": {"cached": 1, "embedded": 0}}));
    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets?lang=Spanish", SESS))
        .await
        .unwrap();
    assert_eq!(body_val(res).await["lang"], "Spanish");
    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets?lang=%20JA%20", SESS))
        .await
        .unwrap();
    assert_eq!(body_val(res).await["lang"], "Japanese");
    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets?lang=zz", SESS))
        .await
        .unwrap();
    assert_eq!(body_val(res).await["lang"], "English");

    // A built catalog: meta ids decide `embedded` per set and `local`;
    // builtLangs lists the language that owns its own bin+meta pair.
    std::fs::write(
        model.join("milo-mtg-local.json"),
        serde_json::to_vec(&json!({"ids": ["e1", "ghost"], "dim": 4})).unwrap(),
    )
    .unwrap();
    std::fs::write(model.join("milo-mtg-local.bin"), [0u8; 32]).unwrap();
    std::fs::write(model.join("cornelius.onnx"), b"onnx").unwrap();
    std::fs::write(model.join("milo.onnx"), b"onnx").unwrap();
    std::fs::write(model.join("milo-mtg-japanese-local.json"), b"{\"ids\":[]}").unwrap();
    std::fs::write(model.join("milo-mtg-japanese-local.bin"), b"").unwrap();

    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets", SESS))
        .await
        .unwrap();
    let v = body_val(res).await;
    assert_eq!(v["sets"], json!({"brc": {"cached": 2, "embedded": 1}}));
    assert_eq!(v["local"], json!(true));
    assert_eq!(v["published"], json!(false));
    assert_eq!(v["builtLangs"], json!(["English", "Japanese"])); // table order, own bin+meta only
}

#[tokio::test]
async fn scan_sets_published_npz_adds_english_to_built_langs() {
    // cvScan.builtLangs: English also answers from a PUBLISHED .npz — not a
    // local build, so it rides at the front only when no English local pair
    // exists. And isBuilt lets the same .npz carry `published` for any lang.
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("models");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(model.join("cornelius.onnx"), b"onnx").unwrap();
    std::fs::write(model.join("milo.onnx"), b"onnx").unwrap();
    std::fs::write(model.join("milo-mtg.npz"), b"npz").unwrap();
    std::fs::write(model.join("milo-mtg-japanese-local.json"), b"{\"ids\":[]}").unwrap();
    std::fs::write(model.join("milo-mtg-japanese-local.bin"), b"").unwrap();
    let r = scan_sets_router(dir.path(), model).await;

    let res = r
        .clone()
        .oneshot(get_uri("/api/scan-sets", SESS))
        .await
        .unwrap();
    let v = body_val(res).await;
    assert_eq!(v["local"], json!(false));
    assert_eq!(v["published"], json!(true)); // English fallback via .npz
    assert_eq!(v["builtLangs"], json!(["English", "Japanese"])); // npz-English front, ja own pair

    // With an English local pair present the npz must not double-list it.
}

#[tokio::test]
async fn scan_sets_missing_langs_or_model_proxies() {
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("models");
    std::fs::create_dir_all(&model).unwrap();
    let shared = write_shared(dir.path());
    let conn = seeded_db(dir.path(), "db.sqlite");
    drop(conn);
    let node = spawn_fake_node().await;
    let up = format!("http://{node}");

    // No languages table => cannot answer like Node => proxy.
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: up.clone(),
        static_dir: None,
        rust_routes: ["catalog_art".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        db_path: Some(dir.path().join("db.sqlite")),
        sidecar_url: None,
    };
    let st = AppState::new(cfg);
    let res = app(st)
        .oneshot(get_uri("/api/scan-sets", SESS))
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").unwrap(), "1");

    // Languages present, model dir unset => also proxy (no silent empties).
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: up,
        static_dir: None,
        rust_routes: ["catalog_art".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        db_path: Some(dir.path().join("db.sqlite")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.shared_dir = Some(shared);
    st.model_dir = None;
    let res = app(st)
        .oneshot(get_uri("/api/scan-sets", SESS))
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
}

// ---------------------------------------------------------------------------
// GET /api/admin/catalogs/languages
// ---------------------------------------------------------------------------

fn langs_router(dir: &std::path::Path, model: std::path::PathBuf) -> Router {
    let shared = write_shared(dir);
    let conn = seeded_db(dir, "db.sqlite");
    card(&conn, "j1", "brc", "Japanese", Some("https://x/j1.jpg"));
    card(&conn, "j2", "brc", "Japanese", None);
    card(&conn, "d1", "all", "German", Some("https://x/d1.jpg"));
    card(&conn, "e1", "all", "English", Some("https://x/e1.jpg"));
    card(&conn, "n1", "all", NULL_LANG, None);
    drop(conn);
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: "http://127.0.0.1:1".into(),
        static_dir: None,
        rust_routes: ["catalog_art".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        db_path: Some(dir.join("db.sqlite")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.shared_dir = Some(shared);
    st.model_dir = Some(model);
    app(st)
}

const NULL_LANG: &str = "__NULL__"; // placeholder replaced by UPDATE below

#[tokio::test]
async fn catalog_languages_admin_ladder_and_shape() {
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("models");
    std::fs::create_dir_all(&model).unwrap();
    let r = langs_router(dir.path(), model.clone());
    {
        let conn = Connection::open(dir.path().join("db.sqlite")).unwrap();
        conn.execute(
            "UPDATE card_cache SET language = NULL WHERE id = ?1",
            [NULL_LANG],
        )
        .unwrap();
    }

    // Auth ladder: 401 no token, 401 bad token, 403 api-key on admin,
    // 403 member, then 200 for admin.
    let res = r
        .clone()
        .oneshot(get_uri("/api/admin/catalogs/languages", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Access denied. No token provided."})
    );
    let res = r
        .clone()
        .oneshot(get_uri(
            "/api/admin/catalogs/languages",
            Some("Bearer nope"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = r
        .clone()
        .oneshot(get_uri(
            "/api/admin/catalogs/languages",
            Some("Bearer tok-api"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_val(res).await,
        json!({"error": "API keys cannot access admin endpoints. Use a login session."})
    );
    let res = r
        .clone()
        .oneshot(get_uri(
            "/api/admin/catalogs/languages",
            Some("Bearer tok-member"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_val(res).await,
        json!({"error": "Access denied. Administrator privileges required."})
    );

    let res = r
        .clone()
        .oneshot(get_uri("/api/admin/catalogs/languages", SESS))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_val(res).await;
    let langs = v["languages"].as_array().unwrap();
    // English is excluded; table order preserved; NULL-language row lands on
    // the English bucket so 'es' shows zeros.
    assert_eq!(
        langs
            .iter()
            .map(|l| l["code"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["ja", "de", "es"]
    );
    assert_eq!(
        langs[0],
        json!({"game":"mtg","lang":"Japanese","code":"ja","cached":2,"withArt":1,"built":null})
    );
    assert_eq!(
        langs[2],
        json!({"game":"mtg","lang":"Spanish","code":"es","cached":0,"withArt":0,"built":null})
    );

    // built.rows rides the meta file when one exists.
    std::fs::write(
        model.join("milo-mtg-japanese-local.json"),
        serde_json::to_vec(&json!({"ids": ["j1", "j2"], "dim": 4})).unwrap(),
    )
    .unwrap();
    let res = r
        .clone()
        .oneshot(get_uri("/api/admin/catalogs/languages", SESS))
        .await
        .unwrap();
    let v = body_val(res).await;
    assert_eq!(v["languages"][0]["built"], json!({"rows": 2}));

    // Non-mtg game short-circuits to an empty list (still after the gate).
    let res = r
        .clone()
        .oneshot(get_uri("/api/admin/catalogs/languages?game=pokemon", SESS))
        .await
        .unwrap();
    assert_eq!(body_val(res).await, json!({"languages": []}));
    // Empty game param is falsy => mtg.
    let res = r
        .oneshot(get_uri("/api/admin/catalogs/languages?game=", SESS))
        .await
        .unwrap();
    assert_eq!(
        body_val(res).await["languages"].as_array().unwrap().len(),
        3
    );
}

#[tokio::test]
async fn catalog_languages_method_mismatch_proxies() {
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("models");
    std::fs::create_dir_all(&model).unwrap();
    let shared = write_shared(dir.path());
    let conn = seeded_db(dir.path(), "db.sqlite");
    drop(conn);
    let node = spawn_fake_node().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: ["catalog_art".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        db_path: Some(dir.path().join("db.sqlite")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.shared_dir = Some(shared);
    st.model_dir = Some(model);
    let res = app(st)
        .oneshot(
            Request::post("/api/admin/catalogs/languages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
}

// ---------------------------------------------------------------------------
// Write side (unit 6d): POST upload + DELETE.
// ---------------------------------------------------------------------------

fn png_ihdr(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "stored file is a PNG");
    assert_eq!(&bytes[12..16], b"IHDR");
    let be = |r: std::ops::Range<usize>| u32::from_be_bytes(bytes[r].try_into().unwrap());
    (be(16..20), be(20..24))
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

fn upload_body(file: &str) -> String {
    let raw = std::fs::read(format!(
        "{}/tests/fixtures/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    // tiny manual base64 (fixtures only; no dep for one call site).
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for ch in raw.chunks(3) {
        let n = (ch[0] as u32) << 16
            | (ch.get(1).copied().unwrap_or(0) as u32) << 8
            | ch.get(2).copied().unwrap_or(0) as u32;
        s.push(T[(n >> 18 & 63) as usize] as char);
        s.push(T[(n >> 12 & 63) as usize] as char);
        s.push(if ch.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        s.push(if ch.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    format!("{{\"image\":\"{s}\"}}")
}

async fn upload_router(dir: &std::path::Path) -> (Router, std::path::PathBuf) {
    let shared = write_shared(dir);
    let conn = seeded_db(dir, "db.sqlite");
    card(&conn, "ca-known", "s1", "en", None);
    drop(conn);
    let node = spawn_fake_node().await;
    let r = router_for(
        dir.join("db.sqlite"),
        &["catalog_art"],
        &format!("http://{node}"),
        Some(shared),
        None,
    )
    .await;
    (r, dir.join("card-art"))
}

#[tokio::test]
async fn upload_auth_ladder() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _user) = upload_router(dir.path()).await;
    // No token → 401 even with a bad id (auth runs before validation).
    let res = app
        .clone()
        .oneshot(post_json("/api/card-art/a..b", None, "{}"))
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(
        body_val(res).await["error"],
        "Access denied. No token provided."
    );
    // Bad token → 401 Invalid or expired.
    let res = app
        .clone()
        .oneshot(post_json(
            "/api/card-art/ca-known",
            Some("Bearer bogus"),
            "{}",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(
        body_val(res).await["error"],
        "Invalid or expired session token."
    );
    // API key on a write → read-only 403.
    let res = app
        .oneshot(post_json(
            "/api/card-art/ca-known",
            Some("Bearer tok-api"),
            "{}",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(
        body_val(res).await["error"],
        "API keys are read-only. Use a login session for this request."
    );
}

#[tokio::test]
async fn upload_rejects_before_touching_disk() {
    let dir = tempfile::tempdir().unwrap();
    let (app, user) = upload_router(dir.path()).await;
    // Invalid id (post-auth) → 400.
    let res = app
        .clone()
        .oneshot(post_json("/api/card-art/a..b", SESS, "{}"))
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(body_val(res).await["error"], "Invalid card id");
    // Unknown card → 404 BEFORE any image check.
    let res = app
        .clone()
        .oneshot(post_json(
            "/api/card-art/mtg-nope",
            SESS,
            r#"{"image":"YWJj"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    assert_eq!(body_val(res).await["error"], "Unknown card");
    // Known card, no image → 400.
    let res = app
        .clone()
        .oneshot(post_json("/api/card-art/ca-known", SESS, "{}"))
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(body_val(res).await["error"], "No image supplied");
    // Array body destructures image to undefined (strict:true ACCEPTS arrays,
    // pinned live) — same 400.
    let res = app
        .clone()
        .oneshot(post_json("/api/card-art/ca-known", SESS, "[]"))
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(body_val(res).await["error"], "No image supplied");
    // Undecodable payload → sharp's pinned message, error+message key order.
    let res = app
        .clone()
        .oneshot(post_json(
            "/api/card-art/ca-known",
            SESS,
            r#"{"image":"notbase64!!@@@"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let v = body_val(res).await;
    assert_eq!(v["error"], "Could not read that image");
    assert_eq!(
        v["message"],
        "Input buffer contains unsupported image format"
    );
    assert_eq!(
        v.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["error", "message"]
    );
    // data: prefix with NO comma → whole string is the b64 leg (indexOf -1).
    let res = app
        .oneshot(post_json(
            "/api/card-art/ca-known",
            SESS,
            r#"{"image":"data:image/png"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(
        body_val(res).await["message"],
        "Input buffer contains unsupported image format"
    );
    assert!(
        !user.exists(),
        "no upload may create USER_DIR on a failure arm"
    );
}

#[tokio::test]
async fn upload_body_parser_precedes_auth() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = upload_router(dir.path()).await;
    // Oversize (>1 MiB normalJson) WITHOUT a token → 413 from the parser.
    let big = format!("{{\"image\":\"{}\"}}", "A".repeat(1_200_000));
    let res = app
        .clone()
        .oneshot(post_json("/api/card-art/ca-known", None, &big))
        .await
        .unwrap();
    assert_eq!(res.status(), 413);
    assert_eq!(
        body_val(res).await["error"],
        "Upload too large. Try exporting/importing in smaller batches."
    );
    // Malformed JSON + scalar JSON → global handler generic 500 (pre-auth too).
    let res = app
        .clone()
        .oneshot(post_json("/api/card-art/ca-known", None, "{oops"))
        .await
        .unwrap();
    assert_eq!(res.status(), 500);
    assert_eq!(body_val(res).await["error"], "Internal server error");
    let res = app
        .oneshot(post_json("/api/card-art/ca-known", None, "\"str\""))
        .await
        .unwrap();
    assert_eq!(res.status(), 500);
    assert_eq!(body_val(res).await["error"], "Internal server error");
}

#[tokio::test]
async fn upload_roundtrip_resize_and_delete_legs() {
    let dir = tempfile::tempdir().unwrap();
    let (app, user) = upload_router(dir.path()).await;
    // Small upload: no resize, reported width = source width.
    let res = app
        .clone()
        .oneshot(post_json(
            "/api/card-art/ca-known",
            SESS,
            &upload_body("ca_small.png"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let v = body_val(res).await;
    assert_eq!(
        v.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["success", "url", "bytes", "width"]
    );
    assert_eq!(v["success"], true);
    assert_eq!(v["url"], "/api/card-art/ca-known.png");
    assert_eq!(v["width"], 100);
    let stored = user.join("ca-known.png");
    let bytes = std::fs::read(&stored).unwrap();
    assert_eq!(
        bytes.len(),
        v["bytes"].as_u64().unwrap() as usize,
        "bytes == stored length"
    );
    assert_eq!(png_ihdr(&bytes), (100, 140));
    // GET index sees it (fresh scan), GET png serves it.
    let res = app
        .clone()
        .oneshot(get_uri("/api/card-art/index", None))
        .await
        .unwrap();
    assert_eq!(body_val(res).await["ids"][0], "ca-known");
    // Big upload: width-capped by the lanczos twin, height rounded.
    let res = app
        .clone()
        .oneshot(post_json(
            "/api/card-art/ca-known",
            SESS,
            &upload_body("ca_big.png"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let v = body_val(res).await;
    assert_eq!(v["width"], 745);
    assert_eq!(png_ihdr(&std::fs::read(&stored).unwrap()), (745, 1076));
    // DELETE → success, no bundled layer.
    let del = |app: Router| async move {
        app.oneshot(
            Request::delete("/api/card-art/ca-known")
                .header("authorization", "Bearer tok-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    };
    let res = del(app.clone()).await;
    assert_eq!(res.status(), 200);
    let v = body_val(res).await;
    assert_eq!(v["success"], true);
    assert_eq!(v["hasBundled"], false);
    assert!(!stored.exists());
    // Second DELETE and invalid-id DELETE → 404 shape; index is empty again.
    let res = del(app.clone()).await;
    assert_eq!(res.status(), 404);
    assert_eq!(
        body_val(res).await["error"],
        "No uploaded art for this card"
    );
    // Bundled layer → hasBundled true after removing the user copy.
    let bundled = dir.path().join("shared/card-art");
    write_png(&bundled.join("ca-known.png"), b"B");
    let res = app
        .clone()
        .oneshot(post_json(
            "/api/card-art/ca-known",
            SESS,
            &upload_body("ca_small.png"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let res = del(app.clone()).await;
    assert_eq!(body_val(res).await["hasBundled"], true);
    // PUT (other method) rides the proxy — Node's router falls through to 404.
    let res = app
        .clone()
        .oneshot(
            Request::put("/api/card-art/ca-known")
                .header("authorization", "Bearer tok-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn write_routes_proxied_when_group_off() {
    let dir = tempfile::tempdir().unwrap();
    let shared = write_shared(dir.path());
    let conn = seeded_db(dir.path(), "db.sqlite");
    card(&conn, "ca-known", "s1", "en", None);
    drop(conn);
    let node = spawn_fake_node().await;
    // catalog_art NOT owned: both write legs must ride the catch-all proxy.
    let app = router_for(
        dir.path().join("db.sqlite"),
        &[],
        &format!("http://{node}"),
        Some(shared),
        None,
    )
    .await;
    let res = app
        .clone()
        .oneshot(post_json("/api/card-art/ca-known", SESS, "{}"))
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
    let res = app
        .oneshot(
            Request::delete("/api/card-art/ca-known")
                .header("authorization", "Bearer tok-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers().get("x-node").unwrap(), "1");
}
