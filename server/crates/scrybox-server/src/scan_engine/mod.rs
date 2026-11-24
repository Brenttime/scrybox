//! TOTORO mono scan2 — the cardscan sidecar's five Scrybox-facing endpoints
//! served in-process (`/api/health`, `/api/detect-cards`, `/api/scan-frame`,
//! `/api/scan-scene`, `/api/scene-complete`), ported from cardscan
//! server.py on the real OpenCV (opencv crate) + the PP-OCRv6 recogniser
//! already ported in `scan_ocr`.
//!
//! `cardscan.rs` calls [`handle`] instead of the HTTP sidecar unless
//! `CARDSCAN_URL` is explicitly set. Data: `SCRYBOX_CARDSCAN_DATA` holds
//! `oracle_titles.json` (+ `oracle_index.meta.json`) and
//! `printing_index.json` (+ `.meta.json`), the same files the sidecar's
//! `data/` volume carries; the recogniser model comes from
//! `SCRYBOX_OCR_MODEL_DIR`.

pub mod detect;
pub mod jobs;
pub mod nightly;
pub mod num;
pub mod printings;
pub mod scene;

use crate::scan_ocr::{self, names::Oracle, rec};
use printings::Printings;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub struct Engine {
    pub oracle: Oracle,
    pub printings: Printings,
    pub rec: Arc<rec::RecEngine>,
    history: Mutex<scene::History>,
    /// `_SCAN_LOCK` + the per-session identity cache it guards.
    scan: Mutex<scene::IdentityCache>,
}

/// The live engine. TOTORO nightly: a hot-swappable slot instead of a
/// OnceLock, so the daily index refresh installs a freshly built engine
/// without a restart (in-flight scans keep their `Arc` to the old one).
static ENGINE: HotSlot<Engine> = HotSlot::new();
/// Serialises the lazy first load (the OnceLock's init exclusivity).
static LOAD: Mutex<()> = Mutex::new(());

/// Explicit `CARDSCAN_URL` keeps the HTTP sidecar path.
/// Longest a status probe waits for the lazy engine load (cardscan.rs sets
/// its in-process status timeout just above this).
pub const STATUS_LOAD_WAIT: Duration = Duration::from_secs(20);

pub fn http_mode() -> bool {
    std::env::var("CARDSCAN_URL").ok().is_some_and(|s| !s.is_empty())
}

/// A value that is either unset, failed to load, or live; `install`
/// replaces it atomically (readers holding the old `Arc` are unaffected).
pub struct HotSlot<T> {
    inner: RwLock<Option<Result<Arc<T>, String>>>,
}

impl<T> Default for HotSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> HotSlot<T> {
    pub const fn new() -> Self {
        HotSlot { inner: RwLock::new(None) }
    }
    /// `None` until a load was attempted; then the live value or its error.
    pub fn get(&self) -> Option<Result<Arc<T>, String>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn live(&self) -> Option<Arc<T>> {
        self.get().and_then(|r| r.ok())
    }
    /// Swap in a new value; returns the previous live one.
    pub fn install(&self, v: T) -> Option<Arc<T>> {
        let prev = self.inner.write().unwrap_or_else(|e| e.into_inner()).replace(Ok(Arc::new(v)));
        prev.and_then(|r| r.ok())
    }
    /// Drop the live value (idle unload); returns it. An error stays.
    pub fn take_live(&self) -> Option<Arc<T>> {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if matches!(g.as_ref(), Some(Ok(_))) {
            g.take().and_then(|r| r.ok())
        } else {
            None
        }
    }
    /// First-load result (only when nothing was attempted yet).
    pub fn init_with(&self, f: impl FnOnce() -> Result<T, String>) -> Result<Arc<T>, String> {
        if let Some(r) = self.get() {
            return r;
        }
        let r = f().map(Arc::new);
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = g.as_ref() {
            return existing.clone();
        }
        *g = Some(r.clone());
        r
    }
}

impl Engine {
    /// Engine over the given (already loaded) indexes and the shared
    /// recogniser.
    pub fn from_parts(oracle: Oracle, printings: Printings) -> Result<Engine, String> {
        let rec = rec::engine()?;
        Ok(Engine { oracle, printings, rec, history: Mutex::new(Default::default()), scan: Mutex::new(Default::default()) })
    }
}

/// `load_oracle()` + `load_printings()` over one data dir.
pub fn load_indexes(dir: &std::path::Path) -> Result<(Oracle, Printings), String> {
    let mut oracle = Oracle::load(dir)?;
    let printings = Printings::load(dir, &mut oracle)?;
    Ok((oracle, printings))
}

/// The live engine, loading it lazily on first call.
///
/// # Errors
///
/// Whatever index/recogniser loading reports; the failure is cached, so
/// later calls report it without retrying.
pub fn engine() -> Result<Arc<Engine>, String> {
    touch_use();
    if let Some(r) = ENGINE.get() {
        return r;
    }
    let _g = LOAD.lock().unwrap_or_else(|e| e.into_inner());
    let r = ENGINE.init_with(|| {
        let t0 = Instant::now();
        let rss0 = rss_mb();
        let dir = scan_ocr::data_dir().ok_or("SCRYBOX_CARDSCAN_DATA unset")?;
        let mut oracle = scan_ocr::names::Oracle::load(&dir)?;
        let rss_oracle = rss_mb();
        let printings = printings::Printings::load(&dir, &mut oracle)?;
        let rss_print = rss_mb();
        let rec = rec::engine()?;
        // `init_ocr` warm-up: pay graph setup before scan one.
        let _ = rec.recognize(&[scan_ocr::pil::Rgb { w: 512, h: 64, px: vec![255; 512 * 64 * 3] }]);
        let rss_rec = rss_mb();
        tracing::info!(
            "cardscan in-process: {} oracle titles, {} printings in {:.1}s (rss MB: start {rss0}, +oracle {}, +printings {}, +ocr {})",
            oracle.norm_list.len(),
            printings.len(),
            t0.elapsed().as_secs_f64(),
            rss_oracle - rss0,
            rss_print - rss_oracle,
            rss_rec - rss_print,
        );
        Engine::from_parts(oracle, printings)
    });
    if r.is_ok() {
        spawn_janitor();
    }
    r
}

/// Resident set in MB (/proc/self/statm), for the load attribution log.
fn rss_mb() -> i64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<i64>().ok()))
        .map(|pages| pages * 4096 / (1 << 20))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Idle unload (TOTORO mem). The loaded engine (oracle titles, printing
// resolver tables, ORT recogniser) is ~230 MB resident for a feature used in
// bursts. After `CARDSCAN_IDLE_SECS` (default 900 = 15 min; 0 = never) with
// no cardscan request, a janitor drops the engine + recogniser and trims the
// heap; the next request reloads the same files (~2 s, the existing lazy
// first-load path, which the status probe already waits out). Mirrors
// cvscan's CV_SCAN_IDLE_SECS janitor for the scan-match catalog.
// ---------------------------------------------------------------------------

static LAST_USE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn touch_use() {
    LAST_USE_MS.store(crate::util::now_ms_u64(), std::sync::atomic::Ordering::Relaxed);
}

fn idle_ttl_ms() -> u64 {
    match std::env::var("CARDSCAN_IDLE_SECS") {
        Ok(v) if !v.trim().is_empty() => v.trim().parse::<u64>().unwrap_or(900).saturating_mul(1000),
        _ => 900_000,
    }
}

/// One janitor decision: unload when loaded, enabled and idle past the TTL.
pub fn idle_due(now_ms: u64, last_ms: u64, ttl_ms: u64, loaded: bool) -> bool {
    loaded && ttl_ms > 0 && now_ms.saturating_sub(last_ms) >= ttl_ms
}

/// Drop the live engine + recogniser if idle (or `force`). Returns true when
/// something was unloaded. Serialised with loads and nightly installs.
pub fn unload_if_idle(force: bool) -> bool {
    let _g = LOAD.lock().unwrap_or_else(|e| e.into_inner());
    let now = crate::util::now_ms_u64();
    let last = LAST_USE_MS.load(std::sync::atomic::Ordering::Relaxed);
    if !force && !idle_due(now, last, idle_ttl_ms(), ENGINE.live().is_some()) {
        return false;
    }
    let dropped = ENGINE.take_live().is_some();
    let rec_dropped = rec::unload();
    if dropped || rec_dropped {
        crate::collection::trim_heap();
        tracing::info!("cardscan in-process: engine unloaded after idle (reloads on next scan)");
    }
    dropped || rec_dropped
}

fn spawn_janitor() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if idle_ttl_ms() == 0 || STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let tick = Duration::from_millis((idle_ttl_ms() / 15).clamp(1_000, 60_000));
    let _ = std::thread::Builder::new().name("cardscan-idle".into()).spawn(move || loop {
        std::thread::sleep(tick);
        unload_if_idle(false);
    });
}

/// The engine if it is loaded (no lazy load).
pub fn loaded_engine() -> Option<Arc<Engine>> {
    ENGINE.live()
}

/// Hot-swap a new engine in (nightly refresh). Clears an earlier load
/// failure too (auto-recovery once good indexes land).
pub fn install_engine(e: Engine) {
    let _g = LOAD.lock().unwrap_or_else(|e| e.into_inner());
    ENGINE.install(e);
}

/// Load the indexes + model in the background at boot (the sidecar loads
/// them before serving), so scan one does not pay ~3 s of JSON parsing.
pub fn spawn_warm() {
    // One warm-up thread at a time; re-armed when it finishes so a status
    // probe after an idle unload can trigger the reload again.
    static WARMING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if http_mode() || scan_ocr::data_dir().is_none() || WARMING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        if let Err(e) = engine() {
            tracing::warn!("cardscan in-process engine unavailable: {e}");
        }
        WARMING.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// Boot hook (main.rs): build missing indexes in the volume, and warm the
/// engine only when SCRYBOX_CARDSCAN_WARM=1 (owner ruling: heavy assets load
/// lazily; the first scan or status probe triggers the load). Default stays
/// lazy: warm costs ~230 MB idle RSS for a feature used occasionally, and the
/// load is ~2-4 s (≤ 9 s under CPU contention), which the first status probe
/// now absorbs (see `handle`, cutfix BUG-2) — so lazy never shows "offline".
pub fn boot() {
    if http_mode() {
        return;
    }
    let Some(dir) = scan_ocr::data_dir() else { return };
    let warm = std::env::var("SCRYBOX_CARDSCAN_WARM").is_ok_and(|v| v == "1");
    tokio::spawn(async move {
        jobs::ensure_indexes(dir).await;
        // TOTORO nightly: daily cardscan + client-scan index refresh.
        nightly::spawn_scheduler();
        if warm {
            spawn_warm();
        }
    });
}

impl Engine {
    fn ctx<'a>(&'a self, rec: scene::Recognize<'a>, call: scene::RecCall<'a>) -> scene::Ctx<'a> {
        scene::Ctx { oracle: &self.oracle, printings: &self.printings, rec, rec_call: call, history: &self.history }
    }
}

fn header<'a>(headers: &'a [(String, String)], k: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
}

fn busy() -> (u16, Value) {
    (429, json!({"ok": false, "retry": true, "error": "scanner busy"}))
}

fn with_request_ms(mut m: Map<String, Value>, t0: Instant) -> Value {
    m.insert("request_ms".into(), json!((t0.elapsed().as_secs_f64() * 1000.0) as i64));
    Value::Object(m)
}

fn err_map(prefix: &str, e: String) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("ok".into(), json!(false));
    m.insert("error".into(), json!(format!("{prefix}: {e}")));
    m
}

/// One sidecar request → (status, JSON). `None` = engine unavailable (the
/// proxy maps it to its 503 "Scanner service is not running" arm).
pub fn handle(method: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> Option<(u16, Value)> {
    if http_mode() {
        return None;
    }
    touch_use();
    if method == "GET" && path == "/api/health" {
        // cutfix BUG-2: the sidecar loaded its models before it listened, so
        // its health probe never saw a half-loaded engine, and FastScanner
        // reads one status probe on mount (`setService(r.ok)`). With the
        // lazy load (default: ~230 MB stays unallocated until someone scans)
        // the probe itself triggers the load and waits for it (~2-4 s
        // measured; bounded by STATUS_LOAD_WAIT) instead of answering a 503
        // placeholder that rendered "Fast scan service is offline".
        if ENGINE.get().is_none() {
            spawn_warm();
            let deadline = Instant::now() + STATUS_LOAD_WAIT;
            while ENGINE.get().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        match ENGINE.get() {
            None => return Some((503, json!({"ok": false, "ocr_backend": "loading", "physical_printings": 0}))),
            Some(Err(_)) => return None,
            Some(Ok(_)) => {}
        }
    }
    let eng = engine().ok()?;
    let rec_fn = |s: &[scan_ocr::pil::Rgb]| eng.rec.recognize(s);
    let call_fn = |s: &scan_ocr::pil::Rgb| eng.rec.recognize_call(s);
    let ctx = eng.ctx(&rec_fn, &call_fn);
    if method == "GET" {
        return Some(match path {
            "/api/health" => {
                (200, json!({"ok": true, "ocr_backend": scene::OCR_BACKEND, "physical_printings": eng.printings.len()}))
            }
            _ => (404, json!({"error": "not found"})),
        });
    }
    if body.len() > 25 * 1024 * 1024 {
        return Some((413, json!({"ok": false, "error": "image exceeds 25 MB"})));
    }
    let t0 = Instant::now();
    Some(match path {
        "/api/scene-complete" => (200, json!({"ok": true})),
        "/api/detect-cards" => {
            let Ok(_g) = eng.scan.try_lock() else { return Some(busy()) };
            let m = if body.is_empty() {
                let mut m = Map::new();
                m.insert("ok".into(), json!(false));
                m.insert("error".into(), json!("empty scene upload"));
                m
            } else {
                match scene::detect_uploaded_scene(body, false) {
                    Ok(d) => scene::detection_json(&d),
                    Err(e) => err_map("invalid scene image", e),
                }
            };
            (200, with_request_ms(m, t0))
        }
        "/api/scan-frame" => {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut guard = loop {
                if let Ok(g) = eng.scan.try_lock() {
                    break g;
                }
                if Instant::now() >= deadline {
                    return Some(busy());
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let session: String = header(headers, "x-scan-session")
                .unwrap_or("")
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .take(60)
                .collect();
            let session = (!session.is_empty()).then_some(session);
            let m = match scene::scan_frame(&ctx, &mut guard, body, session) {
                Ok(m) => m,
                Err(e) => err_map("invalid frame", e),
            };
            drop(guard);
            let mut v = with_request_ms(m, t0);
            let client: String = header(headers, "x-scan-client")
                .unwrap_or("")
                .chars()
                .filter(|c| c.is_alphanumeric() || "_ .:=,/-".contains(*c))
                .take(120)
                .collect();
            if !client.is_empty() {
                v["client"] = json!(client);
            }
            (200, v)
        }
        "/api/scan-scene" => {
            if header(headers, "x-scene-protocol") != Some("3") {
                return Some((409, json!({"ok": false, "error": "outdated scanner tab; reload this page"})));
            }
            let Ok(_g) = eng.scan.try_lock() else { return Some(busy()) };
            let text = String::from_utf8_lossy(body);
            let payload: Result<Value, String> =
                if text.is_empty() { Ok(json!({})) } else { serde_json::from_str(&text).map_err(|e| e.to_string()) };
            let m = match payload.and_then(|p| scene::scan_scene_batch(&ctx, &p)) {
                Ok(m) => m,
                Err(e) => err_map("invalid scene batch", e),
            };
            (200, with_request_ms(m, t0))
        }
        _ => (404, json!({"error": "not found"})),
    })
}

#[cfg(test)]
mod idle_tests {
    use super::idle_due;

    #[test]
    fn idle_unload_decision() {
        let ttl = 900_000;
        assert!(!idle_due(1_000_000, 500_000, ttl, true), "used 500 s ago: keep");
        assert!(idle_due(1_400_000, 500_000, ttl, true), "idle exactly the TTL: unload");
        assert!(!idle_due(9_000_000, 0, ttl, false), "nothing loaded: nothing to do");
        assert!(!idle_due(9_000_000, 0, 0, true), "CARDSCAN_IDLE_SECS=0 disables");
        assert!(!idle_due(0, 5, ttl, true), "clock skew never underflows");
    }
}
