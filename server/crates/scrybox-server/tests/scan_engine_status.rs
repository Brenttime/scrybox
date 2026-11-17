//! BUG-2 (cutfix): the first /api/health after boot must not answer the
//! lazy-load placeholder 503 "uninitialized" — the scanner page reads one
//! status probe on mount and showed "Fast scan service is offline" until a
//! reload. The probe now triggers the load and waits for it (bounded).
//! Own binary: env flips.
use scrybox_server::scan_engine;

#[test]
fn first_health_probe_waits_for_engine_load_instead_of_uninitialized() {
    let dir = std::env::temp_dir().join(format!("scan-status-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: single test in this binary.
    unsafe {
        std::env::remove_var("CARDSCAN_URL");
        std::env::set_var("SCRYBOX_CARDSCAN_DATA", &dir); // empty: load resolves to an error
    }
    let r = scan_engine::handle("GET", "/api/health", &[], b"");
    if let Some((status, body)) = &r {
        assert_ne!(body["ocr_backend"], "uninitialized", "status {status}: lazy placeholder leaked: {body}");
    }
    // An engine that cannot load maps to the proxy's 'not running' arm.
    assert!(r.is_none(), "resolved load failure must be None, got {r:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
