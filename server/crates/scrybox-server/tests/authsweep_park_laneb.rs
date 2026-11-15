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
    // iter-61 post-auth pins additionally exercise the manabox runs,
    // moxfield authors/decks and oracle-tag tables (CREATE-only fixtures).
    // app_settings (scry_schema's shape, inlined to avoid its duplicate
    // list_cards CREATE) is needed by moxfield_schema's ALTER legs.
    conn.execute_batch(
        "CREATE TABLE app_settings (
           id INTEGER PRIMARY KEY CHECK (id = 1),
           public_base_url TEXT DEFAULT '',
           oracle_tags_checked_at INTEGER,
           oracle_id_backfill_completed_at INTEGER,
           mtg_prices_swept_at DATETIME,
           price_refresh_days INTEGER NOT NULL DEFAULT 1,
           scan_exclude_tokens INTEGER NOT NULL DEFAULT 0,
           scan_exclude_art_cards INTEGER NOT NULL DEFAULT 0,
           scan_exclude_jumpstart INTEGER NOT NULL DEFAULT 0,
           scan_exclude_promos INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO app_settings (id) VALUES (1);",
    )
    .unwrap();
    conn.execute_batch(include_str!("manabox_schema.sql")).unwrap();
    conn.execute_batch(include_str!("moxfield_schema.sql")).unwrap();
    conn.execute_batch(include_str!("oracle_tag_schema.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL),
                  (2,'u2','x','admin','tok-2',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00'), ('sess-2', 2, '2099-01-01 00:00:00');",
    )
    .unwrap();
    // Author row for the contended_probe pins (moxfield add_author
    // existing-branch and the direct sync-decklist fold). AFTER users —
    // moxfield_authors.user_id has an FK onto users(id).
    conn.execute_batch(
        "INSERT INTO moxfield_authors (id, user_id, moxfield_user, display_name)
         VALUES (1, 1, 'mockuser', 'Mock User');",
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

// ---------------------------------------------------------------------------
// iter-61: POST-AUTH body sweeps. The iter-60 sweep moved every Lane-B
// authenticate() to authenticate_async but the request-path handlers' OWN
// inline pool.get() checkouts (post-auth) could still park the worker on
// the live cap whenever something else (a job, a stats build, a sibling
// request) held every slot. Two pin classes cover the converted arms
// (manabox handle_runs + handle_report, cards_tags oracle-tags/tag_stats,
// card_art handle_post known-gate, backup createBackup, moxfield
// remove_author, moxfield add_author + sync-decklist):
//
//  * CANARIES (parked_probe shape, valid session, run past auth): assert
//    the converted arm answers correctly and leaks no checkout. They are
//    NOT parking proofs: in a single-request harness the auth hop itself
//    waits on the held slot, so by the time the handler's own checkout
//    runs the connection has returned to idle and is popped inside the
//    same task poll (no yield — no task can interleave). A reverted sync
//    pool.get() cannot be observed through this shape; do not claim
//    red-before-green for a canary site.
//
//  * PARKING PINS (contended_probe, moxfield folds): the handler has a
//    REAL yield window (the Moxfield get_user leg over the mock) between
//    returning the auth checkout and the next body checkout. An idle-only
//    grabber task seizes the connection the instant auth returns it and
//    holds it until the release timer, so that body checkout faces an
//    empty idle list at the live cap: sync pool.get() parks the worker
//    (RED: recv_timeout 'never completed'), get_async hops to the
//    blocking pool and answers (GREEN, guarded non-vacuous by
//    blocking_fallbacks() >= 2 — if the grab never engaged the pin
//    fails). RED-BEFORE-GREEN: revert a parked site to pool.get() and
//    its pin dies at recv_timeout.
// ---------------------------------------------------------------------------

// manabox: handle_runs — SELECT manabox_sync_runs on the seeded (empty) table.
#[test]
fn manabox_runs_body_checkout_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/manabox-sync/runs", "manabox_backup", Some("sess-1"), None, None, 200);
    assert_eq!(parse(&bytes), json!([]), "runs body: {}", String::from_utf8_lossy(&bytes));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// manabox: handle_report — missing run id rides the 404 arm after the checkout.
#[test]
fn manabox_report_body_checkout_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/manabox-sync/runs/999/report", "manabox_backup", Some("sess-1"), None, None, 404);
    assert_eq!(parse(&bytes)["error"], json!("Not found"));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// cards_tags: handle_oracle_tags — active_generation miss answers
// ready:false from the get_async checkout.
#[test]
fn cards_tags_oracle_tags_body_checkout_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/cards/mtg-abc123/oracle-tags", "cards_tags", Some("sess-1"), None, None, 200);
    assert_eq!(parse(&bytes)["ready"], json!(false));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// cards_tags: handle_tag_stats — same no-generation arm past the auth+write bump.
#[test]
fn cards_tags_tag_stats_body_checkout_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "POST",
        "/api/cards/tag-stats",
        "cards_tags",
        Some("sess-1"),
        Some("application/json"),
        Some(r#"{"ids":["mtg-abc123"]}"#),
        200,
    );
    assert_eq!(parse(&bytes)["ready"], json!(false));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// card_art: handle_post known-card gate — empty card_cache → 404 Unknown
// card from the get_async checkout (before any image decode).
#[test]
fn card_art_post_known_gate_checkout_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "POST",
        "/api/card-art/abc123",
        "catalog_art",
        Some("sess-1"),
        Some("application/json"),
        Some("{}"),
        404,
    );
    assert_eq!(parse(&bytes)["error"], json!("Unknown card"));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// backup: POST createBackup (admin) — the create_backup checkout moved to
// get_async; the backup itself lands beside the seeded DB in the temp dir.
#[test]
fn backup_create_checkout_does_not_park_the_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "POST",
        "/api/admin/backups",
        "manabox_backup",
        Some("sess-2"),
        Some("application/json"),
        Some("{}"),
        201,
    );
    let v = parse(&bytes);
    assert!(v["file"].is_string(), "backup body: {}", String::from_utf8_lossy(&bytes));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// moxfield: handle_delete_author → remove_author's opening checkout (tx
// begin, author lookup miss, rollback) — 404 without any network leg.
#[test]
fn moxfield_remove_author_checkout_does_not_park_the_runtime_worker() {
    std::env::set_var("SCRYBOX_DISABLE_MOXFIELD_SCHEDULER", "1");
    let (_s, bytes, pool) = parked_probe("DELETE", "/api/moxfield/authors/999", "moxfield", Some("sess-1"), None, None, 404);
    assert_eq!(parse(&bytes)["error"], json!("Moxfield author not found"));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// contended_probe: parking harness for POST-AUTH checkouts (iter-61).
// ---------------------------------------------------------------------------

/// Moxfield mock on a DEDICATED runtime thread — unaffected by a parked
/// request worker, so the RED case's curl round trip still completes and
/// the handler reaches its contended checkout. /v2/users/search-sfw echoes
/// the `filter` query param as the single entry's userName, with a 20 ms
/// server-side delay: that is the yield window the grabber task uses to
/// seize the connection auth just returned. /v2/decks/search-sfw answers
/// one empty page (the sync_decklist summaries leg).
fn spawn_mfx_mock() -> std::net::SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();
    std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let app = axum::Router::new().fallback(|req: axum::extract::Request| async move {
                let path = req.uri().path().to_string();
                let query = req.uri().query().unwrap_or("").to_string();
                let json = |status: axum::http::StatusCode, body: &Value| -> axum::http::Response<axum::body::Body> {
                    axum::http::Response::builder()
                        .status(status)
                        .header(axum::http::header::CONTENT_TYPE, "application/json")
                        .body(axum::body::Body::from(body.to_string()))
                        .unwrap()
                };
                match path.as_str() {
                    "/v2/users/search-sfw" => {
                        let filter = query
                            .split('&')
                            .find_map(|kv| kv.strip_prefix("filter="))
                            .unwrap_or("")
                            .to_string();
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        json(
                            axum::http::StatusCode::OK,
                            &json!({"data": [{"userName": filter, "displayName": "Mock User"}]}),
                        )
                    }
                    "/v2/decks/search-sfw" => {
                        json(axum::http::StatusCode::OK, &json!({"data": [], "totalPages": 1}))
                    }
                    _ => json(
                        axum::http::StatusCode::NOT_FOUND,
                        &json!({"status_code": 404, "mock": path}),
                    ),
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let _ = tx.send(listener.local_addr().unwrap());
            axum::serve(listener, app).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

/// Parking harness for POST-AUTH handler-local checkouts. live_max=1, but
/// instead of a from-the-start holder (whose slot the auth hop itself
/// would consume, making the body checkout unobservable — see the section
/// comment), an IDLE-ONLY grabber task (pool.try_get_idle: never opens,
/// never waits on the cap, so it cannot starve the auth hop) spins at
/// ~1 ms until the handler RETURNS a connection to the pool — the yield
/// window opened by the Moxfield get_user leg — and holds that connection
/// until the 300 ms release timer. The next body checkout therefore faces
/// an empty idle list at the live cap: sync pool.get() parks the worker
/// (the releasing task needs the parked worker — recv_timeout fires),
/// get_async() hops to the blocking pool and answers. STANDALONE (empty
/// node_upstream); rust_routes pins exactly the group under test.
#[allow(clippy::too_many_arguments)]
fn contended_probe(
    method: &str,
    uri: &str,
    group: &str,
    token: Option<&'static str>,
    content_type: Option<&'static str>,
    body: Option<&'static str>,
    expected: u16,
) -> (u16, Vec<u8>, Arc<scrybox_server::db::DbPool>) {
    let addr = spawn_mfx_mock();
    let dir = std::env::temp_dir().join(format!(
        "scrybox-contendedb-{}-{}",
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
            st.mfx = Arc::new(scrybox_server::moxfield_api::MfxApi {
                base_url: format!("http://{addr}"),
                curl_bin: "curl".into(),
                exec_timeout: std::time::Duration::from_secs(35),
            });
            let r = app(st);
            // Pristine start: a boot-time pooled connection would be
            // grabbed BEFORE the auth window and serialize the whole
            // request behind the release timer (vacuous, not parking).
            assert_eq!(pool_for_rt.open_count(), 0, "boot must not open a pooled connection");

            // The idle-only grabber (holds the seized conn to the timer).
            {
                let pool2 = pool_for_rt.clone();
                tokio::spawn(async move {
                    loop {
                        if let Some(held) = pool2.try_get_idle() {
                            let _ = release_rx.await;
                            drop(held);
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                });
            }

            let mut b = axum::http::Request::builder().method(method_owned.as_str()).uri(uri_owned.as_str());
            if let Some(tok) = token {
                b = b.header("authorization", format!("Bearer {tok}"));
            }
            if let Some(ct) = content_type {
                b = b.header("content-type", ct);
            }
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

    // Free the grabber once the contended checkout has had time to occur
    // (mock leg is 20 ms + curl spawn margin). Pre-fix the worker itself
    // is parked on the cap, so this release can never run and the pin
    // dies at the recv_timeout below.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = release_tx.send(());

    let (status, bytes) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|_| {
        panic!(
            "{method} {uri} never completed: the post-auth checkout parked the runtime worker on \
             the DbPool live cap (synchronous pool.get() inline on the worker while the grabber \
             held the only slot)"
        )
    });
    assert_eq!(status, expected, "{method} {uri} answered {status}, expected {expected}; body: {}", String::from_utf8_lossy(&bytes));
    (status, bytes, pool)
}

// moxfield: handle_add_author — existing-author branch: get_user over the
// mock (20 ms window; the grabber seizes the returned auth conn) THEN the
// existing-lookup checkout, then the whole sync_decklist fold. Reverting
// any fold checkout to sync pool.get() parks the worker at the first one
// the grabber covers.
#[test]
fn moxfield_add_author_checkout_does_not_park_the_runtime_worker() {
    std::env::set_var("SCRYBOX_DISABLE_MOXFIELD_SCHEDULER", "1");
    let (_s, bytes, pool) = contended_probe(
        "POST",
        "/api/moxfield/authors",
        "moxfield",
        Some("sess-1"),
        Some("application/json"),
        Some(r#"{"username":"mockuser"}"#),
        201,
    );
    let v = parse(&bytes);
    assert_eq!(v["ok"], json!(true), "add_author body: {}", String::from_utf8_lossy(&bytes));
    assert_eq!(v["already_tracked"], json!(true), "existing branch expected");
    assert_eq!(v["author"], json!("mockuser"));
    assert!(
        pool.blocking_fallbacks() >= 2,
        "post-auth checkout never hopped (blocking_fallbacks={}): the grabber never engaged — vacuous pin",
        pool.blocking_fallbacks()
    );
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// moxfield: POST /api/moxfield/authors/1/sync-decklist — sync_decklist's
// author lookup pops the auth conn, the get_user window lets the grabber
// seize it, then the author UPDATE checkout faces the contended cap
// (+ empty summaries leg + stamps fold to completion).
#[test]
fn moxfield_sync_decklist_checkout_does_not_park_the_runtime_worker() {
    std::env::set_var("SCRYBOX_DISABLE_MOXFIELD_SCHEDULER", "1");
    let (_s, bytes, pool) = contended_probe(
        "POST",
        "/api/moxfield/authors/1/sync-decklist",
        "moxfield",
        Some("sess-1"),
        None,
        None,
        200,
    );
    let v = parse(&bytes);
    assert_eq!(v["ok"], json!(true), "sync-decklist body: {}", String::from_utf8_lossy(&bytes));
    assert_eq!(v["author"], json!("mockuser"));
    assert_eq!(v["decks_created"], json!(0));
    assert!(
        pool.blocking_fallbacks() >= 2,
        "post-auth checkout never hopped (blocking_fallbacks={}): the grabber never engaged — vacuous pin",
        pool.blocking_fallbacks()
    );
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}
