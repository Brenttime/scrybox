//! One-shot job runner for the DB-diff harness and ops (`scrybox-jobs`).
//!
//!   scrybox-jobs <sets-sync|price-sweep> [--force]
//!
//! Reads SCRYBOX_DB_PATH (the DB copy to work on) and the Scryfall env the
//! server uses (LOCAL_SCRYFALL_*, SCRYFALL_BASE_URL for harnesses,
//! SCRYFALL_GAP_SCALE). Prints a JSON summary line; exit 0 on success.

use rusqlite::Connection;
use scrybox_server::{scryfall::{Scry, ScryEnv}, scry_jobs};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let job = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let force = args.iter().any(|a| a == "--force");
    let db = match std::env::var("SCRYBOX_DB_PATH") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("scrybox-jobs: SCRYBOX_DB_PATH is required");
            std::process::exit(2);
        }
    };
    let conn = match Connection::open(&db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("scrybox-jobs: open {db}: {e}");
            std::process::exit(2);
        }
    };
    let _ = conn.busy_timeout(std::time::Duration::from_millis(10_000));
    let scry = Scry::new(ScryEnv::from_env());
    let outcome = match job {
        "sets-sync" => match scry_jobs::fetch_and_cache_sets(&conn, &scry, force).await {
            Ok(n) => serde_json::json!({ "job": "sets-sync", "sets": n }),
            Err(e) => fail(&format!("sets-sync: {e}")),
        },
        "price-sweep" => {
            match scry_jobs::update_collection_prices(&conn, &scry, force).await {
                Ok(o) => serde_json::json!({
                    "job": "price-sweep",
                    "swept": o.swept,
                    "asked": o.asked,
                    "priced": o.priced,
                    "not_found": o.not_found,
                    "estimates_changed": o.estimates_changed,
                    "floors_filled": o.floors_filled,
                }),
                Err(e) => fail(&format!("price-sweep: {e}")),
            }
        }
        other => {
            eprintln!("scrybox-jobs: unknown job {other:?} (want sets-sync|price-sweep)");
            std::process::exit(2);
        }
    };
    println!("{}", outcome);
}

fn fail(msg: &str) -> ! {
    eprintln!("scrybox-jobs: {msg}");
    std::process::exit(1);
}
