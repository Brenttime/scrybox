//! Path-level tests for the lane-A iter-5 group: `decks_combos`
//! (GET /api/decks/:id/combos, GET /api/decks/:id/synergy).
//!
//! Provider-leg ruling (same as marketplace/moxfield): the Spellbook and
//! EDHREC URLs are Node module constants, the Node container is read-only
//! for the parity harness and never recreated, so the real provider legs
//! are EXCLUDED from the parity matrix and stub-proven here — via the
//! injectable `deck_http` client on AppState, exactly like mp_http.
//!
//! These tests pin: kill-switch proxying, the auth gate with
//! parse-before-auth order, the 404 arms, the empty-deck / no-commander
//! 200 shapes (incl. the missing `slug` key on the hint-less early return),
//! the Spellbook body it receives, the trimCombo output (key order,
//! ??-vs-|| arms, front-face missing fallback), the 502 arms (net fail,
//! !ok answered, JSON-parse throw, truthy non-array), the ETag/304 path,
//! the EDHREC two-tier cache (mem memo for 404s, fresh-disk short circuit,
//! stale-disk fallback, best-effort write-through) and the partner-pair
//! slug ladder.

use std::{collections::{BTreeSet, VecDeque}, net::SocketAddr, path::PathBuf, sync::{Arc, Mutex}};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{
    app,
    decks_combos::{DecksProviders, FetchResp},
    AppState, Config,
};
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

// ---------------------------------------------------------------------------
// Stub provider (mp_http StubHttp precedent)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StubProv {
    log: Arc<Mutex<Vec<(String, String, Option<String>, Vec<(String, String)>)>>>,
    script: Arc<Mutex<VecDeque<Result<(u16, Vec<u8>), ()>>>>,
}

impl StubProv {
    fn new() -> Self {
        StubProv { log: Arc::new(Mutex::new(vec![])), script: Arc::new(Mutex::new(VecDeque::new())) }
    }
    fn push(&self, r: Result<(u16, Vec<u8>), ()>) {
        self.script.lock().unwrap().push_back(r);
    }
    fn ok(&self, v: Value) {
        self.push(Ok((200, serde_json::to_vec(&v).unwrap())));
    }
    fn ok_raw(&self, status: u16, bytes: &[u8]) {
        self.push(Ok((status, bytes.to_vec())));
    }
    fn net(&self) {
        self.push(Err(()));
    }
    fn log(&self) -> Vec<(String, String, Option<String>, Vec<(String, String)>)> {
        self.log.lock().unwrap().clone()
    }
}

impl DecksProviders for StubProv {
    fn fetch(
        &self,
        method: &'static str,
        url: String,
        body: Option<String>,
        extra_headers: Vec<(String, String)>,
        _timeout_ms: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FetchResp, ()>> + Send + 'static>>
    {
        self.log.lock().unwrap().push((method.to_string(), url, body, extra_headers));
        let next = self.script.lock().unwrap().pop_front();
        Box::pin(async move {
            match next {
                Some(Ok((status, bytes))) => Ok(FetchResp { status, bytes }),
                Some(Err(())) => Err(()),
                None => panic!("stub provider ran out of scripted responses"),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Fixtures (seeded_db copied from tests/decks_reads.rs so both groups share
// one corpus; decks 101..105, sessions sess-1 -> user 1, api key 'k55')
// ---------------------------------------------------------------------------

fn seeded_db(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "CREATE TABLE deck_maybe_cards (
            deck_id INTEGER NOT NULL,
            card_id TEXT NOT NULL,
            quantity INTEGER DEFAULT 1,
            PRIMARY KEY(deck_id, card_id)
         );
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL),
                  (2,'u2','x','member','tok-2',0,'k59');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id,name,supertype,subtypes,types,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency)
           VALUES
             ('z-etali','Etali, Primal Storm','MTG','[\"Legendary\",\"Creature\",\"Dragon\"]','[\"Red\",\"Green\"]','Rare','one','Outlaws','1','img/etali-a',1.0,1.0,0,0,0,6,'[\"R\",\"G\"]','USD'),
             ('a-vial','Aether Vial','MTG','[\"Artifact\"]','[]','Uncommon','dmu','Dominaria','1','img/vial-a',0.1,0.1,0,0,0,1,'[]','USD'),
             ('plains-x','Plains','MTG','[\"Basic\",\"Land\",\"Plains\"]','[]','Common','dmu','Dominaria','261',null,0.01,0.01,0,0,0,0,'[]','USD'),
             ('eur-card','Euro Thing','MTG','[\"Artifact\"]','[]','Common','dmu','Dominaria','9',null,2.0,2.0,0,0,0,1,'[]','EUR'),
             ('z-front','Far','MTG','[\"Creature\"]','[]','Common','dmu','Dominaria','7',null,0.5,0.5,0,0,0,2,'[]','USD');

         INSERT INTO decks (id,user_id,name,description,checked_out,created_at) VALUES
             (101,1,'Commander Deck','cd',0,'2026-01-01 00:00:05'),
             (102,1,'Coverage Deck','cv',0,'2026-01-01 00:00:04'),
             (104,1,'Broken Deck','bd',0,'2026-01-01 00:00:02'),
             (105,2,'Foreign Deck','fd',0,'2026-01-01 00:00:01');
         INSERT INTO deck_cards (deck_id,card_id,quantity,is_commander) VALUES
             (101,'z-etali',1,0),
             (101,'a-vial',4,0),
             (101,'z-front',2,0),
             (102,'plains-x',3,0),
             (102,'a-vial',1,0),
             (104,'zzz-nope',1,0);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);
         INSERT OR IGNORE INTO collection_read_revision (id, revision) VALUES (1, 1);
         INSERT OR IGNORE INTO stats_aux_revision (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    path
}

fn router_with(
    db_path: PathBuf,
    routes: &[&str],
    upstream: &str,
    prov: StubProv,
    cache_dir: Option<PathBuf>,
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
    st.deck_http = Arc::new(prov);
    st.edhrec_cache_dir = cache_dir;
    app(st)
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

async fn fix(routes: &[&str]) -> (Router, StubProv, tempfile::TempDir) {
    build(routes, None, None).await
}

async fn build(
    routes: &[&str],
    cache: Option<PathBuf>,
    extra_sql: Option<&str>,
) -> (Router, StubProv, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_fake_node().await;
    let prov = StubProv::new();
    let db = seeded_db(dir.path(), "dc.db");
    if let Some(sql) = extra_sql {
        Connection::open(&db).unwrap().execute_batch(sql).unwrap();
    }
    let rs: Vec<&str> = routes.to_vec();
    let r = router_with(db, &rs, &format!("http://{upstream}"), prov.clone(), cache);
    (r, prov, dir)
}

const BOTH: [&str; 1] = ["decks_combos"];

// ---------------------------------------------------------------------------
// kill switch + auth gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_proxies_both_routes() {
    let (r, prov, _d) = fix(&[]).await;
    for path in ["/api/decks/101/combos", "/api/decks/101/synergy"] {
        let (s, h, _) = request(&r, "GET", path, &AUTH, None).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert_eq!(h.get("x-node").unwrap(), "1", "{path}");
    }
    assert!(prov.log().is_empty());
}

#[tokio::test]
async fn auth_gate_and_parse_before_auth() {
    let (r, _p, _d) = fix(&BOTH).await;
    for path in ["/api/decks/101/combos", "/api/decks/101/synergy"] {
        let (s, _, b) = request(&r, "GET", path, &[], None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(json_body(&b), json!({ "error": "Access denied. No token provided." }));
        let bad = [("authorization", "Bearer nope")];
        let (s, _, b) = request(&r, "GET", path, &bad, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(json_body(&b), json!({ "error": "Invalid or expired session token." }));
        // express.json ahead of the /api auth gate: bad body + no token -> 500.
        let ct = [("content-type", "application/json")];
        let (s, _, b) = request(&r, "GET", path, &ct, Some("{nope")).await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{path}");
        assert_eq!(json_body(&b), json!({ "error": "Internal server error" }));
    }
}

#[tokio::test]
async fn not_found_arms() {
    let (r, _p, _d) = fix(&BOTH).await;
    for path in ["/api/decks/999/combos", "/api/decks/105/combos", "/api/decks/999/synergy", "/api/decks/105/synergy"] {
        let (s, _, b) = request(&r, "GET", path, &AUTH, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(json_body(&b), json!({ "error": "Deck not found or unauthorized" }));
    }
}

// ---------------------------------------------------------------------------
// combos
// ---------------------------------------------------------------------------

#[tokio::test]
async fn combos_empty_rows_short_circuits_without_provider() {
    // Deck 104's only card id is not in card_cache -> JOIN yields 0 rows ->
    // the {identity:'',included:[],almostIncluded:[]} early return.
    let (r, prov, _d) = fix(&BOTH).await;
    let (s, h, b) = request(&r, "GET", "/api/decks/104/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        String::from_utf8_lossy(&b),
        "{\"identity\":\"\",\"included\":[],\"almostIncluded\":[]}"
    );
    assert!(h.get("etag").unwrap().to_str().unwrap().starts_with("W/"));
    assert!(prov.log().is_empty());
}

#[tokio::test]
async fn combos_ok_byte_exact_body_request_and_trim() {
    let (r, prov, _d) = fix(&BOTH).await;
    // Three identical provider responses: the 200, the ETag probe (the
    // handler fetches before it can compute the tag — Node does too), and
    // the If-None-Match 304 replay.
    prov.ok(spellbook_combo_payload());
    prov.ok(spellbook_combo_payload());
    prov.ok(spellbook_combo_payload());
    let (s, _, b) = request(&r, "GET", "/api/decks/101/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    // The POST body Node sends: commanders [] (fixture has is_commander=0),
    // main with Number(quantity)||1 on the SUM values, GROUP BY name order.
    let log = prov.log();
    assert_eq!(log.len(), 1);
    let (method, url, body, headers) = &log[0];
    assert_eq!(method, "POST");
    assert_eq!(url, "https://backend.commanderspellbook.com/find-my-combos");
    assert_eq!(
        serde_json::from_str::<Value>(body.as_ref().unwrap()).unwrap(),
        json!({ "commanders": [], "main": [
            { "card": "Aether Vial", "quantity": 4 },
            { "card": "Etali, Primal Storm", "quantity": 1 },
            { "card": "Far", "quantity": 2 }
        ] })
    );
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v == "application/json"));
    assert!(headers.iter().any(|(k, v)| k == "accept" && v == "application/json"));
    // Trim output: exact bytes (key order + ??/|| arms + front-face missing
    // fallback: deck stores "Far", Spellbook says "Far // Near" -> in deck).
    assert_eq!(
        String::from_utf8_lossy(&b),
        concat!(
            "{\"identity\":\"RG\",\"included\":[{\"id\":7,\"cards\":[",
            "{\"name\":\"Aether Vial\",\"image\":\"s1\",\"missing\":false},",
            "{\"name\":\"Far // Near\",\"image\":\"n2\",\"missing\":false},",
            "{\"name\":\"\",\"image\":null,\"missing\":true}],\"templates\":[\"Creature\"],\"produces\":[\"Draw\"],",
            "\"manaNeeded\":\"R\",\"prerequisites\":\"easy\",\"description\":\"desc\",",
            "\"bracketTag\":null,\"popularity\":0,\"identity\":\"\"}],\"almostIncluded\":[]}"
        )
    );
    // Express-default weak ETag on res.json + If-None-Match -> 304.
    let etag = {
        let (_, h, _) = request(&r, "GET", "/api/decks/101/combos", &AUTH, None).await;
        h.get("etag").unwrap().to_str().unwrap().to_string()
    };
    let inm = [("authorization", "Bearer sess-1"), ("if-none-match", etag.as_str())];
    let (s, _, _) = request(&r, "GET", "/api/decks/101/combos", &inm, None).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn combos_502_arms() {
    // Network failure -> the route catch's single 502 string.
    let (r, prov, _d) = fix(&BOTH).await;
    prov.net();
    let (s, _, b) = request(&r, "GET", "/api/decks/101/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({ "error": "Commander Spellbook is unreachable right now" }));
    // !ok -> answered {status}.
    prov.ok_raw(500, b"nope");
    let (s, _, b) = request(&r, "GET", "/api/decks/101/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({ "error": "Commander Spellbook answered 500" }));
    // ok but unparseable JSON -> upstream.json() throws -> the same catch.
    prov.ok_raw(200, b"<html>");
    let (s, _, b) = request(&r, "GET", "/api/decks/101/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({ "error": "Commander Spellbook is unreachable right now" }));
    // JSON body literal null -> `data.results` TypeError -> the same catch.
    prov.ok_raw(200, b"null");
    let (s, _, b) = request(&r, "GET", "/api/decks/101/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({ "error": "Commander Spellbook is unreachable right now" }));
    // truthy non-array `uses` -> .map TypeError -> the same catch.
    prov.ok(json!({ "results": { "included": [{ "uses": 5 }] } }));
    let (s, _, b) = request(&r, "GET", "/api/decks/101/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({ "error": "Commander Spellbook is unreachable right now" }));
}

#[tokio::test]
async fn combos_non_object_results_fold_to_empty() {
    // `data.results || {}` — a falsy results (or missing) answers the
    // identity-less shape, NOT an error.
    let (r, prov, _d) = fix(&BOTH).await;
    prov.ok(json!({ "results": false }));
    let (s, _, b) = request(&r, "GET", "/api/decks/101/combos", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&b), "{\"identity\":\"\",\"included\":[],\"almostIncluded\":[]}");
}

// ---------------------------------------------------------------------------
// synergy
// ---------------------------------------------------------------------------

fn edhrec_page() -> Value {
    json!({ "container": { "json_dict": { "cardlists": [
        { "cardviews": [
            { "name": "Brainstorm", "synergy": 0.87 },
            { "name": "Sol Ring", "synergy": "0.4" },
            { "name": "Underworld", "synergy": 0.34 }
        ] }
    ] } } })
}

fn spellbook_combo_payload() -> Value {
    json!({
        "results": {
            "identity": "RG",
            "included": [{
                "id": 7,
                "uses": [
                    { "card": { "name": "Aether Vial", "imageUriFrontSmall": "s1" } },
                    { "card": { "name": "Far // Near", "imageUriFrontNormal": "n2" } },
                    { "card": null }
                ],
                "requires": [{ "template": { "name": "Creature" } }, { "template": { "name": "" } }, {}],
                "produces": [{ "feature": { "name": "Draw" } }, { "feature": {} }],
                "manaNeeded": "R",
                "easyPrerequisites": "easy",
                "notablePrerequisites": "",
                "description": "desc",
                "popularity": 0
            }],
            "almostIncluded": []
        }
    })
}

const PAGE_BYTES: &str =
    "{\"commander\":\"HINTNAME\",\"slug\":\"hinter\",\"threshold\":0.35,\"cards\":[{\"name\":\"Brainstorm\",\"synergy\":0.87},{\"name\":\"Sol Ring\",\"synergy\":0.4}]}";

#[tokio::test]
async fn synergy_no_commander_no_hint_early_return() {
    // Deck 102 has cards but no is_commander rows and no hint ->
    // { commander: null, threshold, cards: [] } with NO slug key.
    let (r, prov, _d) = fix(&BOTH).await;
    let (s, _, b) = request(&r, "GET", "/api/decks/102/synergy", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        String::from_utf8_lossy(&b),
        "{\"commander\":null,\"threshold\":0.35,\"cards\":[]}"
    );
    assert!(prov.log().is_empty());
}

#[tokio::test]
async fn synergy_hint_ok_slug_encoding_and_cards() {
    let (r, prov, _d) = fix(&BOTH).await;
    prov.ok(edhrec_page());
    let (s, _, b) = request(
        &r,
        "GET",
        "/api/decks/102/synergy?commander=Hinter",
        &AUTH,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&b), PAGE_BYTES.replace("HINTNAME", "Hinter"));
    let log = prov.log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0, "GET");
    assert_eq!(log[0].1, "https://json.edhrec.com/pages/commanders/hinter.json");
    assert!(log[0].3.iter().any(|(k, v)| k == "accept" && v == "application/json"));
}

#[tokio::test]
async fn synergy_404_memo_and_fallthrough() {
    // 404 arms negative-memo in mem only (no disk file) and fall through to
    // the slug:null empty-cards response.
    let (r, prov, _d) = fix(&BOTH).await;
    prov.ok_raw(404, b"nope");
    let (s, _, b) = request(&r, "GET", "/api/decks/102/synergy?commander=Ghost", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&b), "{\"commander\":\"Ghost\",\"slug\":null,\"threshold\":0.35,\"cards\":[]}");
    // Second hit serves from the negative memo: no new provider fetch.
    let (s, _, _) = request(&r, "GET", "/api/decks/102/synergy?commander=Ghost", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(prov.log().len(), 1);
}

#[tokio::test]
async fn synergy_partner_pair_slug_ladder() {
    // Two commanders: the combined (slug-sorted join) page is tried first;
    // its 404 falls through to the first commander's own page.
    let (r, prov, _d) = build(
        &BOTH,
        None,
        Some(
            "INSERT INTO decks (id,user_id,name,description,checked_out,created_at) VALUES (106,1,'Pair','p',0,'2026-01-01 00:00:00');
             INSERT INTO deck_cards (deck_id,card_id,quantity,is_commander) VALUES (106,'z-etali',1,1),(106,'a-vial',1,1);",
        ),
    )
    .await;
    prov.ok_raw(404, b"");
    prov.ok(edhrec_page());
    let (s, _, b) = request(&r, "GET", "/api/decks/106/synergy", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    let log = prov.log();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].1, "https://json.edhrec.com/pages/commanders/aether-vial-etali-primal-storm.json");
    assert_eq!(log[1].1, "https://json.edhrec.com/pages/commanders/aether-vial.json");
    assert_eq!(
        json_body(&b),
        json!({ "commander": "Aether Vial + Etali, Primal Storm", "slug": "aether-vial", "threshold": 0.35, "cards": [
            { "name": "Brainstorm", "synergy": 0.87 },
            { "name": "Sol Ring", "synergy": 0.4 }
        ] })
    );
    // Both slugs are mem-cached now: zero new fetches on a repeat.
    let (s, _, _) = request(&r, "GET", "/api/decks/102/synergy", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(prov.log().len(), 2);
}

#[tokio::test]
async fn synergy_disk_fresh_short_circuits_and_stale_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("edhrec-cache");
    std::fs::create_dir_all(&cache).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // fresh for `freshguy`, stale (25 h) for `staleguy`.
    std::fs::write(
        cache.join("freshguy.json"),
        json!({ "t": now, "cards": [{ "name": "DiskCard", "synergy": 0.5 }] }).to_string(),
    )
    .unwrap();
    std::fs::write(
        cache.join("staleguy.json"),
        json!({ "t": now - 25 * 3600_000, "cards": [{ "name": "StaleCard", "synergy": 0.6 }] }).to_string(),
    )
    .unwrap();
    let (r, prov, _d) = build(&BOTH, Some(cache.clone()), None).await;
    // fresh disk -> no fetch at all.
    let (s, _, b) = request(&r, "GET", "/api/decks/102/synergy?commander=Freshguy", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        String::from_utf8_lossy(&b),
        "{\"commander\":\"Freshguy\",\"slug\":\"freshguy\",\"threshold\":0.35,\"cards\":[{\"name\":\"DiskCard\",\"synergy\":0.5}]}"
    );
    assert!(prov.log().is_empty());
    // stale disk + net failure -> stale fallback, NOT the 502.
    prov.net();
    let (s, _, b) = request(&r, "GET", "/api/decks/102/synergy?commander=Staleguy", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        String::from_utf8_lossy(&b),
        "{\"commander\":\"Staleguy\",\"slug\":\"staleguy\",\"threshold\":0.35,\"cards\":[{\"name\":\"StaleCard\",\"synergy\":0.6}]}"
    );
    assert_eq!(prov.log().len(), 1);
    // stale disk + !ok answer -> the same stale fallback (throw -> catch -> disk).
    prov.ok_raw(500, b"nope");
    let (s, _, _) = request(&r, "GET", "/api/decks/102/synergy?commander=Staleguy", &AUTH, None).await;
    // mem was refreshed by the previous fetch attempt? No: fallback does NOT
    // memo, so this is a second fetch + stale answer again.
    assert_eq!(s, StatusCode::OK);
    assert_eq!(prov.log().len(), 2);
}

#[tokio::test]
async fn synergy_net_fail_without_disk_is_502() {
    let (r, prov, _d) = fix(&BOTH).await;
    prov.net();
    let (s, _, b) = request(&r, "GET", "/api/decks/102/synergy?commander=Doomed", &AUTH, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(&b), json!({ "error": "EDHREC is unreachable right now" }));
    // !ok without disk -> same 502.
    prov.ok_raw(500, b"nope");
    let (s, _, _) = request(&r, "GET", "/api/decks/102/synergy?commander=Doomed", &AUTH, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn synergy_write_through_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("edhrec-cache");
    let (r, prov, _d) = build(&BOTH, Some(cache.clone()), None).await;
    prov.ok(edhrec_page());
    let (s, _, _) = request(&r, "GET", "/api/decks/102/synergy?commander=Writer", &AUTH, None).await;
    assert_eq!(s, StatusCode::OK);
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(cache.join("writer.json")).unwrap()).unwrap();
    assert_eq!(on_disk["cards"][0]["name"], "Brainstorm");
    assert!(on_disk["t"].is_i64() || on_disk["t"].is_u64() || on_disk["t"].is_f64());
    // key order t,cards like Node's JSON.stringify(entry).
    assert_eq!(
        on_disk.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["t", "cards"]
    );
}
