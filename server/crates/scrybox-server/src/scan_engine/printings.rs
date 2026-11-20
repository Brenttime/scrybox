//! cardscan `load_printings` + the footer-resolution helpers
//! (`_footer_numbers`, `_footer_codes`, `_resolve_footer_group`,
//! `_strong_conflict`, `retro_line_number`, ...), ported from server.py.
//!
//! Printings keep only the fields the resolver needs parsed into the
//! struct; the raw JSON object text of each printing (~84 MB for the full
//! index) is NOT held on the heap (TOTORO mem): the struct records its byte
//! span in `printing_index.json` and `raw()` reads it back from the file
//! handle opened at load (`pread`, page-cache served). The handle pins the
//! loaded inode, so the nightly refresh's rename-over never changes what an
//! engine reads. `dict()` re-materialises the Python `dict(printing)` (key
//! order preserved, `_phys` appended exactly where `load_printings` added it).

use crate::scan_ocr::names::{norm_name, Oracle};
use indexmap::{IndexMap, IndexSet};
use regex::Regex;
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

pub struct Printing {
    /// Byte span of this printing's JSON object in the loaded index file.
    raw_off: u64,
    raw_len: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub set: Option<String>,
    pub num: Option<String>,
    pub aliases: Option<Vec<Option<String>>>,
    pub faces: Vec<FaceNames>,
    /// `printing["_phys"]`, set for every printing reachable by title.
    pub phys: Option<(String, String)>,
}

#[derive(Default)]
pub struct FaceNames {
    pub name: Option<String>,
    pub printed_name: Option<String>,
    pub flavor_name: Option<String>,
}

type Pid = u32;

/// The loaded `printing_index.json` (open handle; see module docs).
pub struct RawSrc(std::fs::File);

#[derive(Default)]
pub struct Printings {
    raw_src: Option<std::sync::Arc<RawSrc>>,
    pub list: Vec<Printing>,
    pub by_title: IndexMap<String, Vec<Pid>>,
    pub by_title_physical: HashMap<(String, String, String), Vec<Pid>>,
    /// Insertion-ordered like Python's defaultdict (build_client_index.py
    /// serialises it in that order).
    pub unique_aliases: IndexMap<String, Vec<(String, Pid)>>,
    pub set_codes: Vec<String>,
    pub set_lookup: HashSet<String>,
    pub set_lengths: Vec<usize>,
    pub set_rank: HashMap<String, usize>,
    pub set_max: HashMap<String, i64>,
}

fn s_opt(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(|s| s.to_string())
}

static SHEET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^([a-z0-9]{2,6})-0*(\d+[a-z]?)[^a-z0-9]*$").unwrap());
static COLLECTOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^0*(\d+)([a-z]*)$").unwrap());

/// `normalize_collector(value)`.
pub fn normalize_collector(value: &str) -> String {
    let v = value.trim().to_lowercase();
    match COLLECTOR.captures(&v) {
        None => v,
        Some(c) => {
            let d = c[1].trim_start_matches('0');
            format!("{}{}", if d.is_empty() { "0" } else { d }, &c[2])
        }
    }
}

/// `physical_footer(printing, set_codes)`.
fn physical_footer(set: &Option<String>, num: &Option<String>, codes: &HashSet<String>) -> (String, String) {
    let n = num.clone().unwrap_or_default();
    if let Some(m) = SHEET.captures(&n) {
        let pre = m[1].to_lowercase();
        if Some(pre.as_str()) != set.as_deref() && codes.contains(&pre) {
            return (pre, normalize_collector(&m[2]));
        }
    }
    (set.clone().unwrap_or_default(), normalize_collector(&n))
}

impl Printings {
    /// `load_printings()` — also extends `oracle` with paper titles and
    /// unambiguous printed/flavor aliases exactly like the Python globals.
    pub fn load(dir: &Path, oracle: &mut Oracle) -> Result<Printings, String> {
        let path = dir.join("printing_index.json");
        // One handle for both the parse and every later `raw()` read, so the
        // spans always index the same inode even if the file is replaced.
        let mut file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let base = bytes.as_ptr() as usize;
        let raw: IndexMap<String, &RawValue> = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let mut list = Vec::with_capacity(raw.len());
        let mut keys_sets = Vec::with_capacity(raw.len());
        for (key, rv) in raw {
            let text = rv.get();
            let raw_off = (text.as_ptr() as usize - base) as u64;
            let raw_len = u32::try_from(text.len()).map_err(|_| "printing object over 4 GiB".to_string())?;
            let v: Map<String, Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
            let aliases = v.get("aliases").and_then(|a| a.as_array()).map(|a| a.iter().map(|x| s_opt(Some(x))).collect());
            let faces = v
                .get("faces")
                .and_then(|f| f.as_array())
                .map(|f| {
                    f.iter()
                        .map(|face| FaceNames {
                            name: s_opt(face.get("name")),
                            printed_name: s_opt(face.get("printed_name")),
                            flavor_name: s_opt(face.get("flavor_name")),
                        })
                        .collect()
                })
                .unwrap_or_default();
            keys_sets.push(key.split(':').next().unwrap_or("").to_string());
            list.push(Printing {
                id: s_opt(v.get("id")),
                name: s_opt(v.get("name")),
                set: s_opt(v.get("set")),
                num: v.get("num").and_then(|n| match n {
                    Value::String(s) => Some(s.clone()),
                    Value::Null => None,
                    other => Some(other.to_string()),
                }),
                aliases,
                faces,
                phys: None,
                raw_off,
                raw_len,
            });
        }
        drop(bytes);
        list.shrink_to_fit();
        let mut p = Printings { list, raw_src: Some(std::sync::Arc::new(RawSrc(file))), ..Default::default() };

        if std::env::var("CARDSCAN_PAPER_TITLES").unwrap_or_else(|_| "1".into()) != "0" {
            for pr in &p.list {
                let faces: Vec<&String> = pr.faces.iter().filter_map(|f| f.name.as_ref()).filter(|n| !n.is_empty()).collect();
                let titles: Vec<Option<&String>> = if faces.is_empty() { vec![pr.name.as_ref()] } else { faces.into_iter().map(Some).collect() };
                for t in titles.into_iter().flatten() {
                    if t.is_empty() || oracle.names.contains(t) {
                        continue;
                    }
                    let n = norm_name(t);
                    if n.is_empty() || oracle.norm_map.contains_key(&n) {
                        continue;
                    }
                    oracle.names.insert(t.clone());
                    oracle.excluded.remove(&n);
                    oracle.push(n, t.clone());
                }
            }
        }

        let mut alias_targets: IndexMap<String, IndexSet<String>> = IndexMap::new();
        for (i, pr) in p.list.iter().enumerate() {
            let canonical = pr.name.clone();
            let mut titles = HashSet::new();
            let aliases: Vec<Option<String>> = match &pr.aliases {
                Some(a) if !a.is_empty() => a.clone(),
                _ => vec![canonical.clone()],
            };
            for alias in aliases.iter().flatten().filter(|a| !a.is_empty()) {
                let na = norm_name(alias);
                if titles.insert(na.clone()) {
                    p.by_title.entry(na.clone()).or_default().push(i as Pid);
                }
                if let Some(c) = &canonical {
                    if oracle.names.contains(c) && na != norm_name(c) {
                        alias_targets.entry(na).or_default().insert(c.clone());
                    }
                }
            }
            for face in &pr.faces {
                let Some(fname) = &face.name else { continue };
                if !oracle.names.contains(fname) {
                    continue;
                }
                for alias in [&face.printed_name, &face.flavor_name].into_iter().flatten() {
                    if !alias.is_empty() && norm_name(alias) != norm_name(fname) {
                        alias_targets.entry(norm_name(alias)).or_default().insert(fname.clone());
                    }
                }
            }
        }
        for (na, ids) in &p.by_title {
            if ids.len() != 1 {
                continue;
            }
            let pr = &p.list[ids[0] as usize];
            let cn = norm_name(pr.name.as_deref().unwrap_or("None"));
            if *na != cn {
                p.unique_aliases.entry(cn).or_default().push((na.clone(), ids[0]));
            }
        }
        for (n, targets) in alias_targets {
            if targets.len() != 1 || oracle.norm_map.contains_key(&n) {
                continue;
            }
            let t = targets.into_iter().next().unwrap();
            oracle.push(n, t);
        }
        let mut codes: Vec<String> = match std::fs::read(dir.join("printing_index.meta.json")) {
            Ok(b) => serde_json::from_slice::<Value>(&b)
                .map_err(|e| e.to_string())?
                .get("set_codes")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            Err(_) => {
                let s: IndexSet<String> = keys_sets.into_iter().collect();
                s.into_iter().collect()
            }
        };
        codes.sort_by_key(|c| std::cmp::Reverse(c.chars().count())); // stable, like sorted(key=len, reverse=True)
        p.set_lookup = codes.iter().cloned().collect();
        let mut lens: Vec<usize> = codes.iter().map(|c| c.len()).collect::<HashSet<_>>().into_iter().collect();
        lens.sort_unstable_by(|a, b| b.cmp(a));
        p.set_lengths = lens;
        p.set_rank = codes.iter().enumerate().map(|(r, c)| (c.clone(), r)).collect();
        p.set_codes = codes;
        let mut all: HashSet<String> = p.set_lookup.clone();
        all.extend(p.list.iter().filter_map(|x| x.set.clone()));
        let titles: Vec<(String, Vec<Pid>)> = p.by_title.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (title, ids) in titles {
            for id in ids {
                let pr = &mut p.list[id as usize];
                let ph = physical_footer(&pr.set, &pr.num, &all);
                pr.phys = Some(ph.clone());
                p.by_title_physical.entry((title.clone(), ph.0, ph.1)).or_default().push(id);
            }
        }
        for pr in &p.list {
            let (ps, pn) = pr.phys.clone().unwrap_or_else(|| physical_footer(&pr.set, &pr.num, &all));
            if !pn.is_empty() && pn.chars().all(|c| c.is_ascii_digit()) {
                let n: i64 = pn.parse().unwrap_or(0);
                let e = p.set_max.entry(ps).or_insert(0);
                *e = (*e).max(n);
            }
        }
        Ok(p)
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// The printing's raw JSON object text, read back from the loaded index
    /// file. An I/O failure yields "" (callers parse with `unwrap_or_default`
    /// / propagate the parse error), and is logged.
    pub fn raw(&self, id: Pid) -> String {
        use std::os::unix::fs::FileExt;
        let pr = &self.list[id as usize];
        let Some(src) = &self.raw_src else { return String::new() };
        let mut buf = vec![0u8; pr.raw_len as usize];
        match src.0.read_exact_at(&mut buf, pr.raw_off).map_err(|e| e.to_string()).and_then(|_| String::from_utf8(buf).map_err(|e| e.to_string())) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("cardscan: printing {id} raw read failed: {e}");
                String::new()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    fn phys(&self, id: Pid) -> (String, String) {
        let pr = &self.list[id as usize];
        pr.phys.clone().unwrap_or_else(|| physical_footer(&pr.set, &pr.num, &self.set_lookup))
    }

    pub fn field(&self, id: Pid, key: &str) -> Value {
        let m: Map<String, Value> = serde_json::from_str(&self.raw(id)).unwrap_or_default();
        m.get(key).cloned().unwrap_or(Value::Null)
    }

    /// `dict(printing)` including the `_phys` tuple load_printings added.
    pub fn dict(&self, id: Pid) -> Map<String, Value> {
        let pr = &self.list[id as usize];
        let mut m: Map<String, Value> = serde_json::from_str(&self.raw(id)).unwrap_or_default();
        if let Some((s, n)) = &pr.phys {
            m.insert("_phys".into(), Value::Array(vec![Value::String(s.clone()), Value::String(n.clone())]));
        }
        m
    }

    /// `_printing_face(printing, title)`.
    pub fn printing_face(&self, id: Pid, title: &str) -> Map<String, Value> {
        let mut result = self.dict(id);
        let wanted = norm_name(title);
        let full_name = result.get("name").cloned().unwrap_or(Value::Null);
        if let Some(Value::Array(faces)) = result.get("faces").cloned() {
            for face in faces {
                let fname = face.get("name").map(py_str).unwrap_or_else(|| "None".into());
                if norm_name(&fname) != wanted {
                    continue;
                }
                if let Value::Object(f) = face {
                    for (k, v) in f {
                        if !v.is_null() {
                            result.insert(k, v);
                        }
                    }
                }
                result.insert("full_name".into(), full_name);
                return result;
            }
        }
        result.insert("full_name".into(), full_name);
        let pr = &self.list[id as usize];
        if pr.aliases.iter().flatten().flatten().any(|a| norm_name(a) == wanted) {
            result.insert("name".into(), Value::String(title.to_string()));
        }
        result
    }

    pub fn choice_evidence(&self, group: &[Pid]) -> Value {
        Value::Array(
            group
                .iter()
                .map(|&id| {
                    let d = self.dict(id);
                    let mut m = Map::new();
                    for k in ["id", "set", "num", "set_name", "image"] {
                        m.insert(k.into(), d.get(k).cloned().unwrap_or(Value::Null));
                    }
                    Value::Object(m)
                })
                .collect(),
        )
    }

    fn title_prints(&self, t: &str) -> &[Pid] {
        self.by_title.get(t).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// `physical_twins(title_norm, printing)`.
    pub fn physical_twins(&self, t: &str, id: Pid) -> Vec<Pid> {
        let (s, n) = self.phys(id);
        let twins = match self.by_title_physical.get(&(t.to_string(), s, n)) {
            Some(v) if !v.is_empty() => v.clone(),
            _ => vec![id],
        };
        if twins.contains(&id) {
            twins
        } else {
            let mut out = vec![id];
            out.extend(twins);
            out
        }
    }

    fn group_of(&self, t: &str, hits: &IndexMap<Option<String>, Pid>) -> Option<Vec<Pid>> {
        let keys: HashSet<(String, String)> = hits.values().map(|&p| self.phys(p)).collect();
        if keys.len() != 1 {
            return None;
        }
        Some(self.physical_twins(t, *hits.values().next().unwrap()))
    }

    /// `_resolve_footer_group(title_norm, codes, numbers)`.
    pub fn resolve_footer_group(&self, t: &str, codes: &[String], numbers: &[String]) -> Option<Vec<Pid>> {
        let tp = self.title_prints(t);
        let mut exact: IndexMap<Option<String>, Pid> = IndexMap::new();
        for code in codes {
            for number in numbers {
                let n = normalize_collector(number);
                if let Some(v) = self.by_title_physical.get(&(t.to_string(), code.clone(), n.clone())) {
                    for &p in v {
                        if self.phys(p).1.to_lowercase() == n.to_lowercase() {
                            exact.insert(self.list[p as usize].id.clone(), p);
                        }
                    }
                }
            }
        }
        if !exact.is_empty() {
            if let Some(g) = self.group_of(t, &exact) {
                return Some(g);
            }
        }
        let mut global: IndexMap<Option<String>, Pid> = IndexMap::new();
        for number in numbers {
            let n = normalize_collector(number);
            for &p in tp {
                if self.phys(p).1 == n {
                    global.insert(self.list[p as usize].id.clone(), p);
                }
            }
        }
        if !global.is_empty() {
            return self.group_of(t, &global);
        }
        if !codes.is_empty() && tp.len() > 1 {
            let mut in_set: IndexMap<Option<String>, Pid> = IndexMap::new();
            for code in codes {
                let hits: Vec<Pid> = tp.iter().copied().filter(|&p| &self.phys(p).0 == code).collect();
                let distinct: HashSet<(String, String)> = hits.iter().map(|&p| self.phys(p)).collect();
                if distinct.len() == 1 {
                    in_set.insert(self.list[hits[0] as usize].id.clone(), hits[0]);
                } else if hits.len() > 1 {
                    return None;
                }
            }
            if !in_set.is_empty() {
                return self.group_of(t, &in_set);
            }
        }
        None
    }

    /// `_footer_codes(raws)`.
    pub fn footer_codes(&self, raws: &[String]) -> Vec<String> {
        let mut codes: Vec<String> = vec![];
        for raw in raws {
            let compact: String = raw.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect();
            let mut matches: Vec<&String> = vec![];
            for pos in 0..compact.len().min(3) {
                for &l in &self.set_lengths {
                    let end = (pos + l).min(compact.len());
                    if let Some(code) = self.set_lookup.get(&compact[pos..end]) {
                        matches.push(code);
                    }
                }
            }
            if let Some(code) = matches.into_iter().min_by_key(|c| self.set_rank[*c]) {
                if !codes.contains(code) {
                    codes.push(code.clone());
                }
            }
        }
        codes
    }

    /// `_strong_conflict(title_norm, printing, raws, held)`.
    pub fn strong_conflict(&self, t: &str, id: Pid, raws: &[String], held: &[String]) -> bool {
        let mine = self.phys(id);
        let twins: HashSet<Pid> = self.physical_twins(t, id).into_iter().collect();
        let mut strong = strong_numbers(raws);
        strong.extend(held.iter().cloned());
        let codes: HashSet<String> = self.footer_codes(raws).into_iter().collect();
        let nums: HashSet<String> = footer_numbers(raws).into_iter().collect();
        for &other in self.title_prints(t) {
            if twins.contains(&other) {
                continue;
            }
            let (os, on) = self.phys(other);
            if on == mine.1 && os != mine.0 {
                continue;
            }
            if strong.contains(&on) || (codes.contains(&os) && nums.contains(&on)) {
                return true;
            }
        }
        false
    }

    /// `identify_unique_title_printing(title)` → printing id.
    pub fn unique_title(&self, title: &str) -> Option<Pid> {
        if self.list.is_empty() || title.is_empty() {
            return None;
        }
        match self.by_title.get(&norm_name(title)) {
            Some(v) if v.len() == 1 => Some(v[0]),
            _ => None,
        }
    }

    /// `identify_unique_ocr_printing(ocr_text, resolved_title)` → id.
    pub fn unique_ocr(&self, ocr: Option<&str>, title: &str) -> Option<Pid> {
        let ocr = ocr.filter(|s| !s.is_empty())?;
        if title.is_empty() {
            return None;
        }
        let tn = norm_name(title);
        let aliases = self.unique_aliases.get(&tn)?;
        if aliases.is_empty() {
            return None;
        }
        let q: Vec<char> = norm_name(ocr).chars().collect();
        // process.extract(..., scorer=fuzz.ratio, score_cutoff=86, limit=2):
        // sorted by score desc, ties by choice index.
        let mut found: Vec<(f64, usize)> = aliases
            .iter()
            .enumerate()
            .map(|(i, (a, _))| (crate::scan_ocr::names::ratio(&q, &a.chars().collect::<Vec<_>>()), i))
            .filter(|(s, _)| *s >= 86.0)
            .collect();
        found.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(a.1.cmp(&b.1)));
        let &(score, index) = found.first()?;
        let runner = found.get(1).map(|x| x.0).unwrap_or(0.0);
        if score < 90.0 && (q.len() < 12 || score - runner < 8.0) {
            return None;
        }
        let (_, pid) = &aliases[index];
        if self.physical_twins(&tn, *pid).len() > 1 {
            return None;
        }
        Some(*pid)
    }

    pub fn retro_set_max(&self, id: Pid) -> (String, i64, String) {
        let (s, n) = self.phys(id);
        let m = *self.set_max.get(&s).unwrap_or(&0);
        (s, m, n)
    }
}

fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        o => o.to_string(),
    }
}

// ---------------------------------------------------------------- regexes

static RUN5: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{5,}").unwrap());
static NUM_SLASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{1,4}[a-z]?)(?:\s*/\s*\d{1,4})?").unwrap());
static NUM_COMPACT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{1,4}[a-z]?)").unwrap());
static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(19|20)\d\d$").unwrap());
static COPYRIGHT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)wizard|coast|\b(19|20)\d\d\b|©|tm\s*&").unwrap());
static RETRO_NT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(\d{1,4}[a-z]?)\s*/\s*(\d{2,4})\s*$").unwrap());
static RETRO_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{1,4}[a-z]?)\s*$").unwrap());
static RETRO_NT3: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{1,3})\s*/\s*(\d{2,3})\s*$").unwrap());

fn is_digit(c: char) -> bool {
    c.is_ascii_digit() || (!c.is_ascii() && unicode_decimal(c))
}

fn unicode_decimal(c: char) -> bool {
    // Python's \d = Unicode Nd; Rust regex \d is the same class.
    static D: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d$").unwrap());
    let mut b = [0u8; 4];
    D.is_match(c.encode_utf8(&mut b))
}

/// Python `re.finditer` for `(?<![forbid])<re>`: a candidate starting
/// right after a forbidden char fails and the scan resumes one char later.
fn finditer_not_after<'t>(re: &Regex, text: &'t str, forbid: &dyn Fn(char) -> bool) -> Vec<regex::Captures<'t>> {
    let mut out = vec![];
    let mut start = 0;
    while start <= text.len() {
        let Some(c) = re.captures_at(text, start) else { break };
        let m = c.get(0).unwrap();
        let prev = text[..m.start()].chars().next_back();
        if prev.is_some_and(forbid) {
            // resume at the next char after the rejected start
            let next = m.start() + text[m.start()..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
            start = next;
            continue;
        }
        start = if m.end() == m.start() { m.end() + text[m.end()..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1) } else { m.end() };
        out.push(c);
    }
    out
}

fn py_int(s: &str) -> i64 {
    let mut n: i64 = 0;
    for c in s.chars() {
        if let Some(d) = c.to_digit(10).or_else(|| unicode_digit_value(c)) {
            n = n.saturating_mul(10).saturating_add(d as i64);
        }
    }
    n
}

fn unicode_digit_value(c: char) -> Option<u32> {
    // Nd blocks are contiguous runs of ten starting at a ...0 code point.
    if !unicode_decimal(c) {
        return None;
    }
    let mut z = c as u32;
    while z > 0 && char::from_u32(z - 1).is_some_and(unicode_decimal) && (c as u32 - (z - 1)) < 10 {
        z -= 1;
    }
    Some(c as u32 - z)
}

/// `_footer_numbers(raws)`.
pub fn footer_numbers(raws: &[String]) -> Vec<String> {
    let mut numbers: Vec<String> = vec![];
    let push = |n: String, numbers: &mut Vec<String>| {
        if !n.is_empty() && !numbers.contains(&n) {
            numbers.push(n);
        }
    };
    for raw in raws {
        for m in RUN5.find_iter(raw) {
            let s = m.as_str();
            let chars: Vec<char> = s.chars().collect();
            let last4: String = chars[chars.len() - 4..].iter().collect();
            push(normalize_collector(&last4), &mut numbers);
        }
        let lower = raw.to_lowercase();
        let mut tokens: Vec<String> =
            finditer_not_after(&NUM_SLASH, &lower, &|c| is_digit(c)).iter().map(|c| c[1].to_string()).collect();
        if tokens.is_empty() {
            let compact: String = lower.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect();
            tokens = NUM_COMPACT.captures_iter(&compact).map(|c| c[1].to_string()).collect();
        }
        for token in tokens {
            let digits: String = token.chars().filter(|c| c.is_ascii_digit()).collect();
            for cand in [token.clone(), digits] {
                push(normalize_collector(&cand), &mut numbers);
            }
        }
    }
    numbers
}

/// Rarity letter + zero-padded 4-place collector number, the footer style
/// of frames with no printed total (Secret Lair "The Zeta Set": `C 0031`).
static PADDED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|[^a-z0-9])[cumrslt]\s*[.,·•\-]?\s*([0o][0-9oqdilz|!sb]{3})(?:$|[^a-z0-9])").unwrap());

/// Digit readings of an OCR'd padded number. The recogniser reads these
/// small serif digits as look-alike letters (`0031` -> `oosi`, `003i`);
/// every reading is only a candidate: `resolve_footer_group` still needs it
/// to be a real collector number of the read title, and an ambiguous set of
/// readings resolves to nothing.
fn padded_readings(token: &str) -> Vec<String> {
    let mut outs = vec![String::new()];
    for c in token.chars() {
        let alts: &[char] = match c {
            '0'..='9' => &[c],
            'o' | 'q' | 'd' => &['0'],
            'i' | 'l' | '|' | '!' => &['1'],
            'z' => &['2'],
            's' => &['5', '3'],
            'b' => &['8', '6'],
            _ => return vec![],
        };
        outs = outs.iter().flat_map(|o| alts.iter().map(move |a| format!("{o}{a}"))).collect();
    }
    outs.into_iter().map(|o| normalize_collector(&o)).collect()
}

/// `footer_numbers` plus the readings of zero-padded `C 0031` numbers.
pub fn footer_numbers_padded(raws: &[String]) -> Vec<String> {
    let mut numbers = footer_numbers(raws);
    for raw in raws {
        let lower = raw.to_lowercase();
        for c in PADDED.captures_iter(&lower) {
            for n in padded_readings(&c[1]) {
                if n != "0" && !numbers.contains(&n) {
                    numbers.push(n);
                }
            }
        }
    }
    numbers
}

/// `_strong_numbers(raws)`.
pub fn strong_numbers(raws: &[String]) -> HashSet<String> {
    let mut strong = HashSet::new();
    let mut seen: IndexMap<String, HashSet<usize>> = IndexMap::new();
    for (i, raw) in raws.iter().enumerate() {
        let lower = raw.to_lowercase();
        for (a, b) in scan_n_over_t(&lower) {
            if py_int(&a) <= py_int(&b) * 2 + 50 {
                strong.insert(normalize_collector(&a));
            }
        }
        for n in footer_numbers(std::slice::from_ref(raw)) {
            seen.entry(n).or_default().insert(i);
        }
    }
    for (n, s) in seen {
        if s.len() >= 2 && !YEAR.is_match(&n) {
            strong.insert(n);
        }
    }
    strong
}

/// finditer `(?<![\d/])(\d{1,4})\s*/\s*(\d{2,4})(?![\d/])`.
fn scan_n_over_t(text: &str) -> Vec<(String, String)> {
    let cs: Vec<char> = text.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < cs.len() {
        let ok_start = is_digit(cs[i]) && (i == 0 || !(is_digit(cs[i - 1]) || cs[i - 1] == '/'));
        if !ok_start {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < cs.len() && is_digit(cs[j]) {
            j += 1;
        }
        if j - i > 4 {
            i += 1;
            continue;
        }
        let mut k = j;
        while k < cs.len() && cs[k].is_whitespace() {
            k += 1;
        }
        if k >= cs.len() || cs[k] != '/' {
            i += 1;
            continue;
        }
        k += 1;
        while k < cs.len() && cs[k].is_whitespace() {
            k += 1;
        }
        let s2 = k;
        while k < cs.len() && is_digit(cs[k]) {
            k += 1;
        }
        let l2 = k - s2;
        let after_bad = k < cs.len() && (is_digit(cs[k]) || cs[k] == '/');
        if !(2..=4).contains(&l2) || after_bad {
            i += 1;
            continue;
        }
        out.push((cs[i..j].iter().collect(), cs[s2..k].iter().collect()));
        i = k;
    }
    out
}

fn search_not_after<'t>(re: &Regex, text: &'t str) -> Option<regex::Captures<'t>> {
    finditer_not_after(re, text, &|c| is_digit(c) || c == '/').into_iter().next()
}

/// `retro_line_number(raw)`.
pub fn retro_line_number(raw: &str) -> Option<String> {
    let text = raw.trim();
    let nt = search_not_after(&RETRO_NT, text);
    if let Some(c) = &nt {
        if YEAR.is_match(&c[1]) {
            return None;
        }
    }
    if COPYRIGHT.is_match(text) {
        if let Some(c) = nt {
            let digits: String = c[1].chars().filter(|ch| !ch.is_ascii_alphabetic()).collect();
            return if py_int(&digits) <= py_int(&c[2]) * 2 + 50 { Some(c[1].to_lowercase()) } else { None };
        }
        if let Some(c) = RETRO_TAIL.captures(text) {
            if !YEAR.is_match(&c[1]) {
                return Some(c[1].to_string());
            }
        }
        return None;
    }
    if let Some(c) = search_not_after(&RETRO_NT3, text) {
        let (a, b) = (py_int(&c[1]), py_int(&c[2]));
        if b >= 40 && 1 <= a && a <= b {
            return Some(a.to_string());
        }
    }
    None
}

/// `_retro_total(raw)`.
pub fn retro_total(raw: &str) -> i64 {
    match search_not_after(&RETRO_NT, raw.trim()) {
        Some(c) => py_int(&c[2]),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn collector_normalisation() {
        assert_eq!(normalize_collector(" 0050 "), "50");
        assert_eq!(normalize_collector("000"), "0");
        assert_eq!(normalize_collector("096Z"), "96z");
        assert_eq!(normalize_collector("ODY-129"), "ody-129");
    }

    #[test]
    fn footer_numbers_like_python() {
        // values generated with the sidecar's own _footer_numbers
        assert_eq!(footer_numbers(&v(&["219/281 U"])), v(&["219"]));
        assert_eq!(footer_numbers(&v(&["80177"])), v(&["177", "8017"]));
        assert_eq!(footer_numbers(&v(&["0330M"])), v(&["330m", "330"]));
        assert_eq!(footer_numbers(&v(&["12345"])), v(&["2345", "1234"]));
        assert_eq!(footer_numbers(&v(&["abc"])), Vec::<String>::new());
    }

    #[test]
    fn zeta_padded_numbers_survive_letter_misreads() {
        // `C 0031` (no printed total) as the recogniser reads it
        assert!(footer_numbers_padded(&v(&["C 0031"])).contains(&"31".to_string()));
        assert!(footer_numbers_padded(&v(&["c oosi"])).contains(&"31".to_string()));
        assert!(footer_numbers_padded(&v(&["c00ss"])).contains(&"55".to_string()));
        assert!(footer_numbers_padded(&v(&["R 0ii9"])).contains(&"119".to_string()));
        // plain words and non-padded tokens add nothing
        assert_eq!(footer_numbers_padded(&v(&["SLZ · EN MSCH"])), footer_numbers(&v(&["SLZ · EN MSCH"])));
        assert_eq!(footer_numbers_padded(&v(&["c ooi"])), footer_numbers(&v(&["c ooi"])));
    }

    #[test]
    fn retro_lines() {
        assert_eq!(retro_line_number("TM & (c) 2021 Wizards of the Coast 408").as_deref(), Some("408"));
        assert_eq!(retro_line_number("Wizards of the Coast LLC 50/249").as_deref(), Some("50"));
        assert_eq!(retro_line_number("4/3"), None);
        assert_eq!(retro_line_number("(c) 2003/2010"), None);
        assert_eq!(retro_line_number("120/249").as_deref(), Some("120"));
        assert_eq!(retro_total("Coast 50/249"), 249);
        assert!(strong_numbers(&v(&["50/249"])).contains("50"));
        assert!(strong_numbers(&v(&["U 0298", "0298 FRA"])).contains("298"));
        assert!(!strong_numbers(&v(&["2021", "2021"])).contains("2021"));
    }

    /// TOTORO mem: raw printing text is read back from the file by span,
    /// byte-for-byte equal to the object text in the index (escapes, key
    /// order, unicode), keeps working after the file is replaced on disk
    /// (rename-over, like the nightly swap), and `dict` adds `_phys`.
    #[test]
    fn raw_spans_read_back_exact_and_survive_rename_over() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("oracle_titles.json"), r#"["Lightning Bolt","Fire // Ice"]"#).unwrap();
        let objs = [
            r#"{"id":"a1","name":"Lightning Bolt","set":"lea","num":"161","zeta":1,"alpha":"é\u00e9\"q"}"#,
            r#"{"id":"b2","name":"Fire // Ice","set":"apc","num":128,"faces":[{"name":"Fire"},{"name":"Ice","printed_name":"Eis"}]}"#,
            r#"{ "id" : "c3", "name":"Lightning Bolt","set":"m10","num":null }"#,
        ];
        let idx = format!(r#"{{"lea:161":{},"apc:128" : {} , "m10:x":{}}}"#, objs[0], objs[1], objs[2]);
        std::fs::write(d.path().join("printing_index.json"), &idx).unwrap();
        let mut oracle = Oracle::load(d.path()).unwrap();
        let p = Printings::load(d.path(), &mut oracle).unwrap();
        assert_eq!(p.len(), 3);
        for (i, o) in objs.iter().enumerate() {
            assert_eq!(p.raw(i as Pid), *o, "printing {i}");
        }
        // Replace the file the way the nightly swap does; the loaded engine
        // keeps reading the inode it parsed.
        let tmp = d.path().join("printing_index.json.tmp");
        std::fs::write(&tmp, "{}").unwrap();
        std::fs::rename(&tmp, d.path().join("printing_index.json")).unwrap();
        assert_eq!(p.raw(1), objs[1]);
        let dict = p.dict(0);
        let keys: Vec<&str> = dict.keys().map(|k| k.as_str()).collect();
        assert_eq!(&keys[..6], &["id", "name", "set", "num", "zeta", "alpha"], "key order kept");
        assert_eq!(dict["alpha"], Value::String("éé\"q".into()));
        assert_eq!(p.field(1, "num"), Value::from(128));
        assert_eq!(p.list[1].num.as_deref(), Some("128"));
    }
}
