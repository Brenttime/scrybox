//! Lane B unit 6b: cvScan.js twin — the CollectorVision scan pipeline
//! (cornelius corners → dewarp → milo embed → cosine sweep) plus
//! POST /api/scan-match (routes/collection.js:279), behind RUST_ROUTES=scan_match.
//!
//! Faithful port of backend/src/cvScan.js as verified against disk this
//! iteration (terminal cat — the file's internals are pinned by the harness
//! bench/scan-match-ab.mjs against the live Node lane):
//!
//!   * tensors are ImageNet-normalised **NCHW** planar float32 [1,3,size,size]
//!     under input name `image` (milo also outputs `embedding`; cornelius
//!     outputs `corners` + `presence` + `sharpness`, presence is unread);
//!   * `searchTopK` keeps Float32Array-rounded sims, insert-sorted;
//!   * `verified` is pinned false, the real gates are the winner-vs-neighbourhood
//!     `gap` (GAP_FLOOR, per-catalog ranks 2..11) → `notInCatalog`;
//!   * candidates are `{cardId, score, catalogLang}` deduped by the `_back`-
//!     stripped catalog id, topK 8, with the route then hydrating
//!     `{name, set, number, card}` from card_cache (Forward-to-Node on any
//!     would-fetch, the unit-6a hydration policy).
//!
//! ORT parity: run through the SAME libonnxruntime the container's
//! onnxruntime-node loads (ORT_DYLIB_PATH, restart-rust-b.sh) with
//! intra=inter=1, sequential — embeddings are BIT-IDENTICAL to Node
//! (sha256-pinned). Without ORT_DYLIB_PATH the whole route forwards: Node
//! answers with its own bundled runtime and the lane stays inert.
//!
//! Image twin: sharp/libvips (mozjpeg/spng + lanczos3 resize) on the Node leg
//! vs the image crate here. JPEG decoders differ ≤1 LSB and the two jpeg
//! ENCODERS (mozjpeg vs image) never agree byte-wise — accepted divergences
//! (decoded-crop compared instead of raw base64); scores are compared with a
//! tolerance, candidate ids / flags / booleans strictly.

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use crate::cardscan;
use crate::npz::{self, NpyData};

const CORN_SIZE: usize = 384;
const EMBED_SIZE: usize = 448;
const MEAN: [f64; 3] = [0.485, 0.456, 0.406];
const STD: [f64; 3] = [0.229, 0.224, 0.225];
const SHARPNESS_GATE: f32 = 0.02;
pub const GAP_FLOOR_DEFAULT: f64 = 0.10;
const GAP_K: usize = 11;
const TOP_K: usize = 8;
const MAX_SIDE: usize = 1200;
const CROP_SIDE: usize = 220;
const CROP_QUALITY: u8 = 70;

fn gap_floor() -> f64 {
    // Number(process.env.CV_SCAN_GAP || 0.10): unset/empty/garbage → 0.10,
    // JS Number() semantics on a numeric string otherwise.
    std::env::var("CV_SCAN_GAP")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| js_number(&s))
        .unwrap_or(GAP_FLOOR_DEFAULT)
}

fn js_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return Some(0.0);
    }
    t.parse::<f64>().ok()
}

fn model_dir() -> Option<PathBuf> {
    match std::env::var_os("CV_MODEL_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        // path.join(__dirname, '..', 'data', 'models') — backend/data/models.
        _ => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("../../../data/models")))
            .filter(|p| p.exists()),
    }
}

fn suffix(lang: Option<&str>) -> String {
    match lang {
        None => String::new(),
        Some(l) if l == "en" || l == "English" || l.is_empty() => String::new(),
        Some(l) => format!("-{}", l.to_lowercase()),
    }
}

fn local_bin(dir: &PathBuf, game: &str, lang: Option<&str>) -> PathBuf {
    dir.join(format!("milo-{game}{}-local.bin", suffix(lang)))
}
fn local_meta(dir: &PathBuf, game: &str, lang: Option<&str>) -> PathBuf {
    dir.join(format!("milo-{game}{}-local.json", suffix(lang)))
}
fn catalog_path(dir: &PathBuf, game: &str) -> PathBuf {
    dir.join(format!("milo-{game}.npz"))
}

fn has_local(dir: &PathBuf, game: &str, lang: Option<&str>) -> bool {
    local_bin(dir, game, lang).exists() && local_meta(dir, game, lang).exists()
}

/// cvScan.isBuilt(game, lang).
pub fn is_built(game: &str, lang: &str) -> bool {
    let Some(dir) = model_dir() else { return false };
    if !dir.join("cornelius.onnx").exists() || !dir.join("milo.onnx").exists() {
        return false;
    }
    let lang_arg = if lang.is_empty() { None } else { Some(lang) };
    if has_local(&dir, game, lang_arg) {
        return true;
    }
    // Node calls hasLocal(game) with NO lang here — suffix(undefined) === ''.
    has_local(&dir, game, None) || catalog_path(&dir, game).exists()
}

/// cvScan.builtLangs(game): the languages with a catalog OF THEIR OWN, with
/// 'English' riding along (front) when only the published .npz exists.
pub fn built_langs(game: &str) -> Vec<String> {
    let Some(dir) = model_dir() else { return vec![] };
    let v: Vec<Value> = serde_json::from_str(include_str!("../../../../shared/languages.json"))
        .expect("shared/languages.json");
    let mut out: Vec<String> = v
        .iter()
        .filter_map(|l| l.get("name").and_then(Value::as_str).map(|s| s.to_string()))
        .filter(|l| has_local(&dir, game, Some(l)))
        .collect();
    if !out.iter().any(|l| l == "English") && catalog_path(&dir, game).exists() {
        out.insert(0, "English".into());
    }
    out
}

// ---------------------------------------------------------------------------
// Models (load once; reset to allow retry on failure, like models = null)
// ---------------------------------------------------------------------------

struct Models {
    // Session::run needs &mut; one Mutex each serialises concurrent scans
    // the way the intra=1 runtime serialises them internally anyway.
    corn: std::sync::Mutex<ort::session::Session>,
    milo: std::sync::Mutex<ort::session::Session>,
}

static ENV: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();
static MODELS: Mutex<Option<Result<Arc<Models>, String>>> = Mutex::new(None);

fn ort_env() -> Result<&'static ort::environment::Environment, String> {
    ENV.get_or_init(|| match std::env::var("ORT_DYLIB_PATH") {
        Ok(p) if !p.is_empty() => ort::init_from(&p)
            .map_err(|e| format!("ort load: {e}"))
            .and_then(|b| b.build().map_err(|e| format!("ort init: {e}"))),
        _ => Err("ORT_DYLIB_PATH unset: cvScan forwards to Node".into()),
    })
    .as_ref()
    .map_err(|e| e.clone())
}

fn load_models() -> Result<Arc<Models>, String> {
    let mut slot = MODELS.lock().unwrap();
    if let Some(res) = slot.as_ref() {
        return res.clone();
    }
    let dir = model_dir().ok_or("no CV_MODEL_DIR")?;
    let built = (|| -> Result<Arc<Models>, String> {
        let env = ort_env()?;
        let build = |file: PathBuf| -> Result<ort::session::Session, String> {
            let mut b = ort::session::Session::builder(env).map_err(|e| e.to_string())?;
            b = b.with_intra_threads(1).map_err(|e| e.to_string())?;
            b = b.with_inter_threads(1).map_err(|e| e.to_string())?;
            b = b.with_parallel_execution(false).map_err(|e| e.to_string())?;
            b.commit_from_file(file).map_err(|e| e.to_string())
        };
        let corn = build(dir.join("cornelius.onnx"))?;
        let milo = build(dir.join("milo.onnx"))?;
        Ok(Arc::new(Models { corn: std::sync::Mutex::new(corn), milo: std::sync::Mutex::new(milo) }))
    })();
    if built.is_err() {
        // Node: .catch(err => { models = null; throw err }) — retry on next call.
        let r = built;
        *slot = None;
        return r;
    }
    *slot = Some(built.clone());
    built
}

// ---------------------------------------------------------------------------
// Catalogs
// ---------------------------------------------------------------------------

pub struct Catalog {
    pub lang: String,
    pub local: bool,
    pub n: usize,
    pub dim: usize,
    pub cat: Vec<f32>,
    pub ids: Vec<String>,
    /// rowSets(): per-row lowercased card_cache.set_id, built lazily.
    set_of: Mutex<Option<Arc<Vec<Option<String>>>>>,
}

static CATALOGS: Mutex<Option<HashMap<String, Arc<Catalog>>>> = Mutex::new(None);

fn catalog_key(game: &str, use_lang: Option<&str>) -> String {
    format!("{game}{}", suffix(use_lang))
}

fn load_catalog(game: &str, lang: Option<&str>) -> Result<Arc<Catalog>, String> {
    // load(): fall back to English when the language has no local catalog.
    let dir = model_dir().ok_or("no CV_MODEL_DIR")?;
    let use_lang: Option<String> = match lang {
        Some(l) if has_local(&dir, game, Some(l)) => Some(l.to_string()),
        _ => None,
    };
    let key = catalog_key(game, use_lang.as_deref());
    {
        let map = CATALOGS.lock().unwrap();
        if let Some(c) = map.as_ref().and_then(|m| m.get(&key)) {
            return Ok(c.clone());
        }
    }
    let _models = load_models()?;
    let cat: Catalog = if use_lang.is_some() && has_local(&dir, game, use_lang.as_deref()) {
        let meta_raw = std::fs::read_to_string(local_meta(&dir, game, use_lang.as_deref()))
            .map_err(|e| e.to_string())?;
        let meta: Value = serde_json::from_str(&meta_raw).map_err(|e| e.to_string())?;
        let dim = meta.get("dim").and_then(Value::as_u64).ok_or("meta.dim")? as usize;
        let ids: Vec<String> = meta
            .get("ids")
            .and_then(Value::as_array)
            .ok_or("meta.ids")?
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        let n = ids.len();
        let buf = std::fs::read(local_bin(&dir, game, use_lang.as_deref())).map_err(|e| e.to_string())?;
        if buf.len() != n * dim * 4 {
            return Err(format!(
                "local {game} catalog is {} bytes, expected {}",
                buf.len(),
                n * dim * 4
            ));
        }
        let cat = buf
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Catalog { lang: use_lang.clone().unwrap_or_else(|| "English".into()), local: true, n, dim, cat, ids, set_of: Mutex::new(None) }
    } else {
        let members = npz::load_npz(&catalog_path(&dir, game).to_string_lossy())?;
        let emb = members
            .get("embeddings")
            .ok_or("npz: no embeddings")?
            .as_ref()
            .map_err(|e| e.clone())?;
        let f32s = match &emb.data {
            NpyData::F32(v) => v.clone(),
            _ => return Err("npz embeddings not f4".into()),
        };
        if emb.shape.len() != 2 {
            return Err("npz embeddings shape".into());
        }
        let n = emb.shape[0];
        let dim = emb.shape[1];
        let ids_member = members
            .get("card_ids")
            .ok_or("npz: no card_ids")?
            .as_ref()
            .map_err(|e| e.clone())?;
        let ids = match &ids_member.data {
            NpyData::Str(v) => v.clone(),
            _ => return Err("npz card_ids not <U".into()),
        };
        Catalog { lang: "English".into(), local: false, n, dim, cat: f32s, ids, set_of: Mutex::new(None) }
    };
    let arc = Arc::new(cat);
    {
        let mut map = CATALOGS.lock().unwrap();
        let m = map.get_or_insert_with(HashMap::new);
        m.insert(key, arc.clone());
    }
    Ok(arc)
}

/// loadAll(game, lang): native catalog first, English appended when the
/// native one is a different language.
fn load_all(game: &str, lang: Option<&str>) -> Result<Vec<Arc<Catalog>>, String> {
    let native = load_catalog(game, lang)?;
    if native.lang == "English" {
        return Ok(vec![native]);
    }
    match load_catalog(game, None) {
        Ok(en) => Ok(vec![native, en]),
        Err(_) => Ok(vec![native]),
    }
}

/// rowSets(): one pass over card_cache mapping catalog rows to lowercased
/// set ids; unknown rows → None ("not in your sets").
fn row_sets(c: &Arc<Catalog>, game: &str, pool: &crate::db::DbPool) -> Result<Arc<Vec<Option<String>>>, String> {
    {
        let g = c.set_of.lock().unwrap();
        if let Some(v) = g.as_ref() {
            return Ok(v.clone());
        }
    }
    let conn = pool.get().map_err(|e| e.to_string())?;
    let mut by_id: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, set_id FROM card_cache")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                let id: String = r.get(0)?;
                let set: Option<String> = r.get(1)?;
                Ok((id, set.unwrap_or_default().to_lowercase()))
            })
            .map_err(|e| e.to_string())?;
        for r in rows {
            let (id, set) = r.map_err(|e| e.to_string())?;
            by_id.insert(id, set);
        }
    }
    let set_of: Vec<Option<String>> = c
        .ids
        .iter()
        .map(|raw| {
            let id = raw.strip_suffix("_back").unwrap_or(raw).to_string();
            let key = if c.local { id } else { format!("{game}-{id}") };
            by_id.get(&key).filter(|s| !s.is_empty()).cloned()
        })
        .collect();
    let arc = Arc::new(set_of);
    *c.set_of.lock().unwrap() = Some(arc.clone());
    Ok(arc)
}

// ---------------------------------------------------------------------------
// Image twin: decode + lanczos3 resize + jpeg encode (sharp/libvips)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RawRgb {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// sharp(buffer).removeAlpha().raw(): content-sniffed decode, alpha dropped
/// (not composited) exactly like removeAlpha.
///
/// JPEG legs go through the pinned mozjpeg 4.1.5 companion shim
/// (data/lib/libscryjpeg.so, raw libjpeg API configured exactly like libvips
/// jpeg2vips.c) — the container decodes JPEGs with mozjpeg 4.1.5 and the
/// pure-Rust zune-jpeg decoder drifts several LSBs, which is enough to flip
/// near-tie candidates. Same dylib-pin pattern as ORT_DYLIB_PATH:
/// SCRYBOX_JPEG_LIB overrides; missing lib falls back to the image crate
/// (documented lane drift, never a failure).
pub fn decode_rgb(bytes: &[u8]) -> Result<RawRgb, String> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        if let Some(r) = mozjpeg_decode(bytes) {
            return Ok(r);
        }
    }
    let img = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| format!("decode: {e}"))?;
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    Ok(RawRgb { w: w as usize, h: h as usize, px: rgb.into_raw() })
}

type ScryDecode = unsafe extern "C" fn(
    *const u8,
    libc::c_long,
    *mut libc::c_int,
    *mut libc::c_int,
    *mut *mut u8,
) -> libc::c_int;

static SCRY_DECODE: OnceLock<Option<ScryDecode>> = OnceLock::new();

fn mozjpeg_decode(bytes: &[u8]) -> Option<RawRgb> {
    let dec = match SCRY_DECODE.get_or_init(|| {
        let path = std::env::var("SCRYBOX_JPEG_LIB").ok().filter(|s| !s.is_empty()).or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.join("../../../../data/lib/libscryjpeg.so"))
                .filter(|p| p.exists())
                .map(|p| p.to_string_lossy().into_owned())
        })?;
        let cpath = std::ffi::CString::new(path.clone()).ok()?;
        // RTLD_NOW (RTLD_LOCAL is 0 on glibc; 4 would be RTLD_NOLOAD, which
        // returns NULL for a not-yet-loaded lib — that silently disabled the
        // shim in every run until the A/B harness exposed the embed drift).
        let h = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW) };
        if h.is_null() {
            let errp = unsafe { libc::dlerror() };
            let err = if errp.is_null() {
                "<none>".to_string()
            } else {
                unsafe { std::ffi::CStr::from_ptr(errp).to_string_lossy().into_owned() }
            };
            eprintln!("[scryjpeg] dlopen FAILED for {path}: {err}");
            tracing::warn!("scryjpeg unavailable: JPEG decodes use the image crate (LSB drift vs mozjpeg)");
            return None;
        }
        let name = std::ffi::CString::new("scry_jpeg_decode").ok()?;
        let p = unsafe { libc::dlsym(h, name.as_ptr()) };
        if p.is_null() {
            eprintln!("[scryjpeg] dlsym FAILED");
            return None;
        }
        // SAFETY: resolved symbol has the ScryDecode signature from scryjpeg.c.
        Some(unsafe { std::mem::transmute::<*mut libc::c_void, ScryDecode>(p) })
    }) {
        Some(d) => *d,
        None => return None,
    };
    let mut w = 0i32;
    let mut h = 0i32;
    let mut px: *mut u8 = std::ptr::null_mut();
    // scry_jpeg_decode copies in via jpeg_mem_src (no borrow outlives the call).
    let rc = unsafe { dec(bytes.as_ptr(), bytes.len() as libc::c_long, &mut w, &mut h, &mut px) };
    if rc != 0 || px.is_null() || w <= 0 || h <= 0 {
        return None;
    }
    let len = (w as usize) * (h as usize) * 3;
    let owned = unsafe { std::slice::from_raw_parts_mut(px, len) }.to_vec();
    unsafe { libc::free(px as *mut libc::c_void) };
    Some(RawRgb { w: w as usize, h: h as usize, px: owned })
}

// ---------------------------------------------------------------------------
// libvips 8.15.3 resize twin (sharp's engine, container-pinned versions).
//
// sharp.resize -> vips_resize(scale, vscale, kernel=lanczos3, gap=0):
//   1. NO integer pre-shrink: resize.c's shrink/subsample fast paths are
//      NEAREST-kernel only, and sharp 0.33.5 never passes @gap (default
//      0.0), so lanczos3 is a single pure reducev-then-reduceh over the
//      FULL shrink factor (verified bit-exact vs container, scan1-photo);
//   2. reducev (if vscale<1) then reduceh (if hscale<1): table
//      lanczos3 in uchar fixed point — n = 2*rint(3*shrink)+1 taps, mask
//      quantised to VIPS_INTERPOLATE_SCALE=4096 (truncated), sampled at
//      VIPS_TRANSFORM_SCALE=64 sub-pixel steps, X accumulated per output
//      pixel (X += shrink from (0.5*shrink - 0.5 - offset)), sum + 2048
//      >> 12, clamp;
//   3. any upscale: affine with the bicubic interpolator.
// Mirroring the quantisation (not just "a lanczos3") is what makes the
// cornelius 384-fill — hence corners, hence the dewarp — agree with Node.
// ---------------------------------------------------------------------------

const TRANSFORM_SCALE: i32 = 64; // VIPS_TRANSFORM_SCALE (2^6)
const INTERP_SCALE: f64 = 4096.0; // VIPS_INTERPOLATE_SCALE (2^12)
const INTERP_ROUND: i32 = 2048;

#[inline]
fn vips_rint(x: f64) -> f64 {
    x.round_ties_even()
}

#[inline]
fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let px = x * std::f64::consts::PI;
        px.sin() / px
    }
}

#[inline]
fn lanczos3_filter(x: f64) -> f64 {
    if x >= -3.0 && x <= 3.0 {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// vips_reduce_get_points(VIPS_KERNEL_LANCZOS3, shrink).
fn reduce_points(shrink: f64) -> usize {
    (2.0 * vips_rint(3.0 * shrink) + 1.0) as usize
}

/// One axis of vips_reduce (uchar path): pre-quantised tap tables, one per
/// TRANSFORM_SCALE+1 sub-pixel positions.
struct ReduceAxis {
    out: usize,
    offset: f64,
    n: usize,
    matrixs: Vec<i16>, // [(TRANSFORM_SCALE+1) * n]
}

fn reduce_axis_build(in_size: usize, shrink: f64) -> ReduceAxis {
    // vips_reduceh_build: width = ROUND_UINT(in / shrink); the "centre"
    // offset shifts so discarded pixels split evenly across the ends.
    let out = ((in_size as f64 / shrink) + 0.5) as usize;
    let extra = out as f64 * shrink - in_size as f64;
    let offset = (1.0 + extra) / 2.0 - 1.0;
    let n = reduce_points(shrink);
    let mut matrixs = vec![0i16; (TRANSFORM_SCALE as usize + 1) * n];
    for tx in 0..=TRANSFORM_SCALE {
        let x = tx as f64 / TRANSFORM_SCALE as f64;
        let half = x + n as f64 / 2.0 - 1.0;
        let mut c = vec![0f64; n];
        let mut sum = 0f64;
        for i in 0..n {
            let xp = (i as f64 - half) / shrink;
            let l = lanczos3_filter(xp);
            c[i] = l;
            sum += l;
        }
        for i in 0..n {
            c[i] /= sum;
            // (short)(double * 4096) — C cast truncates toward zero.
            matrixs[tx as usize * n + i] = (c[i] * INTERP_SCALE) as i16;
        }
    }
    ReduceAxis { out, offset, n, matrixs }
}

/// vips_reduceh uchar path over the whole image (per output row, per pixel).
fn reduceh_uchar(src: &RawRgb, ax: &ReduceAxis, shrink: f64) -> RawRgb {
    let mut out = vec![0u8; ax.out * src.h * 3];
    let emb = (ax.n as f64 / 2.0).ceil() as isize - 1; // vips_embed left margin
    for y in 0..src.h {
        let mut bigx = 0.5 * shrink - 0.5 - ax.offset;
        for x in 0..ax.out {
            let ix = bigx as isize; // C (int) cast: toward zero (bigx >= 0)
            let six = (bigx * (TRANSFORM_SCALE as f64) * 2.0) as i32 & (TRANSFORM_SCALE * 2 - 1);
            let tx = ((six + 1) >> 1) as usize;
            let m = &ax.matrixs[tx * ax.n..tx * ax.n + ax.n];
            for ch in 0..3 {
                let mut sum: i32 = 0;
                for i in 0..ax.n {
                    let sxp = (ix + i as isize - emb).clamp(0, src.w as isize - 1) as usize;
                    sum += src.px[(y * src.w + sxp) * 3 + ch] as i32 * m[i] as i32;
                }
                let v = ((sum + INTERP_ROUND) >> 12).clamp(0, 255) as u8;
                out[(y * ax.out + x) * 3 + ch] = v;
            }
            bigx += shrink;
        }
    }
    RawRgb { w: ax.out, h: src.h, px: out }
}

/// vips_reducev uchar path.
fn reducev_uchar(src: &RawRgb, ax: &ReduceAxis, shrink: f64) -> RawRgb {
    let mut out = vec![0u8; src.w * ax.out * 3];
    let emb = (ax.n as f64 / 2.0).ceil() as isize - 1;
    let mut bigy = 0.5 * shrink - 0.5 - ax.offset; // vips_reducev_gen Y += vshrink
    for y in 0..ax.out {
        let iy = bigy as isize;
        let siy = (bigy * (TRANSFORM_SCALE as f64) * 2.0) as i32 & (TRANSFORM_SCALE * 2 - 1);
        let ty = ((siy + 1) >> 1) as usize;
        let m = &ax.matrixs[ty * ax.n..ty * ax.n + ax.n];
        for x in 0..src.w {
            for ch in 0..3 {
                let mut sum: i32 = 0;
                for i in 0..ax.n {
                    let syp = (iy + i as isize - emb).clamp(0, src.h as isize - 1) as usize;
                    sum += src.px[(syp * src.w + x) * 3 + ch] as i32 * m[i] as i32;
                }
                let v = ((sum + INTERP_ROUND) >> 12).clamp(0, 255) as u8;
                out[(y * src.w + x) * 3 + ch] = v;
            }
        }
        bigy += shrink;
    }
    RawRgb { w: src.w, h: ax.out, px: out }
}

/// vips_resize(scale, vscale, kernel='lanczos3') twin for the downscale
/// regime (every size the scan pipeline produces is a downscale).
pub fn resize_lanczos3(src: &RawRgb, ow: usize, oh: usize) -> RawRgb {
    let scale_x = ow as f64 / src.w as f64;
    let scale_y = oh as f64 / src.h as f64;
    let mut cur = src.clone();
    let hs = scale_x;
    let vs = scale_y;
    // 1. NO integer pre-shrink for lanczos3. vips_resize's shrink/subsample
    // fast paths are NEAREST-kernel only (resize.c lines 166-203); sharp
    // 0.33.5 calls vips_resize without @gap (default 0.0), so lanczos3 is a
    // single pure reducev-then-reduceh over the FULL shrink factor
    // (reduceh/reducev's internal gap-shrink is gap>=1-only too).
 // 2. residual reducev then reduceh (vips_resize order).
    if vs < 1.0 {
        let shrink = 1.0 / vs;
        let ax = reduce_axis_build(cur.h, shrink);
        cur = reducev_uchar(&cur, &ax, shrink);
    }
    if hs < 1.0 {
        let shrink = 1.0 / hs;
        let ax = reduce_axis_build(cur.w, shrink);
        cur = reduceh_uchar(&cur, &ax, shrink);
    }
    // 3. upscale leg (client crops smaller than the embed square): vips uses
    // affine+bicubic; a plain bilinear sits within a few LSB of it and no
    // scan-lane probe hits this path (accepted divergence, logged in
    // PROGRESS.md).
    if hs > 1.0 || vs > 1.0 {
        let owu = ow;
        let ohu = oh;
        let mut out = vec![0u8; owu * ohu * 3];
        for y in 0..ohu {
            for x in 0..owu {
                let fx = ((x as f64 + 0.5) / hs - 0.5).clamp(0.0, cur.w as f64 - 1.0);
                let fy = ((y as f64 + 0.5) / vs - 0.5).clamp(0.0, cur.h as f64 - 1.0);
                let x0 = fx as usize;
                let y0 = fy as usize;
                let x1 = (x0 + 1).min(cur.w - 1);
                let y1 = (y0 + 1).min(cur.h - 1);
                let wx = fx - x0 as f64;
                let wy = fy - y0 as f64;
                for ch in 0..3 {
                    let p00 = cur.px[(y0 * cur.w + x0) * 3 + ch] as f64;
                    let p01 = cur.px[(y0 * cur.w + x1) * 3 + ch] as f64;
                    let p10 = cur.px[(y1 * cur.w + x0) * 3 + ch] as f64;
                    let p11 = cur.px[(y1 * cur.w + x1) * 3 + ch] as f64;
                    out[(y * owu + x) * 3 + ch] =
                        ((p00 * (1.0 - wx) + p01 * wx) * (1.0 - wy) + (p10 * (1.0 - wx) + p11 * wx) * wy) as u8;
                }
            }
        }
        return RawRgb { w: owu, h: ohu, px: out };
    }
    cur
}

/// sharp.resize(w, h, {fit:'fill'}) on an already-decoded image.
pub fn resize_fill(src: &RawRgb, ow: usize, oh: usize) -> RawRgb {
    resize_lanczos3(src, ow, oh)
}

/// sharp.resize({width, withoutEnlargement:true}) — proportional, never up.
pub fn resize_width(src: &RawRgb, width: usize) -> RawRgb {
    if src.w <= width {
        return RawRgb { w: src.w, h: src.h, px: src.px.clone() };
    }
    let scale = width as f64 / src.w as f64;
    let oh = (src.h as f64 * scale).round().max(1.0) as usize;
    resize_lanczos3(src, width, oh)
}

/// sharp.resize({width: 220}) — proportional, upscaling allowed (never up here).
fn resize_width_allow_up(src: &RawRgb, width: usize) -> RawRgb {
    let scale = width as f64 / src.w as f64;
    let oh = (src.h as f64 * scale).round().max(1.0) as usize;
    resize_lanczos3(src, width, oh)
}

/// sharp(...).jpeg({quality:70}) — the image crate's baseline encoder; byte
/// output differs from mozjpeg (accepted divergence, decode-compared).
fn encode_jpeg(img: &RawRgb) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let mut cur = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, CROP_QUALITY)
        .write_image(&img.px, img.w as u32, img.h as u32, image::ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    Ok(cur.into_inner())
}

// ---------------------------------------------------------------------------
// Geometry twins
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

/// orderQuad (shared/imgproc.mjs:546): atan2 sort about the centroid, the
/// min-x+y corner as TL, then the two-branch CW/CCW decision.
pub fn order_quad(p: [Pt; 4]) -> [Pt; 4] {
    let cx = (p[0].x + p[1].x + p[2].x + p[3].x) / 4.0;
    let cy = (p[0].y + p[1].y + p[2].y + p[3].y) / 4.0;
    let mut sorted = p;
    // imgproc.mjs: sort ASCENDING on atan2(y-cy, x-cx); Array#sort is stable.
    sorted.sort_by(|a, b| {
        (a.y - cy).atan2(a.x - cx).partial_cmp(&(b.y - cy).atan2(b.x - cx)).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut tl_idx = 0usize;
    let mut min = f64::INFINITY;
    for i in 0..4 {
        let score = sorted[i].x + sorted[i].y;
        if score < min {
            min = score;
            tl_idx = i;
        }
    }
    let tl = sorted[tl_idx];
    let next = sorted[(tl_idx + 1) % 4];
    let prev = sorted[(tl_idx + 3) % 4];
    if next.x - tl.x > prev.x - tl.x || prev.y - tl.y > next.y - tl.y {
        [tl, next, sorted[(tl_idx + 2) % 4], prev]
    } else {
        [tl, prev, sorted[(tl_idx + 2) % 4], next]
    }
}

/// perspectiveTransform (cvScan.js): 8x8 DLT with partial pivoting; null on a
/// zero pivot (the caller falls back to a plain 448 fill).
pub fn perspective_transform(src: &[Pt; 4], dst: &[Pt; 4]) -> Option<[f64; 8]> {
    let mut a = [[0f64; 8]; 8];
    let mut b = [0f64; 8];
    for i in 0..4 {
        let s = src[i];
        let d = dst[i];
        a[2 * i] = [s.x, s.y, 1.0, 0.0, 0.0, 0.0, -s.x * d.x, -s.y * d.x];
        b[2 * i] = d.x;
        a[2 * i + 1] = [0.0, 0.0, 0.0, s.x, s.y, 1.0, -s.x * d.y, -s.y * d.y];
        b[2 * i + 1] = d.y;
    }
    for col in 0..8 {
        let mut piv = col;
        for r in col + 1..8 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        a.swap(col, piv);
        b.swap(col, piv);
        let p = a[col][col];
        if p == 0.0 {
            return None;
        }
        for r in 0..8 {
            if r == col {
                continue;
            }
            let factor = a[r][col] / p;
            for k in col..8 {
                a[r][k] -= factor * a[col][k];
            }
            b[r] -= factor * b[col];
        }
    }
    let mut h = [0f64; 8];
    for r in 0..8 {
        h[r] = b[r] / a[r][r];
    }
    Some(h)
}

/// warpRgb: inverse-map the EMBED_SIZE square through the homography with
/// bilinear sampling (JS `sx|0` floor, edge clamps, out-of-quad stays black).
pub fn warp_rgb(src: &[u8], sw: usize, sh: usize, m: &[f64; 8], size: usize) -> Vec<u8> {
    let [a, b, c, d, e, f, g, i] = *m;
    let det = a * (e - f * i) - b * (d - f * g) + c * (d * i - e * g);
    let inv = [
        (e - f * i) / det,
        (c * i - b) / det,
        (b * f - c * e) / det,
        (f * g - d) / det,
        (a - c * g) / det,
        (c * d - a * f) / det,
        (d * i - e * g) / det,
        (b * g - a * i) / det,
        (a * e - b * d) / det,
    ];
    let mut out = vec![0u8; size * size * 3];
    for y in 0..size {
        for x in 0..size {
            let den = inv[6] * x as f64 + inv[7] * y as f64 + inv[8];
            let sx = (inv[0] * x as f64 + inv[1] * y as f64 + inv[2]) / den;
            let sy = (inv[3] * x as f64 + inv[4] * y as f64 + inv[5]) / den;
            let o = (y * size + x) * 3;
            if !(sx >= 0.0 && sy >= 0.0 && sx <= sw as f64 - 1.0 && sy <= sh as f64 - 1.0) {
                continue;
            }
            let x0 = sx as usize; // sx | 0 — sx >= 0 here
            let y0 = sy as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let y1 = (y0 + 1).min(sh - 1);
            let fx = sx - x0 as f64;
            let fy = sy - y0 as f64;
            for ch in 0..3 {
                let p00 = src[(y0 * sw + x0) * 3 + ch] as f64;
                let p01 = src[(y0 * sw + x1) * 3 + ch] as f64;
                let p10 = src[(y1 * sw + x0) * 3 + ch] as f64;
                let p11 = src[(y1 * sw + x1) * 3 + ch] as f64;
                out[o + ch] = ((p00 * (1.0 - fx) + p01 * fx) * (1.0 - fy)
                    + (p10 * (1.0 - fx) + p11 * fx) * fy) as u8;
            }
        }
    }
    out
}

/// toTensor: ImageNet-normalised planar NCHW float32 [1,3,size,size]. JS
/// computes in double and rounds ONCE into the Float32Array — mirror that.
fn to_tensor(rgb: &[u8], size: usize) -> Vec<f32> {
    let plane = size * size;
    let mut x = vec![0f32; 3 * plane];
    for p in 0..plane {
        x[p] = ((rgb[p * 3] as f64 / 255.0 - MEAN[0]) / STD[0]) as f32;
        x[plane + p] = ((rgb[p * 3 + 1] as f64 / 255.0 - MEAN[1]) / STD[1]) as f32;
        x[2 * plane + p] = ((rgb[p * 3 + 2] as f64 / 255.0 - MEAN[2]) / STD[2]) as f32;
    }
    x
}

fn run_session(
    session: &std::sync::Mutex<ort::session::Session>,
    data: Vec<f32>,
    size: usize,
    outputs: &[&str],
) -> Result<Vec<Vec<f32>>, String> {
    let t = ort::value::Value::from_array((vec![1usize, 3, size, size], data))
        .map_err(|e| e.to_string())?;
    let mut guard = session.lock().map_err(|e| e.to_string())?;
    let outs = guard
        .run(ort::inputs! { "image" => t })
        .map_err(|e| e.to_string())?;
    outputs
        .iter()
        .map(|name| {
            let v = outs[*name].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            Ok(v.1.to_vec())
        })
        .collect()
}

struct Det {
    rgb: Vec<u8>,
    detected: bool,
    sharpness: f32,
}

/// useClientCrop: an already-rectified browser crop is resized, not trusted.
fn use_client_crop(decoded: &RawRgb) -> Det {
    let r = resize_fill(decoded, EMBED_SIZE, EMBED_SIZE);
    Det { rgb: r.px, detected: true, sharpness: 1.0 }
}

/// detectAndDewarp (cvScan.js:276): decode at most 1200 wide, cornelius on the
/// 384-fill, sharpness gate, orderQuad, homography, bilinear dewarp to 448.
/// Undetected (or a degenerate quad) falls back to the plain 448-fill of the
/// ORIGINAL frame — matching the pre-detector behaviour on purpose.
fn detect_and_dewarp(corn: &std::sync::Mutex<ort::session::Session>, decoded: &RawRgb) -> Result<Det, String> {
    let small = resize_fill(decoded, CORN_SIZE, CORN_SIZE);
    let out = run_session(corn, to_tensor(&small.px, CORN_SIZE), CORN_SIZE, &["corners", "sharpness"])?;
    let corners = &out[0];
    // out.sharpness ? out.sharpness.data[0] : 1
    let sharpness = if out[1].is_empty() { 1.0 } else { out[1][0] };
    if !(sharpness > SHARPNESS_GATE) || corners.len() < 8 {
        let r = resize_fill(decoded, EMBED_SIZE, EMBED_SIZE);
        return Ok(Det { rgb: r.px, detected: false, sharpness });
    }
    let rgb1200 = resize_width(decoded, MAX_SIDE);
    let iw = rgb1200.w;
    let ih = rgb1200.h;
    let pts: Vec<Pt> = (0..4)
        .map(|k| Pt { x: (corners[k * 2] as f64) * iw as f64, y: (corners[k * 2 + 1] as f64) * ih as f64 })
        .collect();
    let quad = [pts[0], pts[1], pts[2], pts[3]];
    let ordered = order_quad(quad);
    if std::env::var("SCAN_DEBUG").is_ok() {
        eprintln!("[dewarp] iw={iw} ih={ih} raw={:?} ordered={:?}", corners, ordered);
    }
    if ordered[0].x == ordered[1].x && ordered[0].y == ordered[1].y {
        let r = resize_fill(decoded, EMBED_SIZE, EMBED_SIZE);
        return Ok(Det { rgb: r.px, detected: false, sharpness });
    }
    // cvScan.js: dst = 0..EMBED_SIZE-1 (NOT 448) — the homography maps the
    // ordered quad onto the 447-pixel span, pixel-centre to pixel-centre.
    let dst = [
        Pt { x: 0.0, y: 0.0 },
        Pt { x: (EMBED_SIZE - 1) as f64, y: 0.0 },
        Pt { x: (EMBED_SIZE - 1) as f64, y: (EMBED_SIZE - 1) as f64 },
        Pt { x: 0.0, y: (EMBED_SIZE - 1) as f64 },
    ];
    match perspective_transform(&ordered, &dst) {
        Some(m) => Ok(Det { rgb: warp_rgb(&rgb1200.px, iw, ih, &m, EMBED_SIZE), detected: true, sharpness }),
        None => {
            let r = resize_fill(decoded, EMBED_SIZE, EMBED_SIZE);
            Ok(Det { rgb: r.px, detected: false, sharpness })
        }
    }
}

/// searchTopK: Float32Array sims (-inf for filtered rows), insert-sorted top-k.
pub fn search_top_k(
    emb: &[f32],
    cat: &[f32],
    n: usize,
    dim: usize,
    k: usize,
    allow: Option<&dyn Fn(usize) -> bool>,
) -> Vec<(usize, f32)> {
    let mut sims = vec![0f32; n];
    for i in 0..n {
        if let Some(a) = allow {
            if !a(i) {
                sims[i] = f32::NEG_INFINITY;
                continue;
            }
        }
        let off = i * dim;
        let mut s = 0f64;
        for d in 0..dim {
            s += emb[d] as f64 * cat[off + d] as f64;
        }
        sims[i] = s as f32;
    }
    let mut top: Vec<usize> = Vec::with_capacity(k);
    for i in 0..n {
        if top.len() < k {
            top.push(i);
            if top.len() == k {
                top.sort_by(|a, b| sims[*b].partial_cmp(&sims[*a]).unwrap_or(std::cmp::Ordering::Equal));
            }
            continue;
        }
        if sims[i] <= sims[top[k - 1]] {
            continue;
        }
        let mut j = k - 1;
        while j > 0 && sims[top[j - 1]] < sims[i] {
            top[j] = top[j - 1];
            j -= 1;
        }
        top[j] = i;
    }
    top.into_iter().map(|i| (i, sims[i])).collect()
}

// ---------------------------------------------------------------------------
// Node Buffer.from(s, 'base64') tolerance: invalid characters are SKIPPED and
// decoding STOPS at the first '=' (padding). Node then decodes the surviving
// stream 4 chars → 3 bytes (2 chars → 1 byte, 3 → 2, lone 1 → dropped).
// ---------------------------------------------------------------------------

pub fn node_base64_decode(s: &str) -> Vec<u8> {
    const TBL: fn(u8) -> Option<u8> = |c| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut vals: Vec<u8> = Vec::with_capacity(s.len() / 4 * 3 + 4);
    for &b in s.as_bytes() {
        if b == b'=' {
            break;
        }
        if let Some(v) = TBL(b) {
            vals.push(v);
        }
    }
    let mut out = Vec::with_capacity(vals.len() / 4 * 3 + 3);
    let mut i = 0;
    while i + 1 < vals.len() {
        let a = vals[i] as u32;
        let b = vals[i + 1] as u32;
        if i + 3 < vals.len() {
            let c = vals[i + 2] as u32;
            let d = vals[i + 3] as u32;
            out.push(((a << 2) | (b >> 4)) as u8);
            out.push((((b & 0xf) << 4) | (c >> 2)) as u8);
            out.push((((c & 3) << 6) | d) as u8);
        } else if i + 2 < vals.len() {
            let c = vals[i + 2] as u32;
            out.push(((a << 2) | (b >> 4)) as u8);
            out.push((((b & 0xf) << 4) | (c >> 2)) as u8);
        } else {
            out.push(((a << 2) | (b >> 4)) as u8);
        }
        i += 4;
    }
    out
}

// ---------------------------------------------------------------------------
// match() — cvScan.js:355
// ---------------------------------------------------------------------------

pub struct MatchError(pub String);

/// The pieces of `req.body` the route reads, with JS coercion semantics.
pub struct MatchReq {
    pub image: Vec<u8>,
    pub sets: Vec<String>,
    pub lang: String,
    pub cropped: bool,
}

/// JS String(x) for set tokens / truthiness gate inside parseSetList.
fn parse_set_list(set: Option<&Value>) -> Result<Vec<String>, ()> {
    // (setQuery || '').split(',').map(trim).filter(Boolean) — a non-string,
    // non-nullish set reaches (x).split → TypeError → route 500s (Err).
    let s = match set {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(s)) => s.clone(),
        // Numbers/objects/arrays: (x || '') keeps the value, .split throws.
        _ => return Err(()),
    };
    Ok(s.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()).map(String::from).collect())
}

/// Full pipeline; returns the res.json body (pre-hydration) in Node's key order.
pub fn match_scan(
    buf: &[u8],
    game: &str,
    top_k: usize,
    sets: &[String],
    lang: Option<&str>,
    cropped: bool,
    pool: &crate::db::DbPool,
) -> Result<Value, MatchError> {
    let cats = load_all(game, lang).map_err(MatchError)?;

    // Set scoping is a per-catalog FILTER (drop catalogs with no rows in scope;
    // all-empty falls back to an unscoped sweep of every catalog).
    let wanted: Vec<String> = sets.iter().map(|x| x.to_lowercase()).filter(|x| !x.is_empty()).collect();
    let want: std::collections::HashSet<&str> = wanted.iter().map(String::as_str).collect();
    let mut search: Vec<(Arc<Catalog>, Option<Arc<Vec<Option<String>>>>)> = Vec::new();
    let mut scoped: Option<(usize, usize)> = None;
    if wanted.is_empty() {
        search = cats.iter().map(|c| (c.clone(), None)).collect();
    } else {
        for c in &cats {
            let set_of = row_sets(c, game, pool).map_err(MatchError)?;
            let rows = set_of.iter().filter(|v| v.as_deref().is_some_and(|v| want.contains(v))).count();
            if rows == 0 {
                continue;
            }
            search.push((c.clone(), Some(set_of)));
            let prev = scoped.map(|(_, r)| r).unwrap_or(0);
            scoped = Some((0, prev + rows));
        }
        if search.is_empty() {
            search = cats.iter().map(|c| (c.clone(), None)).collect();
        }
    }

    let models = load_models().map_err(MatchError)?;
    let decoded = decode_rgb(buf).map_err(MatchError)?;
    let det = if cropped {
        use_client_crop(&decoded)
    } else {
        detect_and_dewarp(&models.corn, &decoded).map_err(MatchError)?
    };

    let emb = run_session(&models.milo, to_tensor(&det.rgb, EMBED_SIZE), EMBED_SIZE, &["embedding"])
        .map_err(MatchError)?
        .remove(0);

    // Per-catalog sweeps; gap is measured INSIDE the producing catalog.
    let mut hits: Vec<(usize, f32, usize)> = Vec::new(); // (row, sim, catalog idx)
    let mut winner: Option<(f32, Option<f64>)> = None;
    for (ci, (c, allow)) in search.iter().enumerate() {
        let allow_fn: Option<Box<dyn Fn(usize) -> bool>> = allow.as_ref().map(|so| {
            let so = so.clone();
            let want = want.clone();
            Box::new(move |i: usize| -> bool {
                so.get(i).and_then(|v| v.as_deref()).is_some_and(|v| want.contains(v))
            }) as Box<dyn Fn(usize) -> bool>
        });
        let k = top_k.max(GAP_K);
        let mut own = search_top_k(&emb, &c.cat, c.n, c.dim, k, allow_fn.as_deref());
        own.retain(|(_, s)| s.is_finite());
        if own.is_empty() {
            continue;
        }
        let tail = &own[1.min(own.len())..own.len().min(GAP_K)];
        let gap = if tail.len() >= 2 {
            let sum: f64 = tail.iter().map(|(_, s)| *s as f64).sum();
            Some(own[0].1 as f64 - sum / tail.len() as f64)
        } else {
            None
        };
        if winner.as_ref().is_none_or(|(w, _)| (own[0].1 as f64) > *w as f64) {
            winner = Some((own[0].1, gap));
        }
        for (i, sim) in own {
            hits.push((i, sim, ci));
        }
    }
    // Stable sort, descending (JS Array#sort is stable).
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Dedupe by the `_back`-stripped id — per LANGUAGE, both faces of a DFC are
    // one printing, the same card in two catalogs is not.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut candidates: Vec<Map<String, Value>> = Vec::new();
    for (i, sim, ci) in &hits {
        let c = &search[*ci].0;
        let raw = c.ids.get(*i).cloned().unwrap_or_default();
        let id = raw.strip_suffix("_back").unwrap_or(&raw).to_string();
        if !seen.insert(id.clone()) {
            continue;
        }
        let card_id = if c.local { id.clone() } else { format!("{game}-{id}") };
        let mut m = Map::new();
        m.insert("cardId".into(), Value::String(card_id));
        m.insert("score".into(), serde_json::Number::from_f64(*sim as f64).map(Value::Number).unwrap_or(Value::Null));
        m.insert("catalogLang".into(), Value::String(c.lang.clone()));
        candidates.push(m);
        if candidates.len() >= top_k {
            break;
        }
    }

    let crop_jpeg = {
        let img = RawRgb { w: EMBED_SIZE, h: EMBED_SIZE, px: det.rgb.clone() };
        let small = resize_width_allow_up(&img, CROP_SIDE);
        base64_encode(&encode_jpeg(&small).map_err(MatchError)?)
    };

    let mut out = Map::new();
    out.insert("game".into(), Value::String(game.into()));
    out.insert("verified".into(), Value::Bool(false));
    out.insert("candidates".into(), Value::Array(candidates.iter().cloned().map(Value::Object).collect()));
    out.insert("crop".into(), Value::String(format!("data:image/jpeg;base64,{crop_jpeg}")));
    if let Some((_, rows)) = scoped {
        out.insert("scoped".into(), Value::String(wanted.join(",")));
        out.insert("scopedRows".into(), Value::from(rows as u64));
    }
    out.insert("lang".into(), Value::String("en".into()));
    out.insert("catalogs".into(), Value::Array(search.iter().map(|(c, _)| Value::String(c.lang.clone())).collect()));
    out.insert("detected".into(), Value::Bool(det.detected));
    out.insert("sharpness".into(), serde_json::Number::from_f64(det.sharpness as f64).map(Value::Number).unwrap_or(Value::Null));
    let score_of = |m: &Map<String, Value>| m.get("score").and_then(Value::as_f64).unwrap_or(0.0);
    let top = candidates.first();
    let margin: f64 = if candidates.len() > 1 {
        score_of(top.unwrap()) - score_of(&candidates[1])
    } else if let Some(t) = top {
        score_of(t)
    } else {
        0.0
    };
    out.insert("margin".into(), serde_json::Number::from_f64(margin).map(Value::Number).unwrap_or(Value::from(0.0)));
    let gap = winner.map(|(_, g)| g).unwrap_or(None);
    out.insert("gap".into(), gap.and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::Null));
    let not_in_catalog = top.is_none() || gap.is_some_and(|g| g < gap_floor());
    out.insert("notInCatalog".into(), Value::Bool(not_in_catalog));
    out.insert("engine".into(), Value::String("collectorvision".into()));
    Ok(Value::Object(out))
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

// ---------------------------------------------------------------------------
// POST /api/scan-match (routes/collection.js:279)
// ---------------------------------------------------------------------------

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use std::net::SocketAddr;

use crate::auth::{authenticate, bearer_token};
use crate::cards_tags::read_body;
use crate::ip_limit::{self, FixedWindowLimiter};
use crate::AppState;

/// express-rate-limit v8 searchLimiter twin: 300 / 15 min per client IP.
fn search_limiter() -> &'static FixedWindowLimiter {
    static L: OnceLock<FixedWindowLimiter> = OnceLock::new();
    L.get_or_init(|| FixedWindowLimiter::new(300, 15 * 60 * 1000))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

fn owns(st: &AppState) -> bool {
    st.owns("scan_match")
}

/// getCardById's cache leg only (SELECT * FROM card_cache WHERE id = ? then
/// parseCardRow). A miss means Node WOULD FETCH from Scryfall — unit-6a
/// hydration policy: Forward the whole request rather than half-answer.
enum Hydrate {
    Done(Map<String, Value>),
    Unchanged(Map<String, Value>),
    Forward,
}

fn hydrate_candidate(
    conn: &rusqlite::Connection,
    cand: &Map<String, Value>,
    lang_name: &str,
) -> Result<Hydrate, String> {
    use rusqlite::types::Value as SqlValue;
    let card_id = match cand.get("cardId") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return Ok(Hydrate::Unchanged(cand.clone())),
    };
    let row = crate::decks_reads::one_row(conn, "SELECT * FROM card_cache WHERE id = ?1", &[SqlValue::Text(card_id.clone())])
        .map_err(|e| e.to_string())?;
    let Some(row) = row else {
        // Node would scryGet(/cards/<uuid>) + cacheCards → Forward.
        return Ok(Hydrate::Forward);
    };
    let card = Value::Object(crate::decks_reads::parse_card_row(&row).map_err(|_| "parseCardRow".to_string())?);

    // getPrintingInLang(card.set_id, card.number, langName) — cache-only leg.
    let (_code, scry) = crate::search::lang_codes(Some(lang_name));
    let mut use_card = card.clone();
    if scry != "en" {
        let set_id = card.get("set_id").cloned().unwrap_or(Value::Null);
        let number = card.get("number").cloned().unwrap_or(Value::Null);
        let set_str = match &set_id {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => crate::mfx_payload::js_to_string(other),
        };
        if !set_str.is_empty() && truthy(Some(&number)) {
            let name = crate::search::to_name(Some(&scry));
            let number_str = if number.is_string() {
                number.as_str().unwrap_or_default().to_string()
            } else {
                crate::mfx_payload::js_to_string(&number)
            };
            let loc = crate::decks_reads::one_row(
                conn,
                "SELECT * FROM card_cache WHERE set_id = ?1 AND number = ?2 AND language = ?3 LIMIT 1",
                &[SqlValue::Text(set_str.to_lowercase()), SqlValue::Text(number_str), SqlValue::Text(name)],
            )
            .map_err(|e| e.to_string())?;
            match loc {
                Some(loc_row) => {
                    use_card = Value::Object(
                        crate::decks_reads::parse_card_row(&loc_row).map_err(|_| "parseCardRow".to_string())?,
                    );
                }
                None => {
                    // Node would scryGet the localized printing → Forward.
                    return Ok(Hydrate::Forward);
                }
            }
        }
    }

    // {...cand, name, set, number, card} — existing keys keep their position,
    // the four hydration keys append in this order.
    let mut out = cand.clone();
    out.insert("name".into(), use_card.get("name").cloned().unwrap_or(Value::Null));
    out.insert("set".into(), use_card.get("set_id").cloned().unwrap_or(Value::Null));
    out.insert("number".into(), use_card.get("number").cloned().unwrap_or(Value::Null));
    out.insert("card".into(), use_card);
    Ok(Hydrate::Done(out))
}

pub async fn handle_scan_match(State(st): State<AppState>, cc: ConnectInfo<SocketAddr>, req: Request) -> Response {
    if req.method() != axum::http::Method::POST || !owns(&st) {
        return crate::proxy(State(st), req).await;
    }
    // authenticateToken (global /api gate, runs before bigJson + the limiter).
    let pool = match st.db.as_ref() {
        Some(p) => p.clone(),
        None => return crate::proxy(State(st), req).await,
    };
    let token = bearer_token(req.headers()).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => {
            let (parts, body) = req.into_parts();
            let _ = axum::body::to_bytes(body, usize::MAX).await;
            let _ = parts;
            return res;
        }
    };
    // Node bumps the user's stats generation on this authenticated POST
    // (invalidateOnWrite runs before the collection router).
    if st.owns("stats") || st.owns("stats_subs") {
        st.stats_cache.bump(user.id);
    }

    let (parts, bytes) = read_body(req).await;
    let headers = parts.headers.clone();
    let body = match cardscan::parse_outer_json(&headers, &bytes) {
        Ok(v) => v,
        // strict bigJson rejects scalars/null/oversize — Node's parser owns
        // that error page; replay the untouched request.
        Err(_) => return crate::proxy(State(st), replay(parts, bytes, headers)).await,
    };

    // searchLimiter AFTER auth (Node's middleware order): 300 / 15 min / IP.
    let peer = Some(cc.0.ip());
    let key = ip_limit::client_ip_key(peer, &headers, &st.trust_proxy);
    let verdict = search_limiter().hit(&key, now_ms());
    let limit_headers = |mut res: Response| -> Response {
        let h = res.headers_mut();
        h.insert("ratelimit-policy", HeaderValue::from_static("300;w=900"));
        h.insert("ratelimit-limit", HeaderValue::from_static("300"));
        h.insert("ratelimit-remaining", HeaderValue::from_str(&verdict.remaining.to_string()).unwrap());
        h.insert("ratelimit-reset", HeaderValue::from_str(&verdict.reset_secs.to_string()).unwrap());
        if verdict.blocked {
            h.insert("retry-after", HeaderValue::from_str(&verdict.reset_secs.to_string()).unwrap());
        }
        res
    };
    if verdict.blocked {
        return limit_headers(crate::cards_tags::json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many searches. Please slow down and try again shortly.",
        ));
    }

    // const { image, set = '', lang, cropped = false } = req.body || {}
    let body_obj: Map<String, Value> = match body {
        Value::Object(m) => m,
        // body-parser strict accepts arrays too; destructuring an array
        // yields all-undefined → the Missing image 400 below.
        _ => Map::new(),
    };
    let image = match body_obj.get("image") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return limit_headers(cardscan::res_json(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": "Missing image" }),
            ));
        }
    };
    if image.is_empty() {
        return limit_headers(cardscan::res_json(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({ "error": "Missing image" }),
        ));
    }
    let base64 = match image.find(',') {
        Some(i) => image[i + 1..].to_string(),
        None => image.clone(),
    };
    let buf = node_base64_decode(&base64);
    if buf.len() < 100 {
        return limit_headers(cardscan::res_json(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({ "error": "Invalid image data" }),
        ));
    }
    let lang_input: Option<String> = match body_obj.get("lang") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(crate::mfx_payload::js_to_string(other)),
    };
    let lang_name = crate::search::to_name(lang_input.as_deref());
    let cropped = truthy(body_obj.get("cropped"));

    // Node's order: the isBuilt gate is checked BEFORE the match() call, and
    // parseSetList only throws while evaluating match()'s arguments — so a
    // not-built language with a garbage `set` answers 503, not 500.
    if !is_built("mtg", &lang_name) {
        return limit_headers(cardscan::res_json(
            StatusCode::SERVICE_UNAVAILABLE,
            &serde_json::json!({
                "lang": lang_name,
                "candidates": [],
                "verified": false,
                "notBuilt": true,
                "error": "No scan catalog is built for this language yet. An admin can build one from Settings → Scan catalogs.",
            }),
        ));
    }
    let sets = match parse_set_list(body_obj.get("set")) {
        Ok(s) => s,
        // set is a number/object/array: (set||'').split throws upstream → 500.
        Err(_) => {
            return limit_headers(cardscan::res_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": "Scan match failed" }),
            ));
        }
    };
    // No ORT dylib configured → the pipeline cannot run here; Node (with its
    // own bundled runtime) answers the whole request.
    if std::env::var("ORT_DYLIB_PATH").unwrap_or_default().is_empty() {
        return crate::proxy(State(st), replay(parts, bytes, headers)).await;
    }

    let pool2 = pool.clone();
    let job = {
        let buf = buf.clone();
        let sets = sets.clone();
        let lang_name2 = lang_name.clone();
        tokio::task::spawn_blocking(move || match_scan(&buf, "mtg", TOP_K, &sets, Some(&lang_name2), cropped, &pool2))
    };
    let result = match job.await {
        Ok(Ok(v)) => v,
        // Node's catch prints the message and answers the same 500 either way.
        other => {
            match &other {
                Ok(Err(e)) => tracing::warn!("scan-match match_scan failed: {}", e.0),
                Err(join) => tracing::warn!("scan-match match_scan panicked: {join}"),
                Ok(Ok(_)) => {}
            }
            return limit_headers(cardscan::res_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": "Scan match failed" }),
            ));
        }
    };

    // Hydrate candidates ({name,set,number,card}); any would-fetch forwards the
    // WHOLE request so Node re-runs it end-to-end (unit-6a policy).
    let mut result = match result {
        Value::Object(m) => m,
        _ => {
            return limit_headers(cardscan::res_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": "Scan match failed" }),
            ));
        }
    };
    let cands = result
        .get("candidates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => {
            return limit_headers(cardscan::res_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": "Scan match failed" }),
            ));
        }
    };
    let mut hydrated = Vec::with_capacity(cands.len());
    for cand in &cands {
        let cand_map = match cand {
            Value::Object(m) => m.clone(),
            other => {
                hydrated.push(other.clone());
                continue;
            }
        };
        match hydrate_candidate(&conn, &cand_map, &lang_name) {
            Ok(Hydrate::Done(m)) => hydrated.push(Value::Object(m)),
            Ok(Hydrate::Unchanged(m)) => hydrated.push(Value::Object(m)),
            // would-fetch or DB oddity: whole-request replay to Node.
            Ok(Hydrate::Forward) | Err(_) => {
                tracing::warn!(card = ?cand_map.get("cardId"), "scan-match hydrate Forward-to-Node");
                return crate::proxy(State(st), replay(parts, bytes, headers)).await;
            }
        }
    }
    result.insert("candidates".into(), Value::Array(hydrated));
    limit_headers(cardscan::res_json(StatusCode::OK, &Value::Object(result)))
}

fn replay(parts: axum::http::request::Parts, bytes: Vec<u8>, headers: HeaderMap) -> Request {
    let mut req = Request::from_parts(parts, Body::from(bytes));
    *req.headers_mut() = headers;
    req
}

/// server.js startup warm twin: preload models + the English catalog so the
/// first scan pays npz/ORT init costs once, at boot, not in a request.
pub fn spawn_warm() {
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(|| {
            if is_built("mtg", "English") {
                let _ = load_all("mtg", None);
            }
        })
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f64, y: f64) -> Pt {
        Pt { x, y }
    }

    #[test]
    fn order_quad_clockwise_from_topleft() {
        // Shuffled TL,BR,TR,BL input of a square must come out TL,TR,BR,BL.
        let q = [pt(300.0, 500.0), pt(900.0, 100.0), pt(950.0, 520.0), pt(280.0, 90.0)];
        let o = order_quad(q);
        assert_eq!((o[0].x, o[0].y), (280.0, 90.0)); // TL
        assert_eq!((o[1].x, o[1].y), (900.0, 100.0)); // TR
        assert_eq!((o[2].x, o[2].y), (950.0, 520.0)); // BR
        assert_eq!((o[3].x, o[3].y), (300.0, 500.0)); // BL
    }

    #[test]
    fn order_quad_matches_js_for_skew() {
        // Rotated-ish quad: imgproc.mjs branch (pNext.x - pTL.x > pPrev.x - pTL.x)
        // picks the CW order; the CCW branch is the mirror. Pinned against the
        // Node source by transcribing the comparator, so exercise both arms.
        let cw = [pt(10.0, 10.0), pt(100.0, 20.0), pt(90.0, 110.0), pt(5.0, 95.0)];
        let o = order_quad(cw);
        assert_eq!((o[0].x, o[0].y), (10.0, 10.0));
        assert!(o[1].x > o[3].x, "TR right of BL");
        let ccw = [pt(10.0, 10.0), pt(105.0, 95.0), pt(90.0, 110.0), pt(100.0, 20.0)];
        let o2 = order_quad(ccw);
        assert_eq!((o2[0].x, o2[0].y), (10.0, 10.0));
    }

    #[test]
    fn perspective_transform_identity_and_singular() {
        let s = [pt(0.0, 0.0), pt(1.0, 0.0), pt(1.0, 1.0), pt(0.0, 1.0)];
        let m = perspective_transform(&s, &s).unwrap();
        // H maps corners to themselves: [1,0,0, 0,1,0, 0,0] up to scale.
        assert!((m[0] - 1.0).abs() < 1e-9 && (m[4] - 1.0).abs() < 1e-9);
        assert!(m[6].abs() < 1e-9);
        // Degenerate quad (all same point) => null (Node returns null => fill).
        let d = [pt(1.0, 1.0); 4];
        assert!(perspective_transform(&d, &s).is_none());
    }

    #[test]
    fn node_base64_skips_invalid_and_stops_at_padding() {
        // Buffer.from('!!aGVsbG8=', 'base64') === Buffer.from('aGVsbG8') — the
        // '!'s are skipped, decoding STOPS at the first '='.
        assert_eq!(node_base64_decode("!!aGVsbG8="), b"hello".to_vec());
        assert_eq!(node_base64_decode("aGVsbG8="), b"hello".to_vec());
        assert_eq!(node_base64_decode("aGVsbG8"), b"hello".to_vec());
        // 2 valid chars => 1 byte; lone char dropped, exactly like Node.
        assert_eq!(node_base64_decode("aG="), b"h".to_vec());
        assert_eq!(node_base64_decode("a"), Vec::<u8>::new());
        // whitespace/newlines skipped (Node's whitespace tolerance).
        assert_eq!(node_base64_decode("aGVs\nbG8="), b"hello".to_vec());
    }

    #[test]
    fn search_top_k_float32_sims_and_insert_order() {
        // 2-dim toy catalog: rows along +x then noise; k=2.
        let emb = [1.0f32, 0.0];
        let cat = vec![
            0.9, 0.0, // 0.9
            1.0, 0.0, // 1.0
            0.0, 1.0, // 0.0
            0.95, 0.0, // 0.95
        ];
        let top = search_top_k(&emb, &cat, 4, 2, 2, None);
        assert_eq!(top[0].0, 1);
        assert_eq!(top[1].0, 3);
        // allow-filter rows get -inf and never enter.
        let allow = |i: usize| i != 1;
        let top2 = search_top_k(&emb, &cat, 4, 2, 2, Some(&allow));
        assert_eq!(top2[0].0, 3);
        assert_eq!(top2[1].0, 0);
    }

    #[test]
    fn lanczos3_matches_vips_kernel() {
        // vips filter: sinc(x)*sinc(x/3) on [-3,3], exactly 0 at nonzero
        // integers inside the support, 1 at 0, 0 outside.
        assert_eq!(lanczos3_filter(0.0), 1.0);
        for k in 1..=3 {
            assert!(lanczos3_filter(k as f64).abs() < 1e-12);
            assert!(lanczos3_filter(-(k as f64)).abs() < 1e-12);
        }
        assert_eq!(lanczos3_filter(3.5), 0.0);
    }

    #[test]
    fn reduce_axis_quantises_truncated_and_normalised() {
        let ax = reduce_axis_build(1000, 2.0);
        assert_eq!(ax.out, 500);
        assert_eq!(ax.n, reduce_points(2.0));
        assert_eq!(ax.n, 2 * 6 + 1); // 2*rint(3*2)+1 = 13 taps at shrink 2
        // Every table sums to ~4096 in the (truncated) fixed point.
        for tx in 0..=TRANSFORM_SCALE {
            let sum: i32 = ax.matrixs[tx as usize * ax.n..(tx as usize + 1) * ax.n]
                .iter()
                .map(|v| *v as i32)
                .sum();
            assert!((sum - 4096).abs() <= ax.n as i32, "tx {tx} sum {sum}");
        }
    }


    #[test]
    fn gap_floor_js_number_semantics() {
        // Number('')===0 but cvScan uses `|| 0.10` after Number() on an
        // empty env string: we treat empty/absent as the 0.10 default.
        unsafe { std::env::set_var("CV_SCAN_GAP", "") };
        assert_eq!(gap_floor(), GAP_FLOOR_DEFAULT);
        unsafe { std::env::set_var("CV_SCAN_GAP", "0.12") };
        assert_eq!(gap_floor(), 0.12);
        unsafe { std::env::set_var("CV_SCAN_GAP", "garbage") };
        assert_eq!(gap_floor(), GAP_FLOOR_DEFAULT);
        unsafe { std::env::remove_var("CV_SCAN_GAP") };
    }

    #[test]
    fn parse_set_list_js_coercion() {
        assert_eq!(parse_set_list(None).unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_set_list(Some(&serde_json::json!(" who , zho,,"))).unwrap(),
            vec!["who".to_string(), "zho".to_string()]
        );
        // (5||'').split throws upstream; the route answers 500.
        assert!(parse_set_list(Some(&serde_json::json!(5))).is_err());
        assert!(parse_set_list(Some(&serde_json::json!(["who"]))).is_err());
        assert_eq!(parse_set_list(Some(&Value::Null)).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn truthy_js_table() {
        assert!(truthy(Some(&serde_json::json!([])))); // [] truthy
        assert!(truthy(Some(&serde_json::json!({}))));
        assert!(!truthy(Some(&serde_json::json!(""))));
        assert!(!truthy(Some(&serde_json::json!(0))));
        assert!(!truthy(Some(&Value::Null)));
        assert!(truthy(Some(&serde_json::json!("x"))));
    }

    // -----------------------------------------------------------------------
    // Recognition parity vs Node ground truth (unit-6b readiness criterion:
    // "at least as good as Node" on the eval corpus). node-truth.json is the
    // frozen :3312 capture of POST /api/scan-match over server/bench/
    // scan-eval/corpus (regenerable with mkcorpus.py + mkvariants.py +
    // node-scan-match.mjs); the corpus itself lives in the lane's data dir
    // (../../data/scan-eval-corpus, CORPUS_DIR overrides). Requires the lane
    // artifacts (ORT dylib + models) — skipped, never failed, when absent.
    // -----------------------------------------------------------------------

    const ABS_SCORE: f64 = 5e-4;
    const ABS_SHARP: f64 = 5e-3;

    fn lane_artifacts() -> Option<(PathBuf, PathBuf, PathBuf)> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
        let dylib = std::env::var_os("ORT_DYLIB_PATH")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                root.join("../bindarr/backend/node_modules/onnxruntime-node/bin/napi-v6/linux/x64/libonnxruntime.so.1")
            });
        let models = std::env::var_os("CV_MODEL_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("data/models"));
        let corpus = std::env::var_os("CORPUS_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("data/scan-eval-corpus"));
        if dylib.exists()
            && models.join("cornelius.onnx").exists()
            && models.join("milo.onnx").exists()
            && models.join("milo-mtg.npz").exists()
            && corpus.exists()
        {
            Some((dylib, models, corpus))
        } else {
            None
        }
    }

    #[test]
    fn node_truth_parity_on_eval_corpus() {
        let Some((dylib, models, corpus)) = lane_artifacts() else {
            eprintln!("SKIP node_truth_parity_on_eval_corpus: lane artifacts absent");
            return;
        };
        if std::env::var("ORT_DYLIB_PATH").unwrap_or_default().is_empty() {
            unsafe { std::env::set_var("ORT_DYLIB_PATH", &dylib) };
        }
        if std::env::var("CV_MODEL_DIR").unwrap_or_default().is_empty() {
            unsafe { std::env::set_var("CV_MODEL_DIR", &models) };
        }
        if std::env::var("SCRYBOX_JPEG_LIB").unwrap_or_default().is_empty() {
            let shim = models.parent().map(|p| p.join("lib/libscryjpeg.so"));
            if shim.as_ref().is_some_and(|p| p.exists()) {
                unsafe { std::env::set_var("SCRYBOX_JPEG_LIB", shim.unwrap()) };
            }
        }
        let truth: Value =
            serde_json::from_str(include_str!("../../../bench/scan-eval/node-truth.json"))
                .expect("node-truth.json");
        // rowSets() reads card_cache (set_id per catalog row), so the truth
        // capture's candidate set is DB-dependent: run against the same lane
        // DB node-truth.json was captured from, not an empty temp file.
        let lane_db = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../data/bindarr.db");
        if !lane_db.exists() {
            eprintln!("SKIP node_truth_parity_on_eval_corpus: lane DB absent");
            return;
        }
        let pool = crate::db::DbPool::open(lane_db, 2).unwrap();

        let cases = truth.as_object().unwrap();
        assert!(!cases.is_empty());
        for (file, exp) in cases {
            let ej = &exp["json"];
            let bytes = std::fs::read(corpus.join(file)).expect("corpus image");
            // Some("English") mirrors the route: Node's route always hands
            // cvScan languages.toName(undefined) = 'English', never undefined
            // (undefined would sweep every built language's catalog).
            let out = match_scan(&bytes, "mtg", TOP_K, &[], Some("English"), false, &pool)
                .unwrap_or_else(|e| panic!("{file}: match_scan: {}", e.0));
            let o = out.as_object().unwrap();
            if std::env::var("SCAN_DEBUG").is_ok() {
                let crop_dbg = o["crop"].as_str().unwrap();
                let cb_dbg = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &crop_dbg["data:image/jpeg;base64,".len()..]).unwrap();
                std::fs::write(format!("/tmp/rust-crop-{file}.jpg"), &cb_dbg).unwrap();
                eprintln!("{file} RUST margin={:?} gap={:?} cands={:?}",
                    o["margin"], o["gap"],
                    o["candidates"].as_array().unwrap().iter().map(|c| (c["cardId"].as_str().unwrap().to_string(), c["score"].as_f64().unwrap())).collect::<Vec<_>>());
                eprintln!("{file} NODE margin={:?} gap={:?} cands={:?}",
                    ej["margin"], ej["gap"],
                    ej["candidates"].as_array().unwrap().iter().map(|c| (c["cardId"].as_str().unwrap().to_string(), c["score"].as_f64().unwrap())).collect::<Vec<_>>());
            }
            // Key order shipped to the client, exactly as Node's res.json.
            let ka: Vec<&str> = o.keys().map(String::as_str).collect();
            let kb: Vec<&str> = ej.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(ka, kb, "{file}: key order");
            for k in ["game", "verified", "lang", "engine", "detected", "notInCatalog"] {
                assert_eq!(o[k], ej[k], "{file}: {k}");
            }
            assert_eq!(
                o["catalogs"].as_array().unwrap().iter().map(Value::as_str).collect::<Vec<_>>(),
                ej["catalogs"].as_array().unwrap().iter().map(Value::as_str).collect::<Vec<_>>(),
                "{file}: catalogs"
            );
            // crop: valid jpeg data-url (encoder bytes diverge by design).
            let crop = o["crop"].as_str().unwrap();
            assert!(crop.starts_with("data:image/jpeg;base64,"), "{file}: crop prefix");
            let cb = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &crop["data:image/jpeg;base64,".len()..]).unwrap();
            assert_eq!((&cb[0], &cb[1]), (&0xFFu8, &0xD8u8), "{file}: crop SOI");
            // Numbers: Float32 sims tolerate ≤1 decoder LSB; gates must agree.
            let near = |a: f64, b: f64, eps: f64, what: &str| {
                assert!((a - b).abs() <= eps, "{file}: {what} {a} vs {b} (d={})", a - b);
            };
            near(o["sharpness"].as_f64().unwrap(), ej["sharpness"].as_f64().unwrap(), ABS_SHARP, "sharpness");
            near(o["margin"].as_f64().unwrap(), ej["margin"].as_f64().unwrap(), ABS_SCORE, "margin");
            match (o["gap"].as_f64(), ej["gap"].as_f64()) {
                (None, None) => {}
                (Some(a), Some(b)) => near(a, b, ABS_SCORE, "gap"),
                (a, b) => panic!("{file}: gap null mismatch {a:?} vs {b:?}"),
            }
            let ca = o["candidates"].as_array().unwrap();
            let cb_ = ej["candidates"].as_array().unwrap();
            // match_scan ships PRE-hydration candidates (3 keys); the route
            // adds {name,set,number,card} from card_cache.
            assert_eq!(ca.len(), cb_.len().min(TOP_K), "{file}: cand count");
            for (i, got) in ca.iter().enumerate() {
                let g = got.as_object().unwrap();
                let t = &cb_[i];
                assert_eq!(g["cardId"], t["cardId"], "{file}: cand[{i}].cardId");
                assert_eq!(g["catalogLang"], t["catalogLang"], "{file}: cand[{i}].catalogLang");
                near(g["score"].as_f64().unwrap(), t["score"].as_f64().unwrap(), ABS_SCORE, &format!("cand[{i}].score"));
            }
        }
    }
}
