//! TOTORO mono scan2 differential runner: replays `requests.jsonl` written by
//! bench/scan-engine/py_drive.py through the in-process engine
//! (`scan_engine::handle`) and prints one JSON line per request.
//!
//! usage: scan_engine_eval <diff_dir> > rust.jsonl
//! Needs ORT_DYLIB_PATH, SCRYBOX_JPEG_LIB, SCRYBOX_OCR_MODEL_DIR and
//! SCRYBOX_CARDSCAN_DATA (oracle + printing indexes).

use scrybox_server::scan_engine;
use std::io::BufRead;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--decode") {
        // raw RGB dump of the engine's JPEG decode (decoder parity check)
        let img = scan_engine::scene::decode(&std::fs::read(&args[2]).unwrap()).unwrap();
        std::fs::write(&args[3], &img.px).unwrap();
        println!("{} {}", img.w, img.h);
        return;
    }
    let dir = args.get(1).expect("diff dir").clone();
    let t0 = Instant::now();
    scan_engine::engine().expect("engine");
    eprintln!("engine load {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
    let f = std::fs::File::open(format!("{dir}/requests.jsonl")).expect("requests");
    for line in std::io::BufReader::new(f).lines() {
        let r: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let body = std::fs::read(format!("{dir}/{}", r["body"].as_str().unwrap())).unwrap();
        let headers: Vec<(String, String)> = r["headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect();
        let t = Instant::now();
        let (status, json) = scan_engine::handle("POST", r["path"].as_str().unwrap(), &headers, &body).expect("engine");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("{}", serde_json::json!({"id": r["id"], "status": status, "ms": ms, "json": json}));
    }
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for l in s.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS")) {
            eprintln!("{l}");
        }
    }
}
