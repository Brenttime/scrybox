//! REVIEW-auth.md H1: request bodies must never be buffered beyond the
//! route's Node body-parser ceiling (backend/src/server.js:204-205,460-464).
//!
//! Each test streams a LARGE chunked body (no content-length, so the read
//! must stop on accumulated size) through a counting request stream and
//! asserts BOTH the Node-parity status AND that the handler stopped
//! consuming at (cap + one frame of slack) — pre-fix the shared
//! read_parts/read_body helpers ran to_bytes(.., usize::MAX) and buffered
//! the entire hostile stream first, which is exactly the memory the
//! review flagged. The consumed-bytes assertion is the red-before-fix leg;
//! the status legs pin the parity behaviour (413 custom body for the
//! parser-oversize arms, auth ordering on bigJson paths).

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::{header, HeaderValue, Method, StatusCode},
};
use futures_util::Stream;
use http_body::Frame;
use rusqlite::Connection;
use tower::ServiceExt;

use scrybox_server::{app, AppState, Config};

const KB: usize = 1024;
const MB: usize = 1024 * KB;

/// Lazy chunked stream handing out `data` in `frame_len` slices; counts
/// every byte actually handed to the reader (never more than what is
/// polled), so it proves the reader ABANDONED the stream at the cap.
struct CountingStream {
    data: Vec<u8>,
    pos: usize,
    frame_len: usize,
    consumed: Arc<AtomicUsize>,
}

impl Stream for CountingStream {
    type Item = Result<Frame<Bytes>, io::Error>;
    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.pos >= this.data.len() {
            return Poll::Ready(None);
        }
        let end = (this.pos + this.frame_len).min(this.data.len());
        let n = end - this.pos;
        this.consumed.fetch_add(n, Ordering::SeqCst);
        let chunk = Bytes::from(this.data[this.pos..end].to_vec());
        this.pos = end;
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }
}

fn counting_body(data: Vec<u8>, frame_len: usize, consumed: &Arc<AtomicUsize>) -> Body {
    let pos = 0;
    Body::new(http_body_util::StreamBody::new(CountingStream {
        data,
        pos,
        frame_len,
        consumed: consumed.clone(),
    }))
}

/// A syntactically valid JSON object of EXACTLY `len` bytes: {"a":"aaa…"}
/// (used to keep the parser arm green at the exact 1 MiB boundary).
fn json_of_len(len: usize) -> Vec<u8> {
    let prefix = b"{\"a\":\"";
    let suffix = b"\"}";
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(prefix);
    while v.len() + suffix.len() < len {
        v.push(b'a');
    }
    v.extend_from_slice(suffix);
    v
}

fn seeded_db(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("bindarr.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    // Same stubs as tests/collection.rs so the prod revision triggers apply.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS collection_snapshot (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS deck_list_revision (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
         INSERT OR IGNORE INTO collection_snapshot (id, revision) VALUES (1, 0);
         INSERT OR IGNORE INTO deck_list_revision (id, revision) VALUES (1, 0);",
    )
    .unwrap();
    conn.execute_batch(include_str!("schema_fixture.sql")).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, role, share_token)
           VALUES (1, 'admin', 'x', 'admin', 'share-admin');
         INSERT INTO sessions (token, user_id, expires_at)
           VALUES ('tok-session', 1, '2999-01-01');",
    )
    .unwrap();
    path
}

async fn send_stream(
    db_path: std::path::PathBuf,
    routes: &[&str],
    method: Method,
    path: &str,
    ct: &str,
    data: Vec<u8>,
    frame_len: usize,
) -> (StatusCode, Vec<u8>, usize, Option<String>) {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: "http://127.0.0.1:1".to_string(), // dead: a proxy leg would 502
        static_dir: None,
        rust_routes: routes.iter().map(|s| s.to_string()).collect(),
        db_path: Some(db_path),
        sidecar_url: None,
    };
    let consumed = Arc::new(AtomicUsize::new(0));
    let body = counting_body(data, frame_len, &consumed);
    // NOTE: deliberately NO content-length header — chunked semantics, so
    // the cap must be enforced on ACCUMULATED bytes, not the size hint.
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap())
        .body(body)
        .unwrap();
    // search::handle extracts ConnectInfo (rate-limiter sibling wiring);
    // bare oneshot calls reject 500 without it — supply the extension.
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo::<std::net::SocketAddr>(
            "127.0.0.1:5555".parse().unwrap(),
        ));
    let res = app(AppState::new(cfg)).oneshot(req).await.unwrap();
    let (parts, rb) = res.into_parts();
    let bytes = axum::body::to_bytes(rb, 10 * MB).await.unwrap().to_vec();
    (
        parts.status,
        bytes,
        consumed.load(Ordering::SeqCst),
        parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string),
    )
}

// ---------------------------------------------------------------------------
// normalJson family (collection_writes::read_parts, 1 MiB, parser BEFORE auth)
// ---------------------------------------------------------------------------

const HOSTILE: usize = 16; // 16 x 1 MiB frames
const SLACK: usize = 3 * MB; // cap + generous frame-granularity headroom

#[tokio::test]
async fn cardlist_get_json_oversize_413_before_auth_bounded_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    let (s, body, consumed, _ct) = send_stream(
        db,
        &["collection_writes"],
        Method::GET,
        "/api/collection/cardlist",
        "application/json",
        vec![b'x'; HOSTILE * MB],
        MB,
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "body: {}/{} B", String::from_utf8_lossy(&body), body.len());
    assert!(
        consumed <= SLACK,
        "read_parts buffered {consumed} bytes — the body-parser ceiling is 1 MiB (hostile stream was {} B)", HOSTILE * MB
    );
    // Node's server.js:638-639 error-middleware 413 body.
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"], "Upload too large. Try exporting/importing in smaller batches.");
}

#[tokio::test]
async fn bulk_post_json_oversize_413_before_auth_bounded_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    let (s, body, consumed, _ct) = send_stream(
        db,
        &["collection_writes"],
        Method::POST,
        "/api/collection/bulk",
        "application/json",
        vec![b'x'; HOSTILE * MB],
        MB,
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "body: {}", String::from_utf8_lossy(&body));
    assert!(consumed <= SLACK, "buffered {consumed} bytes past the 1 MiB normalJson ceiling");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"], "Upload too large. Try exporting/importing in smaller batches.");
}

/// Boundary parity with body-parser (`> limit` → 413, `== limit` passes the
/// parser): exactly 1 MiB must sail past the oversize arm and meet the AUTH
/// arm instead (401 no token) — guards against an off-by-one that 413s
/// legitimate at-limit payloads.
#[tokio::test]
async fn bulk_post_json_at_limit_not_413() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    let (s, body, consumed, _ct) = send_stream(
        db,
        &["collection_writes"],
        Method::POST,
        "/api/collection/bulk",
        "application/json",
        json_of_len(MB), // exactly 1 MiB == limit → parser passes, auth answers
        MB,
    )
    .await;
    assert_ne!(s, StatusCode::PAYLOAD_TOO_LARGE, "1 MiB == limit must pass the parser ({})", String::from_utf8_lossy(&body));
    assert_eq!(s, StatusCode::UNAUTHORIZED, "at-limit body must reach the auth arm");
    assert!(consumed <= SLACK);
}

/// Non-JSON content-types skip express.json entirely in Node: no 413, the
/// route sees `req.body === undefined`. Our abandoned (empty) body must
/// produce the SAME answer as Node's parser skip — pinned here through the
/// bulk route's non-JSON arm (documented {} deviation, not 413).
#[tokio::test]
async fn bulk_post_nonjson_oversize_is_not_413() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    let (s, _body, consumed, _ct) = send_stream(
        db,
        &["collection_writes"],
        Method::POST,
        "/api/collection/bulk",
        "text/plain",
        vec![b'x'; HOSTILE * MB],
        MB,
    )
    .await;
    assert_ne!(s, StatusCode::PAYLOAD_TOO_LARGE, "parser-skipped content-type must never 413");
    assert!(consumed <= SLACK, "stream must still be abandoned at the cap ({consumed} B read)");
}

// ---------------------------------------------------------------------------
// bigJson family (15 MiB, parser AFTER the auth gate)
// ---------------------------------------------------------------------------

/// POST /api/search is a bigJson path: authenticateToken (server.js:454)
/// runs BEFORE bigJson (:462), so an UNAUTHENTICATED hostile stream must
/// meet the 401 — and the stream must still be abandoned at ~15 MiB, never
/// fully buffered (pre-fix: 20 MiB buffered before the 401).
#[tokio::test]
async fn search_post_unauth_oversize_401_not_413_bounded_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_db(dir.path());
    let (s, _body, consumed, _ct) = send_stream(
        db,
        &["search"],
        Method::POST,
        "/api/search",
        "application/json",
        vec![b'x'; 20 * MB],
        MB,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "bigJson paths authenticate before parsing");
    assert!(
        consumed <= 16 * MB,
        "search buffered {consumed} bytes — bigJson ceiling is 15 MiB"
    );
}
