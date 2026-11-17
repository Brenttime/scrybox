//! cardscan scene pipeline: `detect_uploaded_scene`, `scan_frame` (with the
//! per-session identity cache), `scan_scene_batch`, `_scan_prepared_scene`,
//! `_batch_scene_printings` + `_retro_footer_pass`, the scene-batch subset of
//! `scan_image`, and the `identify_exact_printing` fallback.

use super::detect::{self, Cand, Gray};
use super::num::{py_round, Hist};
use super::printings::{footer_numbers, normalize_collector, retro_line_number, retro_total, Printings};
use crate::scan_ocr::names::{norm_name, Oracle};
use crate::scan_ocr::pil::Rgb;
use crate::scan_ocr::{self, Title};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::Instant;

pub const AUTO_SHARP_FLOOR: f64 = 30.0;
pub const SCAN_DEADLINE_S: f64 = 0.90;
pub const MAX_IMAGE_PIXELS: usize = 50_000_000;
pub const OCR_BACKEND: &str = "rapidocr-ppocrv6";

pub type Recognize<'a> = &'a dyn Fn(&[Rgb]) -> Result<Vec<(String, f64)>, String>;

pub struct Ctx<'a> {
    pub oracle: &'a Oracle,
    pub printings: &'a Printings,
    /// `_recognize_scene_strips` minus the BGR swap (RGB strips in).
    pub rec: Recognize<'a>,
    /// `_OCR_ENGINE(img, use_det=False, ...)` (no channel conversion).
    pub rec_call: &'a dyn Fn(&Rgb) -> Result<(String, f64), String>,
    pub history: &'a std::sync::Mutex<History>,
}

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn ms(since: Instant) -> i64 {
    (since.elapsed().as_secs_f64() * 1000.0) as i64
}

fn recognize_rgb(ctx: &Ctx, strips: &[Rgb]) -> Result<Vec<(String, f64)>, String> {
    if strips.is_empty() {
        return Ok(vec![]);
    }
    let bgr: Vec<Rgb> = strips.iter().map(|s| s.swap_rb()).collect();
    (ctx.rec)(&bgr)
}

pub fn decode(raw: &[u8]) -> Result<Rgb, String> {
    // PIL's wording for the two decoder failures clients actually see
    let r = crate::cvscan::decode_rgb(raw).map_err(|e| {
        if e.contains("Not enough bytes") || e.contains("Premature end") || e.contains("truncated") {
            "Truncated File Read".to_string()
        } else if e.contains("could not be determined") {
            "cannot identify image file".to_string()
        } else {
            e
        }
    })?;
    if r.w * r.h > MAX_IMAGE_PIXELS {
        return Err("image exceeds 50 megapixels".into());
    }
    Ok(Rgb { w: r.w, h: r.h, px: r.px })
}

fn crop(img: &Rgb, x: i64, y: i64, w: i64, h: i64) -> Rgb {
    img.crop(x, y, x + w, y + h)
}

// ------------------------------------------------------------ detection

/// `_scene_signature(image)` — 12x8 LANCZOS art grid, float32 math.
pub fn scene_signature(card: &Rgb) -> Vec<i64> {
    let (w, h) = (card.w as f64, card.h as f64);
    let art = card.crop((0.08 * w) as i64, (0.14 * h) as i64, (0.92 * w) as i64, (0.58 * h) as i64);
    let s = art.resize_lanczos(12, 8);
    let mut v: Vec<f32> = s.px.iter().map(|&p| p as f32).collect();
    let mean = (super::num::pairwise_sum_f32(&v) / v.len() as f32) as f32;
    for x in v.iter_mut() {
        *x -= mean;
    }
    // np.linalg.norm(float32) → sdot; accumulated in f64 then cast (≤1 ulp)
    let sq: f64 = v.iter().map(|&x| x as f64 * x as f64).sum();
    let norm = (sq as f32).sqrt();
    if (norm as f64) <= 1e-9 {
        return vec![];
    }
    let k = (1000.0f64 / norm as f64) as f32;
    v.iter().map(|&x| (x * k).round_ties_even() as i64).collect()
}

pub struct Detection {
    pub frame: (usize, usize),
    pub candidates: Vec<Map<String, Value>>,
    pub images: Vec<Rgb>,
    pub prepare_ms: i64,
    pub detect_ms: i64,
    pub total_ms: i64,
}

/// `detect_uploaded_scene(raw, keep_images)`.
pub fn detect_uploaded_scene(raw: &[u8], keep_images: bool) -> Result<Detection, String> {
    let started = Instant::now();
    let image = decode(raw)?;
    let prepared = started.elapsed();
    let gray = Gray::pil_of(&image);
    let cands: Vec<Cand> = detect::scene_candidates(&image, &gray);
    let detected = started.elapsed();
    let (iw, ih) = (image.w as i64, image.h as i64);
    let mut assessed = vec![];
    let mut kept = vec![];
    for (i, c) in cands.into_iter().enumerate() {
        let number = i + 1;
        let [x, y, w, h] = c.bx;
        let card = match &c.quad {
            Some(q) => detect::warp_card_quad(&image, q)?,
            None => crop(&image, x, y, w, h),
        };
        let g = Gray::pil_of(&card);
        let (cw, ch) = (card.w as f64, card.h as f64);
        let ty0 = (0.03 * ch) as i64;
        let tx0 = (0.03 * cw) as i64;
        let title = g.hist(ty0, (ty0 + 1).max((0.16 * ch) as i64), tx0, (tx0 + 1).max((0.90 * cw) as i64));
        let fy0 = (0.85 * ch) as i64;
        let footer = g.hist(fy0, (fy0 + 1).max((0.99 * ch) as i64), 0, 1.max((0.60 * cw) as i64));
        let sharp = detect::box_sharpness(&g);
        let pc = |h: &Hist| h.pct(90.0) - h.pct(10.0);
        let (tc, fc) = (pc(&title), pc(&footer));
        let (tg, fg) = (title.frac_where(|v| v > 250), footer.frac_where(|v| v > 250));
        let ex = 2.max((0.01 * iw as f64) as i64);
        let ey = 2.max((0.01 * ih as f64) as i64);
        let clipped = match &c.quad {
            Some(q) => {
                let xs = q.iter().map(|p| p[0]);
                let ys = q.iter().map(|p| p[1]);
                xs.clone().fold(f64::INFINITY, f64::min) <= ex as f64
                    || ys.clone().fold(f64::INFINITY, f64::min) <= ey as f64
                    || xs.fold(f64::NEG_INFINITY, f64::max) >= (iw - ex) as f64
                    || ys.fold(f64::NEG_INFINITY, f64::max) >= (ih - ey) as f64
            }
            None => x <= ex || y <= ey || x + w >= iw - ex || y + h >= ih - ey,
        };
        let mut warnings: Vec<&str> = vec![];
        if clipped {
            warnings.push("touches frame edge");
        }
        if sharp < AUTO_SHARP_FLOOR {
            warnings.push("too blurry");
        }
        if tc < 35.0 {
            warnings.push("title has low contrast");
        }
        if fc < 35.0 {
            warnings.push("footer has low contrast");
        }
        if tg > 0.55 {
            warnings.push("title glare");
        }
        if fg > 0.55 {
            warnings.push("footer glare");
        }
        let eligible = !clipped && sharp >= AUTO_SHARP_FLOOR;
        let mut m = c.json.clone();
        m.insert("number".into(), json!(number));
        m.insert("eligible".into(), json!(eligible));
        m.insert("status".into(), json!(if eligible { "ready" } else { warnings[0] }));
        m.insert("warnings".into(), json!(warnings));
        m.insert("sharpness".into(), json!(py_round(sharp, 2)));
        m.insert("title_contrast".into(), json!(py_round(tc, 1)));
        m.insert("footer_contrast".into(), json!(py_round(fc, 1)));
        m.insert("title_glare".into(), json!(py_round(tg, 3)));
        m.insert("footer_glare".into(), json!(py_round(fg, 3)));
        m.insert("signature".into(), json!(scene_signature(&card)));
        assessed.push(m);
        if keep_images {
            kept.push(card);
        }
    }
    let as_ms = |d: std::time::Duration| (d.as_secs_f64() * 1000.0) as i64;
    Ok(Detection {
        frame: (image.w, image.h),
        candidates: assessed,
        images: kept,
        prepare_ms: as_ms(prepared),
        detect_ms: as_ms(detected - prepared),
        total_ms: ms(started),
    })
}

pub fn detection_json(d: &Detection) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("ok".into(), json!(true));
    m.insert("frame".into(), json!({"width": d.frame.0, "height": d.frame.1}));
    m.insert("candidates".into(), Value::Array(d.candidates.iter().cloned().map(Value::Object).collect()));
    m.insert("timings".into(), json!({"prepare_ms": d.prepare_ms, "detect_ms": d.detect_ms, "total_ms": d.total_ms}));
    m
}

// ------------------------------------------------------------ printings

type Evidence = Map<String, Value>;

fn ev_resolved(set: Value, num: Value, by: &str) -> Evidence {
    let mut e = Map::new();
    e.insert("collector".into(), json!([]));
    e.insert("set".into(), json!([]));
    e.insert("set_code".into(), set);
    e.insert("collector_number".into(), json!(normalize_collector(&py_str(&num))));
    e.insert("resolved_by".into(), json!(by));
    e
}

fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(), // `str(value or "")`
        o => o.to_string(),
    }
}

fn strip(img: &Rgb, x0: f64, x1: f64, y0: f64, y1: f64, th: usize) -> Option<Rgb> {
    scan_ocr::title_strip(img, x0, x1, y0, y1, th)
}

fn rows(k: &str, d: &str) -> Vec<f64> {
    env(k, d).split(',').filter(|v| !v.is_empty()).filter_map(|v| v.trim().parse().ok()).collect()
}

/// `_batch_scene_printings(images, titles)` → (hints, evidence).
pub fn batch_scene_printings(ctx: &Ctx, images: &[Rgb], titles: &[Title]) -> Result<(Vec<Option<Map<String, Value>>>, Vec<Evidence>), String> {
    let p = ctx.printings;
    let n = images.len();
    let mut hints: Vec<Option<Map<String, Value>>> = vec![None; n];
    let mut evidence: Vec<Evidence> = vec![Map::new(); n];
    let mut unresolved = vec![];
    for (i, t) in titles.iter().enumerate() {
        let Some(name) = &t.name else { continue };
        let (id, by) = match p.unique_title(name) {
            Some(id) => (Some(id), "unique physical printing"),
            None => (p.unique_ocr(t.raw.as_deref(), name), "unique printed title"),
        };
        match id {
            Some(id) => {
                let card = p.printing_face(id, name);
                evidence[i] = ev_resolved(card.get("set").cloned().unwrap_or(Value::Null), card.get("num").cloned().unwrap_or(Value::Null), by);
                hints[i] = Some(card);
            }
            None => unresolved.push(i),
        }
    }
    if unresolved.is_empty() {
        return Ok((hints, evidence));
    }
    let x1_sweep: f64 = env("CARDSCAN_FOOTER_X1", "0.22").parse().unwrap_or(0.22);
    let wide_rows = rows("CARDSCAN_WIDE_ROWS", "0.85,0.865,0.82,0.835");
    let wide_x1: f64 = env("CARDSCAN_WIDE_X1", "0.30").parse().unwrap_or(0.30);
    let mut stages: Vec<(Vec<f64>, bool)> = env("CARDSCAN_FOOTER_STAGES", "0.90,0.92;0.88,0.94,0.86;0.76,0.78,0.80,0.82,0.84")
        .split(';')
        .map(|s| (s.split(',').filter_map(|v| v.trim().parse().ok()).collect(), false))
        .collect();
    if !wide_rows.is_empty() {
        stages.insert(1.min(stages.len()), (wide_rows, true));
    }
    let mut raws: Vec<Vec<String>> = vec![vec![]; n];
    let mut held: HashMap<usize, Vec<String>> = HashMap::new();
    let mut pending = unresolved;
    let n_stages = stages.len();
    for (si, (stage, wide)) in stages.into_iter().enumerate() {
        if pending.is_empty() {
            break;
        }
        let x1 = if wide { wide_x1 } else { x1_sweep };
        pending.retain(|&o| !evidence[o].contains_key("choices"));
        if pending.is_empty() {
            break;
        }
        if si == 1 {
            retro_footer_pass(ctx, images, titles, &pending, &mut raws, &mut hints, &mut evidence, &mut held)?;
            pending.retain(|&o| hints[o].is_none() && !evidence[o].contains_key("choices"));
            if pending.is_empty() {
                break;
            }
        }
        let (mut strips, mut owners) = (vec![], vec![]);
        for &o in &pending {
            for &y0 in &stage {
                if let Some(s) = strip(&images[o], 0.0, x1, y0, (y0 + 0.025).min(0.99), 160) {
                    strips.push(s);
                    owners.push(o);
                }
            }
        }
        let out = recognize_rgb(ctx, &strips)?;
        for (&o, (raw, conf)) in owners.iter().zip(out) {
            if !raw.is_empty() && conf >= 0.45 {
                raws[o].push(raw);
            }
        }
        let mut still = vec![];
        for &o in &pending {
            let name = titles[o].name.clone().unwrap_or_default();
            let tn = norm_name(&name);
            let numbers = footer_numbers(&raws[o]);
            let codes = p.footer_codes(&raws[o]);
            let mut group = p.resolve_footer_group(&tn, &codes, &numbers);
            let mut e = Map::new();
            e.insert("collector".into(), json!(raws[o]));
            e.insert("set".into(), json!([]));
            for k in ["retro_extra", "retro_held"] {
                if let Some(v) = evidence[o].get(k).filter(|v| truthy(v)) {
                    e.insert(k.into(), v.clone());
                }
            }
            evidence[o] = e;
            let h = held.get(&o).cloned().unwrap_or_default();
            if let Some(g) = &group {
                if !h.is_empty() && p.strong_conflict(&tn, g[0], &raws[o], &h) {
                    group = None;
                }
            }
            if let Some(g) = &group {
                if g.len() > 1 {
                    evidence[o].insert("choices".into(), p.choice_evidence(g));
                    continue;
                }
            }
            match group {
                Some(g) => {
                    let card = p.printing_face(g[0], &name);
                    evidence[o].insert("set_code".into(), card.get("set").cloned().unwrap_or(Value::Null));
                    evidence[o].insert("collector_number".into(), json!(normalize_collector(&py_str(card.get("num").unwrap_or(&Value::Null)))));
                    evidence[o].insert("resolved_by".into(), json!("title+set+collector"));
                    hints[o] = Some(card);
                }
                None => still.push(o),
            }
        }
        pending = still;
    }
    pending.retain(|&o| !evidence[o].contains_key("choices"));
    if !pending.is_empty() && n_stages < 2 {
        retro_footer_pass(ctx, images, titles, &pending, &mut raws, &mut hints, &mut evidence, &mut held)?;
    }
    Ok((hints, evidence))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Array(a) => !a.is_empty(),
        Value::String(s) => !s.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn retro_footer_pass(
    ctx: &Ctx,
    images: &[Rgb],
    titles: &[Title],
    pending: &[usize],
    raws: &mut [Vec<String>],
    hints: &mut [Option<Map<String, Value>>],
    evidence: &mut [Evidence],
    held: &mut HashMap<usize, Vec<String>>,
) -> Result<(), String> {
    let p = ctx.printings;
    let retro_rows = rows("CARDSCAN_RETRO_ROWS", "0.855,0.845,0.865,0.875");
    let x0: f64 = env("CARDSCAN_RETRO_X0", "0.35").parse().unwrap_or(0.35);
    let rh: usize = env("CARDSCAN_RETRO_H", "160").parse().unwrap_or(160);
    let (mut strips, mut owners, mut rws) = (vec![], vec![], vec![]);
    for &o in pending {
        for (row, &y0) in retro_rows.iter().enumerate() {
            if let Some(s) = strip(&images[o], x0, 0.95, y0, (y0 + 0.025).min(0.99), rh) {
                strips.push(s);
                owners.push(o);
                rws.push(row);
            }
        }
    }
    let out = recognize_rgb(ctx, &strips)?;
    let mut retro: HashMap<usize, indexmap::IndexMap<String, i64>> = pending.iter().map(|&o| (o, Default::default())).collect();
    let mut extra: HashMap<usize, Vec<String>> = pending.iter().map(|&o| (o, vec![])).collect();
    for ((&o, &row), (raw, conf)) in owners.iter().zip(&rws).zip(out) {
        if raw.is_empty() || conf < 0.6 {
            continue;
        }
        if let Some(num) = retro_line_number(&raw) {
            let num = normalize_collector(&num);
            let r = retro.get_mut(&o).unwrap();
            let cur = *r.get(&num).unwrap_or(&0);
            r.insert(num.clone(), cur.max(retro_total(&raw)));
            let h = held.entry(o).or_default();
            if !h.contains(&num) {
                h.push(num);
            }
        }
        if row < 2 {
            raws[o].push(raw);
        } else {
            extra.get_mut(&o).unwrap().push(raw);
        }
    }
    for &o in pending {
        let name = titles[o].name.clone().unwrap_or_default();
        let tn = norm_name(&name);
        let mut e = Map::new();
        e.insert("collector".into(), json!(raws[o]));
        e.insert("set".into(), json!([]));
        if !extra[&o].is_empty() {
            e.insert("retro_extra".into(), json!(extra[&o]));
        }
        let h = held.get(&o).cloned().unwrap_or_default();
        if !h.is_empty() {
            e.insert("retro_held".into(), json!(h));
        }
        evidence[o] = e;
        let numbers: Vec<String> = retro[&o].keys().cloned().collect();
        if numbers.is_empty() {
            continue;
        }
        let mut group = p.resolve_footer_group(&tn, &[], &numbers);
        if let Some(g) = &group {
            let (_s, smax, pnum) = p.retro_set_max(g[0]);
            let total = *retro[&o].get(&pnum).unwrap_or(&0);
            if total != 0 && smax < total {
                group = None;
            }
        }
        if let Some(g) = &group {
            if p.strong_conflict(&tn, g[0], &raws[o], &h) {
                group = None;
            }
        }
        if let Some(g) = &group {
            if g.len() > 1 {
                evidence[o].insert("choices".into(), p.choice_evidence(g));
                continue;
            }
        }
        let Some(g) = group else { continue };
        let card = p.printing_face(g[0], &name);
        evidence[o].insert("set_code".into(), card.get("set").cloned().unwrap_or(Value::Null));
        evidence[o].insert("collector_number".into(), json!(normalize_collector(&py_str(card.get("num").unwrap_or(&Value::Null)))));
        evidence[o].insert("resolved_by".into(), json!("title+collector (retro frame)"));
        hints[o] = Some(card);
    }
    Ok(())
}

const FOOTER_COLLECTOR_VARIANTS: [(f64, f64, f64, f64, usize); 7] = [
    (0.00, 0.38, 0.85, 0.88, 96),
    (0.08, 0.48, 0.865, 0.895, 96),
    (0.00, 0.30, 0.88, 0.92, 96),
    (0.00, 0.45, 0.90, 0.94, 96),
    (0.00, 0.45, 0.92, 0.96, 96),
    (0.00, 0.45, 0.96, 1.00, 96),
    (0.00, 0.32, 0.86, 0.91, 96),
];
const FOOTER_SET_VARIANTS: [(f64, f64, f64, f64, usize); 4] =
    [(0.00, 0.45, 0.90, 0.94, 96), (0.00, 0.48, 0.92, 0.97, 96), (0.08, 0.48, 0.88, 0.91, 96), (0.00, 0.48, 0.95, 0.985, 96)];

/// `_recognize_footer_strips(crop, (variant,), skew)`. The PIL BICUBIC
/// `strip.rotate(skew)` for |skew| >= 0.75 is NOT ported (documented gap;
/// only reachable with CARDSCAN_SCENE_FALLBACK=1, off in production).
fn footer_strip_raws(ctx: &Ctx, img: &Rgb, v: (f64, f64, f64, f64, usize)) -> Result<Vec<String>, String> {
    let Some(s) = strip(img, v.0, v.1, v.2, v.3, v.4) else { return Ok(vec![]) };
    let (raw, conf) = (ctx.rec_call)(&s)?;
    Ok(if !raw.is_empty() && conf >= 0.55 { vec![raw] } else { vec![] })
}

/// `identify_exact_printing(crop, skew, title)` → (card, footer_ocr).
fn identify_exact_printing(ctx: &Ctx, img: &Rgb, title: &str) -> Result<(Option<Map<String, Value>>, Evidence), String> {
    let p = ctx.printings;
    let tn = norm_name(title);
    let (mut col, mut set): (Vec<String>, Vec<String>) = (vec![], vec![]);
    let ev = |col: &Vec<String>, set: &Vec<String>| {
        let mut e = Map::new();
        e.insert("collector".into(), json!(col));
        e.insert("set".into(), json!(set));
        e
    };
    let variants: Vec<(bool, (f64, f64, f64, f64, usize))> =
        FOOTER_COLLECTOR_VARIANTS.iter().map(|v| (false, *v)).chain(FOOTER_SET_VARIANTS.iter().map(|v| (true, *v))).collect();
    for (is_set, v) in variants {
        let r = footer_strip_raws(ctx, img, v)?;
        if is_set { set.extend(r) } else { col.extend(r) }
        let all: Vec<String> = col.iter().chain(set.iter()).cloned().collect();
        let group = p.resolve_footer_group(&tn, &p.footer_codes(&all), &footer_numbers(&all));
        if let Some(g) = group {
            if g.len() > 1 {
                let mut e = ev(&col, &set);
                e.insert("choices".into(), p.choice_evidence(&g));
                return Ok((None, e));
            }
            let card = p.printing_face(g[0], title);
            let mut e = ev(&col, &set);
            let raw = p.dict(g[0]);
            e.insert("set_code".into(), raw.get("set").cloned().unwrap_or(Value::Null));
            e.insert("collector_number".into(), json!(normalize_collector(&py_str(raw.get("num").unwrap_or(&Value::Null)))));
            return Ok((Some(card), e));
        }
    }
    Ok((None, ev(&col, &set)))
}

// ------------------------------------------------------------ history

#[derive(Default)]
pub struct History {
    entries: Vec<Map<String, Value>>,
}

impl History {
    /// `ScanHistory.record(...)` (in memory; Scrybox never reads it back).
    #[allow(clippy::too_many_arguments)]
    pub fn record(&mut self, name: &str, image: &Value, set_name: &Value, rarity: &Value, id: &Value, set_code: &Value, num: &Value) -> Option<(Map<String, Value>, bool)> {
        if name.is_empty() || !truthy(id) || !truthy(set_code) || !truthy(num) || !truthy(image) {
            return None;
        }
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let slug = {
            let l = name.to_lowercase();
            let mut s = String::new();
            let mut dash = false;
            for c in l.chars() {
                if c.is_ascii_lowercase() || c.is_ascii_digit() {
                    s.push(c);
                    dash = false;
                } else if !dash {
                    s.push('-');
                    dash = true;
                }
            }
            s.trim_matches('-').chars().take(50).collect::<String>()
        };
        let eid = format!("{slug}-{}-{}", py_str(set_code), py_str(num));
        if let Some(i) = self.entries.iter().position(|c| c.get("scryfall_id") == Some(id)) {
            let mut c = self.entries.remove(i);
            let count = c.get("count").and_then(|v| v.as_i64()).unwrap_or(1) + 1;
            c.insert("count".into(), json!(count));
            c.insert("last_seen".into(), json!(ts));
            for (k, v) in [("image", image), ("set", set_name), ("rarity", rarity), ("scryfall_id", id), ("set_code", set_code), ("collector_number", num)] {
                if !v.is_null() {
                    c.insert(k.into(), v.clone());
                }
            }
            c.insert("id".into(), json!(eid));
            self.entries.insert(0, c.clone());
            return Some((c, false));
        }
        let mut c = Map::new();
        for (k, v) in [
            ("id", json!(eid)),
            ("name", json!(name)),
            ("image", image.clone()),
            ("set", set_name.clone()),
            ("set_code", set_code.clone()),
            ("collector_number", num.clone()),
            ("scryfall_id", id.clone()),
            ("rarity", rarity.clone()),
            ("count", json!(1)),
            ("first_seen", json!(ts)),
            ("last_seen", json!(ts)),
        ] {
            c.insert(k.into(), v);
        }
        self.entries.insert(0, c.clone());
        self.entries.truncate(5000);
        Some((c, true))
    }
}

// ------------------------------------------------------------ scan_image

/// `scan_image(image, source="scene-batch", box_hint=(0,0,w,h), title_hint=..)`.
#[allow(clippy::too_many_arguments)]
fn scan_image_scene(
    ctx: &Ctx,
    image: &Rgb,
    name: &str,
    raw_title: Option<&str>,
    score: f64,
    hint: Option<Map<String, Value>>,
    evidence: Evidence,
    checked: bool,
) -> Result<Map<String, Value>, String> {
    let t0 = Instant::now();
    let bx = [0, 0, image.w as i64, image.h as i64];
    let t_ready = t0.elapsed();
    let t_detect = t0.elapsed();
    let g = Gray::pil_of(image);
    let sharpness = detect::box_sharpness(&g);
    let skew = detect::estimate_card_skew(&g);
    let t_crop = t0.elapsed();
    let (rect_img, rectified) = detect::rectify_rotated_card(image, skew);
    let ocr_crop = rect_img.as_ref().unwrap_or(image);
    let t_ocr_start = t0.elapsed();
    let p = ctx.printings;
    let oracle_card = !name.is_empty() && ctx.oracle.names.contains(name);
    let (exact, mut footer): (Option<Map<String, Value>>, Evidence) = if checked {
        (hint, evidence)
    } else {
        let mut ex = None;
        let mut by = "unique physical printing";
        if oracle_card {
            ex = p.unique_title(name).map(|id| p.printing_face(id, name));
            if ex.is_none() {
                ex = p.unique_ocr(raw_title, name).map(|id| p.printing_face(id, name));
                by = "unique printed title";
            }
        }
        match ex {
            Some(card) => {
                let e = ev_resolved(card.get("set").cloned().unwrap_or(Value::Null), card.get("num").cloned().unwrap_or(Value::Null), by);
                (Some(card), e)
            }
            None if oracle_card => {
                let (c, mut e) = identify_exact_printing(ctx, ocr_crop, name)?;
                // visual matching runs only for source == "scene-card"
                if c.is_none() {
                    e.insert("visual".into(), Value::Null);
                }
                // `footer_ocr["visual"] = visual_details` happens only when
                // visual_budget; for scene-batch it never runs → drop it again.
                e.remove("visual");
                (c, e)
            }
            None => (None, Map::new()),
        }
    };
    let printing_exact = exact.is_some();
    let unresolved = oracle_card && exact.is_none();
    let t_ocr = t0.elapsed();
    let matched_via: Value = if printing_exact {
        match footer.get("resolved_by").filter(|v| truthy(v)) {
            Some(v) => v.clone(),
            None => json!("title+set+collector"),
        }
    } else {
        Value::Null
    };
    let deadline = t0.elapsed().as_secs_f64() > SCAN_DEADLINE_S;
    let d = |x: std::time::Duration| (x.as_secs_f64() * 1000.0) as i64;
    let mut out = Map::new();
    if name.is_empty() || exact.is_none() || deadline {
        let err = if deadline {
            "scan exceeded 900 ms deadline"
        } else if unresolved && footer.contains_key("choices") && truthy(&footer["choices"]) {
            "printing needs a choice"
        } else if unresolved {
            "exact printing not resolved"
        } else {
            "no confident card match"
        };
        out.insert("ok".into(), json!(false));
        out.insert("retry".into(), json!(true));
        out.insert("error".into(), json!(err));
        out.insert("printing_exact".into(), json!(false));
        out.insert("footer_ocr".into(), Value::Object(std::mem::take(&mut footer)));
        out.insert("ocr".into(), json!(raw_title));
        out.insert("title".into(), json!(name));
        out.insert("title_score".into(), json!(py_round(score, 3)));
        out.insert("ocr_backend".into(), json!(OCR_BACKEND));
        out.insert("ocr_profile".into(), Value::Null);
        out.insert("box".into(), json!(bx));
        out.insert("sharpness".into(), json!(py_round(sharpness, 2)));
        out.insert(
            "timings".into(),
            json!({"pick_ms": 0, "prepare_ms": d(t_ready), "detect_ms": d(t_detect - t_ready), "crop_ms": d(t_crop - t_detect),
                   "ocr_ms": d(t_ocr - t_ocr_start), "total_ms": ms(t0)}),
        );
        return Ok(out);
    }
    let card = exact.unwrap();
    let canonical = card.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or(name).to_string();
    let printed = [card.get("flavor_name"), card.get("printed_name")].into_iter().flatten().find(|v| truthy(v)).cloned().unwrap_or(Value::Null);
    let visible = match printed.as_str() {
        Some(pn) if norm_name(pn) != norm_name(&canonical) => format!("{pn} ({canonical})"),
        _ => canonical.clone(),
    };
    let t_background = t0.elapsed();
    let g0 = |k: &str| card.get(k).cloned().unwrap_or(Value::Null);
    let rec = ctx.history.lock().unwrap().record(&visible, &g0("image"), &g0("set_name"), &g0("rarity"), &g0("id"), &g0("set"), &g0("num"));
    let Some((entry, is_new)) = rec else {
        return Err("history accepts exact physical printings only".into());
    };
    let t_record = t0.elapsed();
    let now = chrono::Local::now();
    out.insert("ok".into(), json!(true));
    out.insert("source".into(), json!("scene-batch"));
    out.insert("file".into(), json!(format!("/media/out/scan_{}_{:03}.jpg", now.format("%Y%m%d_%H%M%S"), now.timestamp_subsec_millis())));
    out.insert("ts".into(), json!(now.format("%Y-%m-%dT%H:%M:%S").to_string()));
    out.insert("ocr".into(), json!(raw_title));
    out.insert("ocr_backend".into(), json!(OCR_BACKEND));
    out.insert("ocr_profile".into(), Value::Null);
    out.insert("identified".into(), json!({"name": visible, "face": name, "printed_name": printed, "score": py_round(score, 3), "via": matched_via}));
    out.insert("printing_exact".into(), json!(true));
    out.insert("footer_ocr".into(), Value::Object(footer));
    out.insert("card".into(), Value::Object(card));
    out.insert("history".into(), json!({"entry": entry, "new": is_new, "recorded": true}));
    out.insert("box".into(), json!(bx));
    out.insert("orientation".into(), json!(0));
    out.insert("skew".into(), json!(py_round(skew, 2)));
    out.insert("rectified".into(), json!(rectified));
    out.insert("sharpness".into(), json!(py_round(sharpness, 2)));
    out.insert(
        "timings".into(),
        json!({"pick_ms": 0, "prepare_ms": d(t_ready), "detect_ms": d(t_detect - t_ready), "crop_ms": d(t_crop - t_detect),
               "ocr_ms": d(t_ocr - t_ocr_start), "background_ms": d(t_background - t_crop), "record_ms": d(t_record - t_background),
               "total_ms": d(t_record)}),
    );
    Ok(out)
}

/// `_scan_prepared_scene(prepared, numbers, started)`.
pub fn scan_prepared_scene(ctx: &Ctx, mut prepared: Vec<Rgb>, numbers: &[i64], started: Instant) -> Result<Map<String, Value>, String> {
    let decoded = started.elapsed();
    let titles = scan_ocr::batch_scene_titles(&prepared, ctx.oracle, ctx.rec)?;
    for (i, t) in titles.iter().enumerate() {
        if t.angle != 0 {
            prepared[i] = prepared[i].rotate_right_angle(t.angle);
        }
    }
    let titled = started.elapsed();
    let (hints, evidence) = batch_scene_printings(ctx, &prepared, &titles)?;
    let printed = started.elapsed();
    let fallback_off = env("CARDSCAN_SCENE_FALLBACK", "1") == "0";
    let mut results = vec![];
    for (((&number, image), t), (hint, ev)) in numbers.iter().zip(&prepared).zip(&titles).zip(hints.into_iter().zip(evidence)) {
        let Some(name) = &t.name else {
            results.push(json!({"ok": false, "retry": true, "error": "no confident card title", "scene_number": number, "timings": {"total_ms": 0}}));
            continue;
        };
        let checked = hint.is_some()
            || ev.get("choices").is_some_and(truthy)
            || ev.get("retro_held").is_some_and(truthy)
            || fallback_off;
        let mut r = scan_image_scene(ctx, image, name, t.raw.as_deref(), t.score, hint, ev, checked)?;
        r.insert("scene_number".into(), json!(number));
        results.push(Value::Object(r));
    }
    let matched = results.iter().filter(|r| r.get("ok").is_some_and(truthy)).count();
    let d = |x: std::time::Duration| (x.as_secs_f64() * 1000.0) as i64;
    let mut m = Map::new();
    m.insert("ok".into(), json!(true));
    m.insert("attempted".into(), json!(results.len()));
    m.insert("results".into(), Value::Array(results));
    m.insert("matched".into(), json!(matched));
    // key order as the Python literal: ok, results, matched, attempted
    let mut o = Map::new();
    for k in ["ok", "results", "matched", "attempted"] {
        o.insert(k.into(), m.remove(k).unwrap());
    }
    o.insert(
        "timings".into(),
        json!({"decode_ms": d(decoded), "title_batch_ms": d(titled - decoded), "footer_batch_ms": d(printed - titled),
               "publish_ms": d(started.elapsed() - printed), "total_ms": ms(started)}),
    );
    Ok(o)
}

/// `scan_scene_batch(payload)`.
pub fn scan_scene_batch(ctx: &Ctx, payload: &Value) -> Result<Map<String, Value>, String> {
    use base64::Engine;
    let started = Instant::now();
    let cards = match payload.get("cards") {
        Some(Value::Array(a)) => a.clone(),
        Some(v) if truthy(v) => return Err("scene batch requires a card list".into()),
        _ => vec![],
    };
    if !(1..=8).contains(&cards.len()) {
        let mut m = Map::new();
        m.insert("ok".into(), json!(false));
        m.insert("error".into(), json!("scene batch requires 1-8 cards"));
        return Ok(m);
    }
    let (mut prepared, mut numbers) = (vec![], vec![]);
    for item in &cards {
        let enc = match item.get("image") {
            Some(Value::String(s)) => s.clone(),
            Some(v) if truthy(v) => v.to_string(),
            _ => String::new(),
        };
        if enc.len() > 35 * 1024 * 1024 {
            return Err("scene card image is too large".into());
        }
        let raw = base64::engine::general_purpose::STANDARD.decode(enc.as_bytes()).map_err(|e| e.to_string())?;
        let image = decode(&raw).map_err(|e| e.replace("image exceeds", "scene card exceeds"))?;
        let quad = item.get("quad").filter(|q| truthy(q));
        let img = if let Some(q) = quad {
            let flat: Vec<f64> = q.as_array().into_iter().flatten().flat_map(|p| match p {
                Value::Array(xy) => xy.iter().map(|v| v.as_f64().unwrap_or(f64::NAN)).collect::<Vec<_>>(),
                v => vec![v.as_f64().unwrap_or(f64::NAN)],
            }).collect();
            if flat.len() != 8 {
                return Err(format!("cannot reshape array of size {} into shape (4,2)", flat.len()));
            }
            let pts: [[f64; 2]; 4] = std::array::from_fn(|i| [(flat[2 * i] as f32) as f64, (flat[2 * i + 1] as f32) as f64]);
            if pts.iter().any(|p| p[0] < 0.0 || p[1] < 0.0 || p[0] >= image.w as f64 || p[1] >= image.h as f64) {
                return Err("invalid scene card quadrilateral".into());
            }
            detect::warp_card_quad(&image, &pts)?
        } else {
            let bx: Vec<i64> = item.get("box").and_then(|b| b.as_array()).map(|a| a.iter().map(|v| v.as_f64().unwrap_or(0.0) as i64).collect()).unwrap_or_default();
            if bx.len() != 4 {
                return Err("scene card box missing".into());
            }
            let (x, y, w, h) = (bx[0], bx[1], bx[2], bx[3]);
            if x < 0 || y < 0 || w < 40 || h < 60 || x + w > image.w as i64 || y + h > image.h as i64 {
                return Err("invalid scene card box".into());
            }
            crop(&image, x, y, w, h)
        };
        prepared.push(img);
        let n = item.get("number").filter(|v| truthy(v)).and_then(|v| v.as_f64()).map(|f| f as i64).unwrap_or(numbers.len() as i64 + 1);
        numbers.push(n);
    }
    scan_prepared_scene(ctx, prepared, &numbers, started)
}

// ------------------------------------------------------------ scan_frame

pub struct IdentityEntry {
    signature: Vec<i64>,
    bx: Value,
    seen: f64,
    result: Map<String, Value>,
}

#[derive(Default)]
pub struct IdentityCache {
    entries: std::collections::VecDeque<IdentityEntry>,
    session: Option<String>,
}

fn sig_sim(a: &[i64], b: &[i64]) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return -1.0;
    }
    let na = a.iter().map(|&x| (x * x) as f64).sum::<f64>().sqrt();
    let nb = b.iter().map(|&x| (x * x) as f64).sum::<f64>().sqrt();
    if na <= 0.0 || nb <= 0.0 {
        return -1.0;
    }
    a.iter().zip(b).map(|(&x, &y)| (x * y) as f64).sum::<f64>() / (na * nb)
}

fn sig_of(c: &Map<String, Value>) -> Vec<i64> {
    c.get("signature").and_then(|s| s.as_array()).map(|a| a.iter().filter_map(|v| v.as_i64()).collect()).unwrap_or_default()
}

fn centre(b: &Value) -> (f64, f64) {
    let v: Vec<f64> = b.as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect()).unwrap_or_default();
    if v.len() < 4 {
        return (0.0, 0.0);
    }
    (v[0] + v[2] / 2.0, v[1] + v[3] / 2.0)
}

impl IdentityCache {
    fn set_session(&mut self, s: Option<String>) {
        if s != self.session || s.is_none() {
            self.entries.clear();
        }
        self.session = s;
    }

    fn refresh(&mut self, cards: &[Map<String, Value>], fw: f64, fh: f64, now: f64, gap: f64) {
        if self.entries.is_empty() {
            return;
        }
        let diag = (fw * fw + fh * fh).sqrt();
        let mut near: indexmap::IndexMap<usize, Vec<usize>> = indexmap::IndexMap::new();
        for (ei, e) in self.entries.iter().enumerate() {
            if now - e.seen > gap {
                continue;
            }
            let (ex, ey) = centre(&e.bx);
            let mut v = vec![];
            for (ci, c) in cards.iter().enumerate() {
                if c.get("box").is_none_or(|b| !truthy(b)) || sig_sim(&sig_of(c), &e.signature) < 0.97 {
                    continue;
                }
                let (cx, cy) = centre(&c["box"]);
                if ((cx - ex).powi(2) + (cy - ey).powi(2)).sqrt() <= 0.15 * diag {
                    v.push(ci);
                }
            }
            near.insert(ei, v);
        }
        let mut claims: HashMap<usize, usize> = HashMap::new();
        for cis in near.values() {
            for &ci in cis {
                *claims.entry(ci).or_default() += 1;
            }
        }
        let mut old: Vec<Option<IdentityEntry>> = self.entries.drain(..).map(Some).collect();
        for (ei, cis) in near {
            if cis.len() != 1 || claims[&cis[0]] != 1 {
                continue;
            }
            let c = &cards[cis[0]];
            if !c.get("eligible").is_some_and(truthy) {
                continue;
            }
            let mut e = old[ei].take().unwrap();
            e.bx = c["box"].clone();
            e.seen = now;
            self.entries.push_back(e);
        }
    }

    fn lookup(&self, sig: &[i64], bx: &Value) -> Option<Map<String, Value>> {
        for e in self.entries.iter().rev() {
            if sig_sim(sig, &e.signature) < 0.97 {
                continue;
            }
            if e.bx != *bx {
                continue;
            }
            return Some(e.result.clone());
        }
        None
    }

    fn push(&mut self, e: IdentityEntry) {
        self.entries.push_back(e);
        while self.entries.len() > 64 {
            self.entries.pop_front();
        }
    }
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// `scan_frame(raw, max_cards=8, session)`.
pub fn scan_frame(ctx: &Ctx, cache: &mut IdentityCache, raw: &[u8], session: Option<String>) -> Result<Map<String, Value>, String> {
    let started = Instant::now();
    if raw.is_empty() {
        // detect_uploaded_scene's {"ok": False, ...} has no candidates
        cache.set_session(session);
        let mut m = Map::new();
        m.insert("ok".into(), json!(true));
        m.insert("frame".into(), Value::Null);
        m.insert("candidates".into(), json!([]));
        m.insert("results".into(), json!([]));
        m.insert("timings".into(), json!({"detect_ms": 0, "read_ms": 0, "total_ms": 0, "read_cards": 0, "cached_cards": 0}));
        return Ok(m);
    }
    let det = detect_uploaded_scene(raw, true)?;
    let detected_i = Instant::now();
    let detected = now_s();
    let gap: f64 = env("CARDSCAN_IDENTITY_GAP_S", "2.5").parse().unwrap_or(2.5);
    cache.set_session(session.clone());
    cache.refresh(&det.candidates, det.frame.0 as f64, det.frame.1 as f64, detected, gap);
    let (mut todo, mut numbers) = (vec![], vec![]);
    let mut cached: indexmap::IndexMap<i64, Value> = indexmap::IndexMap::new();
    for (c, img) in det.candidates.iter().zip(&det.images) {
        if !c.get("eligible").is_some_and(truthy) {
            continue;
        }
        let num = c["number"].as_i64().unwrap_or(0);
        if let Some(mut hit) = cache.lookup(&sig_of(c), c.get("box").unwrap_or(&Value::Null)) {
            hit.insert("scene_number".into(), json!(num));
            hit.insert("cached".into(), json!(true));
            cached.insert(num, Value::Object(hit));
            continue;
        }
        if todo.len() < 8 {
            todo.push(img.clone());
            numbers.push(num);
        }
    }
    let read_cards = todo.len();
    let batch = if todo.is_empty() {
        let mut m = Map::new();
        m.insert("ok".into(), json!(true));
        m.insert("results".into(), json!([]));
        m.insert("timings".into(), json!({}));
        m
    } else {
        scan_prepared_scene(ctx, todo, &numbers, detected_i)?
    };
    let mut by_number: indexmap::IndexMap<Value, Value> = indexmap::IndexMap::new();
    for r in batch.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default() {
        by_number.insert(r.get("scene_number").cloned().unwrap_or(Value::Null), r);
    }
    for c in &det.candidates {
        let Some(r) = by_number.get(&c["number"]) else { continue };
        let proven = r.get("footer_ocr").and_then(|f| f.get("resolved_by")).and_then(|v| v.as_str());
        if r.get("ok").is_some_and(truthy)
            && session.as_deref().is_some_and(|s| !s.is_empty())
            && matches!(proven, Some("unique physical printing") | Some("unique printed title"))
        {
            let mut res = Map::new();
            for k in ["ok", "card", "identified", "title", "footer_ocr", "printing_exact"] {
                res.insert(k.into(), r.get(k).cloned().unwrap_or(Value::Null));
            }
            cache.push(IdentityEntry { signature: sig_of(c), bx: c.get("box").cloned().unwrap_or(Value::Null), seen: detected, result: res });
        }
    }
    let mut results: Vec<Value> = by_number.into_values().collect();
    results.extend(cached.values().cloned());
    let mut timings = Map::new();
    let detect_ms = (detected_i - started).as_secs_f64() * 1000.0;
    timings.insert("detect_ms".into(), json!(detect_ms as i64));
    timings.insert("read_ms".into(), json!(ms(detected_i)));
    timings.insert("total_ms".into(), json!(ms(started)));
    timings.insert("read_cards".into(), json!(read_cards));
    timings.insert("cached_cards".into(), json!(cached.len()));
    if let Some(Value::Object(t)) = batch.get("timings") {
        for (k, v) in t {
            timings.insert(format!("batch_{k}"), v.clone());
        }
    }
    let mut m = Map::new();
    m.insert("ok".into(), json!(true));
    m.insert("frame".into(), json!({"width": det.frame.0, "height": det.frame.1}));
    m.insert("candidates".into(), Value::Array(det.candidates.into_iter().map(Value::Object).collect()));
    m.insert("results".into(), Value::Array(results));
    m.insert("timings".into(), Value::Object(timings));
    Ok(m)
}
