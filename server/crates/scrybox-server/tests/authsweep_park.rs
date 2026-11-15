//! M5 live-slot parking pins for the Lane-A auth-sweep handlers
//! (limited::handle, decks_combos::guard, export GET — iter-17 sweep of the
//! remaining sync authenticate() call sites in Lane-A-owned modules).
//!
//! Each handler previously ran synchronous authenticate() inline on the
//! runtime worker. DbPool::get() parks on the live-cap Condvar; a worker
//! parked there starves the whole runtime — the releasing task can never
//! run and the request never completes. Each pin holds the pool's single
//! live slot from a runtime task and asserts the request still completes
//! within 10s. RED-BEFORE-GREEN: neuter the handler's authenticate_async
//! back to the sync authenticate() and its pin dies at recv_timeout with
//! 'never completed'.
//!
//! Hermetic by construction: every fold asserted here answers BEFORE any
//! upstream network (limited short-circuits at the empty-decklist 400;
//! combos 404s on the missing deck; export reads an empty collection; the
//! bogus-token arms die inside the two auth SELECTs).

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

/// Shared harness (sets_park / cardscan_park parked_probe shape):
/// current-thread runtime whose DbPool has live_max=1 with the ONLY live
/// slot held by a runtime task, released via oneshot (stored value wakes
/// late registrations). STANDALONE (empty node_upstream). rust_routes pins
/// exactly the group under test. Returns (status, body, pool).
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
        "scrybox-authsweep-{}-{}",
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
            // ConnectInfo for handlers with the rate-limiter sibling
            // wiring (oneshot bypasses the listener; cardscan_park shape).
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

    // Event-driven release (LANE-B iter-65, same shape as legs_park
    // iter-22 / authsweep_park_laneb iter-64): hold the slot until the
    // request has actually attempted its checkout — every probe below
    // authenticates, and authenticate_async's get_async idle-miss
    // increments blocking_fallbacks BEFORE it waits on the live cap. The
    // old blind sleep(300 ms) could release before the worker reached its
    // first idle-miss under load: the holder's returned conn got popped
    // from idle and the pin passed WITHOUT EVER PARKING (vacuous green —
    // the live_count assert below cannot see that). RED arms intact: a
    // synchronous get() build never registers the counter, falls through
    // at the 5 s cap, the worker stays parked (it owns the worker), and
    // recv_timeout below expires exactly as before.
    let hop_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while pool.blocking_fallbacks() == 0 && std::time::Instant::now() < hop_deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let _ = release_tx.send(());

    let (status, bytes) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|_| {
        panic!(
            "{method} {uri} never completed: the handler parked the runtime worker on the DbPool \
             live cap (synchronous authenticate()/pool.get() inline on the worker)"
        )
    });
    assert_eq!(status, expected, "{method} {uri} answered {status}, expected {expected}");
    assert!(
        pool.blocking_fallbacks() >= 1,
        "{method} {uri} answered {status} without EVER hopping to the blocking pool — \
         with the only live slot held by a runtime task that means the request never took \
         its get_async checkout (pin vacuous), not that it was safe"
    );
    (status, bytes, pool)
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

// ---------------------------------------------------------------------------
// POST /api/limited/analyze — settings::authenticate_async twin (headers)
// ---------------------------------------------------------------------------

#[test]
fn limited_valid_auth_never_parks_a_runtime_worker() {
    // Valid session: the auth hop (two SELECTs on the held-slot pool via
    // get_async) must not freeze the worker. The route then short-circuits
    // at the empty-decklist 400 — no scryfall network, no DB beyond auth.
    let (_s, bytes, pool) = parked_probe(
        "POST",
        "/api/limited/analyze",
        "limited",
        Some("sess-1"),
        Some("application/json"),
        Some(r#"{"list":""}"#),
        400,
    );
    assert_eq!(parse(&bytes), json!({"error": "No cards found in the list"}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn limited_auth_reject_never_parks_a_runtime_worker() {
    // Bogus token: the 401 fold still ran BOTH auth SELECTs (session then
    // api_key) on a checked-out connection — the auth-query park vector.
    // (The no-token arm returns before any checkout, nothing to pin.)
    let (_s, bytes, pool) = parked_probe(
        "POST",
        "/api/limited/analyze",
        "limited",
        Some("bogus-token"),
        Some("application/json"),
        Some(r#"{"list":""}"#),
        401,
    );
    assert_eq!(parse(&bytes), json!({"error": "Invalid or expired session token."}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// GET /api/decks/{id}/combos — auth::authenticate_async (token) via guard
// ---------------------------------------------------------------------------

#[test]
fn combos_missing_deck_never_parks_a_runtime_worker() {
    // Valid session: auth hop, then the deck read (already spawn_blocking
    // since iter-87) must both survive the held slot. Missing deck → the
    // route's own 404 fold, no network.
    let (_s, bytes, pool) = parked_probe(
        "GET",
        "/api/decks/777/combos",
        "decks_combos",
        Some("sess-1"),
        None,
        None,
        404,
    );
    assert_eq!(parse(&bytes), json!({"error": "Deck not found or unauthorized"}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn combos_auth_reject_never_parks_a_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "GET",
        "/api/decks/777/combos",
        "decks_combos",
        Some("bogus-token"),
        None,
        None,
        401,
    );
    assert_eq!(parse(&bytes), json!({"error": "Invalid or expired session token."}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// GET /api/export?format=json — auth::authenticate_async (token)
// ---------------------------------------------------------------------------

#[test]
fn export_empty_collection_never_parks_a_runtime_worker() {
    // Valid session: auth hop, then the export read (spawn_blocking since
    // iter-87) on the held slot; empty collection answers JSON [].
    let (_s, bytes, pool) = parked_probe(
        "GET",
        "/api/export?format=json",
        "export",
        Some("sess-1"),
        None,
        None,
        200,
    );
    assert_eq!(parse(&bytes), json!([]), "empty export must survive the hops");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn export_auth_reject_never_parks_a_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "GET",
        "/api/export?format=json",
        "export",
        Some("bogus-token"),
        None,
        None,
        401,
    );
    assert_eq!(parse(&bytes), json!({"error": "Invalid or expired session token."}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}
