//! Rust ports of the cardscan data jobs: `build_oracle_index.py`,
//! `build_printing_index.py`, `update_oracle_data.py`,
//! `update_printing_data.py`. Output files are the ones the sidecar writes
//! (`oracle_index.json`, `oracle_titles.json`, `oracle_index.meta.json`,
//! `printing_index.json`, `printing_index.meta.json`), byte-compatible JSON
//! (compact, `ensure_ascii=False`; metadata `indent=2`).

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const EXCLUDED_LAYOUTS: [&str; 5] = ["art_series", "double_faced_token", "emblem", "front_card", "token"];
// INFALLIBLE: constant regex literal; compiled on first use.
static TOKEN_TYPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(token|emblem)\b").expect("valid regex"));

fn g<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}

/// `a or b`
fn or(a: &Value, b: Value) -> Value {
    if truthy(a) {
        a.clone()
    } else {
        b
    }
}

/// The first `image_uris` entry in Scryfall's preferred-key order (null when none).
pub fn image_url(obj: &Value) -> Value {
    let images = g(obj, "image_uris");
    for k in ["large", "normal", "png", "border_crop", "small"] {
        let v = g(images, k);
        if truthy(v) {
            return v.clone();
        }
    }
    Value::Null
}

/// Python `_is_scannable_physical_card` verbatim: a paper, non-digital
/// `object == "card"` outside the excluded layouts.
pub fn is_scannable_physical_card(card: &Value) -> bool {
    let tl = g(card, "type_line").as_str().unwrap_or("").trim().to_lowercase();
    let games_paper = g(card, "games").as_array().is_some_and(|a| a.iter().any(|x| x == "paper"));
    let layout = g(card, "layout").as_str().unwrap_or("");
    let layout_null = g(card, "layout").is_null();
    g(card, "object") == "card"
        && !truthy(g(card, "digital"))
        && games_paper
        && (layout_null || !EXCLUDED_LAYOUTS.contains(&layout))
        && !TOKEN_TYPE.is_match(&tl)
        && tl != "card"
        && tl != "art card"
}

fn py_fmt(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        o => o.to_string(),
    }
}

fn pt(src: &Value) -> Value {
    if !g(src, "power").is_null() && !g(src, "toughness").is_null() {
        json!(format!("{}/{}", py_fmt(g(src, "power")), py_fmt(g(src, "toughness"))))
    } else {
        or(g(src, "loyalty"), g(src, "defense").clone())
    }
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.into(), v);
    }
    Value::Object(m)
}

/// `build_printing_index.normalize_collector` (NOT the server's variant).
pub fn build_normalize_collector(value: &Value) -> String {
    let s = if truthy(value) { py_fmt(value) } else { String::new() };
    let v = s.trim().to_lowercase();
    let (mut digits, mut suffix) = (String::new(), String::new());
    for c in v.chars() {
        if c.is_numeric() && suffix.is_empty() && c.is_ascii_digit() {
            digits.push(c);
        } else {
            suffix.push(c);
        }
    }
    if !digits.is_empty() {
        let t = digits.trim_start_matches('0');
        digits = if t.is_empty() { "0".into() } else { t.into() };
    }
    digits + &suffix
}

/// Compact printing dict for the scan client (Python `_compact_printing` field order).
pub fn compact_printing(card: &Value) -> Value {
    let mut faces = vec![];
    for face in g(card, "card_faces").as_array().into_iter().flatten() {
        faces.push(obj(vec![
            ("name", g(face, "name").clone()),
            ("printed_name", g(face, "printed_name").clone()),
            ("flavor_name", g(face, "flavor_name").clone()),
            ("image", or(&image_url(face), image_url(card))),
            ("mana", g(face, "mana_cost").clone()),
            ("type", g(face, "type_line").clone()),
            ("text", or(g(face, "oracle_text"), json!(""))),
            ("flavor", or(g(face, "flavor_text"), json!(""))),
            ("colors", g(face, "colors").clone()),
        ]));
    }
    let mut aliases: Vec<Value> = vec![];
    let mut vals = vec![g(card, "name").clone(), g(card, "printed_name").clone(), g(card, "flavor_name").clone()];
    for f in &faces {
        for k in ["name", "printed_name", "flavor_name"] {
            vals.push(g(f, k).clone());
        }
    }
    for v in vals {
        if truthy(&v) && !aliases.contains(&v) {
            aliases.push(v);
        }
    }
    let image = or(&image_url(card), faces.first().map(|f| g(f, "image").clone()).unwrap_or(Value::Null));
    obj(vec![
        ("id", g(card, "id").clone()),
        ("oracle_id", g(card, "oracle_id").clone()),
        ("name", g(card, "name").clone()),
        ("printed_name", g(card, "printed_name").clone()),
        ("flavor_name", g(card, "flavor_name").clone()),
        ("aliases", Value::Array(aliases)),
        ("image", image),
        ("faces", Value::Array(faces)),
        ("mana", g(card, "mana_cost").clone()),
        ("type", g(card, "type_line").clone()),
        ("pt", pt(card)),
        ("text", or(g(card, "oracle_text"), json!(""))),
        ("flavor", or(g(card, "flavor_text"), json!(""))),
        ("set", g(card, "set").clone()),
        ("set_name", g(card, "set_name").clone()),
        ("num", g(card, "collector_number").clone()),
        ("rarity", g(card, "rarity").clone()),
        ("colors", g(card, "colors").clone()),
        ("lang", g(card, "lang").clone()),
        ("layout", g(card, "layout").clone()),
        ("released_at", g(card, "released_at").clone()),
        ("frame_effects", or(g(card, "frame_effects"), json!([]))),
        ("border_color", g(card, "border_color").clone()),
    ])
}

fn lines(source: &Path) -> Result<Box<dyn BufRead>, String> {
    let f = std::fs::File::open(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let r = std::io::BufReader::with_capacity(1 << 20, f);
    Ok(if source.extension().is_some_and(|e| e == "gz") {
        Box::new(std::io::BufReader::with_capacity(1 << 20, flate2::read::MultiGzDecoder::new(r)))
    } else {
        Box::new(r)
    })
}

fn for_each_card(source: &Path, mut f: impl FnMut(Value)) -> Result<(), String> {
    for (i, line) in lines(source)?.lines().enumerate() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| format!("invalid JSON on line {}: {e}", i + 1))?;
        f(v);
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
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

/// Python `json.dumps(obj, indent=2)` (ensure_ascii as given).
fn py_indent2(v: &Value, ensure_ascii: bool) -> String {
    // INFALLIBLE: serde_json::Value always serialises (string keys, no custom Serialize).
    let s = serde_json::to_string_pretty(v).expect("json value serialises");
    if !ensure_ascii {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{:04x}", u));
            }
        }
    }
    out
}

/// `build_printing_index.build(source, output, metadata_output)`.
///
/// # Errors
///
/// Source/metadata read failures and output write failures (path-prefixed
/// `io::Error` messages).
pub fn build_printing_index(source: &Path, output: &Path, meta: &Path) -> Result<Value, String> {
    let mut printings: IndexMap<String, Value> = IndexMap::new();
    let (mut scanned, mut excluded) = (0u64, 0u64);
    for_each_card(source, |card| {
        scanned += 1;
        if !is_scannable_physical_card(&card) {
            excluded += 1;
            return;
        }
        let set = if truthy(g(&card, "set")) { py_fmt(g(&card, "set")).to_lowercase() } else { String::new() };
        let num = build_normalize_collector(g(&card, "collector_number"));
        if set.is_empty() || num.is_empty() {
            excluded += 1;
            return;
        }
        let key = format!("{set}:{num}");
        let cand = compact_printing(&card);
        let replace = match printings.get(&key) {
            None => true,
            Some(cur) => g(&cand, "lang") == "en" && g(cur, "lang") != "en",
        };
        if replace {
            printings.insert(key, cand);
        }
    })?;
    let codes: BTreeSet<String> = printings.keys().map(|k| k.split(':').next().unwrap_or("").to_string()).collect();
    let metadata = obj(vec![
        ("version", json!(2)),
        ("source", json!(source.to_string_lossy())),
        ("bulk_records", json!(scanned)),
        ("excluded_records", json!(excluded)),
        ("printings", json!(printings.len())),
        ("set_codes", json!(codes)),
    ]);
    atomic_write(output, &serde_json::to_vec(&printings).map_err(|e| e.to_string())?)?;
    atomic_write(meta, py_indent2(&metadata, true).as_bytes())?;
    Ok(metadata)
}

fn compact_record(card: &Value, face: Option<&Value>, idx: usize) -> Value {
    let src = face.unwrap_or(card);
    let name = or(g(src, "name"), g(card, "name").clone());
    let colors = if g(src, "colors").is_null() { g(card, "colors").clone() } else { g(src, "colors").clone() };
    let get_or = |k: &str, ck: &str| src.get(k).cloned().unwrap_or_else(|| g(card, ck).clone());
    obj(vec![
        ("id", g(card, "id").clone()),
        ("oracle_id", g(card, "oracle_id").clone()),
        ("name", name),
        ("full_name", g(card, "name").clone()),
        ("face_index", if face.is_some() { json!(idx) } else { Value::Null }),
        ("image", or(&image_url(src), image_url(card))),
        ("mana", get_or("mana_cost", "mana_cost")),
        ("type", get_or("type_line", "type_line")),
        ("pt", pt(src)),
        ("text", or(g(src, "oracle_text"), json!(""))),
        ("flavor", or(g(src, "flavor_text"), json!(""))),
        ("set", g(card, "set").clone()),
        ("set_name", g(card, "set_name").clone()),
        ("num", g(card, "collector_number").clone()),
        ("rarity", g(card, "rarity").clone()),
        ("colors", colors),
        ("lang", g(card, "lang").clone()),
        ("layout", g(card, "layout").clone()),
        ("released_at", g(card, "released_at").clone()),
    ])
}

fn priority(r: &Value) -> (bool, bool, String) {
    (truthy(g(r, "image")), g(r, "lang") == "en", if truthy(g(r, "released_at")) { py_fmt(g(r, "released_at")) } else { String::new() })
}

/// `build_oracle_index.build(source, output, metadata_output)`.
///
/// # Errors
///
/// Source read/parse failures and output write failures (path-prefixed
/// `io::Error` messages).
pub fn build_oracle_index(source: &Path, output: &Path, meta: &Path) -> Result<Value, String> {
    let mut by_title: IndexMap<String, Value> = IndexMap::new();
    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut ambiguous: BTreeSet<String> = BTreeSet::new();
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    let bump = |c: &mut HashMap<String, u64>, k: String| *c.entry(k).or_default() += 1;
    for_each_card(source, |card| {
        bump(&mut counts, "bulk_records".into());
        if !is_scannable_physical_card(&card) {
            bump(&mut counts, "excluded_records".into());
            bump(&mut counts, format!("excluded_layout:{}", py_fmt(g(&card, "layout"))));
            excluded.insert(if truthy(g(&card, "name")) { py_fmt(g(&card, "name")) } else { String::new() });
            for f in g(&card, "card_faces").as_array().into_iter().flatten() {
                excluded.insert(if truthy(g(f, "name")) { py_fmt(g(f, "name")) } else { String::new() });
            }
            return;
        }
        bump(&mut counts, "physical_cards".into());
        let faces = g(&card, "card_faces").as_array().cloned().unwrap_or_default();
        let records: Vec<Value> = if faces.is_empty() {
            vec![compact_record(&card, None, 0)]
        } else {
            faces.iter().enumerate().map(|(i, f)| compact_record(&card, Some(f), i)).collect()
        };
        let mut seen = std::collections::HashSet::new();
        for r in records {
            let title = py_fmt(g(&r, "name")).trim().to_string();
            if title.is_empty() || !seen.insert(title.clone()) {
                continue;
            }
            bump(&mut counts, "searchable_faces".into());
            let prev = by_title.get(&title);
            if let Some(p) = prev {
                if g(p, "oracle_id") != g(&r, "oracle_id") {
                    ambiguous.insert(title.clone());
                }
            }
            if prev.is_none_or(|p| priority(&r) > priority(p)) {
                by_title.insert(title, r);
            }
        }
    })?;
    let titles_out = output.with_file_name("oracle_titles.json");
    let counts: BTreeMap<String, u64> = counts.into_iter().collect();
    let metadata = obj(vec![
        ("version", json!(3)),
        ("source", json!(source.to_string_lossy())),
        ("titles_index", json!("oracle_titles.json")),
        ("cards", json!(by_title.len())),
        ("with_images", json!(by_title.values().filter(|c| truthy(g(c, "image"))).count())),
        ("with_collector_numbers", json!(by_title.values().filter(|c| truthy(g(c, "num"))).count())),
        ("multi_face_titles", json!(by_title.values().filter(|c| !g(c, "face_index").is_null()).count())),
        ("ambiguous_title_count", json!(ambiguous.len())),
        ("ambiguous_titles", json!(ambiguous)),
        ("excluded_titles", json!(excluded.iter().filter(|t| !t.is_empty() && !by_title.contains_key(*t)).collect::<Vec<_>>())),
        ("counts", json!(counts)),
    ]);
    atomic_write(output, &serde_json::to_vec(&by_title).map_err(|e| e.to_string())?)?;
    let mut titles: Vec<&String> = by_title.keys().collect();
    titles.sort();
    atomic_write(&titles_out, &serde_json::to_vec(&titles).map_err(|e| e.to_string())?)?;
    atomic_write(meta, py_indent2(&metadata, false).as_bytes())?;
    Ok(metadata)
}

/// `update_{oracle,printing}_data.main()`: fetch the bulk file named by
/// api.scryfall.com/bulk-data/<type> into `data_dir`, then rebuild.
pub async fn update(data_dir: &Path, which: Which) -> Result<Value, String> {
    let (ty, file) = match which {
        Which::Oracle => ("oracle_cards", "oracle-cards.jsonl.gz"),
        Which::Printing => ("default_cards", "default-cards.jsonl.gz"),
    };
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let c = crate::lsf_data::client();
    let url = crate::lsf_data::live_url(&c, ty).await.ok_or_else(|| format!("Scryfall did not provide a {ty} download URI"))?;
    let target = data_dir.join(file);
    let part = data_dir.join(format!("{file}.part"));
    crate::lsf_data::download(&c, &url, &part).await?;
    std::fs::rename(&part, &target).map_err(|e| e.to_string())?;
    build(data_dir, which, &target)
}

#[derive(Clone, Copy, Debug)]
pub enum Which {
    Oracle,
    Printing,
}

pub fn build(data_dir: &Path, which: Which, source: &Path) -> Result<Value, String> {
    let (s, d) = (source.to_path_buf(), data_dir.to_path_buf());
    match which {
        Which::Oracle => build_oracle_index(&s, &d.join("oracle_index.json"), &d.join("oracle_index.meta.json")),
        Which::Printing => build_printing_index(&s, &d.join("printing_index.json"), &d.join("printing_index.meta.json")),
    }
}

/// Boot job: a fresh volume has no cardscan indexes. Build the printing
/// index from the in-process Scryfall engine's own default-cards bulk when
/// present (same Scryfall file the sidecar downloads), else download; the
/// oracle title index needs the oracle-cards bulk. Runs off the request
/// path; the scan engine loads after it.
pub async fn ensure_indexes(data_dir: PathBuf) {
    let has = |f: &str| data_dir.join(f).metadata().map(|m| m.len() > 0).unwrap_or(false);
    if !has("oracle_titles.json") {
        match update(&data_dir, Which::Oracle).await {
            Ok(m) => tracing::info!("cardscan: oracle index built: {} titles", m["cards"]),
            Err(e) => tracing::warn!("cardscan: oracle index build failed: {e}"),
        }
    }
    if !has("printing_index.json") {
        let lsf = crate::lsf::configured_data_dir().map(|d| d.join("default-cards.jsonl.gz")).filter(|p| p.exists());
        let r = match lsf {
            Some(src) => {
                let d = data_dir.clone();
                tokio::task::spawn_blocking(move || build(&d, Which::Printing, &src)).await.map_err(|e| e.to_string()).and_then(|r| r)
            }
            None => update(&data_dir, Which::Printing).await,
        };
        match r {
            Ok(m) => tracing::info!("cardscan: printing index built: {} printings", m["printings"]),
            Err(e) => tracing::warn!("cardscan: printing index build failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(extra: Value) -> Value {
        let mut base = json!({"object":"card","games":["paper"],"layout":"normal","type_line":"Creature — Elf",
            "name":"Llanowar Elves","id":"a","oracle_id":"o","set":"M19","collector_number":"0314","lang":"en",
            "image_uris":{"normal":"n","large":"L"},"power":"1","toughness":"1"});
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn scannable_filter() {
        assert!(is_scannable_physical_card(&card(json!({}))));
        assert!(!is_scannable_physical_card(&card(json!({"digital": true}))));
        assert!(!is_scannable_physical_card(&card(json!({"layout": "token"}))));
        assert!(!is_scannable_physical_card(&card(json!({"type_line": "Token Creature — Elf"}))));
        assert!(!is_scannable_physical_card(&card(json!({"games": ["arena"]}))));
    }

    #[test]
    fn printing_index_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dc.jsonl");
        let lines = [card(json!({})), card(json!({"lang":"de","id":"b"})), card(json!({"layout":"token","collector_number":"1"}))];
        std::fs::write(&src, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        let m = build(dir.path(), Which::Printing, &src).unwrap();
        assert_eq!(m["printings"], 1);
        assert_eq!(m["excluded_records"], 1);
        let idx: Value = serde_json::from_slice(&std::fs::read(dir.path().join("printing_index.json")).unwrap()).unwrap();
        let p = &idx["m19:314"];
        assert_eq!(p["id"], "a");
        assert_eq!(p["image"], "L");
        assert_eq!(p["pt"], "1/1");
        assert_eq!(p["aliases"], json!(["Llanowar Elves"]));
        let o = build(dir.path(), Which::Oracle, &src).unwrap();
        assert_eq!(o["cards"], 1);
        let t: Value = serde_json::from_slice(&std::fs::read(dir.path().join("oracle_titles.json")).unwrap()).unwrap();
        assert_eq!(t, json!(["Llanowar Elves"]));
    }

    #[test]
    fn collector_build_normalisation() {
        assert_eq!(build_normalize_collector(&json!("0314")), "314");
        assert_eq!(build_normalize_collector(&json!("ODY-129")), "ody-129");
        assert_eq!(build_normalize_collector(&json!("12a3")), "12a3");
    }
}
