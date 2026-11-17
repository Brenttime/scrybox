//! TOTORO mono scan2: in-process cardscan endpoints vs the Python sidecar
//! (cv2 5.0.0, image cardscan:local-scanspeed). Fixtures in
//! tests/fixtures/scan_engine were captured from the throwaway reference
//! container by bench/scan-engine/py_drive.py.

use scrybox_server::scan_engine::{self, scene};
use serde_json::Value;
use std::path::PathBuf;

fn fx(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scan_engine").join(name)
}

fn cases() -> Vec<Value> {
    serde_json::from_slice(&std::fs::read(fx("endpoints.json")).unwrap()).unwrap()
}

/// Exact JSON equality, except floats compare to 1e-12 relative: serde_json
/// without `float_roundtrip` may parse a 17-digit Python repr 1 ulp off.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            (x - y).abs() <= 1e-12 * x.abs().max(1.0)
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b)),
        (Value::Object(x), Value::Object(y)) => x.keys().eq(y.keys()) && x.iter().all(|(k, v)| same(v, &y[k])),
        _ => a == b,
    }
}

#[test]
fn detect_cards_json_matches_python() {
    for c in cases() {
        let raw = std::fs::read(fx(c["frame"].as_str().unwrap())).unwrap();
        let d = scene::detect_uploaded_scene(&raw, false).unwrap();
        let got = Value::Object(scene::detection_json(&d));
        let want = &c["detect"];
        assert!(same(&got["candidates"], &want["candidates"]), "{}: {} vs {}", c["frame"], got["candidates"], want["candidates"]);
        for (k, v) in got.as_object().unwrap() {
            if let Some(w) = want.get(k) {
                assert!(same(v, w), "{} key {k}", c["frame"]);
            }
        }
    }
}

#[test]
fn scan_frame_identity_matches_python() {
    // Needs the OCR model + cardscan indexes (SCRYBOX_OCR_MODEL_DIR,
    // SCRYBOX_CARDSCAN_DATA, ORT_DYLIB_PATH, SCRYBOX_JPEG_LIB).
    if scan_engine::engine().is_err() {
        assert!(std::env::var("SCRYBOX_ALLOW_EVAL_SKIP").is_ok(), "scan engine assets missing; set SCRYBOX_ALLOW_EVAL_SKIP=1 to skip");
        eprintln!("skip: scan engine assets not configured");
        return;
    }
    for c in cases() {
        let raw = std::fs::read(fx(c["frame"].as_str().unwrap())).unwrap();
        let hdr = vec![("content-type".to_string(), "image/jpeg".to_string())];
        let (status, j) = scan_engine::handle("POST", "/api/scan-frame", &hdr, &raw).unwrap();
        assert_eq!(status, 200);
        let got: Vec<Value> = j["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| serde_json::json!([x["scene_number"], x["ok"].as_bool().unwrap_or(false), x["card"].get("id").cloned().unwrap_or(Value::Null), x.get("title").cloned().unwrap_or(Value::Null)]))
            .collect();
        assert_eq!(Value::Array(got), c["scan_frame_identity"], "{}", c["frame"]);
    }
}
