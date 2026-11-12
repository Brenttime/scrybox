//! LANE A — iter-39 route tests for the cards-meta group (cards_meta.rs).
//!
//! Pinned: the decks CROSS-JOIN logical match (all printings, basic-land
//! target exclusion, checked_out gate, Number()||0 quantity), price-history
//! collapse/500/windowDays quirks, highlight validation + analyze-gate
//! replay + engine error ladder (404 → {names:[]}, 400 → axios message,
//! 429-retry), suggest dedup/cap/empty paths, cotd determinism (day-hash
//! pick, show flag, payload shape, per-day cache) — plus the whole-request
//! replays (fake Node answers with x-node: 1).

mod mock_scryfall;

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use rusqlite::Connection;
use scrybox_server::{
    app,
    scryfall::{Scry, ScryEnv},
    AppState, Config,
};
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
         INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, price_trend)
           VALUES ('mtg-bolt','Lightning Bolt',NULL,'[]','Instant','Common','10e','Tenth Edition','1',1.0),
                  ('mtg-bolt2','Lightning Bolt',NULL,'[]','Instant','Uncommon','2me','Two Edition','1x',2.0),
                  ('mtg-plain','Plains','Land','[\"Basic\",\"Plains\"]','Land','Common','10e','Tenth Edition','2',0.1),
                  ('mtg-bad','Bad Dates',NULL,'[]','Creature','Common','10e','Tenth Edition','3',0.5),
                  ('mtg-span','Span Card',NULL,'[]','Creature','Common','10e','Tenth Edition','4',0.5);
         INSERT INTO decks (id, user_id, name, checked_out) VALUES
           (11, 2, 'Alpha Deck', 1),
           (12, 2, 'Beta Deck', 0);
         INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES
           (11,'mtg-bolt',2),
           (11,'mtg-bolt2',1),
           (11,'mtg-plain',1),
           (12,'mtg-bolt',9);
         INSERT INTO price_history (card_id, price, recorded_at) VALUES
           ('mtg-bolt',1.0,'2020-01-01 00:00:00'),
           ('mtg-bolt',2.5,'2026-01-01 00:00:00'),
           ('mtg-bolt',2.5,'2026-01-02 00:00:00'),
           ('mtg-bolt',2.5,'2026-01-03 00:00:00'),
           ('mtg-bolt',3.0,'2026-01-04 00:00:00'),
           ('mtg-bad',2.5,'bad-date'),
           ('mtg-span',2.5,'2026-01-01 00:00:00'),
           ('mtg-span',2.5,'2026-01-01X bad-time'),
           ('mtg-span',2.5,'2026-01-02 00:00:00'),
           ('mtg-span',4.0,'2026-01-03 00:00:00');",
    )
    .unwrap();
    path
}

struct Fix {
    router: Router,
    scry: Arc<mock_scryfall::MockState>,
    _tmp: tempfile::TempDir,
}

async fn fixture(routes: &[&str]) -> Fix {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), "cards_meta.db");
    let node = spawn_fake_node().await;
    let (scry_addr, scry) = mock_scryfall::spawn_mock().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    // Native cards_meta answering needs LOCAL mode pointing at the mock.
    st.scry = Arc::new(Scry::new(ScryEnv {
        local: true,
        local_url: format!("http://{scry_addr}"),
        base_url: format!("http://{scry_addr}"),
        gap_scale: 0.0,
    }));
    Fix { router: app(st), scry, _tmp: tmp }
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

// ---------------------------------------------------------------- decks

#[tokio::test]
async fn decks_logical_match_all_printings_and_gates() {
    let f = fixture(&["cards_meta"]).await;
    // Target 'Lightning Bolt': Alpha Deck holds two logical-printing rows
    // (2 + 1 = 3); Beta Deck is not checked out → excluded.
    let (s, _, b) = request(&f.router, "GET", "/api/cards/mtg-bolt/decks", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        json_body(&b),
        json!({"decks": [{"id": 11, "name": "Alpha Deck", "quantity": 3}]})
    );
    // Basic-land targets are never listed.
    let (_, _, b) = request(&f.router, "GET", "/api/cards/mtg-plain/decks", &MEMBER, None).await;
    assert_eq!(json_body(&b), json!({"decks": []}));
    // Unknown id → CROSS JOIN target yields nothing.
    let (_, _, b) = request(&f.router, "GET", "/api/cards/nope/decks", &MEMBER, None).await;
    assert_eq!(json_body(&b), json!({"decks": []}));
}

#[tokio::test]
async fn decks_requires_auth_and_off_switch() {
    let f = fixture(&["cards_meta"]).await;
    let (s, _, b) = request(&f.router, "GET", "/api/cards/mtg-bolt/decks", &[], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(json_body(&b).get("error").is_some());

    let off = fixture(&[]).await;
    let (s, h, _) = request(&off.router, "GET", "/api/cards/mtg-bolt/decks", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    let (_, h, _) = request(&off.router, "GET", "/api/suggest?q=light", &MEMBER, None).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
    let (_, h, _) = request(&off.router, "GET", "/api/stats/commander-of-the-day", &MEMBER, None).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
    let (_, h, _) = request(&off.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{\"q\":\"a\",\"names\":[\"a\"]}")).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
}

// -------------------------------------------------------- price history

#[tokio::test]
async fn price_history_all_collapses_flat_runs() {
    let f = fixture(&["cards_meta"]).await;
    let (s, _, b) = request(&f.router, "GET", "/api/cards/mtg-bolt/price-history?range=x", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    let v = json_body(&b);
    // 2020 old row included under 'all'; the flat 2.5-run keeps only its
    // ends; the 3.0 tail always survives.
    // Flat 2.5-run keeps BOTH ends (prev/next interior only).
    assert_eq!(v["data"].as_array().unwrap().len(), 4);
    assert_eq!(v["data"][0], json!({"price": 1, "recorded_at": "2020-01-01T00:00:00.000Z", "source": "recorded"}));
    assert_eq!(v["data"][1], json!({"price": 2.5, "recorded_at": "2026-01-01T00:00:00.000Z", "source": "recorded"}));
    assert_eq!(v["data"][2], json!({"price": 2.5, "recorded_at": "2026-01-03T00:00:00.000Z", "source": "recorded"}));
    assert_eq!(v["data"][3], json!({"price": 3, "recorded_at": "2026-01-04T00:00:00.000Z", "source": "recorded"}));
    assert_eq!(v["recordedCount"], 5);
    assert_eq!(v["marketCount"], 0);
    assert_eq!(v["insufficientHistory"], false);
    // (2026-01-04 - 2020-01-01)/86400000 = 2194 days.
    assert_eq!(v["spanDays"], 2195); // 2020-01-01 → 2026-01-04
    assert_eq!(v["windowDays"], Value::Null);
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["data", "marketCount", "recordedCount", "insufficientHistory", "spanDays", "windowDays"]);
}

#[tokio::test]
async fn price_history_default_window_is_30d() {
    let f = fixture(&["cards_meta"]).await;
    let (s, _, b) = request(&f.router, "GET", "/api/cards/mtg-bolt/price-history", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    let v = json_body(&b);
    assert_eq!(v["data"].as_array().unwrap().len(), 0);
    assert_eq!(v["recordedCount"], 0);
    assert_eq!(v["insufficientHistory"], true);
    assert_eq!(v["spanDays"], 0);
    assert_eq!(v["windowDays"], 30);
}

#[tokio::test]
async fn price_history_nan_time_paths() {
    let f = fixture(&["cards_meta"]).await;
    // A SURVIVING Invalid-Date point → toISOString throws → whole 500.
    let (s, _, b) = request(&f.router, "GET", "/api/cards/mtg-bad/price-history?range=x", &MEMBER, None).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&b), json!({"error": "Failed to retrieve price history"}));

    // A NaN point COLLAPSED AWAY only poisons spanDays (Math.max → NaN).
    let (s, _, b) = request(&f.router, "GET", "/api/cards/mtg-span/price-history?range=x", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    let v = json_body(&b);
    assert_eq!(v["recordedCount"], 4);
    assert_eq!(v["data"].as_array().unwrap().len(), 3); // the collapsed-away NaN middle dropped
    assert_eq!(v["spanDays"], Value::Null);
}

#[tokio::test]
async fn price_history_window_days_prototype_curiosities() {
    let f = fixture(&["cards_meta"]).await;
    // 'constructor' → truthy function: windowed query with a garbage
    // modifier (zero rows) and JSON.stringify DROPS windowDays.
    let (s, _, b) = request(&f.router, "GET", "/api/cards/mtg-bolt/price-history?range=constructor", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    let v = json_body(&b);
    assert_eq!(v["data"].as_array().unwrap().len(), 0);
    assert!(v.get("windowDays").is_none());
    // '__proto__' → Object.prototype: {} in JSON.
    let (_, _, b) = request(&f.router, "GET", "/api/cards/mtg-bolt/price-history?range=__proto__", &MEMBER, None).await;
    let v = json_body(&b);
    assert_eq!(v["windowDays"], json!({}));
    // Array values join on ',': ['30d','x'] → '30d,x' → undefined → all.
    let (_, _, b) = request(&f.router, "GET", "/api/cards/mtg-bolt/price-history?range[]=30d&range[]=x", &MEMBER, None).await;
    assert_eq!(json_body(&b)["windowDays"], Value::Null);
}

// ------------------------------------------------------------- highlight

#[tokio::test]
async fn highlight_validation_and_gate() {
    let f = fixture(&["cards_meta"]).await;
    // Missing q/names → 400.
    let (s, _, b) = request(&f.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"error": "q (1-500 chars) and names (1-1000) are required"}));
    // names present but q missing (string check) → same 400.
    let (s, _, _) = request(&f.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{\"names\":[\"a\"]}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Operator syntax → the analyze gate replays the WHOLE POST to Node.
    let (s, h, b) = request(&f.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{\"q\":\"is:land\",\"names\":[\"a\"]}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get("x-node").unwrap(), "1");
    let v = json_body(&b);
    assert_eq!(v["method"], "POST");
    assert_eq!(v["pq"], "/api/cards/highlight");
    assert_eq!(v["body"], "{\"q\":\"is:land\",\"names\":[\"a\"]}");

    // Malformed JSON → body-parser's verdict is Node's → raw replay.
    let (_, h, b) = request(&f.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{oops")).await;
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert_eq!(json_body(&b)["body"], "{oops");

    // Non-JSON content type → express.json leaves body undefined → the
    // 400 (Node's own answer) WITHOUT touching the engine.
    let (s, _, _) = request(&f.router, "POST", "/api/cards/highlight", &[("authorization", "Bearer sess-member"), ("content-type", "text/plain")], Some("q=a")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(f.scry.hits_for("GET /cards/search").is_empty());
}

#[tokio::test]
async fn highlight_native_match_and_engine_error_ladder() {
    let f = fixture(&["cards_meta"]).await;
    *f.scry.search.lock().unwrap() =
        json!({"data": [{"name": "Goblin Racketeer"}, {"name": "Goblin Racketeer // Other"}], "has_more": false});
    let (s, _, b) = request(
        &f.router,
        "POST",
        "/api/cards/highlight",
        &MEMBER_JSON,
        Some("{\"q\":\"goblin\",\"names\":[\"Goblin Racketeer\",\"Island\",\"goblin racketeer\"]}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // Set-dedup is on the RAW string, so both case variants survive
    // (faithful to Node); the DFC front face matches the lowercase variant;
    // 'Island' (not engine-returned) drops out.
    assert_eq!(json_body(&b), json!({"names": ["Goblin Racketeer", "goblin racketeer"]}));
    let hits = f.scry.hits_for("GET /cards/search");
    assert_eq!(hits.len(), 1);
    // (goblin) (!\"Goblin Racketeer\") — encodeURIComponent verbatim, plus
    // fetchWindow's order=name.
    assert!(hits[0].0.contains("q=(goblin)%20(!%22Goblin%20Racketeer%22"), "{}", hits[0].0);
    assert!(hits[0].0.contains("%20or%20!%22Island%22"), "{}", hits[0].0);
    assert!(hits[0].0.contains("&page=1"));
    assert!(hits[0].0.contains("&order=name"));

    // 404 on any page → {names: []}.
    f.scry.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![404]);
    let (s, _, b) = request(&f.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{\"q\":\"goblin\",\"names\":[\"Island\"]}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({"names": []}));

    // 400 → the axios message.
    f.scry.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![400]);
    let (s, _, b) = request(&f.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{\"q\":\"goblin\",\"names\":[\"Island\"]}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"error": "Request failed with status code 400"}));

    // 5xx → 502 'Search engine unavailable'.
    f.scry.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![503]);
    let (s, _, b) = request(&f.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{\"q\":\"goblin\",\"names\":[\"Island\"]}")).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({"error": "Search engine unavailable"}));

    // 429 every time: scryGetRetried's 4 attempts all burn, then the
    // exhausted 429 lands in the catch's else → 502 twin.
    let before = f.scry.hits_for("GET /cards/search").len();
    f.scry.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![429]);
    let (s, _, b) = request(&f.router, "POST", "/api/cards/highlight", &MEMBER_JSON, Some("{\"q\":\"goblin\",\"names\":[\"Goblin Racketeer\"]}")).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({"error": "Search engine unavailable"}));
    // Four attempts per page (scryGetRetried retries = 4).
    assert_eq!(f.scry.hits_for("GET /cards/search").len() - before, 4);
}

// ---------------------------------------------------------------- suggest

#[tokio::test]
async fn suggest_dedup_cap_and_defaults() {
    let f = fixture(&["cards_meta"]).await;
    let mk = |i: usize, name: String| json!({"name": name, "set": format!("s{i}"), "image_small": null, "id": i});
    let many: Vec<Value> = (0..10)
        .map(|i| mk(i, if i == 5 { "card 1".to_string() } else { format!("Card {i}") }))
        .collect();
    *f.scry.suggest.lock().unwrap() = json!({"suggestions": many});
    let (s, _, b) = request(&f.router, "GET", "/api/suggest?q=card", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    let v = json_body(&b);
    let arr = v["suggestions"].as_array().unwrap();
    assert_eq!(arr.len(), 8); // slice(0,8)
    assert_eq!(arr[0]["name"], "Card 0");
    assert_eq!(arr[0]["set"], "s0");
    assert_eq!(arr[0]["image_small"], ""); // null || ''
    assert_eq!(arr[0]["id"], ""); // falsy 0 → ''
    // Case-insensitive dedup: i=5 "card 1" loses to i=1 "Card 1".
    assert_eq!(arr.iter().filter(|e| e["name"] == "Card 1").count(), 1);
    assert!(!arr.iter().any(|e| e["name"] == "card 1"));
}

#[tokio::test]
async fn suggest_empty_paths() {
    let f = fixture(&["cards_meta"]).await;
    // q too short → no sidecar hit at all.
    let (s, _, b) = request(&f.router, "GET", "/api/suggest?q=a", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({"suggestions": []}));
    assert!(f.scry.hits_for("GET /api/suggest").is_empty());

    // Non-2xx → {suggestions: []}.
    *f.scry.suggest.lock().unwrap() = json!({"suggestions": [{"name": "Z"}]});
    f.scry.statuses.lock().unwrap().insert("GET /api/suggest".into(), vec![500]);
    let (s, _, b) = request(&f.router, "GET", "/api/suggest?q=zz", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({"suggestions": []}));

    // Truthy non-string name → .toLowerCase() throws in Node → whole catch.
    f.scry.statuses.lock().unwrap().remove("GET /api/suggest");
    *f.scry.suggest.lock().unwrap() = json!({"suggestions": [{"name": 5}]});
    let (s, _, b) = request(&f.router, "GET", "/api/suggest?q=zz", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_body(&b), json!({"suggestions": []}));
}

// ---------------------------------------------------- commander-of-the-day

fn cotd_card() -> Value {
    json!({
        "id": "c1",
        "name": "Ètali, Idyllic Tutor",
        "type_line": "Legendary Artifact Creature — SCarecrow",
        "color_identity": ["W"],
        "set": "one",
        "image_uris": {"normal": "http://n", "large": "http://l", "art_crop": "http://a"}
    })
}

#[tokio::test]
async fn cotd_day_param_payload_and_cache() {
    let f = fixture(&["cards_meta"]).await;
    *f.scry.search.lock().unwrap() = json!({"data": [cotd_card()], "has_more": false, "total_cards": 1});
    let (s, h, b) = request(&f.router, "GET", "/api/stats/commander-of-the-day?day=2026-11-12", &MEMBER, None).await;
    assert_eq!(s, StatusCode::OK);
    // Express default ETag stamped on res.json.
    assert!(h.get(header::ETAG).is_some());
    let v = json_body(&b);
    // Generated with node from stats.js: dayHash('show:2026-11-12') % 2 === 1.
    assert_eq!(v["day"], "2026-11-12");
    assert_eq!(v["show"], false);
    assert_eq!(v["card"]["id"], "c1");
    assert_eq!(v["card"]["name"], "Ètali, Idyllic Tutor");
    assert_eq!(v["card"]["type_line"], "Legendary Artifact Creature — SCarecrow");
    assert_eq!(v["card"]["color_identity"], json!(["W"]));
    assert_eq!(v["card"]["set"], "one");
    assert_eq!(v["card"]["image_url"], "http://n");
    assert_eq!(v["card"]["art_crop"], "http://a");
    assert_eq!(v["card"]["edhrec_url"], "https://edhrec.com/commanders/etali-idyllic-tutor");
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["day", "show", "card"]);
    assert_eq!(v["card"].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["id", "name", "type_line", "color_identity", "set", "image_url", "art_crop", "edhrec_url"]);

    // Second call for the SAME day is served from the day cache: no new
    // sidecar hit (Node's cotdCache twin).
    let before = f.scry.hits_for("GET /cards/search").len();
    let (_, _, b2) = request(&f.router, "GET", "/api/stats/commander-of-the-day?day=2026-11-12", &MEMBER, None).await;
    assert_eq!(json_body(&b2), v);
    assert_eq!(f.scry.hits_for("GET /cards/search").len(), before);
}

#[tokio::test]
async fn cotd_error_shapes() {
    let f = fixture(&["cards_meta"]).await;
    // total falsy → 503 No commanders available.
    *f.scry.search.lock().unwrap() = json!({"data": [cotd_card()], "has_more": false, "total_cards": 0});
    let (s, _, b) = request(&f.router, "GET", "/api/stats/commander-of-the-day?day=2026-11-13", &MEMBER, None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(&b), json!({"error": "No commanders available"}));

    // total 3000 with an empty answer → the idx window comes back empty →
    // 503 No commander found.
    *f.scry.search.lock().unwrap() = json!({"data": [], "has_more": false, "total_cards": 3000});
    let (s, _, b) = request(&f.router, "GET", "/api/stats/commander-of-the-day?day=2026-11-14", &MEMBER, None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(&b), json!({"error": "No commander found"}));

    // Engine down (404 included — scryPage throws here) → 502 twin.
    f.scry.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![404]);
    let (s, _, b) = request(&f.router, "GET", "/api/stats/commander-of-the-day?day=2026-11-15", &MEMBER, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({"error": "Commander of the day unavailable"}));
}

