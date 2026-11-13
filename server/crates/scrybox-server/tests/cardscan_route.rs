//! LANE B unit 6a — /api/cardscan/* route tests (OCR sidecar proxy twin).
//!
//! Mock sidecar on localhost (paths /api/health, /api/detect-cards,
//! /api/scan-frame, /api/scan-scene, /api/scene-complete), fake Node with
//! the x-node marker. CARDSCAN_URL is boot-snapshotted once per test binary
//! (OnceLock, mirroring the module-load const), so all tests share one
//! mock; the down/sidecar-unreachable arms live in
//! tests/cardscan_sidecar_down.rs. Tests run in parallel over one shared
//! hit log: every leg carries a body/marker unique to its test.

use std::{
    collections::BTreeSet,
    net::SocketAddr,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

#[derive(Clone, Debug)]
struct Hit {
    method: String,
    path: String,
    ct: Option<String>,
    body: String,
    scan_client: Option<String>,
    scan_session: Option<String>,
    scene_proto: Option<String>,
}

fn hits() -> &'static Mutex<Vec<Hit>> {
    static H: OnceLock<Mutex<Vec<Hit>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(Vec::new()))
}

/// Poison-tolerant lock. An assert that fails WHILE holding the hits guard
/// (the iter-11 frame_hydrates cascade) poisoned the shared mutex and every
/// sibling test's lock().unwrap() died with it — the recorded hits are still
/// valid state after a panic, so unwrap_or_else(into_inner) keeps one
/// failure from cascading into a suite-wide 502 storm.
fn hits_lock() -> std::sync::MutexGuard<'static, Vec<Hit>> {
    hits().lock().unwrap_or_else(|e| e.into_inner())
}

async fn hit_matches(pred: impl Fn(&Hit) -> bool) -> bool {
    for _ in 0..150 {
        if hits_lock().iter().any(&pred) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

fn no_hit(pred: impl Fn(&Hit) -> bool) -> bool {
    !hits_lock().iter().any(&pred)
}

fn img(n: usize) -> String {
    "J".repeat(n)
}

fn body_marker_junk(body: &str) -> bool {
    body.starts_with("JUNK")
}

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
                let app = Router::new().route(
                    "/{*rest}",
                    any(|req: Request| async move {
                        let (parts, body) = req.into_parts();
                        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                        let body_str = String::from_utf8_lossy(&bytes).into_owned();
                        let path = parts.uri.path_and_query().map(|s| s.as_str().to_string()).unwrap_or_default();
                        let g = |n: &str| parts.headers.get(n).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                        hits_lock().push(Hit {
                            method: parts.method.as_str().to_string(),
                            path: path.clone(),
                            ct: g("content-type"),
                            body: body_str.clone(),
                            scan_client: g("x-scan-client"),
                            scan_session: g("x-scan-session"),
                            scene_proto: g("x-scene-protocol"),
                        });
                        let j = |s: &'static str| {
                            (StatusCode::OK, [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], Body::from(s)).into_response()
                        };
                        match path.as_str() {
                            "/api/health" => {
                                if path.contains("junk=1") {
                                    // content-type json but unparseable body:
                                    // axios tryParse throws → generic error.
                                    return (
                                        StatusCode::OK,
                                        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
                                        Body::from("not json at all"),
                                    )
                                        .into_response();
                                }
                                j(r#"{"model":"cardscan-mock","ready":true}"#)
                            }
                            "/api/health503" => (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
                                Body::from(r#"{"state":"busy"}"#),
                            )
                                .into_response(),
                            "/api/detect-cards" => {
                                if body_marker_junk(&body_str) {
                                    return (
                                        StatusCode::OK,
                                        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
                                        Body::from("{broken"),
                                    )
                                        .into_response();
                                }
                                j(r#"{"cards":[{"box":[1,2,3,4]}]}"#)
                            }
                            "/api/detect-err" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                            "/api/scan-frame" => j(
                                r#"{"ok":true,"frame":"F","candidates":[{"number":1,"box":[0,0,1,1],"quad":true,"eligible":true,"status":"ok","junk":"drop-me"}],"results":[{"ok":true,"scene_number":"7","card":{"id":"aaaaaaaa-1111-1111-1111-111111111111"},"cached":true},{"ok":false,"error":"no footer","footer_ocr":{"choices":[{"id":"bbbbbbbb-2222-2222-2222-222222222222"},{"id":"cccccccc-3333-3333-3333-333333333333"}]}}],"timings":{"ocr_ms":9}}"#,
                            ),
                            "/api/scan-frame-notok" => j(r#"{"ok":false,"reason":"blur"}"#),
                            "/api/scan-scene" => j(
                                r#"{"ok":true,"results":[{"ok":true,"scene_number":"9","card":{"id":"dddddddd-4444-4444-4444-444444444444"},"footer_ocr":{"resolved_by":"footer"}}],"timings":{"scene_ms":4}}"#,
                            ),
                            "/api/scene-complete" => StatusCode::NO_CONTENT.into_response(),
                            _ => StatusCode::BAD_GATEWAY.into_response(),
                        }
                    }),
                );
                axum::serve(l, app).await.unwrap()
            });
        });
        format!("http://{addr}/") // trailing slash exercises the trim
    })
}

async fn spawn_fake_node() -> SocketAddr {
    let echo = any(|req: Request| async move {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let mut out = HeaderMap::new();
        out.insert(HeaderName::from_static("x-node"), HeaderValue::from_static("1"));
        (
            StatusCode::OK,
            out,
            axum::Json(json!({
                "method": parts.method.as_str(),
                "pq": parts.uri.path_and_query().unwrap().as_str(),
                "body": String::from_utf8_lossy(&body),
            })),
        )
            .into_response()
    });
    let health = axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({ "status": "ok" }))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

const UUID_A: &str = "aaaaaaaa-1111-1111-1111-111111111111";
const UUID_B: &str = "bbbbbbbb-2222-2222-2222-222222222222";
const UUID_C: &str = "cccccccc-3333-3333-3333-333333333333";
const UUID_D: &str = "dddddddd-4444-4444-4444-444444444444";

fn seeded_db(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("t.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id,name,supertype,subtypes,types,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency,language)
           VALUES
             ('mtg-aaaaaaaa-1111-1111-1111-111111111111','Frame Card','MTG','[\"Creature\"]','[\"Goblin\"]','Common','xct','X Set','7','img/a',1.0,2.5,0,0,0,2,'[\"R\"]','USD','English'),
             ('mtg-bbbbbbbb-2222-2222-2222-222222222222','Twin B','MTG','[\"Creature\"]','[\"Goblin\"]','Common','xct','X Set','8','img/b',1.0,2.5,0,0,0,2,'[\"R\"]','USD','English'),
             ('mtg-cccccccc-3333-3333-3333-333333333333','Twin C','MTG','[\"Creature\"]','[\"Goblin\"]','Common','xct','X Set','9','img/c',1.0,2.5,0,0,0,2,'[\"R\"]','USD','English'),
             ('mtg-dddddddd-4444-4444-4444-444444444444','Scene Card','MTG','[\"Instant\"]','[\"Drake\"]','Rare','xct','X Set','11','img/d',1.0,9.9,0,0,0,3,'[\"U\"]','USD','English');",
    )
    .unwrap();
    path
}

struct Harness {
    router: Router,
    db: PathBuf,
    _dir: tempfile::TempDir,
}

async fn harness(routes: &[&str]) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    std::env::set_var("CARDSCAN_URL", mock_sidecar());
    let node = spawn_fake_node().await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        db_path: Some(db.clone()),
        sidecar_url: None,
    };
    Harness { router: app(AppState::new(cfg)), db, _dir: dir }
}

async fn call(
    r: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut rb = Request::builder().method(method).uri(path);
    for &(k, v) in headers {
        rb = rb.header(k, v);
    }
    let req = rb.body(Body::from(body.unwrap_or("").to_string())).unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, headers, bytes)
}

const AUTH: [(&str, &str); 1] = [("authorization", "Bearer sess-1")];
const JSON_CT: [(&str, &str); 2] =
    [("authorization", "Bearer sess-1"), ("content-type", "application/json")];
const JPEG_CT: [(&str, &str); 2] =
    [("authorization", "Bearer sess-1"), ("content-type", "image/jpeg")];

fn json_body(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap()
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_is_401_and_no_sidecar_leg() {
    let h = harness(&["cardscan"]).await;
    let (st, _, b) = call(
        &h.router,
        "POST",
        "/api/cardscan/detect?t=auth401",
        &[("content-type", "image/jpeg")],
        Some(&img(200)),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(json_body(&b).get("error").is_some());
    assert!(no_hit(|x| x.path.contains("t=auth401")), "auth outranks the sidecar leg");
}

#[tokio::test]
async fn kill_switch_off_proxies_everything() {
    let h = harness(&[]).await;
    let (st, hd, b) = call(&h.router, "POST", "/api/cardscan/choices", &JSON_CT, Some(r#"{"ids":[]}"#)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd["x-node"], "1");
    assert_eq!(json_body(&b)["pq"], "/api/cardscan/choices");
    assert!(no_hit(|x| x.path == "/api/choices"));
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_ok_merges_sidecar_health() {
    let h = harness(&["cardscan"]).await;
    let (st, _, b) = call(&h.router, "GET", "/api/cardscan/status", &AUTH, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(json_body(&b), json!({"ok":true,"model":"cardscan-mock","ready":true}));
    assert!(hit_matches(|x| x.method == "GET" && x.path == "/api/health").await);
}

// ---------------------------------------------------------------------------
// detect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_json_body_is_missing_image() {
    let h = harness(&["cardscan"]).await;
    // The OUTER express.json eats JSON bodies before the raw parser: even a
    // 300-char 'image' is not a Buffer → 400 {ok:false,error:'Missing image'}.
    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/detect?t=jsonmiss", &JSON_CT, Some(&format!(r#"{{"image":"{}"}}"#, img(300)))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"ok":false,"error":"Missing image"}));
    assert!(no_hit(|x| x.path.contains("t=jsonmiss")), "JSON body must never reach the sidecar");
}

#[tokio::test]
async fn detect_short_jpeg_is_missing_image() {
    let h = harness(&["cardscan"]).await;
    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/detect", &JPEG_CT, Some("tiny")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"ok":false,"error":"Missing image"}));
}

#[tokio::test]
async fn detect_forwards_image_bytes_and_passes_reply_through() {
    let h = harness(&["cardscan"]).await;
    let body = format!("DETSENT-{}", img(200));
    let (st, hd, b) = call(&h.router, "POST", "/api/cardscan/detect", &JPEG_CT, Some(&body)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd[header::CONTENT_TYPE], "application/json; charset=utf-8"); // res.json
    assert_eq!(json_body(&b), json!({"cards":[{"box":[1,2,3,4]}]}));
    let want = body.clone();
    assert!(
        hit_matches(|x| x.path == "/api/detect-cards"
            && x.ct.as_deref() == Some("image/jpeg")
            && x.body == want
            && x.scan_client.as_deref() == None)
            .await,
        "detect leg missing"
    );
}

// ---------------------------------------------------------------------------
// frame
// ---------------------------------------------------------------------------

#[tokio::test]
async fn frame_hydrates_and_shapes_reply() {
    let h = harness(&["cardscan"]).await;
    let client_hdr = "ondevice|fallback!!x".repeat(20);
    let hdrs: Vec<(&str, &str)> = vec![
        ("authorization", "Bearer sess-1"),
        ("content-type", "image/jpeg"),
        ("x-scan-client", client_hdr.as_str()),
        ("x-scan-session", "sess@@123"),
    ];
    let (st, hd, b) = call(&h.router, "POST", "/api/cardscan/frame", &hdrs, Some(&img(150))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd[header::CONTENT_TYPE], "application/json; charset=utf-8");
    let v = json_body(&b);
    assert_eq!(v["ok"], true);
    assert_eq!(v["frame"], "F");
    // candidate keys are the fixed five (junk dropped, undefined drops too)
    assert_eq!(v["candidates"][0], json!({"number":1,"box":[0,0,1,1],"quad":true,"eligible":true,"status":"ok"}));
    let r0 = &v["results"][0];
    assert_eq!(r0["number"], "7");
    assert_eq!(r0["ok"], true);
    assert_eq!(r0["cached"], true);
    assert_eq!(r0["card"]["name"], "Frame Card");
    let r1 = &v["results"][1];
    assert_eq!(r1["ok"], false);
    assert_eq!(r1["error"], "no footer");
    assert_eq!(r1["choices"][0]["name"], "Twin B");
    assert_eq!(r1["choices"][1]["name"], "Twin C");
    // key order (JSON.stringify of the out literal): ok,error,title,via,
    // choices + cached last — NO number (sidecar result lacks scene_number,
    // undefined drops) and no card (never found).
    let keys: Vec<&String> = r1.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["ok", "error", "title", "via", "choices", "cached"]);
    // timings = sidecar timings + proxy_ms appended
    let tk: Vec<&String> = v["timings"].as_object().unwrap().keys().collect();
    assert_eq!(tk, vec!["ocr_ms", "proxy_ms"]);
    // headers forwarded, sanitized, session prefixed with the user id
    let expect_client: String = "ondevice|fallback!!x"
        .repeat(20)
        .chars()
        .take(120)
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || [' ', '.', ':', '=', ',', '/', '-'].contains(c))
        .collect();
    assert_eq!(expect_client.chars().count(), 102, "sanity of the ladder");
    let pred_client = expect_client.clone();
    // Clone-then-drop-guard: NEVER assert across the hits() guard (the
    // iter-11 root-cause — a failing assert while the guard is live poisoned
    // the shared mutex for every sibling test).
    // Path-only find is NOT unique: sibling tests proxy /api/scan-frame to
    // fake-node without the scan headers, and whichever hit lands first used
    // to win the race (scan_client None flake). Select on the full identity
    // (path + session), same predicate as hit_matches below.
    let frame_hit = hits_lock()
        .iter()
        .find(|h| h.path == "/api/scan-frame" && h.scan_session.as_deref() == Some("1-sess123"))
        .cloned();
    if let Some(h) = frame_hit {
        assert_eq!(h.scan_client, Some(expect_client.clone()), "client header mismatch");
        assert_eq!(h.scan_session, Some("1-sess123".to_string()), "session header mismatch");
    }
    assert!(
        hit_matches(move |x| {
            x.path == "/api/scan-frame"
                && x.scan_client.as_deref() == Some(pred_client.as_str())
                && x.scan_session.as_deref() == Some("1-sess123")
        })
.await,
        "frame leg missing"
    );
}

#[tokio::test]
async fn scan_ok_hydrates_and_fires_scene_complete() {
    let h = harness(&["cardscan"]).await;
    let (st, _, b) = call(&h.router, "POST", "/api/cardscan", &JSON_CT, Some(r#"{"cards":[{"image":"SCN-SENT"}]}"#)).await;
    assert_eq!(st, StatusCode::OK);
    let v = json_body(&b);
    assert_eq!(v["ok"], true);
    assert_eq!(v["results"][0]["card"]["name"], "Scene Card");
    assert_eq!(v["results"][0]["via"], "footer");
    let tk: Vec<&String> = v["timings"].as_object().unwrap().keys().collect();
    assert_eq!(tk, vec!["scene_ms"]);
    assert!(hit_matches(|x| x.path == "/api/scan-scene" && x.scene_proto.as_deref() == Some("3")).await);
    assert!(
        hit_matches(|x| x.path == "/api/scan-scene" && x.body.contains("SCN-SENT") && x.body.starts_with("{\"cards\":")).await,
        "scene payload must be {{cards:[...]}} verbatim"
    );
    // fire-and-forget scene-complete
    assert!(hit_matches(|x| x.path == "/api/scene-complete").await, "scene-complete never fired");
}

// ---------------------------------------------------------------------------
// cards / choices
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cards_validation_ladder() {
    let h = harness(&["cardscan"]).await;
    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/cards", &JSON_CT, Some(r#"{"results":[]}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"ok":false,"error":"Expected 1-8 results"}));

    let many: Vec<Value> = (0..9).map(|i| json!({"scryfallId": format!("{i:08}-1111-1111-1111-111111111111")})).collect();
    let (st, _, b) =
        call(&h.router, "POST", "/api/cardscan/cards", &JSON_CT, Some(&json!({"results": many}).to_string())).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b)["error"], "Expected 1-8 results");

    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/cards", &JSON_CT, Some(r#"{"results":[{"scryfallId":"not-a-uuid"}]}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"ok":false,"error":"Invalid card id"}));
}

#[tokio::test]
async fn cards_hydrates_through_the_shared_hydrate() {
    let h = harness(&["cardscan"]).await;
    let body = format!(
        r#"{{"results":[{{"scryfallId":"{UUID_A}","number":"7","title":"CLI Title","via":"ondevice-{UUID_C}"}}]}}"#
    );
    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/cards", &JSON_CT, Some(&body)).await;
    assert_eq!(st, StatusCode::OK);
    let v = json_body(&b);
    assert_eq!(v["ok"], true);
    let r = &v["results"][0];
    assert_eq!(r["number"], "7");
    assert_eq!(r["ok"], true);
    assert_eq!(r["title"], "CLI Title");
    assert_eq!(r["via"], format!("ondevice-{UUID_C}"));
    assert_eq!(r["card"]["name"], "Frame Card");
    // Node's literal assigns error:undefined when ok — JSON.stringify drops
    // it; number rides because the client supplied it.
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["number", "ok", "title", "via", "card"]);
}

#[tokio::test]
async fn cards_row_drops_number_when_client_omits_it() {
    let h = harness(&["cardscan"]).await;
    let body = format!(r#"{{"results":[{{"scryfallId":"{UUID_B}"}}]}}"#);
    let (_, _, b) = call(&h.router, "POST", "/api/cardscan/cards", &JSON_CT, Some(&body)).await;
    let v = json_body(&b);
    let keys: Vec<&String> = v["results"][0].as_object().unwrap().keys().collect();
    // scene_number undefined at the synth → out.number never exists.
    assert_eq!(keys, vec!["ok", "title", "via", "card"]);
}

#[tokio::test]
async fn cards_missing_row_forwards_to_node() {
    // getCardById's miss enters the Scryfall fetch lane (fetch + cacheCards):
    // the outcome is observable, so the whole request goes to Node.
    let h = harness(&["cardscan"]).await;
    let body = r#"{"results":[{"scryfallId":"eeeeeeee-5555-5555-5555-555555555555"}]}"#;
    let (_, hd, b) = call(&h.router, "POST", "/api/cardscan/cards", &JSON_CT, Some(body)).await;
    assert_eq!(hd["x-node"], "1", "missing row must forward, got {}", String::from_utf8_lossy(&b));
    assert_eq!(json_body(&b)["body"], body);
}

#[tokio::test]
async fn choices_ok_and_validation() {
    let h = harness(&["cardscan"]).await;
    let body = format!(r#"{{"ids":["{UUID_B}","{UUID_C}"]}}"#);
    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/choices", &JSON_CT, Some(&body)).await;
    assert_eq!(st, StatusCode::OK);
    let v = json_body(&b);
    assert_eq!(v["ok"], true);
    assert_eq!(v["cards"][0]["name"], "Twin B");
    assert_eq!(v["cards"][1]["name"], "Twin C");

    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/choices", &JSON_CT, Some(r#"{"ids":["bbbbbbbb-2222-2222-2222-222222222222"]}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"ok":false,"error":"Expected 2-8 card ids that exist"}));

    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/choices", &JSON_CT, Some(r#"{"ids":["zz","yy"]}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b)["error"], "Expected 2-8 card ids that exist");
}

#[tokio::test]
async fn choices_missing_row_forwards_to_node() {
    let h = harness(&["cardscan"]).await;
    // hydrateChoices uses getCardById, whose miss enters the Scryfall lane:
    // whole-request Forward.
    let body = r#"{"ids":["eeeeeeee-5555-5555-5555-555555555555","ffffffff-6666-6666-6666-666666666666"]}"#;
    let (st, hd, b) = call(&h.router, "POST", "/api/cardscan/choices", &JSON_CT, Some(body)).await;
    assert_eq!(hd["x-node"], "1", "expected forward, got {st} {}", String::from_utf8_lossy(&b));
    assert_eq!(json_body(&b)["body"], body);
}

#[tokio::test]
async fn frame_missing_card_forwards_to_node() {
    let h = harness(&["cardscan"]).await;
    // The mock's frame results reference seeded rows; delete one → the
    // scan/frame hydrate (fetch_on_miss=true) must forward the whole POST.
    {
        let conn = Connection::open(&h.db).unwrap();
        conn.execute_batch("DELETE FROM card_cache WHERE id='mtg-dddddddd-4444-4444-4444-444444444444';").unwrap();
    }
    let (st, hd, b) = call(&h.router, "POST", "/api/cardscan", &JSON_CT, Some(r#"{"cards":[{"image":"FWD-SENT"}]}"#)).await;
    assert_eq!(hd["x-node"], "1", "expected forward, got {st} {}", String::from_utf8_lossy(&b));
    assert_eq!(json_body(&b)["pq"], "/api/cardscan");
    assert_eq!(st, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// scan validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scan_validation() {
    let h = harness(&["cardscan"]).await;
    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/scan", &JSON_CT, Some(r#"{"cards":[]}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"ok":false,"error":"Expected 1-8 cards"}));
    let many: Vec<Value> = (0..9).map(|i| json!({"image": i})).collect();
    let (st, _, b) =
        call(&h.router, "POST", "/api/cardscan", &JSON_CT, Some(&json!({"cards": many}).to_string())).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b)["error"], "Expected 1-8 cards");
    // 1 valid card is legal: the leg must fire with the verbatim cards array.
    let (st, _, _) = call(&h.router, "POST", "/api/cardscan?t=scanval", &JSON_CT, Some(r#"{"cards":[{"image":"validation-sentinel"}]}"#)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        hit_matches(|x| x.path == "/api/scan-scene" && x.body.contains("validation-sentinel"))
            .await,
        "valid single-card scan must reach the sidecar"
    );
}

// ---------------------------------------------------------------------------
// telemetry (local JSONL sink — NO sidecar leg)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telemetry_writes_jsonl_with_meta() {
    let h = harness(&["cardscan"]).await;
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let d = h._dir.path().join(format!("tel-{}", std::process::id()));
        std::env::set_var("SCAN_TELEMETRY_DIR", &d);
        d
    }).clone();
    let body = r#"{"session":"s3cr3t-very-long-session-id","platform":"ios","records":[{"pass":"t","ms":1},{"pass":"u"},{"pass":"v"}]}"#;
    let (st, _, _) =
        call(&h.router, "POST", "/api/cardscan/telemetry", &[("authorization", "Bearer sess-1"), ("content-type", "application/json"), ("user-agent", "UA/1")], Some(body)).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let day = scrybox_server::cardscan::cardscan_day();
    let file = dir.join(format!("{day}.jsonl"));
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 3);
    let l0 = &lines[0];
    assert_eq!(l0["user"], 1);
    assert_eq!(l0["session"], "s3cr3t-very-long-ses"); // slice(0,20)
    assert_eq!(l0["platform"], "ios");
    assert!(l0.get("device").is_none(), "undefined drops");
    assert_eq!(l0["ua"], "UA/1");
    assert_eq!(l0["pass"], "t");
    assert!(l0["recv"].as_str().unwrap().ends_with('Z'));
    // fresh recv per record is allowed to be equal (ms clock); shape only.
    assert!(lines[2]["recv"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn telemetry_no_records_and_env_off() {
    let h = harness(&["cardscan"]).await;
    let (st, _, b) = call(&h.router, "POST", "/api/cardscan/telemetry", &JSON_CT, Some(r#"{"records":[]}"#)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b), json!({"ok":false,"error":"No records"}));

    // Non-parsed content-type → req.body {} → No records.
    let (st, _, b) = call(
        &h.router,
        "POST",
        "/api/cardscan/telemetry",
        &[("authorization", "Bearer sess-1"), ("content-type", "application/octet-stream")],
        Some(r#"{"records":[{"a":1}]}"#),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&b)["error"], "No records");

    std::env::set_var("SCAN_TELEMETRY", "0");
    let (st, _, _) = call(&h.router, "POST", "/api/cardscan/telemetry", &JSON_CT, Some(r#"{"records":[{"a":1}]}"#)).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    std::env::remove_var("SCAN_TELEMETRY");
}




#[tokio::test]
async fn unparsable_gateway_json_is_502() {
    // validateStatus always true, so a 200 whose body claims json but cannot
    // be parsed is axios's tryParse throw → generic 502 'Scanner service
    // error' (not 200, not 422, not a passthrough).
    let h = harness(&["cardscan"]).await;
    let (st, _, b) =
        call(&h.router, "POST", "/api/cardscan/detect", &JPEG_CT, Some(&format!("JUNK{}", img(200)))).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert_eq!(
        json_body(&b),
        json!({"ok":false,"unavailable":false,"error":"Scanner service error"})
    );
}

#[tokio::test]
async fn res_json_echo_arms_ship_restringified_bytes() {
    // Every reply is res.json's output: compact JSON.stringify of the
    // PARSED value (canonical numbers), weak ETag = the npm etag package's
    // W/"{len:hex}-{base64 sha1 without padding}" — reproduced by the
    // crate's shared body_etag twin, so we assert against that helper the
    // same rules/stats ports pin against the live Node front.
    let h = harness(&["cardscan"]).await;
    // detect leg -> mock /api/detect-cards replies `{"cards":[{"box":[1,2,3,4]}]}`
    let (st, hd, b) = call(&h.router, "POST", "/api/cardscan/detect?t=echopin", &JPEG_CT, Some(&img(230))).await;
    assert_eq!(st, StatusCode::OK);
    let mock_echo: &[u8] = br#"{"cards":[{"box":[1,2,3,4]}]}"#;
    assert_eq!(b, mock_echo);
    assert_eq!(hd[header::ETAG], scrybox_server::collection::body_etag(mock_echo));
    // ETag len is HEX: 29 bytes → "1d", never "29".
    assert!(hd[header::ETAG].to_str().unwrap().starts_with("W/\"1d-"), "{:?}", hd[header::ETAG]);
    // upstreamError arm.
    let (st, hd, b) =
        call(&h.router, "POST", "/api/cardscan/detect", &JPEG_CT, Some(&format!("JUNK{}", img(210)))).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert_eq!(
        b,
        b"{\"ok\":false,\"unavailable\":false,\"error\":\"Scanner service error\"}"
    );
    assert_eq!(hd[header::ETAG], scrybox_server::collection::body_etag(&b));
}

#[tokio::test]
async fn frame_reply_prints_bare_integers_and_hydration_etag() {
    let h = harness(&["cardscan"]).await;
    let hdrs: Vec<(&str, &str)> = vec![("authorization", "Bearer sess-1"), ("content-type", "image/jpeg")];
    let (st, hd, b) = call(&h.router, "POST", "/api/cardscan/frame", &hdrs, Some(&img(140))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hd[header::ETAG], scrybox_server::collection::body_etag(&b));
    let text = String::from_utf8(b).unwrap();
    assert!(text.contains("\"box\":[0,0,1,1]"), "integers print bare: {text}");
    assert!(!text.contains(".0"), "no JS-stray float tails: {text}");
}

#[tokio::test]
async fn nested_bodies_are_compact_stringify() {
    // Production express: no pretty-print. Key order from hydrate's object
    // literal is the observable part.
    let h = harness(&["cardscan"]).await;
    let body = format!(r#"{{"ids":["{UUID_B}","{UUID_C}"]}}"#);
    let (_, _, b) = call(&h.router, "POST", "/api/cardscan/choices", &JSON_CT, Some(&body)).await;
    let text = String::from_utf8(b).unwrap();
    assert!(text.starts_with("{\"ok\":true,\"cards\":[{\"id\":\"mtg-"), "got {text}");
    assert!(!text.contains("\n"));
}
