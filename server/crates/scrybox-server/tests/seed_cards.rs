//! LANE-A iter-49: POST /api/admin/seed-cards (routes/admin.js:19 — the
//! last Lane-A route). Route-level tests pin the middleware order
//! (normalJson 500/413 BEFORE auth), the auth+admin gates, the
//! getCardsBySet twin (429-only retry ladder, per-set failure → [] → cache
//! fallback, empty cache → 502), the ALLOCATION_CONFLICT abort (409,
//! transaction rolled back), the 208-insert shape (random qty/price pinned
//! only by PREDICATES — the count is random by design and asserted against
//! SUM(quantity) instead), the set-scoped DELETE, and the non-POST proxy.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use serde_json::{json, Value};

use scrybox_server::{auth_support::uuid_v4, AppState, Config};

#[path = "./auth_common.rs"]
mod auth_common;
use auth_common::{bearer, json_ct, merge, request};

#[path = "mock_scryfall.rs"]
mod mock_scryfall;
use mock_scryfall::{spawn_mock, MockState};

const ADMIN_TOK: &str = concat!("tok-", "seed-admin-1");
const MEM_TOK: &str = concat!("tok-", "seed-mem-1");
const KEY: &str =
    concat!("bnd_", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

const PATH: &str = "/api/admin/seed-cards";

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
                "pq": parts.uri.path_and_query().unwrap().as_str(),
                "method": parts.method.as_str(),
                "body": String::from_utf8_lossy(&body),
            })),
        )
            .into_response()
    });
    let health = axum::routing::get(|| async { (StatusCode::OK, axum::Json(json!({"status": "ok"}))) });
    let r = Router::new().route("/api/health", health).fallback(echo);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    addr
}

struct Fix {
    router: Router,
    mock: Arc<MockState>,
    pool: Arc<scrybox_server::db::DbPool>,
}

async fn build() -> Fix {
    let node = spawn_fake_node().await;
    let (mock_addr, mock) = spawn_mock().await;
    let dir = std::env::temp_dir().join(format!("scrybox-seed-it-{}", uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bindarr.db"), []).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: format!("http://{node}"),
        static_dir: None,
        rust_routes: ["seed_cards".to_string()].into_iter().collect(),
        db_path: Some(dir.join("bindarr.db")),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.scry = Arc::new(mock_scryfall::scry_for(mock_addr, 0.001));
    let pool = st.db.clone().unwrap();
    pool.get().unwrap().execute_batch(auth_common::schema_fixture()).unwrap();
    pool.get()
        .unwrap()
        .execute_batch(
            "INSERT INTO users (username, password_hash, role, share_token, api_key)
             VALUES ('seedadmin','h','admin','sh-sa',NULL),
                    ('seedmem','h','member','sh-sm',NULL),
                    ('seedkey','h','admin','sh-sk','bnd_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');
             INSERT INTO sessions (token, user_id, expires_at)
             VALUES ('tok-seed-admin-1',1,'2030-01-01 00:00:00'),
                    ('tok-seed-mem-1',2,'2030-01-01 00:00:00');
             INSERT INTO card_cache (id, set_id, name, price_normal, price_holofoil, language)
             VALUES ('mtg-a1','lea','Alpha Card',1.5,3.0,'English'),
                    ('mtg-a3','mh3','Alpha Three',2.5,NULL,'English'),
                    ('mtg-b1','brc','Kept Card',1.0,NULL,'English');
             INSERT INTO collection (card_id, quantity, condition, printing, language, purchase_price, user_id)
             VALUES ('mtg-a1',2,'Near Mint','Normal','English',1.0,1),
                    ('mtg-b1',3,'Near Mint','Normal','English',1.0,1);",
        )
        .unwrap();
    Fix { router: scrybox_server::app(st), mock, pool }
}

fn admin() -> Vec<(&'static str, String)> {
    bearer(ADMIN_TOK)
}

async fn post(fix: &Fix, headers: Vec<(&'static str, String)>, body: Option<&str>) -> (StatusCode, HeaderMap, String) {
    request(&fix.router, peer_n(11), "POST", PATH, &merge(&headers, &json_ct()), body.map(str::to_string)).await
}

fn peer_n(n: u64) -> SocketAddr {
    SocketAddr::from((std::net::Ipv4Addr::new(10, 198, (n / 250) as u8, (n % 250) as u8 + 1), 45000))
}

/// The mock /cards/search body both set calls share: one lea + one mh3
/// printing, one holofoil-capable.
fn search_body() -> Value {
    json!({"data": [
        {"id":"a1","set":"lea","name":"Alpha Card","lang":"en",
         "prices":{"usd":"1.50","usd_foil":"3.00"}},
        {"id":"a3","set":"mh3","name":"Alpha Three","lang":"en",
         "prices":{"usd":"2.50"}}
    ], "has_more": false})
}

// ---------------------------------------------------------------------------
// Middleware order + gates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_and_oversize_answer_before_auth() {
    let fix = build().await;
    let (s, _, b) = request(&fix.router, peer_n(21), "POST", PATH, &json_ct(), Some("notjson".into())).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(b, "{\"error\":\"Internal server error\"}");
    let huge = format!("{{\"x\":\"{}\"}}", "a".repeat(1024 * 1024));
    let (s, _, b) = request(&fix.router, peer_n(22), "POST", PATH, &json_ct(), Some(huge)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(b, "{\"error\":\"Upload too large. Try exporting/importing in smaller batches.\"}");
    // The mock was never touched: both answers precede the handler AND Scryfall.
    assert!(fix.mock.hits_for("GET /cards/search").is_empty());
}

#[tokio::test]
async fn auth_and_admin_gates() {
    let fix = build().await;
    let (s, _, b) = request(&fix.router, peer_n(31), "POST", PATH, &json_ct(), Some("{}".into())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(b, "{\"error\":\"Access denied. No token provided.\"}");
    // Member session: requireAdmin twin.
    let (s, _, b) = post(&fix, bearer(MEM_TOK), Some("{}".into())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b, "{\"error\":\"Access denied. Administrator privileges required.\"}");
    // API key on POST: auth's read-only leg fires before requireAdmin.
    let (s, _, b) = post(&fix, bearer(KEY), Some("{}".into())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b, "{\"error\":\"API keys are read-only. Use a login session for this request.\"}");
    assert!(fix.mock.hits_for("GET /cards/search").is_empty());
}

#[tokio::test]
async fn non_post_rides_the_proxy() {
    let fix = build().await;
    let (s, h, _) = request(&fix.router, peer_n(41), "GET", PATH, &bearer(ADMIN_TOK), None).await;
    assert_eq!(s, StatusCode::OK); // fake node echoes 200
    assert_eq!(h.get("x-node").unwrap(), "1");
    assert!(fix.mock.hits_for("GET /cards/search").is_empty());
}

// ---------------------------------------------------------------------------
// Success path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn seed_end_to_end_shape() {
    let fix = build().await;
    *fix.mock.search.lock().unwrap() = search_body();
    let (s, h, b) = post(&fix, admin(), Some("{}".into())).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(h.get("content-type").unwrap(), "application/json; charset=utf-8");
    assert!(h.get("etag").is_some(), "res.json ETag twin");
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["message"]);
    let msg = v["message"].as_str().unwrap();
    let n: i64 = msg
        .strip_prefix("Successfully seeded a large test collection: ")
        .and_then(|r| r.strip_suffix(" cards for admin user."))
        .unwrap_or_else(|| panic!("message shape: {msg}"))
        .parse()
        .unwrap();
    // Random by design: 208 rows, qty 1|2 ⇒ 208 ≤ N ≤ 416.
    assert!((208..=416).contains(&n), "N={n}");

    let conn = fix.pool.get().unwrap();
    // Pre-existing mh3/lea row removed, brc row KEPT.
    let kept: i64 = conn
        .query_row("SELECT quantity FROM collection WHERE card_id='mtg-b1' AND user_id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 3);
    // (The seeded rows themselves may legitimately re-use mtg-a1/mtg-a3 —
    // the total-count + kept-row checks below pin that the DELETE landed.)

    // 208 fresh rows for user 1, all in the seed sets, sum(quantity) == N.
    let mut stmt = conn
        .prepare(
            "SELECT c.quantity, c.condition, c.printing, c.language, c.purchase_price, cc.set_id, c.card_id
             FROM collection c JOIN card_cache cc ON cc.id = c.card_id
             WHERE c.user_id = 1",
        )
        .unwrap();
    let rows: Vec<(i64, String, String, String, f64, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // kept brc row + 208 seeded rows.
    assert_eq!(rows.len(), 209);
    let mut sum_qty = 0i64;
    let mut seen_printing: Vec<&str> = Vec::new();
    for (qty, cond, printing, lang, pp, set, card) in &rows {
        if card == "mtg-b1" {
            continue; // the kept row, not part of the assertions
        }
        sum_qty += qty;
        assert!([1, 2].contains(qty), "qty {qty}");
        assert!(
            ["Near Mint", "Lightly Played", "Moderately Played", "Heavily Played"].contains(&cond.as_str()),
            "condition {cond}"
        );
        assert!(["English", "Japanese"].contains(&lang.as_str()), "language {lang}");
        assert!(["Normal", "Holofoil"].contains(&printing.as_str()), "printing {printing}");
        assert!((0.0..=10.0).contains(pp), "price {pp}");
        assert!((0.0..=10.0).contains(pp), "price {pp}");
        assert!((pp * 100.0 - (pp * 100.0).round()).abs() < 1e-6, "price {pp} cents");
        assert!(["lea", "mh3"].contains(&set.as_str()), "set {set}");
        // printsForCard: Holofoil only where price_holofoil > 0 (mtg-a1).
        if printing == "Holofoil" {
            assert_eq!(card, "mtg-a1", "holofoil on a hololess printing");
        }
        if !seen_printing.contains(&printing.as_str()) {
            seen_printing.push(printing.as_str());
        }
    }
    assert_eq!(sum_qty, n, "message count == SUM(quantity) of the seeded rows");
    // Random draws over 208 entries: both printings must show up
    // (P(all-Normal) = 2^-208; a deterministic regression would trip this).
    assert_eq!(seen_printing.len(), 2, "printing variety");
}

#[tokio::test]
async fn scryfall_429_ladder_then_failure_falls_back_to_cache() {
    let fix = build().await;
    // 429, 429, 200 — fetchFromScryfall retries ONLY on 429 (3 attempts).
    *fix.mock.search.lock().unwrap() = search_body();
    fix.mock.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![429, 200]);
    *fix.mock.retry_after.lock().unwrap() = Some("0".into());
    let (s, _, _) = post(&fix, admin(), Some("{}".into())).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(fix.mock.hits_for("GET /cards/search").len(), 3); // 1 retry, then success, then mh3 first-try

    // All-429 → both sets yield [] → card_cache fallback (fixture rows incl
    // brc → brc rows ARE in seedSetIds and get deleted).
    let fix = build().await;
    *fix.mock.search.lock().unwrap() = search_body();
    fix.mock.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![429]);
    *fix.mock.retry_after.lock().unwrap() = Some("0".into());
    let (s, _, _) = post(&fix, admin(), Some("{}".into())).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(fix.mock.hits_for("GET /cards/search").len(), 6); // 3 attempts × 2 sets
    let conn = fix.pool.get().unwrap();
    // Fallback pool spans ALL sets — the pre-existing brc row (qty 3,
    // purchase_price 1.0) is deleted; seed inserts never use qty 3, so its
    // absence proves the DELETE landed even though mtg-b1 rows reappear as
    // random seed entries.
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM collection WHERE card_id='mtg-b1' AND quantity=3 AND user_id=1", [], |r| r.get(0)).unwrap(),
        0,
        "fallback pool spans ALL sets — the brc delete is Node-faithful"
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM collection WHERE user_id=1", [], |r| r.get(0)).unwrap(),
        208
    );
}

#[tokio::test]
async fn empty_pool_and_empty_cache_is_502() {
    let fix = build().await;
    // Upstream answers 500 (non-429 does NOT retry) and the cache is empty.
    fix.mock.statuses.lock().unwrap().insert("GET /cards/search".into(), vec![500]);
    fix.pool
        .get()
        .unwrap()
        .execute_batch("DELETE FROM collection; DELETE FROM card_cache;")
        .unwrap();
    let (s, _, b) = post(&fix, admin(), Some("{}".into())).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(
        b,
        "{\"error\":\"Could not fetch seed card data from the card APIs, and local cache is empty. Try again shortly.\"}"
    );
    // Non-429 errors never loop: exactly one attempt per set.
    assert_eq!(fix.mock.hits_for("GET /cards/search").len(), 2);
}

#[tokio::test]
async fn allocation_conflict_409_rolls_everything_back() {
    let fix = build().await;
    *fix.mock.search.lock().unwrap() = search_body();
    // Checked-out deck pulls the lea row: owned 2, removing 2, locked 1 ⇒
    // 2 - 2 < 1 ⇒ ALLOCATION_CONFLICT.
    fix.pool
        .get()
        .unwrap()
        .execute_batch(
            "INSERT INTO decks (id, user_id, name, checked_out) VALUES (9, 1, 'Trip', 1);
             INSERT INTO deck_cards (deck_id, card_id, quantity) VALUES (9, 'mtg-a1', 1);",
        )
        .unwrap();
    let (s, _, b) = post(&fix, admin(), Some("{}".into())).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(
        b,
        "{\"error\":\"Check in decks using these cards before replacing the seed collection.\"}"
    );
    // The BEGIN IMMEDIATE transaction rolled back: pre-state intact, no
    // seeded rows, and the DELETE never landed.
    let conn = fix.pool.get().unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM collection WHERE user_id=1", [], |r| r.get(0)).unwrap(),
        2
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT quantity FROM collection WHERE card_id='mtg-a1' AND user_id=1", [], |r| r.get(0)).unwrap(),
        2
    );
}

#[tokio::test]
async fn body_is_ignored_but_must_parse() {
    let fix = build().await;
    *fix.mock.search.lock().unwrap() = search_body();
    // Array bodies are valid JSON (req.body unused by the handler) → 200.
    let (s, _, _) = post(&fix, admin(), Some("[1,2,3]")).await;
    assert_eq!(s, StatusCode::OK);
    // text/plain skips body-parser's typeIs (req.body undefined, unused) → 200.
    let (s, _, _) = request(
        &fix.router,
        peer_n(51),
        "POST",
        PATH,
        &merge(&admin(), &[("content-type", "text/plain".into())]),
        Some("nonsense".into()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}
