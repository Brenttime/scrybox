//! In-process cardscan OCR (TOTORO mono scan): the recognition-only
//! PP-OCRv6 path, Pillow/OpenCV-exact strip prep and the oracle title
//! matcher, ported from cardscan server.py (Brenttime/cardscan ef4ebd5).

pub mod cvresize;
pub mod names;
pub mod pil;
pub mod rec;

use names::Oracle;
use pil::Rgb;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Data dir holding the sidecar's indexes (`oracle_titles.json`, ...).
pub fn data_dir() -> Option<PathBuf> {
    std::env::var("SCRYBOX_CARDSCAN_DATA").ok().filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// Process-wide oracle (the sidecar's title indexes), loaded lazily on
/// first use and shared by every scan.
///
/// # Errors
/// `SCRYBOX_CARDSCAN_DATA` unset, or the index files failing to read/parse.
pub fn oracle() -> Result<&'static Oracle, String> {
    static O: OnceLock<Result<Oracle, String>> = OnceLock::new();
    O.get_or_init(|| Oracle::load(&data_dir().ok_or("SCRYBOX_CARDSCAN_DATA unset")?))
        .as_ref()
        .map_err(|e| e.clone())
}

fn env_flag(k: &str, default: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| default.to_string())
}

/// Python `round()` on a float: banker's rounding to an int.
fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// crop by fractions + autocontrast + LANCZOS to target_h (server.py idiom).
pub fn title_strip(img: &Rgb, x0: f64, x1: f64, y0: f64, y1: f64, target_h: usize) -> Option<Rgb> {
    let (w, h) = (img.w as f64, img.h as f64);
    let s = img.crop((w * x0) as i64, (h * y0) as i64, (w * x1) as i64, (h * y1) as i64);
    if s.w < 8 || s.h < 4 {
        return None;
    }
    let s = s.autocontrast();
    let tw = py_round(s.w as f64 * target_h as f64 / s.h as f64).max(64) as usize;
    Some(s.resize_lanczos(tw, target_h))
}

/// One scanned title strip: best oracle match (None = unnamed), its score,
/// the raw OCR text, and the rotation angle that produced the winner.
#[derive(Clone, Debug, PartialEq)]
pub struct Title {
    pub name: Option<String>,
    pub score: f64,
    pub raw: Option<String>,
    pub angle: i32,
}

type Cand = (f64, f64, String, String, i32);

/// (x0, x1, y0, y1) of the first and second line of a wrapped title
/// (measured on Zeta Set scans: line one 0.040-0.095, line two 0.100-0.150).
pub const TWO_LINE_TITLE_BANDS: [(f64, f64, f64, f64); 2] = [(0.040, 0.80, 0.030, 0.100), (0.040, 0.80, 0.095, 0.160)];

fn better(a: &Cand, b: &Cand) -> bool {
    // Python tuple max: (score, confidence, name, raw, angle) compared
    // field-by-field, first non-equal field decides. A NaN operand is never
    // equal to anything, so it IS the first non-equal field and the
    // comparison stops there "not greater" — the incumbent wins. Falling
    // through to the tie-break chain (the old Equal fallback) would let
    // name/raw/angle replace Python's winner, and the recognizer callback
    // can supply a NaN confidence (NaN < 0.60 is false, so it passes the
    // absorb filter). A differing finite score still decides even when the
    // loser's confidence is NaN.
    match a.0.partial_cmp(&b.0) {
        Some(Ordering::Equal) => {}
        Some(o) => return o.is_gt(),
        None => return false,
    }
    match a.1.partial_cmp(&b.1) {
        Some(Ordering::Equal) => {}
        Some(o) => return o.is_gt(),
        None => return false,
    }
    (a.2.as_str(), a.3.as_str(), a.4) > (b.2.as_str(), b.3.as_str(), b.4)
}

/// `_batch_scene_titles(images)`; `recognize` is `_recognize_scene_strips`
/// minus the BGR swap (done here).
pub fn batch_scene_titles(
    images: &[Rgb],
    oracle: &Oracle,
    recognize: &dyn Fn(&[Rgb]) -> Result<Vec<(String, f64)>, String>,
) -> Result<Vec<Title>, String> {
    let variants = [(0.030, 0.82, 0.025, 0.100, 96usize), (0.040, 0.80, 0.055, 0.120, 96)];
    let fast = env_flag("CARDSCAN_TITLE_FAST", "0") == "1";
    let first: &[_] = if fast { &variants[..1] } else { &variants[..] };
    let rec_bgr = |strips: &[Rgb]| -> Result<Vec<(String, f64)>, String> {
        if strips.is_empty() {
            return Ok(vec![]);
        }
        let bgr: Vec<Rgb> = strips.iter().map(|s| s.swap_rb()).collect();
        recognize(&bgr)
    };
    let mut results: Vec<Vec<Cand>> = vec![vec![]; images.len()];
    let mut strips = vec![];
    let mut owners = vec![];
    for (o, im) in images.iter().enumerate() {
        for &(x0, x1, y0, y1, th) in first {
            if let Some(s) = title_strip(im, x0, x1, y0, y1, th) {
                strips.push(s);
                owners.push(o);
            }
        }
    }
    let mut absorb = |owners: &[usize], angles: &[i32], out: Vec<(String, f64)>, results: &mut Vec<Vec<Cand>>| {
        for ((&o, &a), (raw, conf)) in owners.iter().zip(angles).zip(out) {
            if raw.is_empty() || conf < 0.60 {
                continue;
            }
            if let (Some(name), score) = oracle.find_card_by_ocr(&raw, names::NAME_MATCH_MIN) {
                results[o].push((score, conf, name, raw, a));
            }
        }
    };
    let out = rec_bgr(&strips)?;
    absorb(&owners, &vec![0; owners.len()], out, &mut results);

    let tight_h: usize = env_flag("CARDSCAN_TIGHT_H", "128").parse().unwrap_or(128);
    let (mut ts, mut to) = (vec![], vec![]);
    for (o, cands) in results.iter().enumerate() {
        if !cands.is_empty() && cands.iter().map(|c| names::norm_name(&c.2).chars().count()).max().unwrap_or(0) >= 12 {
            continue;
        }
        let mut extra: Vec<(f64, f64, f64, f64)> = variants[first.len()..].iter().map(|v| (v.0, v.1, v.2, v.3)).collect();
        extra.extend([(0.045, 0.80, 0.045, 0.140), (0.050, 0.80, 0.090, 0.170), (0.010, 0.95, 0.000, 0.090)]);
        for (x0, x1, y0, y1) in extra {
            if let Some(s) = title_strip(&images[o], x0, x1, y0, y1, tight_h) {
                ts.push(s);
                to.push(o);
            }
        }
    }
    if !ts.is_empty() {
        let out = rec_bgr(&ts)?;
        absorb(&to, &vec![0; to.len()], out, &mut results);
    }

    // Two-line titles. Frames that print the name in a large font with no
    // title bar (Secret Lair "The Zeta Set": "Oboro, Palace in / the
    // Clouds") wrap a long name onto a second line, so every single-line
    // strip reads only a fragment that matches no oracle name. Only cards
    // still unnamed pay for it: read the two line bands separately and match
    // their joined text.
    let (mut ws, mut wo) = (vec![], vec![]);
    for (o, cands) in results.iter().enumerate() {
        if !cands.is_empty() {
            continue;
        }
        let lines: Vec<Rgb> =
            TWO_LINE_TITLE_BANDS.iter().filter_map(|&(x0, x1, y0, y1)| title_strip(&images[o], x0, x1, y0, y1, tight_h)).collect();
        if lines.len() == 2 {
            ws.extend(lines);
            wo.push(o);
        }
    }
    if !ws.is_empty() {
        let out = rec_bgr(&ws)?;
        let mut joined = vec![];
        // ws is extended TWO strips at a time (lines.len() == 2 gate) and
        // rec_bgr maps 1:1, so out has even length and every chunk is 2.
        for pair in out.chunks(2) {
            let ((a, ca), (b, cb)) = (&pair[0], &pair[1]);
            let ok = !a.trim().is_empty() && !b.trim().is_empty() && *ca >= 0.60 && *cb >= 0.60;
            joined.push(if ok { (format!("{} {}", a.trim(), b.trim()), ca.min(*cb)) } else { (String::new(), 0.0) });
        }
        absorb(&wo, &vec![0; wo.len()], joined, &mut results);
    }

    let (mut rs, mut ro, mut ra) = (vec![], vec![], vec![]);
    for (o, cands) in results.iter().enumerate() {
        if !cands.is_empty() || env_flag("CARDSCAN_TITLE_ROTATE", "1") != "1" {
            continue;
        }
        for angle in [90, -90, 180] {
            let r = images[o].rotate_right_angle(angle);
            let s = r.crop(((r.w as f64) * 0.01) as i64, 0, ((r.w as f64) * 0.95) as i64, ((r.h as f64) * 0.12) as i64);
            if s.w < 8 || s.h < 4 {
                continue;
            }
            let s = s.autocontrast();
            let th = 160usize;
            let tw = py_round(s.w as f64 * th as f64 / s.h as f64).max(64) as usize;
            rs.push(s.resize_lanczos(tw, th));
            ro.push(o);
            ra.push(angle);
        }
    }
    if !rs.is_empty() {
        let out = rec_bgr(&rs)?;
        absorb(&ro, &ra, out, &mut results);
    }

    Ok(results
        .into_iter()
        .map(|cands| {
            let mut best: Option<Cand> = None;
            for c in cands {
                if best.as_ref().is_none_or(|b| better(&c, b)) {
                    best = Some(c);
                }
            }
            match best {
                None => Title { name: None, score: 0.0, raw: None, angle: 0 },
                Some((score, _c, name, raw, angle)) => Title { name: Some(name), score, raw: Some(raw), angle },
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(score: f64, conf: f64, name: &str, raw: &str, angle: i32) -> Cand {
        (score, conf, name.to_string(), raw.to_string(), angle)
    }

    #[test]
    fn better_keeps_incumbent_on_nan_confidence_tie() {
        // Equal scores, NaN confidence from the recognizer callback (NaN
        // passes the `conf < 0.60` filter). Python stops at the first
        // non-equal field — NaN != 0.75 — so `a` is NOT greater even though
        // its name/raw/angle would win the tie-break chain.
        let a = cand(0.9, f64::NAN, "Zzz", "zzz", 180);
        let b = cand(0.9, 0.75, "Aaa", "aaa", 0);
        assert!(!better(&a, &b), "NaN conf must not replace the incumbent");
        assert!(!better(&b, &a), "finite conf is never greater than NaN either");
    }

    #[test]
    fn better_finite_score_decides_over_nan_confidence() {
        // A differing finite score decides at the first field, regardless
        // of either candidate's confidence being NaN.
        assert!(better(&cand(0.95, f64::NAN, "A", "a", 0), &cand(0.90, 0.99, "Z", "z", 0)));
        assert!(!better(&cand(0.90, 0.99, "A", "a", 0), &cand(0.95, f64::NAN, "Z", "z", 0)));
    }

    #[test]
    fn better_rejects_nan_score_and_keeps_python_tie_breaks() {
        assert!(!better(&cand(f64::NAN, 0.9, "Z", "z", 0), &cand(0.9, 0.9, "A", "a", 0)), "NaN score loses to any finite score");
        assert!(!better(&cand(0.9, 0.9, "A", "a", 0), &cand(f64::NAN, 0.9, "Z", "z", 0)), "finite score vs NaN score is 'not greater' too");
        // All fields equal => not greater (max keeps the incumbent).
        assert!(!better(&cand(0.9, 0.8, "X", "x", 0), &cand(0.9, 0.8, "X", "x", 0)));
        // The plain tie-break chain still works on finite, equal scores.
        assert!(better(&cand(0.9, 0.8, "B", "a", 0), &cand(0.9, 0.8, "A", "z", 0)));
        assert!(better(&cand(0.9, 0.8, "A", "b", 0), &cand(0.9, 0.8, "A", "a", 0)));
        assert!(better(&cand(0.9, 0.8, "A", "a", 1), &cand(0.9, 0.8, "A", "a", 0)));
    }
}
