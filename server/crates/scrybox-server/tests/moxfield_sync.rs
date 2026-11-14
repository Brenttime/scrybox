//! Lane B unit 3a: the Moxfield sync engine + scheduler + /api/moxfield
//! routes, driven against a scripted mock Moxfield (the real client execs
//! `curl` — the tests exercise that path for real, pointed at localhost) and
//! the shared Scryfall mock for the card backfill leg. No live upstream.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    response::Response,
    Router,
};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use tower::ServiceExt;

use mock_scryfall::{raw_card, scry_for, spawn_mock};
use scrybox_server::{
    app,
    mfx_payload::{
        board_counts, extract_deck_cards, extract_maybe_cards, js_to_string, mfx_format_label,
        scrybox_card_id, target_size_for_format,
    },
    moxfield::{
        self, add_author, clamp_interval, get_status, pull_deck_content_by_public_id,
        remove_author, run_content_sync, set_deck_enabled, sync_decklist, ErrKind,
    },
    moxfield_api::MfxApi,
    scryfall::Scry,
    AppState, Config,
};

#[path = "mock_scryfall.rs"]
mod mock_scryfall;

// ---------------------------------------------------------------------------
// Mock Moxfield — mirrors the three upstream endpoints the client hits:
//   GET /v2/users/search-sfw?filter=...        -> {"data":[{userName,...}]}
//   GET /v2/decks/search-sfw?pageNumber=N...   -> {"data":[rows],"totalPages":n}
//   GET /v3/decks/all/{publicId}               -> deck details
// Per-key status scripts let tests script 404/403/5xx rejects.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MockMfx {
    users: Mutex<Value>,
    pages: Mutex<Vec<Value>>,
    details: Mutex<HashMap<String, Value>>,
    statuses: Mutex<HashMap<String, Vec<u16>>>,
    hits: Mutex<Vec<String>>,
}

impl MockMfx {
    pub fn hits_for(&self, prefix: &str) -> Vec<String> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|h| h.starts_with(prefix))
            .cloned()
            .collect()
    }
    fn next_status(&self, key: &str) -> u16 {
        // The current hit is already recorded: index with count-1 so a
        // script [404, 200] serves 404 first, 200 after.
        let hits = self
            .hits
            .lock()
            .unwrap()
            .iter()
            .filter(|k| k == &key)
            .count()
            .saturating_sub(1);
        let script = self.statuses.lock().unwrap();
        match script.get(key) {
            Some(v) if !v.is_empty() => *v.get(hits).or_else(|| v.last()).unwrap(),
            _ => 200,
        }
    }
}

pub async fn spawn_mfx_mock() -> (SocketAddr, Arc<MockMfx>) {
    let state = Arc::new(MockMfx::default());
    *state.users.lock().unwrap() = json!({"data": []});
    *state.pages.lock().unwrap() = vec![json!({"data": [], "totalPages": 1})];

    let app_state = state.clone();
    let router = Router::new().fallback(move |req: Request| {
        let state = app_state.clone();
        async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let query = req.uri().query().unwrap_or("").to_string();
            let key = format!("{method} {path}");
            state.hits.lock().unwrap().push(format!("{key}?{query}"));
            let status = state.next_status(&key);
            let json = |status: StatusCode, body: &Value| -> Response {
                Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap()
            };
            if status >= 400 {
                return json(
                    StatusCode::from_u16(status).unwrap(),
                    &json!({"status_code": status, "mock": path}),
                );
            }
            match (method.as_str(), path.as_str()) {
                ("GET", "/v2/users/search-sfw") => {
                    json(StatusCode::OK, &state.users.lock().unwrap().clone())
                }
                ("GET", "/v2/decks/search-sfw") => {
                    let idx = state
                        .hits_for("GET /v2/decks/search-sfw")
                        .len()
                        .saturating_sub(1);
                    let pages = state.pages.lock().unwrap();
                    let body = pages
                        .get(idx)
                        .or_else(|| pages.last())
                        .cloned()
                        .unwrap_or_else(|| json!({"data": [], "totalPages": 1}));
                    json(StatusCode::OK, &body)
                }
                ("GET", p) if p.starts_with("/v3/decks/all/") => {
                    let id = p.trim_start_matches("/v3/decks/all/");
                    // The URL path is percent-encoded; the map keys are raw.
                    let id = percent_decode(id);
                    match state.details.lock().unwrap().get(&id).cloned() {
                        Some(d) => json(StatusCode::OK, &d),
                        None => json(
                            StatusCode::NOT_FOUND,
                            &json!({"status_code": 404, "mock": "no-deck"}),
                        ),
                    }
                }
                (m, p) => json(
                    StatusCode::NOT_FOUND,
                    &json!({"status_code": 404, "mock": format!("{m} {p}")}),
                ),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (addr, state)
}

fn percent_decode(s: &str) -> String {
    // Only %XX is needed here (public ids are alphanumeric-ish); decode utf-8
    // bytes safely.
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn mfx_client(addr: SocketAddr) -> Arc<MfxApi> {
    Arc::new(MfxApi {
        base_url: format!("http://{addr}"),
        // Plain curl: the impersonate probe must never hijack the mock leg.
        curl_bin: "curl".into(),
        // Node's execFile wall (TIMEOUT_S + 5); the mock never gets near it.
        exec_timeout: std::time::Duration::from_secs(35),
    })
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const UUID_A: &str = "00000000-0000-4000-8000-00000000000a"; // cached
const UUID_B: &str = "00000000-0000-4000-8000-00000000000b"; // unknown to scryfall
const UUID_C: &str = "00000000-0000-4000-8000-00000000000c"; // commander

fn seed_card(conn: &Connection, id: &str, name: &str, set: &str, number: &str, source: &str) {
    conn.execute(
        "INSERT OR REPLACE INTO card_cache (id, name, types, set_id, set_name, number, price_source, language) VALUES (?, ?, 'Creature', ?, ?, ?, ?, 'English')",
        rusqlite::params![id, name, set, format!("{set} Set"), number, source],
    )
    .unwrap();
}

struct Fixture {
    // Held so the temp DB survives the test body; never read.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

fn seeded_db(name: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(include_str!("scry_schema.sql")).unwrap();
    conn.execute_batch(include_str!("moxfield_schema.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, api_key, share_token) VALUES (1,'u1','x','admin','tok-api','share-1');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-session', 1, '2099-01-01 00:00:00');",
    )
    .unwrap();
    seed_card(&conn, &format!("mtg-{UUID_A}"), "Alpha Card", "xyz", "1", "scryfall");
    seed_card(&conn, &format!("mtg-{UUID_C}"), "Commander Card", "xyz", "3", "scryfall");
    Fixture { dir, path }
}

fn engine_state(fx: &Fixture, mfx: Arc<MfxApi>, scry: Option<Scry>) -> AppState {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: "http://127.0.0.1:1".to_string(),
        static_dir: None,
        rust_routes: Default::default(),
        db_path: Some(fx.path.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.mfx = mfx;
    if let Some(s) = scry {
        st.scry = Arc::new(s);
    }
    st
}

fn conn_of(fx: &Fixture) -> Connection {
    Connection::open(&fx.path).unwrap()
}

fn one_row(conn: &Connection, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Option<Map<String, Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query_map(p, |row| {
        let mut m = Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v: rusqlite::types::Value = row.get(i)?;
            m.insert(
                c.clone(),
                match v {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(i) => Value::from(i),
                    rusqlite::types::Value::Real(r) => Value::from(r),
                    rusqlite::types::Value::Text(s) => Value::from(s),
                    rusqlite::types::Value::Blob(_) => Value::from("<blob>"),
                },
            );
        }
        Ok(m)
    }).unwrap();
    rows.next().and_then(|r| r.ok())
}

fn rows_len(conn: &Connection, sql: &str, p: &[&dyn rusqlite::ToSql]) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM ({sql})"),
        rusqlite::params_from_iter(p.iter().copied()),
        |r| r.get(0),
    )
    .unwrap()
}

fn board(cards: &[(&str, &str, i64, bool)]) -> Value {
    // cards: (uuid, name, quantity, is_commander_via_board) — commander flag
    // comes from the BOARDS key, the payload just carries the map.
    let mut m = Map::new();
    for (uuid, name, qty, _) in cards {
        m.insert(
            format!("key-{uuid}"),
            json!({"quantity": qty, "card": {"scryfall_id": uuid, "name": name}}),
        );
    }
    Value::Object(m)
}

fn deck_details(public_id: &str, name: &str, format: &str, stamp: &str) -> Value {
    let _ = public_id;
    json!({
        "name": name,
        "description": "desc",
        "format": format,
        "lastUpdatedAtUtc": stamp,
        "boards": {
            "mainboard": {"count": 2, "cards": board(&[(UUID_A, "Alpha Card", 2, false)])},
            "sideboard": {"count": 0, "cards": Value::Null},
            "maybeboard": {"count": 1, "cards": board(&[(UUID_B, "Mystery Card", 1, false)])},
            "commanders": {"count": 1, "cards": board(&[(UUID_C, "Commander Card", 1, true)])},
        }
    })
}

fn summary(public_id: &str, name: &str, format: &str, stamp: &str) -> Value {
    json!({"publicId": public_id, "name": name, "format": format, "lastUpdatedAtUtc": stamp})
}

fn set_pages(mock: &Arc<MockMfx>, pages: Vec<Value>) {
    *mock.pages.lock().unwrap() = pages;
}
fn set_user(mock: &Arc<MockMfx>, entries: Value) {
    *mock.users.lock().unwrap() = json!({"data": entries});
}
fn set_details(mock: &Arc<MockMfx>, public_id: &str, details: Option<Value>) {
    match details {
        Some(d) => {
            mock.details.lock().unwrap().insert(public_id.into(), d);
        }
        None => {
            mock.details.lock().unwrap().remove(public_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Pure ports — mfxPayload + scheduler math
// ---------------------------------------------------------------------------

#[test]
fn payload_pure_folds() {
    assert_eq!(mfx_format_label(Some(&json!("standard"))), "Standard");
    assert_eq!(mfx_format_label(Some(&json!("EDH"))), "Commander / EDH");
    assert_eq!(mfx_format_label(Some(&json!("Oathbreaker"))), "Commander / EDH");
    // default branch: the RAW truthy value passes through…
    assert_eq!(mfx_format_label(Some(&json!("Brawl"))), "Brawl");
    // …and falsy collapses to 'Commander / EDH'
    assert_eq!(mfx_format_label(Some(&json!(""))), "Commander / EDH");
    assert_eq!(mfx_format_label(None), "Commander / EDH");

    assert_eq!(target_size_for_format(Some(&json!("Modern"))), 75);
    assert_eq!(target_size_for_format(Some(&json!("Commander"))), 100);
    assert_eq!(target_size_for_format(None), 100);

    // Template-literal coercion: a number id never collapses to bare "mtg-".
    assert_eq!(scrybox_card_id(&json!({"scryfall_id": 123})), "mtg-123");
    assert_eq!(scrybox_card_id(&json!({"scryfall_id": UUID_A})), format!("mtg-{UUID_A}"));
    assert_eq!(scrybox_card_id(&json!({"id": UUID_A})), "mtg-");
    assert_eq!(scrybox_card_id(&Value::Null), "mtg-");

    assert_eq!(js_to_string(&json!(2.0)), "2");
    assert_eq!(js_to_string(&json!(2.5)), "2.5");
    assert_eq!(js_to_string(&json!(true)), "true");

    let details = deck_details("p", "n", "Commander", "2026-01-01T00:00:00Z");
    let main = extract_deck_cards(&details);
    let maybe = extract_maybe_cards(&details);
    let counts = board_counts(
        &main
            .iter()
            .cloned()
            .chain(maybe.iter().cloned())
            .collect::<Vec<_>>(),
    );
    assert_eq!(counts, [2, 0, 1, 1]);

    // Malformed boards never panic: entries without a card/scryfall_id or a
    // zero quantity are skipped exactly like Node's extract.
    let junk = json!({"boards": {"mainboard": {"cards": {"a": {"quantity": "3x", "card": {"scryfall_id": "zz"}}}}}});
    assert_eq!(extract_deck_cards(&junk)[0].quantity, 3); // parseInt("3x")=3
    let junk0 = json!({"boards": {"mainboard": {"cards": {"a": {"quantity": 0, "card": {"scryfall_id": "zz"}}}}}});
    assert!(extract_deck_cards(&junk0).is_empty());
    let junk_noc = json!({"boards": {"mainboard": {"cards": {"a": {"quantity": "3"}}}}});
    assert!(extract_deck_cards(&junk_noc).is_empty());
    let _ = extract_deck_cards(&json!(null));
}

#[test]
fn scheduler_clamp_interval() {
    // parseInt semantics: prefix parse, garbage → fallback; <1 → fallback;
    // capped at 1440.
    assert_eq!(clamp_interval(None, 60), 60);
    assert_eq!(clamp_interval(Some(&json!(null)), 60), 60);
    assert_eq!(clamp_interval(Some(&json!("abc")), 60), 60);
    assert_eq!(clamp_interval(Some(&json!(0)), 60), 60);
    assert_eq!(clamp_interval(Some(&json!(-5)), 60), 60);
    assert_eq!(clamp_interval(Some(&json!(1)), 60), 1);
    assert_eq!(clamp_interval(Some(&json!("720")), 60), 720);
    assert_eq!(clamp_interval(Some(&json!(1_000_000)), 60), 1440);
    assert_eq!(clamp_interval(Some(&json!("12abc")), 60), 12);
    assert_eq!(clamp_interval(Some(&json!(1440)), 60), 1440);
}

// ---------------------------------------------------------------------------
// API client — reject ladder + pick-once user + pagination (real curl exec)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_reject_ladder() {
    let (addr, mock) = spawn_mfx_mock().await;
    let api = mfx_client(addr);

    // 404 → "Moxfield has no …" style with status 404 (get_user path).
    set_user(&mock, json!([]));
    let e = api.get_user("ghost").await.unwrap_err();
    assert_eq!(e.status, 404);
    assert!(e.message.contains("Moxfield has no user named"));

    // Scripted HTTP statuses on the details endpoint.
    set_details(&mock, "d1", Some(json!({"name": "x"})));
    mock.statuses.lock().unwrap().insert("GET /v3/decks/all/d1".into(), vec![403]);
    let e = api.get_deck_details("d1").await.unwrap_err();
    assert_eq!(e.status, 403);
    mock.statuses.lock().unwrap().insert("GET /v3/decks/all/d1".into(), vec![500]);
    let e = api.get_deck_details("d1").await.unwrap_err();
    assert_eq!(e.status, 500);

    // Dead port → status 0, "Could not reach Moxfield".
    let dead = mfx_client("127.0.0.1:1".parse().unwrap());
    let e = dead.get_deck_details("x").await.unwrap_err();
    assert_eq!(e.status, 0);
    assert!(e.message.starts_with("Could not reach Moxfield"), "{e:?}");
}

#[tokio::test]
async fn api_get_user_pick_once_and_pagination() {
    let (addr, mock) = spawn_mfx_mock().await;
    let api = mfx_client(addr);

    // Exact case-insensitive match wins over entries[0]; all fields come from
    // the SAME picked entry.
    set_user(
        &mock,
        json!([
            {"userName": "SomeoneElse", "displayName": "Other", "profileImageUrl": "u0"},
            {"userName": "Guido", "profileImageUrl": "u1"}
        ]),
    );
    let u = api.get_user("guido").await.unwrap();
    assert_eq!(u.user_name, "Guido");
    // displayName falsy → userName
    assert_eq!(u.display_name, "Guido");
    assert_eq!(u.profile_image_url.as_deref(), Some("u1"));

    // Falsy userName on the picked entry → 404 even with a first entry.
    set_user(&mock, json!([{"displayName": "no name"}]));
    let e = api.get_user("whatever").await.unwrap_err();
    assert_eq!(e.status, 404);

    // Pagination: pageSize=1 walks totalPages=2; empty-rows break wins.
    set_pages(
        &mock,
        vec![
            json!({"data": [summary("d1", "One", "standard", "s1")], "totalPages": 2}),
            json!({"data": [summary("d2", "Two", "modern", "s2")], "totalPages": 2}),
        ],
    );
    let rows = api
        .get_author_deck_summaries_with_page_size("guido", 1)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let hits = mock.hits_for("GET /v2/decks/search-sfw");
    assert!(hits[0].contains("pageNumber=1") && hits[1].contains("pageNumber=2"), "{hits:?}");
    assert!(hits[0].contains("authorUserNames=guido"));
}

// ---------------------------------------------------------------------------
// add_author
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_author_full_flow() {
    let fx = seeded_db("add.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_user(&mock, json!([{"userName": "Guido", "displayName": "Guido T", "profileImageUrl": "pi"}]));
    set_pages(
        &mock,
        vec![json!({"data": [
            summary("deckA", "Deck A", "commander", "2026-01-01T00:00:00Z"),
            summary("deckB", "Deck B", "standard", "2026-01-02T00:00:00Z"),
        ], "totalPages": 1})],
    );
    set_details(&mock, "deckA", Some(deck_details("deckA", "Deck A", "commander", "2026-01-01T00:00:00Z")));
    set_details(&mock, "deckB", Some(deck_details("deckB", "Deck B", "standard", "2026-01-02T00:00:00Z")));
    // Scryfall knows UUID_B; the backfill writes it through normalize+cache.
    *smock.collection_batches.lock().unwrap() =
        vec![json!({"data": [raw_card(UUID_B, "Mystery Card", "xyz", "2", Some(1.5))], "not_found": []})];

    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));
    let report = add_author(&st, 1, &json!(" guido ")).await.unwrap();
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["author"], json!("Guido"));
    assert_eq!(report["decks_created"], json!(2));
    assert_eq!(report["decks_removed"], json!(0));

    let conn = conn_of(&fx);
    let author = one_row(&conn, "SELECT * FROM moxfield_authors WHERE user_id=1", &[]).unwrap();
    assert_eq!(author["moxfield_user"], json!("Guido"));
    assert_eq!(author["display_name"], json!("Guido T"));
    assert_eq!(author["profile_image_url"], json!("pi"));
    assert!(author["last_decklist_sync_at"] != Value::Null);

    // Backfilled via the Scryfall mock (identifier OBJECTS, uuid form).
    let hits = smock.hits_for("POST /cards/collection");
    assert_eq!(hits.len(), 1, "{hits:?}");
    let body: Value = serde_json::from_str(&hits[0].1).unwrap();
    assert_eq!(body["identifiers"][0]["id"], json!(UUID_B));

    // deckA: commander mirror, target 100; deckB: standard mirror, 75.
    let da = one_row(
        &conn,
        "SELECT d.* FROM decks d WHERE d.moxfield_public_id='deckA'",
        &[],
    )
    .unwrap();
    assert_eq!(da["source"], json!("moxfield"));
    assert_eq!(da["format"], json!("Commander / EDH"));
    assert_eq!(da["target_size"], json!(100));
    let db_ = one_row(
        &conn,
        "SELECT d.* FROM decks d WHERE d.moxfield_public_id='deckB'",
        &[],
    )
    .unwrap();
    assert_eq!(db_["target_size"], json!(75));

    // deckA cards: Alpha x2 (main) + commander x1 = 2 rows.
    let deck_a_id = da["id"].as_i64().unwrap();
    assert_eq!(
        rows_len(&conn, "SELECT 1 FROM deck_cards WHERE deck_id=?", &[&deck_a_id]),
        2 // Alpha + Commander Card
    );
    assert_eq!(
        rows_len(&conn, "SELECT 1 FROM deck_cards WHERE deck_id=? AND quantity=2", &[&deck_a_id]),
        1 // Alpha x2 only
    );
    let cmd = one_row(
        &conn,
        "SELECT is_commander FROM deck_cards WHERE deck_id=? AND card_id=?",
        &[&deck_a_id, &format!("mtg-{UUID_C}")],
    )
    .unwrap();
    assert_eq!(cmd["is_commander"], json!(1));
    let non_cmd = one_row(
        &conn,
        "SELECT is_commander FROM deck_cards WHERE deck_id=? AND card_id=?",
        &[&deck_a_id, &format!("mtg-{UUID_A}")],
    )
    .unwrap();
    assert_eq!(non_cmd["is_commander"], json!(0));
    assert_eq!(
        rows_len(&conn, "SELECT 1 FROM deck_maybe_cards WHERE deck_id=? AND card_id=?", &[&deck_a_id, &format!("mtg-{UUID_B}")]),
        1
    );

    // Synthesized/backfilled card is cached; tracking rows hold stamps and
    // mirror pointers.
    assert!(one_row(&conn, "SELECT id FROM card_cache WHERE id=?", &[&format!("mtg-{UUID_B}")]).is_some());
    // Regression: the Scryfall-resolved row must SURVIVE the still-missing
    // recheck. An earlier cut read the TEXT id via get::<i64> (always Err →
    // always "missing") and clobbered this row with the synthesized minimal
    // card — price_source would have read 'moxfield' and price_normal null.
    let resolved_b = one_row(
        &conn,
        "SELECT price_source, price_normal FROM card_cache WHERE id=?",
        &[&format!("mtg-{UUID_B}")],
    )
    .unwrap();
    assert_eq!(resolved_b["price_source"], json!("scryfall"));
    assert_eq!(resolved_b["price_normal"], json!(1.5));
    let row = one_row(
        &conn,
        "SELECT * FROM moxfield_decks WHERE public_id='deckA'",
        &[],
    )
    .unwrap();
    assert_eq!(row["last_synced_updated_at"], json!("2026-01-01T00:00:00Z"));
    assert_eq!(row["last_updated_at"], json!("2026-01-01T00:00:00Z"));
    assert_eq!(row["mainboard_count"], json!(2));
    assert_eq!(row["commander_count"], json!(1));
    assert_eq!(row["bindarr_deck_id"], da["id"]);
    assert_eq!(row["enabled"], json!(1));

    // The mock saw one decklist pass and both full pulls.
    assert_eq!(mock.hits_for("GET /v2/decks/search-sfw").len(), 1);
    assert_eq!(mock.hits_for("GET /v3/decks/all/deckA").len(), 1);
    assert_eq!(mock.hits_for("GET /v3/decks/all/deckB").len(), 1);

    drop(fx);
}

#[tokio::test]
async fn add_author_validation_and_404() {
    let fx = seeded_db("add404.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, _) = spawn_mock().await;
    set_user(&mock, json!([]));
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    // Non-string username → 400-shape plain error (routes map to 400).
    let e = add_author(&st, 1, &Value::Null).await.unwrap_err();
    assert_eq!(e.kind, ErrKind::Plain);
    assert!(e.message.contains("Moxfield username is required"), "{e:?}");
    let e = add_author(&st, 1, &json!("   ")).await.unwrap_err();
    assert!(e.message.contains("Moxfield username is required"));

    // Unknown Moxfield user → Mox(404).
    let e = add_author(&st, 1, &json!("ghost")).await.unwrap_err();
    assert_eq!(e.kind, ErrKind::Mox(404));

    let conn = conn_of(&fx);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_authors", &[]), 0);
}

#[tokio::test]
async fn add_author_already_tracked_pulls_again() {
    let fx = seeded_db("dup.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_user(&mock, json!([{"userName": "Guido"}]));
    set_pages(&mock, vec![json!({"data": [summary("deckA", "Deck A", "commander", "s1")], "totalPages": 1})]);
    set_details(&mock, "deckA", Some(deck_details("deckA", "Deck A", "commander", "s1")));
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let r1 = add_author(&st, 1, &json!("guido")).await.unwrap();
    assert_eq!(r1["decks_created"], json!(1));
    let r2 = add_author(&st, 1, &json!("Guido")).await.unwrap();
    assert_eq!(r2["already_tracked"], json!(true));
    assert_eq!(r2["author"], json!("Guido")); // report spread overwrites (Node)
    assert_eq!(r2["decks_created"], json!(0));

    let conn = conn_of(&fx);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_authors", &[]), 1);
    // Node re-runs syncDecklist on the already-tracked path → second listing.
    assert_eq!(mock.hits_for("GET /v2/decks/search-sfw").len(), 2);
}

#[tokio::test]
async fn add_author_initial_pull_failure_records_last_error() {
    let fx = seeded_db("pullfail.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_user(&mock, json!([{"userName": "Guido"}]));
    set_pages(&mock, vec![json!({"data": [summary("deckA", "Deck A", "commander", "s1")], "totalPages": 1})]);
    set_details(&mock, "deckA", Some(json!({}))); // 200 non-JSON body path:
    // force a scripted 500 instead — details map present but status script wins.
    mock.statuses.lock().unwrap().insert("GET /v3/decks/all/deckA".into(), vec![500]);
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];

    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));
    // The decklist sync still succeeds; the failed pull lands on the row.
    let r = add_author(&st, 1, &json!("guido")).await.unwrap();
    assert_eq!(r["decks_created"], json!(1));
    let conn = conn_of(&fx);
    let row = one_row(&conn, "SELECT last_error FROM moxfield_decks WHERE public_id='deckA'", &[]).unwrap();
    let err = row["last_error"].as_str().unwrap();
    assert!(err.contains("HTTP 500"), "{err}");
}

// ---------------------------------------------------------------------------
// Decklist sync — retire + conflict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn decklist_sync_retires_gone_decks_and_conflicts_on_checked_out() {
    let fx = seeded_db("retire.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_user(&mock, json!([{"userName": "Guido"}]));
    // Remote now only lists deckA; deckB/goneC/goneD must be retired.
    set_pages(&mock, vec![json!({"data": [summary("deckA", "Deck A", "commander", "s1")], "totalPages": 1})]);
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let conn = conn_of(&fx);
    conn.execute(
        "INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (1, 1, 'Guido')",
        [],
    )
    .unwrap();
    // deckB: enabled mirror with cards → must conflict (retire_deck guard).
    conn.execute(
        "INSERT INTO decks (id, user_id, name, source, moxfield_public_id, checked_out) VALUES (10, 1, 'Deck B', 'moxfield', 'deckB', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (10, ?, 1)",
        rusqlite::params![format!("mtg-{UUID_A}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, bindarr_deck_id, enabled) VALUES (1, 'deckB', 'Deck B', 10, 1)",
        [],
    )
    .unwrap();
    // goneC: enabled but CHECKED OUT → conflict too.
    conn.execute(
        "INSERT INTO decks (id, user_id, name, source, moxfield_public_id, checked_out) VALUES (11, 1, 'Deck C', 'moxfield', 'goneC', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, bindarr_deck_id, enabled) VALUES (1, 'goneC', 'Deck C', 11, 1)",
        [],
    )
    .unwrap();
    // goneD: disabled, no mirror → plain delete of the tracking row.
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, bindarr_deck_id, enabled) VALUES (1, 'goneD', 'Deck D', NULL, 0)",
        [],
    )
    .unwrap();

    let e = sync_decklist(&st, Some(1), Some(1)).await.unwrap_err();
    assert_eq!(e.kind, ErrKind::Conflict);
    assert!(e.message.contains("Check this deck in before removing"), "{e:?}");
    // Nothing after the conflict ran: goneD row still present, stamps unset.
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_decks WHERE public_id='goneD'", &[]), 1);

    // Check deck 11 in (deckB's mirror was already retired by the first
    // pass — it wasn't checked out) and sync again: goneC's mirror + cards
    // go, goneD goes.
    conn.execute("UPDATE decks SET checked_out = 0 WHERE id = 11", []).unwrap();
    let r = sync_decklist(&st, Some(1), Some(1)).await.unwrap();
    assert_eq!(r["decks_removed"], json!(2));
    // Only deckA's freshly created mirror survives; deckB/goneC mirrors are gone.
    assert_eq!(
        rows_len(&conn, "SELECT 1 FROM decks WHERE source='moxfield' AND moxfield_public_id != 'deckA'", &[]),
        0
    );
    assert_eq!(rows_len(&conn, "SELECT 1 FROM deck_cards WHERE deck_id=10", &[]), 0);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_decks WHERE public_id IN ('deckB','goneD')", &[]), 0);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_decks WHERE public_id='deckA'", &[]), 1);
}

// ---------------------------------------------------------------------------
// Content sync — stamp rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn content_sync_pulls_only_changed_stamps() {
    let fx = seeded_db("content.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_user(&mock, json!([{"userName": "Guido"}]));
    set_pages(
        &mock,
        vec![json!({"data": [
            summary("c1", "Changed", "commander", "NEWSTAMP"),      // stamp moved → pull
            summary("c2", "Same", "commander", "OLD"),               // unchanged → skip
            summary("c3", "Built", "commander", "OLD"),              // built+seen → skip w/o pull
            summary("c4", "Disabled", "commander", "NEWSTAMP"),      // tracking off → skip
        ], "totalPages": 1})],
    );
    set_details(&mock, "c1", Some(deck_details("c1", "Changed", "commander", "NEWSTAMP")));
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (1, 1, 'Guido')", []).unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, format, last_updated_at, last_synced_updated_at, enabled) VALUES
         (1,'c1','Changed','commander','OLD','OLD',1),
         (1,'c2','Same','commander','OLD','OLD',1),
         (1,'c3','Built','commander','OLD','OLD',1),
         (1,'c4','Disabled','commander','OLD','OLD',0)",
        [],
    )
    .unwrap();
    // c3 counts as "built": mirror deck with cards matching its reported counts.
    conn.execute(
        "INSERT INTO decks (id, user_id, name, source, moxfield_public_id) VALUES (30, 1, 'Built', 'moxfield', 'c3')",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE moxfield_decks SET bindarr_deck_id = 30 WHERE public_id='c3'",
        [],
    )
    .unwrap();
    drop(conn);

    let r = run_content_sync(&st, Some(1), Some(1)).await.unwrap();
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["checked"], json!(4));
    assert_eq!(r["updated"], json!(1));
    assert_eq!(r["removed"], json!(0));

    // Only c1 was pulled in full; c2/c3/c4 never hit the details endpoint.
    assert_eq!(mock.hits_for("GET /v3/decks/all/c1").len(), 1);
    assert_eq!(mock.hits_for("GET /v3/decks/all/c2").len(), 0);
    assert_eq!(mock.hits_for("GET /v3/decks/all/c3").len(), 0);
    assert_eq!(mock.hits_for("GET /v3/decks/all/c4").len(), 0);

    let conn = conn_of(&fx);
    let c1 = one_row(&conn, "SELECT * FROM moxfield_decks WHERE public_id='c1'", &[]).unwrap();
    assert_eq!(c1["last_synced_updated_at"], json!("NEWSTAMP"));
    assert!(c1["last_content_sync_at"] != Value::Null);
    assert_eq!(c1["last_error"], Value::Null);
    let author = one_row(&conn, "SELECT last_content_check_at FROM moxfield_authors WHERE id=1", &[]).unwrap();
    assert!(author["last_content_check_at"] != Value::Null);
}

#[tokio::test]
async fn content_sync_missing_summary_retires_and_failures_stamp_last_error() {
    let fx = seeded_db("content2.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_user(&mock, json!([{"userName": "Guido"}]));
    // c1 moved (pull will 500), c2 vanished (disabled row → retired).
    set_pages(&mock, vec![json!({"data": [summary("c1", "One", "commander", "NEW")], "totalPages": 1})]);
    set_details(&mock, "c1", Some(json!({})));
    mock.statuses.lock().unwrap().insert("GET /v3/decks/all/c1".into(), vec![502]);
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (1, 1, 'Guido')", []).unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, last_updated_at, last_synced_updated_at, enabled) VALUES
         (1,'c1','One','OLD','OLD',1),(1,'c2','Two','OLD','OLD',0)",
        [],
    )
    .unwrap();
    drop(conn);

    let r = run_content_sync(&st, Some(1), Some(1)).await.unwrap();
    assert_eq!(r["updated"], json!(0));
    assert_eq!(r["removed"], json!(1));
    let conn = conn_of(&fx);
    let c1 = one_row(&conn, "SELECT last_error FROM moxfield_decks WHERE public_id='c1'", &[]).unwrap();
    assert!(c1["last_error"].as_str().unwrap().contains("HTTP 502"));
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_decks WHERE public_id='c2'", &[]), 0);
}

// ---------------------------------------------------------------------------
// Forced deck pull + enable/disable + remove_author
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forced_pull_untracked_and_already_built_skip() {
    let fx = seeded_db("forced.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (1, 1, 'Guido')", []).unwrap();
    drop(conn);

    // Unknown public id → plain not-found (routes match /not found|not
    // tracked/i → 404, moxfieldSync.js:629).
    let e = pull_deck_content_by_public_id(&st, 1, "nope").await.unwrap_err();
    assert_eq!(e.kind, ErrKind::Plain);
    assert!(e.message.contains("not found"), "{e:?}");
    assert_eq!(mock.hits_for("GET /v3/decks/all").len(), 0, "must not fetch first");
}

#[tokio::test]
async fn enable_disable_roundtrip_and_checked_out_conflict() {
    let fx = seeded_db("toggle.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_details(&mock, "t1", Some(deck_details("t1", "T One", "commander", "s1")));
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (1, 1, 'Guido')", []).unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, format, last_updated_at, enabled) VALUES (1,'t1','T One','commander','s1',1)",
        [],
    )
    .unwrap();
    drop(conn);

    // Disable a deck that never got a mirror: no pull happened yet; flipping
    // on pulls it (Node: enabling re-imports).
    let r = set_deck_enabled(&st, 1, "t1", true).await.unwrap();
    assert_eq!(r["ok"], json!(true));
    assert_eq!(mock.hits_for("GET /v3/decks/all/t1").len(), 1);
    let conn = conn_of(&fx);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM decks WHERE moxfield_public_id='t1' AND source='moxfield'", &[]), 1);

    // Disable: mirror + cards are dropped, tracking row survives.
    let r = set_deck_enabled(&st, 1, "t1", false).await.unwrap();
    assert_eq!(r["enabled"], json!(false));
    assert_eq!(rows_len(&conn, "SELECT 1 FROM decks WHERE moxfield_public_id='t1'", &[]), 0);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_decks WHERE public_id='t1' AND enabled=0", &[]), 1);

    // Checked-out mirror → conflict on disable.
    conn.execute(
        "INSERT INTO decks (id, user_id, name, source, moxfield_public_id, checked_out) VALUES (40, 1, 'T One', 'moxfield', 't1', 1)",
        [],
    )
    .unwrap();
    conn.execute("UPDATE moxfield_decks SET bindarr_deck_id=40, enabled=1 WHERE public_id='t1'", []).unwrap();
    let e = set_deck_enabled(&st, 1, "t1", false).await.unwrap_err();
    assert_eq!(e.kind, ErrKind::Conflict);

    // Unknown deck for this user → plain not-found.
    let e = set_deck_enabled(&st, 1, "ghost", true).await.unwrap_err();
    assert!(e.message.contains("not found"), "{e:?}");
}

#[tokio::test]
async fn remove_author_cascades_mirrors() {
    let fx = seeded_db("rmauthor.db");
    let (maddr, _mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (1, 1, 'Guido')", []).unwrap();
    conn.execute(
        "INSERT INTO decks (id, user_id, name, source, moxfield_public_id, checked_out) VALUES (50, 1, 'R', 'moxfield', 'r1', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (50, ?, 1)",
        rusqlite::params![format!("mtg-{UUID_A}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO deck_maybe_cards (deck_id, card_id, quantity) VALUES (50, ?, 1)",
        rusqlite::params![format!("mtg-{UUID_A}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, bindarr_deck_id, enabled) VALUES (1,'r1','R',50,1)",
        [],
    )
    .unwrap();
    drop(conn);

    let r = remove_author(&st, 1, Some(1)).await.unwrap();
    assert_eq!(r["ok"], json!(true));
    let conn = conn_of(&fx);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_authors", &[]), 0);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_decks", &[]), 0);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM decks WHERE id=50", &[]), 0);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM deck_cards WHERE deck_id=50", &[]), 0);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM deck_maybe_cards WHERE deck_id=50", &[]), 0);

    // Checked-out mirror → conflict, author survives.
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (2, 1, 'Two')", []).unwrap();
    conn.execute(
        "INSERT INTO decks (id, user_id, name, source, moxfield_public_id, checked_out) VALUES (51, 1, 'C', 'moxfield', 'c1', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, bindarr_deck_id, enabled) VALUES (2,'c1','C',51,1)",
        [],
    )
    .unwrap();
    let e = remove_author(&st, 1, Some(2)).await.unwrap_err();
    assert_eq!(e.kind, ErrKind::Conflict);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_authors WHERE id=2", &[]), 1);

    // Unknown / other-user author → not-found.
    let e = remove_author(&st, 1, Some(999)).await.unwrap_err();
    assert!(e.message.contains("not found"), "{e:?}");
}

// ---------------------------------------------------------------------------
// Scheduler tick — due logic + failure stamping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scheduler_tick_due_order() {
    // sched_state is process-global keyed by author id — use a fresh id so
    // parallel tests never share the clock maps.
    let fx = seeded_db("tick.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_user(&mock, json!([{"userName": "Ticker"}]));
    set_pages(
        &mock,
        vec![json!({"data": [summary("td1", "T Deck", "commander", "STAMP1")], "totalPages": 1})],
    );
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (9001, 1, 'Ticker')", []).unwrap();
    // One enabled tracked deck: runContentSync short-circuits (no network,
    // no stamp) with zero enabled decks — Node's early return.
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, enabled) VALUES (9001, 'td1', 'T Deck', 1)",
        [],
    )
    .unwrap();
    drop(conn);

    // Tick 1: both are due (clocks are 0) — decklist wins, exactly one job.
    moxfield::tick(&st).await;
    assert_eq!(mock.hits_for("GET /v2/decks/search-sfw").len(), 1);
    let conn = conn_of(&fx);
    let a = one_row(&conn, "SELECT last_decklist_sync_at, last_content_check_at FROM moxfield_authors WHERE id=9001", &[]).unwrap();
    assert!(a["last_decklist_sync_at"] != Value::Null);
    assert_eq!(a["last_content_check_at"], Value::Null);

    // Tick 2: decklist not due, content is (last_content clock still 0) —
    // the content check runs (one more summaries call + author stamp).
    moxfield::tick(&st).await;
    assert_eq!(mock.hits_for("GET /v2/decks/search-sfw").len(), 2); // summaries call
    let a = one_row(&conn, "SELECT last_content_check_at FROM moxfield_authors WHERE id=9001", &[]).unwrap();
    assert!(a["last_content_check_at"] != Value::Null);

    // Tick 3: neither due (content marked now < 1 min ago) — no upstream call.
    moxfield::tick(&st).await;
    assert_eq!(mock.hits_for("GET /v2/decks/search-sfw").len(), 2);
}

#[tokio::test]
async fn scheduler_tick_failure_stamps_author_last_error() {
    let fx = seeded_db("tickfail.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let (saddr, smock) = spawn_mock().await;
    set_user(&mock, json!([{"userName": "Failer"}]));
    mock.statuses.lock().unwrap().insert("GET /v2/users/search-sfw".into(), vec![503]);
    *smock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let st = engine_state(&fx, mfx_client(maddr), Some(scry_for(saddr, 0.01)));

    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (9002, 1, 'Failer')", []).unwrap();
    drop(conn);

    moxfield::tick(&st).await;
    let conn = conn_of(&fx);
    let a = one_row(&conn, "SELECT last_error FROM moxfield_authors WHERE id=9002", &[]).unwrap();
    assert!(a["last_error"].as_str().unwrap().contains("HTTP 503"), "{a:?}");
}

// ---------------------------------------------------------------------------
// get_status fold
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_status_fold_shapes() {
    let fx = seeded_db("status.db");
    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (1, 1, 'Guido')", []).unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, format, mainboard_count, last_updated_at, last_synced_updated_at, enabled) VALUES
         (1,'a','A','commander',60,'t2','t2',1),
         (1,'b','B','standard',60,'t3',NULL,1),
         (1,'c','C','modern',60,'t1','tX',0)",
        [],
    )
    .unwrap();
    drop(conn);

    let conn = conn_of(&fx);
    let authors = get_status(&conn, 1).unwrap();
    assert_eq!(authors.len(), 1);
    let a = &authors[0];
    assert_eq!(a["total_decks"], json!(2), "enabled-only aggregate");
    assert_eq!(a["current_decks"], json!(1), "synced==updated counts once");
    assert_eq!(a["error_decks"], json!(0));
    assert_eq!(a["tracked_decks"], json!(3), "every tracked deck");
    let decks = a["decks"].as_array().unwrap();
    assert_eq!(decks.len(), 3);
    // ORDER BY last_updated_at DESC → b(t3), a(t2), c(t1)
    assert_eq!(decks[0]["public_id"], json!("b"));
    assert_eq!(decks[0]["current"], json!(false));
    assert_eq!(decks[1]["current"], json!(true));
    assert_eq!(decks[2]["enabled"], json!(false), "enabled !== 0 fold");
    assert_eq!(decks[0]["card_target"], json!(75), "standard target");
    assert_eq!(decks[1]["card_target"], json!(100), "commander target");
    assert_eq!(decks[1]["card_count"], json!(60));

    // Other users never see it.
    assert_eq!(get_status(&conn, 2).unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Route surface — auth ladder + native answers + method-mismatch proxy
// ---------------------------------------------------------------------------

async fn spawn_fake_node() -> SocketAddr {
    let echo = axum::routing::any(|req: Request| async move {
        let parts = req.into_parts().0;
        (
            StatusCode::OK,
            [("x-node", "1")],
            axum::Json(json!({"pq": parts.uri.path_and_query().unwrap().as_str()})),
        )
    });
    let r = Router::new().fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

async fn router_for(fx: &Fixture, upstream: &str, mfx: Arc<MfxApi>) -> Router {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: upstream.to_string(),
        static_dir: None,
        rust_routes: ["moxfield".to_string()].into_iter().collect(),
        db_path: Some(fx.path.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.mfx = mfx;
    app(st)
}

fn with_auth(mut req: Request<Body>, token: &str) -> Request<Body> {
    // Node's middleware splits `Authorization` on spaces and takes [1].
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    req
}

async fn body_of(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn routes_auth_ladder_and_status_shape() {
    let fx = seeded_db("routes.db");
    let (maddr, _mock) = spawn_mfx_mock().await;
    let node = spawn_fake_node().await;
    let router = router_for(&fx, &format!("http://{node}"), mfx_client(maddr)).await;

    // No token → 401 'Access denied' (the global /api gate is native).
    let resp = router
        .clone()
        .oneshot(Request::get("/api/moxfield").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_of(resp).await["error"], json!("Access denied. No token provided."));

    // GET status (authed, empty DB) — exact shape + key order.
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/moxfield").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let native = resp.headers().get("x-node").is_none();
    let body = body_of(resp).await;
    assert_eq!(body, json!({"authors": [], "intervals": {"decklist_min": 60, "content_min": 1}}));
    assert_eq!(body.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["authors", "intervals"]);
    assert!(native, "status must be native");

    // POST authors without a token → 401; with an API key → 403 read-only.
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/moxfield/authors")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"username": "x"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::post("/api/moxfield/authors")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"username": "x"}).to_string()))
                .unwrap(),
            "tok-api",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Missing username (authed) → 400 'Moxfield username is required'.
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::post("/api/moxfield/authors")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({}).to_string()))
                .unwrap(),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_of(resp).await["error"],
        json!("Moxfield username is required")
    );

    // PUT unknown deck → 404 (Node message pattern), from Rust itself.
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::put("/api/moxfield/decks/ghost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"enabled": false}).to_string()))
                .unwrap(),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(resp.headers().get("x-node").is_none(), true);

    // Method mismatch → proxied untouched (Node owns unregistered shapes).
    let resp = router
        .clone()
        .oneshot(with_auth(Request::delete("/api/moxfield").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.headers().get("x-node").unwrap(), "1");
}

#[tokio::test]
async fn routes_delete_author_and_forced_sync_shapes() {
    let fx = seeded_db("routes2.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    let node = spawn_fake_node().await;
    let router = router_for(&fx, &format!("http://{node}"), mfx_client(maddr)).await;

    // DELETE unknown author → 404 'Moxfield author not found'.
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::delete("/api/moxfield/authors/999").body(Body::empty()).unwrap(),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_of(resp).await["error"], json!("Moxfield author not found"));

    // POST authors/:id/sync-decklist unknown author → 404.
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::post("/api/moxfield/authors/999/sync-decklist").body(Body::empty()).unwrap(),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // POST decks/:publicId/sync untracked → 404 'not tracked'.
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::post("/api/moxfield/decks/ghost/sync").body(Body::empty()).unwrap(),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(body_of(resp).await["error"]
        .as_str()
        .unwrap()
        .contains("not found"));

    // Full native add-author through the route: 201 + report.
    set_user(&mock, json!([{"userName": "Guido", "displayName": "G"}]));
    set_pages(&mock, vec![json!({"data": [], "totalPages": 1})]);
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::post("/api/moxfield/authors")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"username": "guido"}).to_string()))
                .unwrap(),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_of(resp).await;
    assert_eq!(body["author"], json!("Guido"));
    assert_eq!(body["decks_on_moxfield"], json!(0));

    // GET status now lists the author with the decks fold.
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/moxfield").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    let body = body_of(resp).await;
    assert_eq!(body["authors"].as_array().unwrap().len(), 1);
    assert_eq!(body["authors"][0]["moxfield_user"], json!("Guido"));
    assert_eq!(body["authors"][0]["total_decks"], json!(0));
}

// ---------------------------------------------------------------------------
// TOTORO review fixes: scheduler robustness, author delete, in_flight, curl cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tick_without_db_is_a_noop() {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: "http://127.0.0.1:1".to_string(),
        static_dir: None,
        rust_routes: Default::default(),
        db_path: None,
        sidecar_url: None,
    };
    let st = AppState::new(cfg);
    assert!(st.db.is_none());
    moxfield::tick(&st).await; // pre-fix: None.unwrap() panic
    moxfield::spawn_scheduler(&st); // must not start anything
}

#[test]
fn in_flight_guard_clears_on_panic() {
    let r = std::panic::catch_unwind(|| {
        let _g = moxfield::try_in_flight(77001).unwrap();
        assert!(moxfield::try_in_flight(77001).is_none());
        panic!("boom");
    });
    assert!(r.is_err());
    assert!(moxfield::try_in_flight(77001).is_some(), "author stuck in flight after panic");
}

#[tokio::test]
async fn remove_author_deletes_tracked_rows_without_fk_cascade() {
    let fx = seeded_db("rmauthor_nofk.db");
    let (maddr, _mock) = spawn_mfx_mock().await;
    let st = engine_state(&fx, mfx_client(maddr), None);
    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (3, 1, 'NoFk')", []).unwrap();
    conn.execute(
        "INSERT INTO moxfield_decks (author_id, public_id, name, enabled) VALUES (3,'n1','N',0)",
        [],
    )
    .unwrap();
    drop(conn);
    // Pin the pooled connection's FK state OFF (Node-unlike Rust pool never
    // promises it ON): the delete must not depend on ON DELETE CASCADE.
    {
        let c = st.db.as_ref().unwrap().get().unwrap();
        c.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    }
    remove_author(&st, 1, Some(3)).await.unwrap();
    let conn = conn_of(&fx);
    assert_eq!(rows_len(&conn, "SELECT 1 FROM moxfield_decks WHERE author_id=3", &[]), 0);
}

#[tokio::test]
async fn manual_sync_waits_for_scheduler_in_flight() {
    let fx = seeded_db("manual_wait.db");
    let (maddr, mock) = spawn_mfx_mock().await;
    set_user(&mock, json!([{"userName": "Waiter"}]));
    set_pages(&mock, vec![json!({"data": [], "totalPages": 1})]);
    let conn = conn_of(&fx);
    conn.execute("INSERT INTO moxfield_authors (id, user_id, moxfield_user) VALUES (77002, 1, 'Waiter')", []).unwrap();
    drop(conn);
    let router = router_for(&fx, "http://127.0.0.1:1", mfx_client(maddr)).await;

    let guard = moxfield::try_in_flight(77002).unwrap();
    let req = with_auth(
        Request::post("/api/moxfield/authors/77002/sync-decklist").body(Body::empty()).unwrap(),
        "tok-session",
    );
    let handle = tokio::spawn(async move { router.oneshot(req).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(!handle.is_finished(), "manual sync ran while scheduler had the author in flight");
    assert!(mock.hits_for("GET /v2/").is_empty());
    drop(guard);
    let resp = tokio::time::timeout(std::time::Duration::from_secs(20), handle).await.unwrap().unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // the route released its own guard
    assert!(moxfield::try_in_flight(77002).is_some());
}

#[tokio::test]
async fn curl_output_over_max_buffer_kills_child_early() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fake-curl");
    std::fs::write(&bin, "#!/bin/sh\nhead -c 11000000 /dev/zero\nexec sleep 60\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let api = MfxApi {
        base_url: "http://127.0.0.1:1".into(),
        curl_bin: bin.to_string_lossy().into(),
        // The maxBuffer kill must fire long before the execFile wall.
        exec_timeout: std::time::Duration::from_secs(35),
    };
    let t0 = std::time::Instant::now();
    let err = api.get("/v2/x", &[]).await.unwrap_err();
    assert!(t0.elapsed() < std::time::Duration::from_secs(15), "took {:?}", t0.elapsed());
    assert!(format!("{err:?}").contains("maxBuffer"), "{err:?}");
}
