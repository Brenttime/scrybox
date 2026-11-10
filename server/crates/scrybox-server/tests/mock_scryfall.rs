//! Tiny scripted Scryfall stand-in for Lane B tests. One axum server per
//! test; responses (and per-endpoint hit counts + request bodies) come from
//! shared state so assertions can pin the contract the jobs rely on:
//! GET /sets, POST /cards/collection (75-identifier batches), GET
//! /cards/search (oracleid floor fills).

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use serde_json::Value;

#[derive(Default)]
pub struct MockState {
    /// GET /sets response body.
    pub sets: Mutex<Value>,
    /// POST /cards/collection responses, one per batch (last repeats).
    pub collection_batches: Mutex<Vec<Value>>,
    /// GET /cards/search response body.
    pub search: Mutex<Value>,
    /// Per-key status scripts: "GET /sets" -> [429, 200] (last entry
    /// repeats once the script is exhausted).
    pub statuses: Mutex<HashMap<String, Vec<u16>>>,
    /// Retry-After value for scripted 429s.
    pub retry_after: Mutex<Option<String>>,
    /// Every request: (METHOD path?query, raw body).
    pub hits: Mutex<Vec<(String, String)>>,
}

impl MockState {
    pub fn hits_for(&self, prefix: &str) -> Vec<(String, String)> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .cloned()
            .collect()
    }
    fn next_status(&self, key: &str) -> u16 {
        // The current hit is already recorded when this runs: index with
        // count-1 so a script [429, 200] serves 429 first, 200 after.
        let hits = self.hits.lock().unwrap().iter().filter(|(k, _)| k == key).count().saturating_sub(1);
        let script = self.statuses.lock().unwrap();
        match script.get(key) {
            Some(v) if !v.is_empty() => *v.get(hits).or_else(|| v.last()).unwrap(),
            _ => 200,
        }
    }
}

fn json_resp(status: StatusCode, body: &Value, headers: Option<(&str, String)>) -> Response {
    let mut b = Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    if let Some((k, v)) = headers {
        b = b.header(k, v);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

pub async fn spawn_mock() -> (SocketAddr, Arc<MockState>) {
    let state = Arc::new(MockState::default());
    *state.sets.lock().unwrap() = serde_json::json!({"data": []});
    *state.search.lock().unwrap() = serde_json::json!({"data": [], "has_more": false});

    let app_state = state.clone();
    let router = Router::new().fallback(move |req: Request| {
        let state = app_state.clone();
        async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let query = req.uri().query().unwrap_or("").to_string();
            let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            let body = String::from_utf8_lossy(&body_bytes).to_string();
            let key = format!("{method} {path}");
            let hit_key = if query.is_empty() { key.clone() } else { format!("{key}?{query}") };
            state.hits.lock().unwrap().push((hit_key, body.clone()));
            let status = state.next_status(&key);
            if status == 429 {
                let ra = state.retry_after.lock().unwrap().clone();
                return json_resp(StatusCode::TOO_MANY_REQUESTS, &serde_json::json!({"status_code": 429}), ra.map(|v| (axum::http::header::RETRY_AFTER.as_str(), v)));
            }
            if status >= 400 {
                return json_resp(
                    StatusCode::from_u16(status).unwrap(),
                    &serde_json::json!({"status_code": status, "details": "mock"}),
                    None,
                );
            }
            match (method.as_str(), path.as_str()) {
                ("GET", "/sets") => {
                    let body = state.sets.lock().unwrap().clone();
                    json_resp(StatusCode::OK, &body, None)
                }
                ("POST", "/cards/collection") => {
                    // The real API takes identifier OBJECTS ({id} |
                    // {set,collector_number} | {name}) and 400s anything
                    // else — mirror that or the port could regress to the
                    // string form the tests once (wrongly) blessed.
                    let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let bad = req["identifiers"]
                        .as_array()
                        .map(|a| a.iter().any(|v| !v.is_object()))
                        .unwrap_or(true);
                    if bad {
                        return json_resp(
                            StatusCode::BAD_REQUEST,
                            &serde_json::json!({ "status_code": 400, "details": "identifier must be an object" }),
                            None,
                        );
                    }
                    let mut batches = state.collection_batches.lock().unwrap();
                    let idx = state
                        .hits
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(k, _)| k.starts_with("POST /cards/collection"))
                        .count()
                        .saturating_sub(1);
                    let body = batches
                        .get(idx)
                        .or_else(|| batches.last())
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"data": [], "not_found": []}));
                    json_resp(StatusCode::OK, &body, None)
                }
                ("GET", "/cards/search") => {
                    let body = state.search.lock().unwrap().clone();
                    json_resp(StatusCode::OK, &body, None)
                }
                (m, p) => json_resp(
                    StatusCode::NOT_FOUND,
                    &serde_json::json!({"status_code": 404, "mock": format!("{m} {p}")}),
                    None,
                ),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (addr, state)
}

pub fn scry_for(addr: SocketAddr, gap_scale: f64) -> scrybox_server::scryfall::Scry {
    scrybox_server::scryfall::Scry::new(scrybox_server::scryfall::ScryEnv {
        local: false,
        local_url: String::new(),
        base_url: format!("http://{addr}"),
        gap_scale,
    })
}

/// A raw Scryfall card object (subset the ports read).
pub fn raw_card(id: &str, name: &str, set: &str, number: &str, usd: Option<f64>) -> Value {
    serde_json::json!({
        "id": id,
        "name": name,
        "set": set,
        "set_name": format!("{set} Set"),
        "collector_number": number,
        "lang": "en",
        "layout": "normal",
        "rarity": "common",
        "type_line": format!("Creature — Human {name}"),
        "colors": ["W"],
        "color_identity": ["W"],
        "cmc": 2,
        "image_uris": {"normal": format!("https://cards.example/{id}.png")},
        "prices": usd.map(|u| serde_json::json!({"usd": u.to_string(), "usd_foil": null, "usd_etched": null})).unwrap_or(Value::Null),
        "oracle_text": "Test text.",
    })
}
