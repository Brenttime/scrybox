//! Standalone `scryfall-api` — the old sidecar binary, now a thin wrapper
//! over the `local_scryfall` library (same envs, routes, logs, port 8420).

use local_scryfall::boot::{self, BootOpts};

#[tokio::main(worker_threads = 4)]
async fn main() {
    let opts = BootOpts::from_env();
    let ix = boot::load_index(&opts).unwrap_or_else(|| {
        panic!(
            "cards file not found; set CARDS_JSONL_GZ (tried {:?})",
            ["../default-cards.jsonl.gz", "./default-cards.jsonl.gz", "/home/brenttime/projects/local-scryfall/default-cards.jsonl.gz"]
        )
    });

    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "static".into());
    std::fs::create_dir_all(&static_dir).ok();
    eprintln!("serving frontend from ./{} (dir may be empty until the frontend lands)", static_dir);

    let app = boot::router(boot::state(ix), &static_dir);

    // BIND wins if set; otherwise PORT env (default 8420) on all interfaces.
    let addr = std::env::var("BIND").unwrap_or_else(|_| {
        let port = std::env::var("PORT").unwrap_or_else(|_| "8420".into());
        format!("0.0.0.0:{port}")
    });
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    eprintln!("listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("shutting down");
}
