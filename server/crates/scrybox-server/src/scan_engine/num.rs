//! NumPy / Python numeric twins the sidecar's arithmetic relies on.

/// numpy `pairwise_sum` for float32 (the add.reduce inner kernel).
pub fn pairwise_sum_f32(a: &[f32]) -> f32 {
    let n = a.len();
    if n < 8 {
        let mut res = 0.0f32;
        for &v in a {
            res += v;
        }
        res
    } else if n <= 128 {
        let mut r = [0f32; 8];
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
        pairwise_sum_f32(&a[..n2]) + pairwise_sum_f32(&a[n2..])
    }
}

/// `float(arr_f32.mean())`.
pub fn mean_f32(a: &[f32]) -> f64 {
    if a.is_empty() {
        return f64::NAN;
    }
    (pairwise_sum_f32(a) / a.len() as f32) as f64
}

/// `np.percentile(uint8_values, q)` (method="linear") from a histogram.
pub fn percentile_hist(hist: &[u64; 256], n: u64, q: f64) -> f64 {
    if n == 0 {
        return f64::NAN;
    }
    let qq = q / 100.0;
    let nf = n as f64;
    // numpy linear-method virtual index q*(n-1), written exactly as
    // percentile_f64 below does: nf*qq + (1-qq) - 1 (bit-identical ops).
    let virt = nf * qq + (1.0 - qq) - 1.0;
    let virt = virt.clamp(0.0, nf - 1.0);
    let prev = virt.floor();
    let gamma = virt - prev;
    let lo = prev as u64;
    let hi = (lo + 1).min(n - 1);
    let at = |k: u64| -> f64 {
        let mut acc = 0u64;
        for (v, &c) in hist.iter().enumerate() {
            acc += c;
            if acc > k {
                return v as f64;
            }
        }
        255.0
    };
    let (a, b) = (at(lo), at(hi));
    lerp(a, b, gamma)
}

/// numpy `_lerp`.
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    let d = b - a;
    if t >= 0.5 {
        b - d * (1.0 - t)
    } else {
        a + d * t
    }
}

/// `np.percentile(float_values, q)` for a small f64 slice.
pub fn percentile_f64(v: &[f64], q: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    let qq = q / 100.0;
    let nf = n as f64;
    let virt = (nf * qq + (1.0 - qq) - 1.0).clamp(0.0, nf - 1.0);
    let prev = virt.floor();
    let lo = prev as usize;
    let hi = (lo + 1).min(n - 1);
    lerp(s[lo], s[hi], virt - prev)
}

/// Python `round(x, nd)` (correctly rounded, returns float).
pub fn py_round(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", nd, x).parse().unwrap_or(x)
}

/// Python `round(x)` → int (half-even).
pub fn py_round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// numpy float32 `.round(1)`.
pub fn np_round1_f32(x: f32) -> f64 {
    ((x * 10.0f32).round_ties_even() / 10.0f32) as f64
}

pub struct Hist {
    pub h: [u64; 256],
    pub n: u64,
}

impl Hist {
    pub fn new() -> Hist {
        Hist { h: [0; 256], n: 0 }
    }
    pub fn add(&mut self, v: u8) {
        self.h[v as usize] += 1;
        self.n += 1;
    }
    pub fn pct(&self, q: f64) -> f64 {
        percentile_hist(&self.h, self.n, q)
    }
    pub fn frac_where(&self, f: impl Fn(u8) -> bool) -> f64 {
        let c: u64 = (0..256).filter(|&v| f(v as u8)).map(|v| self.h[v]).sum();
        c as f64 / self.n as f64
    }
    pub fn mean(&self) -> f64 {
        let s: u64 = (0..256).map(|v| v as u64 * self.h[v]).sum();
        s as f64 / self.n as f64
    }
}

impl Default for Hist {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_matches_numpy_linear() {
        // np.percentile(np.arange(10, dtype=np.uint8), (2, 98, 10, 90))
        let mut h = Hist::new();
        for v in 0..10u8 {
            h.add(v);
        }
        assert!((h.pct(2.0) - 0.18).abs() < 1e-12);
        assert!((h.pct(98.0) - 8.82).abs() < 1e-12);
        assert!((h.pct(10.0) - 0.9).abs() < 1e-12);
        assert!((h.pct(90.0) - 8.1).abs() < 1e-12);
    }

    #[test]
    fn rounding() {
        assert_eq!(py_round(4.4499999, 3), 4.45);
        assert_eq!(py_round_int(2.5), 2);
        assert_eq!(np_round1_f32(123.44f32), 123.4f32 as f64);
    }

    #[test]
    fn pairwise_is_numpy_order() {
        let v: Vec<f32> = (0..300).map(|i| 0.1f32 * i as f32).collect();
        let s = pairwise_sum_f32(&v);
        assert!((s as f64 - 4485.0).abs() < 0.01);
    }
}
