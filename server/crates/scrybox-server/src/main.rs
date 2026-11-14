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
    // server.js 274-277 boot twin of splitStackedEntries (lane-B iter-28,
    // found by bench/manabox-db-diff.py): Node un-stacks every quantity>1
    // collection row into qty-1 rows on EVERY boot, OUTSIDE initDb — so this
    // runs unconditionally here, not under RUST_MIGRATIONS (pair mode: both
    // boots perform it, idempotent; Node-free installs: Rust must). Failure
    // is logged, not fatal — mirrors Node treating the split as a boot step.
    if let Some(path) = cfg.db_path.clone() {
        match rusqlite::Connection::open(&path)
            .and_then(|c| {
                c.busy_timeout(std::time::Duration::from_millis(5000))?;
                Ok(c)
            })
            .and_then(|c| scrybox_server::migrations::split_stacked_entries(&c))
        {
            Ok(0) => {}
            Ok(n) => tracing::info!("Split {n} stacked collection copies into individual rows."),
            Err(e) => tracing::error!("Boot collection split failed: {e}"),
        }
    }
    // Lane B background jobs (BRIEF-B): RUST_JOBS=scryfall takes the
    // weekly set-sync / hourly price-sweep timers from Node's server.js
    // (which the test container disables via SCRYBOX_DISABLE_SETS_REFRESH /
    // SCRYBOX_DISABLE_PRICE_SWEEP when this is on). Off unless set.
    // Built before AppState so the weekly tick shares AppState.scryfall_sets
    // (REVIEW concurrency-jobs L4: no fresh per-tick memo).
    let state = scrybox_server::AppState::new(cfg.clone());
    if std::env::var("RUST_JOBS").unwrap_or_default().split(',').any(|s| s.trim() == "scryfall") {
        match cfg.db_path.clone() {
            Some(path) => {
                tracing::info!(db = %path.display(), "Rust Scryfall jobs enabled (RUST_JOBS=scryfall)");
                scrybox_server::scry_jobs::start_job_timers(path, state.scryfall_sets.clone());
            }
            None => tracing::warn!("RUST_JOBS=scryfall set but SCRYBOX_DB_PATH missing; jobs not started"),
        }
    }
    let listener = tokio::net::TcpListener::bind(cfg.bind).await.expect("bind");
    // Lane B unit 6b: server.js cvScan.warm twin — preload models + English
    // catalog when the lane owns scan-match (no-op without ORT_DYLIB_PATH or
    // built catalogs, so it never pays costs the lane won't use).
    // Gate D iter-23: boot warm is OPT-IN (SCRYBOX_CVSCAN_WARM=1 restores
    // Node's eager shape). Attribution: the warm alone accounts for ~168 MB
    // of the 229 MB boot VmHWM floor (ort dylib + 2 ONNX sessions + the
    // 56 MB English npz catalog + rowSets); everything else boots at 62 MB,
    // under Gate D's <66 MB idle target. Owner ruling 2026-11-11: heavy
    // assets load lazily on first use — match_scan() already lazy-loads and
    // static-caches models + catalogs, so the only cost of lazy is the first
    // scan-match request paying the load once.
    if state.owns("scan_match") && scrybox_server::cvscan::warm_requested() {
        scrybox_server::cvscan::spawn_warm();
    }
    // Iter-58 cold-start attribution (single-shot cold stats 573 ms vs storm
    // probe 1360 ms — the gap is queueing behind the start-of-process storm):
    // pre-build /stats bodies at boot like the collection prewarm does for
    // pages. Kept in main (NOT app()) so Router-building tests keep their
    // cold-cache contracts. SCRYBOX_STATS_PREWARM=0 restores pure lazy.
    if state.owns("stats") {
        scrybox_server::stats::spawn_boot_warm(&state);
    }
    let router = scrybox_server::app(state);
    // Lane B unit 5c: optional HTTPS listener (utils/tls.js twin). A failure
    // — unreadable operator pair, no DB path, bind error — logs Node's line
    // and the plain-HTTP front keeps serving; a TLS crash can never take it
    // down. HTTPS_PORT unset (test harness default) = no listener, matching
    // Node startHttps's early return.
    if let Some(raw) = std::env::var("HTTPS_PORT").ok().filter(|v| !v.is_empty()) {
        // One env read; Node's startHttps parses once as well.
        let port: u16 = match raw.parse() {
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
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server");
    tracing::info!("scrybox-server stopped");
}

/// Review M1 (concurrency-jobs): Docker/systemd stop with SIGTERM, which
/// tokio does not handle by default (the process died mid-response). Resolve
/// on SIGTERM or Ctrl-C so axum drains in-flight requests; the drain is
/// bounded by a hard exit after `SCRYBOX_SHUTDOWN_GRACE_MS` (default 10 s).
/// Background jobs (backup timer, price sweep, moxfield scheduler) are
/// aborted with the runtime: SQLite WAL keeps the DB consistent and backups
/// write `*.bak.tmp` + rename, so no partial `*.bak` survives.
async fn shutdown_signal() {
    let term = async {
        #[cfg(unix)]
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term => {},
    }
    let grace_ms: u64 = std::env::var("SCRYBOX_SHUTDOWN_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    tracing::info!(grace_ms, "shutdown signal received; draining connections");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(grace_ms));
        tracing::warn!("graceful drain exceeded {grace_ms} ms; exiting");
        std::process::exit(0);
    });
}
