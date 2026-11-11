//! Native read side of `/api/card-art` (Lane B unit 2b): `GET /index` and
//! `GET /:cardId.png` — ports of `routes/cardArt.js` + `cardArt.js`.
//!
//! Reads are unauthenticated on purpose (same comment as Node): a shared
//! collection page renders card images without a session. The mutating
//! surface (POST upload with the sharp/libvips re-encode, DELETE) stays on
//! Node for now: sharp's PNG encoder output cannot be byte-reproduced in
//! Rust, and DELETE would poison Node's process-local `listIds` cache while
//! upload still lives there. Both therefore keep proxying untouched.
//!
//! Node caches `listIds()` process-wide and only invalidates on its own
//! save/remove. Since uploads mutate disk through Node, Rust must always
//! answer from a fresh scan (never cached) so the two sides converge after
//! every mutation the harness performs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value};

use crate::AppState;

fn json_response(status: StatusCode, v: &Value) -> Response {
    let bytes = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=0, must-revalidate"),
            ),
        ],
        Body::from(bytes),
    )
        .into_response()
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    json_response(status, &Value::Object({
        let mut m = Map::new();
        m.insert("error".into(), Value::String(msg.to_string()));
        m
    }))
}

/// `cardArt.js` `ID_RE` + the explicit `..` refusal. The id becomes a
/// filename, so anything beyond this shape is refused rather than escaped.
pub fn is_valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() || b.len() > 128 || id.contains("..") {
        return false;
    }
    if !(b[0] as char).is_ascii_alphanumeric() {
        return false;
    }
    b.iter().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-')
    })
}

/// `USER_DIR` — dirname(dbPath)/card-art, the same host directory the Node
/// container sees through the shared volume bind.
pub fn user_dir(st: &AppState) -> Option<PathBuf> {
    st.cfg.db_path.as_ref()?.parent().map(|p| p.join("card-art"))
}

/// `BUNDLED_DIR` — <shared>/card-art, the repo layer Node reads from
/// `/app/shared/card-art`. Absent directory = no art (Node's readdirSync
/// catch treats it the same).
pub fn bundled_dir(st: &AppState) -> Option<PathBuf> {
    st.shared_dir.as_ref().map(|s| s.join("card-art"))
}

fn file_for(dir: &Path, card_id: &str) -> PathBuf {
    dir.join(format!("{card_id}.png"))
}

/// `cardArt.resolve` — user layer wins over bundled.
pub fn resolve(st: &AppState, card_id: &str) -> Option<PathBuf> {
    if !is_valid_id(card_id) {
        return None;
    }
    for dir in [user_dir(st), bundled_dir(st)].into_iter().flatten() {
        let f = file_for(&dir, card_id);
        if f.exists() {
            return Some(f);
        }
    }
    None
}

/// One directory's `.png` ids. `fs.readdirSync` returns names in
/// code-unit-sorted order (verified against this Node build), so sort by
/// Rust's byte order (equivalent for the UTF-8 names ids use) — an
/// unsorted `read_dir` would diverge in the `ids` array order.
fn dir_ids(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .filter(|n| n.to_lowercase().ends_with(".png"))
            .collect(),
        Err(_) => return Vec::new(), // absent dir = no art
    };
    // Sort the FULL names (what readdirSync sorts), then drop the extension.
    names.sort();
    names.into_iter().map(|n| n[..n.len() - 4].to_string()).collect()
}

/// `cardArt.listIds` — bundled first, then user, first-seen order kept
/// (JS Set insertion order), always recomputed (see module comment).
pub fn list_ids(st: &AppState) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for dir in [bundled_dir(st), user_dir(st)].into_iter().flatten() {
        for id in dir_ids(&dir) {
            if seen.insert(id.clone()) {
                out.push(id);
            }
        }
    }
    out
}

/// `GET /api/card-art/index` — `{ ids: [...] }`. Node's listIds never
/// throws (both readdir legs are caught), so there is no error branch.
pub async fn handle_index(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::GET || !st.owns("catalog_art") {
        return crate::proxy(State(st), req).await;
    }
    let v = Value::Object({
        let mut m = Map::new();
        m.insert("ids".into(), Value::Array(list_ids(&st).into_iter().map(Value::String).collect()));
        m
    });
    json_response(StatusCode::OK, &v)
}

/// `GET /api/card-art/:cardId.png` — art bytes with the same cache header.
pub async fn handle_png(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::GET || !st.owns("catalog_art") {
        return crate::proxy(State(st), req).await;
    }
    let path = req.uri().path().to_string();
    let raw_id = match path
        .strip_prefix("/api/card-art/")
        .and_then(|p| p.strip_suffix(".png"))
    {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => return crate::proxy(State(st.clone()), req).await,
    };
    // Express URIError on a malformed % escape surfaces through the global
    // error handler (same convention as cards_tags).
    let card_id = match crate::cards_tags::decode_uri_component(&raw_id) {
        Some(s) => s,
        None => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    let file = match resolve(&st, &card_id) {
        Some(f) => f,
        None => return json_error(StatusCode::NOT_FOUND, "No art for this card"),
    };
    match std::fs::read(&file) {
        Ok(bytes) => {
            let len = bytes.len();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("public, max-age=0, must-revalidate"),
                    ),
                    (header::CONTENT_LENGTH, HeaderValue::from(len)),
                ],
                Body::from(bytes),
            )
                .into_response()
        }
        Err(e) => {
            // sendFile streams from disk; an unreadable file rides Express's
            // error middleware — same 500 shape as every other ported group.
            tracing::error!(error = %e, path = %file.display(), "card art read failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }
    }
}
