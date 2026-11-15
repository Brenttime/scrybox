//! Index boot + router assembly, shared by the standalone `scryfall-api`
//! binary and the in-process engine inside scrybox-server.
//!
//! The file-resolution rules are the sidecar's `main.rs` rules verbatim
//! (same envs, same search order, same log lines); the only addition is
//! [`BootOpts`], so an embedding process can pin the data dir explicitly and
//! switch off the dev-tree fallbacks (a container must never pick up a
//! stray `../default-cards.jsonl.gz`).

use crate::api::{card, collection, health, search, set_detail, sets, sets_list_scryfall, suggest, AppState, SharedState};
use crate::index::Index;
use crate::prints;
use axum::routing::{get, get_service, post};
use axum::Router;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};

/// Where the boot looks for its inputs.
#[derive(Clone, Debug, Default)]
pub struct BootOpts {
    /// `SCRYFALL_DATA_DIR` equivalent (bulk files: default-cards,
    /// oracle-tags, lang map, all-cards, optional sets meta).
    pub data_dir: Option<PathBuf>,
    /// Extra set-metadata dir searched after `SETS_META_DIR` and the data
    /// dir — the vendored `data/` copy (blocks.json, sets-meta.json,
    /// land_cycles.json).
    pub vendored_dir: Option<PathBuf>,
    /// Keep the sidecar's dev-tree candidates (`../default-cards.jsonl.gz`,
    /// `/home/brenttime/...`). The standalone bin keeps them; the embedded
    /// engine turns them off.
    pub dev_fallbacks: bool,
    /// Honour the explicit-file envs (CARDS_JSONL_GZ, ORACLE_TAGS_JSONL_GZ,
    /// LANG_MAP_JSON_GZ, SETS_META_DIR, SCRYFALL_ALL_CARDS_FILE).
    pub file_envs: bool,
}

impl BootOpts {
    /// The standalone sidecar's behaviour: env-driven, dev fallbacks on.
    pub fn from_env() -> Self {
        Self {
            data_dir: std::env::var("SCRYFALL_DATA_DIR").ok().map(PathBuf::from),
            vendored_dir: None,
            dev_fallbacks: true,
            file_envs: true,
        }
    }
}

const DEFAULT_CANDIDATES: [&str; 3] = [
    "../default-cards.jsonl.gz",
    "./default-cards.jsonl.gz",
    "/home/brenttime/projects/local-scryfall/default-cards.jsonl.gz",
];

fn env_file(o: &BootOpts, key: &str) -> Option<PathBuf> {
    if !o.file_envs {
        return None;
    }
    std::env::var(key).ok().map(PathBuf::from).filter(|p| p.exists())
}

/// The default-cards bulk file, or None when nothing is on disk yet.
pub fn resolve_cards_path(o: &BootOpts) -> Option<PathBuf> {
    if let Some(p) = env_file(o, "CARDS_JSONL_GZ") {
        return Some(p);
    }
    if let Some(dir) = &o.data_dir {
        let pb = dir.join("default-cards.jsonl.gz");
        if pb.exists() {
            return Some(pb);
        }
    }
    if o.dev_fallbacks {
        for c in DEFAULT_CANDIDATES {
            let pb = PathBuf::from(c);
            if pb.exists() {
                return Some(pb);
            }
        }
    }
    None
}

pub fn resolve_tags_path(o: &BootOpts, cards_path: &Path) -> PathBuf {
    if let Some(p) = env_file(o, "ORACLE_TAGS_JSONL_GZ") {
        return p;
    }
    // sibling of the cards file first, then SCRYFALL_DATA_DIR, then crate/parent dirs, then absolute
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = &o.data_dir {
        cands.push(dir.join("oracle-tags.jsonl.gz"));
    }
    if let Some(dir) = cards_path.parent() {
        cands.push(dir.join("oracle-tags.jsonl.gz"));
    }
    if o.dev_fallbacks {
        cands.push(PathBuf::from("../oracle-tags.jsonl.gz"));
        cands.push(PathBuf::from("./oracle-tags.jsonl.gz"));
        cands.push(PathBuf::from("/home/brenttime/projects/local-scryfall/oracle-tags.jsonl.gz"));
    }
    for c in cands {
        if c.exists() {
            return c;
        }
    }
    PathBuf::from("oracle-tags.jsonl.gz") // load_tags() will fail -> otag warns + 0 results
}

pub fn resolve_lang_map_path(o: &BootOpts, cards_path: &Path) -> PathBuf {
    // Wave-5 item 3: bulk language side-table (see index::load_langs and
    // scratch/bulk-langs/README-design.md). Same search shape as the tags
    // file; absence is fine (English-slimmed fallback behavior).
    if let Some(p) = env_file(o, "LANG_MAP_JSON_GZ") {
        return p;
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = &o.data_dir {
        cands.push(dir.join("lang_map_full.json.gz"));
    }
    if let Some(dir) = cards_path.parent() {
        cands.push(dir.join("lang_map_full.json.gz"));
    }
    if o.dev_fallbacks {
        cands.push(PathBuf::from("data/lang_map_full.json.gz"));
        cands.push(PathBuf::from("/home/brenttime/projects/local-scryfall/lang_map_full.json.gz"));
    }
    for c in cands {
        if c.exists() {
            return c;
        }
    }
    PathBuf::from("lang_map_full.json.gz")
}

pub fn resolve_blocks_path(o: &BootOpts) -> (PathBuf, PathBuf) {
    // (blocks.json, sets-meta.json) — vendored Scryfall set metadata, see
    // data/gen_sets_meta.py. Env SETS_META_DIR overrides the search dirs.
    let mut dirs: Vec<PathBuf> = Vec::new();
    if o.file_envs {
        if let Ok(d) = std::env::var("SETS_META_DIR") {
            dirs.push(PathBuf::from(d));
        }
    }
    if let Some(d) = &o.data_dir {
        dirs.push(d.clone());
    }
    if let Some(d) = &o.vendored_dir {
        dirs.push(d.clone());
    }
    if o.dev_fallbacks {
        dirs.push(PathBuf::from("data"));
        dirs.push(PathBuf::from("../data"));
        dirs.push(PathBuf::from("/home/brenttime/projects/local-scryfall/scryfall-api/data"));
    }
    for d in dirs {
        let b = d.join("blocks.json");
        let m = d.join("sets-meta.json");
        if b.exists() && m.exists() {
            return (b, m);
        }
    }
    (PathBuf::from("data/blocks.json"), PathBuf::from("data/sets-meta.json"))
}

fn resolve_side_src(o: &BootOpts) -> Option<PathBuf> {
    // Explicit disable sentinel (SCRYFALL_ALL_CARDS_FILE=none|off|0) so
    // "absent bulk = exact pre-w6a behaviour" can be exercised without
    // deleting the dev fallback file.
    let env = if o.file_envs { std::env::var("SCRYFALL_ALL_CARDS_FILE").ok() } else { None };
    if matches!(env.as_deref(), Some("none" | "off" | "0" | "")) {
        return None;
    }
    [
        env.map(PathBuf::from),
        o.data_dir.as_ref().map(|d| d.join("all-cards.jsonl.gz")),
        o.dev_fallbacks
            .then(|| PathBuf::from("/home/brenttime/projects/local-scryfall-allprints/all-cards.jsonl.gz")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.exists())
}

/// Build the full memory-resident index exactly as the sidecar's boot did
/// (default-cards + oracle tags + lang map + set meta + all_cards side
/// index), logging the same lines. None = no default-cards file found.
pub fn load_index(o: &BootOpts) -> Option<Index> {
    let path = resolve_cards_path(o)?;
    let t = Instant::now();
    eprintln!("loading index from {}", path.display());
    let mut ix = Index::load(&path);
    let tags_path = resolve_tags_path(o, &path);
    let tags_ok = ix.load_tags(&tags_path);
    let lang_map_path = resolve_lang_map_path(o, &path);
    let langs_ok = ix.load_langs(&lang_map_path);
    let (blocks_path, meta_path) = resolve_blocks_path(o);
    let meta_ok = ix.load_set_meta(&blocks_path, &meta_path);

    // Parity w6a: all_cards per-printing side index (price/cn/artist/rarity/
    // finish predicates quantify over ALL printings, incl. gated/variant/
    // promo/foreign rows default-cards omits). Optional: absent file = exact
    // pre-w6a behaviour, so prod without the bulk is unaffected.
    let side_src = resolve_side_src(o);
    let mut side_ms = 0u128;
    if let Some(src) = &side_src {
        let t = Instant::now();
        let stem = src
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "all-cards.jsonl".into());
        let cache = src.with_file_name(format!("{stem}.sideidx"));
        if !prints::load_side_cache(&mut ix, &cache, src) && prints::load_side_gz(&mut ix, src) {
            prints::write_side_cache(&ix, &cache, src);
        }
        side_ms = t.elapsed().as_millis();
    }
    eprintln!(
        "all_cards side index: {} ({} printings, {} ms)",
        if ix.side.loaded { "loaded" } else { "ABSENT — price/cn/artist predicates default-cards-only" },
        ix.side.prints.len(),
        side_ms,
    );
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
    Some(ix)
}

pub fn state(ix: Index) -> SharedState {
    Arc::new(AppState { ix, started: Instant::now() })
}

/// The sidecar's full route table + static portal, byte-for-byte the router
/// the standalone binary serves.
pub fn router(state: SharedState, static_dir: &str) -> Router {
    Router::new()
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
        .route("/sets/{code}", get_service(ServeFile::new(format!("{static_dir}/set.html"))))
        .fallback_service(ServeDir::new(static_dir))
        .layer(CompressionLayer::new().gzip(true).br(true))
        .with_state(state)
}
