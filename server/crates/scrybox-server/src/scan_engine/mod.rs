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
pub mod num;
pub mod printings;
pub mod scene;

use crate::scan_ocr::{self, names::Oracle, rec};
use printings::Printings;
use serde_json::{json, Map, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub struct Engine {
    pub oracle: Oracle,
    pub printings: Printings,
    pub rec: &'static rec::RecEngine,
    history: Mutex<scene::History>,
    /// `_SCAN_LOCK` + the per-session identity cache it guards.
    scan: Mutex<scene::IdentityCache>,
}

static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();

/// Explicit `CARDSCAN_URL` keeps the HTTP sidecar path.
/// Longest a status probe waits for the lazy engine load (cardscan.rs sets
/// its in-process status timeout just above this).
pub const STATUS_LOAD_WAIT: Duration = Duration::from_secs(20);

pub fn http_mode() -> bool {
    std::env::var("CARDSCAN_URL").ok().is_some_and(|s| !s.is_empty())
}

pub fn engine() -> Result<&'static Engine, String> {
    ENGINE
        .get_or_init(|| {
            let t0 = Instant::now();
            let dir = scan_ocr::data_dir().ok_or("SCRYBOX_CARDSCAN_DATA unset")?;
            let mut oracle = Oracle::load(&dir)?;
            let printings = Printings::load(&dir, &mut oracle)?;
            let rec = rec::engine()?;
            // `init_ocr` warm-up: pay graph setup before scan one.
            let _ = rec.recognize(&[scan_ocr::pil::Rgb { w: 512, h: 64, px: vec![255; 512 * 64 * 3] }]);
            tracing::info!(
                "cardscan in-process: {} oracle titles, {} printings in {:.1}s",
                oracle.norm_list.len(),
                printings.len(),
                t0.elapsed().as_secs_f64()
            );
            Ok(Engine { oracle, printings, rec, history: Mutex::new(Default::default()), scan: Mutex::new(Default::default()) })
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// Load the indexes + model in the background at boot (the sidecar loads
/// them before serving), so scan one does not pay ~3 s of JSON parsing.
pub fn spawn_warm() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if http_mode() || scan_ocr::data_dir().is_none() || STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        if let Err(e) = engine() {
            tracing::warn!("cardscan in-process engine unavailable: {e}");
        }
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
        if warm {
            spawn_warm();
        }
    });
}

impl Engine {
    fn ctx<'a>(
        &'a self,
        rec: &'a dyn Fn(&[scan_ocr::pil::Rgb]) -> Result<Vec<(String, f64)>, String>,
        call: &'a dyn Fn(&scan_ocr::pil::Rgb) -> Result<(String, f64), String>,
    ) -> scene::Ctx<'a> {
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
