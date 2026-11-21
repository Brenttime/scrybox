//! cardscan card detection (`detect_card_bbox(all_candidates=True)`, the
//! rotated / closed-face / border / chroma / edge scene candidates, the
//! merge), `_warp_card_quad`, `box_sharpness`, `estimate_card_skew` and
//! `rectify_rotated_card`, on the real OpenCV (opencv crate, static 4.10 +
//! IPP-ICV, same algorithms as the sidecar's cv2 5.0).
//!
//! NumPy float32 arithmetic is reproduced op-for-op (NEP 50: Python floats
//! are weak, so `0.015 * float32_array` stays float32).

use super::num::{np_round1_f32, py_round, py_round_int, Hist};
use crate::scan_ocr::pil::Rgb;
use opencv::core::{self as cvc, Mat, Point, Point2f, Scalar, Size, Vector};
use opencv::imgproc;
use opencv::prelude::*;
use serde_json::{json, Map, Value};

pub const CARD_AR: f64 = 0.716;
const HOLE_BRIGHT: u8 = 140;
const DETECT_MAX_DIM: usize = 960;

/// Grayscale plane (row-major u8).
#[derive(Clone)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

impl Gray {
    /// `ImageOps.grayscale` (Pillow L24: 19595/38470/7471, +0x8000 >> 16).
    pub fn pil_of(img: &Rgb) -> Gray {
        let px = img
            .px
            .chunks_exact(3)
            .map(|p| ((p[0] as u32 * 19595 + p[1] as u32 * 38470 + p[2] as u32 * 7471 + 0x8000) >> 16) as u8)
            .collect();
        Gray { w: img.w, h: img.h, px }
    }
    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.px[y * self.w + x]
    }
    /// `g[::step, ::step]`.
    pub fn stride(&self, step: usize) -> Gray {
        let (w, h) = (self.w.div_ceil(step), self.h.div_ceil(step));
        let mut px = Vec::with_capacity(w * h);
        for y in (0..self.h).step_by(step) {
            for x in (0..self.w).step_by(step) {
                px.push(self.at(x, y));
            }
        }
        Gray { w, h, px }
    }
    /// numpy slice `g[y0:y1, x0:x1]` with Python clipping (non-negative ints).
    pub fn hist(&self, y0: i64, y1: i64, x0: i64, x1: i64) -> Hist {
        let c = |v: i64, m: usize| v.clamp(0, m as i64) as usize;
        let (y0, y1, x0, x1) = (c(y0, self.h), c(y1, self.h), c(x0, self.w), c(x1, self.w));
        let mut h = Hist::new();
        for y in y0..y1.max(y0) {
            for x in x0..x1.max(x0) {
                h.add(self.at(x, y));
            }
        }
        h
    }
    fn mat(&self) -> Mat {
        mat_from(&self.px, self.h, self.w, 1)
    }
}

pub fn mat_from(data: &[u8], rows: usize, cols: usize, ch: usize) -> Mat {
    let m = Mat::new_rows_cols_with_data(rows as i32, (cols * ch) as i32, data).expect("mat");
    m.reshape(ch as i32, rows as i32).expect("reshape").try_clone().expect("clone")
}

fn mat_bytes(m: &Mat) -> Vec<u8> {
    let m = if m.is_continuous() { m.try_clone().unwrap() } else { m.try_clone().unwrap() };
    m.data_bytes().expect("bytes").to_vec()
}

fn mask_mat(g: &Gray, f: impl Fn(u8) -> bool, on: u8) -> Mat {
    let px: Vec<u8> = g.px.iter().map(|&v| if f(v) { on } else { 0 }).collect();
    mat_from(&px, g.h, g.w, 1)
}

fn ones(k: i32) -> Mat {
    Mat::new_rows_cols_with_default(k, k, cvc::CV_8UC1, Scalar::all(1.0)).unwrap()
}

fn morph(src: &Mat, op: i32, kernel: &Mat, iterations: i32) -> Mat {
    let mut dst = Mat::default();
    imgproc::morphology_ex(
        src,
        &mut dst,
        op,
        kernel,
        Point::new(-1, -1),
        iterations,
        cvc::BORDER_CONSTANT,
        imgproc::morphology_default_border_value().unwrap(),
    )
    .unwrap();
    dst
}

fn contours(mask: &Mat, mode: i32) -> Vector<Vector<Point>> {
    let mut cs: Vector<Vector<Point>> = Vector::new();
    imgproc::find_contours(mask, &mut cs, mode, imgproc::CHAIN_APPROX_SIMPLE, Point::new(0, 0)).unwrap();
    cs
}

struct RRect {
    cx: f32,
    cy: f32,
    w: f32,
    h: f32,
    angle: f32,
    raw: cvc::RotatedRect,
}

/// `cv2.minAreaRect` with OpenCV 5.0's (w, h, angle) convention.
///
/// 4.x reports width = |out[1]|, height = |out[2]|, angle = atan2(out1.y,
/// out1.x) in (-180, 180]; 5.0 (geometry/rotcalipers.cpp) reports width =
/// |out[2]|, height = |out[1]|, angle = -atan2(out1.x, out1.y) in [-90, 0),
/// except out1 = (0, +y) → sizes unswapped, angle -90. Same calipers, same
/// rectangle (box points are identical), so the 5.0 triple is rebuilt from
/// the 4.10 one; `raw` stays 4.10 for `boxPoints`.
fn min_area_rect(c: &Vector<Point>) -> RRect {
    let r = imgproc::min_area_rect(c).unwrap();
    let (w4, h4, a4) = (r.size.width, r.size.height, r.angle);
    let (w, h, angle) = if a4 == 90.0 {
        (w4, h4, -90.0f32)
    } else if w4 == 0.0 && h4 == 0.0 {
        (w4, h4, -90.0f32)
    } else {
        let rad = (a4 as f64).to_radians();
        let (x, y) = (rad.cos(), rad.sin());
        (h4, w4, ((-(x.atan2(y))) * 180.0 / std::f64::consts::PI) as f32)
    };
    RRect { cx: r.center.x, cy: r.center.y, w, h, angle, raw: r }
}

fn box_points(r: &RRect) -> [[f32; 2]; 4] {
    let mut m = Mat::default();
    imgproc::box_points(r.raw, &mut m).unwrap();
    let mut out = [[0f32; 2]; 4];
    for (i, o) in out.iter_mut().enumerate() {
        o[0] = *m.at_2d::<f32>(i as i32, 0).unwrap();
        o[1] = *m.at_2d::<f32>(i as i32, 1).unwrap();
    }
    out
}

fn area(c: &Vector<Point>) -> f64 {
    imgproc::contour_area(c, false).unwrap()
}

pub type Quad = [[f32; 2]; 4];

fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

fn norm2(v: [f32; 2]) -> f32 {
    (v[0] * v[0] + v[1] * v[1]).sqrt()
}

fn argmin(v: &[f32; 4]) -> usize {
    let mut b = 0;
    for i in 1..4 {
        if v[i] < v[b] {
            b = i;
        }
    }
    b
}

fn argmax(v: &[f32; 4]) -> usize {
    let mut b = 0;
    for i in 1..4 {
        if v[i] > v[b] {
            b = i;
        }
    }
    b
}

/// `_order_card_quad(points)` (float32).
pub fn order_card_quad(p: &Quad) -> Quad {
    let sums: [f32; 4] = std::array::from_fn(|i| p[i][0] + p[i][1]);
    let diffs: [f32; 4] = std::array::from_fn(|i| p[i][1] - p[i][0]);
    let o = [p[argmin(&sums)], p[argmin(&diffs)], p[argmax(&sums)], p[argmax(&diffs)]];
    let w = norm2(sub(o[1], o[0])).max(norm2(sub(o[2], o[3])));
    let h = norm2(sub(o[2], o[1])).max(norm2(sub(o[3], o[0])));
    if w > h {
        [o[3], o[0], o[1], o[2]]
    } else {
        o
    }
}

/// `_expand_card_quad(quad)` with the default side/top/bottom.
fn expand_card_quad(q: &Quad) -> Quad {
    expand_card_quad_by(q, 1.0)
}

/// `expand_card_quad` with its margins scaled by `k` (1.0 = the full
/// border-pass margins, 0.0 = the outline itself).
fn expand_card_quad_by(q: &Quad, k: f32) -> Quad {
    let (side, top, bottom) = (0.015f32 * k, 0.01f32 * k, 0.07f32 * k);
    let [tl, tr, br, bl] = order_card_quad(q);
    let te = sub(tr, tl);
    let be = sub(br, bl);
    let le = sub(bl, tl);
    let re = sub(br, tr);
    let f = |base: [f32; 2], s1: f32, e1: [f32; 2], s2: f32, e2: [f32; 2]| -> [f32; 2] {
        std::array::from_fn(|k| (base[k] + s1 * e1[k]) + s2 * e2[k])
    };
    [
        f(tl, -side, te, -top, le),
        f(tr, side, te, -top, re),
        f(br, side, be, bottom, re),
        f(bl, -side, be, bottom, le),
    ]
}

// NOTE: numpy evaluates `tl - side*e - top*l` as `(tl - (side*e)) - (top*l)`;
// `base + (-s)*e` is bit-identical to `base - s*e` in IEEE arithmetic.

fn scale(q: &Quad, step: usize) -> Quad {
    let s = step as f32;
    std::array::from_fn(|i| [q[i][0] * s, q[i][1] * s])
}

fn clip(q: &mut Quad, w: usize, h: usize) {
    for p in q.iter_mut() {
        p[0] = p[0].clamp(0.0, (w - 1) as f32);
        p[1] = p[1].clamp(0.0, (h - 1) as f32);
    }
}

fn quad_box(q: &Quad) -> [i64; 4] {
    let minx = q.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min).floor() as i64;
    let miny = q.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min).floor() as i64;
    let maxx = q.iter().map(|p| p[0]).fold(f32::NEG_INFINITY, f32::max).ceil() as i64;
    let maxy = q.iter().map(|p| p[1]).fold(f32::NEG_INFINITY, f32::max).ceil() as i64;
    [minx, miny, maxx - minx, maxy - miny]
}

fn out_of_frame(q: &Quad, w: usize, h: usize) -> bool {
    q.iter().any(|p| p[0] < 0.0 || p[1] < 0.0 || p[0] >= w as f32 || p[1] >= h as f32)
}

/// One scene candidate: the JSON dict plus the numbers the merge reads.
#[derive(Clone, Debug)]
pub struct Cand {
    pub bx: [i64; 4],
    pub score: f64,
    /// The quad exactly as stored in the dict (what `_warp_card_quad` gets).
    pub quad: Option<[[f64; 2]; 4]>,
    pub json: Map<String, Value>,
}

impl Cand {
    fn area(&self) -> i64 {
        self.bx[2] * self.bx[3]
    }
}

fn quad_round_py(q: &Quad) -> [[f64; 2]; 4] {
    std::array::from_fn(|i| [py_round(q[i][0] as f64, 1), py_round(q[i][1] as f64, 1)])
}

fn quad_round_np(q: &Quad) -> [[f64; 2]; 4] {
    std::array::from_fn(|i| [np_round1_f32(q[i][0]), np_round1_f32(q[i][1])])
}

fn cand(bx: [i64; 4], quad: Option<[[f64; 2]; 4]>, score: f64, source: &str, anchor: &str, rotation: Option<f64>) -> Cand {
    let mut m = Map::new();
    m.insert("box".into(), json!(bx));
    if let Some(q) = quad {
        m.insert("quad".into(), json!(q.iter().map(|p| vec![p[0], p[1]]).collect::<Vec<_>>()));
    }
    m.insert("score".into(), json!(score));
    m.insert("source".into(), json!(source));
    m.insert("anchor".into(), json!(anchor));
    if let Some(r) = rotation {
        m.insert("rotation".into(), json!(r));
    }
    Cand { bx, score, quad, json: m }
}

fn step_of(w: usize, h: usize) -> usize {
    (w.max(h) as f64 / DETECT_MAX_DIM as f64).ceil().max(1.0) as usize
}

/// percentile-normalisation LUT (float64 math, astype(uint8) truncation).
fn norm_lut(low: f64, high: f64) -> [u8; 256] {
    std::array::from_fn(|i| ((i as f64 - low) * (255.0 / (high - low))).clamp(0.0, 255.0) as u8)
}

fn sort_desc(c: &mut [Cand]) {
    c.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap().then(b.area().cmp(&a.area())));
}

/// `(long_angle → correction)` used by rotated candidates and skew.
fn correction(rw: f32, rh: f32, angle: f32) -> f64 {
    let a = angle as f64;
    let long = if rw >= rh { a } else { a + 90.0 };
    if long < 0.0 {
        90.0 + long
    } else {
        long - 90.0
    }
}

fn rotated_scene_candidates(seg: &Gray, step: usize, fw: usize, fh: usize) -> Vec<Cand> {
    let frame_area = (seg.w * seg.h) as f64;
    let mask = morph(&mask_mat(seg, |v| v > HOLE_BRIGHT, 255), imgproc::MORPH_CLOSE, &ones(3), 1);
    let mut out = vec![];
    for c in contours(&mask, imgproc::RETR_LIST).iter() {
        let a = area(&c);
        let r = min_area_rect(&c);
        let (rw, rh) = (r.w as f64, r.h as f64);
        if rw.min(rh) <= 0.0 {
            continue;
        }
        let (short, long) = (rw.min(rh), rw.max(rh));
        let aspect = short / long;
        let rect_area = rw * rh;
        let fill = a / rect_area;
        if !(0.62..=0.82).contains(&aspect) || !(0.03 * frame_area <= rect_area && rect_area <= 0.20 * frame_area) || fill < 0.70 {
            continue;
        }
        let corr = correction(r.w, r.h, r.angle);
        if corr.abs() < 15.0 || corr.abs() > 75.0 {
            continue;
        }
        let mut q = scale(&expand_card_quad(&box_points(&r)), step);
        clip(&mut q, fw, fh);
        out.push(cand(quad_box(&q), Some(quad_round_py(&q)), py_round(4.25 + 0.15 * fill.min(1.0), 3), "rotated", "bright-face", Some(py_round(corr, 1))));
    }
    out
}

fn chroma_scene_candidates(img: &Rgb) -> Vec<Cand> {
    let step = step_of(img.w, img.h);
    let (w, h) = (img.w.div_ceil(step), img.h.div_ceil(step));
    let mut px = Vec::with_capacity(w * h);
    for y in (0..img.h).step_by(step) {
        for x in (0..img.w).step_by(step) {
            let o = (y * img.w + x) * 3;
            let (r, g, b) = (img.px[o] as i16, img.px[o + 1] as i16, img.px[o + 2] as i16);
            px.push(if r > g + 15 && r > b + 10 && r > 45 { 255u8 } else { 0 });
        }
    }
    let mask = morph(&mat_from(&px, h, w, 1), imgproc::MORPH_CLOSE, &ones(9), 1);
    let mask = morph(&mask, imgproc::MORPH_OPEN, &ones(3), 1);
    let frame_area = (w * h) as f64;
    let mut out = vec![];
    for c in contours(&mask, imgproc::RETR_EXTERNAL).iter() {
        let a = area(&c);
        let r = min_area_rect(&c);
        let (rw, rh) = (r.w as f64, r.h as f64);
        if rw.min(rh) <= 0.0 {
            continue;
        }
        let (short, long) = (rw.min(rh), rw.max(rh));
        let rect_area = rw * rh;
        let fill = a / rect_area;
        let rel = rect_area / frame_area;
        let aspect = short / long;
        let full_face = (0.62..=0.86).contains(&aspect) && (0.025..=0.10).contains(&rel) && fill >= 0.50;
        let art_anchor = (0.78..=0.92).contains(&aspect) && (0.018..0.028).contains(&rel) && fill >= 0.45;
        if !full_face && !art_anchor {
            continue;
        }
        let art = art_anchor && !full_face;
        let mut q: Quad = if art {
            let theta = (r.angle as f64 + 90.0).to_radians();
            let hz = [theta.cos() as f32, theta.sin() as f32];
            let vt = [(-theta.sin()) as f32, theta.cos() as f32];
            let card_w = long / 0.88;
            let card_h = card_w / CARD_AR;
            let k = (0.14 * card_h) as f32;
            let center = [r.cx + k * vt[0], r.cy + k * vt[1]];
            let (a5, b5) = ((0.5 * card_w) as f32, (0.5 * card_h) as f32);
            let pt = |sx: f32, sy: f32| -> [f32; 2] {
                std::array::from_fn(|i| (center[i] + sx * a5 * hz[i]) + sy * b5 * vt[i])
            };
            scale(&[pt(-1.0, -1.0), pt(1.0, -1.0), pt(1.0, 1.0), pt(-1.0, 1.0)], step)
        } else {
            scale(&expand_card_quad(&box_points(&r)), step)
        };
        clip(&mut q, img.w, img.h);
        out.push(cand(
            quad_box(&q),
            Some(quad_round_py(&q)),
            py_round((if art { 4.36 } else { 4.28 }) + 0.12 * fill.min(1.0), 3),
            if art { "chroma-art" } else { "chroma" },
            if art { "art-region" } else { "saturated-face" },
            Some(py_round(r.angle as f64, 1)),
        ));
    }
    out
}

fn closed_face_scene_candidates(img: &Rgb, gray_full: &Gray) -> Vec<Cand> {
    let step = step_of(img.w, img.h);
    let gray = gray_full.stride(step);
    let hist = gray.hist(0, gray.h as i64, 0, gray.w as i64);
    let (low, high) = (hist.pct(2.0), hist.pct(98.0));
    if high - low < 20.0 {
        return vec![];
    }
    let lut = norm_lut(low, high);
    let normalized = Gray { w: gray.w, h: gray.h, px: gray.px.iter().map(|&v| lut[v as usize]).collect() };
    let size = (gray.w * gray.h) as f64;
    let mut out = vec![];
    for t in [100u8, 125, 150] {
        let mask = morph(&mask_mat(&normalized, |v| v > t, 1), imgproc::MORPH_CLOSE, &ones(5), 1);
        for c in contours(&mask, imgproc::RETR_LIST).iter() {
            let r = min_area_rect(&c);
            let (w, h) = (r.w as f64, r.h as f64);
            if w.min(h) < 35.0 {
                continue;
            }
            let ar = w * h;
            let aspect = w.min(h) / w.max(h);
            let fill = area(&c) / ar;
            if !((0.66..=0.80).contains(&aspect) && fill >= 0.86 && (0.012..=0.15).contains(&(ar / size))) {
                continue;
            }
            let q = scale(&expand_card_quad(&box_points(&r)), step);
            if out_of_frame(&q, img.w, img.h) {
                continue;
            }
            out.push(cand(quad_box(&q), Some(quad_round_np(&q)), py_round(4.5 + 0.1 * fill, 3), "closed-face", "complete-face", None));
        }
    }
    out
}

fn border_scene_candidates(img: &Rgb, gray_full: &Gray) -> Vec<Cand> {
    let step = step_of(img.w, img.h);
    let gray = gray_full.stride(step);
    let size = (gray.w * gray.h) as f64;
    let mut out = vec![];
    let mut collect = |mask: &Mat, source: &str, min_fill: f64| {
        for c in contours(mask, imgproc::RETR_LIST).iter() {
            let r = min_area_rect(&c);
            let (w, h) = (r.w as f64, r.h as f64);
            if w.min(h) < 30.0 {
                continue;
            }
            let a = area(&c);
            let aspect = w.min(h) / w.max(h);
            let fill = a / (w * h);
            if !((0.62..=0.82).contains(&aspect) && fill >= min_fill && (0.003..=0.25).contains(&(a / size))) {
                continue;
            }
            let q = scale(&expand_card_quad(&box_points(&r)), step);
            if out_of_frame(&q, img.w, img.h) {
                continue;
            }
            out.push(cand(quad_box(&q), Some(quad_round_np(&q)), 4.45, source, "enclosing-border", None));
        }
    };
    for t in [25u8, 50, 75, 100, 125] {
        let m = morph(&mask_mat(&gray, |v| v < t, 1), imgproc::MORPH_CLOSE, &ones(3), 1);
        collect(&m, "border", 0.88);
    }
    let mut blur = Mat::default();
    imgproc::gaussian_blur_def(&gray.mat(), &mut blur, Size::new(3, 3), 0.0).unwrap();
    let mut edges = Mat::default();
    imgproc::canny(&blur, &mut edges, 50.0, 150.0, 3, false).unwrap();
    collect(&edges, "edge-open", 0.88);
    // HSV of the strided RGB plane
    let (w, h) = (gray.w, gray.h);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in (0..img.h).step_by(step) {
        for x in (0..img.w).step_by(step) {
            let o = (y * img.w + x) * 3;
            rgb.extend_from_slice(&img.px[o..o + 3]);
        }
    }
    let mut hsv = Mat::default();
    imgproc::cvt_color_def(&mat_from(&rgb, h, w, 3), &mut hsv, imgproc::COLOR_RGB2HSV).unwrap();
    let hb = mat_bytes(&hsv);
    let px: Vec<u8> = hb
        .chunks_exact(3)
        .map(|p| u8::from((p[0] < 10 || p[0] > 170) && p[1] > 100 && p[2] > 30))
        .collect();
    let m = morph(&mat_from(&px, h, w, 1), imgproc::MORPH_CLOSE, &ones(5), 1);
    collect(&m, "saturated-hue", 0.55);
    out
}

/// Largest outer-boundary contour, as a fraction of the frame. Cards with
/// no dark border or frame (Secret Lair "The Zeta Set": cream paper edge to
/// edge) are found only by this edge pass, so it must also accept a card
/// held close: a 600 px tall card in a 1920x1080 frame is ~12% of it.
const EDGE_MAX_FRAME_FRAC: f64 = 0.40;

fn edge_scene_candidates(img: &Rgb, gray_full: &Gray) -> Vec<Cand> {
    let step = step_of(img.w, img.h);
    let gray = gray_full.stride(step);
    let hist = gray.hist(0, gray.h as i64, 0, gray.w as i64);
    let (low, high) = (hist.pct(2.0), hist.pct(98.0));
    if high - low < 20.0 {
        return vec![];
    }
    let lut = norm_lut(low, high);
    let normalized = Gray { w: gray.w, h: gray.h, px: gray.px.iter().map(|&v| lut[v as usize]).collect() };
    let mut blur = Mat::default();
    imgproc::gaussian_blur_def(&normalized.mat(), &mut blur, Size::new(5, 5), 0.0).unwrap();
    let mut edges = Mat::default();
    imgproc::canny(&blur, &mut edges, 12.0, 36.0, 3, false).unwrap();
    let edges = morph(&edges, imgproc::MORPH_CLOSE, &ones(5), 1);
    let frame_area = (gray.w * gray.h) as f64;
    let mut out = vec![];
    for c in contours(&edges, imgproc::RETR_LIST).iter() {
        let a = area(&c);
        let r = min_area_rect(&c);
        let (rw, rh) = (r.w as f64, r.h as f64);
        if rw.min(rh) <= 0.0 {
            continue;
        }
        let (short, long) = (rw.min(rh), rw.max(rh));
        let ra = rw * rh;
        let fill = a / ra;
        if !(0.62..=0.90).contains(&(short / long)) || !(0.025 * frame_area <= ra && ra <= EDGE_MAX_FRAME_FRAC * frame_area) || fill < 0.55 {
            continue;
        }
        let mut q = scale(&expand_card_quad(&box_points(&r)), step);
        let mut anchor = "outer-boundary";
        if quad_in_edge_band(&q, img.w, img.h) {
            // The outer boundary IS the card edge (no inner border to grow
            // from), so the border-pass expansion is only margin. When that
            // margin alone reaches the frame edge band, use the tight outline
            // instead of a clipped quad that would be rejected (or read from
            // replicated border pixels) as "touches frame edge".
            // Keep as much of the margin as fits: the footer/title strip rows
            // are tuned on the expanded crop.
            for k in [0.75f32, 0.5, 0.25, 0.0] {
                let t = scale(&expand_card_quad_by(&box_points(&r), k), step);
                if !quad_in_edge_band(&t, img.w, img.h) {
                    q = t;
                    anchor = "outer-boundary-tight";
                    break;
                }
            }
        }
        clip(&mut q, img.w, img.h);
        out.push(cand(quad_box(&q), Some(quad_round_py(&q)), py_round(4.30 + 0.08 * fill.min(1.0), 3), "edge", anchor, Some(py_round(r.angle as f64, 1))));
    }
    out
}

/// Any corner inside the 1% frame-edge band `detect_uploaded_scene` rejects.
fn quad_in_edge_band(q: &Quad, w: usize, h: usize) -> bool {
    let ex = 2.max((0.01 * w as f64) as i64) as f32;
    let ey = 2.max((0.01 * h as f64) as i64) as f32;
    q.iter().any(|p| p[0] <= ex || p[1] <= ey || p[0] >= w as f32 - ex || p[1] >= h as f32 - ey)
}

fn touches_edge(b: &[i64; 4], fw: i64, fh: i64) -> bool {
    let (ex, ey) = (2.max((0.01 * fw as f64) as i64), 2.max((0.01 * fh as f64) as i64));
    b[0] <= ex || b[1] <= ey || b[0] + b[2] >= fw - ex || b[1] + b[3] >= fh - ey
}

fn drop_art_windows(c: Vec<Cand>) -> Vec<Cand> {
    let keep: Vec<bool> = c
        .iter()
        .map(|cd| {
            let [bx, by, bw, bh] = cd.bx;
            if bw <= bh {
                return true;
            }
            for o in &c {
                let [ox, oy, ow, oh] = o.bx;
                if oh <= ow || ((ow * oh) as f64) < 1.8 * (bw * bh) as f64 {
                    continue;
                }
                let ix = 0.max((bx + bw).min(ox + ow) - bx.max(ox));
                let iy = 0.max((by + bh).min(oy + oh) - by.max(oy));
                if (ix * iy) as f64 >= 0.9 * (bw * bh) as f64 {
                    return false;
                }
            }
            true
        })
        .collect();
    c.into_iter().zip(keep).filter(|(_, k)| *k).map(|(c, _)| c).collect()
}

/// A quad-backed candidate whose box has card proportions (portrait or
/// landscape, a few degrees of tilt).
fn card_shaped(c: &Cand) -> bool {
    let [_, _, w, h] = c.bx;
    if c.quad.is_none() || w <= 0 || h <= 0 {
        return false;
    }
    let r = w.min(h) as f64 / w.max(h) as f64;
    (0.60..=0.86).contains(&r)
}

fn overlap(a: &[i64; 4], b: &[i64; 4]) -> (i64, i64, i64) {
    let [bx, by, bw, bh] = *a;
    let [ex, ey, ew, eh] = *b;
    let inter = 0.max((bx + bw).min(ex + ew) - bx.max(ex)) * 0.max((by + bh).min(ey + eh) - by.max(ey));
    let union = bw * bh + ew * eh - inter;
    let smaller = (bw * bh).min(ew * eh);
    (inter, union, smaller)
}

/// `_merge_scene_candidates(candidates, limit=8, frame)`.
fn merge(c: Vec<Cand>, fw: i64, fh: i64) -> Vec<Cand> {
    let mut c = drop_art_windows(c);
    sort_desc(&mut c);
    let mut sel: Vec<Cand> = vec![];
    for cd in c {
        let [bx, _by, bw, bh] = cd.bx;
        let mut dup = false;
        for i in 0..sel.len() {
            let e = &sel[i];
            let [ex, _ey, ew, eh] = e.bx;
            let (inter, union, smaller) = overlap(&cd.bx, &e.bx);
            let (bwf, bhf) = (bw as f64, bh as f64);
            let similar = (0.70..=1.30).contains(&(bwf / ew.max(1) as f64)) && (0.70..=1.30).contains(&(bhf / eh.max(1) as f64));
            let twin = similar
                && ((bx as f64 + bwf / 2.0) - (ex as f64 + ew as f64 / 2.0)).abs() <= 0.20 * bw.max(ew) as f64
                && smaller != 0
                && inter as f64 / smaller as f64 >= 0.35;
            if (union != 0 && inter as f64 / union as f64 >= 0.45) || (smaller != 0 && inter as f64 / smaller as f64 >= 0.75) || twin {
                let edge_over_inner = touches_edge(&e.bx, fw, fh) && !touches_edge(&cd.bx, fw, fh);
                if edge_over_inner && (bw * bh) as f64 >= 0.75 * (ew * eh) as f64 && cd.score >= e.score - 0.2 {
                    dup = true;
                    // selected.index(existing): first equal dict — the same slot
                    let first = sel.iter().position(|s| s.json == e.json).unwrap_or(i);
                    sel[first] = cd.clone();
                    break;
                }
                if edge_over_inner && card_shaped(&cd) {
                    // Edge status is per candidate: an outline that grew into
                    // the frame edge (light card on a light table) must not
                    // swallow a smaller, in-frame, card-shaped outline of the
                    // same card. Keep evaluating it as its own candidate.
                    continue;
                }
                dup = true;
                break;
            }
        }
        if !dup {
            sel.push(cd);
            if sel.len() >= 8 {
                break;
            }
        }
    }
    sel.sort_by(|a, b| (a.bx[0], a.bx[1]).cmp(&(b.bx[0], b.bx[1])));
    sel
}

fn card_candidates_from_hole(x0: f64, y0: f64, w: f64, h: f64) -> Vec<(f64, f64, f64, f64, &'static str)> {
    let mut c = vec![];
    let ar = w / h;
    if (0.68..=0.88).contains(&ar) {
        let cw = w / 0.96;
        let ch = cw / CARD_AR;
        c.push((x0 - 0.02 * cw, y0 - 0.04 * ch, cw, ch, "face"));
    }
    if ar > 2.2 {
        let cw = w / 0.90;
        let ch = cw / CARD_AR;
        c.push((x0 - 0.05 * cw, y0 - 0.055 * ch, cw, ch, "banner"));
        c.push((x0 - 0.05 * cw, y0 - 0.60 * ch, cw, ch, "type"));
    } else if ar > 1.25 {
        let cw = w / 0.86;
        let ch = cw / CARD_AR;
        c.push((x0 - 0.07 * cw, y0 - 0.14 * ch, cw, ch, "art"));
    } else {
        let cw = w / 0.92;
        let ch = cw / CARD_AR;
        c.push((x0 - 0.04 * cw, y0 - 0.17 * ch, cw, ch, "text"));
    }
    c
}

fn card_likeness(g: &Gray, x0: f64, y0: f64, w: f64, h: f64, vertical: bool) -> f64 {
    let (x0, y0, w, h) = (x0 as i64, y0 as i64, w as i64, h as i64);
    let z = g.hist(y0, y0 + h, x0, x0 + w);
    if z.n < 400 || w <= 0 || h <= 0 {
        return 0.0;
    }
    let bright = z.frac_where(|v| v > 180);
    let dark = z.frac_where(|v| v < 100);
    let contrast = z.pct(90.0) - z.pct(10.0);
    let t3 = 1.max(h / 3);
    let lower = g.hist(y0 + 2 * t3, y0 + h, x0, x0 + w);
    let upper = g.hist(y0, y0 + t3.min(h), x0, x0 + w);
    let structure = lower.mean() - upper.mean();
    if bright > 0.82 || dark < 0.05 || contrast < 35.0 || (vertical && structure < 10.0) {
        return 0.0;
    }
    let aspect = (1.0 - ((w as f64 / h as f64) - CARD_AR).abs() / 0.30).max(0.0);
    let ss = if vertical { 0.25 * (structure / 80.0).min(1.0) } else { 0.0 };
    3.0 + 0.20 * (contrast / 100.0).min(1.0) + 0.20 * (dark / 0.45).min(1.0) + ss + 0.25 * aspect
}

fn components(mask: &Mat) -> Vec<[i32; 5]> {
    let (mut labels, mut stats, mut cent) = (Mat::default(), Mat::default(), Mat::default());
    let n = imgproc::connected_components_with_stats(mask, &mut labels, &mut stats, &mut cent, 4, cvc::CV_32S).unwrap();
    (1..n).map(|i| std::array::from_fn(|k| *stats.at_2d::<i32>(i, k as i32).unwrap())).collect()
}

/// `detect_card_bbox(gray_full, all_candidates=True)`.
pub fn detect_card_bbox_all(gray_full: &Gray) -> Vec<Cand> {
    if gray_full.w.min(gray_full.h) < 80 {
        return vec![];
    }
    let step = step_of(gray_full.w, gray_full.h);
    let g = gray_full.stride(step);
    let (hh, ww) = (g.h as f64, g.w as f64);
    let frame_area = hh * ww;
    let hist = g.hist(0, g.h as i64, 0, g.w as i64);
    let (low, high) = (hist.pct(2.0), hist.pct(98.0));
    if high - low < 20.0 {
        return vec![];
    }
    let lut = norm_lut(low, high);
    let seg = Gray { w: g.w, h: g.h, px: g.px.iter().map(|&v| lut[v as usize]).collect() };
    let mut cands: Vec<(f64, f64, f64, f64, &str, &str)> = vec![];
    let min_comp = 180f64.max(0.0005 * frame_area);
    for [x0, y0, w, h, size] in components(&mask_mat(&seg, |v| v < 105, 1)) {
        let (x0, y0, w, h, size) = (x0 as f64, y0 as f64, w as f64, h as f64, size as f64);
        if size < min_comp {
            continue;
        }
        let (x1, y1) = (x0 + w, y0 + h);
        if w < 35f64.max(0.07 * ww) || h < 50f64.max(0.10 * hh) || w > 0.90 * ww || h > 0.95 * hh {
            continue;
        }
        if !(0.48..=0.98).contains(&(w / h)) || size / (w * h) < 0.34 {
            continue;
        }
        if x0 < 0.008 * ww || x1 > 0.992 * ww || y0 < 0.008 * hh || y1 > 0.998 * hh {
            continue;
        }
        cands.push((x0, y0, w, h, "dark", "silhouette"));
    }
    for [x0, y0, w, h, size] in components(&mask_mat(&seg, |v| v > HOLE_BRIGHT, 1)) {
        let (x0, y0, w, h, size) = (x0 as f64, y0 as f64, w as f64, h as f64, size as f64);
        let (x1, y1) = (x0 + w, y0 + h);
        if w < 8f64.max(0.012 * ww) || h < 6f64.max(0.008 * hh) {
            continue;
        }
        if y0 == 0.0 || x0 == 0.0 || y1 == hh || x1 == ww || size / (w * h) < 0.30 {
            continue;
        }
        for (cx0, cy0, cw, ch, anchor) in card_candidates_from_hole(x0, y0, w, h) {
            if cx0 < 2.0 || cy0 < 2.0 || cx0 + cw > ww - 2.0 || cy0 + ch > hh - 2.0 {
                continue;
            }
            if cw < 35f64.max(0.07 * ww) || ch < 50f64.max(0.10 * hh) || cw > 0.95 * ww || ch > 0.98 * hh || cw * ch < 0.006 * frame_area {
                continue;
            }
            if anchor == "banner" {
                let r0 = (cy0 + 0.010 * ch) as i64;
                let r1 = (r0 + 1).max((cy0 + 0.052 * ch) as i64);
                let top = seg.hist(r0, r1, (cx0 + 0.08 * cw) as i64, (cx0 + 0.92 * cw) as i64);
                if top.n > 0 && top.frac_where(|v| v < 110) < 0.08 && top.mean() > 125.0 {
                    continue;
                }
            }
            cands.push((cx0, cy0, cw, ch, "hole", anchor));
        }
    }
    if (0.58..=0.85).contains(&(ww / hh)) {
        cands.push((0.0, 0.0, ww, hh, "frame", "full"));
    }
    let s = step as f64;
    let mut ranked: Vec<(f64, f64, Cand)> = vec![];
    for (x0, y0, w, h, source, anchor) in cands {
        let face = anchor == "face";
        let lk = card_likeness(&g, x0, y0, w, h, !face);
        if lk < 3.0 {
            continue;
        }
        let rel = if face { 0.55 } else if source == "dark" { 0.38 } else { 0.0 };
        let key = lk + rel;
        let x = 0.max(py_round_int(x0 * s));
        let y = 0.max(py_round_int(y0 * s));
        let x1 = (gray_full.w as i64).min(py_round_int((x0 + w) * s));
        let y1 = (gray_full.h as i64).min(py_round_int((y0 + h) * s));
        ranked.push((key, w * h, cand([x, y, x1 - x, y1 - y], None, py_round(key, 3), source, anchor, None)));
    }
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(b.1.partial_cmp(&a.1).unwrap()));
    let mut opts: Vec<Cand> = ranked.into_iter().filter(|r| r.0 >= 3.88).map(|r| r.2).collect();
    opts.extend(rotated_scene_candidates(&seg, step, gray_full.w, gray_full.h));
    sort_desc(&mut opts);
    let mut sel: Vec<Cand> = vec![];
    for o in opts {
        let dup = sel.iter().any(|e| {
            let (i, u, sm) = overlap(&o.bx, &e.bx);
            (u != 0 && i as f64 / u as f64 >= 0.45) || (sm != 0 && i as f64 / sm as f64 >= 0.75)
        });
        if dup {
            continue;
        }
        sel.push(o);
        if sel.len() >= 8 {
            break;
        }
    }
    sel.sort_by(|a, b| (a.bx[0], a.bx[1]).cmp(&(b.bx[0], b.bx[1])));
    sel
}

/// The candidate list `detect_uploaded_scene` assesses.
pub fn scene_candidates(img: &Rgb, gray: &Gray) -> Vec<Cand> {
    let mut all = detect_card_bbox_all(gray);
    all.extend(closed_face_scene_candidates(img, gray));
    all.extend(border_scene_candidates(img, gray));
    all.extend(chroma_scene_candidates(img));
    all.extend(edge_scene_candidates(img, gray));
    merge(all, img.w as i64, img.h as i64)
}

/// `_warp_card_quad(image, quad)`.
pub fn warp_card_quad(img: &Rgb, quad: &[[f64; 2]; 4]) -> Result<Rgb, String> {
    let q: Quad = std::array::from_fn(|i| [quad[i][0] as f32, quad[i][1] as f32]);
    let o = order_card_quad(&q);
    let ow = py_round_int(norm2(sub(o[1], o[0])).max(norm2(sub(o[2], o[3]))) as f64);
    let oh = py_round_int(norm2(sub(o[2], o[1])).max(norm2(sub(o[3], o[0]))) as f64);
    if ow < 40 || oh < 60 {
        return Err("card quadrilateral is too small".into());
    }
    warp(img, &o, ow, oh, cvc::BORDER_REPLICATE, Scalar::default())
}

pub fn warp(img: &Rgb, o: &Quad, ow: i64, oh: i64, border: i32, value: Scalar) -> Result<Rgb, String> {
    let src: Vector<Point2f> = o.iter().map(|p| Point2f::new(p[0], p[1])).collect();
    let (wf, hf) = ((ow - 1) as f32, (oh - 1) as f32);
    let dst: Vector<Point2f> = [(0.0, 0.0), (wf, 0.0), (wf, hf), (0.0, hf)].iter().map(|&(x, y)| Point2f::new(x, y)).collect();
    let m = imgproc::get_perspective_transform(&src, &dst, cvc::DECOMP_LU).map_err(|e| e.to_string())?;
    let mut inv = Mat::default();
    cvc::invert(&m, &mut inv, cvc::DECOMP_LU).map_err(|e| e.to_string())?;
    let mut mi = [0f64; 9];
    for (k, v) in mi.iter_mut().enumerate() {
        *v = *inv.at_2d::<f64>((k / 3) as i32, (k % 3) as i32).map_err(|e| e.to_string())?;
    }
    let bval = if border == cvc::BORDER_CONSTANT { Some([value[0] as u8, value[1] as u8, value[2] as u8]) } else { None };
    Ok(warp_cubic_cv5(img, &mi, ow as usize, oh as usize, bval))
}

/// OpenCV 5.0 `warpPerspective(INTER_CUBIC)` for 8UC3, i.e. the new
/// `genericWarp` + `bicubicVec` kernel (warp_kernels.simd.hpp) as dispatched
/// on AVX2 (8 float lanes, FMA, round-half-even). OpenCV 4.10 still uses the
/// old fixed-point table remap, which differs by ±1-2 levels per pixel and
/// flipped 2/152 recognised cards on the differential corpus, so this one
/// kernel is ported instead of linked. `minv` = inverted transform
/// (dst→src). `bval` = Some(BORDER_CONSTANT value) else BORDER_REPLICATE.
pub fn warp_cubic_cv5(img: &Rgb, minv: &[f64; 9], ow: usize, oh: usize, bval: Option<[u8; 3]>) -> Rgb {
    const BATCH: usize = 8;
    let m: Vec<f32> = minv.iter().map(|&v| v as f32).collect();
    let (w, h) = (img.w as i32, img.h as i32);
    let (bw, bh) = (w.max(16), h.max(16));
    let (minx, maxx, miny, maxy) = (-(bw as f32), (bw * 2) as f32, -(bh as f32), (bh * 2) as f32);
    let a = -0.75f32;
    let weights = |al: f32| {
        let a2 = al * al;
        let b = 1.0 - al;
        let b2 = b * b;
        let w0 = a * (al * b2);
        let w3 = a * (a2 * b);
        let w1 = a2.mul_add((a + 2.0).mul_add(al, -(a + 3.0)), 1.0);
        let w2 = ((1.0 - w0) - w1) - w3;
        [w0, w1, w2, w3]
    };
    let px = |x: i32, y: i32, c: usize| -> f32 {
        match bval {
            None => {
                let (x, y) = (x.clamp(0, w - 1), y.clamp(0, h - 1));
                img.px[(y as usize * img.w + x as usize) * 3 + c] as f32
            }
            Some(bv) => {
                if x < 0 || y < 0 || x >= w || y >= h {
                    bv[c] as f32
                } else {
                    img.px[(y as usize * img.w + x as usize) * 3 + c] as f32
                }
            }
        }
    };
    let mut out = vec![0u8; ow * oh * 3];
    let mut xs = vec![0f32; ow];
    let mut ys = vec![0f32; ow];
    for y in 0..oh {
        let mx = (y as f32) * m[1] + m[2];
        let my = (y as f32) * m[4] + m[5];
        let mz = (y as f32) * m[7] + m[8];
        for x in 0..ow {
            let xf = x as f64;
            let invz = 1.0 / (mz as f64 + m[6] as f64 * xf);
            xs[x] = ((mx as f64 + m[0] as f64 * xf) * invz) as f32;
            ys[x] = ((my as f64 + m[3] as f64 * xf) * invz) as f32;
        }
        let row = &mut out[y * ow * 3..(y + 1) * ow * 3];
        let mut i = 0usize;
        while i < ow {
            if i + BATCH > ow && i > 0 {
                i = ow - BATCH;
            }
            let dlen = BATCH.min(ow - i);
            let lane = |k: usize| -> (f32, f32) { if k < dlen { (xs[i + k], ys[i + k]) } else { (0.0, 0.0) } };
            if let Some(bv) = bval {
                let all_out = (0..BATCH).all(|k| {
                    let (vx, vy) = lane(k);
                    let ix = vx.clamp(minx, maxx).floor() as i32 - 1;
                    let iy = vy.clamp(miny, maxy).floor() as i32 - 1;
                    ((ix + 4) as u32) >= (w + 4) as u32 || ((iy + 4) as u32) >= (h + 4) as u32
                });
                if all_out {
                    for k in 0..dlen {
                        row[(i + k) * 3..(i + k) * 3 + 3].copy_from_slice(&bv);
                    }
                    i += BATCH;
                    continue;
                }
            }
            for k in 0..dlen {
                let (vx, vy) = lane(k);
                let (vx, vy) = (vx.clamp(minx, maxx), vy.clamp(miny, maxy));
                let (fx, fy) = (vx.floor() as i32, vy.floor() as i32);
                let wx = weights(vx - fx as f32);
                let wy = weights(vy - fy as f32);
                let (x0, y0) = (fx - 1, fy - 1);
                for c in 0..3 {
                    let mut acc = 0f32;
                    for r in 0..4 {
                        let yy = y0 + r as i32;
                        let v0 = px(x0, yy, c);
                        let mut sw = v0 * wx[0];
                        sw = px(x0 + 1, yy, c).mul_add(wx[1], sw);
                        sw = px(x0 + 2, yy, c).mul_add(wx[2], sw);
                        sw = px(x0 + 3, yy, c).mul_add(wx[3], sw);
                        acc = sw.mul_add(wy[r], acc);
                    }
                    row[(i + k) * 3 + c] = (acc.round_ties_even() as i32).clamp(0, 255) as u8;
                }
            }
            i += BATCH;
        }
    }
    Rgb { w: ow, h: oh, px: out }
}

/// `box_sharpness(gray, (0, 0, w, h))` on a whole card plane.
pub fn box_sharpness(g: &Gray) -> f64 {
    if g.w < 4 || g.h < 4 {
        return 0.0;
    }
    let mut lap = Vec::with_capacity((g.w - 2) * (g.h - 2));
    for y in 1..g.h - 1 {
        for x in 1..g.w - 1 {
            let c = g.at(x, y) as f32;
            let l = c * 4.0 - (((g.at(x, y - 1) as f32 + g.at(x, y + 1) as f32) + g.at(x - 1, y) as f32) + g.at(x + 1, y) as f32);
            lap.push(l * l);
        }
    }
    super::num::mean_f32(&lap)
}

fn dark_rect_candidates(g: &Gray, w: usize, h: usize) -> Vec<(f64, RRect)> {
    let mask = mask_mat(g, |v| v < 120, 255);
    let k = 5.max(13.min(py_round_int(w.min(h) as f64 / 120.0))) as i32;
    let kernel = imgproc::get_structuring_element(imgproc::MORPH_RECT, Size::new(k, k), Point::new(-1, -1)).unwrap();
    let mask = morph(&mask, imgproc::MORPH_CLOSE, &kernel, 2);
    let mut cs: Vec<(f64, Vector<Point>)> = contours(&mask, imgproc::RETR_EXTERNAL).iter().map(|c| (area(&c), c)).collect();
    cs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    let mut out = vec![];
    for (a, c) in cs {
        if a < 0.20 * (w * h) as f64 {
            break;
        }
        let r = min_area_rect(&c);
        let (rw, rh) = (r.w as f64, r.h as f64);
        if rw.min(rh) <= 0.0 {
            continue;
        }
        let rect = a / (rw * rh);
        let aspect = rw.min(rh) / rw.max(rh);
        if rect < 0.75 || !(0.58..=0.82).contains(&aspect) {
            continue;
        }
        out.push((a, r));
    }
    out
}

/// `estimate_card_skew(gray, (0, 0, w, h))` on a whole card plane.
pub fn estimate_card_skew(g: &Gray) -> f64 {
    let (w, h) = (g.w, g.h);
    let zh = (8usize.max((0.20 * h as f64) as usize)).min(h);
    let stride = 2.max(w / 180);
    let (mut xs, mut ys) = (vec![], vec![]);
    let mut col = (0.08 * w as f64) as usize;
    let end = (0.92 * w as f64) as usize;
    while col < end {
        if let Some(y) = (0..zh).find(|&y| g.at(col, y) < 80) {
            xs.push(col as f32);
            ys.push(y as f32);
        }
        col += stride;
    }
    if xs.len() < 20 {
        return 0.0;
    }
    let yd: Vec<f64> = ys.iter().map(|&v| v as f64).collect();
    let (low, high) = (super::num::percentile_f64(&yd, 15.0), super::num::percentile_f64(&yd, 85.0));
    let keep: Vec<usize> = (0..ys.len()).filter(|&i| ys[i] as f64 >= low && ys[i] as f64 <= high).collect();
    if keep.len() < 12 {
        return 0.0;
    }
    let slope = polyfit1(&keep.iter().map(|&i| xs[i]).collect::<Vec<_>>(), &keep.iter().map(|&i| ys[i]).collect::<Vec<_>>());
    let angle = slope.atan().to_degrees();
    if angle.abs() >= 3.0 {
        for (_a, r) in dark_rect_candidates(g, w, h) {
            let corr = correction(r.w, r.h, r.angle);
            if corr.abs() <= 15.0 {
                return corr;
            }
        }
    }
    angle.clamp(-4.0, 4.0)
}

/// `float(np.polyfit(x_f32, y_f32, 1)[0])`: scaled Vandermonde in float32,
/// least squares in double, coefficients cast back to float32.
fn polyfit1(x: &[f32], y: &[f32]) -> f64 {
    let (mut s0, mut s1) = (0f32, 0f32);
    for &v in x {
        s0 += v * v;
        s1 += 1.0;
    }
    let (c0, c1) = (s0.sqrt(), s1.sqrt());
    let a: Vec<(f64, f64)> = x.iter().map(|&v| ((v / c0) as f64, (1.0f32 / c1) as f64)).collect();
    let (mut aa, mut ab, mut bb, mut ay, mut by) = (0f64, 0f64, 0f64, 0f64, 0f64);
    for (i, &(p, q)) in a.iter().enumerate() {
        let yy = y[i] as f64;
        aa += p * p;
        ab += p * q;
        bb += q * q;
        ay += p * yy;
        by += q * yy;
    }
    let det = aa * bb - ab * ab;
    let k0 = (ay * bb - ab * by) / det;
    ((k0 as f32) / c0) as f64
}

/// `rectify_rotated_card(crop, skew)` → (image, rectified).
pub fn rectify_rotated_card(img: &Rgb, skew: f64) -> (Option<Rgb>, bool) {
    if skew.abs() < 4.0 {
        return (None, false);
    }
    // cv2.cvtColor(RGB2GRAY) — OpenCV's own weights, not Pillow's
    let mut gm = Mat::default();
    imgproc::cvt_color_def(&mat_from(&img.px, img.h, img.w, 3), &mut gm, imgproc::COLOR_RGB2GRAY).unwrap();
    let g = Gray { w: img.w, h: img.h, px: mat_bytes(&gm) };
    let Some((_a, r)) = dark_rect_candidates(&g, img.w, img.h).into_iter().next() else {
        return (None, false);
    };
    let p = box_points(&r);
    let sums: [f32; 4] = std::array::from_fn(|i| p[i][0] + p[i][1]);
    let diffs: [f32; 4] = std::array::from_fn(|i| p[i][1] - p[i][0]);
    let [tl, tr, br, bl] = [p[argmin(&sums)], p[argmin(&diffs)], p[argmax(&sums)], p[argmax(&diffs)]];
    let mut ow = py_round_int(norm2(sub(br, bl)).max(norm2(sub(tr, tl))) as f64);
    let mut oh = py_round_int(norm2(sub(tr, br)).max(norm2(sub(tl, bl))) as f64);
    let mut o = [tl, tr, br, bl];
    if ow > oh {
        o = [bl, tl, tr, br];
        std::mem::swap(&mut ow, &mut oh);
    }
    if ow < 100 || oh < 140 {
        return (None, false);
    }
    match warp(img, &o, ow, oh, cvc::BORDER_CONSTANT, Scalar::new(255.0, 255.0, 255.0, 0.0)) {
        Ok(r) => (Some(r), true),
        Err(_) => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_quad_portrait_and_landscape() {
        let q: Quad = [[10.0, 10.0], [110.0, 10.0], [110.0, 150.0], [10.0, 150.0]];
        assert_eq!(order_card_quad(&q), q);
        let l: Quad = [[0.0, 0.0], [150.0, 0.0], [150.0, 100.0], [0.0, 100.0]];
        assert_eq!(order_card_quad(&l), [[0.0, 100.0], [0.0, 0.0], [150.0, 0.0], [150.0, 100.0]]);
    }

    #[test]
    fn pil_gray_weights() {
        let img = Rgb { w: 2, h: 1, px: vec![255, 0, 0, 10, 200, 30] };
        let g = Gray::pil_of(&img);
        assert_eq!(g.px, vec![76, 124]); // (10*19595+200*38470+30*7471+0x8000)>>16
    }

    #[test]
    fn synthetic_card_is_detected_and_warped() {
        // dark-bordered portrait card with a bright face on a mid-gray table
        let (w, h) = (900usize, 700usize);
        let mut img = Rgb { w, h, px: vec![128; w * h * 3] };
        let put = |img: &mut Rgb, x0: usize, y0: usize, x1: usize, y1: usize, v: u8| {
            for y in y0..y1 {
                for x in x0..x1 {
                    let o = (y * img.w + x) * 3;
                    img.px[o..o + 3].copy_from_slice(&[v, v, v]);
                }
            }
        };
        put(&mut img, 300, 150, 300 + 286, 150 + 400, 20);
        put(&mut img, 315, 165, 300 + 271, 150 + 330, 235);
        put(&mut img, 330, 200, 300 + 256, 150 + 200, 60);
        let g = Gray::pil_of(&img);
        let c = scene_candidates(&img, &g);
        assert!(!c.is_empty());
        let best = &c[0];
        assert!(best.bx[0] >= 280 && best.bx[0] <= 320, "{:?}", best.bx);
        if let Some(q) = best.quad {
            let warped = warp_card_quad(&img, &q).unwrap();
            assert!(warped.h > warped.w);
        }
    }
}

#[cfg(test)]
mod cv5_parity_tests {
    //! Goldens produced by Python cv2 5.0.0 in cardscan:local-scanspeed
    //! (tests/fixtures/scan_engine; regenerate with bench/scan-engine/README).
    use super::*;

    fn fx(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scan_engine").join(name)
    }

    fn golden() -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(fx("golden.json")).unwrap()).unwrap()
    }

    fn quad(v: &serde_json::Value) -> Quad {
        std::array::from_fn(|i| [v[i][0].as_f64().unwrap() as f32, v[i][1].as_f64().unwrap() as f32])
    }

    #[test]
    fn warp_cubic_matches_cv5_bytes() {
        let im = image::open(fx("src.png")).unwrap().to_rgb8();
        let img = Rgb { w: im.width() as usize, h: im.height() as usize, px: im.into_raw() };
        let g = golden();
        let white = Scalar::new(255.0, 255.0, 255.0, 0.0);
        for (q, border, val, file) in [
            (&g["quad"], cvc::BORDER_REPLICATE, Scalar::default(), "warp_replicate.rgb"),
            (&g["quad"], cvc::BORDER_CONSTANT, white, "warp_const.rgb"),
            (&g["quad_out"], cvc::BORDER_CONSTANT, white, "warp_const_out.rgb"),
        ] {
            let out = warp(&img, &quad(q), 144, 200, border, val).unwrap();
            let want = std::fs::read(fx(file)).unwrap();
            let bad = out.px.iter().zip(&want).filter(|(a, b)| a != b).count();
            assert_eq!(bad, 0, "{file}: {bad} bytes differ from cv2 5.0");
        }
    }

    #[test]
    fn min_area_rect_uses_cv5_convention() {
        for r in golden()["rects"].as_array().unwrap() {
            let pts: Vector<Point> = r["pts"].as_array().unwrap().iter().map(|p| Point::new(p[0].as_i64().unwrap() as i32, p[1].as_i64().unwrap() as i32)).collect();
            let m = min_area_rect(&pts);
            let f = |v: &serde_json::Value| v.as_f64().unwrap() as f32;
            assert_eq!((m.cx, m.cy), (f(&r["c"][0]), f(&r["c"][1])), "{r}");
            assert_eq!((m.w, m.h), (f(&r["wh"][0]), f(&r["wh"][1])), "{r}");
            assert!((m.angle - f(&r["angle"])).abs() < 1e-4, "{r}: {}", m.angle);
        }
    }
}

#[cfg(test)]
mod merge_edge_tests {
    use super::*;

    fn q(b: [i64; 4]) -> Option<[[f64; 2]; 4]> {
        let [x, y, w, h] = b.map(|v| v as f64);
        Some([[x, y], [x + w, y], [x + w, y + h], [x, y + h]])
    }

    /// Edge status is per candidate: an outline grown into the frame edge
    /// must not swallow a smaller in-frame card outline of the same card.
    #[test]
    fn edge_touching_outline_keeps_inner_card_candidate() {
        let big = [10, 60, 760, 1000]; // touches the left 1% band
        let inner = [120, 160, 470, 660]; // card-shaped, well inside
        let c = vec![cand(big, q(big), 4.40, "edge", "outer-boundary", None), cand(inner, q(inner), 4.30, "border", "enclosing-border", None)];
        let out = merge(c, 1920, 1080);
        assert!(out.iter().any(|c| c.bx == inner), "{:?}", out.iter().map(|c| c.bx).collect::<Vec<_>>());
        // a non-card-shaped inner box (art window) is still a duplicate
        let art = [120, 160, 470, 300];
        let c = vec![cand(big, q(big), 4.40, "edge", "outer-boundary", None), cand(art, q(art), 4.30, "border", "enclosing-border", None)];
        assert!(!merge(c, 1920, 1080).iter().any(|c| c.bx == art));
    }

    #[test]
    fn edge_band_corner_test() {
        let inside: Quad = [[30.0, 30.0], [500.0, 30.0], [500.0, 700.0], [30.0, 700.0]];
        assert!(!quad_in_edge_band(&inside, 1920, 1080));
        let touching: Quad = [[5.0, 30.0], [500.0, 30.0], [500.0, 700.0], [5.0, 700.0]];
        assert!(quad_in_edge_band(&touching, 1920, 1080));
    }
}
