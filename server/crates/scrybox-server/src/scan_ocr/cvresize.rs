//! OpenCV `cv2.resize(src, (w, h))` (default INTER_LINEAR) for 8UC3,
//! ported from imgproc/src/resize.cpp: 11-bit fixed-point coefficients,
//! horizontal pass to i32 rows, vertical pass with the SIMD rounding
//! `((b0*(r0>>4))>>16 + (b1*(r1>>4))>>16 + 2) >> 2`, and the exact-2x
//! fast path that OpenCV silently turns into INTER_AREA.

use super::pil::Rgb;

const COEF_BITS: i32 = 11;
const COEF_SCALE: i32 = 1 << COEF_BITS;

fn coeffs(ssize: usize, dsize: usize) -> (Vec<usize>, Vec<[i16; 2]>) {
    // resize(): inv_scale = (double)dsize/ssize; scale = 1./inv_scale.
    let inv = dsize as f64 / ssize as f64;
    let scale = 1.0 / inv;
    let sat = |v: f32| (v.round_ties_even() as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let mut ofs = Vec::with_capacity(dsize);
    let mut a = Vec::with_capacity(dsize);
    for dx in 0..dsize {
        let mut fx = ((dx as f64 + 0.5) * scale - 0.5) as f32;
        let mut sx = fx.floor() as i64;
        fx -= sx as f32;
        if sx < 0 {
            fx = 0.0;
            sx = 0;
        }
        if sx >= ssize as i64 - 1 {
            fx = 0.0;
            sx = ssize as i64 - 1;
        }
        ofs.push(sx as usize);
        a.push([sat((1.0f32 - fx) * COEF_SCALE as f32), sat(fx * COEF_SCALE as f32)]);
    }
    (ofs, a)
}

pub fn resize_linear(src: &Rgb, w: usize, h: usize) -> Rgb {
    if w == src.w && h == src.h {
        return src.clone();
    }
    // is_area_fast && iscale == 2 on both axes -> INTER_AREA 2x2 average.
    if src.w == w * 2 && src.h == h * 2 {
        let mut o = Rgb::new(w, h);
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    let p = |yy: usize, xx: usize| src.px[(yy * src.w + xx) * 3 + c] as u32;
                    let s = p(2 * y, 2 * x) + p(2 * y, 2 * x + 1) + p(2 * y + 1, 2 * x) + p(2 * y + 1, 2 * x + 1);
                    o.px[(y * w + x) * 3 + c] = ((s + 2) >> 2) as u8;
                }
            }
        }
        return o;
    }
    let (xo, xa) = coeffs(src.w, w);
    let (yo, ya) = coeffs(src.h, h);
    let row = |sy: usize| -> Vec<i32> {
        let base = sy * src.w * 3;
        let mut r = vec![0i32; w * 3];
        for dx in 0..w {
            let sx = xo[dx];
            let a = xa[dx];
            // sx+1 clamps for the last column (a[1] == 0 there).
            let sx1 = (sx + 1).min(src.w - 1);
            for c in 0..3 {
                r[dx * 3 + c] = src.px[base + sx * 3 + c] as i32 * a[0] as i32
                    + src.px[base + sx1 * 3 + c] as i32 * a[1] as i32;
            }
        }
        r
    };
    let mut o = Rgb::new(w, h);
    for dy in 0..h {
        let sy = yo[dy];
        let b = ya[dy];
        let r0 = row(sy);
        let r1 = row((sy + 1).min(src.h - 1));
        let (b0, b1) = (b[0] as i32, b[1] as i32);
        let width = w * 3;
        // VResizeLinearVec_32s8u rounding for the whole row. Measured
        // byte-identical to cv2 5.0.0 on every downscale in the differential
        // corpus (the only direction rec preprocessing uses); upscales can
        // differ by 1 LSB (cv2 takes another kernel there) and are unreachable.
        for i in 0..width {
            let v = (((b0 * (r0[i] >> 4)) >> 16) + ((b1 * (r1[i] >> 4)) >> 16) + 2) >> 2;
            o.px[dy * width + i] = v.clamp(0, 255) as u8;
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_stays_constant() {
        let img = Rgb { w: 7, h: 5, px: vec![200; 105] };
        let o = resize_linear(&img, 3, 2);
        assert!(o.px.iter().all(|&v| v == 200));
    }

    #[test]
    fn exact_half_is_area_average() {
        let img = Rgb { w: 2, h: 2, px: vec![0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3] };
        let o = resize_linear(&img, 1, 1);
        assert_eq!(o.px, vec![2, 2, 2]);
    }
}
