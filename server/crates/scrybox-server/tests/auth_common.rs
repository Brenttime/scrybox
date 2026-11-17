//! Shared helpers for the auth test binaries (included via #[path]).

use std::{net::SocketAddr, sync::Arc};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, Method},
};

use tower::ServiceExt;

use scrybox_server::{auth_support::uuid_v4, AppState, Config};

pub fn schema_fixture() -> &'static str {
    include_str!("./schema_fixture.sql")
}

pub fn build_router(routes: &[&str]) -> (axum::Router, Arc<scrybox_server::db::DbPool>) {
    let dir = std::env::temp_dir().join(format!("scrybox-auth-it-{}", uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    // DbPool opens READ_WRITE without CREATE — create the empty file first
    // (SQLite treats a zero-length file as a fresh database).
    std::fs::write(dir.join("bindarr.db"), []).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: "http://127.0.0.1:1".to_string(),
        static_dir: Some(dir.clone()),
        rust_routes: routes.iter().map(|s| s.to_string()).collect(),
        db_path: Some(dir.join("bindarr.db")),
        sidecar_url: None,
    };
    let st = AppState::new(cfg);
    let pool = st.db.clone().unwrap();
    pool.get().unwrap().execute_batch(schema_fixture()).unwrap();
    (scrybox_server::app(st.clone()), pool)
}

pub async fn request(
    router: &axum::Router,
    peer: SocketAddr,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
    body: Option<String>,
) -> (axum::http::StatusCode, HeaderMap, String) {
    let mut rb = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    for (k, v) in headers {
        rb = rb.header(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    let mut req = rb.body(body.map(Body::from).unwrap_or_else(Body::empty)).unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

pub fn peer() -> SocketAddr {
    "203.0.113.9:5555".parse().unwrap()
}

pub fn bearer(token: &str) -> Vec<(&'static str, String)> {
    vec![("authorization", format!("Bearer {token}"))]
}

pub fn json_ct() -> Vec<(&'static str, String)> {
    vec![("content-type", "application/json".into())]
}

pub fn merge(
    a: &[(&'static str, String)],
    b: &[(&'static str, String)],
) -> Vec<(&'static str, String)> {
    a.iter().chain(b.iter()).cloned().collect()
}
