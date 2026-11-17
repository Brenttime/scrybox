//! Nightly scan-index refresh: skip / never-smaller / atomic swap /
//! rollback-on-failure, plus the build_client_index.py byte format.

use super::*;
use serde_json::json;
use std::io::Read;

fn card(name: &str, id: &str, set: &str, num: &str) -> Value {
    json!({"object":"card","games":["paper"],"layout":"normal","type_line":"Creature — Elf",
        "name":name,"id":id,"oracle_id":format!("o-{id}"),"set":set,"set_name":"Core Set","collector_number":num,"lang":"en",
        "released_at":"2019-07-12","border_color":"black","frame":"2015","finishes":["nonfoil","foil"],
        "image_uris":{"normal":"n","large":"L"},"power":"1","toughness":"1"})
}

fn write_jsonl(path: &Path, cards: &[Value]) {
    let body = cards.iter().map(|c| c.to_string()).collect::<Vec<_>>().join("\n");
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(body.as_bytes()).unwrap();
    std::fs::write(path, e.finish().unwrap()).unwrap();
}

struct Fx {
    _t: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    scan: PathBuf,
    model: PathBuf,
}

fn fx() -> Fx {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().to_path_buf();
    let (data, scan) = (root.join("data"), root.join("client-scan"));
    std::fs::create_dir_all(&data).unwrap();
    let model = root.join("PP-OCRv6_rec_small.onnx");
    std::fs::write(&model, b"fake-onnx-model").unwrap();
    Fx { _t: t, root, data, scan, model }
}

fn cards(n: usize) -> Vec<Value> {
    (0..n).map(|i| card(&format!("Card {i}"), &format!("id{i}"), "m19", &format!("{}", i + 1))).collect()
}

impl Fx {
    fn cfg(&self, cs: &[Value], force: bool) -> RunCfg {
        let (o, p) = (self.root.join("oracle.jsonl.gz"), self.root.join("default.jsonl.gz"));
        write_jsonl(&o, cs);
        write_jsonl(&p, cs);
        RunCfg {
            data_dir: self.data.clone(),
            scan_dir: self.scan.clone(),
            oracle_src: o,
            printing_src: p,
            rec: Some(RecAssets::new(&self.model, "a\nb\nc").unwrap()),
            force,
        }
    }
    fn snapshot(&self) -> Vec<(String, Vec<u8>)> {
        let mut v = Vec::new();
        for d in [&self.data, &self.scan] {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    if e.path().is_file() {
                        v.push((e.path().to_string_lossy().to_string(), std::fs::read(e.path()).unwrap()));
                    }
                }
            }
        }
        v.sort();
        v
    }
}

fn no_install(_: Oracle, _: Printings) -> Result<bool, String> {
    Ok(false)
}

fn manifest(f: &Fx) -> Value {
    serde_json::from_slice(&std::fs::read(f.scan.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn first_run_publishes_then_skip_when_nothing_added() {
    let f = fx();
    let r = run_blocking(&f.cfg(&cards(3), false), &no_install).unwrap();
    assert!(matches!(r.outcome, Outcome::Published { backup: None, .. }), "{r:?}");
    assert_eq!(r.after, (3, 3));
    let m = manifest(&f);
    assert!(f.scan.join(m["index"].as_str().unwrap()).exists());
    assert!(f.scan.join(m["rec"].as_str().unwrap()).exists());
    let snap = f.snapshot();
    // Same bulk again: nothing added → skip, nothing touched.
    let installed = std::cell::Cell::new(false);
    let r = run_blocking(&f.cfg(&cards(3), false), &|_, _| {
        installed.set(true);
        Ok(true)
    })
    .unwrap();
    assert_eq!(r.outcome, Outcome::Skipped);
    assert!(!installed.get(), "skip must not swap the engine");
    assert_eq!(f.snapshot(), snap, "skip must not touch data or client-scan files");
    // A new card → swap + publish with a manifest backup, old index kept.
    let old_index = m["index"].as_str().unwrap().to_string();
    let r = run_blocking(&f.cfg(&cards(4), false), &no_install).unwrap();
    let Outcome::Published { index, backup: Some(bak) } = r.outcome else { panic!("{r:?}") };
    assert_ne!(index, old_index);
    assert_eq!(std::fs::read(f.scan.join(&bak)).unwrap(), snap.iter().find(|(p, _)| p.ends_with("/manifest.json")).unwrap().1);
    assert_eq!(r.client_before.as_ref().unwrap().2, 3);
    assert_eq!(r.client_after.as_ref().unwrap().2, 4);
    assert!(f.scan.join(&old_index).exists(), "previous index kept for rollback");
    assert_eq!(counts(&f.data), (4, 4));
}

#[test]
fn prunes_all_but_live_and_previous_index() {
    let f = fx();
    run_blocking(&f.cfg(&cards(2), false), &no_install).unwrap();
    let first = manifest(&f)["index"].as_str().unwrap().to_string();
    run_blocking(&f.cfg(&cards(3), false), &no_install).unwrap();
    let second = manifest(&f)["index"].as_str().unwrap().to_string();
    run_blocking(&f.cfg(&cards(4), false), &no_install).unwrap();
    let third = manifest(&f)["index"].as_str().unwrap().to_string();
    let mut idx: Vec<String> = std::fs::read_dir(&f.scan)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("scan-index."))
        .collect();
    idx.sort();
    let mut want = vec![second, third];
    want.sort();
    assert_eq!(idx, want);
    assert!(!f.scan.join(first).exists());
}

#[test]
fn never_publishes_a_smaller_client_index() {
    let f = fx();
    run_blocking(&f.cfg(&cards(3), false), &no_install).unwrap();
    // The live phone index is bigger than anything we will build.
    let big = json!({"v":1,"names":vec!["x";50],"printings":vec![json!(["a","b","1",0,0]);50]}).to_string();
    let gz = gzip_py(big.as_bytes());
    let name = "scan-index.bigbigbigbig.json.gz";
    std::fs::write(f.scan.join(name), &gz).unwrap();
    let mut m = manifest(&f);
    m["index"] = json!(name);
    std::fs::write(f.scan.join("manifest.json"), m.to_string()).unwrap();
    let snap_scan: Vec<_> = f.snapshot().into_iter().filter(|(p, _)| p.contains("client-scan")).collect();
    let e = run_blocking(&f.cfg(&cards(4), false), &no_install).unwrap_err();
    assert!(e.starts_with("REFUSED: new index smaller"), "{e}");
    let after: Vec<_> = f.snapshot().into_iter().filter(|(p, _)| p.contains("client-scan")).collect();
    assert_eq!(after, snap_scan, "a refused publish leaves the live client-scan dir untouched");
}

#[test]
fn refuses_smaller_cardscan_rebuild_and_keeps_old() {
    let f = fx();
    run_blocking(&f.cfg(&cards(5), false), &no_install).unwrap();
    let snap = f.snapshot();
    let e = run_blocking(&f.cfg(&cards(2), false), &no_install).unwrap_err();
    assert!(e.contains("REFUSED"), "{e}");
    assert_eq!(f.snapshot(), snap);
    assert!(!f.data.join(".nightly-staging").exists());
}

#[test]
fn refuses_recognizer_change() {
    let f = fx();
    run_blocking(&f.cfg(&cards(2), false), &no_install).unwrap();
    std::fs::write(&f.model, b"another-model").unwrap();
    let e = run_blocking(&f.cfg(&cards(3), false), &no_install).unwrap_err();
    assert_eq!(e, "REFUSED: recognizer/dict changed; publish by hand");
}

#[test]
fn rollback_on_corrupt_source_keeps_files_and_engine() {
    let f = fx();
    run_blocking(&f.cfg(&cards(3), false), &no_install).unwrap();
    let snap = f.snapshot();
    let mut cfg = f.cfg(&cards(4), false);
    std::fs::write(&cfg.printing_src, b"\x1f\x8bthis is not gzip").unwrap();
    cfg.printing_src = cfg.printing_src.clone();
    let called = std::cell::Cell::new(false);
    let e = run_blocking(&cfg, &|_, _| {
        called.set(true);
        Ok(true)
    })
    .unwrap_err();
    assert!(e.contains("old kept"), "{e}");
    assert!(!called.get(), "a failed build must not touch the live engine");
    assert_eq!(f.snapshot(), snap);
}

#[test]
fn rollback_on_mid_swap_failure_restores_previous_indexes() {
    let f = fx();
    run_blocking(&f.cfg(&cards(3), false), &no_install).unwrap();
    let snap = f.snapshot();
    FAIL_AT.with(|x| *x.borrow_mut() = Some("printing_index.json"));
    let called = std::cell::Cell::new(false);
    let e = run_blocking(&f.cfg(&cards(4), false), &|_, _| {
        called.set(true);
        Ok(true)
    })
    .unwrap_err();
    FAIL_AT.with(|x| *x.borrow_mut() = None);
    assert!(e.contains("previous indexes restored"), "{e}");
    assert!(!called.get());
    assert_eq!(f.snapshot(), snap, "files must be exactly the previous set");
    // And the next pass recovers.
    let r = run_blocking(&f.cfg(&cards(4), false), &no_install).unwrap();
    assert!(matches!(r.outcome, Outcome::Published { .. }));
}

#[test]
fn engine_hot_swap_is_atomic_and_receives_new_indexes() {
    // Slot semantics: readers holding the old Arc keep it; new readers see
    // the new value; a failed load is replaced by install (auto-recovery).
    let slot: crate::scan_engine::HotSlot<String> = crate::scan_engine::HotSlot::new();
    assert!(slot.init_with(|| Err("boom".into())).is_err());
    assert!(slot.live().is_none());
    slot.install("v1".into());
    let held = slot.live().unwrap();
    let prev = slot.install("v2".into()).unwrap();
    assert_eq!(*prev, "v1");
    assert_eq!(*held, "v1");
    assert_eq!(*slot.live().unwrap(), "v2");
    // The pass hands the freshly loaded indexes (not the old ones) to the
    // installer, after the on-disk swap.
    let f = fx();
    run_blocking(&f.cfg(&cards(3), false), &no_install).unwrap();
    let seen = std::cell::RefCell::new(None);
    let r = run_blocking(&f.cfg(&cards(6), false), &|o, p| {
        assert_eq!(counts(&f.data), (6, 6), "files swapped before the engine");
        *seen.borrow_mut() = Some((o.norm_list.len(), p.len()));
        Ok(true)
    })
    .unwrap();
    assert!(r.engine_swapped);
    assert_eq!(*seen.borrow(), Some((6, 6)));
    assert!(f.data.join(".prev/printing_index.json").exists(), "previous indexes kept for rollback");
}

#[test]
fn python_byte_format() {
    // gzip.compress(raw, 9, mtime=0) header (CPython 3.12) + roundtrip.
    let gz = gzip_py(b"{\"v\":1}");
    assert_eq!(&gz[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 2, 3]);
    let mut out = String::new();
    flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut out).unwrap();
    assert_eq!(out, "{\"v\":1}");
    assert_eq!(
        manifest_json("scan-index.a.json.gz", "m.b.onnx", "rec-dict.c.txt", 42),
        "{\n \"v\": 1,\n \"index\": \"scan-index.a.json.gz\",\n \"rec\": \"m.b.onnx\",\n \"dict\": \"rec-dict.c.txt\",\n \"bytes\": 42\n}"
    );
    assert!(ff_tag("ffvii") && ff_tag("fftactics") && !ff_tag("ff") && !ff_tag("ffz"));
    assert_eq!(released(&json!("2024-08-02")), 20240802);
    assert_eq!(released(&json!("2024-8-2")), 0);
}

#[test]
fn client_index_columns_and_flags() {
    let f = fx();
    let mut cs = cards(2);
    cs[1]["border_color"] = json!("borderless");
    cs[1]["set"] = json!("pm19");
    cs[1]["frame"] = json!("1997");
    cs[1]["finishes"] = json!(["foil"]);
    run_blocking(&f.cfg(&cs, false), &no_install).unwrap();
    let m = manifest(&f);
    let mut raw = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(f.scan.join(m["index"].as_str().unwrap())).unwrap()).read_to_string(&mut raw).unwrap();
    let v: Value = serde_json::from_str(&raw).unwrap();
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["v", "names", "canon", "excluded", "sets", "printings", "byTitle", "uniqueAlias"]);
    assert_eq!(v["printings"][0], json!(["id0", "m19", "1", 20190712, 0]));
    assert_eq!(v["printings"][1], json!(["id1", "pm19", "2", 20190712, 1 | 2 | 4 | 8]));
    assert_eq!(v["byTitle"]["card 0"], json!([0]));
}

#[test]
fn gzip_matches_cpython_zlib_bytes() {
    // sha256(gzip.compress(raw, 9, mtime=0)) from CPython 3.12.15 / zlib
    // 1.3.1 (cardscan:local-scanspeed). zlib-rs (flate2's default backend)
    // produced a different stream for the real index — this pins stock zlib.
    let mut raw = Vec::new();
    for i in 0..20000u64 {
        raw.extend(format!("card {} scan-index é ", i * 7919 % 1000).as_bytes());
    }
    let h = Sha256::digest(gzip_py(&raw));
    let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "71d751e624fd4bee6083563e0e834b9c06f4b31e5dabd4de1e93997cb309652e");
}
