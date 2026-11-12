//! Path-level tests for the iter-35 group: `precons`
//! (GET /api/precons?q=…, POST /api/precons/import).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (pc_* cases). These tests
//! pin the contract locally against the REAL routes/precons.js +
//! utils/preconData.js: kill-switch proxying, the mount-level auth gate
//! (EVERY /api/precons path, unknown subpaths included, is Node-401 — the
//! Rust handlers forward unowned shapes so nothing 405s), the q fold
//! (qs array → JS join, String(undefined) → 'q is required'), the mirror
//! lifecycle ({fetchedAt ISO, mtgjsonDate, decks} doc; cache serve with zero
//! upstream hits; stale fallback; no-mirror failure → 502 'Precon index
//! unavailable'), the SetList best-effort leg, PRECON_TYPES filtering, the
//! rankPrecons 0/1/2 score + _at + localeCompare ladder, the 50 cap, the
//! fileName strip (sanitize-then-empty → 400), deck-fetch axios rejection →
//! 500, malformed file → 502, the uniq/cardIdsByName folds, the
//! source='precon' insert, and the whole-request forwards (unmatched row /
//! Scryfall client error) that keep Node-authored answers byte-exact.

mod mock_scryfall;

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, precons, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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
    let health = get(|| async { (StatusCode::OK, axum::Json(json!({"status":"ok"}))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

/// Scripted MTGJSON v5 stand-in: DeckList/SetList/decks/<name>.json bodies
/// come from shared state; a Null body answers 500 (axios rejection with the
/// standard message), `deck_status` flips the deck route status.
struct MtgjsonState {
    deck_list: std::sync::Mutex<Value>,
    set_list: std::sync::Mutex<Value>,
    deck: std::sync::Mutex<Value>,
    deck_status: std::sync::Mutex<u16>,
    hits: std::sync::Mutex<Vec<String>>,
}

impl MtgjsonState {
    fn new() -> Self {
        Self {
            deck_list: std::sync::Mutex::new(Value::Null),
            set_list: std::sync::Mutex::new(Value::Null),
            deck: std::sync::Mutex::new(Value::Null),
            deck_status: std::sync::Mutex::new(200),
            hits: std::sync::Mutex::new(Vec::new()),
        }
    }
}

async fn spawn_mtgjson(st: Arc<MtgjsonState>) -> SocketAddr {
    let s = st.clone();
    let deck_list = get(move || {
        let s = s.clone();
        async move {
            s.hits.lock().unwrap().push("decklist".into());
            if s.deck_list.lock().unwrap().is_null() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            axum::Json(s.deck_list.lock().unwrap().clone()).into_response()
        }
    });
    let s = st.clone();
    let set_list = get(move || {
        let s = s.clone();
        async move {
            s.hits.lock().unwrap().push("setlist".into());
            if s.set_list.lock().unwrap().is_null() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            axum::Json(s.set_list.lock().unwrap().clone()).into_response()
        }
    });
    let s = st.clone();
    let deck = get(move || {
        let s = s.clone();
        async move {
            s.hits.lock().unwrap().push("deck".into());
            let status = *s.deck_status.lock().unwrap();
            if status != 200 {
                return StatusCode::from_u16(status).unwrap().into_response();
            }
            axum::Json(s.deck.lock().unwrap().clone()).into_response()
        }
    });
    let r = Router::new()
        .route("/api/v5/DeckList.json", deck_list)
        .route("/api/v5/SetList.json", set_list)
        .route("/api/v5/decks/{name}", deck);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

/// One DeckList.json index row (writeIndex maps EXACTLY these six source
/// fields; anything else on the row is dropped by Node too).
fn idx_deck(name: &str, code: &str, type_: &str) -> Value {
    json!({ "name": name, "code": code, "type": type_, "releaseDate": "2024-05-01",
            "fileName": format!("{code}_Deck.json") })
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
           ('sess-member', 2, '2099-01-01 00:00:00');",
    )
    .unwrap();
    path
}

struct Fix {
    router: Router,
    mtg: Arc<MtgjsonState>,
    scry: Arc<mock_scryfall::MockState>,
    index_path: PathBuf,
    db_path: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture(routes: &[&str]) -> Fix {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), "precons.db");
    let node = spawn_fake_node().await;
    let mtg = Arc::new(MtgjsonState::new());
    let mtg_addr = spawn_mtgjson(mtg.clone()).await;
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
    st.mtgjson_base = format!("http://{mtg_addr}/api/v5");
    let index_path = tmp.path().join("precons.json");
    st.precons_index_path = index_path.clone();
    Fix { router: app(st), mtg, scry, index_path, db_path: db, _tmp: tmp }
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

const ADMIN: [(&str, &str); 1] = [("authorization", "Bearer sess-admin")];
const MEMBER: [(&str, &str); 1] = [("authorization", "Bearer sess-member")];
const ADMIN_JSON: [(&str, &str); 2] =
    [("authorization", "Bearer sess-admin"), ("content-type", "application/json")];

// ---------------------------------------------------------------------------
// Pure-semantics pins
// ---------------------------------------------------------------------------

#[test]
fn is_precon_whitelist() {
    let t = |ty: &str| precons::is_precon(&json!({"type": ty}));
    assert!(t("Commander Deck"));
    assert!(t("Box Set"));
    assert!(!t("Jumpstart Deck"));
    assert!(!t("Token Pack"));
    assert!(!t(""));
    assert!(!precons::is_precon(&json!({"type": 5})));
    assert!(!precons::is_precon(&json!({})));
}

#[test]
fn clean_file_name_strips_foreign_runs() {
    assert_eq!(precons::clean_file_name("tcd_Deck"), "tcd_Deck");
    assert_eq!(precons::clean_file_name("etc/passwd.json"), "etcpasswdjson");
    assert_eq!(precons::clean_file_name("../../etc/passwd"), "etcpasswd");
    assert_eq!(precons::clean_file_name("///"), "");
    assert_eq!(precons::clean_file_name("A1_b9"), "A1_b9");
}

#[test]
fn rank_score_ladder_and_sort() {
    let decks = vec![
        idx_deck("Sol Ring", "sr1", "Commander Deck"), // exact name (a name substring) → 0
        idx_deck("Sol Ring Promo", "sr2", "Commander Deck"), // name substring → 0
        idx_deck("Commander's Ball", "sol ring3", "Commander Deck"), // hay hit via code → 1
        idx_deck("Solar Ring Legacy", "sr4", "Commander Deck"), // subsequence s·o·l·' '·r·i·n·g → 2
    ];
    let ranked = precons::rank_precons(&decks, "sol ring");
    let names: Vec<&str> = ranked.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["Sol Ring", "Sol Ring Promo", "Commander's Ball", "Solar Ring Legacy"]
    );
    assert_eq!(ranked[0]["_score"], 0);
    assert_eq!(ranked[1]["_score"], 0);
    assert_eq!(ranked[2]["_score"], 1);
    assert_eq!(ranked[3]["_score"], 2);
    // _at = name.indexOf(needle): prefix → 0; name lacking needle → -1.
    // The score-0 tie breaks on _at (both 0) then localeCompare — the
    // shorter name sorts first.
    assert_eq!(ranked[0]["_at"], 0);
    assert_eq!(ranked[1]["_at"], 0);
    assert_eq!(ranked[2]["_at"], -1);
    assert_eq!(ranked[3]["_at"], -1);
    // localeCompare twin on RAW names: plain ASCII folds case.
    let decks = vec![
        idx_deck("beta", "b", "Commander Deck"),
        idx_deck("Alpha", "a", "Commander Deck"),
        idx_deck("alpha beta", "ab", "Commander Deck"),
    ];
    let ranked = precons::rank_precons(&decks, "a");
    let names: Vec<&str> = ranked.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Alpha", "alpha beta", "beta"]);
    // No match at all (no hay hit, no subsequence) → dropped.
    let decks = vec![idx_deck("Zzz", "z", "Commander Deck")];
    assert!(precons::rank_precons(&decks, "qqq").is_empty());
}

// ---------------------------------------------------------------------------
// GET /api/precons
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_off_proxies() {
    let f = fixture(&["collection"]).await;
    let (s, h, b) = request(&f.router, "GET", "/api/precons?q=x", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["pq"], "/api/precons?q=x");
    let (s, h, _) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some("{}")).await;
    assert_eq!(s, 200);
    assert_eq!(h.get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn auth_gate_401_and_member_ok() {
    let f = fixture(&["precons"]).await;
    *f.mtg.deck_list.lock().unwrap() =
        json!({"meta":{"date":"d"},"data":[idx_deck("Sol thing","s","Commander Deck")]});
    *f.mtg.set_list.lock().unwrap() = json!({"data":[]});
    let (s, _, b) = request(&f.router, "GET", "/api/precons?q=sol", &[], None).await;
    assert_eq!(s, 401);
    assert_eq!(json_body(&b)["error"], "Access denied. No token provided.");
    let (s, _, _) = request(&f.router, "GET", "/api/precons?q=sol", &MEMBER, None).await;
    assert_eq!(s, 200);
    // Unknown subpath: forwarded (Rust never 404/405s it locally).
    let (s, h, _) = request(&f.router, "GET", "/api/precons/bogus", &[], None).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(s, 200);
}

#[tokio::test]
async fn q_required_fold() {
    let f = fixture(&["precons"]).await;
    for q in ["", "?q=", "?q=%20%20", "?other=1"] {
        let (s, _, b) = request(&f.router, "GET", &format!("/api/precons{q}"), &ADMIN, None).await;
        assert_eq!(s, 400, "{q}");
        assert_eq!(json_body(&b)["error"], "q is required");
    }
    *f.mtg.deck_list.lock().unwrap() = json!({"meta":{"date":"d"},"data":[
        idx_deck("Alpha", "a", "Commander Deck"), idx_deck("Beta", "b", "Commander Deck")]});
    *f.mtg.set_list.lock().unwrap() = json!({"data":[]});
    // REPEATED keys build a qs ARRAY — String(['zzz','alpha']) ===
    // 'zzz,alpha' searches for the joined needle (last-wins would match).
    let (s, _, b) = request(&f.router, "GET", "/api/precons?q=zzz&q=alpha", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert!(json_body(&b)["results"].as_array().unwrap().is_empty());
    // Single key behaves normally.
    let (s, _, b) = request(&f.router, "GET", "/api/precons?q=alpha", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(json_body(&b)["results"].as_array().unwrap().len(), 1);
    // malformed percent-escape: qs's decoder SWALLOWS the URIError and keeps
    // the raw '%zz' as the needle (container qs probe) — the request answers
    // 200 with an empty result set, NOT a 400.
    let (s, _, b) = request(&f.router, "GET", "/api/precons?q=%zz", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert!(json_body(&b)["results"].as_array().unwrap().is_empty());
    // Bracket forms (live qs probe): `q[0]=` builds an array slot -> needle
    // 'alpha' matches; `q[k]=` is an object -> '[object Object]' searches
    // empty. Both 200.
    let (s, _, b) = request(&f.router, "GET", "/api/precons?q%5B0%5D=alpha", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(json_body(&b)["results"].as_array().unwrap().len(), 1);
    let (s, _, b) = request(&f.router, "GET", "/api/precons?q%5Bk%5D=alpha", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert!(json_body(&b)["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn fresh_download_mirror_filter_rank_cap() {
    let f = fixture(&["precons"]).await;
    let mut decks: Vec<Value> =
        (0..60).map(|i| idx_deck(&format!("Testdeck {i:02}"), &format!("td{i:02}"), "Commander Deck")).collect();
    decks.push(idx_deck("Jumpstuff", "js", "Jumpstart Deck")); // filtered out
    decks.push(idx_deck("Tokend", "tk", "Token Pack")); // filtered out
    decks.push({
        // writeIndex drops rows without name OR fileName.
        let mut d = idx_deck("", "noName", "Commander Deck");
        d["fileName"] = Value::Null;
        d
    });
    *f.mtg.deck_list.lock().unwrap() = json!({"meta":{"date":"2026-11-01T00:00:00.000Z"},"data":decks});
    *f.mtg.set_list.lock().unwrap() =
        json!({"data":[{"code":"TD00","name":"Test Deck Zero Zero"},{"code":"td01","name":null}]});
    let (s, h, b) = request(&f.router, "GET", "/api/precons?q=testdeck", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "application/json; charset=utf-8");
    let body = json_body(&b);
    assert_eq!(body["stale"], false);
    assert_eq!(body["mtgjsonDate"], "2026-11-01T00:00:00.000Z");
    assert_eq!(body["results"].as_array().unwrap().len(), 50); // slice(0, 50)
    // writeIndex six-key shape, order pinned.
    assert_eq!(
        body["results"][0].as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        vec!["name", "code", "setName", "type", "releaseDate", "fileName"]
    );
    assert_eq!(body["results"][0]["name"], "Testdeck 00");
    // setName: UPPER(code) lookup; the null-name SetList row was skipped → ''.
    assert_eq!(body["results"][0]["setName"], "Test Deck Zero Zero");
    assert_eq!(body["results"][1]["setName"], "");
    // Mirror written like writeIndex: key order + ISO fetchedAt.
    let mirror: Value = serde_json::from_slice(&std::fs::read(&f.index_path).unwrap()).unwrap();
    assert_eq!(
        mirror.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        vec!["fetchedAt", "mtgjsonDate", "decks"]
    );
    assert!(mirror["fetchedAt"].as_str().unwrap().ends_with('Z'));
    assert_eq!(mirror["mtgjsonDate"], "2026-11-01T00:00:00.000Z");
    // Second request: mirror is fresh with setName — ZERO upstream hits.
    let before = f.mtg.hits.lock().unwrap().len();
    let (s, _, _) = request(&f.router, "GET", "/api/precons?q=testdeck", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(f.mtg.hits.lock().unwrap().len(), before);
}

#[tokio::test]
async fn setlist_failure_is_best_effort() {
    // DeckList OK, SetList dead → setName '' everywhere; hasSetNames false →
    // the mirror never satisfies freshness (Node refetches every call).
    let f = fixture(&["precons"]).await;
    *f.mtg.deck_list.lock().unwrap() =
        json!({"meta":{"date":"d"},"data":[idx_deck("Alpha Deck","a","Commander Deck")]});
    let (s, _, b) = request(&f.router, "GET", "/api/precons?q=alpha", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(json_body(&b)["results"][0]["setName"], "");
    let before = f.mtg.hits.lock().unwrap().len();
    let (s, _, _) = request(&f.router, "GET", "/api/precons?q=alpha", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert!(f.mtg.hits.lock().unwrap().len() > before, "no-setName mirror must not cache");
}

#[tokio::test]
async fn stale_fallback_and_502() {
    // Old mirror + dead download: serve the mirror, stale=true.
    let f = fixture(&["precons"]).await;
    let old_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        - (25 * 60 * 60 * 1000);
    let old_iso = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(old_ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    std::fs::write(
        &f.index_path,
        json!({"fetchedAt": old_iso, "mtgjsonDate": "2026-10-01T00:00:00.000Z",
               "decks": [{"name":"Old Mirror Deck","code":"om","setName":"OM Set","type":"Commander Deck","releaseDate":"2020-01-01","fileName":"om_Deck.json"}]})
            .to_string(),
    )
    .unwrap();
    let (s, _, b) = request(&f.router, "GET", "/api/precons?q=old", &ADMIN, None).await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["stale"], true);
    assert_eq!(body["mtgjsonDate"], "2026-10-01T00:00:00.000Z");
    assert_eq!(body["results"][0]["name"], "Old Mirror Deck");
    // No mirror + dead download: 502 with the Node message shape.
    let f2 = fixture(&["precons"]).await;
    let (s, _, b) = request(&f2.router, "GET", "/api/precons?q=x", &ADMIN, None).await;
    assert_eq!(s, 502);
    assert_eq!(
        json_body(&b)["error"],
        "Precon search failed: Precon index unavailable (Request failed with status code 500)"
    );
    // Unparseable fetchedAt + setName → Date.parse NaN → age false → serves
    // from cache forever without any upstream hit.
    let f3 = fixture(&["precons"]).await;
    std::fs::write(
        &f3.index_path,
        json!({"fetchedAt":"garbage","mtgjsonDate":"x","decks":[{"name":"G","code":"g","setName":"G Set","type":"Commander Deck","releaseDate":"","fileName":"g.json"}]}).to_string(),
    )
    .unwrap();
    let (s, _, b) = request(&f3.router, "GET", "/api/precons?q=g", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(json_body(&b)["stale"], false); // source 'cache', not 'stale'
    assert!(f3.mtg.hits.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// POST /api/precons/import
// ---------------------------------------------------------------------------

async fn ok_import_fix() -> Fix {
    let f = fixture(&["precons"]).await;
    // Uniq rows to resolve: (10e|1 qty 4+2), (m19|9 qty 'x'→1),
    // (m19|1 qty 1 commander). sideboard excluded by SECTIONS.
    *f.mtg.deck.lock().unwrap() = json!({"data":{
        "name": "Test Commander Deck", "code": "tcd", "type": "Commander Deck",
        "releaseDate": "2024-05-01",
        "mainBoard": [
            {"setCode":"10E","number":"1","count":4,"name":"Card A","identifiers":{"scryfallId":"aaa"}},
            {"setCode":"10e","number":"1","count":2,"name":"Card A dup"},
            {"setCode":"M19","number":"9","count":"x","name":"Card C"}
        ],
        "commander": [
            {"setCode":"M19","number":"1","count":1,"name":"Card B","identifiers":{"scryfallId":"bbb"}}
        ],
        "sideboard": [
            {"setCode":"M19","number":"5","count":3,"name":"Never"}
        ]
    }});
    *f.scry.collection_batches.lock().unwrap() = vec![json!({
        "data": [
            mock_scryfall::raw_card("aaa", "Card A", "10e", "1", Some(1.0)),
            mock_scryfall::raw_card("bbb", "Card B", "m19", "1", Some(2.0)),
            mock_scryfall::raw_card("ccc", "Card C", "m19", "9", Some(3.0)),
        ],
        "not_found": []
    })];
    f
}

#[tokio::test]
async fn import_happy_shape_and_db() {
    let f = ok_import_fix().await;
    let (s, h, b) = request(
        &f.router, "POST", "/api/precons/import", &ADMIN_JSON,
        Some(r#"{"fileName":"tcd_Deck"}"#),
    )
    .await;
    assert_eq!(s, 201);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "application/json; charset=utf-8");
    let body = json_body(&b);
    assert_eq!(
        body.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        vec!["message", "id", "cards", "notFound"]
    );
    assert_eq!(body["message"], "Precon imported: 3 card types in the deck");
    assert_eq!(body["cards"], 3);
    assert_eq!(body["notFound"], 0);
    let deck_id = body["id"].as_i64().expect(&format!("import forwarded: {body}"));
    assert_eq!(deck_id, 1);
    let conn = Connection::open(&f.db_path).unwrap();
    let row: (String, String, String, Option<String>, String, i64, String) = conn
        .query_row(
            "SELECT name, description, format, category, accent_color, target_size, source FROM decks WHERE id=?1",
            [deck_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .unwrap();
    assert_eq!(row.0, "Test Commander Deck");
    assert_eq!(row.1, "Preconstructed: Test Commander Deck (tcd)");
    assert_eq!(row.2, "Commander / EDH");
    assert_eq!(row.3, None);
    assert_eq!(row.4, "#8b5cf6");
    assert_eq!(row.5, 100);
    assert_eq!(row.6, "precon"); // the insert Node pins; the UI filters on it
    let cards: Vec<(String, i64, i64)> = conn
        .prepare("SELECT card_id, quantity, is_commander FROM deck_cards WHERE deck_id=?1 ORDER BY card_id")
        .unwrap()
        .query_map([deck_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // uniq fold: 10e|1 merged (4+2), count 'x' → 1, commander row included.
    assert_eq!(
        cards,
        vec![
            ("mtg-aaa".to_string(), 6, 0),
            ("mtg-bbb".to_string(), 1, 1),
            ("mtg-ccc".to_string(), 1, 0),
        ]
    );
    // cacheCards ran BEFORE deck writes (all three printings cached).
    let cached: i64 = conn
        .query_row("SELECT COUNT(*) FROM card_cache WHERE id IN ('mtg-aaa','mtg-bbb','mtg-ccc')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cached, 3);
}

#[tokio::test]
async fn import_reprint_fold_by_card_name() {
    // Three distinct uniq rows whose Scryfall cards share a canonical name →
    // ONE deck_card, summed quantity, commander flag OR-ed through the fold.
    let f = fixture(&["precons"]).await;
    *f.mtg.deck.lock().unwrap() = json!({"data":{
        "name":"Reprint Deck","code":"rp","type":"Commander Deck",
        "mainBoard":[
            {"setCode":"10E","number":"1","count":2},
            {"setCode":"M19","number":"5","count":3}
        ],
        "commander":[{"setCode":"M19","number":"7","count":1}]
    }});
    *f.scry.collection_batches.lock().unwrap() = vec![json!({
        "data": [
            mock_scryfall::raw_card("c1", "Shared Name", "10e", "1", None),
            mock_scryfall::raw_card("c2", "Shared Name", "m19", "5", None),
            mock_scryfall::raw_card("c3", "Shared Name", "m19", "7", None),
        ],
        "not_found": []
    })];
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(r#"{"fileName":"rp"}"#)).await;
    assert_eq!(s, 201);
    assert_eq!(json_body(&b)["cards"], 1);
    let conn = Connection::open(&f.db_path).unwrap();
    let cards: Vec<(String, i64, i64)> = conn
        .prepare("SELECT card_id, quantity, is_commander FROM deck_cards WHERE deck_id=1")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // Representative = FIRST pair in response order; quantity 2+3+1; the
    // commander pair sets the flag through the fold.
    assert_eq!(cards, vec![("mtg-c1".to_string(), 6, 1)]);
}

#[tokio::test]
async fn import_shape_fallbacks_and_overrides() {
    // name/code null on the deck file → 'Preconstructed Deck' + the
    // template literal 'Preconstructed: null'; non-commander type → shape.
    let f = fixture(&["precons"]).await;
    *f.mtg.deck.lock().unwrap() = json!({"data":{
        "name":null,"code":null,"type":"Duel Deck",
        "mainBoard":[{"setCode":"10E","number":"1","count":1}]
    }});
    *f.scry.collection_batches.lock().unwrap() =
        vec![json!({"data":[mock_scryfall::raw_card("aaa","Card A","10e","1",None)],"not_found":[]})];
    let (s, _, _) = request(
        &f.router, "POST", "/api/precons/import", &ADMIN_JSON,
        Some(r##"{"fileName":"dd","name":"","format":"Pauper","category":"Tournament","accentColor":"#123456","targetSize":"40"}"##),
    )
    .await;
    assert_eq!(s, 201);
    let conn = Connection::open(&f.db_path).unwrap();
    let row: (String, String, String, String, String, i64) = conn
        .query_row("SELECT name, description, format, category, accent_color, target_size FROM decks WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .unwrap();
    assert_eq!(row.0, "Preconstructed Deck"); // '' falsy → list.name null → default
    assert_eq!(row.1, "Preconstructed: null"); // `${list.name}` template of null
    assert_eq!(row.2, "Pauper");
    assert_eq!(row.3, "Tournament"); // `category || shape.category` — truthy body value flows
    assert_eq!(row.4, "#123456");
    assert_eq!(row.5, 40); // Number('40') finite override
    // An EXPLICIT null targetSize is Number(null) === 0 — finite → 0 sticks
    // (absent key would keep the Duel Deck default of 60).
    let f2 = fixture(&["precons"]).await;
    *f2.mtg.deck.lock().unwrap() = f.mtg.deck.lock().unwrap().clone();
    *f2.scry.collection_batches.lock().unwrap() = f.scry.collection_batches.lock().unwrap().clone();
    let (s, _, _) = request(&f2.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(r#"{"fileName":"dd","targetSize":null}"#)).await;
    assert_eq!(s, 201);
    let conn = Connection::open(&f2.db_path).unwrap();
    let size: i64 = conn.query_row("SELECT target_size FROM decks WHERE id=1", [], |r| r.get(0)).unwrap();
    assert_eq!(size, 0);
}

#[tokio::test]
async fn import_include_commander_gates() {
    let f = ok_import_fix().await;
    let (s, _, b) = request(
        &f.router, "POST", "/api/precons/import", &ADMIN_JSON,
        Some(r#"{"fileName":"tcd_Deck","includeCommander":false}"#),
    )
    .await;
    assert_eq!(s, 201);
    assert_eq!(json_body(&b)["cards"], 2); // commander row dropped
    // Explicit null is falsy (destructuring default fires only on undefined).
    let f2 = ok_import_fix().await;
    let (_, _, b) = request(
        &f2.router, "POST", "/api/precons/import", &ADMIN_JSON,
        Some(r#"{"fileName":"tcd_Deck","includeCommander":null}"#),
    )
    .await;
    assert_eq!(json_body(&b)["cards"], 2);
}

#[tokio::test]
async fn import_guard_order_and_guards() {
    let f = fixture(&["precons"]).await;
    // parse-before-auth: malformed JSON answers the generic 500 WITHOUT a token.
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &[("content-type", "application/json")], Some("{oops")).await;
    assert_eq!(s, 500);
    assert_eq!(json_body(&b)["error"], "Internal server error");
    // >1 MiB: the 413 shape, also before auth.
    let big = format!(r#"{{"fileName":"x","pad":"{}"}}"#, "a".repeat(1024 * 1024));
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &[("content-type", "application/json")], Some(&big)).await;
    assert_eq!(s, 413);
    assert_eq!(json_body(&b)["error"], "Upload too large. Try exporting/importing in smaller batches.");
    // No token → auth 401 (body parsed fine).
    let (s, _, _) = request(&f.router, "POST", "/api/precons/import", &[("content-type", "application/json")], Some(r#"{"fileName":"x"}"#)).await;
    assert_eq!(s, 401);
    // fileName guard AFTER auth.
    for body in ["{}", r#"{"fileName":""}"#, r#"{"fileName":5}"#, r#"{"fileName":null}"#] {
        let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(body)).await;
        assert_eq!(s, 400, "{body}");
        assert_eq!(json_body(&b)["error"], "fileName is required");
    }
    // All-stripped fileName → cleanFileName '' → the 400 wrapper + notFound 0.
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(r#"{"fileName":"///"}"#)).await;
    assert_eq!(s, 400);
    let body = json_body(&b);
    assert_eq!(body["error"], "Precon import failed: Invalid precon file name");
    assert_eq!(body["notFound"], 0);
}

#[tokio::test]
async fn import_deck_fetch_errors() {
    let f = fixture(&["precons"]).await;
    // AxiosError rejection: axios 1.18 attaches .status = response.status,
    // so the route's `err.status ? err.status : 500` answers 404 with the
    // verbatim axios message (live-pinned via parity pc_import_missing_deck).
    *f.mtg.deck_status.lock().unwrap() = 404;
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(r#"{"fileName":"nope"}"#)).await;
    assert_eq!(s, 404);
    let body = json_body(&b);
    assert_eq!(body["error"], "Precon import failed: Request failed with status code 404");
    assert_eq!(body["notFound"], 0);
    // 200 without data.mainBoard → Malformed 502.
    *f.mtg.deck_status.lock().unwrap() = 200;
    *f.mtg.deck.lock().unwrap() = json!({"data":{"name":"no mainboard"}});
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(r#"{"fileName":"nope"}"#)).await;
    assert_eq!(s, 502);
    assert_eq!(json_body(&b)["error"], "Precon import failed: Malformed MTGJSON deck file");
    // A pathy fileName SANITISES and reaches the decks route (not rejected);
    // empty mainBoard → no rows → the 422 local path.
    f.mtg.hits.lock().unwrap().clear();
    *f.mtg.deck.lock().unwrap() = json!({"data":{"name":"x","code":"x","type":"Commander Deck","mainBoard":[]}});
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(r#"{"fileName":"../x/zz"}"#)).await;
    assert_eq!(s, 422);
    assert_eq!(
        json_body(&b)["error"],
        "Precon import failed: None of this preconstructed deck's cards could be resolved to card printings"
    );
    assert!(f.mtg.hits.lock().unwrap().iter().any(|h| h == "deck"));
}

#[tokio::test]
async fn import_all_unresolvable_422() {
    // Pairs exist (nothing 'unmatched') but every card is filtered out of
    // cardIdsByName (blank names) → the Node 422 + notFound passthrough.
    let f = fixture(&["precons"]).await;
    *f.mtg.deck.lock().unwrap() = json!({"data":{"name":"x","code":"x","type":"Commander Deck",
        "mainBoard":[{"setCode":"10E","number":"1","count":1}]}});
    *f.scry.collection_batches.lock().unwrap() = vec![json!({
        "data": [mock_scryfall::raw_card("aaa", "   ", "10e", "1", None)],
        "not_found": ["one"]
    })];
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(r#"{"fileName":"x"}"#)).await;
    assert_eq!(s, 422);
    let body = json_body(&b);
    assert_eq!(body["error"], "Precon import failed: None of this preconstructed deck's cards could be resolved to card printings");
    assert_eq!(body["notFound"], 1);
    let conn = Connection::open(&f.db_path).unwrap();
    let decks: i64 = conn.query_row("SELECT COUNT(*) FROM decks", [], |r| r.get(0)).unwrap();
    assert_eq!(decks, 0);
}

#[tokio::test]
async fn import_partial_miss_forwards_whole_request() {
    // Scryfall answers only ONE of the uniq rows: Node would answer a
    // PARTIAL deck (cards>0, notFound>0) — Rust replays instead of
    // approximating; nothing local was written first.
    let f = ok_import_fix().await;
    *f.scry.collection_batches.lock().unwrap() = vec![json!({
        "data": [mock_scryfall::raw_card("aaa", "Card A", "10e", "1", Some(1.0))],
        "not_found": ["m19/9"]
    })];
    let (s, h, b) = request(
        &f.router, "POST", "/api/precons/import", &ADMIN_JSON,
        Some(r#"{"fileName":"tcd_Deck","name":"Replay Me"}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(h.get("x-node").unwrap(), "1");
    let fwd = json_body(&b);
    assert_eq!(fwd["method"], "POST");
    assert_eq!(fwd["pq"], "/api/precons/import");
    assert_eq!(fwd["body"], r#"{"fileName":"tcd_Deck","name":"Replay Me"}"#);
    let conn = Connection::open(&f.db_path).unwrap();
    let decks: i64 = conn.query_row("SELECT COUNT(*) FROM decks", [], |r| r.get(0)).unwrap();
    assert_eq!(decks, 0);
}

#[tokio::test]
async fn import_scryfall_client_error_forwards() {
    let f = ok_import_fix().await;
    // Persistent 5xx on /cards/collection (post_retried exhausts) → forward.
    f.scry.statuses.lock().unwrap().insert("POST /cards/collection".into(), vec![503]);
    let (s, h, b) = request(
        &f.router, "POST", "/api/precons/import", &ADMIN_JSON,
        Some(r#"{"fileName":"tcd_Deck"}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["body"], r#"{"fileName":"tcd_Deck"}"#);
}

#[tokio::test]
async fn import_batching_over_75() {
    let f = fixture(&["precons"]).await;
    let cards: Vec<Value> = (0..80)
        .map(|i| json!({"setCode":"10e","number":format!("{}", i + 1),"count":1}))
        .collect();
    *f.mtg.deck.lock().unwrap() =
        json!({"data":{"name":"big","code":"bg","type":"Commander Deck","mainBoard":cards}});
    let data: Vec<Value> = (0..80)
        .map(|i| mock_scryfall::raw_card(&format!("b{i:02}"), &format!("B{i}"), "10e", &(i + 1).to_string(), None))
        .collect();
    *f.scry.collection_batches.lock().unwrap() = vec![
        json!({"data": &data[..75], "not_found": []}),
        json!({"data": &data[75..], "not_found": []}),
    ];
    let (s, _, b) = request(&f.router, "POST", "/api/precons/import", &ADMIN_JSON, Some(r#"{"fileName":"bg"}"#)).await;
    assert_eq!(s, 201);
    assert_eq!(json_body(&b)["cards"], 80);
    assert_eq!(f.scry.hits_for("POST /cards/collection").len(), 2);
}
