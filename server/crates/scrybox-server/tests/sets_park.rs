//! M5 live-slot parking pins for the native GET /api/sets handler
//! (Lane-A M5 family, marketplace iter-83/84 / cardscan iter-54 shape).
//!
//! sets::handle previously ran synchronous authenticate() + pool.get()
//! (flat read) + pool.get() (tree exclusions read) inline on the runtime
//! worker. DbPool::get() parks on the live-cap Condvar; a worker parked
//! there starves the whole runtime — the releasing task can never run and
//! the request never completes. Each pin holds the pool's single live slot
//! from a runtime task and asserts the request still completes within 10s.
//! RED-BEFORE-GREEN: neuter sets::handle back to `authenticate`/inline
//! `pool.get()` and every pin dies at recv_timeout with 'never completed'.

use std::sync::{mpsc, Arc};

use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

fn seed(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("bindarr.db");
    std::fs::write(&path, []).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_settings (id INTEGER PRIMARY KEY, scan_exclude_tokens INTEGER NOT NULL DEFAULT 0, scan_exclude_art_cards INTEGER NOT NULL DEFAULT 0, scan_exclude_jumpstart INTEGER NOT NULL DEFAULT 0, scan_exclude_promos INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO app_settings (id, scan_exclude_tokens, scan_exclude_art_cards, scan_exclude_jumpstart, scan_exclude_promos) VALUES (1, 1, 0, 0, 0);
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO sets (id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url)
           VALUES ('mtg-fdn','Foundation','Foundation',235,275,'2024-11-15','fdn','https://c/fdn.svg','https://c/fdn.svg');",
    )
    .unwrap();
    path
}

/// Shared harness (cardscan_park parked_probe shape): current-thread
/// runtime whose DbPool has live_max=1 with the ONLY live slot held by a
/// runtime task, released via oneshot (stored value wakes late
/// registrations). STANDALONE (empty node_upstream). Pre-stamped
/// scryfall_sets memo keeps ?tree=1 hermetic (no network, no mock).
/// Returns (status, body, pool).
fn parked_probe(uri: &str, token: Option<&'static str>, tree: bool, expected: u16) -> (u16, Vec<u8>, Arc<scrybox_server::db::DbPool>) {
    let dir = std::env::temp_dir().join(format!("scrybox-setspark-{}", scrybox_server::auth_support::uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = seed(&dir);

    let pool = Arc::new(
        scrybox_server::db::DbPool::open_with(
            &db_path,
            scrybox_server::db::PoolConfig { max: 1, cache_kb: 1024, mmap_kb: 0, live_max: 1 },
        )
        .unwrap(),
    );

    let (tx, rx) = mpsc::channel::<(u16, Vec<u8>)>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let pool_for_rt = pool.clone();
    let db_for_rt = db_path.clone();
    let uri_owned = uri.to_string();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let cfg = Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                node_upstream: String::new(),
                static_dir: None,
                rust_routes: ["sets".to_string()].into_iter().collect(),
                db_path: Some(db_for_rt),
                sidecar_url: None,
            };
            let mut st = AppState::new(cfg);
            st.db = Some(pool_for_rt.clone());
            if tree {
                // Fresh timestamp inside the 6h TTL: the tree fold answers
                // from the memo — child map exercised, network never touched.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;
                let list = vec![
                    json!({"code": "fdn", "name": "Foundation", "set_type": "expansion"}),
                    json!({"code": "fdnp", "name": "Foundation Promos", "set_type": "promo",
                           "parent_set_code": "fdn", "card_count": 5}),
                    json!({"code": "fdnt", "name": "Foundation Tokens", "set_type": "token",
                           "parent_set_code": "fdn", "card_count": 9}),
                ];
                st.scryfall_sets = Arc::new(std::sync::Mutex::new(Some((now, list))));
            }
            let r = app(st);

            // Occupy the pool's single live slot from a runtime task: the
            // ONLY way to free it is another task on this same worker.
            let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
            {
                let pool2 = pool_for_rt.clone();
                tokio::spawn(async move {
                    let held = pool2.get().unwrap();
                    let _ = held_tx.send(());
                    let _ = release_rx.await;
                    drop(held);
                });
            }
            held_rx.await.unwrap();

            let mut b = axum::http::Request::builder().method("GET").uri(uri_owned.as_str());
            if let Some(tok) = token {
                b = b.header("authorization", format!("Bearer {tok}"));
            }
            let req = b.body(axum::body::Body::empty()).unwrap();
            let resp = r.oneshot(req).await.unwrap();
            let status = resp.status().as_u16();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
            let _ = tx.send((status, bytes));
        });
    });

    // Counter-observed release (LANE-A iter-24, legs_park iter-22 shape):
    // free the holder when the FIRST get_async idle-miss registers (the
    // blocking-pool hop — hence the live-cap wait — has demonstrably
    // started) instead of a blind 300 ms sleep that a loaded scheduler can
    // beat. If the release lands before the handler reaches its checkout,
    // the holder's conn is popped back from idle inline: the pin goes
    // false-RED (no hop ever happened) or vacuously GREEN on pools that
    // close returned conns — both closed by the assert below. 5 s fallback
    // for shapes that legitimately never miss. Pre-fix park still reddens:
    // a worker parked on the Condvar can never run the releasing task, so
    // the counter never fires, the fallback expires, recv_timeout panics.
    {
        let start = std::time::Instant::now();
        while pool.blocking_fallbacks() == 0
            && start.elapsed() < std::time::Duration::from_secs(5)
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    let _ = release_tx.send(());

    let (status, bytes) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|_| {
        panic!(
            "GET {uri} never completed: the handler parked the runtime worker on the DbPool live cap \
             (synchronous authenticate/pool.get() inline on the worker)"
        )
    });
    assert!(
        pool.blocking_fallbacks() >= 1,
        "GET {uri}: pin never observed a get_async blocking-pool hop before the \
         release — the hold was vacuous (the handler popped the returned idle conn inline)"
    );
    assert_eq!(status, expected, "GET {uri} answered {status}, expected {expected}");
    (status, bytes, pool)
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

#[test]
fn flat_list_never_parks_a_runtime_worker() {
    // Full standalone arm: auth (get_async hop) → flat read (spawn_blocking
    // hop), all with the pool's single live slot held by a runtime task.
    let (_s, bytes, pool) = parked_probe("/api/sets", Some("sess-1"), false, 200);
    let v = parse(&bytes);
    let arr = v.as_array().expect("flat set list must survive the hops");
    assert_eq!(arr.len(), 1, "seeded set list expected, got {v}");
    assert_eq!(arr[0]["id"], json!("mtg-fdn"));
    assert_eq!(arr[0]["set_code"], json!("fdn"));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn tree_fold_never_parks_a_runtime_worker() {
    // The tree arm adds the scan_exclusions read: the spawn_blocking hop
    // must have read app_settings (tokens excluded) — only the promo child
    // survives under fdn.
    let (_s, bytes, pool) = parked_probe("/api/sets?tree=1", Some("sess-1"), true, 200);
    let v = parse(&bytes);
    let arr = v.as_array().expect("tree set list expected");
    assert_eq!(arr.len(), 1);
    let children = arr[0]["children"].as_array().unwrap();
    let codes: Vec<&str> = children.iter().map(|c| c["code"].as_str().unwrap()).collect();
    assert_eq!(codes, vec!["fdnp"], "token child must be excluded via the hopped app_settings read");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn auth_reject_never_parks_a_runtime_worker() {
    // Bogus token: the 401 fold still ran BOTH auth SELECTs (session then
    // api_key) on a checked-out connection — this is the auth-query park
    // vector. (The no-token arm returns before any checkout, nothing to
    // pin.) With authenticate() (sync) this dies at recv_timeout too.
    let (_s, bytes, pool) = parked_probe("/api/sets", Some("bogus-token"), false, 401);
    assert_eq!(
        parse(&bytes),
        json!({"error": "Invalid or expired session token."})
    );
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}
