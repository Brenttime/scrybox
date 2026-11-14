use std::net::SocketAddr;

use scrybox_server::Config;
use tracing_subscriber::EnvFilter;

/// review/MEMORY.md ranked item 8 (memory queue item 2, lane-B): tokio's
/// blocking pool defaults to `max_blocking_threads = 512`, and under the
/// scen-varied mix the process was measured at 38 OS threads (5 at boot).
/// Every extra thread carries an 8 MB stack reservation and (pre arena-cap)
/// its own glibc arena. What actually lands on this pool: SQLite
/// checkout/query legs (already gated by the DB pool's `live_max` = 2×
/// `pool_max`, `pool_max` ∈ 2..=16), bcrypt hash legs, cvscan match legs,
/// gunzip/gzip legs, backup/export file legs, and manabox parse — all of
/// which queue behind `live_max` for the DB anyway, so 512 was pure
/// reservation with no throughput behind it. Not affected by the cap: the
/// parallel collection build fans out with `std::thread::scope` INSIDE one
/// blocking task (never a blocking-task-waits-on-blocking-task shape, so a
/// capped pool cannot deadlock it), and every `spawn_blocking(...).await`
/// hop in the tree is issued from async context.
///
/// Owner bound (MEMORY.md #8): 8-16, "size it to the pool live_max". 16 =
/// top of the sanctioned bound and 2× a 8-connection pool, so a DB waiter
/// is never queued behind unrelated CPU work on a default (≤8-core) box.
/// `SCRYBOX_BLOCKING_THREADS` pins the cap (garbage/0/negative/unset ->
/// default, same parse discipline as `M_ARENA_MAX` in collection.rs);
/// tokio's builder rejects 0, so a legal value is clamped to >= 1.
const DEFAULT_BLOCKING_THREADS: usize = 16;

// ── review/MEMORY.md ranked item 5 (memory queue item 5) ───────────────────
// Opt-in jemalloc global allocator, `--features jemalloc`. The LD_PRELOAD
// study (MEMORY.md #5) measured the jemalloc default config at HWM 651 /
// settle 301 MB on the scen-varied mix and its tuned sibling
// (narenas:2, dirty_decay 1 s, muzzy 0) at 466/262 — the best settle AND
// the fastest of every config sampled, vs glibc arena2 as the incumbent
// (#4's mallopt levers, already in collection.rs). Adopted as an OFF-by-
// default cargo feature: response bytes are allocator-independent, but the
// lane binary stays on glibc until the A/B verdict lands; enabling the
// feature (and later the Dockerfile default) is an owner adoption.
//
// Scope note vs the LD_PRELOAD study: `#[global_allocator]` routes the
// RUST heap through jemalloc only — C code linked into the process
// (bundled SQLite) and the dynamically-loaded libonnxruntime keep calling
// glibc malloc, because tikv-jemallocator's default build prefixes its
// symbols (`_rjem_`) instead of interposing `malloc`. The Rust heap is
// what the memory queue is about (serde bodies, RowStore, chunk pool — the
// multi-MB request-path transients), but the probe numbers are NOT
// directly comparable to the LD_PRELOAD rows; they re-baseline the A/B.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Boot-time jemalloc tuning: the MEMORY.md #5 owner bound. jemalloc reads
/// its option string exactly once, at its own init — and with
/// `#[global_allocator]` the Rust runtime allocates (initializing jemalloc)
/// BEFORE `main()` runs, so post-main mallctl *writes* arrive too late:
/// measured on brain, iter-44, `arenas.narenas` -> EPERM and the
/// arena.<agg>.decay_ms writes -> EFAULT while arena.0 writes landed. The
/// reliable lever is therefore the env: tikv-jemalloc-sys is a PREFIXED
/// build (`_rjem_*` symbols via objcopy), and a prefixed jemalloc reads its
/// options from `_RJEM_MALLOC_CONF` (uppercased crate prefix), NOT
/// `MALLOC_CONF` (conf.c case 3: `JEMALLOC_CPREFIX "MALLOC_CONF"`; embedded
/// strings of the linked binary confirm — measured on brain, iter-44: plain
/// MALLOC_CONF left opt.narenas at the 4*ncpus default). `apply()` composes
/// `<operator keys>,<our missing keys>` and re-execs the process once with
/// `_RJEM_MALLOC_CONF` set (loop guard: if that env already equals the
/// composed string there is no second exec; SCRYBOX_JEM_NO_REEXEC=1 skips
/// the re-exec entirely and boots with plain jemalloc defaults). An
/// operator's own key always wins: `plan()` omits any key already set in
/// either `MALLOC_CONF` (intent — forwarded, since jemalloc ignores it for
/// this build) or `_RJEM_MALLOC_CONF` (authoritative). After the (re-)exec
/// `apply()` verifies through read-only mallctls (opt.narenas, arena.0
/// decays) and logs the outcome; every step is best-effort — a failed
/// re-exec just boots untuned.
#[cfg(feature = "jemalloc")]
mod jemalloc_cap {
    use std::os::raw::{c_char, c_void};

    /// Owner bound (MEMORY.md #5): 2 arenas, dirty pages decay back to the
    /// OS in ~1 s, muzzy pages returned immediately (0 = no retention).
    pub(super) const DEFAULT_NARENAS: usize = 2;
    pub(super) const DEFAULT_DIRTY_DECAY_MS: i64 = 1_000;
    pub(super) const DEFAULT_MUZZY_DECAY_MS: i64 = 0;

    /// What `MALLOC_CONF` (if anything) already pins, so the in-code
    /// defaults below do not override operator intent. Pure for tests.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Plan {
        pub narenas: Option<usize>,
        pub dirty_decay_ms: Option<i64>,
        pub muzzy_decay_ms: Option<i64>,
    }

    /// `MALLOC_CONF` is a comma-separated `key:value` list; a key counts as
    /// operator-set when a token starts with `key:` (exact key match —
    /// `narenas:` must not be shadowed by e.g. a `min_narenas:` token).
    fn conf_sets(malloc_conf: &str, key: &str) -> bool {
        malloc_conf.split(',').any(|kv| kv.trim_start().starts_with(key))
    }

    pub(super) fn plan(malloc_conf: Option<&str>) -> Plan {
        let set = |key: &str| malloc_conf.is_some_and(|c| conf_sets(c, key));
        Plan {
            narenas: (!set("narenas:")).then_some(DEFAULT_NARENAS),
            dirty_decay_ms: (!set("dirty_decay_ms:")).then_some(DEFAULT_DIRTY_DECAY_MS),
            muzzy_decay_ms: (!set("muzzy_decay_ms:")).then_some(DEFAULT_MUZZY_DECAY_MS),
        }
    }

    /// The `MALLOC_CONF` string that must be in the environment for jemalloc
    /// to boot tuned: operator tokens verbatim first (jemalloc applies
    /// left-to-right, last write wins, so ours must come after — and
    /// `plan()` guarantees we never add a key the operator set anyway),
    /// then the plan's still-missing keys. `None` when the plan is empty
    /// (operator already pins everything) — no re-exec needed. Pure.
    pub(super) fn build_conf(existing: Option<&str>) -> Option<String> {
        let p = plan(existing);
        let mut ours: Vec<String> = Vec::new();
        if let Some(n) = p.narenas {
            ours.push(format!("narenas:{n}"));
        }
        if let Some(ms) = p.dirty_decay_ms {
            ours.push(format!("dirty_decay_ms:{ms}"));
        }
        if let Some(ms) = p.muzzy_decay_ms {
            ours.push(format!("muzzy_decay_ms:{ms}"));
        }
        if ours.is_empty() {
            return None;
        }
        let ours = ours.join(",");
        Some(match existing.filter(|c| !c.trim().is_empty()) {
            Some(c) => format!("{c},{ours}"),
            None => ours,
        })
    }

    unsafe fn read<T>(name: &[u8], out: &mut T) -> i32 {
        let mut cname: Vec<u8> = name.to_vec();
        cname.push(0);
        let mut len = std::mem::size_of::<T>();
        tikv_jemalloc_sys::mallctl(
            cname.as_ptr() as *const c_char,
            out as *mut T as *mut c_void,
            &mut len as *mut usize,
            std::ptr::null_mut(),
            0,
        )
    }

    /// Read back what jemalloc actually booted with (read-only mallctls
    /// work post-init). Best-effort operator/probe visibility.
    fn verify_and_log(source: &str) {
        let mut narenas: libc::c_uint = 0;
        let mut dirty: i64 = -1;
        let mut muzzy: i64 = -1;
        let (r1, r2, r3) = unsafe {
            (
                read(b"opt.narenas", &mut narenas),
                read(b"arena.0.dirty_decay_ms", &mut dirty),
                read(b"arena.0.muzzy_decay_ms", &mut muzzy),
            )
        };
        eprintln!(
            "scrybox-server: jemalloc tuned [{source}] opt.narenas={narenas}(rc{r1}) arena0.dirty_decay_ms={dirty}(rc{r2}) arena0.muzzy_decay_ms={muzzy}(rc{r3}) {PREFIXED_MALLOC_CONF}={:?} MALLOC_CONF={:?}",
            std::env::var(PREFIXED_MALLOC_CONF).unwrap_or_default(),
            std::env::var("MALLOC_CONF").unwrap_or_default(),
        );
    }

    /// A prefixed jemalloc reads its options env var under the UPPERCASED
    /// crate symbol prefix (jemalloc conf.c case 3: `JEMALLOC_CPREFIX
    /// "MALLOC_CONF"` — CPREFIX is the objcopy prefix uppercased; verified
    /// against the embedded strings of the linked binary: `_RJEM_MALLOC_CONF`
    /// and the `/etc/_rjem_malloc.conf` symlink arm). Lowercase
    /// `_rjem_MALLOC_CONF` is silently ignored — measured, iter-44.
    const PREFIXED_MALLOC_CONF: &str = "_RJEM_MALLOC_CONF";

    /// Compose the tuning env and re-exec once with it set.
    /// Runs at the very top of `main()`, before the tokio runtime exists.
    pub(super) fn apply() {
        // Operator intent: MALLOC_CONF (ignored by this prefixed build but
        // honored as intent and forwarded), then the authoritative
        // _RJEM_MALLOC_CONF (later tokens win in jemalloc's left-to-right
        // parse, so it goes second).
        let trimmed = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        let plain = trimmed(std::env::var("MALLOC_CONF").ok());
        let prefixed = trimmed(std::env::var(PREFIXED_MALLOC_CONF).ok());
        let combined: Option<String> = match (plain.as_deref(), prefixed.as_deref()) {
            (Some(p), Some(q)) => Some(format!("{p},{q}")),
            (Some(p), None) => Some(p.to_string()),
            (None, Some(q)) => Some(q.to_string()),
            (None, None) => None,
        };
        // What _RJEM_MALLOC_CONF must contain for a tuned boot: operator
        // tokens plus the still-missing owner-bound keys (build_conf), or
        // just the operator tokens when they already pin everything.
        let Some(want) = build_conf(combined.as_deref()).or(combined) else {
            verify_and_log("nothing to tune");
            return;
        };
        if prefixed.as_deref() == Some(want.as_str()) {
            // Second boot (post-exec): _RJEM_MALLOC_CONF is exactly right.
            verify_and_log("re-exec");
            return;
        }
        if std::env::var("SCRYBOX_JEM_NO_REEXEC").is_ok_and(|v| !v.is_empty() && v != "0") {
            verify_and_log("re-exec declined via SCRYBOX_JEM_NO_REEXEC (untuned jemalloc)");
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("scrybox-server: jemalloc tuning skipped (current_exe: {e})");
                return;
            }
        };
        eprintln!(
            "scrybox-server: re-exec for jemalloc tuning {PREFIXED_MALLOC_CONF}={want:?} (SCRYBOX_JEM_NO_REEXEC=1 disables)"
        );
        let err = {
            use std::os::unix::process::CommandExt;
            std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .env(PREFIXED_MALLOC_CONF, &want)
                // The forwarded plain MALLOC_CONF must not survive the exec:
                // on pass 2 it would re-prepend the operator tokens, making
                // the composed string differ from _RJEM_MALLOC_CONF forever
                // -> exec loop (measured, iter-44: narenas:8 grew one token
                // per exec until timeout).
                .env_remove("MALLOC_CONF")
                .exec()
        };
        // Only reached if exec() failed: boot on jemalloc defaults rather
        // than die.
        eprintln!("scrybox-server: jemalloc tuning re-exec FAILED ({err}); continuing untuned");
    }
}

fn blocking_threads_from_env() -> usize {
    std::env::var("SCRYBOX_BLOCKING_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_BLOCKING_THREADS)
        .max(1)
}

fn main() {
    #[cfg(feature = "jemalloc")]
    jemalloc_cap::apply();
    // #[tokio::main] equivalent (multi-thread, enable_all) with the blocking
    // pool bounded — see DEFAULT_BLOCKING_THREADS.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(blocking_threads_from_env())
        .build()
        .expect("tokio runtime");
    rt.block_on(run());
}

async fn run() {
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

#[cfg(test)]
mod blocking_cap_tests {
    use super::{blocking_threads_from_env, DEFAULT_BLOCKING_THREADS};

    /// Red-before-fix note: with the pre-cap `#[tokio::main]` these values
    /// were never consulted; the neuter check is `max_blocking_threads(
    /// blocking_threads_from_env().saturating_mul(32))` in main() — the
    /// Threads probe in the iteration log then climbs back to the 5→38 fan
    /// out under the mix. The parse contract itself is pinned here.
    #[test]
    fn blocking_threads_env_parse_contract() {
        let clear = || std::env::remove_var("SCRYBOX_BLOCKING_THREADS");
        clear();
        assert_eq!(blocking_threads_from_env(), DEFAULT_BLOCKING_THREADS, "unset -> in-code default");
        assert_eq!(DEFAULT_BLOCKING_THREADS, 16, "owner-bound top of MEMORY.md #8 range 8-16");

        std::env::set_var("SCRYBOX_BLOCKING_THREADS", "6");
        assert_eq!(blocking_threads_from_env(), 6, "legal value honoured");
        std::env::set_var("SCRYBOX_BLOCKING_THREADS", " 42 ");
        assert_eq!(blocking_threads_from_env(), 42, "value is trimmed; >16 allowed as operator override");
        std::env::set_var("SCRYBOX_BLOCKING_THREADS", "1");
        assert_eq!(blocking_threads_from_env(), 1, "1 is legal (single-slot pool, debug use)");

        std::env::set_var("SCRYBOX_BLOCKING_THREADS", "0");
        assert_eq!(blocking_threads_from_env(), DEFAULT_BLOCKING_THREADS, "0 would panic the tokio builder -> default");
        std::env::set_var("SCRYBOX_BLOCKING_THREADS", "-4");
        assert_eq!(blocking_threads_from_env(), DEFAULT_BLOCKING_THREADS, "negative is nonsense");
        std::env::set_var("SCRYBOX_BLOCKING_THREADS", "many");
        assert_eq!(blocking_threads_from_env(), DEFAULT_BLOCKING_THREADS, "garbage -> default");
        std::env::set_var("SCRYBOX_BLOCKING_THREADS", "");
        assert_eq!(blocking_threads_from_env(), DEFAULT_BLOCKING_THREADS, "empty -> default");
        clear();
    }
}

#[cfg(all(test, feature = "jemalloc"))]
mod jemalloc_cap_tests {
    use super::jemalloc_cap::{plan, Plan, DEFAULT_DIRTY_DECAY_MS, DEFAULT_MUZZY_DECAY_MS, DEFAULT_NARENAS};

    /// The MEMORY.md #5 owner bound pinned in code, and the operator-
    /// precedence contract: an explicit `MALLOC_CONF` key must survive
    /// untouched (same discipline as `arena_max_from_env` for
    /// `MALLOC_ARENA_MAX` — we only fill in what the operator did not set).
    #[test]
    fn malloc_conf_parse_contract() {
        assert_eq!(
            (DEFAULT_NARENAS, DEFAULT_DIRTY_DECAY_MS, DEFAULT_MUZZY_DECAY_MS),
            (2, 1_000, 0),
            "owner bound: narenas=2, dirty decay 1 s, muzzy 0"
        );
        assert_eq!(
            plan(None),
            Plan { narenas: Some(2), dirty_decay_ms: Some(1_000), muzzy_decay_ms: Some(0) },
            "unset MALLOC_CONF -> full in-code default"
        );
        assert_eq!(
            plan(Some("")),
            Plan { narenas: Some(2), dirty_decay_ms: Some(1_000), muzzy_decay_ms: Some(0) },
            "empty MALLOC_CONF -> full in-code default"
        );
        assert_eq!(
            plan(Some("narenas:8,dirty_decay_ms:10000")),
            Plan { narenas: None, dirty_decay_ms: None, muzzy_decay_ms: Some(0) },
            "operator keys untouched; unset key still gets the default"
        );
        assert_eq!(
            plan(Some("muzzy_decay_ms:5000,background_thread:true")),
            Plan { narenas: Some(2), dirty_decay_ms: Some(1_000), muzzy_decay_ms: None },
            "key match is per-token: muzzy_decay_ms does NOT shadow dirty_decay_ms"
        );
        assert_eq!(
            plan(Some(" tcache_max:65536 , narenas:4 ")),
            Plan { narenas: None, dirty_decay_ms: Some(1_000), muzzy_decay_ms: Some(0) },
            "tokens are trimmed before the key match"
        );
        assert_eq!(
            plan(Some("prof:false,lg_prof_sample:19")),
            Plan { narenas: Some(2), dirty_decay_ms: Some(1_000), muzzy_decay_ms: Some(0) },
            "unrelated keys leave every default in place"
        );
    }

    /// The re-exec contract: jemalloc reads MALLOC_CONF exactly once, at
    /// its own init (the Rust runtime allocates before main()), so tuning
    /// must reach the environment BEFORE exec. build_conf is the whole
    /// decision — operator tokens verbatim first, only the missing keys
    /// appended, and no re-exec at all when there is nothing to add.
    #[test]
    fn build_conf_reexec_composition() {
        assert_eq!(
            super::jemalloc_cap::build_conf(None).as_deref(),
            Some("narenas:2,dirty_decay_ms:1000,muzzy_decay_ms:0"),
            "unset MALLOC_CONF -> full tuning string"
        );
        assert_eq!(
            super::jemalloc_cap::build_conf(Some("")).as_deref(),
            Some("narenas:2,dirty_decay_ms:1000,muzzy_decay_ms:0"),
            "empty-but-set MALLOC_CONF still needs the tuning"
        );
        assert_eq!(
            super::jemalloc_cap::build_conf(Some("background_thread:true")),
            Some("background_thread:true,narenas:2,dirty_decay_ms:1000,muzzy_decay_ms:0".into()),
            "operator tokens stay verbatim and FIRST; ours appended"
        );
        assert_eq!(
            super::jemalloc_cap::build_conf(Some("narenas:8,dirty_decay_ms:10000")),
            Some("narenas:8,dirty_decay_ms:10000,muzzy_decay_ms:0".into()),
            "only the missing key is appended"
        );
        assert_eq!(
            super::jemalloc_cap::build_conf(
                Some("narenas:2,dirty_decay_ms:1000,muzzy_decay_ms:0")),
            None,
            "operator pins everything -> nothing to add, no re-exec"
        );
        assert_eq!(
            super::jemalloc_cap::build_conf(
                Some("muzzy_decay_ms:0,narenas:2,dirty_decay_ms:1000")),
            None,
            "order in the operator string does not matter"
        );
    }
}
