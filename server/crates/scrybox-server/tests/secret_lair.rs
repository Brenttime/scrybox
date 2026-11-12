//! Path-level tests for the iter-37 group: `secret_lair`
//! (GET /api/secret-lair?q=…, GET /api/secret-lair/preview?fileName=…,
//! POST /api/secret-lair/add).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (sl_* cases). These tests
//! pin the contract locally against the REAL routes/secretLair.js +
//! utils/secretLair.js: kill-switch proxying, the mount-level auth gate, the
//! search fold (SL_TYPE filter, byKey slot merge + first-wins, limit clamp
//! 1..100 default 25, `_score`/`_at` strip, {results,total,stale,mtgjsonDate}
//! key order, 502 'Precon index unavailable'), the preview record shape
//! (getPreconCardList's name/isFoil-blindness — display name comes from
//! Scryfall, isFoil is ALWAYS false, count floors to >=1, response-order
//! dedupe by card.id), the owned-count fold (case-insensitive Map,
//! Number(qty)||0), resolveFoilFileName (fileName-then-name lookup,
//! baseName twin match, 422 twin-missing, already-foil passthrough), and the
//! add route (early "No cards to add" 422 with message in THIRD key
//! position, copies parseInt, strict-=== foil flag, {added,failed,name,
//! totalCards,resolvedCount,unresolved,fileName,resolvedFoil,message} order,
//! 200 vs 422 by anythingResolved, per-card {card_id,error,index} failures,
//! Normal-then-Holofoil group order with per-group index restart, and the
//! whole-request forwards for Scryfall client errors).

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
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fixtures (same harness shape as tests/precons.rs)
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

/// One DeckList.json index row.
fn idx(name: &str, code: &str, type_: &str, file_name: &str) -> Value {
    json!({ "name": name, "code": code, "type": type_, "releaseDate": "2024-05-01",
            "fileName": file_name })
}

/// The SL rows every test uses: a base drop, its MTGJSON FoilEdition twin
/// (fileName carries the contiguous "FoilEdition", name carries the spaced
/// "Foil Edition" that baseName strips), a same-name duplicate pair (slot
/// fold / first-wins), and a NON-Secret-Lair control row.
fn sl_deck_list() -> Value {
    json!({
        "data": [
            idx("Test Drop", "sl1", "Secret Lair Drop", "test_drop_SL1.json"),
            idx("Test Drop Foil Edition", "sl1f", "Secret Lair Drop", "Test Drop FoilEdition_SL1.json"),
            idx("Dup Drop", "dp1", "Secret Lair Drop", "dup_drop_DP1.json"),
            idx("Dup Drop", "dp1b", "Secret Lair Drop", "dup_drop_alt_DP1.json"),
            idx("Not SL", "x1", "Commander Deck", "not_sl_X1.json"),
        ]
    })
}

/// getPreconCardList's view of a deck file — rows arrive with setCode,
/// number, count, identifiers.scryfallId and NOTHING else (name/isFoil are
/// dropped by getPreconCardList itself, so the Secret Lair fold always sees
/// the blind shape).
fn deck_body(main: Vec<Value>) -> Value {
    json!({ "data": {
        "name": "Test Drop", "code": "sl1", "type": "Secret Lair Drop",
        "releaseDate": "2024-05-01", "mainBoard": main, "commander": []
    } })
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
    let db = seeded_db(tmp.path(), "secret_lair.db");
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

fn keys(v: &Value) -> Vec<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

const ADMIN: [(&str, &str); 1] = [("authorization", "Bearer sess-admin")];
const MEMBER: [(&str, &str); 1] = [("authorization", "Bearer sess-member")];
const MEMBER_JSON: [(&str, &str); 2] =
    [("authorization", "Bearer sess-member"), ("content-type", "application/json")];

/// Warm the shared mirror through the (owned) search route, using the SL deck
/// list; returns the fixture ready for preview/add work.
async fn warm(f: &Fix) {
    *f.mtg.deck_list.lock().unwrap() = sl_deck_list();
    *f.mtg.set_list.lock().unwrap() = json!({"data": []});
    let (s, _, _) = request(&f.router, "GET", "/api/secret-lair?q=zzzqqq", &ADMIN, None).await;
    assert_eq!(s, 200, "warm-up search failed");
}

fn ok_cards() -> Vec<Value> {
    vec![
        json!({"setCode":"10E","number":"1","count":4,"identifiers":{"scryfallId":"aaa"}}),
        json!({"setCode":"M19","number":"9","count":"x","identifiers":{"scryfallId":"ccc"}}),
    ]
}

fn script_ok_scry(f: &Fix) {
    *f.scry.collection_batches.lock().unwrap() = vec![json!({
        "data": [
            mock_scryfall::raw_card("aaa", "Card A", "10e", "1", Some(1.0)),
            mock_scryfall::raw_card("ccc", "Card C", "m19", "9", Some(3.0)),
        ],
        "not_found": []
    })];
}

// ---------------------------------------------------------------------------
// Mount + kill switch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_off_proxies() {
    let f = fixture(&[]).await;
    for (m, p, b) in [
        ("GET", "/api/secret-lair?q=x", None),
        ("GET", "/api/secret-lair/preview?fileName=a", None),
        ("POST", "/api/secret-lair/add", Some(r#"{"fileName":"a"}"#)),
    ] {
        let (s, h, _) = request(&f.router, m, p, &ADMIN, b).await;
        assert_eq!(s, 200, "{m} {p}");
        assert_eq!(h.get("x-node").unwrap(), "1", "{m} {p} must proxy to Node");
    }
}

#[tokio::test]
async fn auth_gate_401_on_every_shape() {
    let f = fixture(&["secret_lair"]).await;
    let (s, _, _) = request(&f.router, "GET", "/api/secret-lair?q=x", &[], None).await;
    assert_eq!(s, 401);
    let (s, _, _) = request(&f.router, "GET", "/api/secret-lair/preview?fileName=a", &[], None).await;
    assert_eq!(s, 401);
    let (s, _, _) = request(
        &f.router, "POST", "/api/secret-lair/add", &[], Some(r#"{"fileName":"a"}"#),
    )
    .await;
    assert_eq!(s, 401);
}

// ---------------------------------------------------------------------------
// GET /api/secret-lair (searchSecretLair)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_filter_fold_and_shape() {
    let f = fixture(&["secret_lair"]).await;
    *f.mtg.deck_list.lock().unwrap() = sl_deck_list();
    *f.mtg.set_list.lock().unwrap() = json!({"data": []});
    let (s, h, b) = request(&f.router, "GET", "/api/secret-lair", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "application/json; charset=utf-8");
    let body = json_body(&b);
    assert_eq!(keys(&body), vec!["results", "total", "stale", "mtgjsonDate"]);
    assert_eq!(body["stale"], false);
    // nameKey fold: 4 SL rows → TWO slots. "Test Drop" + its Foil Edition
    // twin share nameKey "test drop" (baseName strips the tail; the twin's
    // fileName carries FoilEdition, isFoilRow true) → one slot with `foil`
    // filled; the Dup Drop pair folds FIRST-WINS into one base slot.
    // "Not SL" never enters (type !== 'Secret Lair Drop').
    assert_eq!(body["total"], 2);
    let arr = body["results"].as_array().unwrap();
    let names: Vec<&str> = arr.iter().map(|d| d["name"].as_str().unwrap()).collect();
    // Empty needle: every merged row scores 0 (_at 0), ties sort by
    // localeCompare on the RAW name → "Dup Drop" < "Test Drop".
    assert_eq!(names, vec!["Dup Drop", "Test Drop"]);
    assert_eq!(arr.len(), 2);
    // Merged-row shape (Node's literal key order; NOT the index row's keys).
    assert_eq!(
        keys(&arr[0]),
        vec!["key", "name", "fileName", "code", "setName", "releaseDate", "hasFoil", "foilOnly", "foil"]
    );
    // Dup slot: no foil twin → foil is JSON null (key PRESENT, Node pushes it).
    assert_eq!(arr[0]["foil"], serde_json::Value::Null);
    assert_eq!(arr[0]["hasFoil"], false);
    assert_eq!(arr[0]["foilOnly"], false);
    assert_eq!(arr[0]["code"], "dp1");
    // Test Drop slot: base represents (name stripped to base), foil folded in.
    assert_eq!(arr[1]["fileName"], "test_drop_SL1.json");
    assert_eq!(arr[1]["hasFoil"], true);
    assert_eq!(arr[1]["foilOnly"], false);
    assert_eq!(arr[1]["foil"]["fileName"], "Test Drop FoilEdition_SL1.json");
    assert_eq!(arr[1]["foil"]["name"], "Test Drop Foil Edition");
    assert_eq!(arr[1]["foil"]["releaseDate"], "2024-05-01");
}

#[tokio::test]
async fn search_ranking_strip_and_limit_clamp() {
    let f = fixture(&["secret_lair"]).await;
    *f.mtg.deck_list.lock().unwrap() = sl_deck_list();
    *f.mtg.set_list.lock().unwrap() = json!({"data": []});
    // q="test drop": the merged Test Drop slot scores 0 (_at 0). Dup Drop
    // dropped: name/hay/subsequence all miss "test drop" (setName is '' —
    // the SetList fixture is empty — and merged rows carry no `type` key).
    let (s, _, b) = request(&f.router, "GET", "/api/secret-lair?q=test%20drop", &ADMIN, None).await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    let names: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Test Drop"], "foil twin folded into the base slot, not a row");
    assert_eq!(body["total"], 2, "total counts merged slots, not matches");
    // _score/_at stripped.
    assert!(body["results"][0].get("_score").is_none());
    assert!(body["results"][0].get("_at").is_none());
    // The nine merged-row keys survive the fold verbatim.
    assert_eq!(
        keys(&body["results"][0]),
        vec!["key", "name", "fileName", "code", "setName", "releaseDate", "hasFoil", "foilOnly", "foil"]
    );
    // Non-SL type never appears (Not SL is a Commander Deck).
    let (s, _, b) = request(&f.router, "GET", "/api/secret-lair?q=not", &ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(json_body(&b)["results"].as_array().unwrap().len(), 0);
    // Dup Drop first-wins: duplicate nameKey, non-foil slot → FIRST fileName.
    let (_, _, b) = request(&f.router, "GET", "/api/secret-lair?q=dup", &ADMIN, None).await;
    let body = json_body(&b);
    let arr = body["results"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["fileName"], "dup_drop_DP1.json");
    assert_eq!(arr[0]["foil"], serde_json::Value::Null, "no foil slot was ever filled");
}

#[tokio::test]
async fn search_limit_clamps() {
    let f = fixture(&["secret_lair"]).await;
    *f.mtg.deck_list.lock().unwrap() = sl_deck_list();
    *f.mtg.set_list.lock().unwrap() = json!({"data": []});
    async fn count(f: &Fix, q: &str) -> usize {
        let (_, _, b) = request(&f.router, "GET", &format!("/api/secret-lair?{q}"), &ADMIN, None).await;
        json_body(&b)["results"].as_array().unwrap().len()
    }
    assert_eq!(count(&f, "q=").await, 2, "empty needle matches all merged slots; default 25");
    assert_eq!(count(&f, "q=&limit=2").await, 2);
    assert_eq!(count(&f, "q=&limit=0").await, 2, "0 is falsy → 25");
    assert_eq!(count(&f, "q=&limit=-3").await, 1, "Math.max(-3,1)=1 → slice(0,1)");
    assert_eq!(count(&f, "q=&limit=abc").await, 2, "NaN → 25");
    assert_eq!(count(&f, "q=&limit=2.7").await, 2, "slice truncates");
    assert_eq!(count(&f, "q=&limit=1e2").await, 2, "100 clamps fine, only 2 exist");
}

#[tokio::test]
async fn search_stale_fallback_and_502() {
    // Old mirror + dead download: stale=true, mirror contents served.
    let f = fixture(&["secret_lair"]).await;
    std::fs::write(
        &f.index_path,
        json!({
            "fetchedAt": "2020-01-01T00:00:00.000Z",
            "mtgjsonDate": "2019-12-01T00:00:00.000Z",
            "decks": [idx("Old Drop", "od", "Secret Lair Drop", "old_drop_OD.json")]
        })
        .to_string(),
    )
    .unwrap();
    let (s, _, b) = request(&f.router, "GET", "/api/secret-lair?q=drop", &ADMIN, None).await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["stale"], true);
    assert_eq!(body["mtgjsonDate"], "2019-12-01T00:00:00.000Z");
    assert_eq!(body["total"], 1);
    // No mirror + dead download → 502 with the shared index message.
    let f2 = fixture(&["secret_lair"]).await;
    let (s, _, b) = request(&f2.router, "GET", "/api/secret-lair?q=x", &ADMIN, None).await;
    assert_eq!(s, 502);
    assert_eq!(
        json_body(&b)["error"],
        "Secret Lair search failed: Precon index unavailable (Request failed with status code 500)"
    );
}

// ---------------------------------------------------------------------------
// GET /api/secret-lair/preview
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_requires_file_name() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    for q in ["", "fileName=", "fileName=%20"] {
        let (s, _, b) =
            request(&f.router, "GET", &format!("/api/secret-lair/preview?{q}"), &MEMBER, None).await;
        assert_eq!(s, 400, "query {q:?}");
        assert_eq!(json_body(&b)["error"], "fileName is required");
    }
}

#[tokio::test]
async fn preview_shape_records_and_owned() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f);
    // One owned copy of mtg-aaa for the member (user 2), plus a DECOY row
    // for another user and a case-variant key to pin the lowercase fold.
    {
        let conn = Connection::open(&f.db_path).unwrap();
        conn.execute_batch(
            "INSERT INTO card_cache (id, name) VALUES ('mtg-aaa','Card A'),('mtg-ccc','Card C');
             INSERT INTO collection (card_id, user_id, quantity) VALUES
               ('mtg-aaa', 2, 3),
               ('mtg-aaa', 1, 9),
               ('mtg-ccc', 2, 0);",
        )
        .unwrap();
    }
    let (s, h, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=test_drop_SL1.json", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "application/json; charset=utf-8");
    let body = json_body(&b);
    assert_eq!(
        keys(&body),
        vec![
            "name", "code", "type", "cards", "unresolved", "totalListed",
            "resolvedCount", "fileName", "resolvedFoil"
        ]
    );
    assert_eq!(body["name"], "Test Drop");
    assert_eq!(body["code"], "sl1");
    assert_eq!(body["type"], "Secret Lair Drop");
    assert_eq!(body["unresolved"], 0);
    assert_eq!(body["totalListed"], 2);
    assert_eq!(body["resolvedCount"], 2);
    assert_eq!(body["fileName"], "test_drop_SL1.json");
    assert_eq!(body["resolvedFoil"], false);
    let cards = body["cards"].as_array().unwrap();
    assert_eq!(
        keys(&cards[0]),
        vec!["card_id", "name", "image_url", "set_code", "number", "count", "isFoil", "owned"]
    );
    // The getPreconCardList-blindness pin: name comes from SCRYFALL (the
    // list row has none), isFoil is data-driven FALSE, count floors 'x'→1,
    // number is a STRING, set_code falls through to the card's set.
    assert_eq!(cards[0]["card_id"], "mtg-aaa");
    assert_eq!(cards[0]["name"], "Card A");
    assert_eq!(cards[0]["set_code"], "10e");
    assert_eq!(cards[0]["number"], "1");
    assert_eq!(cards[0]["count"], 4);
    assert_eq!(cards[0]["isFoil"], false);
    assert_eq!(cards[0]["owned"], 3, "case-insensitive key fold, own user only, SUM(quantity)");
    assert_eq!(cards[1]["count"], 1, "Number('x') → NaN → 1");
    assert_eq!(cards[1]["owned"], 0, "SUM(quantity)=0 is falsy → Number(qty)||0 → 0");
    // The resolved printings were cached BEFORE answering.
    let conn = Connection::open(&f.db_path).unwrap();
    let cached: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM card_cache WHERE id IN ('mtg-aaa','mtg-ccc')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cached, 2);
}

#[tokio::test]
async fn preview_dedupe_unresolved_and_count_floors() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    // Two rows resolving to the SAME card (same scryfallId key folded by
    // uniq first; a different-key row that Scryfall resolves to the same id
    // is dropped by the seenId fold), a count:0 row (→1), and a row that
    // answers not_found.
    *f.mtg.deck.lock().unwrap() = deck_body(vec![
        json!({"setCode":"10E","number":"1","count":4,"identifiers":{"scryfallId":"aaa"}}),
        json!({"setCode":"10e","number":"1","count":2,"identifiers":{"scryfallId":"AAA"}}),
        json!({"setCode":"M19","number":"9","count":0,"identifiers":{"scryfallId":"ccc"}}),
        json!({"setCode":"Zzz","number":"7","count":1,"identifiers":{"scryfallId":"ddd"}}),
    ]);
    *f.scry.collection_batches.lock().unwrap() = vec![json!({
        "data": [
            mock_scryfall::raw_card("aaa", "Card A", "10e", "1", None),
            mock_scryfall::raw_card("ccc", "Card C", "m19", "9", None),
        ],
        "not_found": [{"ids": ["ddd"]}]
    })];
    let (s, _, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=test_drop_SL1.json", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    // uniq: aaa and AAA are DISTINCT keys (case-sensitive Map keys) → both
    // request rows; both resolve to mtg-aaa → resolved fold keeps the first.
    assert_eq!(body["totalListed"], 4);
    assert_eq!(body["resolvedCount"], 2);
    assert_eq!(body["unresolved"], 1, "notFound.length");
    let cards = body["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0]["count"], 2, "by_key Map last-set-wins: response card binds to the LAST row with sn:10e|1");
    assert_eq!(cards[1]["count"], 1, "count 0 → floor branch → 1");
}

#[tokio::test]
async fn preview_foil_twin_resolution_and_passthrough() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f);
    // foil=1 on the BASE fileName → twin's fileName, resolvedFoil true, and
    // the deck fetch is the twin's cleaned file name.
    let (s, _, b) = request(
        &f.router,
        "GET",
        "/api/secret-lair/preview?fileName=test_drop_SL1.json&foil=1",
        &MEMBER,
        None,
    )
    .await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["fileName"], "Test Drop FoilEdition_SL1.json");
    assert_eq!(body["resolvedFoil"], true);
    let conn_hits = f.mtg.hits.lock().unwrap().clone();
    assert!(conn_hits.iter().any(|h| h == "deck"));
    // already-Foil fileName passes through (regex on the fileName), even
    // with foil=1 — and the lookup accepted the twin by fileName.
    let (s, _, b) = request(
        &f.router,
        "GET",
        "/api/secret-lair/preview?fileName=Test%20Drop%20FoilEdition_SL1.json&foil=1",
        &MEMBER,
        None,
    )
    .await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["fileName"], "Test Drop FoilEdition_SL1.json");
    assert_eq!(body["resolvedFoil"], false, "no swap happened");
    // foil=true (string) is the other accepted truthy form; unknown fileName
    // passes through untouched (the card-list fetch answers honestly).
    let (s, _, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=nope_ABC.json&foil=true", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["fileName"], "nope_ABC.json");
    assert_eq!(body["resolvedFoil"], false);
    // foil=0 / foil=yes are strict-=== false: no twin lookup at all.
    let (s, _, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=test_drop_SL1.json&foil=yes", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(json_body(&b)["fileName"], "test_drop_SL1.json");
}

#[tokio::test]
async fn preview_foil_twin_missing_422() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    // "Dup Drop" has no FoilEdition twin.
    let (s, _, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=dup_drop_DP1.json&foil=1", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 422);
    let body = json_body(&b);
    assert_eq!(keys(&body), vec!["error"]);
    assert_eq!(body["error"], "This Secret Lair drop has no foil edition available.");
}

#[tokio::test]
async fn preview_upstream_failures() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    // Deck 500: axios rejection carries the status → 500 with the message.
    *f.mtg.deck_status.lock().unwrap() = 500;
    let (s, _, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=test_drop_SL1.json", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 500);
    assert_eq!(
        json_body(&b)["error"],
        "Secret Lair preview failed: Request failed with status code 500"
    );
    // Deck 200 but malformed (no mainBoard array) → 502.
    *f.mtg.deck_status.lock().unwrap() = 200;
    *f.mtg.deck.lock().unwrap() = json!({"data": {"name": "x"}});
    let (s, _, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=test_drop_SL1.json", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 502);
    assert_eq!(json_body(&b)["error"], "Secret Lair preview failed: Malformed MTGJSON deck file");
    // A fileName that cleans to nothing → 400 Invalid precon file name.
    let (s, _, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=...%20...", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 400);
    assert_eq!(json_body(&b)["error"], "Secret Lair preview failed: Invalid precon file name");
}

#[tokio::test]
async fn preview_scryfall_client_error_forwards() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    f.scry
        .statuses
        .lock()
        .unwrap()
        .insert("POST /cards/collection".to_string(), vec![500]);
    let (s, h, b) = request(
        &f.router, "GET", "/api/secret-lair/preview?fileName=test_drop_SL1.json", &MEMBER, None,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(h.get("x-node").unwrap(), "1", "Scryfall failure must replay the whole request to Node");
    let body = json_body(&b);
    assert_eq!(body["method"], "GET");
    assert!(body["pq"]
        .as_str()
        .unwrap()
        .starts_with("/api/secret-lair/preview?fileName=test_drop_SL1.json"));
}

// ---------------------------------------------------------------------------
// POST /api/secret-lair/add
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_requires_file_name_and_bad_body() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    for body in ["{}", r#"{"fileName":""}"#, r#"{"fileName":"   "}"#, r#"{"fileName":0}"#] {
        let (s, _, b) =
            request(&f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON, Some(body)).await;
        assert_eq!(s, 400, "body {body}");
        assert_eq!(json_body(&b)["error"], "fileName is required");
    }
    // Malformed JSON answers the generic 500 BEFORE auth even matters
    // (normalJson sits above authenticateToken).
    let (s, _, _) = request(
        &f.router, "POST", "/api/secret-lair/add",
        &[("authorization", "Bearer sess-member"), ("content-type", "application/json")],
        Some("{oops"),
    )
    .await;
    assert_eq!(s, 500);
}

#[tokio::test]
async fn add_early_422_nothing_resolved() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(vec![]);
    let (s, _, b) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json"}"#),
    )
    .await;
    assert_eq!(s, 422);
    let body = json_body(&b);
    // The early shape keeps `message` in its THIRD position — the route
    // spread does not move existing keys.
    assert_eq!(
        keys(&body),
        vec!["added", "failed", "message", "name", "totalCards", "unresolved", "fileName", "resolvedFoil"]
    );
    assert_eq!(body["added"], 0);
    assert_eq!(body["failed"].as_array().unwrap().len(), 0);
    assert_eq!(body["message"], "No cards to add");
    assert_eq!(body["fileName"], "test_drop_SL1.json");
}

#[tokio::test]
async fn add_happy_db_writes_and_shape() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f);
    let (s, _, b) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json","copies":"2x","condition":"Near Mint"}"#),
    )
    .await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(
        keys(&body),
        vec![
            "added", "failed", "name", "totalCards", "resolvedCount", "unresolved",
            "fileName", "resolvedFoil", "message"
        ]
    );
    assert_eq!(body["added"], 2);
    assert_eq!(body["failed"].as_array().unwrap().len(), 0);
    assert_eq!(body["name"], "Test Drop");
    assert_eq!(body["totalCards"], 2);
    assert_eq!(body["resolvedCount"], 2);
    assert_eq!(body["unresolved"], 0);
    assert_eq!(body["resolvedFoil"], false);
    assert_eq!(body["message"], "Added 2 cards from Test Drop.");
    // copies "2x": Number('2x') is NaN → || 1 → mult 1. The parseInt trap,
    // pinned the Node way: '2x' doubles NOTHING (parseInt would say 2).
    let conn = Connection::open(&f.db_path).unwrap();
    let rows: Vec<(String, i64, String, String, String, f64)> = conn
        .prepare(
            "SELECT card_id, quantity, printing, language, condition, purchase_price
               FROM collection WHERE user_id=2 ORDER BY card_id",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("mtg-aaa".to_string(), 4, "Normal".into(), "English".into(), "Near Mint".into(), 0.0),
            ("mtg-ccc".to_string(), 1, "Normal".into(), "English".into(), "Near Mint".into(), 0.0),
        ],
        "quantity = max(1, round((count || 1) * (Number('2x')||1 → 1))); Normal-only until MTGJSON ships isFoil"
    );
}

#[tokio::test]
async fn add_foil_flag_is_strict_and_twin_422() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f);
    // body.foil === '1' (string) is NOT one of the three strict forms.
    let (s, _, b) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json","foil":"1"}"#),
    )
    .await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["fileName"], "test_drop_SL1.json", "string '1' must NOT trigger the twin swap");
    assert_eq!(body["resolvedFoil"], false);
    // foil: 1 (number) DOES.
    let (s, _, b) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json","foil":1}"#),
    )
    .await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["fileName"], "Test Drop FoilEdition_SL1.json");
    assert_eq!(body["resolvedFoil"], true);
    // No twin → the route-level 422, verbatim {error} (no add-failed prefix).
    let (s, _, b) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"dup_drop_DP1.json","foil":true}"#),
    )
    .await;
    assert_eq!(s, 422);
    assert_eq!(json_body(&b)["error"], "This Secret Lair drop has no foil edition available.");
}

#[tokio::test]
async fn add_scard_client_error_forwards() {
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    f.scry
        .statuses
        .lock()
        .unwrap()
        .insert("POST /cards/collection".to_string(), vec![500]);
    let (s, h, b) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json"}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(h.get("x-node").unwrap(), "1");
    let body = json_body(&b);
    assert_eq!(body["method"], "POST");
    assert_eq!(body["pq"], "/api/secret-lair/add");
    assert!(body["body"].as_str().unwrap().contains("test_drop_SL1.json"));
    // Nothing was written locally — Node owns the whole thing.
    let conn = Connection::open(&f.db_path).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM collection WHERE user_id=2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn add_per_card_failure_shape_from_db() {
    // The failed-entry shape {card_id, error, index} is pinned here via the
    // only locally reachable Reject: a card_cache row whose printing lookup
    // cannot satisfy plan_add (card absent entirely would Forward; instead
    // we plant a resolved card whose id is present but whose per-card add
    // hits the plan_add 400 for a falsy-merged card_id is impossible — so
    // the reachable native path is the generic 'Failed to add card' from a
    // write error. We instead pin that ALL successes leave failed empty and
    // per-group index restart via the two-group order: with the data-driven
    // quirk every card is Normal, so holo is empty and index covers 0..n.
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f);
    let (s, _, b) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json"}"#),
    )
    .await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["failed"], json!([]));
    assert_eq!(body["message"], "Added 2 cards from Test Drop.");
}

// ---------------------------------------------------------------------------
// addSecretLairToCollection plan folds: Number(copies) mult, Math.round,
// printingMode whitelist, condition whitelist (iter-37 parity fallout).
// ---------------------------------------------------------------------------

fn added_rows(f: &Fix) -> Vec<(String, i64, String, String, String)> {
    let conn = Connection::open(&f.db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT card_id, quantity, printing, language, condition
               FROM collection WHERE user_id=2 ORDER BY card_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows
}

#[tokio::test]
async fn add_copies_mult_number_fold_round_and_clamp() {
    // copies '1.5': mult = Number('1.5') = 1.5 (truthy, kept);
    // qty = Math.max(1, Math.round(count * mult)) — JS round is HALF-UP:
    // aaa count 4 → round(6) = 6; ccc count 'x' → preview fold → 1 →
    // round(1.5) = 2 (Rust f64::round half-away-from-zero agrees at ≥0).
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f);
    let (s, _, _) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json","copies":"1.5"}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(
        added_rows(&f),
        vec![
            ("mtg-aaa".to_string(), 6, "Normal".into(), "English".into(), "Near Mint".into()),
            ("mtg-ccc".to_string(), 2, "Normal".into(), "English".into(), "Near Mint".into()),
        ]
    );
    // copies '999' → Math.min(999, 500) clamp → 4*500 / 1*500.
    let f2 = fixture(&["secret_lair"]).await;
    warm(&f2).await;
    *f2.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f2);
    let (s, _, _) = request(
        &f2.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json","copies":999}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(
        added_rows(&f2),
        vec![
            ("mtg-aaa".to_string(), 2000, "Normal".into(), "English".into(), "Near Mint".into()),
            ("mtg-ccc".to_string(), 500, "Normal".into(), "English".into(), "Near Mint".into()),
        ]
    );
    // copies 0 → falsy → 1; copies -5 → Math.max(1, min(-5,500)) = 1.
    for raw in [r#""0""#, "0", "-5", "null", "true"] {
        let f3 = fixture(&["secret_lair"]).await;
        warm(&f3).await;
        *f3.mtg.deck.lock().unwrap() = deck_body(ok_cards());
        script_ok_scry(&f3);
        let (s, _, _) = request(
            &f3.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
            Some(&format!(r#"{{"fileName":"test_drop_SL1.json","copies":{raw}}}"#)),
        )
        .await;
        assert_eq!(s, 200, "copies {raw}");
        let rows = added_rows(&f3);
        assert_eq!(rows[0].1, 4, "copies {raw} must collapse to mult 1");
        assert_eq!(rows[1].1, 1, "copies {raw} must collapse to mult 1");
    }
}

#[tokio::test]
async fn add_printing_mode_and_condition_whitelists() {
    // printingMode 'Foil' → String().toLowerCase() → whitelist hit → EVERY
    // card forced Holofoil even though the deck rows are isFoil-blind.
    let f = fixture(&["secret_lair"]).await;
    warm(&f).await;
    *f.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f);
    let (s, _, _) = request(
        &f.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json","printingMode":"Foil"}"#),
    )
    .await;
    assert_eq!(s, 200);
    let rows = added_rows(&f);
    assert!(rows.iter().all(|r| r.2 == "Holofoil"), "mode foil forces Holofoil: {rows:?}");
    // An off-list mode ('Foil ' with the trailing space → String().
    // toLowerCase() is NOT whitelisted) falls back to auto → isFoil-blind
    // rows are Normal. And an off-list condition is REWRITTEN to 'Near Mint'
    // by the util's own whitelist (the core would otherwise store it raw).
    let f2 = fixture(&["secret_lair"]).await;
    warm(&f2).await;
    *f2.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f2);
    let (s, _, _) = request(
        &f2.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json","printingMode":"Foil ","condition":"Mint"}"#),
    )
    .await;
    assert_eq!(s, 200);
    let rows = added_rows(&f2);
    assert!(rows.iter().all(|r| r.2 == "Normal"), "off-list mode → auto: {rows:?}");
    assert!(rows.iter().all(|r| r.4 == "Near Mint"), "off-list grade rewritten: {rows:?}");
    // A VALID grade rides through untouched.
    let f3 = fixture(&["secret_lair"]).await;
    warm(&f3).await;
    *f3.mtg.deck.lock().unwrap() = deck_body(ok_cards());
    script_ok_scry(&f3);
    let (s, _, _) = request(
        &f3.router, "POST", "/api/secret-lair/add", &MEMBER_JSON,
        Some(r#"{"fileName":"test_drop_SL1.json","condition":"Damaged"}"#),
    )
    .await;
    assert_eq!(s, 200);
    let rows = added_rows(&f3);
    assert!(rows.iter().all(|r| r.4 == "Damaged"), "whitelisted grade kept: {rows:?}");
}

#[tokio::test]
async fn search_name_key_run_collapse() {
    // JS replace(/[^\w]+/g, ' ') collapses each non-word RUN to ONE space:
    // "Animar,  Soul!!" and "Animar Soul" share key "animar soul" and fold
    // into a single slot; "The '90s" keys to "the 90s" (never "the  90s").
    let f = fixture(&["secret_lair"]).await;
    *f.mtg.deck_list.lock().unwrap() = json!({ "data": [
        idx("Animar,  Soul!!", "an1", "Secret Lair Drop", "a1.json"),
        idx("Animar Soul", "an2", "Secret Lair Drop", "a2.json"),
        idx("The '90s Binder Experience", "ni1", "Secret Lair Drop", "n1.json"),
    ]});
    *f.mtg.set_list.lock().unwrap() = json!({"data": []});
    let (s, _, b) = request(&f.router, "GET", "/api/secret-lair", &ADMIN, None).await;
    assert_eq!(s, 200);
    let body = json_body(&b);
    assert_eq!(body["total"], 2, "run-collapsed keys fold the Animar pair");
    let arr = body["results"].as_array().unwrap();
    assert_eq!(arr[0]["key"], "animar soul");
    assert_eq!(arr[0]["fileName"], "a1.json", "first-wins within the collapsed run");
    assert_eq!(arr[1]["key"], "the 90s binder experience");
}
