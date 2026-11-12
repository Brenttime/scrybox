use std::net::SocketAddr;

use scrybox_server::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        // App-target default, NOT bare "info": the bare default also enables
        // tower_http's per-request INFO span events, which meant every API
        // request paid a fmt+write to journald on a runtime worker (~9k
        // lines/s during the hot bench — iter 6). Route errors still log
        // (they are scrybox_server:: events at warn/error).
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "scrybox_server=info".into()),
        )
        .init();
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("scrybox-server: config error: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(bind = %cfg.bind, upstream = %cfg.node_upstream, static_dir = ?cfg.static_dir, rust_routes = ?cfg.rust_routes, "scrybox-server P0 starting");
    // Lane B unit 4: Rust-owned schema baseline + migrations (twin of
    // backend/src/db.js initDb). Off unless RUST_MIGRATIONS=true; the test
    // container pairs it with SCRYBOX_SKIP_DB_INIT=true on Node so exactly
    // one side runs the ladder. Skips via PRAGMA user_version once applied.
    if std::env::var("RUST_MIGRATIONS").unwrap_or_default() == "true" {
        match cfg.db_path.clone() {
            Some(path) => {
                let opts = scrybox_server::migrations::MigOpts {
                    admin_password: std::env::var("DEFAULT_ADMIN_PASSWORD").ok().filter(|s| !s.is_empty()),
                    model_dir: std::env::var_os("CV_MODEL_DIR").map(std::path::PathBuf::from),
                    user_art_dir: None,
                };
                if opts.model_dir.is_none() {
                    // Node's cardArt.js sweeps `backend/data/models` when
                    // CV_MODEL_DIR is unset (path relative to the Node
                    // sources, unknowable from the Rust front). The scrybox
                    // image always sets it (Dockerfile ENV); a bare source
                    // install must too, or the retired-model sweep is lost.
                    tracing::warn!(
                        "RUST_MIGRATIONS without CV_MODEL_DIR: retired-model sweep \
                         skipped (Node default backend/data/models is unreachable \
                         from the Rust front)"
                    );
                }
                match scrybox_server::migrations::run_boot_migration(&path, &opts) {
                    Ok(rep) => tracing::info!(
                        db = %path.display(),
                        skipped = rep.skipped,
                        seeded_admin = rep.seeded_admin,
                        removed_models = rep.removed_models,
                        removed_art = rep.removed_art,
                        "Rust migrations complete (RUST_MIGRATIONS=true)"
                    ),
                    Err(e) => {
                        eprintln!("scrybox-server: migrations failed: {e}");
                        std::process::exit(3);
                    }
                }
            }
            None => tracing::warn!("RUST_MIGRATIONS set but SCRYBOX_DB_PATH missing; migrations not run"),
        }
    }
    // Lane B background jobs (BRIEF-B): RUST_JOBS=scryfall takes the
    // weekly set-sync / hourly price-sweep timers from Node's server.js
    // (which the test container disables via SCRYBOX_DISABLE_SETS_REFRESH /
    // SCRYBOX_DISABLE_PRICE_SWEEP when this is on). Off unless set.
    if std::env::var("RUST_JOBS").unwrap_or_default().split(',').any(|s| s.trim() == "scryfall") {
        match cfg.db_path.clone() {
            Some(path) => {
                tracing::info!(db = %path.display(), "Rust Scryfall jobs enabled (RUST_JOBS=scryfall)");
                scrybox_server::scry_jobs::start_job_timers(path);
            }
            None => tracing::warn!("RUST_JOBS=scryfall set but SCRYBOX_DB_PATH missing; jobs not started"),
        }
    }
    let listener = tokio::net::TcpListener::bind(cfg.bind).await.expect("bind");
    let state = scrybox_server::AppState::new(cfg.clone());
    let router = scrybox_server::app(state);
    // Lane B unit 5c: optional HTTPS listener (utils/tls.js twin). A failure
    // — unreadable operator pair, no DB path, bind error — logs Node's line
    // and the plain-HTTP front keeps serving; a TLS crash can never take it
    // down. HTTPS_PORT unset (test harness default) = no listener, matching
    // Node startHttps's early return.
    if !std::env::var("HTTPS_PORT").unwrap_or_default().is_empty() {
        let port: u16 = match std::env::var("HTTPS_PORT").unwrap().parse() {
            Ok(p) => p,
            Err(_) => {
                tracing::error!("HTTPS listener failed to start: HTTPS_PORT is not a number");
                0
            }
        };
        if port != 0 {
            let addr = std::net::SocketAddr::new(cfg.bind.ip(), port);
            let db = cfg.db_path.clone();
            let rt = router.clone();
            tokio::spawn(async move {
                if let Err(e) = scrybox_server::tls::serve(addr, db, rt).await {
                    tracing::error!("HTTPS listener failed to start: {e}");
                }
            });
        }
    }
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}
