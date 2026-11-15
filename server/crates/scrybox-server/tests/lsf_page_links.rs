//! TOTORO mono build — local-scryfall self/next_page links are deterministic.
//!
//! The engine used to echo the request's query params back into the
//! `self` / `next_page` links by iterating a `HashMap`, so two identical
//! requests (or the in-process engine vs the standalone router) could emit
//! the params in different orders — the cause of the flaky
//! `lsf_inproc::inprocess_engine_replaces_sidecar_byte_identically`.
//! The links must now preserve the REQUEST's param order exactly.

use std::path::PathBuf;

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

#[tokio::test]
async fn page_links_preserve_request_param_order() {
    let root = repo();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::copy(
        root.join("server/crates/scrybox-server/tests/fixtures/lsf/default-cards.jsonl.gz"),
        tmp.path().join("default-cards.jsonl.gz"),
    )
    .unwrap();
    let opts = local_scryfall::BootOpts {
        data_dir: Some(tmp.path().to_path_buf()),
        vendored_dir: Some(root.join("local-scryfall/data")),
        dev_fallbacks: false,
        file_envs: false,
    };
    let ix = local_scryfall::load_index(&opts).expect("fixture index");
    let state = local_scryfall::state(ix);
    let static_dir = root.join("local-scryfall/static");

    // Many params -> a HashMap would reorder them across instances.
    let q = "q=t%3Aland&unique=prints&order=name&dir=asc&include_extras=false&zz=1&aa=2&limit=1";
    for i in 0..40 {
        // A fresh router each time: a fresh RandomState per request map.
        let r = local_scryfall::router(state.clone(), static_dir.to_str().unwrap());
        let res = r
            .oneshot(
                Request::get(format!("/cards/search?{q}"))
                    .header("host", "h:1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let b = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["self"], format!("http://h:1/cards/search?{q}&page=1"), "iteration {i}");
        assert_eq!(v["has_more"], true, "fixture must have >1 land");
        assert_eq!(v["next_page"], format!("http://h:1/cards/search?{q}&page=2"), "iteration {i}");
    }

    // offset-style paging keeps order too, with the cursor last.
    let r = local_scryfall::router(state.clone(), static_dir.to_str().unwrap());
    let res = r
        .oneshot(
            Request::get("/cards/search?zz=1&offset=0&q=t%3Aland&aa=2&limit=1")
                .header("host", "h:1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let b = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["next_page"], "http://h:1/cards/search?zz=1&q=t%3Aland&aa=2&limit=1&offset=1");
}
