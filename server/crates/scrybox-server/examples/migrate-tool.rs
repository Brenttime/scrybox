//! Run the Rust migration ladder against an arbitrary DB file — the
//! DB-diff harness entry for unit 4 (Lane B brief): `cargo run --release
//! --example migrate-tool -- <db-path>` then dump/compare against the Node
//! twin (bench/mig-node-baseline.cjs) on a sibling copy.
//!
//! Env mirrors main.rs: DEFAULT_ADMIN_PASSWORD, CV_MODEL_DIR.

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: migrate-tool <db-path>");
        std::process::exit(2);
    };
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let opts = scrybox_server::migrations::MigOpts {
        admin_password: std::env::var("DEFAULT_ADMIN_PASSWORD").ok().filter(|s| !s.is_empty()),
        model_dir: std::env::var_os("CV_MODEL_DIR").map(std::path::PathBuf::from),
        user_art_dir: None,
    };
    match scrybox_server::migrations::run_boot_migration(std::path::Path::new(&path), &opts) {
        Ok(rep) => {
            println!(
                "migrations ok: skipped={} seeded_admin={} removed_models={} removed_art={}",
                rep.skipped, rep.seeded_admin, rep.removed_models, rep.removed_art
            );
        }
        Err(e) => {
            eprintln!("migrations failed: {e}");
            std::process::exit(3);
        }
    }
}
