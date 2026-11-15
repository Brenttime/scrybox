//! M5 live-slot parking pins for the iter-18 pool.get() leg sweep
//! (Lane-A M5 family, marketplace iter-83/84 / sets_park / authsweep_park
//! shape).
//!
//! Before this iteration these Lane-A request paths still ran their DB
//! legs (checkouts, SELECTs, UPDATE/INSERT/DELETE tx legs) INLINE on the
//! runtime worker: DbPool::get() parks on the live-cap Condvar, and a
//! worker parked there starves the whole runtime — the releasing task can
//! never run and the request never completes (iter-82/83 class). Each pin
//! holds the pool's single live slot from a runtime task and asserts the
//! request still completes within 10s.
//! pins hold BOTH pool permits from a runtime task and assert the request
//! still completes within 10s (a worker that parks on the live-cap Condvar
//! starves the releasing task and the request never completes — iter-82/83
//! class). TRUE RED (iter-19): the harness arms DbPool's debug-gated
//! after_checkout_hook as a checkout-thread tripwire after the holders take
//! their permits — any sync pool.get() executing on the named
//! "parked-runtime-worker" thread panics (CHECKOUT-ON-RUNTIME-WORKER), so
//! neutering ANY leg back to an inline worker checkout reddens its pin
//! immediately, no timeout race. get_async's checkout runs on the blocking
//! pool via get_conn and never fires the tripwire.
//!
//! Hermetic folds only: none of these routes touch the network. The
//! precons-import write leg and the secret-lair preview/add legs sit
//! behind scryfall fetches (no mock exists in this harness) — their park
//! pins are flagged for the next tick together with the remaining
//! secret_lair add leg.

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
        "CREATE TABLE IF NOT EXISTS app_settings (id INTEGER PRIMARY KEY,
            public_base_url TEXT DEFAULT '',
            price_refresh_days INTEGER DEFAULT 1,
            scan_exclude_tokens INTEGER DEFAULT 0,
            scan_exclude_art_cards INTEGER DEFAULT 0,
            scan_exclude_jumpstart INTEGER DEFAULT 0,
            scan_exclude_promos INTEGER DEFAULT 0,
            setup_complete INTEGER DEFAULT 0,
            moxfield_decklist_interval_min INTEGER DEFAULT 60,
            moxfield_content_interval_min INTEGER DEFAULT 1
         );
         INSERT INTO app_settings (id, public_base_url, price_refresh_days,
             scan_exclude_tokens, scan_exclude_art_cards, scan_exclude_jumpstart,
             scan_exclude_promos, setup_complete, moxfield_decklist_interval_min,
             moxfield_content_interval_min)
           VALUES (1, '', 1, 1, 0, 0, 0, 1, 120, 5);
         CREATE TABLE IF NOT EXISTS card_lists (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             name TEXT NOT NULL,
             description TEXT DEFAULT '',
             accent_color TEXT DEFAULT '#10b981',
             icon TEXT,
             user_id INTEGER NOT NULL,
             sort_order INTEGER DEFAULT 0,
             created_at TEXT DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS list_cards (
             list_id INTEGER NOT NULL,
             card_id INTEGER NOT NULL,
             quantity INTEGER NOT NULL DEFAULT 1,
             PRIMARY KEY (list_id, card_id)
         );
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'adm','x','admin','tok-a',0,NULL),
                  (2,'victim','x','member','tok-v',0,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES
             ('sess-admin', 1, '2099-01-01 00:00:00');",
    )
    .unwrap();
    path
}

/// Shared harness (sets_park/authsweep_park parked_probe shape):
/// current-thread runtime whose DbPool has live_max=1 with the ONLY live
/// slot held by a runtime task, released via oneshot. STANDALONE (empty
/// node_upstream). ConnectInfo extension supplied (lists_writes handlers
/// extract it — cardscan_park:190 precedent).
/// Returns (status, body, pool).
#[allow(clippy::too_many_arguments)]
fn parked_probe(
    method: &'static str,
    uri: &'static str,
    group: &'static str,
    token: Option<&'static str>,
    ct: Option<&'static str>,
    body: Option<&'static str>,
    expected: u16,
) -> (u16, Vec<u8>, Arc<scrybox_server::db::DbPool>) {
    let dir = std::env::temp_dir().join(format!("scrybox-legspark-{}", scrybox_server::auth_support::uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = seed(&dir);

    // Two-permit hold protocol. Both permits are held by ONE runtime task;
    // max=0 means put() CLOSES returned conns (an idle pop can never smuggle
    // a checkout past the cap). The request's auth get_async therefore waits
    // on the live-cap Condvar ON THE BLOCKING POOL (get_async never parks the
    // worker) until the 300ms release frees the holding task — a handler
    // that runs ANY checkout (auth included) inline on the worker instead
    // dies there: the release lands but the holding task can never run.
    // RED-VERIFICATION (iter-19, measured): neutering a leg fold back to an
    // inline pool.get() does NOT deadlock the worker (auth's get_async only
    // reaches its post-release window after the single CV wait is already
    // freed, so an inline leg checkout would be instant), but it DOES fire
    // the checkout tripwire armed below — the pin reddens on the CHECKOUT-
    // ON-RUNTIME-WORKER panic. Pins additionally redden on any sync-auth
    // regression (auth parks the worker at the cap before release).
    let pool = Arc::new(
        scrybox_server::db::DbPool::open_with(
            &db_path,
            scrybox_server::db::PoolConfig { max: 0, cache_kb: 1024, mmap_kb: 0, live_max: 2 },
        )
        .unwrap(),
    );
    pool.warm(3);

    let (tx, rx) = mpsc::channel::<(u16, Vec<u8>)>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let pool_for_rt = pool.clone();
    let db_for_rt = db_path.clone();

    std::thread::Builder::new()
        .name("parked-runtime-worker".to_string())
        .spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let cfg = Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                node_upstream: String::new(),
                static_dir: None,
                rust_routes: [group.to_string()].into_iter().collect(),
                db_path: Some(db_for_rt),
                sidecar_url: None,
            };
            let mut st = AppState::new(cfg);
            st.db = Some(pool_for_rt.clone());
            if group == "rules" {
                // Fresh rules_mem (fetched=now < MAX_AGE): get_rules(false)
                // answers from the memo — the keywords route reaches
                // build_keywords (and its mine_reminders card_cache scan
                // with the folded checkout) with the network never touched.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;
                *st.rules_mem.lock().unwrap() = Some(json!({"fetched": now, "rules": []}));
            }
            let r = app(st);

            // Occupy BOTH pool permits from ONE runtime task: the only way
            // to free them is another task on this same worker (the auth
            // leg never touches these — it idle-pops the third warm conn).
            let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
            {
                let pool2 = pool_for_rt.clone();
                tokio::spawn(async move {
                    let h1 = pool2.get().unwrap_or_else(|e| panic!("HELD-CHECKOUT-FAIL {e:?}"));
                    let h2 = pool2.get().unwrap_or_else(|e| panic!("HELD-CHECKOUT-FAIL {e:?}"));
                    let _ = held_tx.send(());
                    let _ = release_rx.await;
                    drop((h1, h2));
                });
            }
            held_rx.await.unwrap();

            // iter-19 TRUE RED for the leg class: once the holders hold both
            // permits, arm DbPool's debug-gated after_checkout_hook as a
            // checkout-thread tripwire. Any SYNC pool.get() executing on
            // THIS thread while the request is in flight is an inline leg
            // on the parked worker (the iter-82/83 starvation class) —
            // panic immediately instead of waiting for the 10s timeout.
            // get_async never fires it (its checkout runs on the blocking
            // pool via get_conn, bypassing get()), and the holders checked
            // out before the hook was armed.
            pool_for_rt.set_after_checkout_hook(Some(std::sync::Arc::new(|| {
                if std::thread::current().name() == Some("parked-runtime-worker") {
                    panic!(
                        "CHECKOUT-ON-RUNTIME-WORKER: sync pool.get() ran inline \
                         on the parked runtime worker (M5 regression)"
                    );
                }
            })));

            let mut b = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                // ConnectInfo for the lists_writes handlers (oneshot bypasses
                // the listener — cardscan_park:192 precedent).
                .extension(axum::extract::ConnectInfo(
                    std::net::SocketAddr::from(([127, 0, 0, 1], 54321)),
                ));
            if let Some(tok) = token {
                b = b.header("authorization", format!("Bearer {tok}"));
            }
            if let Some(c) = ct {
                b = b.header("content-type", c);
            }
            let req = match body {
                Some(bd) => b.body(axum::body::Body::from(bd.to_string())).unwrap(),
                None => b.body(axum::body::Body::empty()).unwrap(),
            };
            let resp = r.oneshot(req).await.unwrap();
            let status = resp.status().as_u16();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
            let _ = tx.send((status, bytes));
        });
    })
    .unwrap();

    // Give the request task time to reach its cap wait, then free the pool.
    // Pre-fix this never helps: the worker parked on the Condvar cannot run
    // the releasing task either, so recv_timeout below expires.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = release_tx.send(());

    let (status, bytes) = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|e| {
            match e {
                std::sync::mpsc::RecvTimeoutError::Disconnected => panic!(
                    "{method} {uri}: the parked-worker thread died mid-request — with the \
                     checkout tripwire armed this means an inline sync pool.get() ran on the \
                     runtime worker (see CHECKOUT-ON-RUNTIME-WORKER panic above)"
                ),
                std::sync::mpsc::RecvTimeoutError::Timeout => panic!(
                    "{method} {uri} never completed: the handler parked the runtime worker on the \
                     DbPool live cap (synchronous pool.get() inline on the worker)"
                ),
            }
        });
    assert_eq!(status, expected, "{method} {uri} answered {status}, expected {expected}: {}", String::from_utf8_lossy(&bytes));
    (status, bytes, pool)
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

// ---------------------------------------------------------------------------
// GET /api/admin/users — full read region hop (USERS_SQL + STATS_SQL loop)
// ---------------------------------------------------------------------------

#[test]
fn admin_users_get_never_parks_a_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/admin/users", "admin_users", Some("sess-admin"), None, None, 200);
    let j = parse(&bytes);
    assert_eq!(j.as_array().map(|a| a.len()), Some(2), "both seeded users must come back through the hop");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// POST /api/admin/users — per-leg hops (SELECT lookup / hash / INSERT)
// ---------------------------------------------------------------------------

#[test]
fn admin_users_post_taken_never_parks_a_runtime_worker() {
    // Duplicate username: the lookup hop answers 400 on the held-slot pool;
    // the INSERT leg never runs — exactly the pre-hops order.
    let (_s, bytes, pool) = parked_probe(
        "POST", "/api/admin/users", "admin_users", Some("sess-admin"),
        Some("application/json"),
        Some(r#"{"username":"victim","password":"password123"}"#), 400,
    );
    assert_eq!(parse(&bytes), json!({"error": "Username is already taken"}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// PUT /api/admin/users/:id — per-leg hops (SELECT lookup / hash / UPDATEs)
// ---------------------------------------------------------------------------

#[test]
fn admin_users_put_role_update_never_parks_a_runtime_worker() {
    // No password arm: lookup hop (user 2 exists) then the UPDATE hop;
    // same 200 body as the inline era.
    let (_s, bytes, pool) = parked_probe(
        "PUT", "/api/admin/users/2", "admin_users", Some("sess-admin"),
        Some("application/json"), Some(r#"{"role":"admin"}"#), 200,
    );
    assert_eq!(parse(&bytes), json!({"message": "User updated successfully"}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// DELETE /api/admin/users/:id — whole-tx hop (pragma + BEGIN IMMEDIATE)
// ---------------------------------------------------------------------------

#[test]
fn admin_users_delete_never_parks_a_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("DELETE", "/api/admin/users/2", "admin_users", Some("sess-admin"), None, None, 200);
    assert_eq!(
        parse(&bytes)["message"].as_str().map(str::to_string),
        Some("User \"victim\" and all their card collections have been permanently deleted.".to_string())
    );
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// GET /api/settings — get_effective_settings hop
// ---------------------------------------------------------------------------

#[test]
fn settings_get_never_parks_a_runtime_worker() {
    let (_s, bytes, pool) = parked_probe("GET", "/api/settings", "settings", Some("sess-admin"), None, None, 200);
    let j = parse(&bytes);
    assert_eq!(j["price_refresh_days"], json!(1), "seeded row must read back through the hop");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// PUT /api/settings — whole write region hop (checkout + arms + re-read)
// ---------------------------------------------------------------------------

#[test]
fn settings_put_never_parks_a_runtime_worker() {
    // price_refresh_days gate passes, UPDATE runs, after-update re-read
    // answers through the SAME hop; early-400 order untouched.
    let (_s, bytes, pool) = parked_probe(
        "PUT", "/api/settings", "settings", Some("sess-admin"),
        Some("application/json"), Some(r#"{"price_refresh_days":5}"#), 200,
    );
    assert_eq!(parse(&bytes)["price_refresh_days"], json!(5));
    assert_eq!(pool.live_count(), 0, "every checkout (checkout leg + re-read) must be returned");
}

// ---------------------------------------------------------------------------
// POST /api/lists — create_list hop (INSERT + per-line find_cached_card)
// ---------------------------------------------------------------------------

#[test]
fn lists_create_never_parks_a_runtime_worker() {
    // No list_text: the hydration probe leg is skipped (Node skips the
    // batched fetch too), the INSERT leg runs on the held-slot pool.
    let (_s, bytes, pool) = parked_probe(
        "POST", "/api/lists", "lists_writes", Some("sess-admin"),
        Some("application/json"), Some(r#"{"name":"My List"}"#), 201,
    );
    let j = parse(&bytes);
    assert_eq!(j["message"], json!("List created"));
    assert_eq!(j["id"].as_i64().map(|i| i > 0), Some(true));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// PUT /api/lists/order — reorder_lists hop (BEGIN IMMEDIATE + UPDATE loop)
// ---------------------------------------------------------------------------

#[test]
fn lists_order_never_parks_a_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "PUT", "/api/lists/order", "lists_writes", Some("sess-admin"),
        Some("application/json"), Some(r#"{"ids":[7,8,9]}"#), 200,
    );
    assert_eq!(parse(&bytes), json!({"ok": true}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// PUT /api/lists/:id — update_list hop (UPDATE … 0 rows -> 404)
// ---------------------------------------------------------------------------

#[test]
fn lists_update_missing_never_parks_a_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "PUT", "/api/lists/999", "lists_writes", Some("sess-admin"),
        Some("application/json"), Some(r#"{"name":"Ghost"}"#), 404,
    );
    assert_eq!(parse(&bytes), json!({"error": "List not found or unauthorized"}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// DELETE /api/lists/:id — delete_list hop (SELECT + cascade tx)
// ---------------------------------------------------------------------------

#[test]
fn lists_delete_missing_never_parks_a_runtime_worker() {
    let (_s, bytes, pool) = parked_probe(
        "DELETE", "/api/lists/999", "lists_writes", Some("sess-admin"), None, None, 404,
    );
    assert_eq!(parse(&bytes), json!({"error": "List not found or unauthorized"}));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

// ---------------------------------------------------------------------------
// GET /api/rules/keywords — build_keywords hop (mine_reminders card_cache
// scan behind an inline pool.get() before the fold)
// ---------------------------------------------------------------------------

#[test]
fn rules_keywords_never_parks_a_runtime_worker() {
    // Fresh rules_mem memo (see harness): the route skips get_rules' fetch,
    // kw_mem is empty so build_keywords runs — mine_reminders' checkout
    // happens inside the hop on the held-slot pool. Empty rules => empty
    // keywords, served 200.
    let (_s, bytes, pool) = parked_probe("GET", "/api/rules/keywords", "rules", Some("sess-admin"), None, None, 200);
    let j = parse(&bytes);
    assert_eq!(j["keywords"], json!([]));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}
