//! In-process vs explicit CARDSCAN_URL sidecar selection (own binary: env flips).
use scrybox_server::scan_engine;

#[test]
fn http_mode_only_with_explicit_cardscan_url() {
    // SAFETY: test-local env flip; no other test in this binary reads it.
    unsafe { std::env::remove_var("CARDSCAN_URL") };
    assert!(!scan_engine::http_mode());
    unsafe { std::env::set_var("CARDSCAN_URL", "http://127.0.0.1:9") };
    assert!(scan_engine::http_mode());
    assert!(scan_engine::handle("GET", "/api/health", &[], b"").is_none(), "http mode must not serve in-process");
    unsafe { std::env::remove_var("CARDSCAN_URL") };
}
