//! Local Scryfall search API — pure Rust, memory-resident, no SQLite.

mod api;
mod eval;
mod index;
mod query;

use api::{card, collection, health, search, set_detail, sets, sets_list_scryfall, suggest, AppState};
use axum::routing::{get, get_service, post};
use axum::Router;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};

// Bulk-data location. SCRYFALL_DATA_DIR is the deployment knob (Docker mounts
// /data there); the old explicit-file envs stay as escape hatches, and the
// dev-tree candidates keep the bare `cargo run` workflow working.
fn data_dir() -> Option<PathBuf> {
    std::env::var("SCRYFALL_DATA_DIR").ok().map(PathBuf::from)
}

const DEFAULT_CANDIDATES: [&str; 3] = [
    "../default-cards.jsonl.gz",
    "./default-cards.jsonl.gz",
    "/home/brenttime/projects/local-scryfall/default-cards.jsonl.gz",
];

fn resolve_cards_path() -> PathBuf {
    if let Ok(p) = std::env::var("CARDS_JSONL_GZ") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    if let Some(dir) = data_dir() {
        let pb = dir.join("default-cards.jsonl.gz");
        if pb.exists() {
            return pb;
        }
    }
    for c in DEFAULT_CANDIDATES {
        let pb = PathBuf::from(c);
        if pb.exists() {
            return pb;
        }
    }
    panic!(
        "cards file not found; set CARDS_JSONL_GZ (tried {:?})",
        DEFAULT_CANDIDATES
    );
}

fn resolve_tags_path(cards_path: &PathBuf) -> PathBuf {
    if let Ok(p) = std::env::var("ORACLE_TAGS_JSONL_GZ") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    // sibling of the cards file first, then SCRYFALL_DATA_DIR, then crate/parent dirs, then absolute
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = data_dir() {
        cands.push(dir.join("oracle-tags.jsonl.gz"));
    }
    if let Some(dir) = cards_path.parent() {
        cands.push(dir.join("oracle-tags.jsonl.gz"));
    }
    cands.push(PathBuf::from("../oracle-tags.jsonl.gz"));
    cands.push(PathBuf::from("./oracle-tags.jsonl.gz"));
    cands.push(PathBuf::from("/home/brenttime/projects/local-scryfall/oracle-tags.jsonl.gz"));
    for c in cands {
        if c.exists() {
            return c;
        }
    }
    PathBuf::from("oracle-tags.jsonl.gz") // load_tags() will fail -> otag warns + 0 results
}

fn resolve_lang_map_path(cards_path: &PathBuf) -> PathBuf {
    // Wave-5 item 3: bulk language side-table (see index::load_langs and
    // scratch/bulk-langs/README-design.md). Same search shape as the tags
    // file; absence is fine (English-slimmed fallback behavior).
    if let Ok(p) = std::env::var("LANG_MAP_JSON_GZ") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = data_dir() {
        cands.push(dir.join("lang_map_full.json.gz"));
    }
    if let Some(dir) = cards_path.parent() {
        cands.push(dir.join("lang_map_full.json.gz"));
    }
    cands.push(PathBuf::from("data/lang_map_full.json.gz"));
    cands.push(PathBuf::from("/home/brenttime/projects/local-scryfall/lang_map_full.json.gz"));
    for c in cands {
        if c.exists() {
            return c;
        }
    }
    PathBuf::from("lang_map_full.json.gz")
}

fn resolve_blocks_path() -> (PathBuf, PathBuf) {
    // (blocks.json, sets-meta.json) — vendored Scryfall set metadata, see
    // data/gen_sets_meta.py. Env SETS_META_DIR overrides the search dirs.
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("SETS_META_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(d) = data_dir() {
        dirs.push(d);
    }
    dirs.push(PathBuf::from("data"));
    dirs.push(PathBuf::from("../data"));
    dirs.push(PathBuf::from("/home/brenttime/projects/local-scryfall/scryfall-api/data"));
    for d in dirs {
        let b = d.join("blocks.json");
        let m = d.join("sets-meta.json");
        if b.exists() && m.exists() {
            return (b, m);
        }
    }
    (PathBuf::from("data/blocks.json"), PathBuf::from("data/sets-meta.json"))
}

#[tokio::main(worker_threads = 4)]
async fn main() {
    let path = resolve_cards_path();
    let t = Instant::now();
    eprintln!("loading index from {}", path.display());
    let mut ix = index::Index::load(&path);
    let tags_path = resolve_tags_path(&path);
    let tags_ok = ix.load_tags(&tags_path);
    let lang_map_path = resolve_lang_map_path(&path);
    let langs_ok = ix.load_langs(&lang_map_path);
    let (blocks_path, meta_path) = resolve_blocks_path();
    let meta_ok = ix.load_set_meta(&blocks_path, &meta_path);
    eprintln!(
        "ready: {} cards in {} ms (oracle tags: {} from {}; set meta: {} from {})",
        ix.cards.len(),
        t.elapsed().as_millis(),
        if tags_ok {
            format!("{} loaded", ix.otags.len())
        } else {
            "NOT LOADED — otag: will warn + 0 results".to_string()
        },
        tags_path.display(),
        if meta_ok { "loaded" } else { "MISSING — block/icon fields disabled" },
        meta_path.display(),
    );
    eprintln!(
        "lang side-table: {} ({} langs, {} oracles mapped)",
        if langs_ok { "loaded" } else { "NOT LOADED — lang:/in: fall back to the English-slimmed dump" },
        ix.oracle_lang_idx.len(),
        ix.oracle_lang_idx.values().map(|b| b.len()).max().unwrap_or(0),
    );

    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "static".into());
    std::fs::create_dir_all(&static_dir).ok();
    eprintln!(
        "serving frontend from ./{} (dir may be empty until the frontend lands)",
        static_dir
    );

    let state = Arc::new(AppState {
        ix,
        started: Instant::now(),
    });

    let app = Router::new()
        .route("/api/search", get(search))
        .route("/api/cards/search", get(search))
        // Scryfall-compatible API aliases: one baseURL swap in the backend
        // (https://api.scryfall.com → this server) has to make the two paths
        // Scrybox actually calls work verbatim.
        .route("/cards/search", get(search))
        // ...and the card/collection lookups the backend's price refresh and
        // limited mode use. Static routes win over the {id} wildcard.
        .route("/cards/collection", post(collection))
        .route("/cards/{*id}", get(card))
        .route("/api/suggest", get(suggest))
        .route("/api/sets", get(sets))
        .route("/api/set/{code}", get(set_detail))
        .route("/api/card/{*id}", get(card))
        .route("/api/health", get(health))
        .route("/api/healthz", get(health))
        // Scryfall-style paths: /sets is content-negotiated — JSON clients get
        // the Scryfall /sets envelope, browsers get the 301 to the SPA page;
        // /sets/{code} → detail SPA
        .route("/sets", get(sets_list_scryfall))
        .route(
            "/sets/{code}",
            get_service(ServeFile::new(format!("{static_dir}/set.html"))),
        )
        .fallback_service(ServeDir::new(&static_dir))
        .layer(CompressionLayer::new().gzip(true).br(true))
        .with_state(state);

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
