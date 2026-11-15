//! Pillow 12.x image ops the cardscan sidecar runs on title/footer strips,
//! ported bit-exactly: `Image.crop`, `ImageOps.autocontrast` (cutoff=0),
//! `Image.resize(.., LANCZOS)` (libImaging/Resample.c 8bpc fixed point) and
//! the lossless right-angle `Image.rotate(.., expand=True)` transposes.

/// Packed RGB8 image (row-major, 3 bytes/pixel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

impl Rgb {
    pub fn new(w: usize, h: usize) -> Rgb {
        Rgb { w, h, px: vec![0; w * h * 3] }
    }

    /// PIL `crop((l, t, r, b))`: out-of-bounds area is zero filled.
    pub fn crop(&self, l: i64, t: i64, r: i64, b: i64) -> Rgb {
        let w = (r - l).max(0) as usize;
        let h = (b - t).max(0) as usize;
        let mut out = Rgb::new(w, h);
        for y in 0..h {
            let sy = t + y as i64;
            if sy < 0 || sy >= self.h as i64 {
                continue;
            }
            for x in 0..w {
                let sx = l + x as i64;
                if sx < 0 || sx >= self.w as i64 {
                    continue;
                }
                let s = (sy as usize * self.w + sx as usize) * 3;
                let d = (y * w + x) * 3;
                out.px[d..d + 3].copy_from_slice(&self.px[s..s + 3]);
            }
        }
        out
    }

    /// `ImageOps.autocontrast(img)` with defaults (per-band histogram LUT).
    pub fn autocontrast(&self) -> Rgb {
        let mut hist = [[0u64; 256]; 3];
        for p in self.px.chunks_exact(3) {
            for c in 0..3 {
                hist[c][p[c] as usize] += 1;
            }
        }
        let mut lut = [[0u8; 256]; 3];
        for c in 0..3 {
            let h = &hist[c];
            let lo = (0..256).find(|&i| h[i] != 0).unwrap_or(256) as i64;
            let hi = (0..256).rev().find(|&i| h[i] != 0).map(|v| v as i64).unwrap_or(-1);
            if hi <= lo {
                for i in 0..256 {
                    lut[c][i] = i as u8;
                }
            } else {
                let scale = 255.0f64 / (hi - lo) as f64;
                let offset = -(lo as f64) * scale;
                for i in 0..256 {
                    // Python int() truncates toward zero.
                    let v = (i as f64 * scale + offset).trunc() as i64;
                    lut[c][i] = v.clamp(0, 255) as u8;
                }
            }
        }
        let mut out = self.clone();
        for p in out.px.chunks_exact_mut(3) {
            for c in 0..3 {
                p[c] = lut[c][p[c] as usize];
            }
        }
        out
    }

    /// `Image.resize((w, h), Image.LANCZOS)` (default box, no reducing_gap).
    pub fn resize_lanczos(&self, w: usize, h: usize) -> Rgb {
        if w == self.w && h == self.h {
            return self.clone();
        }
        let (bh, kh, ksh) = precompute_coeffs(self.w, 0.0, self.w as f64, w);
        let (bv, kv, ksv) = precompute_coeffs(self.h, 0.0, self.h as f64, h);
        let need_h = w != self.w;
        let need_v = h != self.h;
        let ybox_first = bv[0];
        let ybox_last = bv[(h - 1) * 2] + bv[(h - 1) * 2 + 1];
        let mut src_owned;
        let mut src: &Rgb = self;
        let mut bv = bv;
        if need_h {
            for i in 0..h {
                bv[i * 2] -= ybox_first;
            }
            let th = ybox_last - ybox_first;
            let mut tmp = Rgb::new(w, th);
            for y in 0..th {
                let srow = (y + ybox_first) * self.w * 3;
                for x in 0..w {
                    let xmin = bh[x * 2];
                    let xmax = bh[x * 2 + 1];
                    let k = &kh[x * ksh..x * ksh + xmax];
                    let mut ss = [1i32 << (PRECISION_BITS - 1); 3];
                    for (i, kk) in k.iter().enumerate() {
                        let s = srow + (xmin + i) * 3;
                        for c in 0..3 {
                            ss[c] = ss[c].wrapping_add(self.px[s + c] as i32 * kk);
                        }
                    }
                    let d = (y * w + x) * 3;
                    for c in 0..3 {
                        tmp.px[d + c] = clip8(ss[c]);
                    }
                }
            }
            src_owned = tmp;
            src = &src_owned;
        } else {
            src_owned = Rgb::new(0, 0);
            let _ = &src_owned;
        }
        if !need_v {
            return src.clone();
        }
        let mut out = Rgb::new(w, h);
        for y in 0..h {
            let ymin = bv[y * 2];
            let ymax = bv[y * 2 + 1];
            let k = &kv[y * ksv..y * ksv + ymax];
            for x in 0..w {
                let mut ss = [1i32 << (PRECISION_BITS - 1); 3];
                for (i, kk) in k.iter().enumerate() {
                    let s = ((ymin + i) * src.w + x) * 3;
                    for c in 0..3 {
                        ss[c] = ss[c].wrapping_add(src.px[s + c] as i32 * kk);
                    }
                }
                let d = (y * w + x) * 3;
                for c in 0..3 {
                    out.px[d + c] = clip8(ss[c]);
                }
            }
        }
        out
    }

    /// `Image.rotate(angle, expand=True)` for angle in {90, -90, 180}
    /// (Pillow short-circuits these to exact transposes; positive = CCW).
    pub fn rotate_right_angle(&self, angle: i32) -> Rgb {
        let a = angle.rem_euclid(360);
        match a {
            0 => self.clone(),
            180 => {
                let mut o = Rgb::new(self.w, self.h);
                for y in 0..self.h {
                    for x in 0..self.w {
                        let s = (y * self.w + x) * 3;
                        let d = ((self.h - 1 - y) * self.w + (self.w - 1 - x)) * 3;
                        o.px[d..d + 3].copy_from_slice(&self.px[s..s + 3]);
                    }
                }
                o
            }
            90 | 270 => {
                let mut o = Rgb::new(self.h, self.w);
                for y in 0..self.h {
                    for x in 0..self.w {
                        let s = (y * self.w + x) * 3;
                        // ROTATE_90 (CCW): (x, y) -> (y, W-1-x)
                        // ROTATE_270 (CW): (x, y) -> (H-1-y, x)
                        let (nx, ny) = if a == 90 { (y, self.w - 1 - x) } else { (self.h - 1 - y, x) };
                        let d = (ny * o.w + nx) * 3;
                        o.px[d..d + 3].copy_from_slice(&self.px[s..s + 3]);
                    }
                }
                o
            }
            _ => panic!("rotate_right_angle: unsupported angle {angle}"),
        }
    }

    /// RGB -> BGR channel swap (`cv2.cvtColor(.., COLOR_RGB2BGR)`).
    pub fn swap_rb(&self) -> Rgb {
        let mut o = self.clone();
        for p in o.px.chunks_exact_mut(3) {
            p.swap(0, 2);
        }
        o
    }
}

const PRECISION_BITS: u32 = 32 - 8 - 2;

#[inline]
fn clip8(v: i32) -> u8 {
    (v >> PRECISION_BITS).clamp(0, 255) as u8
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let x = x * std::f64::consts::PI;
    x.sin() / x
}

fn lanczos(x: f64) -> f64 {
    // Pillow: truncated sinc, support 3.
    if -3.0 <= x && x < 3.0 {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// Resample.c `precompute_coeffs` + `normalize_coeffs_8bpc`.
/// Returns (bounds [xmin, xmax]*, int coefficients, ksize).
fn precompute_coeffs(in_size: usize, in0: f64, in1: f64, out_size: usize) -> (Vec<usize>, Vec<i32>, usize) {
    let support_base = 3.0f64;
    let scale = (in1 - in0) / out_size as f64;
    let filterscale = if scale < 1.0 { 1.0 } else { scale };
    let support = support_base * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let mut kk = vec![0f64; out_size * ksize];
    let mut bounds = vec![0usize; out_size * 2];
    let ss = 1.0 / filterscale;
    for xx in 0..out_size {
        let center = in0 + (xx as f64 + 0.5) * scale;
        let mut ww = 0.0;
        // C (int) cast truncates toward zero.
        let mut xmin = (center - support + 0.5) as i64;
        if xmin < 0 {
            xmin = 0;
        }
        let mut xmax = (center + support + 0.5) as i64;
        if xmax > in_size as i64 {
            xmax = in_size as i64;
        }
        xmax -= xmin;
        let k = &mut kk[xx * ksize..(xx + 1) * ksize];
        for x in 0..xmax as usize {
            let w = lanczos((x as f64 + xmin as f64 - center + 0.5) * ss);
            k[x] = w;
            ww += w;
        }
        for x in 0..xmax as usize {
            if ww != 0.0 {
                k[x] /= ww;
            }
        }
        for x in xmax as usize..ksize {
            k[x] = 0.0;
        }
        bounds[xx * 2] = xmin as usize;
        bounds[xx * 2 + 1] = xmax as usize;
    }
    let ik = kk
        .iter()
        .map(|&k| {
            let s = k * (1u64 << PRECISION_BITS) as f64;
            if k < 0.0 { (-0.5 + s) as i32 } else { (0.5 + s) as i32 }
        })
        .collect();
    (bounds, ik, ksize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autocontrast_stretches_each_band() {
        let img = Rgb { w: 2, h: 1, px: vec![10, 0, 7, 20, 255, 7] };
        let o = img.autocontrast();
        assert_eq!(o.px, vec![0, 0, 7, 255, 255, 7]);
    }

    #[test]
    fn rotate_90_is_ccw() {
        // 2x1: [A B] -> CCW -> column [B; A]
        let img = Rgb { w: 2, h: 1, px: vec![1, 1, 1, 2, 2, 2] };
        let o = img.rotate_right_angle(90);
        assert_eq!((o.w, o.h), (1, 2));
        assert_eq!(o.px, vec![2, 2, 2, 1, 1, 1]);
        let o = img.rotate_right_angle(-90);
        assert_eq!(o.px, vec![1, 1, 1, 2, 2, 2]);
    }

    #[test]
    fn lanczos_identity_and_constant() {
        let img = Rgb { w: 5, h: 4, px: vec![77; 60] };
        let o = img.resize_lanczos(13, 9);
        assert!(o.px.iter().all(|&v| v == 77));
    }
}
