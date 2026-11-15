//! TOTORO mono — the local Scryfall engine IN-PROCESS (hermetic proof).
//!
//! One test function: the engine, the Scry client env and the portal gate
//! are process-global (env + OnceLock), so every arm runs sequentially.
//!
//! * Red/green inside the test: with the in-process engine NOT configured
//!   (the pre-mono default) the portal answers the 502 "Card portal
//!   unavailable" arm and the sidecar walk base is None; with
//!   SCRYBOX_LSF_DATA_DIR set (and no external URL) the very same legs are
//!   answered by the engine without any socket.
//! * Self-differential: the same index served by the STANDALONE router
//!   (`scryfall-api` bin's router, over TCP like the sidecar) vs the
//!   in-process `ScryfallEngine` — status, content-type, cache-control and
//!   body bytes must be identical for every probe.
//! * Explicit external URL keeps the HTTP fallback (SCRYBOX_SIDECAR_URL /
//!   LOCAL_SCRYFALL_URL win over the in-process sentinel).

use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use axum::{body::Body, http::Request, Router};
use http_body_util::BodyExt;
use tower::ServiceExt;

use scrybox_server::{app, lsf, AppState, Config};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn cfg_with(routes: &[&str], dir: &std::path::Path) -> Config {
    rusqlite::Connection::open(dir.join("t.db")).unwrap();
    std::env::set_var("SCRYBOX_NODE_UPSTREAM", "");
    let mut c = Config::from_env().unwrap();
    c.rust_routes = routes.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    c.db_path = Some(dir.join("t.db"));
    c
}

async fn collect(res: axum::response::Response) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let status = res.status().as_u16();
    let keep = ["content-type", "cache-control", "location"];
    let mut h: Vec<(String, String)> = res
        .headers()
        .iter()
        .filter(|(k, _)| keep.contains(&k.as_str()))
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    h.sort();
    let b = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, h, scrub(b))
}

/// `took_ms` is wall-clock timing — the only field two runs of the same
/// handler may legitimately disagree on. Everything else is compared raw.
fn scrub(b: Vec<u8>) -> Vec<u8> {
    let re = regex::bytes::Regex::new(r#""took_ms":[0-9.]+"#).unwrap();
    re.replace_all(&b, &b"\"took_ms\":0"[..]).into_owned()
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<Body>();
    let req = Request::get(format!("http://{addr}{path}"))
        .header("host", "scryfall-local:8420")
        .body(Body::empty())
        .unwrap();
    let res = client.request(req).await.unwrap();
    collect(res.map(Body::new)).await
}

async fn inproc_get(path: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<Body>();
    let req = Request::get(format!("{}{path}", lsf::INPROC_BASE)).body(Body::empty()).unwrap();
    collect(lsf::send(&client, req).await.expect("in-process engine answered")).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inprocess_engine_replaces_sidecar_byte_identically() {
    let root = repo();
    let static_dir = root.join("local-scryfall/static");
    let vendored = root.join("local-scryfall/data");
    for k in ["LOCAL_SCRYFALL_URL", "SCRYBOX_SIDECAR_URL", "SCRYFALL_DATA_DIR", "SCRYBOX_LSF_DATA_DIR", "SCRYBOX_LSF"] {
        std::env::remove_var(k);
    }
    std::env::set_var("LOCAL_WEB_PORTAL_ENABLED", "true");
    std::env::set_var("LOCAL_SCRYFALL_ENABLED", "true");

    // ---------------- RED arm: engine not configured (pre-mono default) ----
    let tmp = tempfile::tempdir().unwrap();
    assert!(!lsf::inproc_configured());
    let cfg = cfg_with(&["static_assets"], tmp.path());
    assert_eq!(cfg.sidecar_url, None, "no engine, no explicit URL -> no walk base");
    let r: Router = app(AppState::new(cfg));
    let (st, _, body) = collect(
        r.oneshot(Request::get("/scryfall-portal/").body(Body::empty()).unwrap()).await.unwrap(),
    )
    .await;
    assert_eq!(st, 502, "pre-mono: unreachable sidecar -> portal unavailable");
    assert!(String::from_utf8_lossy(&body).contains("Card portal unavailable"));

    // ---------------- GREEN arm: in-process engine ------------------------
    let data = tmp.path().join("scryfall-data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::copy(
        root.join("server/crates/scrybox-server/tests/fixtures/lsf/default-cards.jsonl.gz"),
        data.join("default-cards.jsonl.gz"),
    )
    .unwrap();
    std::env::set_var("SCRYBOX_LSF_DATA_DIR", &data);
    std::env::set_var("SCRYBOX_LSF_FETCH", "off");
    std::env::set_var("SCRYBOX_LSF_REFRESH_AT", "off");
    std::env::set_var("SCRYBOX_LSF_STATIC_DIR", &static_dir);
    std::env::set_var("SCRYBOX_LSF_VENDORED_DIR", &vendored);
    assert!(lsf::inproc_configured());
    lsf::start_from_env();
    assert!(lsf::wait_ready(Duration::from_secs(120)).await, "engine never became ready");
    let eng = lsf::inproc().unwrap();
    assert!(eng.first_ready_ms() > 0);

    // Config / Scry env pick the in-process sentinel...
    let cfg = cfg_with(&["static_assets"], tmp.path());
    assert_eq!(cfg.sidecar_url.as_deref(), Some(lsf::INPROC_BASE));
    let st = AppState::new(cfg.clone());
    assert_eq!(st.scry.env().local_url, lsf::INPROC_BASE);
    // ...but an explicit external URL keeps the HTTP fallback.
    std::env::set_var("SCRYBOX_SIDECAR_URL", "http://10.9.9.9:8420/");
    std::env::set_var("LOCAL_SCRYFALL_URL", "http://10.9.9.9:8420");
    assert_eq!(Config::from_env().unwrap().sidecar_url.as_deref(), Some("http://10.9.9.9:8420"));
    assert_eq!(scrybox_server::scryfall::ScryEnv::from_env().local_url, "http://10.9.9.9:8420");
    std::env::remove_var("SCRYBOX_SIDECAR_URL");
    std::env::remove_var("LOCAL_SCRYFALL_URL");

    // Scry client (LOCAL_SCRYFALL swap): no socket, the engine answers.
    let v = st
        .scry
        .get("https://api.scryfall.com/cards/search?q=bolt", &Default::default())
        .await
        .expect("scry via in-process engine")
        .expect("json");
    assert!(v["total_cards"].as_u64().unwrap() >= 1, "{v}");
    assert!(v["data"].as_array().unwrap().iter().any(|c| c["name"] == "Lightning Bolt"));

    // Sidecar walk leg (field-filter search) over the sentinel base.
    let walk = scrybox_server::sidecar::walk_name(&st, lsf::INPROC_BASE, "Lightning Bolt", "English", 0, 50)
        .await
        .ok()
        .expect("walk answered in-process");
    assert_eq!(walk.source, "scryfall");
    assert!(!walk.cards.is_empty());

    // Portal through the scrybox app: served from the same binary.
    let r: Router = app(AppState::new(cfg));
    let (code, _, body) = collect(
        r.clone().oneshot(Request::get("/scryfall-portal/").body(Body::empty()).unwrap()).await.unwrap(),
    )
    .await;
    assert_eq!(code, 200);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("/scryfall-portal/"), "portal HTML rewritten onto the prefix");
    let (code, _, body) = collect(
        r.oneshot(Request::get("/scryfall-portal/api/search?q=opt").body(Body::empty()).unwrap()).await.unwrap(),
    )
    .await;
    assert_eq!(code, 200);
    assert!(String::from_utf8_lossy(&body).contains("\"Opt\""));

    // ---------------- self-differential vs the standalone router ----------
    let opts = eng.boot_opts();
    let ix = local_scryfall::load_index(&opts).unwrap();
    let standalone = local_scryfall::router(local_scryfall::state(ix), static_dir.to_str().unwrap());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, standalone).await.unwrap() });

    let probes = [
        "/cards/search?q=bolt",
        "/cards/search?q=lightning%20bolt",
        "/cards/search?q=t%3Acreature",
        "/cards/search?q=c%3Ag%20t%3Aelf",
        "/cards/search?q=unique%3Aprints%20forest",
        "/cards/search?q=is%3Aland",
        "/cards/search?q=cmc%3E%3D3",
        "/cards/search?q=o%3A%22draw%20a%20card%22",
        "/cards/search?q=bolt&order=released",
        "/cards/search?q=island&unique=prints&page=2",
        "/cards/search?q=xyzzynotacard",
        "/cards/search?q=%28",
        "/cards/search?q=",
        "/cards/search?q=!%22Fire%20//%20Ice%22",
        "/api/search?q=opt",
        "/api/cards/search?q=lotus",
        "/api/suggest?q=light",
        "/api/suggest?q=s",
        "/api/sets",
        "/sets",
        "/cards/named-nope",
        "/api/card/nope",
        "/index.html",
        "/app.js",
        "/styles.css",
        "/sets.html",
        "/sets/lea",
        "/nope.txt",
    ];
    let mut diffs = Vec::new();
    for p in probes {
        let a = inproc_get(p).await;
        let b = http_get(addr, p).await;
        if a != b {
            let at = a.2.iter().zip(b.2.iter()).position(|(x, y)| x != y).unwrap_or(a.2.len().min(b.2.len()));
            let lo = at.saturating_sub(60);
            diffs.push(format!(
                "{p}: inproc {} {:?} {}B vs standalone {} {:?} {}B @{at}: {:?} | {:?}",
                a.0, a.1, a.2.len(), b.0, b.1, b.2.len(),
                String::from_utf8_lossy(&a.2[lo..(at + 60).min(a.2.len())]),
                String::from_utf8_lossy(&b.2[lo..(at + 60).min(b.2.len())])
            ));
        }
    }
    // A real card id from the engine, by id + collection POST.
    let (_, _, body) = inproc_get("/cards/search?q=bolt").await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id = v["data"][0]["id"].as_str().unwrap().to_string();
    let (a, b) = (inproc_get(&format!("/cards/{id}")).await, http_get(addr, &format!("/cards/{id}")).await);
    assert_eq!(a.0, 200);
    if a != b {
        diffs.push(format!("/cards/{id} differs"));
    }
    assert!(diffs.is_empty(), "in-process vs standalone diffs:\n{}", diffs.join("\n"));

}
