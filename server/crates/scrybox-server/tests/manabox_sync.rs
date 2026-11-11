//! Lane B unit 3b: the ManaBox sync port (utils/manaboxSync.js +
//! routes/manaboxSync.js) and the backup job (backup.js + the admin backup
//! routes). Pure folds are asserted directly; routes run through the real
//! axum app against a real SQLite copy; the bulk-fetch leg rides the shared
//! Scryfall mock. No live upstream.

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    response::Response,
    Router,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tower::ServiceExt;

use mock_scryfall::{raw_card, scry_for, spawn_mock};
use scrybox_server::{
    app,
    backup::{backup_dir, create_backup, keep_last, list_backups},
    manabox::{
        identity_mismatch, js_number_pub, parse_csv, parse_manabox_export, plan_collection,
        plan_lists, CardRow, CollRow, Expectation, ExistingList, OwnedRow,
    },
    scryfall::Scry,
    AppState, Config,
};

#[path = "mock_scryfall.rs"]
mod mock_scryfall;

const UUID_A: &str = "00000000-0000-4000-8000-00000000000a"; // cached Alpha Card
const UUID_B: &str = "00000000-0000-4000-8000-00000000000b"; // unknown to cache + scryfall
const UUID_C: &str = "00000000-0000-4000-8000-00000000000c"; // cached list card

const HEADER: &str = "Binder Name,Binder Type,Name,Set code,Collector number,Foil,Quantity,Scryfall ID,Condition,Language,Added";

fn binder_row(name: &str, binder: &str, set: &str, num: &str, foil: &str, qty: &str, sid: &str, cond: &str, lang: &str, added: &str) -> String {
    format!("{binder},binder,{name},{set},{num},{foil},{qty},{sid},{cond},{lang},{added}")
}

fn list_row(binder: &str, name: &str, set: &str, num: &str, qty: &str, sid: &str) -> String {
    format!("{binder},list,{name},{set},{num},normal,{qty},{sid},near_mint,en,2024-01-01T00:00:00.000Z")
}

fn owned_csv(rows: &[String]) -> String {
    format!("{HEADER}\n{}\n", rows.join("\n"))
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct Fixture {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

fn seed_card(conn: &Connection, id: &str, name: &str, set: &str, number: &str, printed: Option<&str>) {
    conn.execute(
        "INSERT OR REPLACE INTO card_cache (id, name, types, set_id, set_name, number, price_source, language, printed_name) VALUES (?, ?, 'Creature', ?, ?, ?, 'scryfall', 'English', ?)",
        rusqlite::params![id, name, set, format!("{set} Set"), number, printed],
    )
    .unwrap();
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
    conn.execute_batch(include_str!("manabox_schema.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, api_key, share_token) VALUES (1,'admin1','x','admin','tok-api','share-1');
         INSERT INTO users (id, username, password_hash, role, share_token) VALUES (2,'member1','x','member','share-2');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-session', 1, '2099-01-01 00:00:00');
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-member', 2, '2099-01-01 00:00:00');",
    )
    .unwrap();
    seed_card(&conn, &format!("mtg-{UUID_A}"), "Alpha Card", "xyz", "1", None);
    seed_card(&conn, &format!("mtg-{UUID_C}"), "Gamma Card", "xyz", "3", Some("Gamma Named"));
    Fixture { dir, path }
}

fn state_for(fx: &Fixture, scry: Option<Scry>) -> AppState {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: "http://127.0.0.1:1".to_string(),
        static_dir: None,
        rust_routes: ["manabox_backup".to_string()].into_iter().collect(),
        db_path: Some(fx.path.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    if let Some(s) = scry {
        st.scry = Arc::new(s);
    }
    st
}

fn with_auth(mut req: Request<Body>, token: &str) -> Request<Body> {
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    req
}

fn json_post(uri: &str, body: Value) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_of(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn body_text(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

fn one_row(conn: &Connection, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Option<HashMap<String, Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt
        .query_map(p, |row| {
            let mut m = HashMap::new();
            for (i, c) in cols.iter().enumerate() {
                let v: rusqlite::types::Value = row.get(i)?;
                m.insert(
                    c.clone(),
                    match v {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(i) => Value::from(i),
                        rusqlite::types::Value::Real(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
                        rusqlite::types::Value::Text(t) => Value::String(t),
                        rusqlite::types::Value::Blob(_) => Value::Null,
                    },
                );
            }
            Ok(m)
        })
        .unwrap()
        .next()
        .map(|r| r.unwrap());
    rows.take()
}

fn rows_of(conn: &Connection, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Vec<HashMap<String, Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    stmt.query_map(p, |row| {
        let mut m = HashMap::new();
        for (i, c) in cols.iter().enumerate() {
            let v: rusqlite::types::Value = row.get(i)?;
            m.insert(
                c.clone(),
                match v {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(i) => Value::from(i),
                    rusqlite::types::Value::Real(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
                    rusqlite::types::Value::Text(t) => Value::String(t),
                    rusqlite::types::Value::Blob(_) => Value::Null,
                },
            );
        }
        Ok(m)
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

// ---------------------------------------------------------------------------
// Pure folds — CSV + parseManaboxExport
// ---------------------------------------------------------------------------

#[test]
fn csv_parser_rfc4180_quirks() {
    // Quoted commas and doubled quotes.
    let rows = parse_csv("a,b\n\"x,y\",\"he said \"\"hi\"\"\"\n");
    assert_eq!(rows[0].get("a").unwrap(), "x,y");
    assert_eq!(rows[0].get("b").unwrap(), "he said \"hi\"");
    // CRLF and lone CR both break lines; quoted newline does not.
    let rows = parse_csv("a,b\r\n1,2\r3,4\n\"5\n6\",7\n");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get("a").unwrap(), "1");
    assert_eq!(rows[1].get("a").unwrap(), "3");
    assert_eq!(rows[2].get("a").unwrap(), "5\n6");
    // Blank lines are skipped (single empty field), trailing comma kept.
    let rows = parse_csv("a,b\n1,2\n\n3,\n");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].get("b").unwrap(), "");
    // BOM stripped, header trimmed.
    let rows = parse_csv("\u{FEFF} a ,b\n1,2\n");
    assert_eq!(rows[0].get("a").unwrap(), "1");
    assert_eq!(rows[0].get("b").unwrap(), "2");
    // Missing trailing cells default to ''.
    let rows = parse_csv("a,b,c\n1\n");
    assert_eq!(rows[0].get("c").unwrap(), "");
    // A bare quote outside quotes opens quoted mode anywhere on the field.
    let rows = parse_csv("a\nab\"c,d\"\n");
    assert_eq!(rows[0].get("a").unwrap(), "abc,d");
}

fn full_header(extra: &str) -> String {
    if extra.is_empty() {
        HEADER.to_string()
    } else {
        format!("{HEADER},{extra}")
    }
}

fn good_row(qty: &str, added: &str) -> String {
    binder_row("Alpha Card", "My Binder", "xyz", "1", "normal", qty, UUID_A, "near_mint", "en", added)
}

#[test]
fn parse_export_errors_and_maps() {
    // Empty input and header-only input both report 'The file is empty.'.
    assert_eq!(parse_manabox_export("").unwrap_err(), "The file is empty.");
    assert_eq!(parse_manabox_export(&format!("{HEADER}\n")).unwrap_err(), "The file is empty.");
    // Missing required columns enumerate in REQUIRED order.
    let err = parse_manabox_export("Name,Quantity\nx,2\n").unwrap_err();
    assert_eq!(err, "This is not a ManaBox collection export (missing Binder Name, Binder Type, Set code, Collector number, Foil, Scryfall ID, Condition, Language, Added).");

    let p = parse_manabox_export(&owned_csv(&[good_row("2.9", "2024-01-02T03:04:05.000Z")])).unwrap();
    assert_eq!(p.rows, 1);
    assert_eq!(p.owned.len(), 1);
    // parseInt('2.9', 10) === 2 — quantity truncates.
    assert_eq!(p.owned[0].quantity, 2);
    assert_eq!(p.owned[0].printing, "Normal");
    assert_eq!(p.owned[0].condition, "Near Mint");
    assert_eq!(p.owned[0].language, "English");
    assert_eq!(p.owned[0].dates, vec![Some("2024-01-02 03:04:05".to_string()); 2]);
    assert_eq!(p.owned[0].card_id, format!("mtg-{UUID_A}"));
    assert_eq!(p.owned[0].key, format!("mtg-{UUID_A}|Normal|Near Mint|English"));

    // bad quantity arms: 0 and NaN
    let p = parse_manabox_export(&owned_csv(&[good_row("0", ""), good_row("abc", "")])).unwrap();
    assert_eq!(p.errors.len(), 2);
    assert_eq!(p.errors[0].reason, "bad quantity");
    assert_eq!(p.errors[0].line, Some(2));
    assert_eq!(p.errors[1].reason, "bad quantity");
    // bad Scryfall ID arms (checked BEFORE quantity): non-hex, wrong length.
    let p = parse_manabox_export(&owned_csv(&[
        binder_row("X", "B", "xyz", "1", "normal", "1", "ZZ000000-0000-4000-8000-00000000000a", "near_mint", "en", ""),
    ]))
    .unwrap();
    assert_eq!(p.errors[0].reason, "missing Scryfall ID");
    // unknown foil / condition / language arms (first-missing wins).
    let p = parse_manabox_export(&owned_csv(&[binder_row("X", "B", "xyz", "1", "metal", "1", UUID_A, "near_mint", "en", "")])).unwrap();
    assert_eq!(p.errors[0].reason, "unknown foil \"metal\"");
    let p = parse_manabox_export(&owned_csv(&[binder_row("X", "B", "xyz", "1", "foil", "1", UUID_A, "minty", "en", "")])).unwrap();
    assert_eq!(p.errors[0].reason, "unknown condition \"minty\"");
    let p = parse_manabox_export(&owned_csv(&[binder_row("X", "B", "xyz", "1", "etched", "1", UUID_A, "poor", "xx", "")])).unwrap();
    assert_eq!(p.errors[0].reason, "unknown language \"xx\"");
    // foil etched -> Holofoil, condition poor -> Damaged, mint -> Near Mint.
    let p = parse_manabox_export(&owned_csv(&[binder_row("X", "B", "xyz", "1", "etched", "1", UUID_A, "mint", "zhs", "")])).unwrap();
    assert_eq!((p.owned[0].printing.as_str(), p.owned[0].condition.as_str(), p.owned[0].language.as_str()), ("Holofoil", "Near Mint", "Chinese Simplified"));

    // Aggregate duplicates; dates sorted; purchase price = first parseFloat>0.
    let mut rows = vec![
        format!("B,binder,Alpha Card,xyz,1,normal,2,{UUID_A},near_mint,en,2024-05-01T00:00:00.000Z"),
        format!("B,binder,Alpha Card,xyz,1,normal,1,{UUID_A},near_mint,en,2024-01-01T00:00:00.000Z"),
    ];
    let csv = format!("{},Purchase price\n{}\n", HEADER, rows.join("\n") + ",abc\n" + &format!("B,binder,Alpha Card,xyz,1,normal,1,{UUID_A},near_mint,en,2024-02-01T00:00:00.000Z,3.5"));
    let p = parse_manabox_export(&csv).unwrap();
    assert_eq!(p.owned.len(), 1);
    assert_eq!(p.owned[0].quantity, 4);
    assert_eq!(
        p.owned[0].dates,
        vec![
            Some("2024-01-01 00:00:00".to_string()),
            Some("2024-02-01 00:00:00".to_string()),
            Some("2024-05-01 00:00:00".to_string()),
            Some("2024-05-01 00:00:00".to_string()),
        ]
    );
    assert_eq!(p.owned[0].purchase_price, Some(3.5));
    let _ = &mut rows;

    // List rows aggregate by (name, cardId); empty Binder Name defaults.
    let csv = owned_csv(&[
        list_row("Gems", "Gamma Card", "xyz", "3", "2", UUID_C),
        list_row("Gems", "Gamma Card", "xyz", "3", "1", UUID_C),
        list_row("", "Alpha Card", "xyz", "1", "1", UUID_A),
    ]);
    let p = parse_manabox_export(&csv).unwrap();
    assert_eq!(p.lists.len(), 2);
    assert_eq!(p.lists[0].0, "Gems");
    assert_eq!(p.lists[0].1.len(), 1);
    assert_eq!(p.lists[0].1[0].1.quantity, 3);
    assert_eq!(p.lists[1].0, "ManaBox list");

    // 'Added' that does not match the ISO shape stores a null date.
    let p = parse_manabox_export(&owned_csv(&[good_row("1", "not-a-date")])).unwrap();
    assert_eq!(p.owned[0].dates, vec![None]);
}

// ---------------------------------------------------------------------------
// identityMismatch
// ---------------------------------------------------------------------------

fn exp(set: &str, number: &str, name: &str) -> Expectation {
    Expectation { set: set.into(), number: number.into(), name: name.into() }
}

fn card(name: Option<&str>, printed: Option<&str>, set: Option<&str>, number: Option<&str>) -> CardRow {
    CardRow { id: "mtg-x".into(), name: name.map(Into::into), printed_name: printed.map(Into::into), set_id: set.map(Into::into), number: number.map(Into::into) }
}

#[test]
fn identity_mismatch_arms() {
    assert_eq!(identity_mismatch(&exp("xyz", "1", "Alpha Card"), None).unwrap(), "Scryfall has no card with this ID");
    assert!(identity_mismatch(&exp("xyz", "1", "Alpha Card"), Some(&card(Some("Alpha Card"), None, Some("xyz"), Some("1")))).is_none());
    // Set compared case-insensitively, reported uppercased; mtg- stripped.
    assert_eq!(identity_mismatch(&exp("abc", "1", "Alpha Card"), Some(&card(Some("Alpha Card"), None, Some("xyz"), Some("1")))).unwrap(), "set ABC vs XYZ");
    assert!(identity_mismatch(&exp("xyz", "1", "X"), Some(&card(Some("X"), None, Some("mtg-xyz"), Some("1")))).is_none());
    assert_eq!(identity_mismatch(&exp("xyz", "1", "X"), Some(&card(Some("X"), None, Some(""), Some("1")))).unwrap(), "set XYZ vs ");
    // Collector number strict string compare.
    assert_eq!(identity_mismatch(&exp("xyz", "01", "X"), Some(&card(Some("X"), None, Some("xyz"), Some("1")))).unwrap(), "collector number 01 vs 1");
    // Name: face before ' // ', then printed_name fallback, report quotes card.name.
    assert!(identity_mismatch(&exp("xyz", "1", "Gamma Named"), Some(&card(Some("Whatever"), Some("Gamma Named"), Some("xyz"), Some("1")))).is_none());
    assert!(identity_mismatch(&exp("xyz", "1", "Front"), Some(&card(Some("Front // Back"), None, Some("xyz"), Some("1")))).is_none());
    assert_eq!(
        identity_mismatch(&exp("xyz", "1", "Nope"), Some(&card(Some("Other"), None, Some("xyz"), Some("1")))).unwrap(),
        "name \"Nope\" vs \"Other\""
    );
    // Empty expectations skip their checks.
    assert!(identity_mismatch(&exp("", "", ""), Some(&card(Some("Anything"), None, Some("qrs"), Some("9")))).is_none());
}

// ---------------------------------------------------------------------------
// planCollection / planLists
// ---------------------------------------------------------------------------

fn owned_row(card_id: &str, qty: i64, dates: Vec<Option<String>>) -> OwnedRow {
    OwnedRow {
        key: format!("{card_id}|Normal|Near Mint|English"),
        card_id: card_id.into(),
        printing: "Normal".into(),
        condition: "Near Mint".into(),
        language: "English".into(),
        dates,
        quantity: qty,
        purchase_price: Some(1.5),
        expect: exp("xyz", "1", "Alpha Card"),
    }
}

fn coll_row(id: i64, card_id: &str, qty: i64, source: Option<&str>, added: Option<&str>) -> CollRow {
    CollRow {
        id,
        card_id: card_id.into(),
        quantity: qty,
        printing: Some("Normal".into()),
        condition: Some("Near Mint".into()),
        language: Some("English".into()),
        added_at: added.map(Into::into),
        source: source.map(Into::into),
    }
}

#[test]
fn plan_collection_fold() {
    let a = format!("mtg-{UUID_A}");
    // 2 manabox copies (ids 5 and 3 — must be kept oldest-id-first) + 1 hand row.
    let rows = vec![
        coll_row(5, &a, 1, Some("manabox"), Some("2023-01-01 00:00:00")),
        coll_row(3, &a, 1, Some("manabox"), Some("2022-01-01 00:00:00")),
        coll_row(9, &a, 1, Some("manual"), Some("2021-01-01 00:00:00")),
    ];
    // owned 2: keep both (ids 3 then 5), redate both from the CSV dates.
    let plan = plan_collection(
        &rows,
        &[owned_row(&a, 2, vec![Some("2024-01-01 00:00:00".into()), Some("2024-02-01 00:00:00".into())])],
    );
    assert!(plan.remove.is_empty());
    assert_eq!(plan.redate, vec![(3, "2024-01-01 00:00:00".to_string()), (5, "2024-02-01 00:00:00".to_string())]);
    assert!(plan.add.is_empty());

    // owned 1: keep id 3, remove id 5 (kept=3 total-1 removed >= ownedQty=1 -> no add).
    let plan = plan_collection(&rows, &[owned_row(&a, 1, vec![Some("2024-01-01 00:00:00".into())])]);
    assert_eq!(plan.remove.iter().map(|r| r.id).collect::<Vec<_>>(), vec![5]);
    assert!(plan.add.is_empty());
    assert_eq!(plan.redate, vec![(3, "2024-01-01 00:00:00".to_string())]);

    // owned 4: keep 2 manabox, 1 hand counts towards kept -> 1 add, dates
    // exhausted -> added_at null. purchase_price carried from the owned row.
    let plan = plan_collection(
        &rows,
        &[owned_row(&a, 4, vec![Some("2024-01-01 00:00:00".into()), Some("2024-02-01 00:00:00".into())])],
    );
    assert!(plan.remove.is_empty());
    assert_eq!(plan.add.len(), 1);
    assert_eq!(plan.add[0].card_id, a);
    assert_eq!(plan.add[0].added_at, None);
    assert_eq!(plan.add[0].purchase_price, Some(1.5));

    // owned 2 with no DB rows: kept=0 -> two adds consuming both dates.
    let plan = plan_collection(
        &[],
        &[owned_row(&a, 2, vec![Some("2024-01-01 00:00:00".into()), Some("2024-02-01 00:00:00".into())])],
    );
    assert_eq!(plan.add.iter().map(|a| a.added_at.as_deref()).collect::<Vec<_>>(), vec![Some("2024-01-01 00:00:00"), Some("2024-02-01 00:00:00")]);

    // Redate only when the date differs from the row's added_at.
    let rows = vec![coll_row(1, &a, 1, Some("manabox"), Some("2024-01-01 00:00:00"))];
    let plan = plan_collection(&rows, &[owned_row(&a, 1, vec![Some("2024-01-01 00:00:00".into())])]);
    assert!(plan.redate.is_empty());
    // Null DB added_at still redate-pairs with a CSV date.
    let rows = vec![coll_row(2, &a, 1, Some("manabox"), None)];
    let plan = plan_collection(&rows, &[owned_row(&a, 1, vec![Some("2024-03-03 00:00:00".into())])]);
    assert_eq!(plan.redate, vec![(2, "2024-03-03 00:00:00".to_string())]);
}

#[test]
fn plan_lists_arms() {
    use scrybox_server::manabox::ListEntry;
    let e = |id: &str, q: i64| (id.to_string(), ListEntry { card_id: id.into(), quantity: q, expect: exp("xyz", "1", "X") });
    let existing = vec![
        ExistingList { id: 1, name: "Gems".into(), source: Some("manabox".into()), cards: vec![(format!("mtg-{UUID_C}"), 3)] },
        ExistingList { id: 2, name: "Stash".into(), source: None, cards: vec![] },
    ];
    let csv = vec![
        ("Gems".to_string(), vec![e(&format!("mtg-{UUID_C}"), 3)]),
        ("Stash".to_string(), vec![e(&format!("mtg-{UUID_A}"), 1)]),
        ("New".to_string(), vec![e(&format!("mtg-{UUID_A}"), 1)]),
    ];
    let plan = plan_lists(&existing, &csv);
    assert_eq!(plan.create.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), vec!["New"]);
    assert_eq!(plan.skipped, vec!["Stash".to_string()]);
    assert!(plan.update.is_empty(), "identical content must not update");

    let csv = vec![("Gems".to_string(), vec![e(&format!("mtg-{UUID_C}"), 2)])];
    let plan = plan_lists(&existing, &csv);
    assert_eq!(plan.update.len(), 1);
    assert_eq!(plan.update[0].0, 1);
    let csv = vec![("Gems".to_string(), vec![e(&format!("mtg-{UUID_C}"), 3), e(&format!("mtg-{UUID_A}"), 1)])];
    let plan = plan_lists(&existing, &csv);
    assert_eq!(plan.update.len(), 1, "size mismatch updates");
}

#[test]
fn js_number_semantics() {
    assert_eq!(js_number_pub(&json!(5)), Some(5.0));
    assert_eq!(js_number_pub(&json!("5")), Some(5.0));
    assert_eq!(js_number_pub(&json!(" 12 ")), Some(12.0));
    assert_eq!(js_number_pub(&json!("")), Some(0.0));
    assert_eq!(js_number_pub(&json!("12abc")), None, "Number() is strict, not parseFloat");
    assert_eq!(js_number_pub(&json!(true)), Some(1.0));
    assert_eq!(js_number_pub(&json!([5])), Some(5.0));
    assert_eq!(js_number_pub(&json!([])), Some(0.0));
    assert_eq!(js_number_pub(&json!([1, 2])), None);
    assert_eq!(js_number_pub(&json!({"a":1})), None);
}

// ---------------------------------------------------------------------------
// DDL identity vs db.js
// ---------------------------------------------------------------------------

#[test]
fn db_js_ddl_matches_fixture() {
    let db_js = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../backend/src/db.js")).unwrap();
    let extract = |needle: &str| -> String {
        let start = db_js.find(needle).unwrap_or_else(|| panic!("missing {needle} in db.js"));
        let rest = &db_js[start..];
        let end = rest.find("`);").unwrap();
        rest[..end].to_string()
    };
    let live = Connection::open_in_memory().unwrap();
    live.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY);").unwrap();
    live.execute_batch(&extract("CREATE TABLE IF NOT EXISTS card_lists (")).unwrap();
    // The unit-relevant ALTER from db.js's migration block.
    live.execute_batch("ALTER TABLE card_lists ADD COLUMN source TEXT").unwrap();
    live.execute_batch(&extract("CREATE TABLE IF NOT EXISTS list_cards (")).unwrap();
    live.execute_batch(&extract("CREATE TABLE IF NOT EXISTS manabox_sync_runs (")).unwrap();

    let mine = Connection::open_in_memory().unwrap();
    mine.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY);").unwrap();
    mine.execute_batch(include_str!("manabox_schema.sql")).unwrap();

    for table in ["card_lists", "list_cards", "manabox_sync_runs"] {
        let cols = |conn: &Connection| -> Vec<(String, String, bool, Option<String>, i64)> {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
            stmt.query_map([], |r| {
                Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)? != 0, r.get::<_, Option<String>>(4)?, r.get::<_, i64>(5)?))
            })
            .unwrap()
            .map(|x| x.unwrap())
            .collect()
        };
        assert_eq!(cols(&live), cols(&mine), "PRAGMA table_info diverged for {table}");
    }
}

// ---------------------------------------------------------------------------
// Routes — auth ladder + preview
// ---------------------------------------------------------------------------

async fn preview_router(fx: &Fixture, mock: Option<(SocketAddr, Arc<mock_scryfall::MockState>)>) -> Router {
    let scry = mock.as_ref().map(|(addr, _)| scry_for(*addr, 0.0));
    app(state_for(fx, scry))
}

#[tokio::test]
async fn preview_auth_ladder_and_errors() {
    let fx = seeded_db("preview_auth.db");
    let router = preview_router(&fx, None).await;

    // No token -> 401 from the shared gate (before body parse).
    let resp = router.clone().oneshot(json_post("/api/manabox-sync/preview", json!({"csv": "x"}))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_of(resp).await["error"], json!("Access denied. No token provided."));

    // API key on POST -> 403 read-only (gate runs before the 40mb parser).
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/preview", json!({"csv": "x"})), "tok-api"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_of(resp).await["error"], json!("API keys are read-only. Use a login session for this request."));

    // Member session -> ALLOWED (no requireAdmin on this router): parses the
    // body as the member and plans against the member's own collection.
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/preview", json!({"csv": "x"})), "tok-member"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_of(resp).await["error"], json!("The file is empty."));

    // GET runs with an API key -> plain authenticateToken lets GETs through.
    let resp = router.clone().oneshot(with_auth(Request::get("/api/manabox-sync/runs").body(Body::empty()).unwrap(), "tok-api")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_of(resp).await, json!([]));

    // csv missing -> String(undefined) one-cell CSV -> 400 'The file is empty.'.
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/preview", json!({})), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_of(resp).await["error"], json!("The file is empty."));

    // Wrong content type -> req.body undefined -> same 400 (body not parsed).
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::post("/api/manabox-sync/preview")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("csv".repeat(5)))
                .unwrap(),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_of(resp).await["error"], json!("The file is empty."));

    // Malformed JSON body -> global error handler's 500.
    let resp = router
        .clone()
        .oneshot(with_auth(
            Request::post("/api/manabox-sync/preview")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{oops"))
                .unwrap(),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_of(resp).await["error"], json!("Internal server error"));

    // GET runs is native (not proxied): no x-node marker, 200 empty array.
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/manabox-sync/runs").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-node").is_none());
    assert_eq!(body_of(resp).await, json!([]));
}

#[tokio::test]
async fn preview_plan_and_report() {
    let fx = seeded_db("preview_plan.db");
    let (addr, mock) = spawn_mock().await;
    mock.collection_batches.lock().unwrap().push(json!({"data": [raw_card(UUID_B, "Beta Card", "xyz", "2", Some(2.5))], "not_found": []}));
    let router = preview_router(&fx, Some((addr, mock.clone()))).await;

    // 2 owned copies of Alpha (cached), 1 list copy of Gamma (cached, printed
    // name accepted), 1 Beta row resolved through the mock (cache write!),
    // 1 unknown-id row rejected, 1 bad-quantity row rejected.
    let csv = owned_csv(&[
        binder_row("Alpha Card", "Bind", "xyz", "1", "foil", "2", UUID_A, "near_mint", "en", "2024-01-02T03:04:05.000Z"),
        list_row("Gems", "Gamma Named", "xyz", "3", "1", UUID_C),
        binder_row("Beta Card", "Bind", "xyz", "2", "normal", "1", UUID_B, "near_mint", "en", "2024-03-01T00:00:00.000Z"),
        binder_row("Ghost", "Bind", "xyz", "9", "normal", "1", "00000000-0000-4000-8000-00000000dead", "near_mint", "en", ""),
        good_row("abc", ""),
    ]);
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/preview", json!({"csv": csv, "fileName": "manabox.csv"})), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_of(resp).await;
    // Top-level key order and summary key order.
    assert_eq!(body.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["summary", "report"]);
    let s = body["summary"].as_object().unwrap();
    assert_eq!(
        s.keys().collect::<Vec<_>>(),
        vec!["csvRows", "owned", "before", "after", "add", "remove", "redate", "handAdded", "listsCreate", "listsUpdate", "listsSkipped", "rejected"]
    );
    assert_eq!(s["csvRows"], json!(5));
    // The dead-id row: not_found from the mock -> identity reject.
    assert_eq!(s["owned"], json!(3), "2 alpha + 1 beta; gamma is list-only");
    assert_eq!(s["before"], json!(0));
    assert_eq!(s["after"], json!(3));
    assert_eq!(s["add"], json!(3), "one add row per missing copy");
    assert_eq!(s["remove"], json!(0));
    assert_eq!(s["redate"], json!(0));
    assert_eq!(s["handAdded"], json!(0));
    assert_eq!(s["listsCreate"], json!(1));
    assert_eq!(s["listsUpdate"], json!(0), "create and update are separate buckets (Node planLists)");
    assert_eq!(s["listsSkipped"], json!([]));
    assert_eq!(s["rejected"], json!(2));

    // Bulk fetch ran through the paced client with object identifiers:
    // deduped missing cache ids in parse order (owned before list entries).
    let hits = mock.hits.lock().unwrap().clone();
    assert_eq!(hits.iter().filter(|(k, _)| k.starts_with("POST /cards/collection")).count(), 1);
    let req_body: Value =
        serde_json::from_str(&hits.iter().find(|(k, _)| k.starts_with("POST /cards/collection")).unwrap().1).unwrap();
    assert_eq!(
        req_body["identifiers"],
        json!([{"id": UUID_B}, {"id": "00000000-0000-4000-8000-00000000dead"}]),
        "identifiers ride as objects, uuid-only — the port must not fall back to the string form"
    );

    // Beta got cached with its Scryfall price data (price_source scryfall).
    let conn = Connection::open(&fx.path).unwrap();
    let row = one_row(
        &conn,
        "SELECT id, name, set_id, number, price_source, price_normal FROM card_cache WHERE id = ?",
        &[&format!("mtg-{UUID_B}")],
    )
    .expect("Beta cached");
    assert_eq!(row["name"], json!("Beta Card"));
    assert_eq!(row["price_source"], json!("scryfall"));
    // Preview writes NOTHING to collection/runs.
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM collection", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM manabox_sync_runs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);

    // Report: exact markdown modulo the timestamp line.
    let md = body["report"].as_str().unwrap();
    let lines: Vec<&str> = md.split('\n').collect();
    assert!(lines[0].starts_with("# ManaBox sync ") && lines[0].ends_with(" UTC (PREVIEW - nothing changed)"), "{}", lines[0]);
    let tail = [
        "",
        "- File: manabox.csv (5 rows, 3 owned copies)",
        "- Collection: 0 -> 3 copies",
        "- Added: **3**, removed: **0**, dates set from ManaBox: 0",
        "- Kept untouched: 0 rows added in Scrybox itself",
        "- Lists created: 1, updated: 0",
        "- Rejected rows (set/number/name did not match the Scryfall ID, or unreadable): 2",
        "",
        "## Removed",
        "",
        "Row | Card | Set # | Printing | Condition | Language | Added",
        "---|---|---|---|---|---|---",
        "",
        "## Added",
        "",
        "Card | Set # | Printing | Condition | Language | ManaBox added",
        "---|---|---|---|---|---",
        "Alpha Card | XYZ 1 | Holofoil | Near Mint | English | 2024-01-02",
        "Alpha Card | XYZ 1 | Holofoil | Near Mint | English | 2024-01-02",
        "Beta Card | XYZ 2 | Normal | Near Mint | English | 2024-03-01",
        "",
        "## Lists",
        "",
        "- Created **Gems** (1 cards)",
        "",
        "## Rejected",
        "",
        "- line 6: Alpha Card: bad quantity",
        "- Ghost: Scryfall has no card with this ID",
        "",
    ];
    assert_eq!(&lines[1..], &tail[..], "report body must match the Node template verbatim\n{md}");
}

#[tokio::test]
async fn preview_identity_and_list_rejects() {
    let fx = seeded_db("preview_reject.db");
    let (addr, _mock) = spawn_mock().await;
    let router = preview_router(&fx, Some((addr, _mock.clone()))).await;
    // Wrong set + wrong number for a cached card; list entry with wrong name.
    let csv = owned_csv(&[
        binder_row("Alpha Card", "Bind", "WRONGSET", "1", "normal", "1", UUID_A, "near_mint", "en", ""),
        list_row("Gems", "Not Gamma", "xyz", "3", "1", UUID_C),
    ]);
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/preview", json!({"csv": csv})), "tok-session"))
        .await
        .unwrap();
    let body = body_of(resp).await;
    assert_eq!(body["summary"]["rejected"], json!(2));
    assert_eq!(body["summary"]["owned"], json!(0));
    // Node deletes the rejected entry from the list's Map but the (now empty)
    // list still plans a CREATE — the port must create an EMPTY 'Gems'.
    assert_eq!(body["summary"]["listsCreate"], json!(1));
    let md = body["report"].as_str().unwrap();
    assert!(md.contains("- Created **Gems** (0 cards)"), "{md}");
    let md = body["report"].as_str().unwrap();
    assert!(md.contains("- Alpha Card: set WRONGSET vs XYZ"), "{md}");
    assert!(md.contains("- Not Gamma: name \"Not Gamma\" vs \"Gamma Card\" (list Gems)"), "{md}");
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn seed_collection_row(fx: &Fixture, user: i64, card_id: &str, qty: i64, source: &str, added: &str, printing: &str, condition: &str, language: &str) -> i64 {
    let conn = Connection::open(&fx.path).unwrap();
    conn.execute(
        "INSERT INTO collection (user_id, card_id, quantity, printing, condition, language, added_at, source) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![user, card_id, qty, printing, condition, language, added, source],
    )
    .unwrap();
    conn.last_insert_rowid()
}

#[tokio::test]
async fn apply_writes_everything() {
    let fx = seeded_db("apply.db");
    let (addr, _mock) = spawn_mock().await;
    let router = preview_router(&fx, Some((addr, _mock.clone()))).await;
    let a = format!("mtg-{UUID_A}");
    let c = format!("mtg-{UUID_C}");
    // Pre-existing: one manabox row that survives (will redate), one manabox
    // row that must be removed, one hand row that must survive untouched, and
    // one row belonging to another user.
    let keep = seed_collection_row(&fx, 1, &a, 1, "manabox", "2020-01-01 00:00:00", "Normal", "Near Mint", "English");
    let gone = seed_collection_row(&fx, 1, &c, 2, "manabox", "2020-01-02 00:00:00", "Normal", "Near Mint", "English");
    let hand = seed_collection_row(&fx, 1, &a, 3, "manual", "2019-01-01 00:00:00", "Normal", "Near Mint", "English");
    let other = seed_collection_row(&fx, 2, &a, 1, "manabox", "2019-01-01 00:00:00", "Normal", "Near Mint", "English");
    // Lists: 'Gems' (manabox, must update), 'Stash' (manual, must skip).
    {
        let conn = Connection::open(&fx.path).unwrap();
        conn.execute("INSERT INTO card_lists (id, user_id, name, source) VALUES (1, 1, 'Gems', 'manabox')", []).unwrap();
        conn.execute("INSERT INTO list_cards (list_id, card_id, quantity) VALUES (1, 'stale-card', 9)", []).unwrap();
        conn.execute("INSERT INTO card_lists (id, user_id, name) VALUES (2, 1, 'Stash')", []).unwrap();
        conn.execute("INSERT INTO list_cards (list_id, card_id, quantity) VALUES (2, 'keep-me', 5)", []).unwrap();
    }
    // CSV: keep Alpha x2 (1 manabox kept + 1 hand counts... owned 2, total
    // after remove of Gamma = 1 manabox + 3 hand = 4 >= 2 -> add 0) — make
    // owned 3 to force exactly one add, with a purchase price and a date.
    let csv = owned_csv(&[
        binder_row("Alpha Card", "Bind", "xyz", "1", "normal", "3", UUID_A, "near_mint", "en", "2024-06-07T08:09:10.000Z"),
        list_row("Gems", "Gamma Card", "xyz", "3", "2", UUID_C),
        list_row("Stash", "Alpha Card", "xyz", "1", "1", UUID_A),
        list_row("Fresh", "Gamma Card", "xyz", "3", "1", UUID_C),
    ]);

    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/apply", json!({"csv": csv})), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_of(resp).await;
    let s = body["summary"].as_object().unwrap();
    assert_eq!(s["csvRows"], json!(4));
    assert_eq!(s["owned"], json!(3));
    assert_eq!(s["before"], json!(6), "user-1 quantities only: 1+2+3; other user excluded");
    assert_eq!(s["remove"], json!(2), "the Gamma manabox row (qty 2)");
    assert_eq!(s["add"], json!(0), "owned 3 <= kept 1 manabox + 3 hand -> no add");
    assert_eq!(s["redate"], json!(1));
    assert_eq!(s["handAdded"], json!(1));
    assert_eq!(s["listsCreate"], json!(1));
    assert_eq!(s["listsUpdate"], json!(1), "Gems content differs");
    assert_eq!(s["listsSkipped"], json!(["Stash"]));
    assert_eq!(s["rejected"], json!(0));
    let after = s["after"].as_i64().unwrap();
    assert_eq!(after, 6 - 2 + 0);

    let conn = Connection::open(&fx.path).unwrap();
    // Removed / kept / hand / other rows.
    assert!(one_row(&conn, "SELECT id FROM collection WHERE id = ?", &[&gone]).is_none());
    assert!(one_row(&conn, "SELECT id FROM collection WHERE id = ?", &[&keep]).is_some());
    assert!(one_row(&conn, "SELECT id FROM collection WHERE id = ?", &[&hand]).is_some());
    assert!(one_row(&conn, "SELECT id FROM collection WHERE id = ?", &[&other]).is_some());
    // Kept row got the ManaBox date.
    let kept = one_row(&conn, "SELECT added_at FROM collection WHERE id = ?", &[&keep]).unwrap();
    assert_eq!(kept["added_at"], json!("2024-06-07 08:09:10"));
    // No adds this plan; nothing else touched.
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM collection WHERE user_id = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
    // Gems was replaced with the CSV contents; Stash untouched; Fresh created.
    let gems: Vec<_> = rows_of(&conn, "SELECT card_id, quantity FROM list_cards WHERE list_id = 1", &[]);
    assert_eq!(gems.len(), 1);
    assert_eq!(gems[0]["card_id"], json!(c));
    assert_eq!(gems[0]["quantity"], json!(2));
    let stash: Vec<_> = rows_of(&conn, "SELECT card_id FROM list_cards WHERE list_id = 2", &[]);
    assert_eq!(stash[0]["card_id"], json!("keep-me"));
    let fresh = one_row(&conn, "SELECT id, description, source FROM card_lists WHERE name = 'Fresh'", &[] as &[&dyn rusqlite::ToSql]).unwrap();
    assert_eq!(fresh["description"], json!("Synced from ManaBox"));
    assert_eq!(fresh["source"], json!("manabox"));
    let fresh_id = fresh["id"].as_i64().unwrap();
    let fresh_cards = rows_of(&conn, "SELECT card_id, quantity FROM list_cards WHERE list_id = ?", &[&fresh_id]);
    assert_eq!(fresh_cards[0]["quantity"], json!(1));

    // Run row stored verbatim from the response.
    let run = one_row(&conn, "SELECT summary, report, created_at FROM manabox_sync_runs WHERE user_id = 1", &[] as &[&dyn rusqlite::ToSql]).unwrap();
    assert_eq!(run["summary"], Value::String(serde_json::to_string(&body["summary"]).unwrap()), "stored summary is the response summary JSON");
    assert_eq!(run["report"], json!(body["report"].as_str().unwrap()));
    assert!(run["report"].as_str().unwrap().contains("APPLIED"));
    assert!(run["created_at"].as_str().unwrap().contains('-'));

    // Re-running the same file is a no-op.
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/apply", json!({"csv": csv})), "tok-session"))
        .await
        .unwrap();
    let body2 = body_of(resp).await;
    assert_eq!(body2["summary"]["add"], json!(0));
    assert_eq!(body2["summary"]["remove"], json!(0));
    assert_eq!(body2["summary"]["redate"], json!(0));
    assert_eq!(body2["summary"]["listsUpdate"], json!(0), "contents now match -> no update");
    assert_eq!(body2["summary"]["listsCreate"], json!(0));
}

#[tokio::test]
async fn apply_expect_before_and_include_lists() {
    let fx = seeded_db("apply_guards.db");
    let (addr, _mock) = spawn_mock().await;
    let router = preview_router(&fx, Some((addr, _mock.clone()))).await;
    let csv = owned_csv(&[binder_row("Alpha Card", "Bind", "xyz", "1", "normal", "1", UUID_A, "near_mint", "en", "2024-01-01T00:00:00.000Z")]);

    // expectBefore mismatch -> 409 before any write.
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/apply", json!({"csv": csv, "expectBefore": 999999})), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(body_of(resp).await["error"], json!("The collection changed since the preview. Preview again."));
    {
        let conn = Connection::open(&fx.path).unwrap();
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM collection", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }
    // expectBefore matches (string form uses Number() semantics).
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/manabox-sync/apply", json!({"csv": csv, "expectBefore": "0"})), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_of(resp).await["summary"]["add"], json!(1));

    // includeLists:false skips the list plan entirely.
    let fx2 = seeded_db("apply_nolists.db");
    let router2 = preview_router(&fx2, Some((addr, _mock.clone()))).await;
    let resp = router2
        .clone()
        .oneshot(with_auth(
            json_post(
                "/api/manabox-sync/apply",
                json!({"csv": csv, "includeLists": false}),
            ),
            "tok-session",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_of(resp).await;
    assert_eq!(body["summary"]["listsCreate"], json!(0));
    assert_eq!(body["summary"]["listsUpdate"], json!(0));
    let conn = Connection::open(&fx2.path).unwrap();
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM card_lists", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
}

#[tokio::test]
async fn runs_and_report_routes() {
    let fx = seeded_db("runs.db");
    let (addr, _mock) = spawn_mock().await;
    let router = preview_router(&fx, Some((addr, _mock.clone()))).await;
    let a = format!("mtg-{UUID_A}");
    {
        let conn = Connection::open(&fx.path).unwrap();
        for i in 0..25 {
            conn.execute(
                "INSERT INTO manabox_sync_runs (user_id, summary, report) VALUES (1, ?, ?)",
                rusqlite::params![format!("{{\"csvRows\":{i},\"x\":[]}}"), format!("report {i}")],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO manabox_sync_runs (user_id, summary, report) VALUES (2, '{\"a\":1}', 'mine')", []).unwrap();
    }
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/manabox-sync/runs").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    let body = body_of(resp).await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 20, "LIMIT 20");
    assert_eq!(arr[0]["id"], json!(25), "newest first");
    assert_eq!(arr[0]["summary"], json!({"csvRows": 24, "x": []}), "JSON.parse keeps key order");
    assert_eq!(arr[0]["created_at"].as_str().unwrap().len(), 19, "SQLite CURRENT_TIMESTAMP passthrough");
    assert!(!arr.iter().any(|r| r["created_at"].as_str().unwrap() == "mine"), "other user's runs excluded");

    // report 200: markdown type + attachment disposition + raw body.
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/manabox-sync/runs/23/report").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/markdown; charset=utf-8");
    assert_eq!(resp.headers()[header::CONTENT_DISPOSITION], "attachment; filename=\"manabox-sync-23.md\"");
    assert_eq!(body_text(resp).await, "report 22");

    // Other user's report -> 404 {error:'Not found'}; missing id -> same.
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/manabox-sync/runs/26/report").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_of(resp).await["error"], json!("Not found"));
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/manabox-sync/runs/abc/report").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // Member session -> allowed; member sees only ITS OWN runs (user 2's one row).
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/manabox-sync/runs").body(Body::empty()).unwrap(), "tok-member"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_of(resp).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], json!(26));
    let _ = a;
}

// ---------------------------------------------------------------------------
// Backup job + admin routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn backup_create_list_prune_and_vacuum_determinism() {
    let fx = seeded_db("backup.db");
    std::env::set_var("BACKUP_KEEP_LAST", "3");
    assert_eq!(keep_last(), 3);
    std::env::set_var("BACKUP_KEEP_LAST", "abc");
    assert_eq!(keep_last(), 10, "parseInt NaN || 10");
    std::env::set_var("BACKUP_KEEP_LAST", "0");
    assert_eq!(keep_last(), 10, "parseInt 0 is falsy || 10");
    std::env::set_var("BACKUP_KEEP_LAST", "2");

    let conn = Connection::open(&fx.path).unwrap();
    let mut names = Vec::new();
    for _ in 0..4 {
        let meta = create_backup(&fx.path, &conn).unwrap();
        assert!(meta.file.starts_with("backup."), "{}", meta.file);
        assert!(meta.file.ends_with(".bak"));
        // <base>.YYYY-MM-DDTHH-MM-SS-mmm.bak
        let rest = &meta.file["backup.".len()..meta.file.len() - 4];
        assert_eq!(rest.len(), 24, "{rest}");
        assert!(meta.size > 0);
        assert!(meta.created_at.ends_with("Z") && meta.created_at.contains('T'));
        names.push(meta.file.clone());
        // Different filenames need >1ms spacing; sleep to keep the test race-free.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let listing = list_backups(&backup_dir(&fx.path));
    assert_eq!(listing.len(), 2, "pruned to KEEP_LAST=2");
    // Newest first.
    assert!(listing[0].created_at >= listing[1].created_at);
    // Non-.bak files are invisible.
    std::fs::write(backup_dir(&fx.path).join("notes.txt"), b"x").unwrap();
    assert_eq!(list_backups(&backup_dir(&fx.path)).len(), 2);

    // VACUUM INTO determinism: two snapshots of a quiet DB are byte-equal
    // (the jobs-db-diff basis for the backup job).
    std::env::remove_var("BACKUP_KEEP_LAST");
    let a = create_backup(&fx.path, &conn).unwrap();
    let b = create_backup(&fx.path, &conn).unwrap();
    let ba = std::fs::read(backup_dir(&fx.path).join(&a.file)).unwrap();
    let bb = std::fs::read(backup_dir(&fx.path).join(&b.file)).unwrap();
    assert_eq!(ba, bb, "VACUUM INTO must be deterministic on a quiet DB");

    // Route half runs HERE (not as its own test) so no other test can read
    // BACKUP_KEEP_LAST while this one mutates it.
    backup_routes_body().await;
}

async fn backup_routes_body() {
    let fx = seeded_db("backup_routes.db");
    let router = preview_router(&fx, None).await;

    // Ladder first.
    let resp = router.clone().oneshot(Request::get("/api/admin/backups").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/admin/backups").body(Body::empty()).unwrap(), "tok-member"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/admin/backups").body(Body::empty()).unwrap(), "tok-api"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_of(resp).await["error"], json!("API keys cannot access admin endpoints. Use a login session."));

    // Empty dir -> {dir, backups: []}; dir points at <db dir>/backups.
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/admin/backups").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    let body = body_of(resp).await;
    assert_eq!(body["dir"], json!(backup_dir(&fx.path).to_string_lossy().to_string()));
    assert_eq!(body["backups"], json!([]));

    // POST creates -> 201 meta; listed afterwards; file exists.
    let resp = router
        .clone()
        .oneshot(with_auth(json_post("/api/admin/backups", json!({})), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let meta = body_of(resp).await;
    let file = meta["file"].as_str().unwrap().to_string();
    assert!(file.starts_with("backup_routes.") && file.ends_with(".bak"));
    assert!(meta["size"].as_u64().unwrap() > 0);
    assert!(backup_dir(&fx.path).join(&file).exists());

    let resp = router
        .clone()
        .oneshot(with_auth(Request::get("/api/admin/backups").body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    let body = body_of(resp).await;
    assert_eq!(body["backups"][0]["file"], json!(file));

    // Download: octet-stream + attachment + byte-identical to the file.
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get(&format!("/api/admin/backups/{file}/download")).body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(resp.headers()[header::CONTENT_DISPOSITION], format!("attachment; filename=\"{file}\""));
    let got = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    assert_eq!(got, std::fs::read(backup_dir(&fx.path).join(&file)).unwrap());

    // Non-.bak, traversal-stripped-to-.db and missing names all 404.
    std::fs::write(backup_dir(&fx.path).join("sneaky.db"), b"x").unwrap();
    for uri in [
        "/api/admin/backups/sneaky.db/download".to_string(),
        "/api/admin/backups/..%2Fsneaky.db/download".to_string(),
        "/api/admin/backups/nope.bak/download".to_string(),
    ] {
        let resp = router
            .clone()
            .oneshot(with_auth(Request::get(&uri).body(Body::empty()).unwrap(), "tok-session"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body_of(resp).await["error"], json!("Backup not found"));
    }
    // A traversal name that basename-strips to a real .bak still downloads
    // exactly like Node (path.basename strips the directory first).
    let resp = router
        .clone()
        .oneshot(with_auth(Request::get(&format!("/api/admin/backups/..%2F{file}/download")).body(Body::empty()).unwrap(), "tok-session"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // POST failure arm: unreachable -> 500 {error:'Backup failed', message}
    // is exercised through the pure fn instead (DB always available here).
    let bogus = std::path::PathBuf::from("/proc/definitely-not-writable/x.db");
    let conn = Connection::open(&fx.path).unwrap();
    let err = create_backup(&bogus, &conn).unwrap_err();
    assert!(!err.is_empty());
}
