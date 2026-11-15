//! TOTORO mono: the local Scryfall engine (`local_scryfall` crate) served
//! IN-PROCESS instead of from the `scryfall-local` sidecar container.
//!
//! Architecture
//! ------------
//! Every leg that used to talk HTTP to the sidecar (Scry client with
//! LOCAL_SCRYFALL on, the field-filter / collection-names walks over
//! `SCRYBOX_SIDECAR_URL`, the `/scryfall-portal` proxy) still builds the
//! same request against a base URL — but when no external URL is configured
//! that base is the sentinel [`INPROC_BASE`], and [`send`] hands the request
//! to the [`ScryfallEngine`] trait object instead of a socket:
//!
//! * [`InProcessEngine`] — the sidecar's own axum `Router` (built by
//!   `local_scryfall::boot::router`, i.e. the very same handlers, response
//!   assembly and static portal) driven with `tower::oneshot` on a dedicated
//!   4-worker runtime (the sidecar ran `worker_threads = 4`), so a heavy
//!   rayon search never parks the front door's workers. Bytes are the
//!   sidecar's bytes by construction — there is no second Scryfall parser.
//! * [`HttpEngine`] — the old hyper client path, used ONLY when an operator
//!   explicitly points `SCRYBOX_SIDECAR_URL` / `LOCAL_SCRYFALL_URL` at an
//!   external engine.
//!
//! Until the index is built (cold boot ~20 s, or no bulk data yet) the
//! in-process engine answers like an unreachable sidecar (transport error),
//! so every caller takes its existing sidecar-down arm unchanged.
//!
//! Data: `SCRYBOX_LSF_DATA_DIR` (falls back to `SCRYFALL_DATA_DIR`) — an
//! existing sidecar `/data` dir is reused as-is. Missing bulk files are
//! fetched at boot and refreshed daily by [`crate::lsf_data`] (the Rust port
//! of fetch-data.sh + refresh-scryfall-data.sh), hot-swapping the index.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, Response, Uri};
use axum::Router;
use http_body_util::BodyExt;
use hyper_util::client::legacy::{connect::HttpConnector, Client};

/// Host of the in-process sentinel base. Never resolved: [`send`] routes any
/// URI with this authority to the in-process engine.
pub const INPROC_HOST: &str = "scryfall-local.inproc";
pub const INPROC_BASE: &str = "http://scryfall-local.inproc";

#[derive(Debug, Clone)]
pub enum EngineError {
    /// Index not built yet / engine disabled — same as "sidecar down".
    NotReady,
    Transport(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::NotReady => f.write_str("local scryfall engine not ready"),
            EngineError::Transport(s) => f.write_str(s),
        }
    }
}

pub type EngineFuture = Pin<Box<dyn Future<Output = Result<Response<Body>, EngineError>> + Send>>;

/// The one seam between scrybox-server and the Scryfall engine.
pub trait ScryfallEngine: Send + Sync {
    fn call(&self, req: Request<Body>) -> EngineFuture;
}

// ---------------------------------------------------------------------------
// HTTP fallback (explicit external URL only)
// ---------------------------------------------------------------------------

pub struct HttpEngine {
    client: Client<HttpConnector, Body>,
}

impl HttpEngine {
    pub fn new(client: Client<HttpConnector, Body>) -> Self {
        Self { client }
    }
}

impl ScryfallEngine for HttpEngine {
    fn call(&self, req: Request<Body>) -> EngineFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .request(req)
                .await
                .map(|r| r.map(Body::new))
                .map_err(|e| EngineError::Transport(e.to_string()))
        })
    }
}

// ---------------------------------------------------------------------------
// In-process engine
// ---------------------------------------------------------------------------

/// Fingerprint of the bulk files the live index was built from (path, len,
/// mtime) — the refresh job reloads when the files on disk move away from it.
pub type DataFingerprint = Vec<(PathBuf, u64, i64)>;

pub struct Loaded {
    pub router: Router,
    pub cards: usize,
    pub fingerprint: DataFingerprint,
    pub built_ms: u128,
}

pub struct InProcessEngine {
    rt: tokio::runtime::Handle,
    live: RwLock<Option<Arc<Loaded>>>,
    pub data_dir: PathBuf,
    pub static_dir: String,
    pub vendored_dir: Option<PathBuf>,
    pub boot_started: Instant,
    ready_ms: std::sync::atomic::AtomicU64,
}

impl InProcessEngine {
    /// Test-only bare engine: empty live slot, caller's runtime, temp data dir.
    /// Avoids the leaked-thread runtime of `init` for scheduler pins that only
    /// need `data_dir`/`current()` semantics (LANE-B iter-67).
    #[cfg(test)]
    pub(crate) fn for_test(data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            rt: tokio::runtime::Handle::current(),
            live: RwLock::new(None),
            data_dir,
            static_dir: String::new(),
            vendored_dir: None,
            boot_started: Instant::now(),
            ready_ms: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn is_ready(&self) -> bool {
        self.live.read().map(|g| g.is_some()).unwrap_or(false)
    }
    pub fn current(&self) -> Option<Arc<Loaded>> {
        self.live.read().ok().and_then(|g| g.clone())
    }
    /// Milliseconds from engine init to the first installed index (0 = not yet).
    pub fn first_ready_ms(&self) -> u64 {
        self.ready_ms.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.rt
    }

    pub fn boot_opts(&self) -> local_scryfall::BootOpts {
        local_scryfall::BootOpts {
            data_dir: Some(self.data_dir.clone()),
            vendored_dir: self.vendored_dir.clone(),
            dev_fallbacks: false,
            file_envs: false,
        }
    }

    /// Build a fresh index from the data dir (blocking; run off-runtime).
    /// None = no default-cards yet, or the load panicked on bad data.
    pub fn build(&self) -> Option<Loaded> {
        let opts = self.boot_opts();
        let fp = fingerprint(&self.data_dir);
        let t = Instant::now();
        let ix = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| local_scryfall::load_index(&opts)))
            .map_err(|_| tracing::error!("local scryfall: index build panicked (bad bulk data?)"))
            .ok()
            .flatten()?;
        let cards = ix.cards.len();
        let router = local_scryfall::router(local_scryfall::state(ix), &self.static_dir);
        Some(Loaded { router, cards, fingerprint: fp, built_ms: t.elapsed().as_millis() })
    }

    pub fn install(&self, l: Loaded) {
        tracing::info!(cards = l.cards, built_ms = l.built_ms as u64, "local scryfall: index installed (in-process)");
        if let Ok(mut g) = self.live.write() {
            *g = Some(Arc::new(l));
        }
        let _ = self.ready_ms.compare_exchange(
            0,
            self.boot_started.elapsed().as_millis().max(1) as u64,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Install a router directly (tests).
    pub fn install_router(&self, router: Router, cards: usize) {
        self.install(Loaded { router, cards, fingerprint: Vec::new(), built_ms: 0 });
    }
}

/// (path, len, mtime) of the bulk inputs that shape the index.
pub fn fingerprint(dir: &std::path::Path) -> DataFingerprint {
    ["default-cards.jsonl.gz", "oracle-tags.jsonl.gz", "lang_map_full.json.gz", "all-cards.jsonl.gz"]
        .iter()
        .filter_map(|f| {
            let p = dir.join(f);
            let m = std::fs::metadata(&p).ok()?;
            let mt = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Some((p, m.len(), mt))
        })
        .collect()
}

impl ScryfallEngine for InProcessEngine {
    fn call(&self, req: Request<Body>) -> EngineFuture {
        let Some(live) = self.current() else {
            return Box::pin(async { Err(EngineError::NotReady) });
        };
        let handle = self.rt.clone();
        let mut req = req;
        // The sidecar era's hyper client always sent `Host: scryfall-local:8420`
        // (the compose URL), and the engine echoes Host into `next_page`
        // links. Keep those bytes: stamp the same Host when none was given.
        if !req.headers().contains_key(axum::http::header::HOST) {
            if let Ok(v) = axum::http::HeaderValue::from_str(&inproc_host_header()) {
                req.headers_mut().insert(axum::http::header::HOST, v);
            }
        }
        Box::pin(async move {
            // Run the sidecar handler AND drain its body on the engine
            // runtime; hand the front door a fully buffered response.
            let job = handle.spawn(async move {
                use tower::ServiceExt;
                let resp = live.router.clone().oneshot(req).await.map_err(|e| EngineError::Transport(e.to_string()))?;
                let (parts, body) = resp.into_parts();
                let bytes = body
                    .collect()
                    .await
                    .map_err(|e| EngineError::Transport(format!("body: {e}")))?
                    .to_bytes();
                Ok::<_, EngineError>(Response::from_parts(parts, Body::from(bytes)))
            });
            job.await.map_err(|e| EngineError::Transport(format!("engine task: {e}")))?
        })
    }
}

/// Host header the in-process engine sees (`SCRYBOX_LSF_HOST`, default the
/// compose service authority the sidecar was reached by).
pub fn inproc_host_header() -> String {
    env_nonempty("SCRYBOX_LSF_HOST").unwrap_or_else(|| "scryfall-local:8420".into())
}

static INPROC: OnceLock<Arc<InProcessEngine>> = OnceLock::new();

/// The process-wide in-process engine, once [`init`] ran.
pub fn inproc() -> Option<&'static Arc<InProcessEngine>> {
    INPROC.get()
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Data dir of the in-process engine, or None when it is switched off.
/// `SCRYBOX_LSF=off` disables; otherwise `SCRYBOX_LSF_DATA_DIR`, then the
/// sidecar's own `SCRYFALL_DATA_DIR`. Nothing configured = no in-process
/// engine (bare dev/test runs keep their old behaviour).
pub fn configured_data_dir() -> Option<PathBuf> {
    if matches!(env_nonempty("SCRYBOX_LSF").as_deref(), Some("off" | "0" | "false" | "none")) {
        return None;
    }
    env_nonempty("SCRYBOX_LSF_DATA_DIR").or_else(|| env_nonempty("SCRYFALL_DATA_DIR")).map(PathBuf::from)
}

pub fn inproc_configured() -> bool {
    configured_data_dir().is_some()
}

/// Sidecar base for the walk legs (`cfg.sidecar_url`): an explicitly
/// configured `SCRYBOX_SIDECAR_URL` keeps the HTTP fallback; otherwise the
/// in-process sentinel when the engine is configured; otherwise None.
pub fn resolve_sidecar_url(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| inproc_configured().then(|| INPROC_BASE.to_string()))
}

/// Base for the Scry client's LOCAL_SCRYFALL swap and the portal proxy:
/// explicit `LOCAL_SCRYFALL_URL`, else the sentinel when the engine is
/// configured, else the compose default the sidecar era used.
pub fn local_scryfall_url() -> String {
    match env_nonempty("LOCAL_SCRYFALL_URL") {
        Some(u) => u.trim_end_matches('/').to_string(),
        None if inproc_configured() => INPROC_BASE.to_string(),
        None => "http://scryfall-local:8420".to_string(),
    }
}

pub fn is_inproc_uri(uri: &Uri) -> bool {
    uri.authority().map(|a| a.as_str().eq_ignore_ascii_case(INPROC_HOST)).unwrap_or(false)
}

pub fn is_inproc_url(url: &str) -> bool {
    url.strip_prefix(INPROC_BASE).map(|r| r.is_empty() || r.starts_with('/') || r.starts_with('?')).unwrap_or(false)
}

/// THE dispatch: in-process engine for sentinel URIs, the HTTP fallback for
/// everything else (explicit external engine URLs, test mocks).
pub async fn send(client: &Client<HttpConnector, Body>, req: Request<Body>) -> Result<Response<Body>, EngineError> {
    if is_inproc_uri(req.uri()) {
        match inproc() {
            Some(e) => e.call(req).await,
            None => Err(EngineError::NotReady),
        }
    } else {
        HttpEngine::new(client.clone()).call(req).await
    }
}

/// Create the engine (dedicated runtime + empty slot). Idempotent; returns
/// the engine. Does NOT load anything — see [`start`].
pub fn init(data_dir: PathBuf, static_dir: String, vendored_dir: Option<PathBuf>) -> &'static Arc<InProcessEngine> {
    INPROC.get_or_init(|| {
        let threads = env_nonempty("SCRYBOX_LSF_THREADS").and_then(|v| v.parse().ok()).filter(|n: &usize| *n > 0).unwrap_or(4);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("lsf")
            .enable_all()
            .build()
            .expect("lsf runtime");
        let handle = rt.handle().clone();
        // The runtime lives for the whole process: park it on its own thread.
        std::thread::Builder::new()
            .name("lsf-rt".into())
            .spawn(move || rt.block_on(std::future::pending::<()>()))
            .expect("lsf runtime thread");
        Arc::new(InProcessEngine {
            rt: handle,
            live: RwLock::new(None),
            data_dir,
            static_dir,
            vendored_dir,
            boot_started: Instant::now(),
            ready_ms: std::sync::atomic::AtomicU64::new(0),
        })
    })
}

/// Boot from env: create the engine, fetch any missing bulk data, build the
/// index in the background, and start the daily refresh. No-op when the
/// engine is not configured.
pub fn start_from_env() {
    let Some(data_dir) = configured_data_dir() else {
        tracing::info!("local scryfall: in-process engine off (no SCRYBOX_LSF_DATA_DIR / SCRYFALL_DATA_DIR)");
        return;
    };
    let static_dir = env_nonempty("SCRYBOX_LSF_STATIC_DIR").unwrap_or_else(|| "/app/scryfall/static".into());
    let vendored = env_nonempty("SCRYBOX_LSF_VENDORED_DIR").map(PathBuf::from).or_else(|| Some(PathBuf::from("/app/scryfall/data")));
    let eng = init(data_dir.clone(), static_dir, vendored);
    tracing::info!(data_dir = %data_dir.display(), "local scryfall: in-process engine booting");
    let eng2 = Arc::clone(eng);
    eng.rt.spawn(async move {
        // Per-tick panic isolation on the boot legs (LANE-B iter-67, same M2
        // class as the scry_jobs loops): pre-fix, a panic inside ensure_data
        // (fetch/gunzip) unwound this task BEFORE refresh_loop was ever
        // entered — the engine then never fetched, built, or refreshed again
        // for the life of the process, silently. Each leg now panics into a
        // logged error; the following legs still run.
        crate::lsf_data::tick_isolated("boot_ensure_data", {
            let eng = Arc::clone(&eng2);
            async move {
                if !matches!(env_nonempty("SCRYBOX_LSF_FETCH").as_deref(), Some("off" | "0" | "false")) {
                    crate::lsf_data::ensure_data(&eng.data_dir).await;
                }
            }
        })
        .await;
        crate::lsf_data::tick_isolated("boot_build", {
            let eng = Arc::clone(&eng2);
            async move {
                let e3 = Arc::clone(&eng);
                match tokio::task::spawn_blocking(move || e3.build()).await {
                    Ok(Some(l)) => eng.install(l),
                    _ => tracing::warn!("local scryfall: no index (bulk data missing/unloadable) — searches take the engine-down arms until the refresh job lands data"),
                }
            }
        })
        .await;
        tracing::info!(first_ready_ms = eng2.first_ready_ms(), "local scryfall: boot done");
        crate::lsf_data::refresh_loop(eng2).await;
    });
}

/// Wait (bounded) for the engine to be ready — used by tests/benches.
pub async fn wait_ready(timeout: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if inproc().map(|e| e.is_ready()).unwrap_or(false) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
