// Debug tool: call match_scan exactly like node_truth_parity_on_eval_corpus.
// usage: scanmatch <img>
use std::path::PathBuf;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    if std::env::var("ORT_DYLIB_PATH").unwrap_or_default().is_empty() {
        // The old fallback into the retired Node backend's onnxruntime-node
        // node_modules is gone — point at the dylib explicitly.
        eprintln!(
            "scanmatch: set ORT_DYLIB_PATH to the libonnxruntime dylib \
             (e.g. $(npm root -g)/onnxruntime-node/bin/napi-v6/linux/x64/libonnxruntime.so.1)"
        );
        std::process::exit(1);
    }
    if std::env::var("CV_MODEL_DIR").unwrap_or_default().is_empty() {
        std::env::set_var("CV_MODEL_DIR", root.join("data/models"));
    }
    if std::env::var("SCRYBOX_JPEG_LIB").unwrap_or_default().is_empty() {
        std::env::set_var("SCRYBOX_JPEG_LIB", root.join("data/lib/libscryjpeg.so"));
    }
    let pool = scrybox_server::db::DbPool::open(root.join("data/bindarr.db"), 2).unwrap();
    let bytes = std::fs::read(&a[1]).unwrap();
    // Run twice on a fresh thread each: probes for per-thread MXCSR (FTZ/DAZ)
    // drift — the server answers on tokio blocking threads, the bin on main.
    for round in 0..2 {
        let out = std::thread::scope(|s| {
            s.spawn(|| {
                scrybox_server::cvscan::match_scan(&bytes, "mtg", 8, &[], Some("English"), false, &pool)
            })
            .join()
            .unwrap()
            .unwrap_or_else(|e| panic!("match_scan: {}", e.0))
        });
        println!("round {round} {}", serde_json::to_string(&out).unwrap());
    }
}
