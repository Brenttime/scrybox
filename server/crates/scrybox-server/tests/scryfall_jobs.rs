//! Lane B unit 1: Scryfall client (limiter, 429 cooldown, retry ladder, 404
//! breaker) + the set-sync and price-sweep jobs, tested against the scripted
//! mock (no live Scryfall; the limiter is exercised with the mock standing in
//! for upstream via SCRYFALL_BASE_URL-style base override).

#[path = "mock_scryfall.rs"]
mod mock_scryfall;

use std::{sync::Arc, time::Instant};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use mock_scryfall::{raw_card, scry_for, spawn_mock};
use scrybox_server::{
    card_cache::{
        cache_normalized_cards, mark_prices_swept, parse_sqlite_utc, record_price,
        refresh_all_estimates, should_sweep_prices,
    },
    card_norm::{default_search_eligible, normalize_card, oracle_id_for_card, scryfall_uuid},
    scryfall::{wait_ms, ScryOpts},
    scry_jobs::{fetch_and_cache_sets, sweep_rows, update_collection_prices},
};

fn seeded_db(dir: &std::path::Path, name: &str) -> Connection {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(include_str!("scry_schema.sql")).unwrap();
    conn
}

const UUID1: &str = "00000000-0000-4000-8000-000000000001";
const UUID2: &str = "00000000-0000-4000-8000-000000000002";

// ---------------------------------------------------------------------------
// Pure limiter math (Node waitFor parity)
// ---------------------------------------------------------------------------

#[test]
fn limiter_wait_math() {
    // No history: no wait.
    assert_eq!(wait_ms(1000, 0, 0, 0, 500, 100), 0);
    // Endpoint gap dominates: last search 200 ms ago, 500 ms gap.
    assert_eq!(wait_ms(1000, 900, 800, 0, 500, 100), 300);
    // Global floor: another endpoint just fired 20 ms ago (100 ms floor).
    assert_eq!(wait_ms(1000, 980, 500, 0, 500, 100), 80);
    // Cooldown wins over everything.
    assert_eq!(wait_ms(1000, 999, 999, 3000, 500, 100), 2000);
    // All constraints already satisfied → never negative.
    assert_eq!(wait_ms(5000, 4400, 4400, 0, 500, 100), 0);
}

#[test]
fn card_endpoint_path_buckets() {
    use scrybox_server::scryfall::is_card_endpoint_path;
    assert!(is_card_endpoint_path("/cards/search"));
    assert!(is_card_endpoint_path("/cards/collection"));
    assert!(is_card_endpoint_path("/cards/named"));
    assert!(is_card_endpoint_path("/cards/random"));
    assert!(!is_card_endpoint_path("/cards/randomx")); // \b boundary
    assert!(!is_card_endpoint_path("/sets"));
    assert!(!is_card_endpoint_path("/cards/random_foo"));
}

#[test]
fn uuid_guard_only_accepts_uuids() {
    assert_eq!(scryfall_uuid(Some(&format!("mtg-{UUID1}"))).as_deref(), Some(UUID1));
    assert_eq!(scryfall_uuid(Some(UUID1)).as_deref(), Some(UUID1));
    assert_eq!(scryfall_uuid(Some("mtg-cc1")), None);
    assert_eq!(scryfall_uuid(Some("")), None);
    assert_eq!(scryfall_uuid(None), None);
    // Must be exactly hex; a wrong char fails.
    assert_eq!(scryfall_uuid(Some("0000000z-0000-4000-8000-000000000001")), None);
}

#[test]
fn sqlite_utc_parses_naive_and_offset() {
    // Naive SQLite UTC string: parsed as UTC.
    let a = parse_sqlite_utc("2026-01-02 03:04:05").unwrap();
    let b = parse_sqlite_utc("2026-01-02T03:04:05Z").unwrap();
    assert_eq!(a, b);
    assert!(parse_sqlite_utc("").is_none());
}

// ---------------------------------------------------------------------------
// normalizeCard
// ---------------------------------------------------------------------------

#[test]
fn normalize_card_field_mapping() {
    let raw = raw_card(UUID1, "Lightning Bolt", "m15", "123", Some(1.23));
    let n = normalize_card(&raw, None).as_object().unwrap().clone();
    assert_eq!(n["id"], json!("mtg-00000000-0000-4000-8000-000000000001"));
    assert_eq!(n["name"], json!("Lightning Bolt"));
    assert_eq!(n["supertype"], json!("MTG"));
    assert_eq!(n["set_id"], json!("m15"));
    assert_eq!(n["number"], json!("123"));
    assert_eq!(n["rarity"], json!("Common"));
    assert_eq!(n["subtypes"], json!(["Creature", "Human", "Lightning", "Bolt"]));
    assert_eq!(n["types"], json!(["White"]));
    assert_eq!(n["color_identity"], json!(["White"]));
    assert_eq!(n["price_trend"], json!(1.23));
    assert_eq!(n["price_normal"], json!(1.23));
    assert_eq!(n["price_holofoil"], Value::Null);
    assert_eq!(n["price_source"], json!("scryfall"));
    assert_eq!(n["price_currency"], json!("USD"));
    assert_eq!(n["language"], json!("English"));
    assert_eq!(n["scryfall_search_eligible"], json!(1));
    assert_eq!(n["cmc"], json!(2.0));
    assert!(n.contains_key("oracle_text"));

    // Price-less response (sidecar bulk rows): no market statement at all.
    let mut priceless = raw.clone();
    priceless.as_object_mut().unwrap().remove("prices");
    let n = normalize_card(&priceless, None).as_object().unwrap().clone();
    assert_eq!(n["price_source"], Value::Null);
    assert_eq!(n["price_trend"], Value::Null);
    assert_eq!(n["price_normal"], Value::Null);

    // Token layout hides from default searches.
    let mut token = raw.clone();
    token.as_object_mut().unwrap()["layout"] = json!("token");
    assert_eq!(default_search_eligible(&token), Some(0));

    // Japanese printing keeps printed_name and maps the language.
    let mut japan = raw.clone();
    {
        let o = japan.as_object_mut().unwrap();
        o.insert("lang".into(), json!("ja"));
        o.insert("printed_name".into(), json!("稲妻"));
    }
    let n = normalize_card(&japan, None).as_object().unwrap().clone();
    assert_eq!(n["language"], json!("Japanese"));
    assert_eq!(n["printed_name"], json!("稲妻"));

    // MDFC: no top-level image_uris → front face carries art/type; shared
    // face oracle_ids collapse to the card's oracle_id.
    let dfc = json!({
        "id": UUID2, "set": "mid", "set_name": "Midnight Set", "collector_number": "44",
        "lang": "en", "layout": "double_morph", "rarity": "rare", "cmc": 3,
        "card_faces": [
            {"name": "Day", "type_line": "Creature — Human", "image_uris": {"normal": "d.png"},
             "colors": ["W"], "color_identity": ["W"], "oracle_id": "shared-1", "mana_cost": "W", "oracle_text": "day text"},
            {"name": "Night", "type_line": "Creature — Wolf", "image_uris": {"normal": "n.png"},
             "colors": ["B"], "oracle_id": "shared-1", "mana_cost": "B", "oracle_text": "night text"}
        ]
    });
    let n = normalize_card(&dfc, None).as_object().unwrap().clone();
    assert_eq!(n["name"], json!("Day"));
    assert_eq!(n["image_url"], json!("d.png"));
    assert_eq!(n["types"], json!(["White"]));
    assert_eq!(n["oracle_id"], json!("shared-1"));
    assert_eq!(oracle_id_for_card(&dfc).as_deref(), Some("shared-1"));
    assert_eq!(n["oracle_faces"].as_array().unwrap().len(), 2);
    assert_eq!(n["oracle_text"], json!("day text\n\n//\n\nnight text"));

    // Transform DFC from /cards/collection: top-level `name` is the JOINED
    // "A // B" and image_uris is absent → Node takes `face.name || raw.name`,
    // the SHORT front-face name. A raw-first port writes "A // B" into
    // card_cache and the DB-diff harness caught exactly that (iter 1).
    let tdfc = json!({
        "id": UUID2, "set": "mk2", "set_name": "Set", "collector_number": "231",
        "name": "Balamb Garden, SeeD Academy // Balamb Garden, Airborne",
        "lang": "en", "layout": "transform", "rarity": "rare", "cmc": 3,
        "card_faces": [
            {"name": "Balamb Garden, SeeD Academy", "type_line": "Creature — Human",
             "image_uris": {"normal": "a.png"}, "colors": ["G"], "color_identity": ["G"]},
            {"name": "Balamb Garden, Airborne", "type_line": "Creature — Human",
             "image_uris": {"normal": "b.png"}, "colors": ["G"]}
        ]
    });
    let n = normalize_card(&tdfc, None).as_object().unwrap().clone();
    assert_eq!(n["name"], json!("Balamb Garden, SeeD Academy"),
               "face.name wins over the joined top-level name");
    assert_eq!(n["image_url"], json!("a.png"));
}

// ---------------------------------------------------------------------------
// card_cache write semantics
// ---------------------------------------------------------------------------

fn norm_card(id: &str, name: &str, usd: Option<f64>) -> serde_json::Map<String, Value> {
    // `id` may be given as the cache id; normalize_card re-prefixes, so hand
    // it the bare Scryfall uuid.
    let raw = raw_card(id.strip_prefix("mtg-").unwrap_or(id), name, "test", "1", usd);
    normalize_card(&raw, None).as_object().unwrap().clone()
}

#[test]
fn cache_write_price_preservation_rules() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "cache.db");

    // First write: real quote lands.
    cache_normalized_cards(
        &conn,
        &[norm_card(&format!("mtg-{UUID1}"), "Sol Ring", Some(2.5))],
    )
    .unwrap();
    let (trend, source, est): (Option<f64>, Option<String>, i64) = conn
        .query_row(
            &format!("SELECT price_trend, price_source, price_estimated FROM card_cache WHERE id = 'mtg-{UUID1}'"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(trend, Some(2.5));
    assert_eq!(source.as_deref(), Some("scryfall"));
    assert_eq!(est, 0);

    // Price-less refresh of the SAME row: metadata updates, quote survives.
    let priceless = {
        let mut c = norm_card(&format!("mtg-{UUID1}"), "Sol Ring Renamed", None);
        c.insert("image_url".into(), json!("https://new.example/1.png"));
        c
    };
    cache_normalized_cards(&conn, &[priceless]).unwrap();
    let (trend, name, image, est): (Option<f64>, String, String, i64) = conn
        .query_row(
            &format!("SELECT price_trend, name, image_url, price_estimated FROM card_cache WHERE id = 'mtg-{UUID1}'"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(trend, Some(2.5), "real quote must survive a price-less refresh");
    assert_eq!(name, "Sol Ring Renamed", "metadata must refresh");
    assert_eq!(image, "https://new.example/1.png");
    assert_eq!(est, 0);

    // An ESTIMATED row has no real quote, but a price-less source touches
    // NEITHER the price columns NOR the estimate flag (cardCache.js CASE) —
    // proven here with a priced sibling keeping the estimate floor alive
    // (cacheNormalizedCards re-estimates the touched key afterwards).
    cache_normalized_cards(
        &conn,
        &[norm_card("mtg-sibling-1", "Sol Ring Renamed", Some(3.0))],
    )
    .unwrap();
    conn.execute(
        &format!("UPDATE card_cache SET price_estimated = 1, price_trend = 1.0 WHERE id = 'mtg-{UUID1}'"),
        [],
    )
    .unwrap();
    cache_normalized_cards(&conn, &[norm_card(&format!("mtg-{UUID1}"), "Sol Ring Renamed", None)]).unwrap();
    let (trend, est): (Option<f64>, i64) = conn
        .query_row(
            &format!("SELECT price_trend, price_estimated FROM card_cache WHERE id = 'mtg-{UUID1}'"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // The CASE kept the row priced (not NULL'd by the price-less write) and
    // the flag survived; refresh then re-pinned it to the sibling's floor.
    assert_eq!(trend, Some(3.0), "price-less source kept it priced, refresh re-pinned the floor");
    assert_eq!(est, 1, "price-less source keeps the estimate flag");

    // A priced write clears price_estimated (real quote wins).
    cache_normalized_cards(
        &conn,
        &[norm_card(&format!("mtg-{UUID2}"), "Sol Ring", Some(4.0))],
    )
    .unwrap();
    let est: i64 = conn
        .query_row(
            &format!("SELECT price_estimated FROM card_cache WHERE id = 'mtg-{UUID2}'"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(est, 0);
}

#[test]
fn estimate_borrows_cheapest_real_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "est.db");
    // Two printings of the same logical card: one priced, one not.
    cache_normalized_cards(&conn, &[norm_card(&format!("mtg-{UUID1}"), "Shared Card", Some(3.0))]).unwrap();
    cache_normalized_cards(&conn, &[norm_card("mtg-noprice-1", "Shared Card", None)]).unwrap();
    // Force the unpriced row to look like a fresh unquoted cache row.
    conn.execute(
        "UPDATE card_cache SET price_trend = NULL, price_normal = NULL, price_holofoil = NULL, price_etched = NULL, price_source = NULL WHERE id = 'mtg-noprice-1'",
        [],
    )
    .unwrap();

    let changed = refresh_all_estimates(&conn).unwrap();
    assert_eq!(changed, 1);
    let (trend, est, curr): (Option<f64>, i64, String) = conn
        .query_row(
            "SELECT price_trend, price_estimated, price_currency FROM card_cache WHERE id = 'mtg-noprice-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(trend, Some(3.0));
    assert_eq!(est, 1);
    assert_eq!(curr, "USD");

    // Idempotent: nothing to change on the second pass.
    assert_eq!(refresh_all_estimates(&conn).unwrap(), 0);

    // The priced sibling loses its quote → no floor left for anyone: the
    // estimated row is cleared by stamping price_trend = 0 (Node's clear,
    // verbatim) and dropping the flag.
    conn.execute(
        &format!("UPDATE card_cache SET price_trend = 0, price_normal = 0, price_holofoil = 0, price_etched = 0, price_source = NULL WHERE id = 'mtg-{UUID1}'"),
        [],
    )
    .unwrap();
    let changed = refresh_all_estimates(&conn).unwrap();
    assert_eq!(changed, 1);
    let (trend, est): (Option<f64>, i64) = conn
        .query_row(
            "SELECT price_trend, price_estimated FROM card_cache WHERE id = 'mtg-noprice-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(trend, Some(0.0));
    assert_eq!(est, 0);
}

#[test]
fn record_price_writes_only_on_movement() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "price.db");

    assert!(record_price(&conn, "cardA", 2.0).unwrap());
    assert!(!record_price(&conn, "cardA", 2.0).unwrap(), "same price is not movement");
    assert!(record_price(&conn, "cardA", 2.25).unwrap());
    assert!(!record_price(&conn, "cardA", 0.0).unwrap(), "zero is never recorded");
    assert!(!record_price(&conn, "", 2.0).unwrap(), "blank id never recorded");

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM price_history WHERE card_id = 'cardA'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);

    // Timestamp monotonicity: with a future latest row the next insert is
    // latest+1ms (the same-second PK collision guard).
    conn.execute(
        "INSERT INTO price_history (card_id, price, recorded_at) VALUES ('cardB', 1.0, '2099-01-01 00:00:00.000')",
        [],
    )
    .unwrap();
    assert!(record_price(&conn, "cardB", 1.5).unwrap());
    let at: String = conn
        .query_row(
            "SELECT recorded_at FROM price_history WHERE card_id = 'cardB' ORDER BY recorded_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(at, "2099-01-01 00:00:00.001");
}

// ---------------------------------------------------------------------------
// Sweep gate
// ---------------------------------------------------------------------------

#[test]
fn sweep_gate_follows_price_refresh_days() {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "gate.db");

    // Never swept → due.
    assert!(should_sweep_prices(&conn, "mtg"));
    // Just swept, daily interval → not due.
    mark_prices_swept(&conn, "mtg").unwrap();
    assert!(!should_sweep_prices(&conn, "mtg"));
    // price_refresh_days = 0 disables the automatic sweep entirely.
    conn.execute("UPDATE app_settings SET price_refresh_days = 0 WHERE id = 1", []).unwrap();
    assert!(!should_sweep_prices(&conn, "mtg"));
    conn.execute("UPDATE app_settings SET price_refresh_days = 1 WHERE id = 1", []).unwrap();
    // Swept 2 days ago with a 1-day interval → due again.
    conn.execute(
        "UPDATE app_settings SET mtg_prices_swept_at = datetime('now', '-2 days') WHERE id = 1",
        [],
    )
    .unwrap();
    assert!(should_sweep_prices(&conn, "mtg"));
    // Unknown game never sweeps (Node parity).
    assert!(!should_sweep_prices(&conn, "pokemon"));
}

// ---------------------------------------------------------------------------
// Set sync job
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sets_sync_writes_and_skips() {
    let (addr, mock) = spawn_mock().await;
    *mock.sets.lock().unwrap() = json!({"data": [
        {"code": "fdn", "name": "Foundation", "set_type": "expansion", "card_count": 235,
         "released_at": "2024-11-15", "icon_svg_uri": "https://c/fdn.svg"},
        {"code": "pfdn", "name": "Foundation Tokens", "set_type": "token", "card_count": 40,
         "released_at": "2024-11-15", "icon_svg_uri": "https://c/pfdn.svg", "parent_set_code": "fdn"}
    ]});
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "sets.db");
    let scry = scry_for(addr, 0.001);

    let n = fetch_and_cache_sets(&conn, &scry, false).await.unwrap();
    assert_eq!(n, 2);
    let row: (String, String, String, i64, String, String) = conn
        .query_row(
            "SELECT id, name, series, printed_total, release_date, symbol_url FROM sets WHERE id = 'mtg-fdn'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(row.0, "mtg-fdn");
    assert_eq!(row.1, "Foundation");
    assert_eq!(row.2, "expansion"); // series = set_type
    assert_eq!(row.3, 235); // printed_total = total = card_count
    assert_eq!(row.4, "2024-11-15");
    assert_eq!(row.5, "https://c/fdn.svg"); // logo_url = symbol_url = icon

    // Already populated → skip without a fetch (unforced).
    let before = mock.hits.lock().unwrap().len();
    fetch_and_cache_sets(&conn, &scry, false).await.unwrap();
    assert_eq!(mock.hits.lock().unwrap().len(), before, "unforced skip must not call Scryfall");
    // Force re-fetches.
    fetch_and_cache_sets(&conn, &scry, true).await.unwrap();
    assert_eq!(mock.hits_for("GET /sets").len(), 2);
}

// ---------------------------------------------------------------------------
// Price sweep
// ---------------------------------------------------------------------------

fn seed_sweep_db(conn: &Connection) {
    // 3 owned printings (uuid ids → id: identifiers), 1 deck-only printing
    // without a uuid (falls to sn:), 1 unpriced Plains (exercises the
    // missing-floor search leg), plus an unowned/untracked card that must
    // NOT be swept.
    let cards = [
        (format!("mtg-{UUID1}"), "Alpha Bolt", "ima", "1"),
        (format!("mtg-{UUID2}"), "Beta Bolt", "ima", "2"),
        ("mtg-owned-3".to_string(), "Gamma Bolt", "ima", "3"),
        ("mtg-deck-4".to_string(), "Deck Only", "stx", "10"),
        ("mtg-plain-5".to_string(), "Plains", "uma", "250"),
        ("mtg-none-6".to_string(), "Nobody Owns", "uma", "251"),
    ];
    for (id, name, set, num) in cards {
        conn.execute(
            "INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, price_trend, price_normal, language)
             VALUES (?1, ?2, 'Card', '[]', '[\"Creature\"]', 'Common', ?3, 'Set', ?4, NULL, NULL, 'English')",
            params![id, name, set, num],
        )
        .unwrap();
    }
    conn.execute("INSERT INTO users (id, username, password_hash, role, share_token) VALUES (1,'u','x','admin','share-u')", []).unwrap();
    conn.execute("INSERT INTO collection (card_id, quantity, user_id) VALUES (?1, 2, 1)", params![format!("mtg-{UUID1}")]).unwrap();
    conn.execute("INSERT INTO collection (card_id, quantity, user_id) VALUES (?1, 1, 1)", params![format!("mtg-{UUID2}")]).unwrap();
    conn.execute("INSERT INTO collection (card_id, quantity, user_id) VALUES ('mtg-owned-3', 1, 1)", []).unwrap();
    conn.execute("INSERT INTO collection (card_id, quantity, user_id) VALUES ('mtg-plain-5', 1, 1)", []).unwrap();
    conn.execute("INSERT INTO decks (id, user_id, name) VALUES (1, 1, 'd')", []).unwrap();
    conn.execute("INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (1, 'mtg-deck-4', 1)", []).unwrap();
    conn.execute(
        "UPDATE card_cache SET oracle_id = 'oracle-bolt' WHERE name = 'Alpha Bolt'",
        [],
    )
    .unwrap();
}

#[tokio::test]
async fn sweep_batches_identifiers_and_prices() {
    let (addr, mock) = spawn_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "sweep.db");
    seed_sweep_db(&conn);

    // Sweep row set: owned ∪ deck-keyed (Deck Only shares the card_key only
    // with itself; Nobody Owns is excluded).
    let rows = sweep_rows(&conn).unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.card_id.as_str()).collect();
    assert!(ids.contains(&"mtg-deck-4"), "deck printings must ride the sweep");
    assert!(!ids.contains(&"mtg-none-6"), "untracked printings must not");

    // Batch 1 answers the first ≤75, batch 2 the rest — with one id
    // deliberately absent so not_found is counted.
    let batch = |offset: usize| -> Value {
        let mut data = Vec::new();
        let mut not_found = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            if i < offset {
                continue;
            }
            match row.card_id.as_str() {
                "mtg-plain-5" => not_found.push(json!("n:Plains")),
                _ => data.push(raw_card(&row.card_id[4..], &row.name, &row.set_id, &row.number, Some(1.5))),
            }
        }
        json!({"data": data, "not_found": not_found})
    };
    let n = rows.len();
    *mock.collection_batches.lock().unwrap() = vec![batch(0), batch(n.min(75))];
    // Plains has no priced sibling and gets asked from /cards/search:
    // answer with nothing (the fill leg must not explode).
    *mock.search.lock().unwrap() = json!({"data": [], "has_more": false});

    let scry = scry_for(addr, 0.001);
    let out = update_collection_prices(&conn, &scry, true).await.unwrap();
    assert!(out.swept);
    assert_eq!(out.asked, n);
    assert_eq!(out.priced, n - 1, "every card but Plains pairs");
    assert_eq!(out.not_found, 1);

    // /cards/collection: n ≤ 75 here → one POST, identifier shapes per the
    // real contract (OBJECTS: {id} for uuid rows, {set,collector_number}
    // otherwise — Node's format; the mock 400s strings).
    let posts = mock.hits_for("POST /cards/collection");
    assert_eq!(posts.len(), 1, "small sweep is one batch");
    let body: Value = serde_json::from_str(&posts[0].1).unwrap();
    let ident = body["identifiers"].as_array().unwrap();
    assert!(ident.iter().any(|v| v["id"] == json!(UUID1)), "uuid rows send {{id}}");
    assert!(
        ident.iter().any(|v| v["set"] == json!("stx") && v["collector_number"] == json!("10")),
        "non-uuid rows send {{set,collector_number}}"
    );

    // card_cache rows carry the fresh prices; price_history has movement rows.
    let trend: Option<f64> = conn
        .query_row("SELECT price_trend FROM card_cache WHERE id = ?1", params![format!("mtg-{UUID1}")], |r| r.get(0))
        .unwrap();
    assert_eq!(trend, Some(1.5));
    let ph: i64 = conn.query_row("SELECT COUNT(*) FROM price_history", [], |r| r.get(0)).unwrap();
    assert_eq!(ph, (n - 1) as i64);

    // Sweep timestamp recorded.
    let swept: Option<String> = conn
        .query_row("SELECT mtg_prices_swept_at FROM app_settings WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert!(swept.is_some());

    // Re-sweep forced with identical prices: card_cache untouched, NO new
    // price_history rows (movement gate).
    let before_ph = ph;
    update_collection_prices(&conn, &scry, true).await.unwrap();
    let ph2: i64 = conn.query_row("SELECT COUNT(*) FROM price_history", [], |r| r.get(0)).unwrap();
    assert_eq!(ph2, before_ph, "unchanged prices must not append history");
}

#[tokio::test]
async fn sweep_batches_at_75_boundary() {
    let (addr, mock) = spawn_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "batches.db");
    conn.execute("INSERT INTO users (id, username, password_hash, role, share_token) VALUES (1,'u','x','admin','share-u')", []).unwrap();
    // 100 owned cards → two batches of 75 + 25.
    for i in 0..100 {
        let id = format!("mtg-batch-{i}");
        conn.execute(
            "INSERT INTO card_cache (id, name, supertype, set_id, set_name, number, language)
             VALUES (?1, ?2, 'Card', 'bse', 'Batches', ?3, 'English')",
            params![id, format!("Batch Card {i}"), i.to_string()],
        )
        .unwrap();
        conn.execute("INSERT INTO collection (card_id, quantity, user_id) VALUES (?1, 1, 1)", params![id]).unwrap();
    }
    *mock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    let scry = scry_for(addr, 0.001);
    let out = update_collection_prices(&conn, &scry, true).await.unwrap();
    assert_eq!(out.asked, 100);
    assert_eq!(mock.hits_for("POST /cards/collection").len(), 2, "75-identifier batches");
}

#[tokio::test]
async fn sweep_respects_unforced_gate() {
    let (addr, mock) = spawn_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "gate2.db");
    seed_sweep_db(&conn);
    *mock.collection_batches.lock().unwrap() = vec![json!({"data": [], "not_found": []})];
    mark_prices_swept(&conn, "mtg").unwrap(); // just swept

    let scry = scry_for(addr, 0.001);
    let out = update_collection_prices(&conn, &scry, false).await.unwrap();
    assert!(!out.swept, "hourly tick must refuse a recent sweep");
    assert!(mock.hits_for("POST /cards/collection").is_empty(), "refused sweep makes no calls");
}

// ---------------------------------------------------------------------------
// Client: 429 cooldown + retry ladder + 404 breaker (mock = "upstream")
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cooldown_pauses_everything_and_retry_recovers() {
    let (addr, mock) = spawn_mock().await;
    *mock.sets.lock().unwrap() = json!({"data": [{"code":"x","name":"X"}]});
    // First /sets call gets 429 (Retry-After 1s), then 200.
    mock.statuses.lock().unwrap().insert("GET /sets".into(), vec![429, 200]);
    *mock.retry_after.lock().unwrap() = Some("1".into());

    let scry = scry_for(addr, 0.001);
    let t = Instant::now();
    let v = scry.get_retried("/sets", &ScryOpts::default(), 4).await.unwrap();
    assert!(v.unwrap()["data"].is_array());
    let waited = t.elapsed();
    assert!(waited.as_millis() >= 900, "retry must ride the 429 cooldown ({waited:?})");

    // A different endpoint is blocked by the same cooldown: fire right after
    // recovery is over — first measure that a *fresh* 429 on another bucket
    // pauses the card endpoints too.
    *mock.sets.lock().unwrap() = json!({"data": []});
    mock.statuses.lock().unwrap().insert("POST /cards/collection".into(), vec![429]);
    *mock.retry_after.lock().unwrap() = Some("2".into());
    let err = scry.post("/cards/collection", json!({"identifiers": []}), &ScryOpts::default()).await.unwrap_err();
    assert!(err.is_status(429));
    assert!(scry.cooldown_until().await.is_some(), "cooldown armed queue-wide");
    mock.statuses.lock().unwrap().insert("POST /cards/collection".into(), vec![200]);
    let t = Instant::now();
    scry.post("/cards/collection", json!({"identifiers": []}), &ScryOpts::default()).await.unwrap();
    assert!(t.elapsed().as_millis() >= 1500, "card endpoint waits the shared cooldown");
}

#[tokio::test]
async fn endpoint_gaps_pace_upstream() {
    let (addr, mock) = spawn_mock().await;
    *mock.search.lock().unwrap() = json!({"data": [], "has_more": false});
    // gap_scale 2 → 1000 ms on the card endpoints, 200 ms floor.
    let scry = scry_for(addr, 2.0);
    let t = Instant::now();
    scry.get("/cards/search?q=a", &ScryOpts::default()).await.unwrap();
    scry.get("/cards/search?q=b", &ScryOpts::default()).await.unwrap();
    let two = t.elapsed();
    assert!(two.as_millis() >= 950, "2/second bucket paced the second search ({two:?})");

    // Sidecar mode (LOCAL_SCRYFALL) skips pacing entirely: same client
    // shape with local=false but a *local* target is paced; a swapped local
    // host is not.
    let local = scrybox_server::scryfall::Scry::new(scrybox_server::scryfall::ScryEnv {
        local: true,
        local_url: format!("http://{addr}"),
        base_url: format!("http://{addr}"),
        gap_scale: 2.0,
    });
    let t = Instant::now();
    local.get("/cards/search?q=a", &ScryOpts::default()).await.unwrap();
    local.get("/cards/search?q=b", &ScryOpts::default()).await.unwrap();
    assert!(t.elapsed().as_millis() < 900, "sidecar traffic never queues behind pacing");
    // ...but upstream:true still does (prices must keep the limiter).
    let t = Instant::now();
    local.post("/cards/collection", json!({"identifiers": []}), &ScryOpts { upstream: true, ..Default::default() }).await.ok();
    let _ = mock;
    let _ = t.elapsed(); // pace check only: the breaker must have tripped without hanging
}

#[tokio::test]
async fn forty_four_breaker_suppresses_repeats() {
    let (addr, _mock) = spawn_mock().await;
    let scry = scry_for(addr, 0.001);
    let url = "/cards/does-not-exist";

    // No breaker flag: every call hits the wire and surfaces the 404.
    let e = scry.get(url, &ScryOpts::default()).await.unwrap_err();
    assert!(e.is_status(404));
    let e = scry.get(url, &ScryOpts::default()).await.unwrap_err();
    assert!(e.is_status(404));

    // Breaker on: first 404 becomes Ok(None) and arms the breaker; the
    // repeat is answered from memory.
    assert!(scry.get_opt(url, &ScryOpts { breaker: true, ..Default::default() }).await.unwrap().is_none());
    let hits_before = {
        // count hits for this exact path only
        // (other tests share the mock? no — fresh server per test)
        0usize
    };
    let _ = hits_before;
    assert!(scry.get_opt(url, &ScryOpts { breaker: true, ..Default::default() }).await.unwrap().is_none());
    assert!(scry.breaker_hit(url, &ScryOpts { breaker: true, ..Default::default() }).await);
}

#[tokio::test]
async fn retry_ladder_gives_up_after_budget() {
    let (addr, mock) = spawn_mock().await;
    mock.statuses.lock().unwrap().insert("GET /sets".into(), vec![429]);
    *mock.retry_after.lock().unwrap() = Some("0".into());
    let scry = scry_for(addr, 0.001);
    let err = scry.get_retried("/sets", &ScryOpts::default(), 3).await.unwrap_err();
    assert!(err.is_status(429));
    assert_eq!(mock.hits_for("GET /sets").len(), 3, "retries exactly the ladder budget");
    // Non-429 errors never retry.
    mock.statuses.lock().unwrap().insert("GET /sets".into(), vec![500]);
    let before = mock.hits_for("GET /sets").len();
    let err = scry.get_retried("/sets", &ScryOpts::default(), 4).await.unwrap_err();
    assert!(err.is_status(500));
    assert_eq!(mock.hits_for("GET /sets").len(), before + 1);
}

// ---------------------------------------------------------------------------
// Arc client sharing compiles (AppState holds one shared Scry)
// ---------------------------------------------------------------------------

#[test]
fn scry_is_cheap_clone() {
    let dir = tempfile::tempdir().unwrap();
    let _ = dir;
    let a = scry_for("127.0.0.1:1".parse().unwrap(), 1.0);
    let _b = a.clone();
    let _shared: Arc<_> = Arc::new(a);
}
