//! Lane B iter-20 (Gate D): the boot-sweep RSS spike guard.
//!
//! update_collection_prices used to hold TWO full serde_json::Value trees per
//! card (the cache list + the pairs list) for the ENTIRE collection before
//! writing anything — the named suspect for the 1.4-1.7 GB VmHWM seen during
//! the boot_catchup sweep on the live instance (~25.5k owned/deck printings).
//! This test forces a sweep over a 20k-row synthetic collection (realistic
//! card payloads: oracle text, faces, images, prices) against the scripted
//! mock and asserts the process peak (VmHWM) grew by less than a fixed
//! budget while the sweep ran. The mock's scripted batches are built BEFORE
//! the baseline sample so the mock's own retention cannot pollute the delta.

#[path = "mock_scryfall.rs"]
mod mock_scryfall;

use std::sync::Arc;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use mock_scryfall::{scry_for, spawn_mock};
use scrybox_server::scry_jobs::update_collection_prices;

const N: usize = 20_000;

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

/// Process peak RSS in kB (VmHWM), the same measure the restart/live probes
/// report as the instance VmHWM.
fn vmhwb_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap()
        .parse()
        .unwrap()
}
fn vmrss_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap()
        .parse()
        .unwrap()
}

/// A realistic Scryfall card object (sized like the real /cards/collection
/// payloads: oracle text, image URI set, prices; every 5th card is a two-face
/// double so the oracle_faces tree is exercised too).
fn big_raw(i: usize) -> Value {
    let name = format!("Card The Wandering {i}");
    let text = "Flying, vigilance. Whenever this creature attacks, create a 1/1 white Soldier creature token. Then if you control three or more other Creatures, target creature gets +2/+2 until end of turn and you draw a card. Splice onto Arcane";
    let set = format!("st{i}");
    if i % 5 == 0 {
        json!({
            "id": format!("card-{i}"),
            "name": name,
            "set": set,
            "set_name": format!("Set {i}"),
            "collector_number": i.to_string(),
            "lang": "en",
            "layout": "double_faced_token",
            "rarity": "uncommon",
            "type_line": format!("Creature — Human {name} // Creature — Spirit"),
            "colors": ["W", "U"],
            "color_identity": ["W", "U"],
            "cmc": 4.0,
            "mana_cost": "{{1}}{{W}}{{U}}",
            "card_faces": [
                { "name": name, "mana_cost": "{{1}}{{W}}", "type_line": "Creature — Human",
                  "oracle_text": text,
                  "image_uris": { "normal": format!("https://cards.example/{i}.png"),
                                  "large": format!("https://cards.example/{i}-large.png"),
                                  "small": format!("https://cards.example/{i}-small.png"),
                                  "png": format!("https://cards.example/{i}.png"),
                                  "art_crop": format!("https://cards.example/{i}-art.png") } },
                { "name": format!("The Spark of {i}"), "mana_cost": "", "type_line": "Creature — Spirit",
                  "oracle_text": text,
                  "image_uris": { "normal": format!("https://cards.example/{i}b.png") } }
            ],
            "prices": { "usd": (1.0 + (i % 100) as f64 / 100.0).to_string(), "usd_foil": null, "usd_etched": null },
            "purchase_uris": { "tcgplayer": format!("https://tcgplayer.example/{i}"), "cardmarket": format!("https://cardmarket.example/{i}") },
            "identifiers": { "mtgo_id": i },
            "related_uris": { "gatherer": format!("https://gatherer.example/{i}") },
            "printings": ["stx", "ima", "uma", "m11", "znr"],
        })
    } else {
        json!({
            "id": format!("card-{i}"),
            "name": name,
            "set": set,
            "set_name": format!("Set {i}"),
            "collector_number": i.to_string(),
            "lang": "en",
            "layout": "normal",
            "rarity": "common",
            "type_line": format!("Creature — Human {name}"),
            "colors": ["W"],
            "color_identity": ["W"],
            "cmc": 2.0,
            "mana_cost": "{{1}}{{W}}",
            "oracle_text": text,
            "flavor_text": "~ \"Footsteps echo where it walks, though none are there to hear them.\"",
            "image_uris": { "normal": format!("https://cards.example/{i}.png"),
                            "large": format!("https://cards.example/{i}-large.png"),
                            "small": format!("https://cards.example/{i}-small.png"),
                            "png": format!("https://cards.example/{i}.png"),
                            "art_crop": format!("https://cards.example/{i}-art.png") },
            "prices": { "usd": (1.0 + (i % 100) as f64 / 100.0).to_string(), "usd_foil": null, "usd_etched": null },
            "purchase_uris": { "tcgplayer": format!("https://tcgplayer.example/{i}"), "cardmarket": format!("https://cardmarket.example/{i}") },
            "identifiers": { "mtgo_id": i },
            "related_uris": { "gatherer": format!("https://gatherer.example/{i}") },
            "printings": ["stx", "ima", "uma", "m11", "znr"],
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_peak_rss_stays_in_budget() {
    let (addr, mock) = spawn_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded_db(dir.path(), "mem.db");

    // Seed 20k owned printings — the sweep row count matches the live
    // instance's boot sweep order of magnitude (25.5k).
    conn.execute("INSERT INTO users (id, username, password_hash, role, share_token) VALUES (1,'u','x','admin','share-u')", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 0..N {
        let id = format!("mtg-card-{i}");
        conn.execute(
            "INSERT INTO card_cache (id, name, supertype, subtypes, types, rarity, set_id, set_name, number, price_trend, price_normal, language, oracle_id)
             VALUES (?1, ?2, 'Card', '[]', '[\"Creature\"]', 'Common', ?3, 'Set', ?4, NULL, NULL, 'English', ?5)",
            params![id, format!("Card The Wandering {i}"), format!("st{i}"), i.to_string(), format!("or-{i}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO collection (card_id, quantity, user_id) VALUES (?1, 1, 1)",
            params![id],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();

    // Identifier-driven mock: every POST /cards/collection answers from the
    // request's own identifiers, so the chunk order sweep_rows picks (SQL
    // order, not insertion order) cannot skew the pairing.
    *mock.collection_generator.lock().unwrap() = Some(Arc::new(|id: &Value| {
        let set = id.get("set").and_then(|v| v.as_str()).unwrap_or("st0");
        let i: usize = set.get(2..).and_then(|s| s.parse().ok()).unwrap_or(0);
        big_raw(i)
    }));
    *mock.search.lock().unwrap() = json!({ "data": [], "has_more": false });

    // Everything the mock itself retains exists NOW; the delta after this
    // point is the sweep's own working set.
    let before_hwm = vmhwb_kb();
    let before_rss = vmrss_kb();

    let scry = scry_for(addr, 0.001);
    let out = update_collection_prices(&conn, &scry, true).await.unwrap();
    assert!(out.swept);
    assert_eq!(out.priced, N, "every card priced");

    let after_hwm = vmhwb_kb();
    let delta_mb = after_hwm.saturating_sub(before_hwm) / 1024;
    println!(
        "SWEEP-MEMORY n={N} rss_before={before_rss}kB hwm_before={before_hwm}kB hwm_after={after_hwm}kB delta={delta_mb}MB"
    );

    // Final-state checks: the compact representation must write exactly what
    // the old Value-tree accumulation wrote.
    let priced: i64 = conn
        .query_row("SELECT COUNT(*) FROM card_cache WHERE price_trend IS NOT NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(priced as usize, N, "card_cache fully priced");
    let ph: i64 = conn
        .query_row("SELECT COUNT(*) FROM price_history", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ph as usize, N, "price_history movement rows");
    let img: Option<String> = conn
        .query_row("SELECT image_url FROM card_cache WHERE id = 'mtg-card-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(img.as_deref(), Some("https://cards.example/1.png"));
    let faces: Option<String> = conn
        .query_row("SELECT oracle_faces FROM card_cache WHERE id = 'mtg-card-5'", [], |r| r.get(0))
        .unwrap();
    assert!(faces.unwrap().contains("The Spark of 5"), "oracle_faces persisted");

    // Budget: streaming must keep the sweep's whole working set (parsed
    // responses + pre-bound rows for every card) under 200 MB. The legacy
    // two-Value-trees-per-card accumulation needs well over this at N=20k —
    // see PROGRESS-B iter-20 for the before/after numbers.
    assert!(
        delta_mb < 200,
        "sweep peak grew {delta_mb} MB over the {before_rss} kB baseline — JSON retention regression?"
    );
    let _ = Arc::strong_count(&mock);
}
