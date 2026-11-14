//! Node-free (SCRYBOX_NODE_UPSTREAM empty): collection-scope `q` searches
//! outside the old name/t:/c: subset used to return Plan::Proxy and answer
//! 502 "Upstream unavailable" with no Node. They are now classified by the
//! full Scryfall-syntax port (scryq.rs) and answered natively through
//! Node's own legs: answerLocally (upstream mode), the local-scryfall walk
//! (sidecar mode, by printing id for printing-level terms), and the
//! engine-outage local-fallback / 503.

use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Query, Request},
    routing::get,
    Router,
};
use axum::http::StatusCode;
use rusqlite::Connection;
use scrybox_server::{app, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

type Hits = Arc<Mutex<Vec<String>>>;

const LOTUS: &str = "11111111-0000-4000-8000-000000000001";
const BOLT: &str = "11111111-0000-4000-8000-000000000002";

async fn mock_sidecar(hits: Hits) -> String {
    let r = Router::new().route(
        "/cards/search",
        get(move |Query(qs): Query<std::collections::HashMap<String, String>>| {
            let hits = hits.clone();
            async move {
                let q = qs.get("q").cloned().unwrap_or_default();
                hits.lock().unwrap().push(q.clone());
                let data = if q == "s:lea unique:prints" {
                    json!([{"id": LOTUS, "name": "Black Lotus"}, {"id": BOLT, "name": "Lightning Bolt"}])
                } else {
                    json!([])
                };
                axum::Json(json!({"object":"list","total_cards": data.as_array().unwrap().len(),"has_more":false,"data":data}))
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
    hits: Hits,
    _dir: tempfile::TempDir,
}

/// `sidecar`: None = upstream mode (LOCAL_SCRYFALL off); Some(true) =
/// sidecar mode with a live mock engine; Some(false) = sidecar mode with
/// the engine down (connection refused).
async fn harness(sidecar: Option<bool>) -> H {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key)
           VALUES (1,'u1','x','member','tok-1',1,NULL);
         INSERT INTO sessions (token, user_id, expires_at) VALUES ('sess-1', 1, '2099-01-01 00:00:00');
         INSERT INTO card_cache (id, name, subtypes, types, color_identity, rarity, set_id, number, language, cmc) VALUES
           ('mtg-{LOTUS}', 'Black Lotus', '[\"Artifact\"]', '[]', '[]', 'rare', 'lea', '232', 'English', 0),
           ('mtg-{BOLT}', 'Lightning Bolt', '[\"Instant\"]', '[\"Red\"]', '[\"Red\"]', 'common', 'lea', '161', 'English', 1),
           ('mtg-bolt-m10', 'Lightning Bolt', '[\"Instant\"]', '[\"Red\"]', '[\"Red\"]', 'common', 'm10', '146', 'English', 1),
           ('mtg-guide', 'Goblin Guide', '[\"Creature\",\"Goblin\",\"Scout\"]', '[\"Red\"]', '[\"Red\"]', 'rare', 'zen', '126', 'English', 1),
           ('mtg-elves', 'Llanowar Elves', '[\"Creature\",\"Elf\",\"Druid\"]', '[\"Green\"]', '[\"Green\"]', 'common', 'm19', '314', 'English', 1),
           ('mtg-jace', 'Jace, the Mind Sculptor', '[\"Legendary\",\"Planeswalker\",\"Jace\"]', '[\"Blue\"]', '[\"Blue\"]', 'mythic', 'wwk', '31', 'English', 4),
           ('mtg-emrakul', 'Emrakul, the Aeons Torn', '[\"Legendary\",\"Creature\",\"Eldrazi\"]', '[]', '[]', 'mythic', 'roe', '6', 'English', 15);
         INSERT INTO collection (id, card_id, user_id, quantity, added_at) VALUES
           (1, 'mtg-{LOTUS}', 1, 1, '2026-01-01 00:00:01'),
           (2, 'mtg-{BOLT}', 1, 1, '2026-01-01 00:00:02'),
           (3, 'mtg-bolt-m10', 1, 2, '2026-01-01 00:00:03'),
           (4, 'mtg-guide', 1, 1, '2026-01-01 00:00:04'),
           (5, 'mtg-elves', 1, 1, '2026-01-01 00:00:05'),
           (6, 'mtg-jace', 1, 1, '2026-01-01 00:00:06'),
           (7, 'mtg-emrakul', 1, 1, '2026-01-01 00:00:07');"
    ))
    .unwrap();
    drop(conn);
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let sidecar_url = match sidecar {
        None => None,
        Some(true) => Some(mock_sidecar(hits.clone()).await),
        Some(false) => Some("http://127.0.0.1:1".to_string()),
    };
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: String::new(),
        static_dir: None,
        rust_routes: ["search".to_string(), "search_sidecar".to_string()].into_iter().collect::<BTreeSet<_>>(),
        db_path: Some(db.clone()),
        sidecar_url,
    };
    let mut st = AppState::new(cfg);
    st.local_scrayfall = sidecar.is_some();
    H { router: app(st), hits, _dir: dir }
}

async fn get_search(r: &Router, q: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let qs = form_urlencoded::Serializer::new(String::new()).append_pair("q", q).append_pair("scope", "collection").finish();
    let req = Request::builder()
        .uri(format!("/api/search?{qs}"))
        .header("authorization", "Bearer sess-1")
        .extension(ConnectInfo(SocketAddr::new("127.0.0.9".parse::<IpAddr>().unwrap(), 5555)))
        .body(Body::empty())
        .unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let st = res.status();
    let h = res.headers().clone();
    let b = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (st, h, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn entry_ids(v: &Value) -> Vec<i64> {
    v.as_array().unwrap_or(&vec![]).iter().map(|c| c["entry_id"].as_i64().unwrap()).collect()
}

#[tokio::test]
async fn upstream_mode_full_grammar_answers_locally() {
    let h = harness(None).await;
    // rarity comparator + mana value comparator: outside the old subset.
    let (st, hd, v) = get_search(&h.router, "r>=rare mv<=4").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(entry_ids(&v), vec![6, 4, 1], "newest-first owned rows");
    assert_eq!(hd["x-catalog-cache"], "local");
    assert_eq!(hd["x-total-count"], "3");
    assert!(hd.get("x-source").is_none());
    // OR / negation / grouping / set / number / lang.
    let (st, _, v) = get_search(&h.router, "(s:lea or cn:146) -t:artifact lang:en").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(entry_ids(&v), vec![3, 2]);
    let (st, _, v) = get_search(&h.router, "!\"lightning bolt\" or is:planeswalker").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(entry_ids(&v), vec![6, 3, 2]);
}

#[tokio::test]
async fn upstream_mode_parser_reject_is_invalid_query() {
    let h = harness(None).await;
    for q in ["c:zz", "mv:abc", "t>elf", "a and", "-or"] {
        let (st, _, v) = get_search(&h.router, q).await;
        assert_eq!((st, v), (StatusCode::BAD_REQUEST, json!({"error":"INVALID_QUERY"})), "{q}");
    }
}

#[tokio::test]
async fn sidecar_mode_printing_terms_walk_by_id() {
    let h = harness(Some(true)).await;
    let (st, hd, v) = get_search(&h.router, "s:lea").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // ownedByCardIds: exact printings only — the m10 Bolt (entry 3) is out.
    assert_eq!(h.hits.lock().unwrap().as_slice(), ["s:lea unique:prints"]);
    assert_eq!(entry_ids(&v), vec![2, 1], "{hd:?}");
    assert_eq!(hd["x-source"], "sidecar");
}

#[tokio::test]
async fn sidecar_mode_normalizes_scrybox_extensions() {
    let h = harness(Some(true)).await;
    let (st, _, v) = get_search(&h.router, "is:basic-land OR (r:R -C:G) unique:prints").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // toScryfall(stripDirectives(ast)) + unique:cards (game-card walk).
    assert_eq!(h.hits.lock().unwrap().as_slice(), ["t:\"basic land\" or (r:R -c:G) unique:cards"]);
}

#[tokio::test]
async fn sidecar_down_local_query_falls_back_loudly() {
    let h = harness(Some(false)).await;
    let (st, hd, v) = get_search(&h.router, "t:elf or t:goblin").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(entry_ids(&v), vec![5, 4]);
    assert_eq!(hd["x-catalog-cache"], "local-fallback");
    assert_eq!(hd["x-source"], "local-fallback");
    assert_eq!(
        hd["access-control-expose-headers"],
        "X-Total-Count, X-Catalog-Complete, X-Catalog-Upstream-Total, X-Catalog-Cache, X-Catalog-Snapshot, X-Source"
    );
}

#[tokio::test]
async fn sidecar_down_catalog_query_is_503() {
    let h = harness(Some(false)).await;
    let (st, _, v) = get_search(&h.router, "o:flying").await;
    assert_eq!(
        (st, v),
        (StatusCode::SERVICE_UNAVAILABLE, json!({"error":"Card API is having trouble. Try again in a moment."}))
    );
}
