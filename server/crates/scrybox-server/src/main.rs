use std::net::SocketAddr;

use scrybox_server::{app, AppState, Config};
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
    let listener = tokio::net::TcpListener::bind(cfg.bind).await.expect("bind");
    let router = app(AppState::new(cfg));
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}
