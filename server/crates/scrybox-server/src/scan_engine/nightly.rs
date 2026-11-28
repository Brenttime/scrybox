//! TOTORO nightly — the in-process port of the host's
//! `scan-index-refresh.timer` (`~/projects/bindarr-host/scan-index-refresh.py`)
//! plus cardscan `tools/build_client_index.py`.
//!
//! One pass ([`run_blocking`]):
//!  1. rebuild the cardscan `oracle_titles` + printing indexes from fresh
//!     Scryfall bulk (`oracle_cards` download, `default_cards` = the
//!     in-process local-scryfall file the 05:00 refresh just landed) into a
//!     staging dir — the live files and the live engine are untouched;
//!  2. skip (`no new cards`) unless titles or printings grew, or the
//!     published phone index lags the printings (the script's `stale`);
//!     refuse when the rebuild is SMALLER than the live data;
//!  3. load the candidate engine from staging (a load failure keeps the old
//!     files and engine), swap the files in (old ones kept in `.prev/`,
//!     rolled back on a mid-swap error) and hot-swap the engine;
//!  4. build the client-scan index exactly like build_client_index.py
//!     (byte-identical JSON + gzip) and publish it: never smaller than the
//!     live one, recogniser/dict must match, `manifest.json.bak-<stamp>`
//!     backup, prune all but the live + previous index.
//!
//! Config: `SCRYBOX_SCAN_INDEX_REFRESH` (`on` default / `off`),
//! `SCRYBOX_SCAN_INDEX_REFRESH_AT` (`after-lsf` = right after a successful
//! local-scryfall daily refresh — the default when that refresh runs — or
//! `HH:MM` for its own daily schedule, default `05:30` without lsf),
//! `SCRYBOX_SCAN_INDEX_REFRESH_ON_BOOT=1` (one pass after boot),
//! `SCRYBOX_SCAN_INDEX_ORACLE_SRC` / `_PRINTING_SRC` (bulk file overrides,
//! offline). Status: `GET /api/admin/models/scan-index`; manual pass:
//! `POST /api/admin/models/scan-index/refresh {"force":true}`.
//! Rollback a publish: `cp manifest.json.bak-<stamp> manifest.json` in the
//! client-scan dir; the previous cardscan indexes sit in `<data>/.prev/`.

use super::printings::Printings;
use crate::scan_ocr::names::{norm_name, Oracle};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

pub const INDEX_FILES: [&str; 5] =
    ["oracle_index.json", "oracle_index.meta.json", "oracle_titles.json", "printing_index.json", "printing_index.meta.json"];

// ---------------------------------------------------------------------------
// build_client_index.py
// ---------------------------------------------------------------------------

/// `_card_meta()` row: (frame, promo_types, full_art, len(finishes)).
pub type CardMeta = HashMap<Option<String>, (Option<String>, Vec<String>, bool, usize)>;

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `_card_meta()` over a default-cards jsonl(.gz). Missing file → empty
/// (the script warns and builds flags without frame/promo_types).
///
/// # Errors
///
/// Read/parse failure of the default-cards file (Python `repr` parity).
pub fn load_card_meta(path: &Path) -> Result<CardMeta, String> {
    let mut out = CardMeta::new();
    if !path.exists() {
        tracing::warn!("client-scan index: {} missing; printing flags without frame/promo_types", path.display());
        return Ok(out);
    }
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let rd: Box<dyn BufRead> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(std::io::BufReader::with_capacity(1 << 20, flate2::read::MultiGzDecoder::new(f)))
    } else {
        Box::new(std::io::BufReader::with_capacity(1 << 20, f))
    };
    for line in rd.lines() {
        let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
        let line = line.trim().trim_end_matches(',');
        if !line.starts_with('{') {
            continue;
        }
        let c: Value = serde_json::from_str(line).map_err(|e| format!("{}: {e}", path.display()))?;
        let null = Value::Null;
        let g = |k: &str| c.get(k).unwrap_or(&null);
        let promo: Vec<String> = match g("promo_types") {
            Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
            _ => vec![],
        };
        let finishes = match g("finishes") {
            Value::Array(a) if !a.is_empty() => a.len(),
            Value::String(s) if !s.is_empty() => s.chars().count(),
            _ => 2,
        };
        out.insert(g("id").as_str().map(str::to_string), (g("frame").as_str().map(str::to_string), promo, truthy(g("full_art")), finishes));
    }
    Ok(out)
}

const VARIANT_FX: [&str; 5] = ["showcase", "extendedart", "inverted", "etched", "fullart"];
const PROMO_SET: [&str; 15] = [
    "promo", "secret lair", "the list", "mystery booster", "playtest", "judge", "prerelease", "league", "arena",
    "championship", "store", "play network", "magicfest", "gift", "bundle",
];
const OLD_FRAMES: [&str; 3] = ["1993", "1997", "2003"];

/// `_released(p)`.
fn released(v: &Value) -> i64 {
    let s = match v {
        Value::String(s) => s.replace('-', ""),
        _ => return 0,
    };
    if s.chars().count() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        s.parse().unwrap_or(0)
    } else {
        0
    }
}

/// `re.fullmatch(r"ff[ivx]+|fftactics|ffcc", t)`.
fn ff_tag(t: &str) -> bool {
    t == "fftactics" || t == "ffcc" || (t.len() > 2 && t.starts_with("ff") && t[2..].chars().all(|c| matches!(c, 'i' | 'v' | 'x')))
}

/// `_treatment_tags(promo_types)` non-empty.
fn has_treatment(promo: &[String]) -> bool {
    promo.iter().any(|t| t != "universesbeyond" && t != "sourcematerial" && !ff_tag(t))
}

#[derive(Default)]
struct PrintingCols {
    released_at: Value,
    frame_effects: Value,
    border_color: Value,
    set_name: Value,
    set: Value,
}

impl PrintingCols {
    fn parse(raw: &str) -> Result<PrintingCols, String> {
        let mut m: serde_json::Map<String, Value> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        let mut t = |k: &str| m.remove(k).unwrap_or(Value::Null);
        Ok(PrintingCols { released_at: t("released_at"), frame_effects: t("frame_effects"), border_color: t("border_color"), set_name: t("set_name"), set: t("set") })
    }
}

/// `_variant_flags(p)`.
fn variant_flags(p: &PrintingCols, id: &Option<String>, meta: &CardMeta) -> i64 {
    let fx_hit = match &p.frame_effects {
        Value::Array(a) => a.iter().any(|x| x.as_str().is_some_and(|s| VARIANT_FX.contains(&s))),
        _ => false,
    };
    let (frame, promo, full_art, finishes) = match meta.get(id) {
        Some((f, pr, fa, fi)) => (f.as_deref(), pr.as_slice(), *fa, *fi),
        None => (None, &[][..], false, 2),
    };
    let mut flags = 0;
    if fx_hit || p.border_color.as_str() == Some("borderless") || full_art || has_treatment(promo) {
        flags |= 1;
    }
    if frame.is_some_and(|f| OLD_FRAMES.contains(&f)) {
        flags |= 4;
    }
    if finishes == 1 {
        flags |= 8;
    }
    let name = match &p.set_name {
        Value::String(s) => s.to_lowercase(),
        v if !truthy(v) => String::new(),
        v => v.to_string().to_lowercase(),
    };
    let code = p.set.as_str().unwrap_or("");
    if PROMO_SET.iter().any(|w| name.contains(w)) || (code.chars().count() == 4 && code.starts_with('p')) {
        flags |= 2;
    }
    flags
}

fn js(out: &mut String, s: &str) {
    // serde_json escapes exactly like json.dumps(ensure_ascii=False):
    // `"`, `\` and C0 controls only.
    // INFALLIBLE: serialising a &str to JSON cannot fail.
    out.push_str(&serde_json::to_string(s).expect("string json"));
}

fn js_opt(out: &mut String, s: &Option<String>) {
    match s {
        Some(s) => js(out, s),
        None => out.push_str("null"),
    }
}

/// One built client index (the gzipped bytes + counts).
pub struct ClientIndex {
    pub name: String,
    pub gz: Vec<u8>,
    pub names: usize,
    pub printings: usize,
}

/// `build_index()` + `json.dumps(..., separators=(",", ":"), ensure_ascii=False)`.
///
/// # Errors
///
/// Index assembly failures (only structural ones; row building is total).
pub fn build_index_json(oracle: &Oracle, printings: &Printings, meta: &CardMeta) -> Result<(Vec<u8>, usize, usize), String> {
    let names = &oracle.norm_list;
    let mut pid: HashMap<Option<String>, usize> = HashMap::new();
    let mut rows: Vec<String> = Vec::new();
    let mut pidx = |i: u32| -> Result<usize, String> {
        let p = &printings.list[i as usize];
        if let Some(&n) = pid.get(&p.id) {
            return Ok(n);
        }
        let cols = PrintingCols::parse(&printings.raw(i))?;
        let mut r = String::with_capacity(96);
        r.push('[');
        js_opt(&mut r, &p.id);
        r.push(',');
        match &cols.set {
            Value::Null => r.push_str("null"),
            v => r.push_str(&v.to_string()),
        }
        r.push(',');
        js(&mut r, p.num.as_deref().unwrap_or(""));
        r.push_str(&format!(",{},{}]", released(&cols.released_at), variant_flags(&cols, &p.id, meta)));
        pid.insert(p.id.clone(), rows.len());
        rows.push(r);
        Ok(rows.len() - 1)
    };
    let mut by_title = String::with_capacity(8 << 20);
    by_title.push('{');
    for (k, (title, ids)) in printings.by_title.iter().enumerate() {
        if k > 0 {
            by_title.push(',');
        }
        js(&mut by_title, title);
        by_title.push_str(":[");
        for (j, &i) in ids.iter().enumerate() {
            if j > 0 {
                by_title.push(',');
            }
            by_title.push_str(&pidx(i)?.to_string());
        }
        by_title.push(']');
    }
    by_title.push('}');
    let mut ua = String::with_capacity(1 << 20);
    ua.push('{');
    for (k, (c, lst)) in printings.unique_aliases.iter().enumerate() {
        if k > 0 {
            ua.push(',');
        }
        js(&mut ua, c);
        ua.push_str(":[");
        for (j, (a, i)) in lst.iter().enumerate() {
            if j > 0 {
                ua.push(',');
            }
            ua.push('[');
            js(&mut ua, a);
            ua.push(',');
            ua.push_str(&pidx(*i)?.to_string());
            ua.push(']');
        }
        ua.push(']');
    }
    ua.push('}');

    let mut o = String::with_capacity(16 << 20);
    o.push_str("{\"v\":1,\"names\":[");
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        js(&mut o, n);
    }
    o.push_str("],\"canon\":{");
    let mut first = true;
    for (i, n) in names.iter().enumerate() {
        let c = norm_name(oracle.norm_map.get(n).map(String::as_str).unwrap_or(""));
        if c != *n {
            if !first {
                o.push(',');
            }
            first = false;
            o.push_str(&format!("\"{i}\":"));
            js(&mut o, &c);
        }
    }
    o.push_str("},\"excluded\":[");
    let mut ex: Vec<&String> = oracle.excluded.iter().collect();
    ex.sort();
    for (i, n) in ex.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        js(&mut o, n);
    }
    o.push_str("],\"sets\":[");
    for (i, n) in printings.set_codes.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        js(&mut o, n);
    }
    o.push_str("],\"printings\":[");
    o.push_str(&rows.join(","));
    o.push_str("],\"byTitle\":");
    o.push_str(&by_title);
    o.push_str(",\"uniqueAlias\":");
    o.push_str(&ua);
    o.push('}');
    Ok((o.into_bytes(), names.len(), rows.len()))
}

/// CPython 3.12 `gzip.compress(raw, 9, mtime=0)` = `zlib.compress(raw, 9,
/// wbits=31)` on stock zlib (header `1f8b 08 00 00000000 02 03`). zlib-rs
/// (flate2's backend) emits a different, valid deflate stream, so this goes
/// through libz-sys' bundled stock zlib to stay byte-identical.
pub fn gzip_py(raw: &[u8]) -> Vec<u8> {
    use libz_sys as z;
    let mut out: Vec<u8> = Vec::with_capacity(raw.len() / 3 + 1024);
    // SAFETY: `s` is a stack z_stream initialised by deflateInit2_ (rc checked)
    // and ended exactly once; next_in/avail_in describe the live `raw` slice;
    // next_out/avail_out describe the uninitialised spare capacity of `out`,
    // and set_len only covers the `produced` bytes zlib reports writing.
    unsafe {
        extern "C" fn zalloc(_: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
            // SAFETY: plain libc allocation; zlib frees the result via zfree.
            unsafe { libc::calloc(items as usize, size as usize) }
        }
        extern "C" fn zfree(_: z::voidpf, p: z::voidpf) {
            // SAFETY: zlib only passes pointers previously returned by zalloc (calloc).
            unsafe { libc::free(p) }
        }
        let mut s = z::z_stream {
            next_in: std::ptr::null_mut(),
            avail_in: 0,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            zalloc,
            zfree,
            opaque: std::ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        };
        let rc = z::deflateInit2_(
            &mut s,
            9,
            z::Z_DEFLATED,
            31,
            8,
            z::Z_DEFAULT_STRATEGY,
            z::zlibVersion(),
            std::mem::size_of::<z::z_stream>() as i32,
        );
        assert_eq!(rc, z::Z_OK, "deflateInit2");
        s.next_in = raw.as_ptr() as *mut u8;
        s.avail_in = raw.len() as u32;
        loop {
            if out.capacity() - out.len() < 64 * 1024 {
                out.reserve(1 << 20);
            }
            let spare = out.capacity() - out.len();
            s.next_out = out.as_mut_ptr().add(out.len());
            s.avail_out = spare as u32;
            let rc = z::deflate(&mut s, z::Z_FINISH);
            let produced = spare - s.avail_out as usize;
            out.set_len(out.len() + produced);
            if rc == z::Z_STREAM_END {
                break;
            }
            assert!(rc == z::Z_OK || rc == z::Z_BUF_ERROR, "deflate rc {rc}");
        }
        z::deflateEnd(&mut s);
    }
    out
}

fn digest12(data: &[u8]) -> String {
    let h = Sha256::digest(data);
    h.iter().map(|b| format!("{b:02x}")).collect::<String>()[..12].to_string()
}

pub fn build_client_index(oracle: &Oracle, printings: &Printings, meta: &CardMeta) -> Result<ClientIndex, String> {
    let (raw, names, n_printings) = build_index_json(oracle, printings, meta)?;
    let gz = gzip_py(&raw);
    Ok(ClientIndex { name: format!("scan-index.{}.json.gz", digest12(&gz)), gz, names, printings: n_printings })
}

/// The recogniser files the manifest names (`<stem>.<hash>.onnx`,
/// `rec-dict.<hash>.txt`).
pub struct RecAssets {
    pub model: Vec<u8>,
    pub model_name: String,
    pub dict: Vec<u8>,
    pub dict_name: String,
}

impl RecAssets {
    pub fn new(model_path: &Path, dict_raw: &str) -> Result<RecAssets, String> {
        let model = std::fs::read(model_path).map_err(|e| format!("{}: {e}", model_path.display()))?;
        let stem = model_path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let dict = dict_raw.as_bytes().to_vec();
        Ok(RecAssets { model_name: format!("{stem}.{}.onnx", digest12(&model)), dict_name: format!("rec-dict.{}.txt", digest12(&dict)), model, dict })
    }

    /// The image's recogniser (`SCRYBOX_OCR_MODEL_DIR/PP-OCRv6_rec_small.onnx`).
    pub fn from_env() -> Result<RecAssets, String> {
        let p = crate::scan_ocr::rec::model_dir().ok_or("no OCR model dir")?.join(crate::scan_ocr::rec::REC_MODEL_FILE);
        let d = crate::scan_ocr::rec::model_character(&p)?;
        RecAssets::new(&p, &d)
    }
}

/// `json.dump(manifest, fh, indent=1)`.
pub fn manifest_json(index: &str, rec: &str, dict: &str, bytes: u64) -> String {
    // INFALLIBLE: serialising a &str to JSON cannot fail.
    let q = |s: &str| serde_json::to_string(s).expect("string serialises");
    format!("{{\n \"v\": 1,\n \"index\": {},\n \"rec\": {},\n \"dict\": {},\n \"bytes\": {bytes}\n}}", q(index), q(rec), q(dict))
}

/// `_hashed(out_dir, stem, ext, data)`: drop same-stem files, write `name`.
fn write_hashed(dir: &Path, name: &str, stem: &str, ext: &str, data: &[u8]) -> Result<(), String> {
    for e in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let f = e.file_name().to_string_lossy().to_string();
        if f.starts_with(&format!("{stem}.")) && f.ends_with(ext) && f != name {
            std::fs::remove_file(e.path()).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(dir.join(name), data).map_err(|e| e.to_string())
}

/// build_client_index.py `main()` into `out_dir` (the CLI twin used by the
/// differential): index + model + dict + manifest.json.
pub fn write_out_dir(out_dir: &Path, idx: &ClientIndex, rec: &RecAssets, model_stem: &str) -> Result<String, String> {
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    write_hashed(out_dir, &idx.name, "scan-index", ".json.gz", &idx.gz)?;
    write_hashed(out_dir, &rec.dict_name, "rec-dict", ".txt", &rec.dict)?;
    write_hashed(out_dir, &rec.model_name, model_stem, ".onnx", &rec.model)?;
    let bytes = (idx.gz.len() + rec.model.len() + rec.dict.len()) as u64;
    let m = manifest_json(&idx.name, &rec.model_name, &rec.dict_name, bytes);
    std::fs::write(out_dir.join("manifest.json"), &m).map_err(|e| e.to_string())?;
    Ok(m)
}

// ---------------------------------------------------------------------------
// scan-index-refresh.py
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Manifest {
    pub index: String,
    pub rec: String,
    pub dict: String,
}

/// The published manifest + its index's (names, printings) counts.
///
/// # Errors
///
/// `manifest.json` read/parse failure (path-prefixed).
pub fn live_client(scan_dir: &Path) -> Result<Option<(Manifest, usize, usize)>, String> {
    let mp = scan_dir.join("manifest.json");
    let Ok(b) = std::fs::read(&mp) else { return Ok(None) };
    let v: Value = serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", mp.display()))?;
    let f = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string).ok_or_else(|| format!("{}: no {k}", mp.display()));
    let m = Manifest { index: f("index")?, rec: f("rec")?, dict: f("dict")? };
    let ip = scan_dir.join(&m.index);
    let f = std::fs::File::open(&ip).map_err(|e| format!("{}: {e}", ip.display()))?;
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(f).read_to_end(&mut raw).map_err(|e| format!("{}: {e}", ip.display()))?;
    let ix: HashMap<String, &RawValue> = serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", ip.display()))?;
    let len = |k: &str| ix.get(k).and_then(|r| serde_json::from_str::<Vec<&RawValue>>(r.get()).ok()).map(|v| v.len()).unwrap_or(0);
    Ok(Some((m, len("names"), len("printings"))))
}

/// `counts()`: (titles, printings) the data dir's indexes hold.
pub fn counts(dir: &Path) -> (usize, usize) {
    let t = std::fs::read(dir.join("oracle_titles.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<&RawValue>>(&b).ok().map(|v| v.len()))
        .unwrap_or(0);
    let p = std::fs::read(dir.join("printing_index.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<HashMap<String, &RawValue>>(&b).ok().map(|v| v.len()))
        .unwrap_or(0);
    (t, p)
}

pub struct RunCfg {
    pub data_dir: PathBuf,
    pub scan_dir: PathBuf,
    pub oracle_src: PathBuf,
    pub printing_src: PathBuf,
    /// The recogniser files (compared with / written beside the live ones).
    /// `None` reuses the live manifest's rec/dict (tests).
    pub rec: Option<RecAssets>,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Nothing added (sidecar + published index current).
    Skipped,
    /// Indexes swapped; the rebuilt client index hashes like the live one.
    Unchanged,
    Published { index: String, backup: Option<String> },
}

#[derive(Debug, Clone)]
pub struct Report {
    pub outcome: Outcome,
    pub before: (usize, usize),
    pub after: (usize, usize),
    pub client_before: Option<(String, usize, usize)>,
    pub client_after: Option<(String, usize, usize)>,
    pub engine_swapped: bool,
}

fn rm_rf(p: &Path) {
    let _ = std::fs::remove_dir_all(p);
}

/// Move the staged index files into `data_dir`, the current ones into
/// `data_dir/.prev/`; any error restores the previous set.
///
/// # Errors
///
/// Any create/copy/rename failure during the swap.
pub fn swap_files(data_dir: &Path, staging: &Path) -> Result<(), String> {
    let prev = data_dir.join(".prev");
    rm_rf(&prev);
    std::fs::create_dir_all(&prev).map_err(|e| e.to_string())?;
    let mut moved_old: Vec<&str> = Vec::new();
    let mut placed: Vec<&str> = Vec::new();
    let r = (|| -> Result<(), String> {
        for f in INDEX_FILES {
            if !staging.join(f).exists() {
                return Err(format!("staged {f} missing"));
            }
        }
        for f in INDEX_FILES {
            if data_dir.join(f).exists() {
                std::fs::rename(data_dir.join(f), prev.join(f)).map_err(|e| format!("backup {f}: {e}"))?;
                moved_old.push(f);
            }
            fail_point(f)?;
            std::fs::rename(staging.join(f), data_dir.join(f)).map_err(|e| format!("install {f}: {e}"))?;
            placed.push(f);
        }
        Ok(())
    })();
    if let Err(e) = r {
        for f in placed {
            let _ = std::fs::remove_file(data_dir.join(f));
        }
        for f in moved_old {
            let _ = std::fs::rename(prev.join(f), data_dir.join(f));
        }
        return Err(format!("index swap failed, previous indexes restored: {e}"));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub static FAIL_AT: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn fail_point(f: &str) -> Result<(), String> {
    FAIL_AT.with(|x| if *x.borrow() == Some(f) { Err(format!("injected failure at {f}")) } else { Ok(()) })
}

#[cfg(not(test))]
fn fail_point(_: &str) -> Result<(), String> {
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| format!("{}: no file name", path.display()))?;
    let tmp = dir.join(format!(".{}.{}.tmp", name.to_string_lossy(), std::process::id()));
    let r = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.map_err(|e| format!("{}: {e}", path.display()))
}

/// Publish `idx` into the live client-scan dir under the script's rules.
///
/// # Errors
///
/// Any file copy/write/rename failure (path-prefixed).
pub fn publish(scan_dir: &Path, idx: &ClientIndex, rec: Option<&RecAssets>, live: Option<&(Manifest, usize, usize)>) -> Result<Outcome, String> {
    std::fs::create_dir_all(scan_dir).map_err(|e| e.to_string())?;
    if let Some((m, names, prints)) = live {
        if m.index == idx.name {
            return Ok(Outcome::Unchanged);
        }
        if idx.names < *names || idx.printings < *prints {
            return Err(format!("REFUSED: new index smaller ({}/{} vs {names}/{prints})", idx.names, idx.printings));
        }
        if let Some(r) = rec {
            if r.model_name != m.rec || r.dict_name != m.dict {
                return Err("REFUSED: recognizer/dict changed; publish by hand".into());
            }
        }
    }
    let (rec_name, dict_name) = match (live, rec) {
        (Some((m, _, _)), _) => (m.rec.clone(), m.dict.clone()),
        (None, Some(r)) => {
            // First publish (fresh volume): ship the recogniser files too.
            for (n, b) in [(&r.model_name, &r.model), (&r.dict_name, &r.dict)] {
                if !scan_dir.join(n).exists() {
                    atomic_write(&scan_dir.join(n), b)?;
                }
            }
            (r.model_name.clone(), r.dict_name.clone())
        }
        (None, None) => return Err("no live manifest and no recogniser to publish with".into()),
    };
    for n in [&rec_name, &dict_name] {
        if !scan_dir.join(n).exists() {
            return Err(format!("REFUSED: {n} missing from {}", scan_dir.display()));
        }
    }
    atomic_write(&scan_dir.join(&idx.name), &idx.gz)?;
    let size = |n: &str| std::fs::metadata(scan_dir.join(n)).map(|m| m.len()).unwrap_or(0);
    let bytes = size(&idx.name) + size(&rec_name) + size(&dict_name);
    let mut backup = None;
    let man = scan_dir.join("manifest.json");
    if man.exists() {
        let stamp = chrono::Local::now().format("%Y%m%dT%H%M%S").to_string();
        let b = format!("manifest.json.bak-{stamp}");
        std::fs::copy(&man, scan_dir.join(&b)).map_err(|e| format!("backup manifest: {e}"))?;
        backup = Some(b);
    }
    atomic_write(&man, manifest_json(&idx.name, &rec_name, &dict_name, bytes).as_bytes())?;
    // keep the live index + the previous one (rollback); drop older ones
    let keep: HashSet<String> = [Some(idx.name.clone()), live.map(|l| l.0.index.clone())].into_iter().flatten().collect();
    if let Ok(rd) = std::fs::read_dir(scan_dir) {
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().to_string();
            if f.starts_with("scan-index.") && !keep.contains(&f) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(Outcome::Published { index: idx.name.clone(), backup })
}

/// One refresh pass (blocking). `install` receives the freshly loaded
/// indexes after the on-disk swap (production: hot-swap the engine).
///
/// # Errors
///
/// The pipeline failure string: fetch, index build, or file IO.
pub fn run_blocking(cfg: &RunCfg, install: &dyn Fn(Oracle, Printings) -> Result<bool, String>) -> Result<Report, String> {
    let staging = cfg.data_dir.join(".nightly-staging");
    rm_rf(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let r = run_inner(cfg, &staging, install);
    rm_rf(&staging);
    r
}

fn run_inner(cfg: &RunCfg, staging: &Path, install: &dyn Fn(Oracle, Printings) -> Result<bool, String>) -> Result<Report, String> {
    let before = counts(&cfg.data_dir);
    super::jobs::build_oracle_index(&cfg.oracle_src, &staging.join("oracle_index.json"), &staging.join("oracle_index.meta.json"))
        .map_err(|e| format!("oracle index build failed (old kept): {e}"))?;
    super::jobs::build_printing_index(&cfg.printing_src, &staging.join("printing_index.json"), &staging.join("printing_index.meta.json"))
        .map_err(|e| format!("printing index build failed (old kept): {e}"))?;
    let after = counts(staging);
    tracing::info!("scan-index refresh: sidecar titles/printings {before:?} -> {after:?}");
    if (after.0 < before.0 || after.1 < before.1) && !cfg.force {
        return Err(format!("REFUSED: rebuilt cardscan indexes smaller ({after:?} vs live {before:?}); old kept"));
    }
    let live = live_client(&cfg.scan_dir).unwrap_or_else(|e| {
        tracing::warn!("scan-index refresh: live client index unreadable ({e}); treating as absent");
        None
    });
    let client_before = live.as_ref().map(|(m, n, p)| (m.index.clone(), *n, *p));
    let lagging = live.as_ref().map(|l| after.1 > l.2).unwrap_or(true);
    let stale = lagging || after.0 > before.0 || after.1 > before.1;
    if !(stale || cfg.force) {
        tracing::info!("scan-index refresh: no new cards; nothing to do");
        return Ok(Report { outcome: Outcome::Skipped, before, after, client_before: client_before.clone(), client_after: client_before, engine_swapped: false });
    }
    // Candidate engine data: a load failure here keeps files + engine.
    let (oracle, printings) = super::load_indexes(staging).map_err(|e| format!("candidate indexes failed to load (old kept): {e}"))?;
    let meta = load_card_meta(&cfg.printing_src)?;
    let idx = build_client_index(&oracle, &printings, &meta)?;
    swap_files(&cfg.data_dir, staging)?;
    // Keep the bulk beside the indexes like the sidecar does (rollback to
    // the Python sidecar sees consistent data). Best effort.
    for (src, name) in [(&cfg.oracle_src, "oracle-cards.jsonl.gz"), (&cfg.printing_src, "default-cards.jsonl.gz")] {
        let dst = cfg.data_dir.join(name);
        if src.as_path() != dst.as_path() && src.exists() {
            let tmp = cfg.data_dir.join(format!(".{name}.nightly.tmp"));
            // Only our own downloads move; the lsf bulk (or an override) is copied.
            let ours = src.starts_with(cfg.data_dir.join(".nightly-bulk"));
            let r = (if ours { std::fs::rename(src, &tmp) } else { std::fs::copy(src, &tmp).map(|_| ()) }).and_then(|_| std::fs::rename(&tmp, &dst));
            if let Err(e) = r {
                let _ = std::fs::remove_file(&tmp);
                tracing::warn!("scan-index refresh: could not place {name}: {e}");
            }
        }
    }
    let engine_swapped = match install(oracle, printings) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("scan-index refresh: engine hot-swap failed ({e}); new indexes load on next start");
            false
        }
    };
    let outcome = publish(&cfg.scan_dir, &idx, cfg.rec.as_ref(), live.as_ref())?;
    match &outcome {
        Outcome::Published { index, backup } => tracing::info!(
            "scan-index refresh: PUBLISHED {index}: names {}->{}, printings {}->{}; rollback {}",
            live.as_ref().map(|l| l.1).unwrap_or(0),
            idx.names,
            live.as_ref().map(|l| l.2).unwrap_or(0),
            idx.printings,
            backup.as_deref().unwrap_or("(first publish)")
        ),
        _ => tracing::info!("scan-index refresh: index unchanged; live already current"),
    }
    Ok(Report { outcome, before, after, client_before, client_after: Some((idx.name.clone(), idx.names, idx.printings)), engine_swapped })
}

// ---------------------------------------------------------------------------
// Scheduling + status
// ---------------------------------------------------------------------------

static STATUS: Mutex<Option<Value>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

pub fn enabled() -> bool {
    !matches!(env_nonempty("SCRYBOX_SCAN_INDEX_REFRESH").as_deref(), Some("off" | "0" | "false"))
}

fn lsf_refresh_on() -> bool {
    crate::lsf::configured_data_dir().is_some()
        && !matches!(std::env::var("SCRYBOX_LSF_REFRESH_AT").as_deref(), Ok("off" | "0" | "false" | ""))
}

/// `after-lsf` or `HH:MM`.
pub fn schedule() -> String {
    match env_nonempty("SCRYBOX_SCAN_INDEX_REFRESH_AT") {
        Some(s) if s == "after-lsf" && !lsf_refresh_on() => "05:30".into(),
        Some(s) => s,
        None if lsf_refresh_on() => "after-lsf".into(),
        None => "05:30".into(),
    }
}

/// Admin status: config + the last pass.
pub fn status_json() -> Value {
    let last = STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or(Value::Null);
    json!({"enabled": enabled(), "schedule": schedule(), "running": RUNNING.load(Ordering::SeqCst), "last": last})
}

fn set_status(v: Value) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
}

fn scan_dir() -> PathBuf {
    if let Some(d) = env_nonempty("CLIENT_SCAN_DIR") {
        return PathBuf::from(d);
    }
    env_nonempty("CV_MODEL_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/app/database/models")).join("client-scan")
}

/// Resolve the bulk sources (download when needed) into `staging_bulk`.
async fn sources(data_dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let bulk = data_dir.join(".nightly-bulk");
    std::fs::create_dir_all(&bulk).map_err(|e| e.to_string())?;
    let c = crate::lsf_data::client();
    let fetch = |ty: &'static str, file: &'static str| {
        let c = c.clone();
        let bulk = bulk.clone();
        async move {
            let url = crate::lsf_data::live_url(&c, ty).await.ok_or_else(|| format!("Scryfall did not provide a {ty} download URI"))?;
            let part = bulk.join(format!("{file}.part"));
            crate::lsf_data::download(&c, &url, &part).await?;
            if !crate::lsf_data::gzip_ok(&part) {
                let _ = std::fs::remove_file(&part);
                return Err(format!("{file}: truncated/corrupt download"));
            }
            let dst = bulk.join(file);
            std::fs::rename(&part, &dst).map_err(|e| e.to_string())?;
            Ok::<PathBuf, String>(dst)
        }
    };
    let oracle = match env_nonempty("SCRYBOX_SCAN_INDEX_ORACLE_SRC") {
        Some(p) => PathBuf::from(p),
        None => match fetch("oracle_cards", "oracle-cards.jsonl.gz").await {
            Ok(p) => p,
            Err(e) if data_dir.join("oracle-cards.jsonl.gz").exists() => {
                tracing::warn!("scan-index refresh: oracle_cards download failed ({e}); using the previous oracle-cards bulk");
                data_dir.join("oracle-cards.jsonl.gz")
            }
            Err(e) => return Err(e),
        },
    };
    let printing = match env_nonempty("SCRYBOX_SCAN_INDEX_PRINTING_SRC") {
        Some(p) => PathBuf::from(p),
        None => match crate::lsf::configured_data_dir().map(|d| d.join("default-cards.jsonl.gz")).filter(|p| p.exists()) {
            Some(p) => p,
            None => fetch("default_cards", "default-cards.jsonl.gz").await?,
        },
    };
    Ok((oracle, printing))
}

fn install_live(oracle: Oracle, printings: Printings) -> Result<bool, String> {
    if super::loaded_engine().is_none() {
        // Lazy engine never loaded: the next load reads the new files.
        return Ok(false);
    }
    let (t, n) = (oracle.norm_list.len(), printings.len());
    let e = super::Engine::from_parts(oracle, printings)?;
    super::install_engine(e);
    tracing::info!("scan-index refresh: scan engine hot-swapped ({t} titles, {n} printings; no restart)");
    Ok(true)
}

/// One full pass with production wiring (download, hot-swap, publish).
///
/// # Errors
///
/// `SCRYBOX_CARDSCAN_DATA` unset, or any `run_blocking` pipeline failure.
pub async fn run_now(force: bool, trigger: &str) -> Result<Report, String> {
    let Some(data_dir) = crate::scan_ocr::data_dir() else { return Err("SCRYBOX_CARDSCAN_DATA unset".into()) };
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("a scan-index refresh is already running".into());
    }
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            RUNNING.store(false, Ordering::SeqCst);
        }
    }
    let _clear = Clear;
    let t0 = Instant::now();
    let started = chrono::Local::now().to_rfc3339();
    tracing::info!("scan-index refresh: start (trigger={trigger}, force={force})");
    let r = async {
        let (oracle_src, printing_src) = sources(&data_dir).await?;
        let rec = match tokio::task::spawn_blocking(RecAssets::from_env).await.map_err(|e| e.to_string())? {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("scan-index refresh: recogniser unavailable ({e}); reusing the live manifest's rec/dict");
                None
            }
        };
        let cfg = RunCfg { data_dir: data_dir.clone(), scan_dir: scan_dir(), oracle_src, printing_src, rec, force };
        tokio::task::spawn_blocking(move || run_blocking(&cfg, &install_live)).await.map_err(|e| e.to_string())?
    }
    .await;
    rm_rf(&data_dir.join(".nightly-bulk"));
    let ms = t0.elapsed().as_millis() as u64;
    let status = match &r {
        Ok(rep) => {
            let (outcome, index) = match &rep.outcome {
                Outcome::Skipped => ("skipped", None),
                Outcome::Unchanged => ("unchanged", None),
                Outcome::Published { index, .. } => ("published", Some(index.clone())),
            };
            tracing::info!("scan-index refresh: done in {ms} ms — {outcome}");
            json!({"trigger": trigger, "started": started, "durationMs": ms, "ok": true, "outcome": outcome,
                   "titles": [rep.before.0, rep.after.0], "printings": [rep.before.1, rep.after.1],
                   "engineSwapped": rep.engine_swapped, "published": index,
                   "clientBefore": rep.client_before.as_ref().map(|c| json!({"index": c.0, "names": c.1, "printings": c.2})),
                   "clientAfter": rep.client_after.as_ref().map(|c| json!({"index": c.0, "names": c.1, "printings": c.2}))})
        }
        Err(e) => {
            tracing::error!("ALERT scan-index refresh failed after {ms} ms: {e}");
            json!({"trigger": trigger, "started": started, "durationMs": ms, "ok": false, "outcome": "failed", "error": e})
        }
    };
    set_status(status);
    r
}

/// lsf hook: a successful daily bulk refresh just landed.
pub fn after_lsf_refresh() {
    if !enabled() || schedule() != "after-lsf" || crate::scan_ocr::data_dir().is_none() || super::http_mode() {
        return;
    }
    tokio::spawn(async {
        let _ = run_now(false, "after-lsf").await;
    });
}

/// Boot: own daily schedule (`HH:MM`) and/or one pass on boot.
pub fn spawn_scheduler() {
    if !enabled() {
        tracing::info!("scan-index refresh: disabled (SCRYBOX_SCAN_INDEX_REFRESH=off)");
        return;
    }
    let sched = schedule();
    tracing::info!("scan-index refresh: enabled, schedule {sched}");
    if env_nonempty("SCRYBOX_SCAN_INDEX_REFRESH_ON_BOOT").is_some_and(|v| v == "1") {
        tokio::spawn(async {
            let _ = run_now(false, "boot").await;
        });
    }
    let Some((h, m)) = sched.split_once(':').and_then(|(h, m)| Some((h.parse::<u32>().ok()?.min(23), m.parse::<u32>().ok()?.min(59)))) else {
        return;
    };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(crate::lsf_data::secs_until(h, m))).await;
            let _ = run_now(false, "daily").await;
            tokio::time::sleep(std::time::Duration::from_secs(61)).await;
        }
    });
}

#[cfg(test)]
mod tests;
