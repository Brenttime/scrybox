//! Micro-profile of the cached-hit blocking work: auth lookup + revision
//! probe + cache probe, as the handlers run them per request. Run with
//! `cargo test --release -- --ignored --nocapture`.

use std::time::Instant;

use rusqlite::Connection;
use scrybox_server::{auth, db::DbPool};

fn seeded_db(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(include_str!("stats_seed.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO collection_read_revision (id, revision) VALUES (1, 1);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);
         INSERT OR IGNORE INTO stats_aux_revision (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    path
}

#[test]
#[ignore]
fn profile_hit_path() {
    let dir = std::env::temp_dir().join("srx6-profile");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = seeded_db(&dir, "p.db");
    let pool = DbPool::open(&path, 16).unwrap();

    let token = {
        // Find a live session token from the seed fixture.
        let conn = pool.get().unwrap();
        conn.query_row(
            "SELECT s.token FROM sessions s JOIN users u ON s.user_id = u.id \
             WHERE s.expires_at > DATETIME('now') LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };

    let mut hm = axum::http::HeaderMap::new();
    hm.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    let token = auth::bearer_token(&hm).map(|s| s.to_string());

    const N: u32 = 50_000;
    // Warm
    for _ in 0..1000 {
        auth::authenticate(&pool, token.as_deref(), true).ok();
    }
    let t = Instant::now();
    for _ in 0..N {
        auth::authenticate(&pool, token.as_deref(), true).ok();
    }
    println!("authenticate: {:.2} us/call", t.elapsed().as_secs_f64() * 1e6 / N as f64);

    let conn = pool.get().unwrap();
    for _ in 0..1000 {
        scrybox_server::db::current_revision(&conn).unwrap();
    }
    let t = Instant::now();
    for _ in 0..N {
        scrybox_server::db::current_revision(&conn).unwrap();
    }
    println!("current_revision: {:.2} us/call", t.elapsed().as_secs_f64() * 1e6 / N as f64);

    // Full per-request blocking sequence as stats handler does it today:
    // two separate pool checkouts.
    let t = Instant::now();
    for _ in 0..N {
        let c1 = pool.get().unwrap();
        auth::authenticate(&pool, token.as_deref(), true).ok();
        drop(c1);
        let c2 = pool.get().unwrap();
        scrybox_server::db::current_revision(&c2).unwrap();
        drop(c2);
    }
    println!("checkout+auth+checkout+rev: {:.2} us/call", t.elapsed().as_secs_f64() * 1e6 / N as f64);

    // Merged: one checkout, both reads on one connection.
    let t = Instant::now();
    for _ in 0..N {
        let c = pool.get().unwrap();
        auth::authenticate(&pool, token.as_deref(), true).ok();
        scrybox_server::db::current_revision(&c).unwrap();
        drop(c);
    }
    println!("one-checkout auth+rev: {:.2} us/call", t.elapsed().as_secs_f64() * 1e6 / N as f64);
}
