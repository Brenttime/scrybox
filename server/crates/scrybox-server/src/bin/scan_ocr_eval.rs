//! Differential harness for the in-process cardscan OCR (TOTORO mono scan).
//!
//!   scan-ocr-eval <corpus_dir> <python_ref_dir> > rust.json
//!
//! corpus_dir/manifest.json: [{frame, boxes:[[x,y,w,h]..]}]; python_ref_dir
//! holds ref.json + strips/<frame>_<card>.png from the sidecar harness.
//! Needs ORT_DYLIB_PATH, SCRYBOX_OCR_MODEL_DIR, SCRYBOX_CARDSCAN_DATA.

use scrybox_server::cvscan::decode_rgb;
use scrybox_server::scan_ocr::{self, pil::Rgb, rec};
use serde_json::{json, Value};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let corpus = std::path::PathBuf::from(&a[1]);
    let refdir = std::path::PathBuf::from(&a[2]);
    let man: Vec<Value> = serde_json::from_slice(&std::fs::read(corpus.join("manifest.json")).unwrap()).unwrap();
    let eng = rec::engine().expect("rec engine");
    let oracle = scan_ocr::oracle().expect("oracle");
    // warm like init_ocr
    let _ = eng.recognize(&[Rgb { w: 512, h: 64, px: vec![255; 512 * 64 * 3] }]);
    let mut out = vec![];
    let (mut strips_eq, mut strips_n, mut maxdiff) = (0, 0, 0u8);
    for (fi, item) in man.iter().enumerate() {
        let raw = std::fs::read(corpus.join(item["frame"].as_str().unwrap())).unwrap();
        let d = decode_rgb(&raw).unwrap();
        let img = Rgb { w: d.w, h: d.h, px: d.px };
        let crops: Vec<Rgb> = item["boxes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                let v: Vec<i64> = b.as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
                img.crop(v[0], v[1], v[0] + v[2], v[1] + v[3])
            })
            .collect();
        let mut strips = vec![];
        for (ci, c) in crops.iter().enumerate() {
            let s = scan_ocr::title_strip(c, 0.030, 0.82, 0.025, 0.100, 96).unwrap();
            if let Ok(p) = image::open(refdir.join(format!("strips/{fi}_{ci}.png"))) {
                let p = p.to_rgb8();
                strips_n += 1;
                if p.width() as usize == s.w && p.height() as usize == s.h {
                    let md = p.as_raw().iter().zip(&s.px).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
                    maxdiff = maxdiff.max(md);
                    if md == 0 {
                        strips_eq += 1;
                    }
                } else {
                    maxdiff = 255;
                }
            }
            strips.push(s);
        }
        let bgr: Vec<Rgb> = strips.iter().map(|s| s.swap_rb()).collect();
        let t0 = Instant::now();
        let batch = eng.recognize(&bgr).unwrap();
        let t1 = Instant::now();
        let single: Vec<(String, f64)> = bgr.iter().map(|s| eng.recognize(std::slice::from_ref(s)).unwrap().remove(0)).collect();
        let t2 = Instant::now();
        let titles = scan_ocr::batch_scene_titles(&crops, oracle, &|x| eng.recognize(x)).unwrap();
        let t3 = Instant::now();
        out.push(json!({
            "frame": item["frame"],
            "batch_strip": batch.iter().map(|(t, s)| json!([t, s])).collect::<Vec<_>>(),
            "single_strip": single.iter().map(|(t, s)| json!([t, s])).collect::<Vec<_>>(),
            "match": batch.iter().map(|(t, _)| { let (n, s) = oracle.find_card_by_ocr(t, 0.9); json!([n, s]) }).collect::<Vec<_>>(),
            "titles": titles.iter().map(|t| json!([t.name, t.score, t.raw, t.angle])).collect::<Vec<_>>(),
            "ms": {"batch_rec": (t1 - t0).as_secs_f64() * 1e3, "titles": (t3 - t2).as_secs_f64() * 1e3},
        }));
    }
    eprintln!("strips byte-identical {strips_eq}/{strips_n} (max channel diff {maxdiff})");
    // OpenCV INTER_LINEAR parity on the Python strips (ref/cv/<b>_{a,l,u}.png).
    let (mut cv_eq, mut cv_n, mut cv_md) = (0, 0, 0u8);
    if let Ok(rd) = std::fs::read_dir(refdir.join("strips")) {
        for e in rd.flatten() {
            let p = e.path();
            let b = p.file_stem().unwrap().to_string_lossy().to_string();
            let s = image::open(&p).unwrap().to_rgb8();
            let src = Rgb { w: s.width() as usize, h: s.height() as usize, px: s.into_raw() };
            for k in ["a", "l", "u"] {
                let Ok(r) = image::open(refdir.join(format!("cv/{b}_{k}.png"))) else { continue };
                let r = r.to_rgb8();
                let mine = scan_ocr::cvresize::resize_linear(&src, r.width() as usize, r.height() as usize);
                let md = r.as_raw().iter().zip(&mine.px).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
                cv_n += 1;
                cv_md = cv_md.max(md);
                if md == 0 {
                    cv_eq += 1;
                } else if cv_n - cv_eq <= 3 {
                    eprintln!("cv diff {b}_{k} max {md}");
                }
            }
        }
    }
    eprintln!("cv2.resize byte-identical {cv_eq}/{cv_n} (max channel diff {cv_md})");
    println!("{}", serde_json::to_string(&out).unwrap());
}
