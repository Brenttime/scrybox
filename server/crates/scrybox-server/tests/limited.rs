//! LANE-A iter-48: POST /api/limited/analyze (routes/limited.js +
//! utils/limitedMana.js). Pure twins for parseDecklist/costPips/analyzeCards,
//! then route-level tests pinning the probe-verified middleware order
//! (normalJson 500/413 BEFORE auth), the auth twins, the scryfall chunking
//! (BATCH=75, 429 retry ladder, non-2xx → 502), the DFC front-name keying
//! and the res.json shape (key order + ETag). GET rides the proxy verbatim.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use scrybox_server::{auth_support::uuid_v4, AppState, Config};

#[path = "./auth_common.rs"]
mod auth_common;
use auth_common::{bearer, json_ct, merge, request};

#[path = "mock_scryfall.rs"]
mod mock_scryfall;
use mock_scryfall::{spawn_mock, MockState};

const TOK: &str = concat!("tok-", "lim-member-1");
const KEY: &str =
    concat!("bnd_", "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

// ---------------------------------------------------------------------------
// Harness
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
                "pq": parts.uri.path_and_query().unwrap().as_str(),
                "method": parts.method.as_str(),
                "body": String::from_utf8_lossy(&body),
            })),
        )
            .into_response()
    });
    let health =
        axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({"status":"ok"}))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

struct Fix {
    router: Router,
    mock: Arc<MockState>,
}

async fn build() -> Fix {
    let node = spawn_fake_node().await;
    let (mock_addr, mock) = spawn_mock().await;
    let dir = std::env::temp_dir().join(format!("scrybox-lim-it-{}", uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bindarr.db"), []).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: ["limited".to_string()].into_iter().collect(),
        db_path: Some(dir.join("bindarr.db")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    // Face the scripted mock; gap scale ~0 keeps the limiter inert in-suite.
    st.scry = Arc::new(mock_scryfall::scry_for(mock_addr, 0.001));
    let pool = st.db.clone().unwrap();
    pool.get().unwrap().execute_batch(auth_common::schema_fixture()).unwrap();
    pool.get()
        .unwrap()
        .execute_batch(
            "INSERT INTO users (username, password_hash, role, share_token, api_key)
             VALUES ('limmember','h','member','sh-lim','bnd_eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee');
             INSERT INTO sessions (token, user_id, expires_at)
             VALUES ('tok-lim-member-1',1,'2030-01-01 00:00:00');",
        )
        .unwrap();
    Fix { router: scrybox_server::app(st), mock }
}

fn auth() -> Vec<(&'static str, String)> {
    bearer(TOK)
}

async fn post_json(fix: &Fix, body: &str) -> (StatusCode, HeaderMap, String) {
    request(&fix.router, peer_n(11), "POST", "/api/limited/analyze", &merge(&auth(), &json_ct()), Some(body.to_string())).await
}

fn peer_n(n: u64) -> SocketAddr {
    SocketAddr::from((std::net::Ipv4Addr::new(10, 198, (n / 250) as u8, (n % 250) as u8 + 1), 45000))
}

fn land(name: &str, type_line: &str, oracle: &str, produced: &[&str]) -> Value {
    json!({
        "name": name,
        "type_line": type_line,
        "oracle_text": oracle,
        "mana_cost": "",
        "produced_mana": produced,
    })
}

// ---------------------------------------------------------------------------
// parseDecklist
// ---------------------------------------------------------------------------

#[test]
fn parse_basic_quantity_forms_and_dedupe() {
    use scrybox_server::limited::parse_decklist;
    let e = parse_decklist("2 Doom Blade\n2x Scoured Barrens\nIsland\n3 x Forest");
    assert_eq!(e.len(), 4);
    assert_eq!((e[0].name.as_str(), e[0].quantity), ("Doom Blade", 2));
    assert_eq!((e[1].name.as_str(), e[1].quantity), ("Scoured Barrens", 2));
    assert_eq!((e[2].name.as_str(), e[2].quantity), ("Island", 1));
    assert_eq!((e[3].name.as_str(), e[3].quantity), ("Forest", 3));
    // Dedupe on lowercase; first-seen name + summed quantity wins.
    let e = parse_decklist("1 Sol Ring\n2 sol ring\n4 SOL RING");
    assert_eq!(e.len(), 1);
    assert_eq!((e[0].name.as_str(), e[0].quantity), ("Sol Ring", 7));
    // >3 digits never matches the qty pattern → whole line is a name, qty 1.
    let e = parse_decklist("1234 Foo Bar");
    assert_eq!((e[0].name.as_str(), e[0].quantity), ("1234 Foo Bar", 1));
    // quantity 0 skipped BEFORE seen_card is set (the blank line after it
    // does not end the deck).
    let e = parse_decklist("0 Skip Me\n\n1 Sol Ring");
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].name, "Sol Ring");
}

#[test]
fn parse_set_code_and_foil_strip() {
    use scrybox_server::limited::parse_decklist;
    let e = parse_decklist("2 Doom Blade (M10) 88\n1 Sol Ring (LEA) 1a *F*\n1 Forest (10E)");
    assert_eq!(e[0].name, "Doom Blade");
    // Set-code strip runs BEFORE foil strip: the foil tail makes the anchored
    // set regex miss, so (M10)-style code survives once the foil is removed —
    // faithful to limitedMana.js replace order (this is what Node does).
    assert_eq!(e[1].name, "Sol Ring (LEA) 1a");
    assert_eq!(e[2].name, "Forest");
}

#[test]
fn parse_comments_headers_and_sections() {
    use scrybox_server::limited::parse_decklist;
    let e = parse_decklist("// comment\n# also comment\ndeck\nMainboard:\n1 Island\n\nSideboard\n1 Sol Ring");
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].name, "Island");
    // Section keyword must stand as a word: "Commander's Palace" (leading
    // digit, so never anchored) is a card, but a bare "sideboard …" line —
    // keyword followed by a non-word char — breaks the deck there.
    let e = parse_decklist("1 Commander's Palace\nsideboard is a card name here is not tested\n");
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].name, "Commander's Palace");
    // Leading blanks never end the deck; first blank AFTER cards does.
    let e = parse_decklist("\n\n1 Island\n\n2 Sol Ring");
    assert_eq!(e.len(), 1);
    // "tokens" (singular or plural) breaks.
    let e = parse_decklist("1 Island\nToken: Copy");
    assert_eq!(e.len(), 1);
}

#[test]
fn parse_crlf_and_untouched_lone_cr() {
    use scrybox_server::limited::parse_decklist;
    let e = parse_decklist("1 Island\r\n2 Swamp\r\n");
    assert_eq!(e.len(), 2);
    // A lone \r mid-line is a JS dot terminator: the qty regex fails and the
    // whole line becomes the name (qty 1).
    let e = parse_decklist("2 Doom\rBlade");
    assert_eq!(e[0].name, "2 Doom\rBlade");
    assert_eq!(e[0].quantity, 1);
}

// ---------------------------------------------------------------------------
// costPips
// ---------------------------------------------------------------------------

#[test]
fn cost_pips_symbols() {
    use scrybox_server::limited::cost_pips;
    let p = cost_pips("{1}{B}");
    assert_eq!((p[0], p[1], p[2], p[3], p[4], p[5]), (0.0, 0.0, 1.0, 0.0, 0.0, 0.0));
    let p = cost_pips("{W/U}{W/U}"); // hybrid: half to each, twice
    assert_eq!((p[0], p[1]), (1.0, 1.0));
    let p = cost_pips("{B/P}"); // Phyrexian: whole to its color
    assert_eq!(p[2], 1.0);
    let p = cost_pips("{2/W}"); // colored pip counts whole
    assert_eq!(p[0], 1.0);
    let p = cost_pips("{C}");
    assert_eq!(p[5], 1.0);
    let p = cost_pips("{X}{S}{G}"); // X/S ignored
    assert_eq!((p[3], p[4], p[5]), (0.0, 1.0, 0.0));
    // Triforce {W/U/B}: a third to each.
    let p = cost_pips("{W/U/B}");
    assert_eq!((p[0], p[1], p[2]), (1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0));
}

// ---------------------------------------------------------------------------
// analyzeCards
// ---------------------------------------------------------------------------

fn card_slice(cards: Vec<(i64, Value)>) -> Value {
    Value::Object(scrybox_server::limited::analyze_cards(&cards))
}

#[test]
fn analyze_basics_and_lands() {
    let out = card_slice(vec![
        (4, json!({"name": "Island", "type_line": "Basic Land — Island"})),
        (1, json!({"name": "Snow-Covered Plains", "type_line": "Basic Land — Plains"})),
        (
            2,
            land(
                "Scoured Barrens",
                "Land",
                "({T}: Add {W} or {B}.) Scoured Barrens enters the battlefield tapped.",
                &["W", "B", "X", "W"],
            ),
        ),
    ]);
    assert_eq!(out["basics"], json!({"W":1,"U":4,"B":0,"R":0,"G":0,"C":0}));
    assert_eq!(out["lands"][0]["name"], json!("Scoured Barrens"));
    assert_eq!(out["lands"][0]["produces"], json!(["W", "B"])); // deduped, WUBRGC-only
    assert_eq!(out["lands"][0]["tapped"], json!(true));
    assert_eq!(out["landCount"], json!(7));
    assert_eq!(out["basicCount"], json!(5));
    assert_eq!(out["spells"], json!(0));
    assert_eq!(out["deckSize"], json!(7));
    // Key order matches Node's { ...analyzeCards(cards), notFound }.
    let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec!["pips", "colorCards", "basics", "lands", "spells", "landCount", "basicCount", "deckSize"]
    );
}

#[test]
fn analyze_spell_pips_and_colorcards() {
    // Two copies of a {W/U}{W/U} spell: 1.0 pip per color per card → 2/2,
    // and colorCards counts CARDS needing the color (2 each).
    let out = card_slice(vec![(2, json!({"name": "Spell", "type_line": "Creature", "mana_cost": "{W/U}{W/U}"}))]);
    assert_eq!(out["pips"], json!({"W":2,"U":2,"B":0,"R":0,"G":0,"C":0}));
    assert_eq!(out["colorCards"], json!({"W":2,"U":2,"B":0,"R":0,"G":0,"C":0}));
    assert_eq!(out["spells"], json!(2));
    // Front face only: card_faces[0] carries the counted cost/type.
    let out = card_slice(vec![(
        1,
        json!({
            "name": "Delver of Secrets // Insectile Aberration",
            "type_line": "Creature — Human Inventor // Creature — Human Insect",
            "mana_cost": "{U}",
            "card_faces": [
                {"type_line": "Creature — Human Inventor", "mana_cost": "{U}", "oracle_text": "Back to the labs!"},
                {"type_line": "Creature — Human Insect", "mana_cost": ""}
            ]
        }),
    )]);
    assert_eq!(out["pips"]["U"], json!(1));
    // MDFC with a LAND front face counts as a land row, not a spell.
    let out = card_slice(vec![(
        1,
        json!({
            "name": "Blasted Landscape",
            "type_line": "Land",
            "mana_cost": "",
            "card_faces": [
                {"type_line": "Land", "oracle_text": "Blasted Landscape enters the battlefield tapped."},
                {"type_line": "Land — Swamp", "mana_cost": ""}
            ]
        }),
    )]);
    assert_eq!(out["spells"], json!(0));
    assert_eq!(out["lands"][0]["tapped"], json!(true));
}

#[test]
fn analyze_land_flags() {
    // "unless" cancels the tapped credit; fetch lands get no fixing credit;
    // "enters tapped" (without "the battlefield") still counts.
    let out = card_slice(vec![
        (1, land("Karakas", "Legendary Land", "Tap: ... unless you control a Legend", &[])),
        (1, land("Flooded Strand", "Land", "Sacrifice Flooded Strand: Search your library for a Plains or Island card", &["W", "U"])),
        (1, land("Ghost Quarter", "Land", "Ghost Quarter enters tapped.", &["W"])),
    ]);
    assert_eq!(out["lands"][0]["tapped"], json!(false)); // unless
    assert_eq!(out["lands"][1]["conditional"], json!(true)); // search your library
    assert_eq!(out["lands"][2]["tapped"], json!(true));
    // \bLand\b is case-sensitive ASCII word: "Legendary Land" matches, a
    // spell with "Land" inside a word does not.
    let out = card_slice(vec![(1, json!({"name": "X", "type_line": "Enchantment — Landwalk", "mana_cost": "{G}"}))]);
    assert_eq!(out["spells"], json!(1));
}

// ---------------------------------------------------------------------------
// Route: middleware order + gates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_json_and_oversize_answer_before_auth() {
    let fix = build().await;
    // Probe-verified: normalJson sits above authenticateToken — no token at
    // all still gets the global handler's 500 / 413.
    let (s, _, b) = request(
        &fix.router,
        peer_n(21),
        "POST",
        "/api/limited/analyze",
        &json_ct(),
        Some("notjson".into()),
    )
    .await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(b, "{\"error\":\"Internal server error\"}");
    let huge = format!("{{\"list\":\"{}\"}}", "a".repeat(1024 * 1024));
    let (s, _, b) =
        request(&fix.router, peer_n(22), "POST", "/api/limited/analyze", &json_ct(), Some(huge))
            .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(b, "{\"error\":\"Upload too large. Try exporting/importing in smaller batches.\"}");
}

#[tokio::test]
async fn auth_gates() {
    let fix = build().await;
    let (s, _, b) = request(
        &fix.router,
        peer_n(31),
        "POST",
        "/api/limited/analyze",
        &json_ct(),
        Some("{\"list\":\"1 Island\"}".into()),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(b, "{\"error\":\"Access denied. No token provided.\"}");
    // API keys are GET-only reads: POST + Bearer 60-char key → 403.
    let (s, _, _) = request(
        &fix.router,
        peer_n(32),
        "POST",
        "/api/limited/analyze",
        &merge(&bearer(KEY), &json_ct()),
        Some("{\"list\":\"1 Island\"}".into()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn body_shape_gates() {
    let fix = build().await;
    // No list / non-string list / text/plain / array body all collapse to
    // the empty-string path: 400 No cards found.
    for (label, ct, body) in [
        ("missing", true, "{}"),
        ("number list", true, "{\"list\":5}"),
        ("null list", true, "{\"list\":null}"),
        ("array body", true, "[\"1 Island\"]"),
        ("text/plain", false, "{\"list\":\"1 Island\"}"),
    ] {
        let headers = if ct { merge(&auth(), &json_ct()) } else { merge(&auth(), &[("content-type", "text/plain".into())]) };
        let (s, _, b) =
            request(&fix.router, peer_n(41), "POST", "/api/limited/analyze", &headers, Some(body.into())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{label}");
        assert_eq!(b, "{\"error\":\"No cards found in the list\"}", "{label}");
    }
    // 20001 UTF-16 units (10001 astral chars count 20002): 413, checked on
    // the JS (UTF-16) length, not chars/bytes.
    let long = format!("{{\"list\":\"{}\"}}", "😀".repeat(10001));
    let (s, _, b) = post_json(&fix, &long).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(b, "{\"error\":\"Decklist is too long\"}");
}

#[tokio::test]
async fn distinct_cap_and_batching() {
    let fix = build().await;
    // 121 distinct → 400 before any Scryfall call. (Quantities start at 1:
    // qty "0" lines are skipped by the parser, not counted.)
    let list: String = (1..=121).map(|i| format!("{i} Fake Card {i}\n")).collect();
    let (s, _, b) = post_json(&fix, &format!("{{\"list\":{:?}}}", list)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b, "{\"error\":\"Too many distinct cards (max 120)\"}");
    assert!(fix.mock.hits_for("POST /cards/collection").is_empty());
    // 120 distinct → two 75-sized batches, identifiers = first-seen names.
    let list: String = (0..120).map(|i| format!("1 fake card {i}\nFake Card {i}\n")).collect();
    let (s, _, _) = post_json(&fix, &format!("{{\"list\":{:?}}}", list)).await;
    assert_eq!(s, StatusCode::OK);
    let hits = fix.mock.hits_for("POST /cards/collection");
    assert_eq!(hits.len(), 2);
    let first: Value = serde_json::from_str(&hits[0].1).unwrap();
    assert_eq!(first["identifiers"].as_array().unwrap().len(), 75);
    assert_eq!(first["identifiers"][0], json!({"name": "fake card 0"}));
}

// ---------------------------------------------------------------------------
// Route: the scryfall leg
// ---------------------------------------------------------------------------

#[tokio::test]
async fn analyze_ok_end_to_end() {
    let fix = build().await;
    *fix.mock.collection_batches.lock().unwrap() = vec![json!({
        "object": "list",
        "data": [
            {"name": "Doom Blade", "type_line": "Instant", "mana_cost": "{1}{B}"},
            land("Scoured Barrens", "Land", "Scoured Barrens enters the battlefield tapped.", &["B", "W"]),
            {"name": "Island", "type_line": "Basic Land — Island"},
            {"name": "Fire // Ice", "type_line": "Instant // Instant", "mana_cost": "{1}{R} // {1}{U}"},
        ],
        "not_found": [{"name": "Not A Real Card"}],
    })];
    let (s, h, b) = post_json(
        &fix,
        "{\"list\":\"2 Doom Blade\\n1 Scoured Barrens\\n4 Island\\n1 Not A Real Card\\n1 Fire\"}",
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[axum::http::header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert!(h[axum::http::header::ETAG].to_str().unwrap().starts_with("W/\""));
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["pips"], json!({"W":0,"U":1,"B":2,"R":1,"G":0,"C":0}));
    assert_eq!(v["colorCards"], json!({"W":0,"U":1,"B":2,"R":1,"G":0,"C":0}));
    assert_eq!(v["basics"], json!({"W":0,"U":4,"B":0,"R":0,"G":0,"C":0}));
    assert_eq!(v["lands"][0]["produces"], json!(["B", "W"]));
    assert_eq!(v["spells"], json!(3)); // 2 Doom Blade + Fire (front of Fire // Ice)
    assert_eq!(v["landCount"], json!(5));
    assert_eq!(v["basicCount"], json!(4));
    assert_eq!(v["deckSize"], json!(8));
    assert_eq!(v["notFound"], json!(["Not A Real Card"]));
    // Wire key order (deepDiff pins it in parity too).
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec!["pips", "colorCards", "basics", "lands", "spells", "landCount", "basicCount", "deckSize", "notFound"]
    );
}

#[tokio::test]
async fn scryfall_failure_and_retry_ladder() {
    // Non-2xx (not 429) → route catch → 502 generic.
    let fix = build().await;
    fix.mock
        .statuses
        .lock()
        .unwrap()
        .insert("POST /cards/collection".to_string(), vec![500]);
    let (s, _, b) = post_json(&fix, "{\"list\":\"1 Island\"}").await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(b, "{\"error\":\"Card lookup failed. Try again in a moment.\"}");

    // 429 re-queues then succeeds (scryPostRetried, 4 attempts). Retry-After
    // 0 keeps the shared cooldown instant (a bare 429 would arm 60 s,
    // exactly as upstream would — the limiter math itself is lane B's).
    let fix = build().await;
    *fix.mock.retry_after.lock().unwrap() = Some("0".to_string());
    fix.mock
        .statuses
        .lock()
        .unwrap()
        .insert("POST /cards/collection".to_string(), vec![429, 429, 200]);
    *fix.mock.collection_batches.lock().unwrap() =
        vec![json!({"data": [{"name":"Island","type_line":"Basic Land — Island"}], "not_found": []})];
    let (s, _, _) = post_json(&fix, "{\"list\":\"1 Island\"}").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(fix.mock.hits_for("POST /cards/collection").len(), 3);
}

#[tokio::test]
async fn not_found_without_name_key_becomes_null() {
    let fix = build().await;
    *fix.mock.collection_batches.lock().unwrap() = vec![json!({
        "data": [],
        "not_found": [{"name": "Ghost"}, {"id": "zzz"}],
    })];
    let (s, _, b) = post_json(&fix, "{\"list\":\"1 Ghost\"}").await;
    assert_eq!(s, StatusCode::OK);
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["notFound"], json!(["Ghost", null]));
}

#[tokio::test]
async fn get_rides_the_proxy() {
    let fix = build().await;
    let (s, h, b) = request(&fix.router, peer_n(51), "GET", "/api/limited/analyze", &auth(), None)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-node"], "1");
    assert!(b.contains("\"GET\""));
    assert!(b.contains("/api/limited/analyze"));
}
