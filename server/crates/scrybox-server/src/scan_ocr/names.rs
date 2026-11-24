//! cardscan `norm_name` / `load_oracle` / `find_card_by_ocr` (RapidFuzz
//! branch: `process.extract(q, NORM_LIST, scorer=fuzz.ratio,
//! score_cutoff=60, limit=2)`), over the sidecar's own data files
//! (`oracle_titles.json` + `oracle_index.meta.json`).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

/// Minimum normalised ratio for a card name to be recognised at all
/// (cardscan scan.py `NAME_MATCH_MIN`).
pub const NAME_MATCH_MIN: f64 = 0.90;

pub fn norm_name(s: &str) -> String {
    let lowered = s.to_lowercase().replace('\u{2019}', "'");
    let folded: String = lowered.nfkd().filter(|&c| canonical_combining_class(c) == 0).collect();
    let mut out = String::with_capacity(folded.len());
    let mut gap = false;
    for c in folded.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    out
}

pub struct Oracle {
    /// Fuzzy candidate list, `norm_name`-folded (RapidFuzz `choices`).
    pub norm_list: Vec<String>,
    /// Same strings pre-split to `Vec<char>` (the fuzz kernel's hot input).
    norm_chars: Vec<Box<[char]>>,
    /// normalised form -> canonical title.
    pub norm_map: HashMap<String, String>,
    /// normalised titles rejected outright (`oracle_index.meta.json`).
    pub excluded: HashSet<String>,
    /// Python `ORACLE`: the exact searchable titles.
    pub names: HashSet<String>,
}

/// RapidFuzz `process.extract(..., scorer=fuzz.ratio, score_cutoff=60)`
/// cutoff (module header).
const FUZZ_CUTOFF: f64 = 60.0;
/// Python ranks the drop-lead noisy queries slightly below the raw query.
const NOISY_QUERY_PENALTY: f64 = 0.99;
/// Python's long-query acceptance gate: `len(q) >= 12 and best_r >= 0.86
/// and best_margin >= 0.12`.
const LONG_QUERY_MIN_CHARS: usize = 12;
const LONG_QUERY_ACCEPT: f64 = 0.86;
const LONG_QUERY_MARGIN: f64 = 0.12;

impl Oracle {
    /// Read the sidecar's `oracle_titles.json` (+ the excluded-title list
    /// from `oracle_index.meta.json` when present) out of `dir`.
    ///
    /// # Errors
    /// `oracle_titles.json` missing/unreadable or not a JSON array of
    /// strings.
    pub fn load(dir: &Path) -> Result<Oracle, String> {
        let titles = dir.join("oracle_titles.json");
        let raw = std::fs::read(&titles).map_err(|e| format!("{}: {e}", titles.display()))?;
        let names: Vec<serde_json::Value> = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
        let mut excluded = HashSet::new();
        if let Ok(m) = std::fs::read(dir.join("oracle_index.meta.json")) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&m) {
                for n in v.get("excluded_titles").and_then(|x| x.as_array()).into_iter().flatten() {
                    if let Some(s) = n.as_str() {
                        excluded.insert(norm_name(s));
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        let names: Vec<String> = names
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) if !s.is_empty() => Some(s),
                _ => None,
            })
            .filter(|s| seen.insert(s.clone()))
            .collect();
        Ok(Oracle::from_names(names, excluded))
    }

    /// Python builds NORM_LIST by iterating a `set` (hash-seed order); here
    /// the file order is kept, which is deterministic.
    pub fn from_names(names: Vec<String>, excluded: HashSet<String>) -> Oracle {
        let mut norm_list = Vec::with_capacity(names.len());
        let mut norm_map = HashMap::with_capacity(names.len());
        let set: HashSet<String> = names.iter().cloned().collect();
        for n in names {
            let k = norm_name(&n);
            norm_list.push(k.clone());
            norm_map.insert(k, n);
        }
        let norm_chars = norm_list.iter().map(|s: &String| s.chars().collect()).collect();
        Oracle { norm_list, norm_chars, norm_map, excluded, names: set }
    }

    /// Append one searchable normalized title (NORM_MAP/NORM_LIST push).
    pub fn push(&mut self, normalized: String, exact: String) {
        self.norm_chars.push(normalized.chars().collect());
        self.norm_list.push(normalized.clone());
        self.norm_map.insert(normalized, exact);
    }

    /// Returns (name, score) like Python `(name or None, float)`.
    pub fn find_card_by_ocr(&self, ocr: &str, threshold: f64) -> (Option<String>, f64) {
        if ocr.is_empty() {
            return (None, 0.0);
        }
        let q = norm_name(ocr);
        if q.is_empty() || self.excluded.contains(&q) {
            return (None, 0.0);
        }
        if let Some(n) = self.norm_map.get(&q) {
            return (Some(n.clone()), 1.0);
        }
        let mut queries = vec![q.clone()];
        let parts: Vec<&str> = q.split(' ').collect();
        for (drop, max_noise) in [(1usize, 4usize), (2, 7)] {
            if parts.len() > drop {
                let dropped: String = parts[..drop].concat();
                let tail = parts[drop..].join(" ");
                if dropped.chars().count() <= max_noise && tail.chars().count() >= 4 && !queries.contains(&tail) {
                    queries.push(tail);
                }
            }
        }
        let (mut best, mut best_r, mut best_margin): (Option<usize>, f64, f64) = (None, 0.0, 0.0);
        for (qi, query) in queries.iter().enumerate() {
            let found = self.extract2(query, FUZZ_CUTOFF);
            let Some((cand, score)) = found[0] else { continue };
            let penalty = if qi == 0 { 1.0 } else { NOISY_QUERY_PENALTY };
            let ratio = score * 0.01 * penalty;
            let runner = match found[1] {
                Some((_, s)) => s * 0.01 * penalty,
                None => 0.0,
            };
            if ratio > best_r {
                best_r = ratio;
                best = Some(cand);
                best_margin = ratio - runner;
            }
        }
        let accepted = best_r >= threshold
            || (q.chars().count() >= LONG_QUERY_MIN_CHARS
                && best_r >= LONG_QUERY_ACCEPT
                && best_margin >= LONG_QUERY_MARGIN);
        match best {
            Some(i) if accepted => (self.norm_map.get(&self.norm_list[i]).cloned(), best_r),
            _ => (None, best_r),
        }
    }

    /// Top-2 by `fuzz.ratio` >= cutoff; ties keep list order (RapidFuzz).
    fn extract2(&self, query: &str, cutoff: f64) -> [Option<(usize, f64)>; 2] {
        let qb: Vec<char> = query.chars().collect();
        let mut top: [Option<(usize, f64)>; 2] = [None, None];
        for (i, cand) in self.norm_chars.iter().enumerate() {
            let cl = cand.len();
            let lensum = qb.len() + cl;
            // Upper bound: ratio <= 2*min/lensum; skip hopeless ones fast.
            let ub = if lensum == 0 { 100.0 } else { 200.0 * qb.len().min(cl) as f64 / lensum as f64 };
            if ub < cutoff || top[1].is_some_and(|(_, s)| ub < s) {
                continue;
            }
            let s = ratio(&qb, cand);
            if s < cutoff {
                continue;
            }
            match top[0] {
                Some((_, s0)) if s <= s0 => match top[1] {
                    Some((_, s1)) if s <= s1 => {}
                    _ => top[1] = Some((i, s)),
                },
                _ => {
                    top[1] = top[0];
                    top[0] = Some((i, s));
                }
            }
        }
        top
    }
}

/// RapidFuzz `fuzz.ratio` (normalized Indel similarity * 100).
pub fn ratio(a: &[char], b: &[char]) -> f64 {
    let lensum = a.len() + b.len();
    if lensum == 0 {
        return 100.0;
    }
    let lcs = lcs_len(a, b);
    let dist = lensum - 2 * lcs;
    let nd = dist as f64 / lensum as f64;
    (1.0 - nd) * 100.0
}

fn lcs_len(a: &[char], b: &[char]) -> usize {
    // Hyyrö bit-parallel LCS (RapidFuzz's kernel) for short ASCII patterns.
    if a.len() <= 64 && a.iter().all(|c| c.is_ascii()) {
        let mut pm = [0u64; 128];
        for (i, &c) in a.iter().enumerate() {
            pm[c as usize] |= 1u64 << i;
        }
        let mut s: u64 = !0;
        for &c in b {
            let m = if c.is_ascii() { pm[c as usize] } else { 0 };
            let u = s & m;
            s = s.wrapping_add(u) | (s.wrapping_sub(u));
        }
        let mask = if a.len() == 64 { !0 } else { (1u64 << a.len()) - 1 };
        return (!s & mask).count_ones() as usize;
    }
    lcs_dp(a, b)
}

fn lcs_dp(a: &[char], b: &[char]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for &ca in a {
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = if ca == cb { prev[j] + 1 } else { cur[j].max(prev[j + 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> Oracle {
        let names = ["Syr Gwyn, Hero of Ashvale", "Relic Axe", "Cryogen Relic", "Nazgûl", "Fling", "Sentinel"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        Oracle::from_names(names, ["goblin token"].iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn norm_name_folds_like_python() {
        assert_eq!(norm_name("Nazgûl"), "nazgul");
        assert_eq!(norm_name("  Hunter\u{2019}s  Insight!! "), "hunter s insight");
        assert_eq!(norm_name("Æther Vial"), " ther vial".trim());
    }

    #[test]
    fn ratio_matches_rapidfuzz() {
        let a: Vec<char> = "this is a test".chars().collect();
        let b: Vec<char> = "this is a test!".chars().collect();
        assert!((ratio(&a, &b) - 96.55172413793103).abs() < 1e-9);
    }

    #[test]
    fn bitparallel_lcs_equals_dp() {
        let words = ["", "a", "syr gwyn hero of ashvale", "syrgwyn her0 of ashva1e", "relic axe", "cryogen relic", "aaaa", "abab"];
        for x in words {
            for y in words {
                let (a, b): (Vec<char>, Vec<char>) = (x.chars().collect(), y.chars().collect());
                assert_eq!(lcs_len(&a, &b), lcs_dp(&a, &b), "{x} / {y}");
            }
        }
    }

    #[test]
    fn exact_fuzzy_and_excluded() {
        let o = oracle();
        assert_eq!(o.find_card_by_ocr("Nazgul", 0.9), (Some("Nazgûl".into()), 1.0));
        assert_eq!(o.find_card_by_ocr("Goblin Token", 0.9), (None, 0.0));
        let (n, s) = o.find_card_by_ocr("Flinq", 0.9);
        assert_eq!(n, None);
        assert!((s - 0.8).abs() < 1e-12);
        // leading debris drop: "xx relic axe" -> tail "relic axe" at 0.99 penalty
        let (n, s) = o.find_card_by_ocr("xx Relic Axe", 0.9);
        assert_eq!(n.as_deref(), Some("Relic Axe"));
        assert!((s - 0.99).abs() < 1e-12);
    }
}
