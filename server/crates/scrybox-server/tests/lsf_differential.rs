//! TOTORO mono — LIVE differential: the in-process engine vs the running
//! `scryfall-local` sidecar container (the production engine).
//!
//! Gated on two envs (SKIPs loudly otherwise — it needs the live sidecar
//! and a copy of ITS bulk data, which only exist on the docker host):
//!   SCRYBOX_LSF_DIFF_URL   e.g. http://172.29.0.2:8420  (docker inspect scryfall-local)
//!   SCRYBOX_LSF_DIFF_DATA  a COPY of the sidecar's /data (same files it booted from)
//! Optional SCRYBOX_LSF_DIFF_REPORT=<path> writes a JSON summary.
//!
//! Corpus: tests/fixtures/scryq_expected.json (291 Scryfall-syntax queries
//! shared with the scryq differential) + tests/fixtures/lsf/diff_queries.json
//! (local-scryfall/tests.sh queries + side-index/price/edge-case extras),
//! deduped; each as /cards/search page 1, a third also with page=2 /
//! unique=prints / order variants, plus suggest, card-by-id,
//! /cards/collection POST, /sets, /api/sets, /api/set/{code} and the static
//! portal files. Per request: status, content-type, cache-control and the
//! FULL body bytes (only `took_ms` wall-clock scrubbed); for searches the
//! ids, their order and total_cards are additionally tallied.

use std::time::{Duration, Instant};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;

use scrybox_server::lsf;

type Resp = (u16, Vec<(String, String)>, Vec<u8>);

/// Two fields the SIDECAR ITSELF does not reproduce request-to-request:
/// `took_ms` (wall clock) and the query-param ORDER inside `self` /
/// `next_page` (api.rs page_link iterates a std HashMap — RandomState —
/// so the live container answers `?dir=asc&q=..&order=..` on one request
/// and `?order=..&q=..&dir=asc` on the next; measured with curl x4). The
/// params themselves are compared — only their order is canonicalised.
fn scrub(b: Vec<u8>) -> Vec<u8> {
    let re = regex::bytes::Regex::new(r#""took_ms":[0-9.]+"#).unwrap();
    let b = re.replace_all(&b, &b"\"took_ms\":0"[..]).into_owned();
    let link = regex::bytes::Regex::new(r#""(self|next_page)":"([^"?]*)\?([^"]*)""#).unwrap();
    link.replace_all(&b, |c: &regex::bytes::Captures| {
        let q = String::from_utf8_lossy(&c[3]).into_owned();
        let mut parts: Vec<&str> = q.split('&').collect();
        let last = parts.pop().unwrap_or(""); // page/offset always appended last
        parts.sort();
        parts.push(last);
        format!("\"{}\":\"{}?{}\"", String::from_utf8_lossy(&c[1]), String::from_utf8_lossy(&c[2]), parts.join("&")).into_bytes()
    })
    .into_owned()
}

async fn collect(res: axum::response::Response) -> Resp {
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

fn enc(s: &str) -> String {
    scrybox_server::sidecar::encode_uri_component(s)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inprocess_matches_live_sidecar() {
    let (Ok(live), Ok(data)) = (std::env::var("SCRYBOX_LSF_DIFF_URL"), std::env::var("SCRYBOX_LSF_DIFF_DATA")) else {
        eprintln!("SKIP lsf_differential: set SCRYBOX_LSF_DIFF_URL + SCRYBOX_LSF_DIFF_DATA (live sidecar + copy of its data)");
        return;
    };
    let live = live.trim_end_matches('/').to_string();
    // SCRYBOX_LSF_REPO_ROOT: the binary may be built on another host (remote
    // cargo) and run here, where the compile-time manifest dir does not exist.
    let root = std::env::var("SCRYBOX_LSF_REPO_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.."))
        .canonicalize()
        .unwrap();
    std::env::set_var("SCRYBOX_LSF_DATA_DIR", &data);
    std::env::set_var("SCRYBOX_LSF_FETCH", "off");
    std::env::set_var("SCRYBOX_LSF_REFRESH_AT", "off");
    std::env::set_var("SCRYBOX_LSF_STATIC_DIR", root.join("local-scryfall/static"));
    std::env::set_var("SCRYBOX_LSF_VENDORED_DIR", root.join("local-scryfall/data"));
    let t0 = Instant::now();
    lsf::start_from_env();
    assert!(lsf::wait_ready(Duration::from_secs(600)).await, "engine never became ready");
    let boot_ms = t0.elapsed().as_millis();
    eprintln!("in-process engine ready in {boot_ms} ms");

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http::<Body>();
    let get_both = |path: String, post: Option<String>| {
        let client = client.clone();
        let live = live.clone();
        async move {
            let mk = |base: &str| {
                let b = Request::builder()
                    .method(if post.is_some() { "POST" } else { "GET" })
                    .uri(format!("{base}{path}"))
                    .header("host", "scryfall-local:8420")
                    .header("user-agent", "Scrybox/1.0");
                let b = if post.is_some() { b.header("content-type", "application/json") } else { b };
                b.body(post.clone().map(Body::from).unwrap_or_else(Body::empty)).unwrap()
            };
            let a = collect(lsf::send(&client, mk(lsf::INPROC_BASE)).await.expect("inproc")).await;
            let b = collect(client.request(mk(&live)).await.expect("live sidecar").map(Body::new)).await;
            (a, b)
        }
    };

    let mut queries: Vec<String> = Vec::new();
    let corpus: Vec<Value> = serde_json::from_str(include_str!("fixtures/scryq_expected.json")).unwrap();
    queries.extend(corpus.iter().map(|r| r["q"].as_str().unwrap().to_string()));
    let extra: Vec<String> = serde_json::from_str(include_str!("fixtures/lsf/diff_queries.json")).unwrap();
    queries.extend(extra);
    let mut seen = std::collections::HashSet::new();
    queries.retain(|q| seen.insert(q.clone()));

    let mut requests = 0usize;
    let mut diffs: Vec<String> = Vec::new();
    let mut searches = 0usize;
    let mut id_order_equal = 0usize;
    let mut totals_equal = 0usize;
    let mut nonempty = 0usize;
    let mut ids_seen: Vec<String> = Vec::new();
    let compare = |what: String, a: &Resp, b: &Resp, diffs: &mut Vec<String>| {
        if a != b {
            let at = a.2.iter().zip(b.2.iter()).position(|(x, y)| x != y).unwrap_or(a.2.len().min(b.2.len()));
            let lo = at.saturating_sub(80);
            diffs.push(format!(
                "{what}: inproc {} {:?} {}B vs live {} {:?} {}B @{at}: {:?} | {:?}",
                a.0, a.1, a.2.len(), b.0, b.1, b.2.len(),
                String::from_utf8_lossy(&a.2[lo..(at + 80).min(a.2.len())]),
                String::from_utf8_lossy(&b.2[lo..(at + 80).min(b.2.len())])
            ));
        }
    };

    for (i, q) in queries.iter().enumerate() {
        let mut variants = vec![format!("/cards/search?q={}", enc(q))];
        if i % 3 == 0 {
            variants.push(format!("/cards/search?q={}&page=2", enc(q)));
            variants.push(format!("/cards/search?q={}&unique=prints", enc(q)));
            variants.push(format!("/cards/search?q={}&order=released&dir=asc", enc(q)));
        }
        for path in variants {
            let (a, b) = get_both(path.clone(), None).await;
            requests += 1;
            searches += 1;
            let ja: Option<Value> = serde_json::from_slice(&a.2).ok();
            let jb: Option<Value> = serde_json::from_slice(&b.2).ok();
            let ids = |j: &Option<Value>| -> Vec<String> {
                j.as_ref()
                    .and_then(|j| j["data"].as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|c| c["id"].as_str().map(str::to_string))
                    .collect()
            };
            let (ia, ib) = (ids(&ja), ids(&jb));
            if ia == ib {
                id_order_equal += 1;
            }
            let tot = |j: &Option<Value>| j.as_ref().map(|j| j["total_cards"].clone());
            if tot(&ja) == tot(&jb) {
                totals_equal += 1;
            }
            if !ia.is_empty() {
                nonempty += 1;
                if ids_seen.len() < 60 {
                    ids_seen.push(ia[0].clone());
                }
            }
            compare(path, &a, &b, &mut diffs);
        }
    }
    // suggest
    for q in queries.iter().take(80) {
        let p = format!("/api/suggest?q={}", enc(q));
        let (a, b) = get_both(p.clone(), None).await;
        requests += 1;
        compare(p, &a, &b, &mut diffs);
    }
    // by id + api/card
    ids_seen.sort();
    ids_seen.dedup();
    for id in &ids_seen {
        for p in [format!("/cards/{id}"), format!("/api/card/{id}")] {
            let (a, b) = get_both(p.clone(), None).await;
            requests += 1;
            compare(p, &a, &b, &mut diffs);
        }
    }
    // collection POST (ids + a name + a set/cn + a miss)
    let mut idents: Vec<Value> = ids_seen.iter().take(40).map(|i| serde_json::json!({ "id": i })).collect();
    idents.push(serde_json::json!({"name": "Lightning Bolt"}));
    idents.push(serde_json::json!({"set": "lea", "collector_number": "161"}));
    idents.push(serde_json::json!({"id": "00000000-0000-0000-0000-000000000000"}));
    let body = serde_json::json!({ "identifiers": idents }).to_string();
    let (a, b) = get_both("/cards/collection".into(), Some(body)).await;
    requests += 1;
    compare("/cards/collection POST".into(), &a, &b, &mut diffs);
    // sets + portal statics
    let mut fixed: Vec<String> = ["/sets", "/api/sets", "/api/sets?q=dominaria", "/api/healthz-not", "/", "/index.html", "/app.js", "/styles.css", "/sets.html", "/sets.js", "/set.html", "/set.js", "/sets.css", "/sets/lea", "/nope"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for code in ["lea", "dom", "mh2", "sld", "m21", "zen", "neo", "one", "woe", "lci", "mkm", "otj", "blb", "dsk", "fdn", "plst", "cmm", "40k", "who", "ltr"] {
        fixed.push(format!("/api/set/{code}"));
    }
    for p in fixed {
        let (a, b) = get_both(p.clone(), None).await;
        requests += 1;
        compare(p, &a, &b, &mut diffs);
    }

    // Through scrybox's own consumers: Scry client (LOCAL_SCRYFALL swap) and
    // the sidecar name walk, in-process sentinel vs the live sidecar URL.
    std::env::set_var("LOCAL_SCRYFALL_ENABLED", "true");
    std::env::set_var("SCRYBOX_NODE_UPSTREAM", "");
    let tmp = tempfile::tempdir().unwrap();
    rusqlite::Connection::open(tmp.path().join("t.db")).unwrap();
    let mut cfg = scrybox_server::Config::from_env().unwrap();
    cfg.db_path = Some(tmp.path().join("t.db"));
    std::env::remove_var("LOCAL_SCRYFALL_URL");
    let st_in = scrybox_server::AppState::new(cfg.clone());
    std::env::set_var("LOCAL_SCRYFALL_URL", &live);
    let st_live = scrybox_server::AppState::new(cfg);
    std::env::remove_var("LOCAL_SCRYFALL_URL");
    assert_eq!(st_in.scry.env().local_url, lsf::INPROC_BASE);
    let mut consumer_checks = 0usize;
    for q in queries.iter().take(120) {
        let u = format!("https://api.scryfall.com/cards/search?q={}", enc(q));
        let a = st_in.scry.get(&u, &Default::default()).await.map_err(|e| (e.status, e.message.contains("timeout")));
        let b = st_live.scry.get(&u, &Default::default()).await.map_err(|e| (e.status, e.message.contains("timeout")));
        let norm = |r: Result<Option<Value>, (Option<u16>, bool)>| {
            r.map(|o| {
                o.map(|mut v| {
                    v.as_object_mut().map(|m| {
                        m.remove("took_ms");
                        m.remove("self");
                        m.remove("next_page");
                    });
                    v
                })
            })
        };
        consumer_checks += 1;
        if norm(a) != norm(b) {
            diffs.push(format!("scry.get {q:?} differs"));
        }
    }
    for name in ["Lightning Bolt", "Shalai, Voice of Plenty", "Fire // Ice", "Black Lotus", "Opt", "Llanowar Elves", "Sol Ring", "Nonexistent Cardname Zz"] {
        let a = scrybox_server::sidecar::walk_name(&st_in, lsf::INPROC_BASE, name, "English", 0, 200).await.ok().map(|o| (o.cards, o.total, o.source));
        let b = scrybox_server::sidecar::walk_name(&st_live, &live, name, "English", 0, 200).await.ok().map(|o| (o.cards, o.total, o.source));
        consumer_checks += 1;
        if a != b {
            diffs.push(format!("walk_name {name:?} differs"));
        }
    }

    let summary = serde_json::json!({
        "consumer_checks": consumer_checks,
        "boot_to_ready_ms": boot_ms as u64,
        "distinct_queries": queries.len(),
        "requests": requests,
        "search_requests": searches,
        "search_nonempty": nonempty,
        "search_ids_and_order_equal": id_order_equal,
        "search_totals_equal": totals_equal,
        "byte_diffs": diffs.len(),
        "diffs": diffs,
    });
    eprintln!("{}", serde_json::to_string_pretty(&summary).unwrap());
    if let Ok(p) = std::env::var("SCRYBOX_LSF_DIFF_REPORT") {
        std::fs::write(p, serde_json::to_string_pretty(&summary).unwrap()).unwrap();
    }
    assert!(queries.len() >= 300, "corpus too small: {}", queries.len());
    assert!(diffs.is_empty(), "{} byte diffs vs live sidecar", summary["byte_diffs"]);
}
