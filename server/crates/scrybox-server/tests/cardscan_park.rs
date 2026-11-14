//! LANE B — cardscan M5 park pins (REVIEW M5 family, iter-83/84 marketplace
//! shape).
//!
//! handle_frame / handle_cards / handle_choices / handle_scan previously ran
//! synchronous authenticate() + pool.get() + hydrate ON the runtime worker
//! after their awaits (prefetch_ids). DbPool::get() parks on the live-cap
//! Condvar; a worker parked there starves the whole runtime (iter-82
//! ALL_mixed freeze). Each pin runs the FULL standalone arm (stub sidecar
//! answers the scan leg, every card pre-cached) while the pool's single live
//! slot is held by a runtime task: if any handler step parks the worker, the
//! release task can never run and the request never completes — the
//! red-before-green signal. Fix shape: authenticate_async + ONE
//! spawn_blocking hop per DB leg (marketplace iter-83/84).

use std::sync::{Arc, OnceLock};

use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

mod mock_scryfall;

const UUID_A: &str = "aaaaaaaa-1111-1111-1111-111111111111";
const UUID_B: &str = "bbbbbbbb-2222-2222-2222-222222222222";
const UUID_D: &str = "dddddddd-4444-4444-4444-444444444444";

/// Mock sidecar (CARDSCAN_URL is boot-snapshotted once per test binary,
/// mirroring the cardscan.rs module-load const): the ok arms of
/// /api/scan-frame and /api/scan-scene, ok-everything elsewhere.
fn mock_sidecar() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let std_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_l.set_nonblocking(true).unwrap();
        let addr = std_l.local_addr().unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(std_l).unwrap();
                let app = axum::Router::new().fallback(|req: axum::extract::Request| async move {
                    let path = req.uri().path().to_string();
                    let _body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
                    let j = |s: String| {
                        (
                            axum::http::StatusCode::OK,
                            [(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"))],
                            axum::body::Body::from(s),
                        )
                    };
                    match path.as_str() {
                        "/api/scan-frame" => j(format!(
                            r#"{{"ok":true,"frame":"data:image/jpeg;base64,Zg==","candidates":[{{"number":"7","box":0,"quad":[[0,0],[1,0],[1,1],[0,1]],"eligible":true,"status":"ok"}}],"results":[{{"ok":true,"scene_number":7,"title":"Frame Card","card":{{"id":"{UUID_A}"}},"cached":true}}],"timings":{{"ocr_ms":5}}}}"#
                        )),
                        "/api/scan-scene" => j(format!(
                            r#"{{"ok":true,"results":[{{"ok":true,"scene_number":7,"title":"Frame Card","card":{{"id":"{UUID_A}"}}}}],"timings":{{"match_ms":2}}}}"#
                        )),
                        _ => j(r#"{"ok":true}"#.to_string()),
                    }
                });
                axum::serve(l, app).await.unwrap();
            });
        });
        format!("http://{addr}")
    })
}

fn seed(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("bindarr.db");
    std::fs::write(&path, []).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id,name,supertype,subtypes,types,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency,language)
           VALUES
             ('mtg-aaaaaaaa-1111-1111-1111-111111111111','Frame Card','MTG','[\"Creature\"]','[\"Goblin\"]','Common','xct','X Set','7','img/a',1.0,2.5,0,0,0,2,'[\"R\"]','USD','English'),
             ('mtg-bbbbbbbb-2222-2222-2222-222222222222','Twin B','MTG','[\"Creature\"]','[\"Goblin\"]','Common','xct','X Set','8','img/b',1.0,2.5,0,0,0,2,'[\"R\"]','USD','English'),
             ('mtg-dddddddd-4444-4444-4444-444444444444','Scene Card','MTG','[\"Instant\"]','[\"Drake\"]','Rare','xct','X Set','11','img/d',1.0,9.9,0,0,0,3,'[\"U\"]','USD','English');",
    )
    .unwrap();
    path
}

/// Shared harness (marketplace parked_probe_scoped shape): current-thread
/// runtime whose DbPool has live_max=1 with the ONLY live slot held by a
/// runtime task that frees it on `release`. The server runs STANDALONE
/// (empty node_upstream) so the scan legs are answered by the stub sidecar
/// and hydration runs fully on the Rust side. Returns (status, body, pool).
#[allow(clippy::type_complexity)]
fn parked_probe(
    method: &str,
    uri: &str,
    content_type: Option<&'static str>,
    body: Option<&'static str>,
    expected: u16,
) -> (u16, Vec<u8>, Arc<scrybox_server::db::DbPool>) {
    use std::sync::mpsc;
    use tokio::sync::Notify;

    std::env::set_var("CARDSCAN_URL", mock_sidecar());
    let dir = std::env::temp_dir().join(format!("scrybox-cspark-{}", scrybox_server::auth_support::uuid_v4()));
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
    let release = Arc::new(Notify::new());
    let release_for_rt = release.clone();
    let pool_for_rt = pool.clone();
    let db_for_rt = db_path.clone();
    let method_owned = method.to_string();
    let uri_owned = uri.to_string();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let cfg = Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                // Standalone: node_upstream empty → fetch_on_miss=false, the
                // whole scan+hydrate fold must complete on the Rust side.
                node_upstream: String::new(),
                static_dir: None,
                rust_routes: ["cardscan".to_string(), "scan_match".to_string()].into_iter().collect(),
                db_path: Some(db_for_rt),
                sidecar_url: None,
            };
            let mut st = AppState::new(cfg);
            st.db = Some(pool_for_rt.clone());
            let (addr, _ms) = mock_scryfall::spawn_mock().await;
            st.scry = Arc::new(mock_scryfall::scry_for(addr, 0.0));
            let r = app(st);

            // Occupy the pool's single live slot from a runtime task: the
            // ONLY way to free it is another task on this same worker.
            let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
            {
                let pool2 = pool_for_rt.clone();
                let rel = release_for_rt.clone();
                tokio::spawn(async move {
                    let held = pool2.get().unwrap();
                    let _ = held_tx.send(());
                    rel.notified().await;
                    drop(held);
                });
            }
            held_rx.await.unwrap();

            let mut b = axum::http::Request::builder()
                .method(method_owned.as_str())
                .uri(uri_owned.as_str())
                .header("authorization", "Bearer sess-1")
                // ConnectInfo for the scan-match handler (oneshot bypasses the
                // listener injector; the extractor reads this extension).
                .extension(axum::extract::ConnectInfo("127.0.0.1:9".parse::<std::net::SocketAddr>().unwrap()));
            if let Some(ct) = content_type {
                b = b.header("content-type", ct);
            }
            let req = b.body(axum::body::Body::from(body.unwrap_or(""))).unwrap();
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
    release.notify_waiters();

    let (status, bytes) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|_| {
        panic!(
            "{method} {uri} never completed: the handler parked the runtime worker on the DbPool live cap \
             (synchronous authenticate/pool.get()/hydrate inline on the worker)"
        )
    });
    assert_eq!(status, expected, "{method} {uri} answered {status}, expected {expected}");
    (status, bytes, pool)
}

fn img(n: usize) -> String {
    "J".repeat(n)
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

#[test]
fn frame_does_not_park_the_runtime_worker_at_the_live_cap() {
    // Full standalone fold: auth → read body → sidecar /api/scan-frame →
    // prefetch_ids (fetch_card_by_id cache read) → hydrate loop, all with
    // the pool's single live slot held by a runtime task.
    let (_s, bytes, pool) = parked_probe("POST", "/api/cardscan/frame", Some("image/jpeg"), Some(Box::leak(img(200).into_boxed_str())), 200);
    let v = parse(&bytes);
    assert_eq!(v["ok"], json!(true), "frame ok body expected, got {v}");
    assert_eq!(v["results"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(v["results"][0]["card"]["name"], json!("Frame Card"), "hydrated card row expected: {v}");
    assert_eq!(v["results"][0]["cached"], json!(true));
    assert_eq!(v["candidates"].as_array().map(|a| a.len()), Some(1));
    assert!(v["timings"].get("proxy_ms").is_some(), "proxy_ms timing expected: {v}");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn cards_does_not_park_the_runtime_worker_at_the_live_cap() {
    let body = format!(
        r#"{{"results":[{{"scryfallId":"{UUID_A}","number":7,"title":"Frame Card","via":"ocr"}}]}}"#
    );
    let (_s, bytes, pool) = parked_probe("POST", "/api/cardscan/cards", Some("application/json"), Some(Box::leak(body.into_boxed_str())), 200);
    let v = parse(&bytes);
    assert_eq!(v["ok"], json!(true), "cards ok body expected, got {v}");
    assert_eq!(v["results"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(v["results"][0]["card"]["name"], json!("Frame Card"), "hydrated card row expected: {v}");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn choices_does_not_park_the_runtime_worker_at_the_live_cap() {
    let body = format!(r#"{{"ids":["{UUID_A}","{UUID_B}"]}}"#);
    let (_s, bytes, pool) = parked_probe("POST", "/api/cardscan/choices", Some("application/json"), Some(Box::leak(body.into_boxed_str())), 200);
    let v = parse(&bytes);
    assert_eq!(v["ok"], json!(true), "choices ok body expected, got {v}");
    let cards = v["cards"].as_array().cloned().unwrap_or_default();
    assert_eq!(cards.len(), 2, "two picker rows expected: {v}");
    assert_eq!(cards[0]["name"], json!("Frame Card"));
    assert_eq!(cards[1]["name"], json!("Twin B"));
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn status_does_not_park_the_runtime_worker_at_the_live_cap() {
    // iter-55 same-class pins: handle_status / handle_detect / handle_telemetry
    // ran synchronous authed() (authenticate -> pool.get()) inline on the
    // runtime worker. Full standalone arm with the pool's single live slot held.
    std::env::set_var("SCAN_TELEMETRY_DIR", std::env::temp_dir().join("scrybox-cspark-telemetry-dir"));
    let (_s, bytes, pool) = parked_probe("GET", "/api/cardscan/status", None, None, 200);
    let v = parse(&bytes);
    assert_eq!(v["ok"], json!(true), "status ok=true expected (sidecar /api/health 200): {v}");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn detect_does_not_park_the_runtime_worker_at_the_live_cap() {
    // Full standalone arm: auth → capped body read → parse_raw_image →
    // sidecar /api/detect-cards passthrough.
    let (_s, bytes, pool) = parked_probe("POST", "/api/cardscan/detect", Some("image/jpeg"), Some(Box::leak(img(200).into_boxed_str())), 200);
    let v = parse(&bytes);
    assert_eq!(v["ok"], json!(true), "detect passthrough body expected: {v}");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn telemetry_does_not_park_the_runtime_worker_at_the_live_cap() {
    // Full standalone arm (NO sidecar leg): auth → body → record fold →
    // JSONL append via the existing spawn_blocking file hop.
    std::env::set_var("SCAN_TELEMETRY_DIR", std::env::temp_dir().join("scrybox-cspark-telemetry-dir"));
    let body = r#"{"session":"s-1","records":[{"ev":"tap"}]}"#;
    let (_s, bytes, pool) = parked_probe("POST", "/api/cardscan/telemetry", Some("application/json"), Some(body), 204);
    assert!(bytes.is_empty(), "204 carries no body, got {bytes:?}");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn scan_match_does_not_park_the_runtime_worker_at_the_live_cap() {
    // iter-55 same-class pin for handle_scan_match (cvscan.rs): synchronous
    // authenticate AND the hydrate loop's pool.get() ran inline on the
    // runtime worker. This pin fires the pre-ort fold (no built catalog /
    // no ORT runtime in the test env → Node's own 503-notBuilt / 500
    // "Scan match failed" arms): the auth checkout is the per-request park
    // vector and must go through the async hop. The hydrate-loop hop is
    // same-class hardening behind it (red-flagged identically by this
    // harness whenever the pool is exhausted before the loop).
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(vec![7u8; 200]);
    let body = format!(r#"{{"image":"data:image/jpeg;base64,{b64}"}}"#);
    let (_s, bytes, pool) = parked_probe("POST", "/api/scan-match", Some("application/json"), Some(Box::leak(body.into_boxed_str())), 200);
    let v = parse(&bytes);
    assert!(
        v["error"].is_string() && (v["notBuilt"].as_bool() == Some(true) || v["error"] == serde_json::json!("Scan match failed")),
        "expected the pre-ort 503-notBuilt or 500 scan-failed fold, got {bytes:?}"
    );
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}

#[test]
fn scan_does_not_park_the_runtime_worker_at_the_live_cap() {
    let body = format!(r#"{{"cards":[{{"scryfallId":"{UUID_D}"}}]}}"#);
    let (_s, bytes, pool) = parked_probe("POST", "/api/cardscan", Some("application/json"), Some(Box::leak(body.into_boxed_str())), 200);
    let v = parse(&bytes);
    assert_eq!(v["ok"], json!(true), "scan ok body expected, got {v}");
    assert_eq!(v["results"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(v["results"][0]["card"]["name"], json!("Frame Card"), "hydrated card row expected: {v}");
    assert_eq!(pool.live_count(), 0, "every checkout must be returned to the pool");
}
