//! Lane B unit 6c: utils/modelAssets.js twin — the scan engine's
//! downloadable pieces (models + published catalog) behind
//! `RUST_ROUTES=catalog_build`:
//!
//!   GET  /api/admin/models          status (assets, progress, last)
//!   POST /api/admin/models/download start (409 'already running'/unknown what)
//!
//! One download at a time (module-global `current`), stream-to-`.tmp` +
//! rename, exact-size assertion, `cvScan.reload('mtg')` on success. HF is a
//! hardcoded base — the download legs are exercised against an unreachable
//! host in tests (phase='error'), never the live CDN.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode, Uri},
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::cards_tags::json_response;
use crate::collection_writes::node_exact;
use crate::cvscan;
use crate::AppState;

const HF: &str = "https://huggingface.co";

struct Asset {
    name: &'static str,
    repo: &'static str,
    file: &'static str,
    bytes: u64,
    game: Option<&'static str>,
    snapshot: Option<&'static str>,
}

const MODELS: [Asset; 2] = [
    Asset {
        name: "cornelius.onnx",
        repo: "HanClinto/ccgdetector-fastweb-single",
        file: "fastweb-single-1.39.onnx",
        bytes: 3_185_226,
        game: None,
        snapshot: None,
    },
    Asset {
        name: "milo.onnx",
        repo: "HanClinto/milo",
        file: "model.onnx",
        bytes: 5_191_100,
        game: None,
        snapshot: None,
    },
];

const CATALOGS: [Asset; 1] = [Asset {
    name: "milo-mtg.npz",
    repo: "HanClinto/milo",
    file: "catalogs/milo1-scryfall-mtg-2026-07-09.npz",
    bytes: 56_252_182,
    game: Some("mtg"),
    snapshot: Some("2026-07-09"),
}];

fn license() -> Value {
    let mut m = Map::new();
    m.insert("spdx".into(), "MIT AND AGPL-3.0-only".into());
    m.insert(
        "urls".into(),
        Value::Array(
            [
                "https://huggingface.co/HanClinto/ccgdetector-fastweb-single",
                "https://huggingface.co/HanClinto/milo",
            ]
            .map(Value::from)
            .to_vec(),
        ),
    );
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// Job state (modelAssets.js `current` / `last`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct DlJob {
    what: String,
    name: String,
    done: u64,
    total: u64,
    phase: String,
    message: String,
}

impl DlJob {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("what".into(), self.what.clone().into());
        m.insert("name".into(), self.name.clone().into());
        m.insert("done".into(), self.done.into());
        m.insert("total".into(), self.total.into());
        m.insert("phase".into(), self.phase.clone().into());
        m.insert("message".into(), self.message.clone().into());
        Value::Object(m)
    }
}

static CURRENT: Mutex<Option<DlJob>> = Mutex::new(None);
static LAST: Mutex<Option<Value>> = Mutex::new(None);

fn with_current<F: FnOnce(&mut DlJob)>(f: F) {
    if let Some(j) = CURRENT.lock().unwrap().as_mut() {
        f(j);
    }
}

/// modelAssets `state()`.
pub fn state() -> Value {
    CURRENT.lock().unwrap().as_ref().map(|j| j.to_value()).unwrap_or(Value::Null)
}

/// modelAssets `lastResult()`.
pub fn last_result() -> Value {
    LAST.lock().unwrap().clone().unwrap_or(Value::Null)
}

fn is_present(dir: &Path, a: &Asset) -> bool {
    std::fs::metadata(dir.join(a.name)).map(|m| m.len() == a.bytes).unwrap_or(false)
}

fn model_dir(st_model: Option<&PathBuf>) -> Option<PathBuf> {
    st_model.cloned().or_else(cvscan::model_dir_pub)
}

/// modelAssets `status()` — key order dir, license, models, catalogs,
/// progress, last.
pub fn status(st_model: Option<&PathBuf>) -> Value {
    let dir = model_dir(st_model);
    let dir_display = dir
        .as_ref()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    let present = |a: &Asset| dir.as_ref().map(|d| is_present(d, a)).unwrap_or(false);
    let mut m = Map::new();
    m.insert("dir".into(), dir_display.into());
    m.insert("license".into(), license());
    m.insert(
        "models".into(),
        MODELS
            .iter()
            .map(|a| {
                let mut e = Map::new();
                e.insert("name".into(), a.name.into());
                e.insert("bytes".into(), a.bytes.into());
                e.insert("present".into(), present(a).into());
                Value::Object(e)
            })
            .collect::<Vec<_>>()
            .into(),
    );
    m.insert(
        "catalogs".into(),
        CATALOGS
            .iter()
            .map(|a| {
                let mut e = Map::new();
                e.insert("name".into(), a.name.into());
                e.insert("game".into(), a.game.into());
                e.insert("bytes".into(), a.bytes.into());
                e.insert("snapshot".into(), a.snapshot.into());
                e.insert("present".into(), present(a).into());
                Value::Object(e)
            })
            .collect::<Vec<_>>()
            .into(),
    );
    m.insert("progress".into(), state());
    m.insert("last".into(), last_result());
    Value::Object(m)
}

/// modelAssets `start(what)` — Ok(state snapshot) or Err(message) for 409.
pub fn start(what: &str, st_model: Option<PathBuf>) -> Result<Value, String> {
    let what = what.to_string();
    let mut cur = CURRENT.lock().unwrap();
    if cur.is_some() {
        return Err("a download is already running".into());
    }
    let wanted: Vec<&'static Asset> = if what == "models" {
        MODELS.iter().collect()
    } else {
        CATALOGS.iter().filter(|c| format!("catalog:{}", c.game.unwrap_or("")) == what).collect()
    };
    if wanted.is_empty() {
        return Err(format!("nothing to download for \"{what}\""));
    }
    let job = DlJob {
        what: what.to_string(),
        name: wanted[0].name.to_string(),
        done: 0,
        total: wanted.iter().map(|a| a.bytes).sum(),
        phase: "downloading".into(),
        message: String::new(),
    };
    let reply = job.to_value();
    *cur = Some(job);
    drop(cur);

    let dir = model_dir(st_model.as_ref());
    let wanted_n = wanted.len();
    tokio::spawn(async move {
        let mut base = 0u64;
        let mut fetched = 0usize;
        let result: Result<(), String> = async {
            let dir = dir.ok_or("no CV_MODEL_DIR")?;
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            for a in wanted.iter().copied() {
                with_current(|j| j.name = a.name.to_string());
                let r = fetch_asset(&dir, a, |got| with_current(|j| j.done = base + got)).await?;
                if r == "fetched" {
                    fetched += 1;
                }
                base += a.bytes;
                with_current(|j| j.done = base);
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                with_current(|j| {
                    j.phase = "done".into();
                    j.message =
                        format!("{fetched} file(s) downloaded, {} already present", wanted_n - fetched);
                });
                // A newly installed catalog or model has to be picked up
                // without a restart.
                cvscan::reload("mtg", "");
            }
            Err(e) => {
                tracing::error!("model download failed ({what}): {e}");
                with_current(|j| {
                    j.phase = "error".into();
                    j.message = e;
                });
            }
        }
        let snap = {
            let mut m = CURRENT.lock().unwrap().as_ref().map(|j| j.to_value()).unwrap_or(Value::Null);
            if let Some(o) = m.as_object_mut() {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                o.insert("finishedAt".into(), ms.into());
            }
            m
        };
        *LAST.lock().unwrap() = Some(snap);
        *CURRENT.lock().unwrap() = None;
    });
    Ok(reply)
}

/// fetchAsset: stream to `.tmp`, assert exact size, rename. 'present' short
/// circuits. On any failure the tmp file is removed.
async fn fetch_asset(dir: &Path, a: &'static Asset, on_progress: impl FnMut(u64)) -> Result<&'static str, String> {
    let dest = dir.join(a.name);
    if std::fs::metadata(&dest).map(|m| m.len() == a.bytes).unwrap_or(false) {
        return Ok("present");
    }
    let url = format!("{HF}/{}/resolve/main/{}", a.repo, a.file);
    let tmp = format!("{}.tmp", dest.to_string_lossy());
    let result: Result<(u64, Vec<u8>), String> = async {
        let mut url = url;
        let body = 'redirects: {
            for _ in 0..10 {
                let uri = url.parse::<Uri>().map_err(|e| e.to_string())?;
                let req = hyper::Request::builder()
                    .method(hyper::Method::GET)
                    .uri(uri)
                    .header(header::USER_AGENT, HeaderValue::from_static("Mozilla/5.0"))
                    .body(String::new())
                    .map_err(|e| e.to_string())?;
                let res = dl_client().request(req).await.map_err(|e| e.to_string())?;
                let status = res.status();
                if status.is_redirection() {
                    if let Some(loc) = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()) {
                        url = crate::catalog_build::join_uri(&url, loc);
                        continue;
                    }
                }
                if !status.is_success() {
                    return Err(format!("HTTP {} fetching {}", status.as_u16(), a.name));
                }
                break 'redirects res.into_body();
            }
            return Err("too many redirects".to_string());
        };
        let bytes = body.collect().await.map_err(|e| e.to_string())?.to_bytes();
        Ok((bytes.len() as u64, bytes.to_vec()))
    }
    .await;
    let (got, buf) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    {
        // Progress like the streaming writer: chunk-fed ticks after the body
        // is in hand (counters advance identically for the poll loop).
        let mut on_progress = on_progress;
        let mut got_tick = 0u64;
        for chunk in buf.chunks(64 * 1024) {
            got_tick += chunk.len() as u64;
            on_progress(got_tick);
        }
    }
    if got != a.bytes {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{} came back {got} bytes, expected {} — refusing to install it", a.name, a.bytes));
    }
    std::fs::write(&tmp, &buf).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    Ok("fetched")
}

type DlClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    String,
>;

fn dl_client() -> &'static DlClient {
    use std::sync::OnceLock;
    static C: OnceLock<DlClient> = OnceLock::new();
    C.get_or_init(|| {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("native roots")
            .https_or_http()
            .enable_http1()
            .build();
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_max_idle_per_host(2)
            .build(https)
    })
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn json_error(status: StatusCode, msg: &str) -> Response {
    let mut m = Map::new();
    m.insert("error".into(), msg.into());
    json_response(status, &Value::Object(m))
}

/// GET /api/admin/models + POST /api/admin/models/download.
pub async fn handle_models(State(st): State<AppState>, mut req: Request) -> Response {
    if !(st.db.is_some() && (st.cfg.rust_routes.contains("*") || st.cfg.rust_routes.contains("catalog_build"))) {
        return crate::proxy(State(st), req).await;
    }
    let path = req.uri().path().to_string();
    let is_get = req.method() == axum::http::Method::GET;

    // normalJson ran before the auth gate (server.js chain); parse, Forward
    // on strict-parse failures so Node renders its own page.
    let body_val: Value = if req.method() == axum::http::Method::POST {
        let (mut parts, body) = req.into_parts();
        // Capped stream read at normalJson's 1mb ceiling (REVIEW-auth.md
        // H1). A content-length above the cap (body-parser's immediate 413
        // arm) answers here for JSON content-types — pre-auth, as Node's
        // global normalJson runs before the auth gate. Non-JSON skips
        // body-parser entirely: the abandoned (empty) body under a cleared
        // content-length is the equivalent request. A chunked stream error
        // past the cap forwards body-less, where Node renders its own
        // verdict (its parser sees the real size and 413s JSON itself).
        let cl_over_cap = parts
            .headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n > crate::collection_writes::BODY_NORMAL)
            .unwrap_or(false);
        let bytes = match axum::body::to_bytes(body, crate::collection_writes::BODY_NORMAL + 1).await {
            Ok(b) => b,
            Err(_) if cl_over_cap && crate::collection_writes::is_json_ct(&parts.headers) => {
                return crate::collection_writes::too_large();
            }
            Err(_) if cl_over_cap => {
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                axum::body::Bytes::new()
            }
            Err(_) => {
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                return crate::proxy(State(st), Request::from_parts(parts, Body::empty())).await;
            }
        };
        match crate::cardscan::parse_outer_json(&parts.headers, &bytes) {
            Ok(v) => {
                req = Request::from_parts(parts, Body::from(bytes.to_vec()));
                v
            }
            Err(()) => {
                let fwd = Request::from_parts(parts, Body::from(bytes.to_vec()));
                return crate::proxy(State(st), fwd).await;
            }
        }
    } else {
        Value::Object(Map::new())
    };

    let pool = st.db.clone().unwrap();
    let headers = req.headers().clone();
    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), is_get) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    if user.via_api_key {
        return json_error(
            StatusCode::FORBIDDEN,
            "API keys cannot access admin endpoints. Use a login session.",
        );
    }
    if user.role.as_deref() != Some("admin") {
        return json_error(StatusCode::FORBIDDEN, "Access denied. Administrator privileges required.");
    }

    match (req.method().clone(), path.as_str()) {
        (axum::http::Method::GET, "/api/admin/models") => json_response(StatusCode::OK, &status(st.model_dir.as_ref())),
        (axum::http::Method::POST, "/api/admin/models/download") => {
            let what = body_val
                .get("what")
                .map(|v| crate::catalog_build::js_to_string_pub(Some(v)))
                .unwrap_or_else(|| "models".to_string());
            match start(&what, st.model_dir.clone()) {
                Ok(progress) => {
                    let mut m = Map::new();
                    m.insert("progress".into(), progress);
                    json_response(StatusCode::OK, &Value::Object(m))
                }
                Err(e) => {
                    let mut m = Map::new();
                    m.insert("error".into(), e.into());
                    m.insert("progress".into(), state());
                    json_response(StatusCode::CONFLICT, &Value::Object(m))
                }
            }
        }
        _ => {
            let (parts, _) = req.into_parts();
            crate::proxy(State(st), Request::from_parts(parts, Body::empty())).await
        }
    }
}
