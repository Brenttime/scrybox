//! Node-zero (no SCRYBOX_NODE_UPSTREAM) fallbacks for the decks + lists
//! groups. Each case used to reach `crate::proxy` and answer 502 "Upstream
//! unavailable" (card_cache-miss hydration Forwards) or finalhandler's 404
//! (HEAD on Express GET routes, GET|DELETE /api/lists/order which Express
//! routes to `/:id`). Every response here must be Node's.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::{Path, Request},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

const GOOD: &str = "aaaaaaaa-1111-4111-8111-111111111111";
const GOOD2: &str = "bbbbbbbb-2222-4222-8222-222222222222";
const BAD: &str = "cccccccc-3333-4333-8333-333333333333";

type Hits = Arc<Mutex<Vec<String>>>;

async fn mock_scryfall(hits: Hits) -> String {
    let r = Router::new().route(
        "/cards/{id}",
        get(move |Path(id): Path<String>| {
            let hits = hits.clone();
            async move {
                hits.lock().unwrap().push(id.clone());
                if id != GOOD && id != GOOD2 {
                    return (StatusCode::NOT_FOUND, axum::Json(json!({"object":"error","status":404}))).into_response();
                }
                let name = if id == GOOD { "Fetched Card" } else { "Other Card" };
                axum::Json(json!({
                    "object":"card","id": id,"name": name,"lang":"en","set":"tst","set_name":"Test Set",
                    "collector_number":"7","rarity":"common","type_line":"Instant","cmc":1.0,
                    "colors":["R"],"color_identity":["R"],"layout":"normal",
                    "image_uris":{"normal":"http://x/a.jpg"},"prices":{"usd":"1.00"}
                }))
                .into_response()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    format!("http://{addr}")
}

struct H {
    router: Router,
    db: std::path::PathBuf,
    hits: Hits,
    _dir: tempfile::TempDir,
}

async fn harness() -> H {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS card_lists (
            id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, name TEXT NOT NULL,
            description TEXT, accent_color TEXT DEFAULT '#10b981',
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP, source TEXT, icon TEXT, sort_order INTEGER);
         CREATE TABLE IF NOT EXISTS list_cards (list_id INTEGER NOT NULL, card_id TEXT NOT NULL,
            quantity INTEGER DEFAULT 1, PRIMARY KEY(list_id, card_id));
         INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id,name,supertype,subtypes,types,rarity,set_id,set_name,number,image_url,price_trend,price_normal,price_holofoil,price_etched,price_estimated,cmc,color_identity,price_currency)
           VALUES ('z-alpha','Alpha Card','MTG','[\"Creature\"]','[\"White\"]','Rare','lrs','LR Set','1','img/a',1.0,5.0,0,0,0,2,'[\"W\"]','USD');
         INSERT INTO card_lists (id,user_id,name,description,accent_color,source) VALUES (201,1,'Wishlist','w','#10b981','web');
         INSERT INTO list_cards (list_id,card_id,quantity) VALUES (201,'z-alpha',1);
         INSERT INTO decks (id,user_id,name) VALUES (301,1,'Deck A'),(302,1,'Deck B');",
    )
    .unwrap();
    drop(conn);
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let base = mock_scryfall(hits.clone()).await;
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: String::new(),
        static_dir: None,
        rust_routes: ["*".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(db.clone()),
        sidecar_url: None,
    };
    let mut st = AppState::new(cfg);
    st.scry = Arc::new(scrybox_server::scryfall::Scry::new(scrybox_server::scryfall::ScryEnv {
        local: false,
        local_url: base.clone(),
        base_url: base,
        gap_scale: 0.0,
    }));
    H { router: app(st), db, hits, _dir: dir }
}

impl H {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, HeaderMap, String) {
        let mut b = Request::builder().method(method).uri(path).header("authorization", "Bearer sess-1");
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        let s = res.status();
        let h = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (s, h, String::from_utf8_lossy(&bytes).into_owned())
    }
    fn conn(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }
    fn cached(&self, id: &str) -> bool {
        self.conn()
            .query_row("SELECT COUNT(*) FROM card_cache WHERE id = ?1", [id], |r| r.get::<_, i64>(0))
            .unwrap()
            > 0
    }
    fn list_qty(&self, list: i64, card: &str) -> Option<i64> {
        self.conn()
            .query_row("SELECT quantity FROM list_cards WHERE list_id = ?1 AND card_id = ?2", rusqlite::params![list, card], |r| r.get(0))
            .ok()
    }
}

fn mtg(id: &str) -> String {
    format!("mtg-{id}")
}

#[tokio::test]
async fn list_add_card_miss_fetches_natively() {
    let h = harness().await;
    let (s, _, b) = h.call("POST", "/api/lists/201/cards", Some(json!({"card_id": mtg(GOOD), "quantity": 3}))).await;
    assert_eq!((s, b.as_str()), (StatusCode::OK, r#"{"message":"Card added to list"}"#));
    assert!(h.cached(&mtg(GOOD)), "cacheCards ran");
    assert_eq!(h.list_qty(201, &mtg(GOOD)), Some(3));
    assert_eq!(h.hits.lock().unwrap().as_slice(), [GOOD.to_string()]);

    let (s, _, b) = h.call("POST", "/api/lists/201/cards", Some(json!({"card_id": mtg(BAD)}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b, format!(r#"{{"error":"Card {} not found on any card provider."}}"#, mtg(BAD)));
    assert_eq!(h.list_qty(201, &mtg(BAD)), None);
}

#[tokio::test]
async fn list_bulk_add_miss_fetches_and_reports_failed() {
    let h = harness().await;
    let (s, _, b) = h
        .call(
            "POST",
            "/api/lists/201/cards/bulk",
            Some(json!({"cards": [{"card_id": mtg(GOOD2), "quantity": 2}, {"card_id": mtg(BAD)}, {"card_id": "z-alpha"}]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, format!(r#"{{"added":3,"failed":["{}"]}}"#, mtg(BAD)));
    assert_eq!(h.list_qty(201, &mtg(GOOD2)), Some(2));
    assert_eq!(h.list_qty(201, "z-alpha"), Some(2));

    // Every entry fails -> Node's 500 with the counts body.
    let (s, _, b) = h.call("POST", "/api/lists/201/cards/bulk", Some(json!({"cards": [{"card_id": mtg(BAD)}]}))).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(b, format!(r#"{{"added":0,"failed":["{}"]}}"#, mtg(BAD)));
}

#[tokio::test]
async fn list_change_printing_miss_fetches_natively() {
    let h = harness().await;
    // Unknown target: fetch null -> 404.
    let (s, _, b) = h.call("PUT", "/api/lists/201/cards/z-alpha/printing", Some(json!({"card_id": mtg(BAD)}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b, format!(r#"{{"error":"Card {} not found on any card provider."}}"#, mtg(BAD)));
    // Fetched but a different card -> same-card 400 (Node continues past the fetch).
    let (s, _, b) = h.call("PUT", "/api/lists/201/cards/z-alpha/printing", Some(json!({"card_id": mtg(GOOD)}))).await;
    assert_eq!((s, b.as_str()), (StatusCode::BAD_REQUEST, r#"{"error":"Pick a printing of the same card"}"#));
    assert!(h.cached(&mtg(GOOD)));
}

#[tokio::test]
async fn deck_add_card_miss_fetches_natively() {
    let h = harness().await;
    let (s, _, b) = h.call("POST", "/api/decks/301/cards", Some(json!({"card_id": mtg(BAD)}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b, format!(r#"{{"error":"Card {} not found on any card provider."}}"#, mtg(BAD)));

    // Miss -> fetch -> same outcome as the already-cached path (control on deck 302).
    let (s1, _, b1) = h.call("POST", "/api/decks/301/cards", Some(json!({"card_id": mtg(GOOD), "quantity": 1}))).await;
    assert_ne!(s1, StatusCode::BAD_GATEWAY, "{b1}");
    assert!(h.cached(&mtg(GOOD)));
    let (s2, _, b2) = h.call("POST", "/api/decks/302/cards", Some(json!({"card_id": mtg(GOOD), "quantity": 1}))).await;
    assert_eq!((s1, b1), (s2, b2));
    assert_eq!(h.hits.lock().unwrap().len(), 2, "BAD + GOOD fetched once each");
}

#[tokio::test]
async fn head_on_get_routes_is_served() {
    let h = harness().await;
    for p in ["/api/decks", "/api/decks/301", "/api/decks/301/locations", "/api/lists"] {
        let (gs, gh, _) = h.call("GET", p, None).await;
        let (s, hd, _) = h.call("HEAD", p, None).await;
        assert_eq!(s, gs, "HEAD {p}");
        assert_eq!(hd.get("etag"), gh.get("etag"), "HEAD {p} etag");
    }
}

#[tokio::test]
async fn lists_order_non_put_hits_id_route() {
    let h = harness().await;
    // Express: GET /api/lists/order -> router.get('/:id') with id "order".
    let (s, _, b) = h.call("GET", "/api/lists/order", None).await;
    assert_eq!((s, b.as_str()), (StatusCode::NOT_FOUND, r#"{"error":"List not found"}"#));
    let (gs, gb) = {
        let (s, _, b) = h.call("DELETE", "/api/lists/999999", None).await;
        (s, b)
    };
    let (s, _, b) = h.call("DELETE", "/api/lists/order", None).await;
    assert_eq!((s, b), (gs, gb));
}
