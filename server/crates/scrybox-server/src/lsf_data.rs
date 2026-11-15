//! Bulk-data lifecycle for the in-process Scryfall engine — the Rust port of
//! the sidecar's two shell scripts:
//!
//! * `local-scryfall/fetch-data.sh` (container boot): download any MISSING
//!   bulk file (`default-cards.jsonl.gz`, `oracle-tags.jsonl.gz`), resolving
//!   the live `jsonl_download_uri` from api.scryfall.com/bulk-data/{type}
//!   with the pinned 2026-10-24 URLs as the fallback, via `.part` + atomic
//!   rename. Network-tolerant: every failure warns and boot continues.
//!   (Its `/sets` → gen_sets_meta.py leg is NOT ported: in the live
//!   container it has always failed — `curl: (23)`, unwritable /data — so
//!   production runs on the vendored set meta, which the in-process engine
//!   reads from `SCRYBOX_LSF_VENDORED_DIR`. Same bytes as today.)
//! * `bindarr-host/scripts/refresh-scryfall-data.sh` (host cron, daily
//!   05:00): flock `.refresh.lock`, compare the live versioned file stamp to
//!   `.refresh-stamps/<file>.stamp`, download to `.part`, gzip-verify, back
//!   up the old file to `.backup/`, swap, write the stamp — then (instead of
//!   `docker restart` + curl health) build the new index off to the side,
//!   health-check it with a real `/cards/search?q=lotus`, and hot-swap. A
//!   failed build/health check rolls the files back from `.backup/` and
//!   drops the stamp (forces a re-pull next run), keeping the old index
//!   serving throughout — no downtime window at all.
//!
//! The same lock/stamp/backup layout as the host script, so the two can
//! share a data dir during the cut-over without fighting. The loop also
//! reloads when the bulk files change underneath it (e.g. that host cron
//! still running against a shared volume).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use tokio::io::AsyncWriteExt;

use crate::lsf::InProcessEngine;

const UA: &str = "curl/8.5.0";
pub const BULK: [(&str, &str, &str); 2] = [
    (
        "default_cards",
        "default-cards.jsonl.gz",
        "https://data.scryfall.io/default-cards/default-cards-20261024090552.jsonl.gz",
    ),
    (
        "oracle_tags",
        "oracle-tags.jsonl.gz",
        "https://data.scryfall.io/oracle-tags/oracle-tags-20261024090035.jsonl.gz",
    ),
];

pub(crate) type Https = Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>;

pub(crate) fn client() -> Https {
    let https = HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("native roots")
        .https_or_http()
        .enable_http1()
        .build();
    Client::builder(TokioExecutor::new()).build(https)
}

fn bulk_meta_base() -> String {
    std::env::var("SCRYBOX_LSF_BULK_META")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.scryfall.com/bulk-data".into())
        .trim_end_matches('/')
        .to_string()
}

async fn get(c: &Https, url: &str, timeout: Duration) -> Result<hyper::Response<hyper::body::Incoming>, String> {
    let req = hyper::Request::get(url)
        .header("user-agent", UA)
        .header("accept", "*/*")
        .body(Empty::new())
        .map_err(|e| e.to_string())?;
    let res = tokio::time::timeout(timeout, c.request(req))
        .await
        .map_err(|_| format!("{url}: timeout"))?
        .map_err(|e| format!("{url}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("{url}: HTTP {}", res.status().as_u16()));
    }
    Ok(res)
}

/// The live versioned `jsonl_download_uri` for a bulk type (None on any
/// failure — callers fall back to the pinned URL / skip the refresh).
pub async fn live_url(c: &Https, bulk_type: &str) -> Option<String> {
    let res = get(c, &format!("{}/{bulk_type}", bulk_meta_base()), Duration::from_secs(30)).await.ok()?;
    let b = res.into_body().collect().await.ok()?.to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&b).ok()?;
    v.get("jsonl_download_uri")?.as_str().map(str::to_string).filter(|s| !s.is_empty())
}

/// `.part` download with the 3600 s ceiling of the scripts; the part file
/// never survives a failure.
pub(crate) async fn download(c: &Https, url: &str, part: &Path) -> Result<u64, String> {
    let fut = async {
        let res = get(c, url, Duration::from_secs(60)).await?;
        let mut f = tokio::fs::File::create(part).await.map_err(|e| format!("create {}: {e}", part.display()))?;
        let mut body = res.into_body();
        let mut n = 0u64;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| format!("body: {e}"))?;
            if let Some(d) = frame.data_ref() {
                f.write_all(d).await.map_err(|e| format!("write: {e}"))?;
                n += d.len() as u64;
            }
        }
        f.flush().await.map_err(|e| format!("flush: {e}"))?;
        f.sync_all().await.ok();
        Ok::<u64, String>(n)
    };
    let r = match tokio::time::timeout(Duration::from_secs(3600), fut).await {
        Ok(r) => r,
        Err(_) => Err(format!("{url}: download timeout")),
    };
    if r.is_err() {
        let _ = tokio::fs::remove_file(part).await;
    }
    r
}

/// `gunzip -t`: the whole stream must decode (a truncated gzip must never
/// reach the index loader, which would panic mid-build).
pub fn gzip_ok(p: &Path) -> bool {
    let Ok(f) = std::fs::File::open(p) else { return false };
    let mut d = flate2::read::MultiGzDecoder::new(std::io::BufReader::with_capacity(1 << 20, f));
    std::io::copy(&mut d, &mut std::io::sink()).is_ok()
}

fn stamp_of(url: &str) -> String {
    let base = url.rsplit('/').next().unwrap_or(url);
    base.strip_suffix(".jsonl.gz").unwrap_or(base).to_string()
}

/// fetch-data.sh: download any missing bulk file. Never fails boot.
pub async fn ensure_data(dir: &Path) {
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        tracing::warn!(dir = %dir.display(), "fetch-data: cannot create data dir: {e}");
        return;
    }
    let c = client();
    for (ty, out, pinned) in BULK {
        let dest = dir.join(out);
        if std::fs::metadata(&dest).map(|m| m.len() > 0).unwrap_or(false) {
            tracing::info!("fetch-data: {out} already present, skipping");
            continue;
        }
        let url = match live_url(&c, ty).await {
            Some(u) => u,
            None => {
                tracing::warn!("fetch-data: {ty}: could not resolve live URL, using pinned {pinned}");
                pinned.to_string()
            }
        };
        tracing::info!("fetch-data: downloading {out} from {url}");
        let part = dir.join(format!("{out}.part"));
        match download(&c, &url, &part).await {
            Ok(n) => {
                if let Err(e) = std::fs::rename(&part, &dest) {
                    tracing::warn!("fetch-data: WARN {out} rename failed: {e}");
                    let _ = std::fs::remove_file(&part);
                    continue;
                }
                let sd = dir.join(".refresh-stamps");
                let _ = std::fs::create_dir_all(&sd);
                let _ = std::fs::write(sd.join(format!("{out}.stamp")), format!("{}\n", stamp_of(&url)));
                tracing::info!("fetch-data: {out} OK ({n} bytes)");
            }
            Err(e) => tracing::warn!(
                "fetch-data: WARN {out} download failed — continuing; search runs without it (otag:/cards need it): {e}"
            ),
        }
    }
}

/// Exclusive non-blocking flock on `.refresh.lock` (same file the host
/// script locks). None = someone else is refreshing.
struct Flock(#[allow(dead_code)] std::fs::File);
fn try_lock(dir: &Path) -> Option<Flock> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".refresh.lock")).ok()?;
    // SAFETY: plain flock(2) on an fd we own; released on drop/close.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(Flock(f))
}

pub enum RefreshOne {
    Applied(String),
    Current,
    Failed,
    Unreachable,
}

/// refresh_one(): stamp compare → .part → gzip test → backup → swap → stamp.
async fn refresh_one(c: &Https, dir: &Path, ty: &str, out: &str) -> RefreshOne {
    let dest = dir.join(out);
    let Some(url) = live_url(c, ty).await else {
        if get(c, &bulk_meta_base(), Duration::from_secs(15)).await.is_ok() {
            tracing::warn!("scryfall refresh: {ty} — could not resolve live URL from /bulk-data (old data kept)");
            return RefreshOne::Failed;
        }
        return RefreshOne::Unreachable;
    };
    let stamp = stamp_of(&url);
    let stamp_file = dir.join(".refresh-stamps").join(format!("{out}.stamp"));
    let cur = std::fs::read_to_string(&stamp_file).unwrap_or_default();
    let present = std::fs::metadata(&dest).map(|m| m.len() > 0).unwrap_or(false);
    if cur.trim() == stamp && present {
        return RefreshOne::Current;
    }
    let part = dir.join(format!("{out}.part"));
    if let Err(e) = download(c, &url, &part).await {
        tracing::warn!("scryfall refresh: {ty} download failed for {url} (old data kept): {e}");
        return RefreshOne::Failed;
    }
    let p2 = part.clone();
    if !tokio::task::spawn_blocking(move || gzip_ok(&p2)).await.unwrap_or(false) {
        let _ = std::fs::remove_file(&part);
        tracing::warn!("scryfall refresh: {ty} downloaded file failed gzip integrity check (old data kept)");
        return RefreshOne::Failed;
    }
    let bdir = dir.join(".backup");
    let _ = std::fs::create_dir_all(&bdir);
    if present {
        let _ = std::fs::copy(&dest, bdir.join(out));
    }
    if let Err(e) = std::fs::rename(&part, &dest) {
        let _ = std::fs::remove_file(&part);
        tracing::warn!("scryfall refresh: {ty} swap failed: {e}");
        return RefreshOne::Failed;
    }
    let _ = std::fs::create_dir_all(dir.join(".refresh-stamps"));
    let _ = std::fs::write(&stamp_file, format!("{stamp}\n"));
    tracing::info!("applied {out} -> {stamp} ({} bytes)", std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0));
    RefreshOne::Applied(out.to_string())
}

/// The health probe the host script ran after a restart, against the
/// candidate index BEFORE it goes live.
async fn healthy(l: &crate::lsf::Loaded) -> bool {
    use tower::ServiceExt;
    let req = axum::http::Request::get("/cards/search?q=lotus").body(axum::body::Body::empty()).unwrap();
    matches!(l.router.clone().oneshot(req).await, Ok(r) if r.status().is_success())
}

async fn rebuild(eng: &Arc<InProcessEngine>) -> Option<crate::lsf::Loaded> {
    let e = Arc::clone(eng);
    let l = tokio::task::spawn_blocking(move || e.build()).await.ok().flatten()?;
    healthy(&l).await.then_some(l)
}

/// One refresh pass (also the unit tests' entry point). Returns true when a
/// new index was installed.
pub async fn refresh_pass(eng: &Arc<InProcessEngine>) -> bool {
    let dir = eng.data_dir.clone();
    let Some(_lock) = try_lock(&dir) else { return false };
    let c = client();
    let mut changed: Vec<String> = Vec::new();
    if !matches!(std::env::var("SCRYBOX_LSF_FETCH").as_deref(), Ok("off" | "0" | "false")) {
        for (ty, out, _) in BULK {
            if let RefreshOne::Applied(f) = refresh_one(&c, &dir, ty, out).await {
                changed.push(f);
            }
        }
    }
    let stale = eng.current().map(|l| l.fingerprint != crate::lsf::fingerprint(&dir)).unwrap_or(true);
    if changed.is_empty() && !stale {
        return false;
    }
    match rebuild(eng).await {
        Some(l) => {
            eng.install(l);
            tracing::info!(changed = ?changed, "scryfall-local refreshed & healthy (in-process hot swap)");
            true
        }
        None if changed.is_empty() => {
            // Externally changed files that do not load: keep serving the old
            // index, and do not retry until they change again.
            tracing::error!("ALERT local scryfall: bulk files changed on disk but the new index failed to build/health-check — old index kept");
            false
        }
        None => {
            tracing::error!(changed = ?changed, "ALERT local scryfall unhealthy after refresh — rolling back");
            for f in &changed {
                let b = dir.join(".backup").join(f);
                if b.exists() {
                    let _ = std::fs::copy(&b, dir.join(f));
                    let _ = std::fs::remove_file(dir.join(".refresh-stamps").join(format!("{f}.stamp")));
                } else {
                    tracing::error!("ALERT no backup for {f} — leaving new file in place");
                }
            }
            // The old index never stopped serving; re-stamp its fingerprint
            // view by rebuilding only if nothing is live at all.
            if !eng.is_ready() {
                if let Some(l) = rebuild(eng).await {
                    eng.install(l);
                }
            }
            false
        }
    }
}

/// Seconds until the next local HH:MM (default 05:00 — the host cron's slot).
fn secs_until(hh: u32, mm: u32) -> u64 {
    use chrono::{Local, Timelike};
    let now = Local::now();
    let now_s = now.num_seconds_from_midnight() as i64;
    let target = (hh * 3600 + mm * 60) as i64;
    let d = if target > now_s { target - now_s } else { 86_400 - now_s + target };
    d.max(1) as u64
}

/// `SCRYBOX_LSF_REFRESH_AT=HH:MM` (default 05:00), `off` disables. Also
/// polls the data dir for externally changed files every 15 min.
pub async fn refresh_loop(eng: Arc<InProcessEngine>) {
    let at = std::env::var("SCRYBOX_LSF_REFRESH_AT").unwrap_or_else(|_| "05:00".into());
    if matches!(at.as_str(), "off" | "0" | "false" | "") {
        tracing::info!("local scryfall: daily refresh disabled (SCRYBOX_LSF_REFRESH_AT=off)");
        return;
    }
    let (hh, mm) = at
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<u32>().ok()?.min(23), m.parse::<u32>().ok()?.min(59))))
        .unwrap_or((5, 0));
    let poll = Duration::from_secs(15 * 60);
    loop {
        let until = Duration::from_secs(secs_until(hh, mm));
        if until <= poll {
            tokio::time::sleep(until).await;
            refresh_pass(&eng).await;
            tokio::time::sleep(Duration::from_secs(61)).await;
        } else {
            tokio::time::sleep(poll).await;
            // Cheap external-change check only (no network).
            let dir = eng.data_dir.clone();
            let stale = eng.current().map(|l| l.fingerprint != crate::lsf::fingerprint(&dir)).unwrap_or(true);
            let has_cards = dir.join("default-cards.jsonl.gz").exists();
            if stale && has_cards {
                if let Some(_lock) = try_lock(&dir) {
                    drop(_lock);
                    if let Some(l) = rebuild(&eng).await {
                        eng.install(l);
                        tracing::info!("local scryfall: bulk files changed on disk — index reloaded");
                    } else {
                        // Remember the failed fingerprint so we do not rebuild-loop.
                        tracing::error!("ALERT local scryfall: on-disk bulk files failed to load — old index kept");
                        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
                    }
                }
            }
        }
    }
}

