//! Secret Lair "The Zeta Set" (slz) regression frames: cream paper edge to
//! edge (no dark border or frame), a large serif title that wraps long names
//! onto two lines, and a `C 0031` / `SLZ • EN` footer with no printed total.
//! Frames are 1920x1080 webcam-style scenes synthesised from the Scryfall
//! scans (card ~600 px tall).

use scrybox_server::scan_engine::{self, scene};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

/// `/api/scan-frame` answers 429 while another scan holds the engine; the
/// identify tests share one engine, so run them one at a time.
static ENGINE: Mutex<()> = Mutex::new(());

fn fx(name: &str) -> Vec<u8> {
    std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scan_engine").join(name)).unwrap()
}

fn ready(frame: &str) -> usize {
    let d = scene::detect_uploaded_scene(&fx(frame), false).unwrap();
    d.candidates.iter().filter(|c| c.get("eligible").and_then(Value::as_bool) == Some(true)).count()
}

#[test]
fn borderless_zeta_cards_are_detected() {
    // CMYK-stripe cream card on a grey table: only the outer-edge pass can
    // find it, and the card is bigger than 10% of the frame.
    assert_eq!(ready("zeta_cathartic_cmyk.jpg"), 1);
    assert_eq!(ready("zeta_oboro.jpg"), 1);
    assert_eq!(ready("zeta_thought_scour.jpg"), 1);
    assert_eq!(ready("zeta_cathartic_black.jpg"), 1);
}

#[test]
fn zeta_frames_identify_exact_printing() {
    let _g = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if scan_engine::engine().is_err() {
        assert!(std::env::var("SCRYBOX_ALLOW_EVAL_SKIP").is_ok(), "scan engine assets missing; set SCRYBOX_ALLOW_EVAL_SKIP=1 to skip");
        eprintln!("skip: scan engine assets not configured");
        return;
    }
    let cases = [
        // two-line title "Oboro, Palace in / the Clouds", legendary land
        ("zeta_oboro.jpg", "7cc24cab-157e-4487-9f6d-264e00ef5de4", "Oboro, Palace in the Clouds"),
        ("zeta_thought_scour.jpg", "5f32c119-009e-4368-a4f3-827829a066cd", "Thought Scour"),
        ("zeta_cathartic_cmyk.jpg", "9e7baa5f-f505-4200-9a38-465bdca8ad76", "Cathartic Reunion"),
        // footer `C 0055` read as letters (`c00ss`): padded-number readings
        ("zeta_cathartic_black.jpg", "b67f7944-ca6d-4fec-a488-5895a4b20751", "Cathartic Reunion"),
    ];
    for (frame, id, name) in cases {
        let hdr = vec![("content-type".to_string(), "image/jpeg".to_string())];
        let (status, j) = scan_engine::handle("POST", "/api/scan-frame", &hdr, &fx(frame)).unwrap();
        assert_eq!(status, 200);
        let hits: Vec<(String, String)> = j["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["ok"].as_bool() == Some(true))
            .map(|r| (r["card"]["id"].as_str().unwrap_or("").to_string(), r["card"]["name"].as_str().unwrap_or("").to_string()))
            .collect();
        assert_eq!(hits, vec![(id.to_string(), name.to_string())], "{frame}: {}", j["results"]);
    }
}

/// Light/cream Zeta cards near (not touching) the frame edge, or held close:
/// the outer-edge outline's expansion margin reached the 1% edge band and
/// the whole upload came back `touches frame edge` with no read.
#[test]
fn near_edge_zeta_cards_are_ready() {
    for f in ["zeta_animate_dead_light_left.jpg", "zeta_raffine_light_close.jpg", "zeta_signet_wood_right.jpg"] {
        assert_eq!(ready(f), 1, "{f}");
    }
}

#[test]
fn near_edge_zeta_cards_identify() {
    let _g = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if scan_engine::engine().is_err() {
        assert!(std::env::var("SCRYBOX_ALLOW_EVAL_SKIP").is_ok(), "scan engine assets missing; set SCRYBOX_ALLOW_EVAL_SKIP=1 to skip");
        return;
    }
    let cases = [
        ("zeta_animate_dead_light_left.jpg", Some("d5d9bc32-e2d6-43e6-bb32-4237a795abfa")),
        // two-line title "Raffine, / Scheming Seer"
        ("zeta_raffine_light_close.jpg", Some("19c4dcd3-4cad-4bb4-9c3a-00362254f577")),
        ("zeta_signet_wood_right.jpg", Some("93ad8d8c-afaf-4525-907c-2521aebcd7da")),
    ];
    for (frame, want) in cases {
        let hdr = vec![("content-type".to_string(), "image/jpeg".to_string())];
        let (_, j) = scan_engine::handle("POST", "/api/scan-frame", &hdr, &fx(frame)).unwrap();
        let hits: Vec<String> =
            j["results"].as_array().unwrap().iter().filter(|r| r["ok"].as_bool() == Some(true)).map(|r| r["card"]["id"].as_str().unwrap_or("").to_string()).collect();
        assert_eq!(hits, vec![want.unwrap().to_string()], "{frame}: {}", j["results"]);
    }
}
