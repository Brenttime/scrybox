//! Path-level tests for the iter-34 group: `settings`
//! (GET/PUT /api/settings, GET /api/settings/version).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (set_* cases). These
//! tests pin the contract locally against the REAL settings.js: kill-switch
//! proxying, method guards (nothing 405s), the auth/requireAdmin gates,
//! body-parser parse-before-auth order, the 11-key effective-settings fold
//! (incl. the no-row fold where price lands on 0 via Number(null)), the
//! PUT arm order with early 400 leaving earlier writes committed, the
//! !== undefined gates (an explicit null RUNS the arm), moxfield
//! parseInt-prefix clamping, the base-url scheme/trailing-slash rules and
//! the non-string crash divergence (Node dies on .trim, Rust answers the
//! route-catch 500), and the version route's check gate + 6 h update cache.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{self, any},
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, settings, AppState, Config};
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

/// Mock GitHub releases endpoint. `hits` counts SERVED /ok requests so a
/// cache hit (still 1) vs a re-fetch is distinguishable.
async fn spawn_mock_github(hits: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> SocketAddr {
    use std::sync::atomic::Ordering;
    let ok = routing::get(move || {
        hits.fetch_add(1, Ordering::SeqCst);
        async move {
            axum::Json(json!({
                "tag_name": "v9.9.9",
                "html_url": "https://example.test/releases/tag/v9.9.9",
                "published_at": "2026-11-01T00:00:00Z",
            }))
        }
    });
    let old = routing::get(|| async { axum::Json(json!({ "tag_name": "V1.0.0-beta" })) });
    let notag = routing::get(|| async { axum::Json(json!({ "html_url": "https://example.test/x" })) });
    let fail = routing::get(|| async { StatusCode::FORBIDDEN });
    let r = Router::new()
        .route("/ok", ok)
        .route("/old", old)
        .route("/notag", notag)
        .route("/fail", fail);
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
        "CREATE TABLE app_settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            public_base_url TEXT DEFAULT '',
            price_refresh_days INTEGER DEFAULT 1,
            scan_exclude_tokens INTEGER DEFAULT 0,
            scan_exclude_art_cards INTEGER DEFAULT 0,
            scan_exclude_jumpstart INTEGER DEFAULT 0,
            scan_exclude_promos INTEGER DEFAULT 0,
            setup_complete INTEGER DEFAULT 0,
            moxfield_decklist_interval_min INTEGER DEFAULT 60,
            moxfield_content_interval_min INTEGER DEFAULT 1
         );
         INSERT INTO app_settings (id, public_base_url, price_refresh_days,
             scan_exclude_tokens, scan_exclude_art_cards, scan_exclude_jumpstart,
             scan_exclude_promos, setup_complete, moxfield_decklist_interval_min,
             moxfield_content_interval_min)
           VALUES (1, '', 1, 1, 0, 0, 0, 1, 120, 5);
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'adm','x','admin','tok-a',0,NULL),
                  (2,'mem','x','member','tok-m',0,NULL),
                  (3,'key','x','admin','tok-k',0,'apikey59');
         INSERT INTO sessions (token, user_id, expires_at) VALUES
             ('sess-admin', 1, '2099-01-01 00:00:00'),
             ('sess-member', 2, '2099-01-01 00:00:00');",
    )
    .unwrap();
    path
}

fn env_for(url: &str) -> settings::SettingsEnv {
    settings::SettingsEnv {
        public_base_url: Some("https://portal.test:13310".into()),
        local_scryfall_enabled: Some("true".into()),
        local_web_portal_enabled: Some("TRUE".into()),
        app_version: settings::DEFAULT_APP_VERSION.into(),
        releases_api_url: url.into(),
    }
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

fn key_order(b: &[u8]) -> Vec<String> {
    json_body(b).as_object().unwrap().keys().cloned().collect()
}

const ADMIN: [(&str, &str); 1] = [("authorization", "Bearer sess-admin")];
const MEMBER: [(&str, &str); 1] = [("authorization", "Bearer sess-member")];
const ADMIN_JSON: [(&str, &str); 2] =
    [("authorization", "Bearer sess-admin"), ("content-type", "application/json")];

const EFFECTIVE_KEYS: [&str; 11] = [
    "public_base_url",
    "price_refresh_days",
    "scan_exclude_tokens",
    "scan_exclude_art_cards",
    "scan_exclude_jumpstart",
    "scan_exclude_promos",
    "setup_complete",
    "moxfield_decklist_interval_min",
    "moxfield_content_interval_min",
    "scryfall_local",
    "scryfall_portal_enabled",
];

// ---------------------------------------------------------------------------
// Pure-semantics pins (parseInt / isNewer / check gate)
// ---------------------------------------------------------------------------

#[test]
fn parse_int_10_matches_node() {
    assert_eq!(settings::parse_int_10_pub(&json!(" 45abc ")), Some(45.0));
    assert_eq!(settings::parse_int_10_pub(&json!("-7xyz")), Some(-7.0));
    assert_eq!(settings::parse_int_10_pub(&json!("09")), Some(9.0));
    assert_eq!(settings::parse_int_10_pub(&json!("")), None); // parseInt('') NaN
    assert_eq!(settings::parse_int_10_pub(&json!("abc")), None);
    assert_eq!(settings::parse_int_10_pub(&json!(null)), None); // parseInt(null) NaN
    assert_eq!(settings::parse_int_10_pub(&json!(true)), None); // parseInt(true) NaN
    assert_eq!(settings::parse_int_10_pub(&json!({})), None);
    assert_eq!(settings::parse_int_10_pub(&json!([1, 2])), Some(1.0)); // "1,2" -> 1
    assert_eq!(settings::parse_int_10_pub(&json!(2.5)), Some(2.0)); // "2.5" -> 2
}

#[test]
fn is_newer_matches_node() {
    // settings.js isNewer: replace(/^v/i,''), split '.', parseInt(n,10)||0,
    // compare at max length, missing parts 0.
    assert!(settings::is_newer(&json!("9.9.9"), "1.8.5"));
    assert!(settings::is_newer(&json!("v1.9.0"), "1.8.5"));
    assert!(settings::is_newer(&json!("V1.8.6"), "1.8.5")); // /i
    assert!(!settings::is_newer(&json!("1.8.5"), "1.8.5"));
    assert!(!settings::is_newer(&json!("1.8.4"), "1.8.5"));
    assert!(settings::is_newer(&json!("1.8.10"), "1.8.9")); // numeric, not lex
    assert!(!settings::is_newer(&json!("1.8"), "1.8.5")); // missing part 0 < 5
    assert!(settings::is_newer(&json!("1.8.5.1"), "1.8.5")); // extra part
    assert!(!settings::is_newer(&json!("garbage"), "1.8.5")); // [0]
    assert!(!settings::is_newer(&json!(""), "1.8.5"));
    assert!(!settings::is_newer(&json!(null), "1.8.5")); // "null" -> [0]
    assert!(settings::is_newer(&json!("2"), "1.8.5"));
    assert!(!settings::is_newer(&json!("1.x.5"), "1.8.5")); // [1,0,5]
}

#[test]
fn check_gate_requires_single_scalar_one() {
    assert!(settings::check_param_is_one_pub(Some("check=1")));
    assert!(settings::check_param_is_one_pub(Some("foo=a&check=1&bar=b")));
    assert!(!settings::check_param_is_one_pub(Some("check=0")));
    assert!(!settings::check_param_is_one_pub(Some("check=true")));
    assert!(!settings::check_param_is_one_pub(Some("foo=bar")));
    assert!(!settings::check_param_is_one_pub(None));
    // qs makes a repeated key an ARRAY: String(['1','1']) === '1,1' !== '1'.
    assert!(!settings::check_param_is_one_pub(Some("check=1&check=1")));
    assert!(!settings::check_param_is_one_pub(Some("check=1&check=0")));
    assert!(!settings::check_param_is_one_pub(Some("check=0&check=1")));
    // decoded exactly; '+' is space
    assert!(settings::check_param_is_one_pub(Some("check=%31")));
    assert!(!settings::check_param_is_one_pub(Some("check=1+")));
    assert!(!settings::check_param_is_one_pub(Some("check=")));
}

#[test]
fn app_version_pins_backend_package_json() {
    // settings.js: APP_VERSION = require('../../package.json').version.
    // BUMP BOTH TOGETHER (PROGRESS iter-34 note).
    let pkg: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../backend/package.json"
        ))
        .expect("backend/package.json"),
    )
    .unwrap();
    assert_eq!(pkg["version"].as_str().unwrap(), settings::DEFAULT_APP_VERSION);
}

// ---------------------------------------------------------------------------
// GET /api/settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_settings_body_shape_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s1.db");
    let node = spawn_fake_node().await;
    let r = router_for(db, &["settings"], &format!("http://{node}"));
    let (st, hd, body) = request(&r, "GET", "/api/settings", &ADMIN, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd[header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert_eq!(key_order(&body), EFFECTIVE_KEYS.to_vec());
    let v = json_body(&body);
    // Test process has none of the env vars set -> base '' and both flags
    // false; the seeded row drives the rest.
    assert_eq!(v["public_base_url"], json!(""));
    assert_eq!(v["price_refresh_days"], json!(1));
    assert_eq!(v["scan_exclude_tokens"], json!(true));
    assert_eq!(v["scan_exclude_art_cards"], json!(false));
    assert_eq!(v["setup_complete"], json!(true));
    assert_eq!(v["moxfield_decklist_interval_min"], json!(120));
    assert_eq!(v["moxfield_content_interval_min"], json!(5));
    assert_eq!(v["scryfall_local"], json!(false));
    assert_eq!(v["scryfall_portal_enabled"], json!(false));
    assert!(hd.get("x-node").is_none());
}

#[test]
fn effective_settings_env_and_no_row_folds() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "fold.db");
    let pool = db_pool(&db);
    // env half: strict === 'true' (no trim/lower); '' column falls to env.
    let env = env_for(settings::DEFAULT_RELEASES_API);
    let m = settings::get_effective_settings_pub(&pool, &env).unwrap();
    assert_eq!(m["public_base_url"], json!("https://portal.test:13310")); // '' is falsy
    assert_eq!(m["scryfall_local"], json!(true));
    assert_eq!(m["scryfall_portal_enabled"], json!(false)); // 'TRUE' !== 'true'
    let mut env2 = env.clone();
    env2.local_web_portal_enabled = Some("true".into());
    env2.public_base_url = Some("".into()); // set-empty: falsy -> ''
    let m = settings::get_effective_settings_pub(&pool, &env2).unwrap();
    assert_eq!(m["public_base_url"], json!(""));
    assert_eq!(m["scryfall_portal_enabled"], json!(true));

    // No-row fold: every read guarded by `row &&` — price = Number(null) = 0.
    let db2 = dir.path().join("norow.db");
    {
        let c = Connection::open(&db2).unwrap();
        c.execute_batch(include_str!("schema_fixture.sql")).unwrap();
        c.execute_batch(
            "CREATE TABLE app_settings (id INTEGER PRIMARY KEY CHECK (id = 1),
                public_base_url TEXT, price_refresh_days INTEGER,
                scan_exclude_tokens INTEGER, scan_exclude_art_cards INTEGER,
                scan_exclude_jumpstart INTEGER, scan_exclude_promos INTEGER,
                setup_complete INTEGER, moxfield_decklist_interval_min INTEGER,
                moxfield_content_interval_min INTEGER);",
        )
        .unwrap();
    }
    let m = settings::get_effective_settings_pub(&db_pool(&db2), &env).unwrap();
    assert_eq!(m["price_refresh_days"], json!(0));
    assert_eq!(m["scan_exclude_tokens"], json!(false));
    assert_eq!(m["moxfield_decklist_interval_min"], json!(60));
    assert_eq!(m["moxfield_content_interval_min"], json!(1));
}

#[tokio::test]
async fn auth_gates() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s2.db");
    let node = spawn_fake_node().await;
    let r = router_for(db, &["settings"], &format!("http://{node}"));
    let (st, _, b) = request(&r, "GET", "/api/settings", &[], None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(&b), json!({"error": "Access denied. No token provided."}));
    let (st, _, _) = request(&r, "GET", "/api/settings", &[("authorization", "Bearer nope")], None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // member GET is allowed (authenticateToken only).
    let (st, _, _) = request(&r, "GET", "/api/settings", &MEMBER, None).await;
    assert_eq!(st, StatusCode::OK);
    // member PUT -> requireAdmin 403.
    let (st, _, b) = request(
        &r,
        "PUT",
        "/api/settings",
        &[("authorization", "Bearer sess-member"), ("content-type", "application/json")],
        Some("{}"),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(&b),
        json!({"error": "Access denied. Administrator privileges required."})
    );
    // API key PUT -> authenticateToken's read-only gate fires first (the
    // user IS admin; requireAdmin's api-key message stays unreachable for
    // the PUT — pinned as the Node middleware order).
    let (st, _, b) = request(
        &r,
        "PUT",
        "/api/settings",
        &[("authorization", "Bearer apikey59"), ("content-type", "application/json")],
        Some("{}"),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(&b),
        json!({"error": "API keys are read-only. Use a login session for this request."})
    );
    // version is behind the /api authenticateToken mount too.
    let (st, _, _) = request(&r, "GET", "/api/settings/version", &[], None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _, _) = request(&r, "GET", "/api/settings/version", &ADMIN, None).await;
    assert_eq!(st, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// PUT /api/settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_price_arms() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s3.db");
    let node = spawn_fake_node().await;
    let r = router_for(db.clone(), &["settings"], &format!("http://{node}"));

    let (st, _, b) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"price_refresh_days\":7}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&b)["price_refresh_days"], json!(7));
    assert_eq!(db_col(&db, "price_refresh_days"), json!(7));

    let (st, _, _) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"price_refresh_days\":\"7\"}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "price_refresh_days"), json!(7));

    // 0 is VALID (means off).
    let (st, _, _) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"price_refresh_days\":0}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "price_refresh_days"), json!(0));

    // true -> Number(true)=1 -> writes 1.
    let (st, _, _) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"price_refresh_days\":true}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "price_refresh_days"), json!(1));

    // null RUNS the arm: Number(null)=0 -> writes 0.
    let (st, _, _) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"price_refresh_days\":null}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "price_refresh_days"), json!(0));

    // [1] -> Number([1]) === Number('1') === 1 (single-element array);
    // [] -> Number([]) === Number('') === 0. Both VALID.
    let (st, _, _) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"price_refresh_days\":[1]}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "price_refresh_days"), json!(1));
    let (st, _, _) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"price_refresh_days\":[]}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "price_refresh_days"), json!(0));

    for bad in ["2.5", "31", "-1", "\"7.9abc\"", "\"abc\"", "{}", "[1,2]"] {
        let body = format!("{{\"price_refresh_days\":{bad}}}");
        let (st, _, b) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some(&body)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            json_body(&b),
            json!({"error": "Price refresh interval must be a whole number of days from 0 (off) to 30."})
        );
    }
    assert_eq!(db_col(&db, "price_refresh_days"), json!(0)); // unchanged by 400s
}

#[tokio::test]
async fn put_scan_flags_and_early_400_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s4.db");
    let node = spawn_fake_node().await;
    let r = router_for(db.clone(), &["settings"], &format!("http://{node}"));

    // truthy string "false" -> 1 (JS truthiness, not parsing).
    let (st, _, _) =
        request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"scan_exclude_art_cards\":\"false\"}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "scan_exclude_art_cards"), json!(1));
    let (st, _, _) =
        request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"scan_exclude_art_cards\":0}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "scan_exclude_art_cards"), json!(0));
    // null RUNS the falsy arm -> 0.
    let (st, _, _) =
        request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"scan_exclude_art_cards\":null}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "scan_exclude_art_cards"), json!(0));
    let (st, _, _) =
        request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"setup_complete\":\"yes\"}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "setup_complete"), json!(1));

    // Arm ORDER: the price gate runs first; its 400 leaves the (later) scan
    // arm untouched — Node's early return is positional.
    let (st, _, _) = request(
        &r,
        "PUT",
        "/api/settings",
        &ADMIN_JSON,
        Some("{\"price_refresh_days\":-1,\"scan_exclude_jumpstart\":true}"),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(db_col(&db, "scan_exclude_jumpstart"), json!(0));

    // And the reverse: earlier arms COMMIT before a later arm's 400.
    let (st, _, _) = request(
        &r,
        "PUT",
        "/api/settings",
        &ADMIN_JSON,
        Some("{\"scan_exclude_jumpstart\":true,\"moxfield_decklist_interval_min\":\"abc\"}"),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(db_col(&db, "scan_exclude_jumpstart"), json!(1)); // committed
}

#[tokio::test]
async fn put_moxfield_intervals() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s5.db");
    let node = spawn_fake_node().await;
    let r = router_for(db.clone(), &["settings"], &format!("http://{node}"));

    // parseInt prefix: "45abc" -> 45.
    let (st, _, _) = request(
        &r,
        "PUT",
        "/api/settings",
        &ADMIN_JSON,
        Some("{\"moxfield_decklist_interval_min\":\"45abc\"}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "moxfield_decklist_interval_min"), json!(45));
    // clamp at 1440.
    let (st, _, _) =
        request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"moxfield_decklist_interval_min\":2000}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "moxfield_decklist_interval_min"), json!(1440));
    // 2.5 -> parseInt("2.5")=2 -> writes 2 (parseInt, not the Number gate).
    let (st, _, _) =
        request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"moxfield_decklist_interval_min\":2.5}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "moxfield_decklist_interval_min"), json!(2));

    for (key, msg) in [
        (
            "moxfield_decklist_interval_min",
            "Moxfield decklist interval must be a whole number of minutes (1–1440)",
        ),
        (
            "moxfield_content_interval_min",
            "Moxfield content interval must be a whole number of minutes (1–1440)",
        ),
    ] {
        for bad in ["0", "\"abc\"", "null", "true", "{}"] {
            let body = format!("{{\"{key}\":{bad}}}");
            let (st, _, b) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some(&body)).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(json_body(&b), json!({"error": msg}));
        }
    }
    assert_eq!(db_col(&db, "moxfield_content_interval_min"), json!(5)); // unchanged
}

#[tokio::test]
async fn put_public_base_url() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s6.db");
    let node = spawn_fake_node().await;
    let r = router_for(db.clone(), &["settings"], &format!("http://{node}"));

    // trim + trailing slash run stripped.
    let (st, _, b) = request(
        &r,
        "PUT",
        "/api/settings",
        &ADMIN_JSON,
        Some("{\"public_base_url\":\"  https://parity.test:9//  \"}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&b)["public_base_url"], json!("https://parity.test:9"));
    assert_eq!(db_col(&db, "public_base_url"), json!("https://parity.test:9"));

    // case-insensitive scheme gate; value preserved verbatim.
    let (st, _, _) =
        request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"public_base_url\":\"HTTPS://X.TEST\"}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "public_base_url"), json!("HTTPS://X.TEST"));

    // scheme required when non-empty.
    let (st, _, b) =
        request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"public_base_url\":\"ftp://x\"}")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"error": "Public base URL must start with http:// or https://"}));
    assert_eq!(db_col(&db, "public_base_url"), json!("HTTPS://X.TEST")); // unchanged

    // empty-after-trim still WRITES ''.
    let (st, _, _) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{\"public_base_url\":\"   \"}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_col(&db, "public_base_url"), json!(""));

    // non-string: Node dies (.trim TypeError outside the try); Rust
    // answers the route-catch 500 AFTER earlier arms committed.
    let (st, _, b) = request(
        &r,
        "PUT",
        "/api/settings",
        &ADMIN_JSON,
        Some("{\"price_refresh_days\":5,\"public_base_url\":0}"),
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({"error": "Internal server error"}));
    assert_eq!(db_col(&db, "price_refresh_days"), json!(5)); // committed first

    // empty object: all gates skip, still 200 with the 11-key fold.
    let (st, _, b) = request(&r, "PUT", "/api/settings", &ADMIN_JSON, Some("{}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(key_order(&b), EFFECTIVE_KEYS.to_vec());
}

#[tokio::test]
async fn put_body_parser_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s7.db");
    let node = spawn_fake_node().await;
    let r = router_for(db, &["settings"], &format!("http://{node}"));
    // malformed JSON parses BEFORE auth: 500 without any token.
    let (st, _, b) =
        request(&r, "PUT", "/api/settings", &[("content-type", "application/json")], Some("{oops")).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({"error": "Internal server error"}));
    // primitive JSON is a strict-mode rejection -> same 500.
    let (st, _, _) =
        request(&r, "PUT", "/api/settings", &[("content-type", "application/json")], Some("42")).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    // >1 MiB answers the error handler's 413 shape (before auth).
    let big = format!("{{\"public_base_url\":\"{}\"}}", "x".repeat(1024 * 1024));
    let (st, _, b) =
        request(&r, "PUT", "/api/settings", &[("content-type", "application/json")], Some(&big)).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        json_body(&b),
        json!({"error": "Upload too large. Try exporting/importing in smaller batches."})
    );
    // non-JSON content type: req.body = {} -> auth runs -> admin 200 no-op.
    let (st, _, _) = request(
        &r,
        "PUT",
        "/api/settings",
        &[("authorization", "Bearer sess-admin"), ("content-type", "text/plain")],
        Some("garbage"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// GET /api/settings/version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn version_check_gate_and_cache_legs() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s8.db");
    let node = spawn_fake_node().await;

    // no check / check!=1: exactly {version, releases_url}.
    let r = router_for(db.clone(), &["settings"], &format!("http://{node}"));
    for q in ["", "?check=0", "?check=true", "?check=1&check=1"] {
        let (st, _, b) = request(&r, "GET", &format!("/api/settings/version{q}"), &ADMIN, None).await;
        assert_eq!(st, StatusCode::OK, "{q}");
        assert_eq!(key_order(&b), vec!["version", "releases_url"]);
        let v = json_body(&b);
        assert_eq!(v["version"], json!("1.8.5"));
        assert_eq!(v["releases_url"], json!("https://github.com/Brenttime/scrybox/releases"));
    }

    // check=1 legs run through check_for_update_pub against the mock (the
    // route itself reads process env; no live GitHub in tests).
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gh = spawn_mock_github(hits.clone()).await;
    use std::sync::atomic::Ordering;
    let env = env_for(&format!("http://{gh}/ok"));
    let data = settings::check_for_update_pub(&env).await.unwrap();
    assert_eq!(
        data.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        vec!["latest", "update_available", "release_url", "published_at"]
    );
    assert_eq!(data["latest"], json!("9.9.9"));
    assert_eq!(data["update_available"], json!(true));
    assert_eq!(data["release_url"], json!("https://example.test/releases/tag/v9.9.9"));
    assert_eq!(data["published_at"], json!("2026-11-01T00:00:00Z"));
    // Cached: the second call must not reach the mock again.
    let _again = settings::check_for_update_pub(&env).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // V-prefix + beta suffix: 'V1.0.0-beta' -> '1.0.0-beta' -> parts
    // [1,0,0] not newer than 1.8.5; missing html_url/published_at fall back.
    let env_old = env_for(&format!("http://{gh}/old"));
    let data = settings::check_for_update_pub(&env_old).await.unwrap();
    assert_eq!(data["latest"], json!("1.0.0-beta"));
    assert_eq!(data["update_available"], json!(false));
    assert_eq!(data["release_url"], json!("https://github.com/Brenttime/scrybox/releases"));
    assert_eq!(data["published_at"], json!(null));

    // no tag_name: falsy -> latest '' -> update_available false.
    let env_notag = env_for(&format!("http://{gh}/notag"));
    let data = settings::check_for_update_pub(&env_notag).await.unwrap();
    assert_eq!(data["latest"], json!(""));
    assert_eq!(data["update_available"], json!(false));
    assert_eq!(data["published_at"], json!(null));

    // failed fetch: Err -> the route answers {...base, check_failed:true}
    // (200, not 502 — settings.js catches into check_failed).
    let env_fail = env_for(&format!("http://{gh}/fail"));
    let err = settings::check_for_update_pub(&env_fail).await.unwrap_err();
    assert_eq!(err, "Request failed with status code 403");
}

// ---------------------------------------------------------------------------
// Kill switch / method guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_proxies_everything() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s9.db");
    let node = spawn_fake_node().await;
    let r = router_for(db, &[], &format!("http://{node}"));
    for (method, path, body) in [
        ("GET", "/api/settings", None),
        ("PUT", "/api/settings", Some("{}")),
        ("GET", "/api/settings/version", None),
    ] {
        let (st, hd, b) = request(&r, method, path, &ADMIN_JSON, body).await;
        assert_eq!(st, StatusCode::OK, "{method} {path}");
        assert_eq!(hd.get("x-node").map(|_| true), Some(true), "{method} {path} must proxy");
        let v = json_body(&b);
        assert_eq!(v["method"], json!(method));
        assert!(v["pq"].as_str().unwrap().starts_with(path));
    }
}

#[tokio::test]
async fn other_methods_never_405() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path(), "s10.db");
    let node = spawn_fake_node().await;
    let r = router_for(db, &["settings"], &format!("http://{node}"));
    let (st, hd, _) = request(&r, "DELETE", "/api/settings", &ADMIN, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd.get("x-node").map(|_| true), Some(true));
    let (st, hd, _) = request(&r, "POST", "/api/settings/version", &ADMIN, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd.get("x-node").map(|_| true), Some(true));
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn db_pool(db: &PathBuf) -> std::sync::Arc<scrybox_server::db::DbPool> {
    std::sync::Arc::new(scrybox_server::db::DbPool::open(db, 2).unwrap())
}

fn db_col(db: &PathBuf, col: &str) -> Value {
    let c = Connection::open(db).unwrap();
    let v: rusqlite::types::Value = c
        .query_row(&format!("SELECT {col} FROM app_settings WHERE id = 1"), [], |r| r.get(0))
        .unwrap();
    match v {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(i) => Value::from(i),
        rusqlite::types::Value::Real(f) => Value::from(f),
        rusqlite::types::Value::Text(s) => Value::String(s),
        rusqlite::types::Value::Blob(_) => Value::String("<blob>".into()),
    }
}
