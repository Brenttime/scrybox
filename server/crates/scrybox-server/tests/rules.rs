//! Path-level tests for the iter-36 group: `rules`
//! (GET /api/rules[?refresh=1], GET /api/rules/keywords[?refresh=1]).
//!
//! Live byte-vs-Node equality is bench/parity.mjs (rl_* cases). These tests
//! pin the contract locally against the REAL routes/rules.js: kill-switch
//! proxying, the mount-level auth gate, the shared-cache lifecycle (serve
//! with zero upstream hits; stale fallback with `stale` APPENDED; no-cache
//! failure → 502 axios-shape), the build legs (index → CR .txt → commander,
//! sanity gate, crVersion from the URL, cache write-through, mem memo),
//! the parseCR/parseCommander/unescape folds, the keyword head filter +
//! mined-reminder fold (tie = first-observed wins, per-row card count),
//! the kwMem memo + refresh rebuild, ETag conditional GET, and the
//! method/subpath fall-throughs.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, rules, AppState, Config};
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

/// Scripted stand-in for the three upstream legs: index page, CR .txt,
/// commander page. `*_status` flips a leg to an error answer (axios
/// non-2xx rejection), `hits` records every leg call.
struct Origin {
    index: std::sync::Mutex<String>,
    cr: std::sync::Mutex<String>,
    cmd: std::sync::Mutex<String>,
    index_status: std::sync::Mutex<u16>,
    cr_status: std::sync::Mutex<u16>,
    cmd_status: std::sync::Mutex<u16>,
    hits: std::sync::Mutex<Vec<String>>,
}

impl Origin {
    fn new() -> Self {
        Self {
            index: std::sync::Mutex::new(String::new()),
            cr: std::sync::Mutex::new(String::new()),
            cmd: std::sync::Mutex::new(String::new()),
            index_status: std::sync::Mutex::new(200),
            cr_status: std::sync::Mutex::new(200),
            cmd_status: std::sync::Mutex::new(200),
            hits: std::sync::Mutex::new(Vec::new()),
        }
    }
}

async fn spawn_origin(st: Arc<Origin>) -> SocketAddr {
    let s = st.clone();
    let index = get(move || {
        let s = s.clone();
        async move {
            s.hits.lock().unwrap().push("index".into());
            let status = *s.index_status.lock().unwrap();
            if status != 200 {
                return StatusCode::from_u16(status).unwrap().into_response();
            }
            (
                [(header::CONTENT_TYPE, "text/html")],
                s.index.lock().unwrap().clone(),
            )
                .into_response()
        }
    });
    let s = st.clone();
    let cr = get(move || {
        let s = s.clone();
        async move {
            s.hits.lock().unwrap().push("cr".into());
            let status = *s.cr_status.lock().unwrap();
            if status != 200 {
                return StatusCode::from_u16(status).unwrap().into_response();
            }
            ([(header::CONTENT_TYPE, "text/plain")], s.cr.lock().unwrap().clone()).into_response()
        }
    });
    let s = st.clone();
    let cmd = get(move || {
        let s = s.clone();
        async move {
            s.hits.lock().unwrap().push("cmd".into());
            let status = *s.cmd_status.lock().unwrap();
            if status != 200 {
                return StatusCode::from_u16(status).unwrap().into_response();
            }
            (
                [(header::CONTENT_TYPE, "text/html")],
                s.cmd.lock().unwrap().clone(),
            )
                .into_response()
        }
    });
    let r = Router::new()
        .route("/en/rules", index)
        // The CR leg must end in .txt to be found by the index scan.
        .route("/media/CR20260925.txt", cr)
        .route("/commander", cmd);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

/// Synthetic CR doc: the FIRST '1. Game Concepts' is preface noise (parseCR
/// starts at the SECOND occurrence), 1000 numbered rules, keyword heads
/// 701.2 Exile / 702.4 Flying (+ a General-gated 702.9 the filter drops),
/// a glossary and a Credits stop line.
fn cr_doc() -> String {
    let mut s = String::new();
    s.push_str("intro blurb\n1. Game Concepts\npreface noise that must be skipped\n\n");
    s.push_str("1. Game Concepts\n");
    s.push_str("100. Foo Section\n");
    for i in 0..1000 {
        s.push_str(&format!("100.{i}. Rule text {i}\n"));
    }
    s.push_str("701. Actions\n");
    s.push_str("701.1. Definition of keyword actions\n"); // excluded id
    s.push_str("701.2. Exile\n701.2a If an object would be moved to the library, it's exiled instead.\n");
    s.push_str("702. Static Abilities\n");
    s.push_str("702.4. Flying\n702.4a An ability that restricts blocking.\n");
    s.push_str("702.9. General notes on abilities\n"); // /^(General|Keyword)/i gate
    s.push_str("\nGlossary\nCard\nA thing with rules text.\nAnother body line.\n\nMore Glossary Term\nSolo body.\n\nCredits\nBy WotC\n");
    s
}

fn cmd_doc() -> String {
    let mut s = String::from("<html><article><div class=\"entry-content extra\"><h2>Philosophy</h2>");
    for i in 0..6 {
        s.push_str(&format!("<p>Commander rule&nbsp;{i} with   extra\u{a0}space.</p>"));
    }
    s.push_str("</div></article><footer>noise <p>outside article</p></footer></html>");
    s
}

fn seeded_db(dir: &std::path::Path, name: &str, oracle: &[&str]) -> PathBuf {
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
    for (i, t) in oracle.iter().enumerate() {
        conn.execute(
            "INSERT INTO card_cache (id,name,types,subtypes,rarity,price_trend,price_normal,price_holofoil,price_estimated,oracle_text)
             VALUES (?1,'card', 'Creature', '', 'common', 0,0,0,0, ?2)",
            rusqlite::params![i as i64 + 1, t],
        )
        .unwrap();
    }
    path
}

struct Fix {
    router: Router,
    st: AppState,
    origin: Arc<Origin>,
    cache_path: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture(routes: &[&str], oracle: &[&str]) -> Fix {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), "rules.db", oracle);
    let node = spawn_fake_node().await;
    let origin = Arc::new(Origin::new());
    *origin.cr.lock().unwrap() = cr_doc();
    *origin.cmd.lock().unwrap() = cmd_doc();
    let addr = spawn_origin(origin.clone()).await;
    *origin.index.lock().unwrap() =
        format!("see https://other.example/nope.pdf and http://{addr}/media/CR20260925.txt here");
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.rules_index_url = format!("http://{addr}/en/rules");
    st.rules_commander_url = format!("http://{addr}/commander");
    st.rules_cr_base = format!("http://{addr}/media/");
    let cache_path = tmp.path().join("mtg-rules.json");
    st.rules_cache_path = cache_path.clone();
    let router = app(st.clone());
    Fix { router, st, origin, cache_path, _tmp: tmp }
}

async fn get_r(r: &Router, path: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut rb = Request::builder().method("GET").uri(path);
    for &(k, v) in headers {
        rb = rb.header(k, v);
    }
    let req = rb.body(Body::empty()).unwrap();
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

fn seed_cache(path: &std::path::Path, fetched: i64, extra_stale: bool) {
    let mut doc = json!({
        "fetched": fetched,
        "crUrl": "http://origin/media/CR20260925.txt",
        "crVersion": "20260925",
        "commanderUrl": "http://origin/commander",
        "rules": [
            {"src": "Commander", "id": "CMD 1", "section": "Philosophy", "text": "one"},
            {"src": "Comprehensive", "id": "702.4", "section": "702. Static Abilities", "text": "Flying"},
            {"src": "Comprehensive", "id": "702.4a", "section": "702. Static Abilities", "text": "sub a"},
            {"src": "Comprehensive", "id": "701.2", "section": "701. Actions", "text": "Exile"},
            {"src": "Comprehensive", "id": "701.2a", "section": "701. Actions", "text": "sub a"},
            {"src": "Comprehensive", "id": "100", "section": "100. Foo Section", "text": "100. Foo Section", "header": true}
        ]
    });
    if extra_stale {
        doc.as_object_mut().unwrap().insert("stale".into(), json!("old-stale-msg"));
    }
    std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

// ---------------------------------------------------------------------------
// Pure-semantics pins
// ---------------------------------------------------------------------------

#[test]
fn parse_cr_twin() {
    let rows = rules::parse_cr(&cr_doc());
    // preface before the SECOND '1. Game Concepts' skipped: first row is the
    // section header, not the intro noise.
    assert_eq!(rows[0]["text"], "100. Foo Section");
    assert_eq!(rows[0]["header"], true);
    assert_eq!(rows[0]["id"], "100");
    // 1 section header + 1000 rules + 2 action/ability heads (701.1 excluded,
    // 'General' gate drops 702.9, 'Actions'/'Static Abilities' are sec3 heads)
    // + 2 glossary rows.
    assert_eq!(rows.iter().filter(|r| r["src"] == "Glossary").count(), 2);
    let gloss = rows.iter().find(|r| r["id"] == "Card").unwrap();
    assert_eq!(gloss["text"], "A thing with rules text. Another body line.");
    assert_eq!(gloss["section"], "Glossary");
    assert!(rows.iter().all(|r| r["text"] != "By WotC")); // Credits stop
    let rule = rows.iter().find(|r| r["id"] == "100.7").unwrap();
    assert_eq!(rule["text"], "Rule text 7");
    assert_eq!(rule["section"], "100. Foo Section");
}

#[test]
fn parse_cr_example_appends() {
    let txt = "1. Game Concepts\n100. Sec\n101.1. Some rule.\nExample: you cast a spell.\n";
    let rows = rules::parse_cr(txt);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["text"], "Some rule.\nExample: you cast a spell.");
}

#[test]
fn unescape_all_forms() {
    assert_eq!(rules::unescape("&#65;&#x42;&#98;").unwrap(), "ABb");
    assert_eq!(rules::unescape("&amp;lt;").unwrap(), "&lt;"); // &amp; LAST
    assert_eq!(rules::unescape("&nbsp;&quot;&apos;&rsquo;&lsquo;&rdquo;&ldquo;&ndash;&mdash;&lt;&gt;").unwrap(), " \"'''\"\"--<>");
    assert_eq!(rules::unescape("&#X41;").unwrap(), "A"); // /i hex
    assert_eq!(rules::unescape("&#1114112;").unwrap_err(), "Invalid code point 1114112");
    assert_eq!(rules::unescape("&#55296;").unwrap_err(), "Invalid code point 55296");
    assert_eq!(rules::unescape("&#065;").unwrap(), "A"); // leading zeros
}

#[test]
fn parse_commander_twin() {
    let rows = rules::parse_commander(&cmd_doc()).unwrap();
    assert_eq!(rows.len(), 6); // the <p> in <footer> is OUTSIDE the capture
    assert_eq!(rows[0]["id"], "CMD 1");
    assert_eq!(rows[0]["section"], "Philosophy");
    assert_eq!(rows[0]["src"], "Commander");
    // nbsp is JS \s → collapsed to a plain space with the double space.
    assert_eq!(rows[0]["text"], "Commander rule 0 with extra space.");
    // no entry-content div → whole page scanned; tag scan is CASE-SENSITIVE
    // like the JS regex (<P> never matches).
    let rows = rules::parse_commander("<li><b>bold</b>tail</li><p class=x>para&#33;</p><P>caps ignored</P>").unwrap();
    assert_eq!(rows[0]["text"], "boldtail");
    assert_eq!(rows[1]["text"], "para!");
    // nested same-tag: first SAME close wins, content keeps the inner tags
    // stripped; numbering continues across.
    let rows = rules::parse_commander("<ul><li>one<ul><li>two</li></ul></li><li>three</li></ul>").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["text"], "onetwo"); // first </li> ends the first match
    assert_eq!(rows[1]["text"], "three");
    // open tag without ANY close → cursor advances by 1 and rescans.
    let rows = rules::parse_commander("<p>a<p>b</p>").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["text"], "ab");
}

#[test]
fn decode_twin() {
    assert_eq!(rules::decode("\u{FEFF}hello".as_bytes()), "hello");
    assert_eq!(rules::decode("ok".as_bytes()), "ok");
    // invalid utf8 → latin1 of raw bytes (cp1252-ish é byte 0xE9).
    assert_eq!(rules::decode(&[0x65, 0xE9]), "eé");
    // a GENUINE U+FFFD in valid utf8 also triggers the latin1 leg (Node twin).
    assert_eq!(rules::decode("\u{FFFD}x".as_bytes()), "\u{FFFD}x".as_bytes().iter().map(|b| *b as char).collect::<String>());
}

#[tokio::test]
async fn mine_reminders_fold() {
    let f = fixture(
        &["rules"],
        &[
            "Flying (AAA tie reminder one text)",
            "Flying (BBB tie reminder two text)",
            "Ward {2} (Whenever this creature becomes the target, counter that spell.)",
        ],
    )
    .await;
    let mined = rules::mine_reminders(&f.st, &["Flying".into(), "Exile".into(), "Ward".into()]).unwrap();
    assert_eq!(mined["flying"].1, 2, "per-row unique card count");
    assert!(mined["flying"].0.starts_with("AAA"), "tie → first-observed: {:?}", mined["flying"].0);
    assert_eq!(mined["ward"].0, "Whenever this creature becomes the target, counter that spell.");
    assert!(mined.get("exile").is_none());
}

// ---------------------------------------------------------------------------
// Route-level pins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kill_switch_off_proxies() {
    let f = fixture(&[], &[]).await;
    let (s, h, b) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(h["x-node"], "1");
    assert_eq!(json_body(&b)["pq"], "/api/rules");
    let (s, h, _) = get_r(&f.router, "/api/rules/keywords", &ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(h["x-node"], "1");
}

#[tokio::test]
async fn auth_gate_401() {
    let f = fixture(&["rules"], &[]).await;
    let (s, _, b) = get_r(&f.router, "/api/rules", &[]).await;
    assert_eq!(s, 401);
    assert_eq!(json_body(&b)["error"], "Access denied. No token provided.");
    let (s, _, _) = get_r(&f.router, "/api/rules/keywords", &[]).await;
    assert_eq!(s, 401);
    // member role passes the mount-level gate (no requireAdmin in rules.js)
    seed_cache(&f.cache_path, now_ms(), false);
    let (s, _, _) = get_r(&f.router, "/api/rules", &[("authorization", "Bearer sess-member")]).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn cache_serve_zero_upstream() {
    let f = fixture(&["rules"], &[]).await;
    seed_cache(&f.cache_path, now_ms(), false);
    let (s, h, b) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(h[header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert_eq!(h[header::CACHE_CONTROL], "private, max-age=3600");
    assert!(f.origin.hits.lock().unwrap().is_empty());
    // byte-faithful res.json: re-stringified parsed doc, key order preserved.
    let file = std::fs::read(&f.cache_path).unwrap();
    let v: Value = serde_json::from_slice(&file).unwrap();
    assert_eq!(b, serde_json::to_vec(&v).unwrap());
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["fetched", "crUrl", "crVersion", "commanderUrl", "rules"]);
}

#[tokio::test]
async fn stale_fallback_appends_stale() {
    let f = fixture(&["rules"], &[]).await;
    seed_cache(&f.cache_path, now_ms() - 8 * 86400 * 1000, false); // > MAX_AGE
    *f.origin.index_status.lock().unwrap() = 500;
    let (s, _, b) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 200);
    let j = json_body(&b);
    assert_eq!(j["stale"], "Request failed with status code 500");
    // {...mem, stale} → stale APPENDS (no stale key in the source doc).
    let keys: Vec<String> = j.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys.last().unwrap(), "stale");
    // failed refetch never rewrites the cache file.
    let file: Value = serde_json::from_slice(&std::fs::read(&f.cache_path).unwrap()).unwrap();
    assert!(file.get("stale").is_none());
}

#[tokio::test]
async fn stale_key_position_preserved_when_present() {
    let f = fixture(&["rules"], &[]).await;
    seed_cache(&f.cache_path, now_ms() - 8 * 86400 * 1000, true); // stale at tail already
    *f.origin.index_status.lock().unwrap() = 404;
    let (s, _, b) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 200);
    let j = json_body(&b);
    assert_eq!(j["stale"], "Request failed with status code 404");
}

#[tokio::test]
async fn no_cache_fetch_fail_502() {
    let f = fixture(&["rules"], &[]).await;
    *f.origin.index_status.lock().unwrap() = 500;
    let (s, _, b) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 502);
    assert_eq!(json_body(&b)["error"], "Could not load rules: Request failed with status code 500");
}

#[tokio::test]
async fn fresh_build_writes_serves_and_memos() {
    let f = fixture(&["rules"], &[]).await;
    let (s, _, b) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 200);
    let j = json_body(&b);
    let keys: Vec<String> = j.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["fetched", "crUrl", "crVersion", "commanderUrl", "rules"]);
    assert_eq!(j["crVersion"], "20260925"); // digits8 fold off the matched URL
    assert_eq!(j["crUrl"], format!("{}/media/CR20260925.txt", f.st.rules_index_url.replace("/en/rules", "")));
    assert_eq!(j["rules"][0]["src"], "Commander"); // cmd rows lead
    assert_eq!(j["rules"][0]["id"], "CMD 1");
    assert_eq!(j["rules"][6]["src"], "Comprehensive"); // then CR (6 cmd rows)
    // build wrote the shared cache file
    let file = std::fs::read(&f.cache_path).unwrap();
    assert_eq!(file, serde_json::to_vec(&j).unwrap());
    let hits1 = f.origin.hits.lock().unwrap().clone();
    assert_eq!(hits1, ["index", "cr", "cmd"]);
    // mem memo: second request is zero-upstream
    let (s, _, _) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(*f.origin.hits.lock().unwrap(), hits1);
}

#[tokio::test]
async fn refresh_forces_refetch_even_when_fresh() {
    let f = fixture(&["rules"], &[]).await;
    seed_cache(&f.cache_path, now_ms(), false);
    let (s, _, b) = get_r(&f.router, "/api/rules?refresh=1", &ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(*f.origin.hits.lock().unwrap(), ["index", "cr", "cmd"]);
    assert_ne!(json_body(&b)["fetched"], json!(now_ms() - 1)); // rebuilt value
}

#[tokio::test]
async fn no_txt_link_and_sanity_errors() {
    let f = fixture(&["rules"], &[]).await;
    *f.origin.index.lock().unwrap() = "no links here".into();
    let (s, _, b) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 502);
    assert_eq!(json_body(&b)["error"], "Could not load rules: could not find Comprehensive Rules .txt link");

    let f = fixture(&["rules"], &[]).await;
    *f.origin.cmd.lock().unwrap() = "<p>too few</p>".into(); // cmd.length < 5
    let (s, _, b) = get_r(&f.router, "/api/rules", &ADMIN).await;
    assert_eq!(s, 502);
    // sanity gate counts parseCR's FULL output (glossary rows included).
    assert_eq!(json_body(&b)["error"], format!("Could not load rules: parse sanity failed cr={} cmd=1", 1011));
}

#[tokio::test]
async fn keywords_fold_memo_and_refresh() {
    let f = fixture(
        &["rules"],
        &[
            "Flying (This creature can't be blocked except by creatures with flying.)",
            "Exile (Put this card into the exile zone.) and more text",
        ],
    )
    .await;
    let (s, h, b) = get_r(&f.router, "/api/rules/keywords", &ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(h[header::CACHE_CONTROL], "private, max-age=3600");
    let j = json_body(&b);
    let keys: Vec<String> = j.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["fetched", "crVersion", "keywords"]);
    let kws = j["keywords"].as_array().unwrap();
    assert_eq!(kws.len(), 2); // 701.2 Exile + 702.4 Flying; 701.1/General excluded
    assert_eq!(kws[0]["name"], "Exile"); // localeCompare: Exile < Flying
    assert_eq!(kws[0]["kind"], "action");
    assert_eq!(kws[0]["id"], "701.2");
    assert_eq!(kws[0]["reminder"], "Put this card into the exile zone.");
    assert_eq!(kws[0]["summary"], "If an object would be moved to the library, it's exiled instead.");
    assert_eq!(kws[0]["rules"][0]["id"], "701.2a");
    assert_eq!(kws[1]["kind"], "ability");
    assert_eq!(kws[1]["cards"], 1);
    let row_keys: Vec<String> = kws[0].as_object().unwrap().keys().cloned().collect();
    assert_eq!(row_keys, ["name", "id", "kind", "reminder", "summary", "cards", "rules"]);

    // memo: DB change is invisible until refresh
    {
        let conn = Connection::open(&f.st.cfg.db_path.unwrap()).unwrap();
        conn.execute("UPDATE card_cache SET oracle_text = 'Flying (Flying gets blocked only by flying creatures now.)' WHERE id = 1", []).unwrap();
    }
    let (_, _, b2) = get_r(&f.router, "/api/rules/keywords", &ADMIN).await;
    assert_eq!(json_body(&b2)["keywords"][1]["reminder"], "This creature can't be blocked except by creatures with flying.");
    let (_, _, b3) = get_r(&f.router, "/api/rules/keywords?refresh=1", &ADMIN).await;
    assert_eq!(json_body(&b3)["keywords"][1]["reminder"], "Flying gets blocked only by flying creatures now.");
}

#[tokio::test]
async fn keywords_502_without_rules() {
    let f = fixture(&["rules"], &[]).await;
    *f.origin.index_status.lock().unwrap() = 500;
    let (s, _, b) = get_r(&f.router, "/api/rules/keywords", &ADMIN).await;
    assert_eq!(s, 502);
    assert_eq!(json_body(&b)["error"], "Could not load keywords: Request failed with status code 500");
}

#[tokio::test]
async fn etag_conditional_get() {
    let f = fixture(&["rules"], &[]).await;
    seed_cache(&f.cache_path, now_ms(), false);
    let (_, h, _) = get_r(&f.router, "/api/rules", &ADMIN).await;
    let tag = h[header::ETAG].to_str().unwrap().to_string();
    assert!(tag.starts_with("W/\""), "express default weak tag: {tag}");
    let (s, h2, b) = get_r(&f.router, "/api/rules", &[ADMIN[0], ("if-none-match", tag.as_str())]).await;
    assert_eq!(s, 304);
    assert!(b.is_empty());
    assert_eq!(h2[header::ETAG], tag);
    // no-cache directive disables the 304 (npm fresh twin)
    let (s, _, _) = get_r(
        &f.router,
        "/api/rules",
        &[ADMIN[0], ("if-none-match", tag.as_str()), ("cache-control", "no-cache")],
    )
    .await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn methods_and_subpaths_fall_through() {
    let f = fixture(&["rules"], &[]).await;
    let (s, h, b) = {
        let req = Request::builder()
            .method("POST")
            .uri("/api/rules")
            .header("authorization", "Bearer sess-admin")
            .body(Body::from("{}"))
            .unwrap();
        let resp = f.router.clone().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        let b = axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec();
        (parts.status, parts.headers, b)
    };
    assert_eq!(s, 200);
    assert_eq!(h["x-node"], "1");
    assert_eq!(json_body(&b)["method"], "POST");
    assert_eq!(json_body(&b)["pq"], "/api/rules");
    // unknown subpath under the mount: no route registered → proxy → Node 404
    let (s, h, _) = get_r(&f.router, "/api/rules/other", &ADMIN).await;
    assert_eq!(h["x-node"], "1");
    assert_eq!(s, 200); // the fake node echoes; real Node 404s, through the proxy
}
