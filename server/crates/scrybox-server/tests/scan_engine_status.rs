//! BUG-2 (cutfix): the first /api/health after boot must not answer the
//! lazy-load placeholder 503 "uninitialized" — the scanner page reads one
//! status probe on mount and showed "Fast scan service is offline" until a
//! reload. The probe now triggers the load and waits for it (bounded).
//! Own binary: env flips.
use scrybox_server::scan_engine;

#[test]
fn first_health_probe_waits_for_engine_load_instead_of_uninitialized() {
    // LANE-B iter-68 (REVIEW LANE-A iter-25 probe) — unconfigured shape,
    // run FIRST and sequentially in this env-flip binary (single test,
    // ENGINE OnceLock untouched until the real-dir leg below).
    // SCRYBOX_CARDSCAN_DATA unset: spawn_warm() early-returns without ever
    // setting ENGINE, so pre-fix the probe spun the full STATUS_LOAD_WAIT
    // (20.02 s measured live) and answered the 503 "loading" placeholder.
    // Node's twin answers the proxy's immediate 503 'Scanner service is not
    // running' arm (handle -> None). The leg must not touch the ENGINE
    // Err-cache (no engine() call), or the empty-dir leg below would see a
    // cached 'SCRYBOX_CARDSCAN_DATA unset' error instead of its own load
    // failure.
    unsafe {
        std::env::remove_var("CARDSCAN_URL");
        std::env::remove_var("SCRYBOX_CARDSCAN_DATA");
    }
    let t0 = std::time::Instant::now();
    let r = scan_engine::handle("GET", "/api/health", &[], b"");
    let elapsed = t0.elapsed();
    assert!(r.is_none(), "unconfigured deploy must map to the proxy's 'not running' arm, got {r:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "unconfigured probe must answer immediately (Node: instant ECONNREFUSED 503), took {elapsed:?} — STATUS_LOAD_WAIT spin regression",
    );

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
