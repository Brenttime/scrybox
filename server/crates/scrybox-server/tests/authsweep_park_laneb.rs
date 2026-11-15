//! M5 live-slot parking pins for the Lane-B auth-sweep handlers
//! (iter-60 sweep of the remaining sync authenticate() call sites in
//! Lane-B-owned modules: backup, card_art, cards_meta::gate_core,
//! cards_tags, catalog_build, catalog_read::standalone_miss,
//! manabox::authed, model_assets, moxfield::auth_gate).
//!
//! Same harness as authsweep_park.rs (sets_park / cardscan_park
//! parked_probe shape): current-thread runtime whose DbPool has
//! live_max=1 with the ONLY live slot held by a runtime task, released
//! via oneshot. authenticate_async() checks out through get_async()
//! (idle-only pop on the worker, blocking-pool hop when nothing is
//! idle), so the request completes even while every live slot is
//! parked. RED-BEFORE-GREEN: neuter any pin's handler back to the sync
//! crate::auth::authenticate() and its pin dies at recv_timeout with
//! 'never completed' (the releasing task can never run on the parked
//! worker).
//!
//! Hermetic by construction: every pin is a bogus-token arm of a
//! Lane-B route that reaches the handler-local auth gate with NO
//! earlier return (method/owns gates pass first; catalog_read is a
//! POST so get_like falls through to standalone_miss; card_art is a
//! PUT so route_card falls through to auth_then_404). The bogus token
//! misses the sessions/api-key lookups and the fold answers the
//! authenticateToken 401 — no upstream network, no other table touched.

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
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');",
    )
    .unwrap();
    path
}

/// Shared harness (authsweep_park.rs parked_probe shape). STANDALONE
/// (empty node_upstream). rust_routes pins exactly the group under
/// test. Returns (status, body, pool).
fn parked_probe(
    method: &str,
    uri: &str,
    group: &str,
    token: Option<&'static str>,
    content_type: Option<&'static str>,
    body: Option<&'static str>,
    expected: u16,
) -> (u16, Vec<u8>, Arc<scrybox_server::db::DbPool>) {
    let dir = std::env::temp_dir().join(format!(
        "scrybox-authsweepb-{}-{}",
        group,
        scrybox_server::auth_support::uuid_v4()
    ));
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
    let group_owned = group.to_string();
    let method_owned = method.to_string();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let cfg = Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                node_upstream: String::new(),
                static_dir: None,
                rust_routes: [group_owned].into_iter().collect(),
                db_path: Some(db_for_rt),
                sidecar_url: None,
            };
            let mut st = AppState::new(cfg);
            st.db = Some(pool_for_rt.clone());
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

            let mut b = axum::http::Request::builder().method(method_owned.as_str()).uri(uri_owned.as_str());
            if let Some(tok) = token {
                b = b.header("authorization", format!("Bearer {tok}"));
            }
            if let Some(ct) = content_type {
                b = b.header("content-type", ct);
            }
            // ConnectInfo for handlers with the rate-limiter sibling wiring.
            b = b.extension(axum::extract::ConnectInfo(
                "127.0.0.1:9".parse::<std::net::SocketAddr>().unwrap(),
            ));
            let req = b.body(axum::body::Body::from(body.unwrap_or("").to_string())).unwrap();
            let resp = r.oneshot(req).await.unwrap();
            let status = resp.status().as_u16();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
            let _ = tx.send((status, bytes));
        });
    });

    // Give the request task time to reach its cap wait, then free the pool.
    // Pre-fix this never helps: the worker parked on the Condvar cannot run
    // the releasing task either, so recv_timeout below expires.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = release_tx.send(());

    let (status, bytes) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|_| {
        panic!(
            "{method} {uri} never completed: the handler parked the runtime worker on the DbPool \
             live cap (synchronous authenticate()/pool.get() inline on the worker)"
        )
    });
    assert_eq!(status, expected, "{method} {uri} answered {status}, expected {expected}; body: {}", String::from_utf8_lossy(&bytes));
    (status, bytes, pool)
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("json body")
}

// cards_tags: handle_oracle — authenticate_async before the Scryfall leg.
#[test]
fn cards_tags_oracle_auth_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/cards/some-id/oracle", "cards_tags", Some("nope"), None, None, 401);
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// cards_meta: gate_core (shared by 5 handlers incl. handle_highlight).
#[test]
fn cards_meta_highlight_gate_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "POST",
        "/api/cards/highlight",
        "cards_meta",
        Some("nope"),
        Some("application/json"),
        Some("{}"),
        401,
    );
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// catalog_build: handle_catalogs — authenticate_async before requireAdmin.
#[test]
fn catalog_build_admin_gate_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/admin/catalogs", "catalog_build", Some("nope"), None, None, 401);
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// catalog_read: standalone_miss — POST so get_like falls through to the
// authed miss path (authenticate_async after the body parse).
#[test]
fn catalog_read_standalone_miss_auth_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "POST",
        "/api/admin/catalogs/languages",
        "catalog_art",
        Some("nope"),
        Some("application/json"),
        Some("{}"),
        401,
    );
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// manabox: authed() (shared by the 4 /api/manabox-sync handlers).
#[test]
fn manabox_runs_authed_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/manabox-sync/runs", "manabox_backup", Some("nope"), None, None, 401);
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// moxfield: auth_gate (6 call sites; handle_status is the GET arm).
// The moxfield group boots the scheduler inside app(); its tick() runs
// synchronous pool.get() on the worker (known scheduler-side M5 item,
// off the request path) — quiesce it via the built-in kill switch so the
// pin measures only the handler's auth checkout.
#[test]
fn moxfield_status_auth_gate_does_not_park_the_runtime_worker() {
    std::env::set_var("SCRYBOX_DISABLE_MOXFIELD_SCHEDULER", "1");
    let (_s, bytes, pool) = parked_probe("GET", "/api/moxfield", "moxfield", Some("nope"), None, None, 401);
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// backup: handle_backups_collection — authenticate_async before requireAdmin.
#[test]
fn backup_collection_auth_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/admin/backups", "manabox_backup", Some("nope"), None, None, 401);
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// model_assets: handle_models — authenticate_async before requireAdmin.
#[test]
fn model_assets_models_auth_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/admin/models", "catalog_build", Some("nope"), None, None, 401);
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// card_art: auth_then_404 — PUT falls past route_card's method arms;
// settings::authenticate_async runs before the finalhandler 404.
#[test]
fn card_art_auth_then_404_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "PUT",
        "/api/card-art/some-card",
        "catalog_art",
        Some("nope"),
        Some("application/json"),
        Some("{}"),
        401,
    );
    assert_eq!(parse(&bytes)["error"], json!("Invalid or expired session token."));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}
