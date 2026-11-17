//! Memory queue item 6 (MEMORY.md #9): the stats boot warm must NOT run at
//! boot — it parks until the FIRST real (non-health) request hits a native
//! route, then pre-builds like iter-58. Gate D idle-HWM contract: an idle
//! process (uptime probes only) does zero warm work — no scratch pool, no
//! body builds, no DB touch on the warm path.
//!
//! Observable proof: `StatsCache::store_len()`. `spawn_refresher` only
//! pre-builds for users who REQUESTED stats (`seen`), and
//! `hist_store`/sub-route caches are untouched by these paths, so the store
//! growing is exclusively the boot warm.

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
};
use rusqlite::Connection;
use scrybox_server::{app, stats::BootWarmMode, AppState, Config};
use tower::ServiceExt;

use scrybox_server::stats::boot_warm_mode;

/// Env is process-global and cargo runs test fns in parallel threads — the
/// scan_match_route.rs convention: hold this across set + call + remove.
/// Poison-tolerant: a failing assertion must not cascade into lock errors
/// for the sibling tests (the env is still safe to mutate after a panic).
fn env_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    env_lock().lock().unwrap_or_else(|p| p.into_inner())
}

fn seeded_db(dir: &std::path::Path, name: &str) -> PathBuf {
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

fn state_for(db_path: PathBuf) -> AppState {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        // Empty upstream: standalone shape — /api/health is answered
        // natively, no Node needed, no prober.
        node_upstream: String::new(),
        static_dir: None,
        rust_routes: ["stats".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    AppState::new(cfg)
}

async fn get(st: &AppState, path: &str) -> StatusCode {
    let req = Request::builder().method("GET").uri(path).body(Body::empty()).unwrap();
    app(st.clone()).oneshot(req).await.unwrap().status()
}

async fn wait_store_len(cache: &scrybox_server::stats::StatsCache, want: usize, budget_ms: u64) -> bool {
    for _ in 0..budget_ms / 20 {
        if cache.store_len() >= want {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    cache.store_len() >= want
}

/// Default mode: boot = no warm; first /api/health = still no warm; first
/// real native request = warm runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_warm_defers_until_first_real_request() {
    let _g = env_guard();
    std::env::remove_var("SCRYBOX_STATS_PREWARM");
    assert_eq!(boot_warm_mode(), BootWarmMode::Traffic);
    let dir = tempfile::tempdir().unwrap();
    let st = state_for(seeded_db(dir.path(), "gate-traffic.db"));
    let cache = st.stats_cache.clone();

    scrybox_server::stats::spawn_boot_warm(&st);

    // Boot window: give an EAGER warm every chance to have started and
    // finished its first build (single-shot cold compute measured 573 ms;
    // 400 ms is already > the scratch-open + user-query + build for this
    // 3-user fixture) — the store must be untouched.
    // (Red-before-fix hook A: neuter the gate park in spawn_boot_warm and
    // this assertion fails with store_len >= 1.)
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(cache.store_len(), 0, "idle boot must run ZERO warm work");

    // Health-only traffic (what an idle container actually sees forever):
    // must NOT open the gate.
    // (Red-before-fix hook B: drop the `/api/health` exclusion in the
    // lib.rs traffic layer and the wait below times out.)
    assert_eq!(get(&st, "/api/health").await, StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(cache.store_len(), 0, "health probes alone must not start the warm");

    // First REAL request opens the gate: the layer sits AHEAD of the
    // handler, so even an unauthenticated 401 on /api/stats counts as real
    // traffic (an uptime probe never hits this path). The warm then fills
    // the store with the collection-owning seeded users' bodies — proof it
    // ran, since nothing else in this test touches the stats store (the
    // refresher only pre-builds for users who requested stats, and the 401
    // request path stores nothing).
    assert_eq!(get(&st, "/api/stats").await, StatusCode::UNAUTHORIZED);
    assert!(
        wait_store_len(&cache, 1, 5_000).await,
        "warm must run after the first real request (store_len={})",
        cache.store_len()
    );
}

/// Eager restore (`SCRYBOX_STATS_PREWARM=boot`): the iter-58 shape still
/// works — warm runs with no traffic at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_warm_boot_mode_warms_without_traffic() {
    let _g = env_guard();
    std::env::set_var("SCRYBOX_STATS_PREWARM", "boot");
    let dir = tempfile::tempdir().unwrap();
    let st = state_for(seeded_db(dir.path(), "gate-eager.db"));
    let cache = st.stats_cache.clone();

    scrybox_server::stats::spawn_boot_warm(&st);
    assert!(
        wait_store_len(&cache, 1, 5_000).await,
        "=boot must warm at boot without any traffic (store_len={})",
        cache.store_len()
    );
    std::env::remove_var("SCRYBOX_STATS_PREWARM");
}

/// Kill switch stays pure lazy (`=0`): no traffic + no warm ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_warm_off_mode_never_warms() {
    let _g = env_guard();
    std::env::set_var("SCRYBOX_STATS_PREWARM", "0");
    let dir = tempfile::tempdir().unwrap();
    let st = state_for(seeded_db(dir.path(), "gate-off.db"));
    let cache = st.stats_cache.clone();

    scrybox_server::stats::spawn_boot_warm(&st);
    // Open the gate anyway: Off must not warm even then.
    st.open_traffic_gate_for_test();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(cache.store_len(), 0, "=0 must stay pure lazy even with traffic");
    std::env::remove_var("SCRYBOX_STATS_PREWARM");
}
