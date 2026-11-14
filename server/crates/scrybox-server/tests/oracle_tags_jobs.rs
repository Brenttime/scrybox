//! Oracle Tags maintenance + session purge + RUST_JOBS parsing
//! (backend/src/oracleTags.js, server.js daily timer). A local axum mock
//! serves /bulk-data/* metadata, the bulk files (gzip + plain JSONL), and
//! /cards/collection for the residual exact-id fallback.

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::{extract::Request, response::IntoResponse, routing::any, Router};
use rusqlite::Connection;
use scrybox_server::oracle_tags::{self, BulkHttp, TagsStatus};
use scrybox_server::scry_jobs::{purge_sessions, JobSet};
use scrybox_server::scryfall::{Scry, ScryEnv};
use serde_json::json;

fn db(dir: &std::path::Path) -> (std::path::PathBuf, Connection) {
    let p = dir.join("t.db");
    scrybox_server::migrations::run_boot_migration(
        &p,
        &scrybox_server::migrations::MigOpts { admin_password: None, model_dir: None, user_art_dir: None },
    )
    .unwrap();
    let c = Connection::open(&p).unwrap();
    (p, c)
}

fn gz(s: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(s.as_bytes()).unwrap();
    e.finish().unwrap()
}

#[derive(Default)]
struct Mock {
    hits: Mutex<Vec<String>>,
    tags_updated: Mutex<String>,
}

async fn mock(state: Arc<Mock>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let b2 = base.clone();
    let app = Router::new().fallback(any(move |req: Request| {
        let st = state.clone();
        let base = b2.clone();
        async move {
            let path = req.uri().path().to_string();
            st.hits.lock().unwrap().push(format!("{} {}", req.method(), path));
            let tags = [
                json!({"object":"tag","type":"oracle","id":"t-root","slug":"Removal","label":"Removal","aliases":["Kill"," kill "],"child_ids":["t-kid","t-missing"],"taggings":[{"oracle_id":"o-1"}]}),
                json!({"object":"tag","type":"oracle","id":"t-kid","slug":"creature-removal","parent_ids":["t-root"],"taggings":[{"oracle_id":"o-2"},{"oracle_id":"o-2"}]}),
                json!({"object":"tag","type":"illustration","id":"t-art","slug":"art"}),
            ];
            let jsonl: String = tags.iter().map(|t| format!("{t}\n")).collect();
            match path.as_str() {
                "/bulk-data/oracle_tags" => axum::Json(json!({
                    "object":"bulk_data","type":"oracle_tags",
                    "download_uri": format!("{base}/files/tags.jsonl.gz"),
                    "updated_at": st.tags_updated.lock().unwrap().clone()
                })).into_response(),
                "/files/tags.jsonl.gz" => gz(&jsonl).into_response(),
                "/bulk-data/default_cards" => axum::Json(json!({
                    "object":"bulk_data","type":"default_cards",
                    "download_uri": format!("{base}/files/default.json")
                })).into_response(),
                // JSON-array shape (trailing commas) — Node strips them.
                "/files/default.json" => format!(
                    "[\n{},\n{},\n{}\n]\n",
                    json!({"id":"00000000-0000-0000-0000-000000000001","oracle_id":"o-1","layout":"normal","lang":"en"}),
                    json!({"id":"00000000-0000-0000-0000-000000000002","layout":"token","lang":"en","card_faces":[{"oracle_id":"o-tok"},{"oracle_id":"o-tok"}]}),
                    json!({"id":"00000000-0000-0000-0000-000000000009","oracle_id":"o-x","layout":"normal","lang":"ja"}),
                ).into_response(),
                "/cards/collection" => axum::Json(json!({"data":[
                    {"object":"card","id":"00000000-0000-0000-0000-000000000003","oracle_id":"o-3","name":"Three","set":"tst","collector_number":"3","layout":"normal","lang":"en"}
                ],"not_found":[]})).into_response(),
                _ => (axum::http::StatusCode::NOT_FOUND, "nope").into_response(),
            }
        }
    }));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

#[tokio::test]
async fn oracle_tags_refresh_import_gate_and_backfill() {
    let dir = tempfile::tempdir().unwrap();
    let (_p, conn) = db(dir.path());
    conn.execute_batch(
        "INSERT OR IGNORE INTO app_settings (id) VALUES (1);
         INSERT INTO card_cache (id, name, set_id, number) VALUES
           ('mtg-00000000-0000-0000-0000-000000000001','One','tst','1'),
           ('mtg-00000000-0000-0000-0000-000000000002','Tok','tst','2'),
           ('mtg-00000000-0000-0000-0000-000000000003','Three','tst','3');",
    )
    .unwrap();
    let st = Arc::new(Mock::default());
    *st.tags_updated.lock().unwrap() = "2026-11-14T00:00:00Z".into();
    let base = mock(st.clone()).await;
    let http = BulkHttp::new(&base);
    let scry = Scry::new(ScryEnv { local: false, local_url: base.clone(), base_url: base.clone(), gap_scale: 0.0 });

    let r = oracle_tags::run_maintenance(&conn, &http, &scry).await;
    let TagsStatus::Refreshed(imp) = r.tags.expect("tags ok") else { panic!("expected refresh") };
    assert_eq!((imp.tags, imp.assignments), (2, 2), "non-oracle tag skipped, duplicate tagging deduped");
    let q = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(q("SELECT COUNT(*) FROM oracle_tag_generations WHERE active = 1"), 1);
    assert_eq!(q("SELECT COUNT(*) FROM oracle_tag_edges"), 1, "dangling child edge dropped");
    assert_eq!(q("SELECT COUNT(*) FROM oracle_tag_closure"), 3, "root->root, root->kid, kid->kid");
    assert_eq!(q("SELECT COUNT(*) FROM oracle_tag_aliases WHERE tag_id='t-root'"), 2, "removal + kill (trim/lower/dedup)");
    assert!(q("SELECT oracle_tags_checked_at FROM app_settings WHERE id=1") > 0);

    let b = r.backfill.expect("backfill ok");
    assert_eq!((b.bulk, b.fallback, b.unresolved), (2, 1, 0), "bulk 1+2 (ja skipped), fallback 3");
    let row = |id: &str| -> (Option<String>, Option<i64>) {
        conn.query_row("SELECT oracle_id, scryfall_search_eligible FROM card_cache WHERE id = ?", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    };
    assert_eq!(row("mtg-00000000-0000-0000-0000-000000000001"), (Some("o-1".into()), Some(1)));
    assert_eq!(row("mtg-00000000-0000-0000-0000-000000000002"), (Some("o-tok".into()), Some(0)));
    assert_eq!(row("mtg-00000000-0000-0000-0000-000000000003"), (Some("o-3".into()), Some(1)));
    assert!(q("SELECT oracle_id_backfill_completed_at FROM app_settings WHERE id=1") > 0);

    // Second run inside the day: not due, no network for tags or bulk.
    let before = st.hits.lock().unwrap().len();
    let r2 = oracle_tags::run_maintenance(&conn, &http, &scry).await;
    assert_eq!(r2.tags.unwrap(), TagsStatus::NotDue);
    assert_eq!(st.hits.lock().unwrap().len(), before, "gated run is offline");

    // Forced re-check with the same updated_at: 'current', no download, one generation.
    let r3 = oracle_tags::refresh_oracle_tags(&conn, &http, i64::MAX / 2, true).await.unwrap();
    assert_eq!(r3, TagsStatus::Current);
    // New upstream stamp: swap generation, old one reaped.
    *st.tags_updated.lock().unwrap() = "2026-11-15T00:00:00Z".into();
    let r4 = oracle_tags::refresh_oracle_tags(&conn, &http, i64::MAX / 2, true).await.unwrap();
    assert!(matches!(r4, TagsStatus::Refreshed(_)));
    assert_eq!(q("SELECT COUNT(*) FROM oracle_tag_generations"), 1, "old generation reaped");
    assert_eq!(q("SELECT COUNT(*) FROM oracle_tags"), 2);
}

#[tokio::test]
async fn oracle_tags_failed_download_keeps_gate_open() {
    let dir = tempfile::tempdir().unwrap();
    let (_p, conn) = db(dir.path());
    conn.execute_batch("INSERT OR IGNORE INTO app_settings (id) VALUES (1)").unwrap();
    let http = BulkHttp::new("http://127.0.0.1:9"); // nothing listens
    let err = oracle_tags::refresh_oracle_tags(&conn, &http, 1_000_000_000_000, false).await;
    assert!(err.is_err());
    let checked: Option<i64> =
        conn.query_row("SELECT oracle_tags_checked_at FROM app_settings WHERE id=1", [], |r| r.get(0)).unwrap();
    assert_eq!(checked, None, "failure must not advance the daily gate");
}

#[test]
fn session_purge_removes_only_expired() {
    let dir = tempfile::tempdir().unwrap();
    let (_p, conn) = db(dir.path());
    conn.execute_batch(
        "PRAGMA foreign_keys=OFF;
         INSERT INTO sessions VALUES ('old',1,DATETIME('now','-1 hour')),('new',1,DATETIME('now','+1 day'));",
    )
    .unwrap();
    assert_eq!(purge_sessions(&conn).unwrap(), 1);
    let left: String = conn.query_row("SELECT group_concat(token) FROM sessions", [], |r| r.get(0)).unwrap();
    assert_eq!(left, "new");
}

#[test]
fn rust_jobs_parse_standalone_implies_oracle_and_sessions() {
    let all = JobSet { scryfall: true, oracle: true, sessions: true };
    assert_eq!(JobSet::parse("scryfall", true), all, "Node-free image (Dockerfile.rust) runs every job");
    assert_eq!(JobSet::parse("scryfall", false), JobSet { scryfall: true, oracle: false, sessions: false });
    assert_eq!(JobSet::parse("oracle, sessions", false), JobSet { scryfall: false, oracle: true, sessions: true });
    assert_eq!(JobSet::parse("all", false), all);
    assert!(!JobSet::parse("", true).any());
}
