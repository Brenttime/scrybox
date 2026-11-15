//! RapidOCR 3.9.2 recognition-only path (`TextRecognizer` + `CTCLabelDecode`)
//! over the SAME PP-OCRv6_rec_small.onnx the sidecar ships, on the shared ort
//! runtime. Detection/classification are never used by cardscan
//! (`use_det=False, use_cls=False`), so only recognition is ported.

use super::cvresize::resize_linear;
use super::pil::Rgb;
use http_body_util::BodyExt as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

pub const REC_MODEL_FILE: &str = "PP-OCRv6_rec_small.onnx";

// ---------------------------------------------------------------------------
// Operator fetch policy (lane-B iter-64)
// ---------------------------------------------------------------------------
//
// PP-OCRv6_rec_small.onnx no longer ships in the Node-free image
// (Dockerfile.rust `docker images` SIZE gate). It follows the SAME policy as
// the two card models in model_assets: missing from the model dir -> fetch
// on demand, verify against the pinned sha256, install atomically into the
// model volume (${CV_MODEL_DIR}/ocr persists across container recreations).
// The pins mirror the `ocr` builder stage of Dockerfile.rust (ModelScope
// RapidAI/RapidOCR v3.9.2 — Apache-2.0, so shipping it is allowed too; this
// is purely a size call, unlike the AGPL card models).
//
// Env knobs:
//   SCRYBOX_OCR_MODEL_FETCH=0  never fetch (offline hosts). A missing file
//                              then behaves exactly like the pre-iter-64
//                              permanent "offline" arm.
//   SCRYBOX_OCR_MODEL_BASE     base-URL override (private mirror; tests).
//   SCRYBOX_OCR_MODEL_SHA256   re-pin the checksum (empty = skip verify —
//                              same test convention as model_assets).
// The status probe stays non-poisoning: engine init treats a `retryable`
// fetch error as "try again", so a transient network failure never leaves
// cardscan permanently offline (cutfix BUG-2's "never show offline" spirit).

pub const REC_MODEL_URL: &str =
    "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/rec/PP-OCRv6_rec_small.onnx";
pub const REC_MODEL_SHA256: &str = "6f327246b50388f3c176ae304bd95767ea6dc0c9ae92153ef8cbe210b3c14884";

#[derive(Debug)]
pub struct OcrFetchError {
    /// true = worth retrying on the next probe/scan (network, CDN, sha
    /// mismatch on a corrupt response); false = permanent until the
    /// environment changes (no model dir, fetch disabled).
    pub retryable: bool,
    pub message: String,
}

impl OcrFetchError {
    fn fatal(m: impl Into<String>) -> Self {
        Self { retryable: false, message: m.into() }
    }
    fn retry(m: impl Into<String>) -> Self {
        Self { retryable: true, message: m.into() }
    }
}

impl std::fmt::Display for OcrFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn fetch_locks() -> &'static std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::Mutex<()>>>> {
    static TABLE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::Mutex<()>>>>> =
        OnceLock::new();
    TABLE.get_or_init(Default::default)
}

/// Single-flight: concurrent first scans/probes share one download per path.
fn fetch_lock(path: &std::path::Path) -> Arc<std::sync::Mutex<()>> {
    let mut t = fetch_locks().lock().unwrap_or_else(|p| p.into_inner());
    t.entry(path.to_string_lossy().into_owned())
        .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
        .clone()
}

/// Ensure `REC_MODEL_FILE` exists in `model_dir()`. Ok = the recogniser is
/// loadable (already present, or fetched + sha256-verified into place).
pub fn ensure_model_present() -> Result<(), OcrFetchError> {
    let dir = model_dir()
        .ok_or_else(|| OcrFetchError::fatal("no OCR model dir (set SCRYBOX_OCR_MODEL_DIR or CV_MODEL_DIR)"))?;
    ensure_model_in(&dir)
}

/// ensure_model_present over an explicit dir (test seam: env-pinned model
/// dirs stay untouched).
pub fn ensure_model_in(dir: &std::path::Path) -> Result<(), OcrFetchError> {
    let path = dir.join(REC_MODEL_FILE);
    if path.is_file() {
        return Ok(());
    }
    if std::env::var("SCRYBOX_OCR_MODEL_FETCH").map(|v| v == "0").unwrap_or(false) {
        return Err(OcrFetchError::fatal(format!(
            "{REC_MODEL_FILE} not present in {} and SCRYBOX_OCR_MODEL_FETCH=0",
            dir.display()
        )));
    }
    let base = std::env::var("SCRYBOX_OCR_MODEL_BASE").unwrap_or_else(|_| REC_MODEL_URL.to_string());
    if base.trim().is_empty() {
        return Err(OcrFetchError::fatal("SCRYBOX_OCR_MODEL_BASE is empty"));
    }
    let sha = std::env::var("SCRYBOX_OCR_MODEL_SHA256").unwrap_or_else(|_| REC_MODEL_SHA256.to_string());
    let lock = fetch_lock(&path);
    let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
    if path.is_file() {
        return Ok(()); // another fetch won the race — fine.
    }
    std::fs::create_dir_all(dir).map_err(|e| OcrFetchError::fatal(format!("create {}: {e}", dir.display())))?;
    // Runs on the dedicated warm thread or the blocking pool (cardscan.rs
    // dispatches handle() through spawn_blocking), never on a runtime worker,
    // so a throwaway current-thread runtime for the hyper hop is safe here.
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(fetch_to(&path, &base, &sha)),
        Err(e) => Err(OcrFetchError::retry(format!("ocr fetch runtime: {e}"))),
    }
}

/// Redirect-following GET (same 10-hop ladder as model_assets::fetch_asset),
/// sha256 verify BEFORE install, `.tmp` + atomic rename, tmp removed on any
/// failure — a failed fetch never truncates or half-installs the model.
/// Builds its OWN hyper client (pool_idle_timeout=0): ensure_model_in runs
/// the future on a throwaway current-thread runtime, and the shared
/// model_assets dl_client would strand its keep-alive pool tasks there —
/// dropping that runtime then blocks ~90 s per fetch waiting for them.
async fn fetch_to(path: &std::path::Path, url: &str, sha: &str) -> Result<(), OcrFetchError> {
    use axum::http::header;
    let client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        String,
    > = {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|e| OcrFetchError::fatal(format!("native roots: {e}")))?
            .https_or_http()
            .enable_http1()
            .build();
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::ZERO)
            .build(https)
    };
    let tmp = format!("{}.tmp", path.to_string_lossy());
    let body = 'redirects: {
        let mut url = url.to_string();
        for _ in 0..10 {
            let uri = url.parse::<axum::http::Uri>().map_err(|e| OcrFetchError::retry(e.to_string()))?;
            let req = hyper::Request::builder()
                .method(hyper::Method::GET)
                .uri(uri)
                .header(header::USER_AGENT, axum::http::HeaderValue::from_static("curl/8.5.0"))
                .body(String::new())
                .map_err(|e| OcrFetchError::retry(e.to_string()))?;
            let res = client
                .request(req)
                .await
                .map_err(|e| OcrFetchError::retry(format!("GET {url}: {e}")))?;
            let status = res.status();
            if status.is_redirection() {
                if let Some(loc) = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()) {
                    url = crate::catalog_build::join_uri(&url, loc);
                    continue;
                }
            }
            if !status.is_success() {
                return Err(OcrFetchError::retry(format!("HTTP {} fetching {REC_MODEL_FILE}", status.as_u16())));
            }
            break 'redirects res.into_body();
        }
        return Err(OcrFetchError::retry("too many redirects".to_string()));
    };
    let bytes = body.collect().await.map_err(|e| OcrFetchError::retry(format!("body: {e}")))?.to_bytes();
    if !sha.is_empty() {
        use sha2::Digest as _;
        let got = format!("{:x}", sha2::Sha256::new().chain_update(&bytes).finalize());
        if !got.eq_ignore_ascii_case(sha) {
            // Retryable: CDN edges serve corrupt bodies sometimes; a genuinely
            // wrong pin just retries (and logs) until the operator fixes it.
            return Err(OcrFetchError::retry(format!(
                "sha256 mismatch for {url}: expected {sha}, got {got} — refusing to install"
            )));
        }
    }
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(OcrFetchError::fatal(format!("write {tmp}: {e}")));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(OcrFetchError::fatal(format!("rename {tmp} -> {}: {e}", path.display())));
    }
    tracing::info!("cardscan OCR recogniser fetched: {} bytes from {url}", bytes.len());
    Ok(())
}

const IMG_H: usize = 48;
const IMG_W: usize = 320;
const BATCH: usize = 6;

pub struct RecEngine {
    session: Mutex<ort::session::Session>,
    chars: Vec<String>,
    output: String,
    input: String,
}

static ENGINE: OnceLock<Result<RecEngine, String>> = OnceLock::new();

/// Model dir: SCRYBOX_OCR_MODEL_DIR, else `<CV_MODEL_DIR>/ocr`.
pub fn model_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("SCRYBOX_OCR_MODEL_DIR") {
        if !d.is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    crate::cvscan::model_dir_pub().map(|d| d.join("ocr"))
}

/// Python `str.splitlines()` (no keepends).
pub fn py_splitlines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn engine() -> Result<&'static RecEngine, String> {
    ENGINE
        .get_or_init(|| {
            let dir = model_dir().ok_or("no OCR model dir")?;
            let env = crate::cvscan::ort_env()?;
            let threads: usize = std::env::var("CARDSCAN_OCR_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8));
            let eps = [ort::ep::CPU::default().with_arena_allocator(false).build()];
            let mut b = ort::session::Session::builder(env).map_err(|e| e.to_string())?;
            b = b.with_intra_threads(threads).map_err(|e| e.to_string())?;
            b = b.with_inter_threads(1).map_err(|e| e.to_string())?;
            b = b.with_execution_providers(&eps).map_err(|e| e.to_string())?;
            let s = b.commit_from_file(dir.join(REC_MODEL_FILE)).map_err(|e| e.to_string())?;
            let meta = s.metadata().map_err(|e| e.to_string())?;
            let raw = meta.custom("character").ok_or("rec model has no character metadata")?;
            let mut chars = py_splitlines(&raw);
            chars.push(" ".into());
            chars.insert(0, "blank".into());
            let input = s.inputs()[0].name().to_string();
            let output = s.outputs()[0].name().to_string();
            drop(meta);
            Ok(RecEngine { session: Mutex::new(s), chars, output, input })
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// numpy pairwise_sum (float64) as used by `np.mean`.
fn np_pairwise_sum(a: &[f64]) -> f64 {
    let n = a.len();
    if n < 8 {
        let mut r = 0.0;
        for v in a {
            r += v;
        }
        r
    } else if n <= 128 {
        let mut r = [0f64; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - (n % 8) {
            for j in 0..8 {
                r[j] += a[i + j];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        res
    } else {
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        np_pairwise_sum(&a[..n2]) + np_pairwise_sum(&a[n2..])
    }
}

/// Python builtin `round(x, 5)` (correctly rounded decimal).
fn py_round5(x: f64) -> f64 {
    format!("{x:.5}").parse().unwrap_or(x)
}

/// numpy `ndarray.round(5)` on a float64 scalar.
fn np_round5(x: f64) -> f64 {
    (x * 1e5).round_ties_even() / 1e5
}

/// `TextRecognizer.resize_norm_img` into a CHW f32 slab of width `pad_w`.
fn resize_norm(img: &Rgb, max_wh_ratio: f64, out: &mut [f32]) {
    let img_w = (IMG_H as f64 * max_wh_ratio) as usize;
    let ratio = img.w as f64 / img.h as f64;
    let c = (IMG_H as f64 * ratio).ceil() as usize;
    let rw = if c > img_w { img_w } else { c };
    let r = resize_linear(img, rw, IMG_H);
    let plane = IMG_H * img_w;
    for y in 0..IMG_H {
        for x in 0..rw {
            let p = (y * rw + x) * 3;
            for ch in 0..3 {
                // astype(float32) / 255 - 0.5 / 0.5, all f32 numpy ops.
                let mut v = r.px[p + ch] as f32 / 255.0f32;
                v -= 0.5;
                v /= 0.5;
                out[ch * plane + y * img_w + x] = v;
            }
        }
    }
}

impl RecEngine {
    pub fn chars(&self) -> &[String] {
        &self.chars
    }

    /// `TextRecognizer.__call__` on a list of images (channel order as given;
    /// the sidecar passes BGR for scene strips, RGB for `ocr_name` reads).
    pub fn recognize(&self, imgs: &[Rgb]) -> Result<Vec<(String, f64)>, String> {
        let n = imgs.len();
        let mut res = vec![(String::new(), 0.0f64); n];
        if n == 0 {
            return Ok(res);
        }
        let ratios: Vec<f64> = imgs.iter().map(|i| i.w as f64 / i.h as f64).collect();
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| ratios[a].partial_cmp(&ratios[b]).unwrap_or(std::cmp::Ordering::Equal));
        let mut beg = 0;
        while beg < n {
            let end = (beg + BATCH).min(n);
            let mut max_wh = IMG_W as f64 / IMG_H as f64;
            for &i in &idx[beg..end] {
                let (h, w) = (imgs[i].h as f64, imgs[i].w as f64);
                max_wh = max_wh.max(w * 1.0 / h);
            }
            let img_w = (IMG_H as f64 * max_wh) as usize;
            let per = 3 * IMG_H * img_w;
            let bn = end - beg;
            let mut data = vec![0f32; bn * per];
            for (k, &i) in idx[beg..end].iter().enumerate() {
                resize_norm(&imgs[i], max_wh, &mut data[k * per..(k + 1) * per]);
            }
            let t = ort::value::Value::from_array((vec![bn, 3, IMG_H, img_w], data)).map_err(|e| e.to_string())?;
            let (shape, probs) = {
                let mut g = self.session.lock().map_err(|e| e.to_string())?;
                let outs = g.run(ort::inputs! { self.input.as_str() => t }).map_err(|e| e.to_string())?;
                let (s, v) = outs[self.output.as_str()].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
                (s.iter().map(|&d| d as usize).collect::<Vec<_>>(), v.to_vec())
            };
            let (tn, cn) = (shape[1], shape[2]);
            for (k, &i) in idx[beg..end].iter().enumerate() {
                res[i] = self.ctc_decode(&probs[k * tn * cn..(k + 1) * tn * cn], tn, cn);
            }
            beg = end;
        }
        Ok(res)
    }

    fn ctc_decode(&self, p: &[f32], tn: usize, cn: usize) -> (String, f64) {
        let mut ids = Vec::with_capacity(tn);
        let mut mx = Vec::with_capacity(tn);
        for t in 0..tn {
            let row = &p[t * cn..(t + 1) * cn];
            let (mut bi, mut bv) = (0usize, row[0]);
            for (j, &v) in row.iter().enumerate().skip(1) {
                if v > bv {
                    bi = j;
                    bv = v;
                }
            }
            ids.push(bi);
            mx.push(bv);
        }
        let mut text = String::new();
        let mut confs = Vec::new();
        for t in 0..tn {
            if t > 0 && ids[t] == ids[t - 1] {
                continue;
            }
            if ids[t] == 0 {
                continue;
            }
            text.push_str(self.chars.get(ids[t]).map(|s| s.as_str()).unwrap_or(""));
            confs.push(py_round5(mx[t] as f64));
        }
        if confs.is_empty() {
            confs.push(0.0);
        }
        let mean = np_pairwise_sum(&confs) / confs.len() as f64;
        (text, np_round5(mean))
    }

    /// `RapidOCR.__call__(img, use_det=False, use_cls=False, use_rec=True)`
    /// on an ndarray (no channel conversion; max/min side bounds applied).
    pub fn recognize_call(&self, img: &Rgb) -> Result<(String, f64), String> {
        let mut im = img.clone();
        let (h, w) = (im.h as f64, im.w as f64);
        if h.max(w) > 2000.0 {
            let ratio = if h > w { 2000.0 / h } else { 2000.0 / w };
            let rh = (((h * ratio) as i64 as f64) / 32.0).round_ties_even() as usize * 32;
            let rw = (((w * ratio) as i64 as f64) / 32.0).round_ties_even() as usize * 32;
            im = resize_linear(&im, rw.max(1), rh.max(1));
        }
        let (h, w) = (im.h as f64, im.w as f64);
        if h.min(w) < 30.0 {
            let ratio = if h < w { 30.0 / h } else { 30.0 / w };
            let rh = (((h * ratio) as i64 as f64) / 32.0).round_ties_even() as usize * 32;
            let rw = (((w * ratio) as i64 as f64) / 32.0).round_ties_even() as usize * 32;
            im = resize_linear(&im, rw.max(1), rh.max(1));
        }
        Ok(self.recognize(std::slice::from_ref(&im))?.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(py_splitlines("a\nb\r\nc\x0bd\n"), vec!["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
    }

    #[test]
    fn numpy_mean_and_rounding() {
        let v: Vec<f64> = (0..11).map(|i| 0.9 + i as f64 * 0.001).collect();
        let m = np_pairwise_sum(&v) / v.len() as f64;
        assert!((m - 0.905).abs() < 1e-12);
        assert_eq!(py_round5(0.123456789), 0.12346);
        assert_eq!(np_round5(0.999995), 1.0);
    }
}
