//! Lane B unit 4: Rust-owned DB migrations (twin of backend/src/db.js
//! initDb). The three golden files under tests/migrations_golden/ were
//! produced by Node's own initDb (bench/mig-node-baseline.cjs inside the
//! harness container) against (1) an empty file, (2) legacy_schema.sql,
//! (3) legacy2_schema.sql. Rust's migrate() must reproduce them BYTE for
//! BYTE — schema (sqlite_master text incl. trigger bodies), columns, and
//! data. Remaining tests cover the user_version gate, idempotency, the
//! admin-seed/adopt path, and the cardArt.cleanupRetiredData twin.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use scrybox_server::auth_support::verify_password;
use scrybox_server::migrations::{
    has_mtg_printing_check, migrate, open_migration_conn, retired_catalog_name, MigOpts,
    TARGET_USER_VERSION,
};

const LEGACY_SQL: &str = include_str!("./legacy_schema.sql");
const LEGACY2_SQL: &str = include_str!("./legacy2_schema.sql");

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scrybox-mig-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_migrate(db: &Path, opts: &MigOpts) -> scrybox_server::migrations::MigReport {
    let conn = open_migration_conn(db).unwrap();
    migrate(&conn, db, opts).unwrap()
}

fn reset_version(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute_batch("PRAGMA user_version = 0").unwrap();
}

// Canonical dump — must match bench/mig-node-baseline.cjs exactly.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\u{1f}', "\\u001f")
}

fn dump(db: &Path) -> String {
    let conn = Connection::open(db).unwrap();
    let mut out = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap();
        for r in rows {
            let (t, n, sql) = r.unwrap();
            out.push(format!("S|{}|{}|{}", t, n, sql.as_deref().map(esc).unwrap_or_else(|| "\\N".into())));
        }
    }
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    for table in tables {
        let cols: Vec<String> = {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info(\"{}\")", table))
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        out.push(format!("T|{}|{}", table, cols.join(",")));
        if cols.is_empty() {
            continue;
        }
        let casts: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, n)| format!("CAST(\"{}\" AS TEXT) AS \"c{}\"", n, i))
            .collect();
        let sql = format!(
            "SELECT rowid AS __rid, {} FROM \"{}\" ORDER BY rowid",
            casts.join(", "),
            table
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let ncols = cols.len();
        let rows = stmt
            .query_map([], |r| {
                let rid: i64 = r.get::<_, i64>(0)?;
                let mut vals = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    vals.push(r.get::<_, Option<String>>(i + 1)?);
                }
                Ok((rid, vals))
            })
            .unwrap();
        for row in rows {
            let (rid, vals) = row.unwrap();
            let joined = vals
                .iter()
                .map(|v| v.as_deref().map(esc).unwrap_or_else(|| "\\N".into()))
                .collect::<Vec<_>>()
                .join("\u{1f}");
            out.push(format!("R|{}|{}|{}", table, rid, joined));
        }
    }
    out.join("\n") + "\n"
}

fn golden(name: &str) -> String {    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/migrations_golden")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("golden {} missing ({}) — regenerate with bench/mig-node-baseline.cjs", name, e))
}

/// Byte-strict golden filter. SQLite stores CREATE text verbatim, so the
/// exact whitespace inside db.js template literals is externally observable
/// in sqlite_master.sql; the Rust literals mirror the templates byte for
/// byte and the comparison is raw (unit-18 review fix: the old collapse-
/// whitespace norm() let 64/95 schema objects pass with Node-invisible
/// re-indentation). Only the query-planner stats tables are excluded:
/// they are a property of the SQLite build, not the application — the Node
/// container's sqlite3 predates STAT4 (PRAGMA optimize -> sqlite_stat1)
/// while the bundled rusqlite has STAT4 on (-> sqlite_stat4).
fn norm(s: &str) -> String {
    s.lines()
        .filter(|l| {
            !l.starts_with("S|table|sqlite_stat") && !l.starts_with("R|sqlite_stat")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Golden check: if MIG_DUMP_DIR is set, drop norm()-ed actual/want dumps
/// there for offline diffing (keeps cargo output small on failures).
fn check_golden(db: &std::path::Path, name: &str) {
    let actual = norm(&dump(db));
    let want = norm(&golden(name));
    if let Ok(dir) = std::env::var("MIG_DUMP_DIR") {
        let _ = std::fs::write(Path::new(&dir).join(format!("{}.actual", name)), &actual);
        let _ = std::fs::write(Path::new(&dir).join(format!("{}.want", name)), &want);
    }
    assert_eq!(actual, want, "golden {} mismatch", name);
}

fn legacy_db(dir: &Path, name: &str, sql: &str) -> PathBuf {
    let db = dir.join(name);
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(sql).unwrap();
    drop(conn);
    db
}

// ---------------------------------------------------------------------------

#[test]
fn fresh_install_matches_node_baseline() {
    let dir = tmp_dir("fresh");
    let db = dir.join("bindarr.db");
    std::fs::write(&db, []).unwrap();
    let rep = run_migrate(&db, &MigOpts::default());
    assert!(!rep.skipped);
    check_golden(&db, "fresh.txt");
    let conn = Connection::open(&db).unwrap();
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v, TARGET_USER_VERSION);
}

#[test]
fn legacy_upgrade_matches_node_baseline() {
    let dir = tmp_dir("legacy1");
    let db = legacy_db(&dir, "legacy1.db", LEGACY_SQL);
    // Sanity: the fixture really is the legacy shape.
    let conn = Connection::open(&db).unwrap();
    assert!(conn
        .query_row("SELECT sql FROM sqlite_master WHERE name='collection'", [], |r| r.get::<_, String>(0))
        .unwrap()
        .contains("'Reverse Holofoil'"));
    drop(conn);
    run_migrate(&db, &MigOpts::default());
    check_golden(&db, "upgrade1.txt");
}

#[test]
fn transitional_upgrade_matches_node_baseline() {
    let dir = tmp_dir("legacy2");
    let db = legacy_db(&dir, "legacy2.db", LEGACY2_SQL);
    run_migrate(&db, &MigOpts::default());
    check_golden(&db, "upgrade2.txt");
}

#[test]
fn user_version_skips_and_full_reapply_is_noop() {
    let dir = tmp_dir("version");
    let db = dir.join("bindarr.db");
    std::fs::write(&db, []).unwrap();
    run_migrate(&db, &MigOpts::default());
    let first = dump(&db);

    // Second boot: skipped entirely.
    let rep = run_migrate(&db, &MigOpts::default());
    assert!(rep.skipped);

    // Force the whole ladder again over an already-migrated DB: must be a
    // no-op and must preserve data.
    let conn = Connection::open(&db).unwrap();
    conn.execute("INSERT INTO card_cache (id, name) VALUES ('mtg-v', 'Version Card')", []).unwrap();
    conn.execute("INSERT INTO collection (card_id, quantity) VALUES ('mtg-v', 2)", []).unwrap();
    drop(conn);
    reset_version(&db);
    let rep = run_migrate(&db, &MigOpts::default());
    assert!(!rep.skipped);
    let conn = Connection::open(&db).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM collection WHERE card_id='mtg-v' AND quantity=2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "reapply must preserve rows");
    // Remove the seeded rows again before comparing no-op-ness: the inserts
    // (and these deletes) fire the revision-triggers, so the four counters
    // legitimately moved and are stripped below; the seeded rows are not
    // part of "the ladder changed nothing".
    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM collection WHERE card_id='mtg-v'", []).unwrap();
    conn.execute("DELETE FROM card_cache WHERE id='mtg-v'", []).unwrap();
    drop(conn);
    let second = dump(&db);
    let strip = |s: &str| {
        s.lines()
            .filter(|l| {
                !l.starts_with("R|collection_read_revision|")
                    && !l.starts_with("R|collection_snapshot|")
                    && !l.starts_with("R|deck_list_revision|")
                    && !l.starts_with("R|stats_aux_revision|")
                    // PRAGMA optimize legitimately refreshes planner stats
                    // between passes — planner stats, not application data.
                    && !l.starts_with("R|sqlite_stat")
                    && !l.starts_with("S|table|sqlite_stat")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&first), strip(&second));
}

#[test]
fn legacy_reapply_is_noop() {
    let dir = tmp_dir("legacy-reapply");
    let db = legacy_db(&dir, "legacy1.db", LEGACY_SQL);
    run_migrate(&db, &MigOpts::default());
    let first = dump(&db);
    reset_version(&db);
    run_migrate(&db, &MigOpts::default());
    assert_eq!(first, dump(&db));
}

#[test]
fn admin_seed_format_and_adopt_orphans() {
    let dir = tmp_dir("seed");
    let db = dir.join("bindarr.db");
    std::fs::write(&db, []).unwrap();
    let opts = MigOpts { admin_password: Some("hunter2!".into()), ..Default::default() };
    let rep = run_migrate(&db, &opts);
    assert!(rep.seeded_admin);
    let conn = Connection::open(&db).unwrap();
    let (hash, token): (String, String) = conn
        .query_row("SELECT password_hash, share_token FROM users WHERE username='admin'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let parts: Vec<&str> = hash.split(':').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], "210000");
    assert_eq!(parts[1].len(), 32, "16-byte hex salt");
    assert!(parts[1].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(parts[2].len(), 128, "64-byte hex dk");
    assert!(verify_password("hunter2!", &hash));
    assert!(!verify_password("wrong", &hash));
    assert_eq!(token.len(), 32, "share_token = randomHex(16)");

    // No re-seed once users exist, even with a password.
    drop(conn);
    reset_version(&db);
    let rep = run_migrate(&db, &opts);
    assert!(!rep.seeded_admin, "seed is count==0 gated");
}

#[test]
fn gated_boot_still_runs_seed_adopt_and_cleanup() {
    // Node runs the seed check, adoptOrphanRows and cleanupRetiredData on
    // EVERY boot, not just unmigrated ones. The user_version gate must skip
    // the ladder only — if the tail stops running, admin re-seed, orphan
    // adoption and the retired-file sweep silently die after first boot.
    let dir = tmp_dir("gated-tail");
    let models = dir.join("models");
    std::fs::create_dir_all(&models).unwrap();
    let art = dir.join("card-art");
    std::fs::create_dir_all(&art).unwrap();
    let db = dir.join("bindarr.db");
    let opts = MigOpts {
        admin_password: Some("pw-gate".into()),
        model_dir: Some(models.clone()),
        ..Default::default()
    };
    run_migrate(&db, &opts);

    // Post-migration state: admin exists; add an orphan collection row and
    // retired files that a LATER boot must clean up.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO card_cache (id, name) VALUES ('mtg-g1','Gated')", []).unwrap();
        conn.execute(
            "INSERT INTO collection (card_id, quantity) VALUES ('mtg-g1', 1)",
            [],
        )
        .unwrap();
    }
    // Only known retired scan-catalog names are swept (cardArt.js
    // retiredCatalog regex), not arbitrary files.
    std::fs::write(models.join("milo-pokemon-zh.npz"), b"x").unwrap();
    std::fs::write(art.join("zzz_retired.png"), b"art").unwrap();

    let rep = run_migrate(&db, &opts);
    assert!(rep.skipped, "ladder must be gated");
    assert_eq!(rep.removed_models, 1, "gated boot must still sweep retired models");
    assert_eq!(rep.removed_art, 1, "gated boot must still sweep orphan art");
    let (n, uid): (i64, Option<i64>) = {
        let conn = Connection::open(&db).unwrap();
        conn.query_row(
            "SELECT COUNT(*), MAX(user_id) FROM collection WHERE card_id='mtg-g1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(n, 1);
    assert_eq!(uid, Some(1), "gated boot must still adopt orphan rows");

    // Admin re-seed on a gated boot: emptying users must let
    // DEFAULT_ADMIN_PASSWORD recreate admin WITHOUT resetting user_version.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute("DELETE FROM users", []).unwrap();
    }
    let rep = run_migrate(&db, &opts);
    assert!(rep.skipped);
    assert!(rep.seeded_admin, "gated boot must still re-seed admin");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn gated_boot_replays_every_boot_row_statements() {
    // Unit-18 review fix: app_settings seed, the price_history orphan
    // delete and the four revision-counter seeds are NOT no-ops on an
    // already-migrated DB (Node runs all of initDb every boot). A gated
    // (user_version-hit) boot must still reproduce them.
    let dir = tmp_dir("gated-rows");
    let db = dir.join("bindarr.db");
    std::fs::write(&db, []).unwrap();
    run_migrate(&db, &MigOpts::default());
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute("DELETE FROM app_settings WHERE id = 1", []).unwrap();
        for t in [
            "collection_snapshot",
            "deck_list_revision",
            "collection_read_revision",
            "stats_aux_revision",
        ] {
            conn.execute(&format!("DELETE FROM {t} WHERE id = 1"), [])
                .unwrap();
        }
        conn.execute(
            "INSERT INTO price_history (card_id, price) VALUES ('ghost-card', 1.5)",
            [],
        )
        .unwrap();
    }
    let rep = run_migrate(&db, &MigOpts::default());
    assert!(rep.skipped, "must exercise the gated-skip path");
    let conn = Connection::open(&db).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM app_settings WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "app_settings row re-seeded by gated boot");
    for t in [
        "collection_snapshot",
        "deck_list_revision",
        "collection_read_revision",
        "stats_aux_revision",
    ] {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {t} WHERE id = 1"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "{t} seed row restored by gated boot");
    }
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM price_history WHERE card_id = 'ghost-card'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "orphan price point deleted by gated boot");
}

#[test]
fn legacy_pokemon_db_is_renamed_before_open() {
    // db.js resolveDbPath twin (unit-18 review fix): in pair mode the Rust
    // front opens the file first; without the twin Node's module-load
    // rename silently never happens (empty DB where the collection was).
    let dir = tmp_dir("legacyname");
    let legacy = dir.join("pokemon_cards.db");
    std::fs::write(&legacy, b"legacy-bytes").unwrap();
    std::fs::write(dir.join("pokemon_cards.db-wal"), b"wal-bytes").unwrap();
    let target = dir.join("bindarr.db");
    let resolved = scrybox_server::migrations::resolve_legacy_db_path(&target);
    assert_eq!(resolved, target);
    assert!(target.exists() && !legacy.exists(), "main file renamed");
    assert!(!dir.join("pokemon_cards.db-wal").exists(), "wal sidecar moved");
    assert_eq!(std::fs::read(&target).unwrap(), b"legacy-bytes");
    // No overwrite: target already present -> legacy untouched.
    let dir2 = tmp_dir("legacyname2");
    std::fs::write(dir2.join("bindarr.db"), b"real").unwrap();
    std::fs::write(dir2.join("pokemon_cards.db"), b"stale").unwrap();
    let t2 = dir2.join("bindarr.db");
    assert_eq!(scrybox_server::migrations::resolve_legacy_db_path(&t2), t2);
    assert_eq!(std::fs::read(dir2.join("pokemon_cards.db")).unwrap(), b"stale");
    // Custom DB_PATH is the operator's decision: never attract the legacy name.
    let dir3 = tmp_dir("legacyname3");
    std::fs::write(dir3.join("pokemon_cards.db"), b"x").unwrap();
    let custom = dir3.join("mine.db");
    assert_eq!(
        scrybox_server::migrations::resolve_legacy_db_path(&custom),
        custom
    );
    assert!(dir3.join("pokemon_cards.db").exists());
}

#[test]
fn legacy_upgrade_adopts_orphan_collection_rows() {
    let dir = tmp_dir("adopt");
    let db = legacy_db(&dir, "legacy1.db", LEGACY_SQL);
    let conn = Connection::open(&db).unwrap();
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM collection WHERE user_id IS NULL", [], |r| r.get(0))
        .unwrap();
    drop(conn);
    assert!(before >= 1, "fixture must carry orphan rows");
    run_migrate(&db, &MigOpts::default());
    let conn = Connection::open(&db).unwrap();
    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM collection WHERE user_id IS NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, 0);
}

#[test]
fn cleanup_retired_data_twin() {
    let dir = tmp_dir("cleanup");
    let models = dir.join("models");
    std::fs::create_dir_all(&models).unwrap();
    let art = dir.join("card-art");
    std::fs::create_dir_all(&art).unwrap();

    let retired = [
        "milo-pokemon.npz",
        "milo-pokemon.bin",
        "milo-pokemon.json",
        "milo-pokemon-zh.npz",
        "milo-pokemon-.bin",
        "MILO-POKEMON.npz",           // /i
        "milo-pokemon.npz.tmp",
        "milo-pokemon-CN.json.tmp",
    ];
    let keep_models = [
        "milo-pokemonfoo.npz",         // no '-' / '.' boundary
        "milo_pokemon.npz",
        "milo-pokemon.txt",
        "milo-pokemon.npzx",
        "cornelius-mtg.npz",
        "milo-pokemon.npz.tmpx",
    ];
    for f in retired.iter().chain(keep_models.iter()) {
        std::fs::write(models.join(f), b"x").unwrap();
    }
    // directory named like a retired file: isFile() guard must skip it
    std::fs::create_dir(models.join("milo-pokemon-dir.npz")).unwrap();

    let db = dir.join("bindarr.db");
    std::fs::write(&db, []).unwrap();
    let conn = open_migration_conn(&db).unwrap();
    // fresh base tables first, then seed retention rows, then the sweep.
    migrate(&conn, &db, &MigOpts::default()).unwrap();
    drop(conn);
    let conn = Connection::open(&db).unwrap();
    conn.execute("INSERT INTO card_cache (id, name) VALUES ('mtg-keep', 'Keep')", []).unwrap();
    conn.execute("INSERT INTO card_cache (id, name) VALUES ('mtg-coll', 'Coll')", []).unwrap();
    conn.execute("INSERT INTO collection (card_id) VALUES ('mtg-coll')", []).unwrap();
    drop(conn);

    for f in ["mtg-keep.png", "mtg-coll.png", "orphan.png", "ORPHAN2.PNG", "notpng.txt"] {
        std::fs::write(art.join(f), b"png?").unwrap();
    }
    reset_version(&db);
    let opts = MigOpts {
        model_dir: Some(models.clone()),
        user_art_dir: Some(art.clone()),
        ..Default::default()
    };
    let rep = run_migrate(&db, &opts);
    assert_eq!(rep.removed_models, retired.len(), "all retired model names, nothing else");
    for f in retired.iter() {
        assert!(!models.join(f).exists(), "{} must be gone", f);
    }
    for f in keep_models.iter() {
        assert!(models.join(f).exists(), "{} must be kept", f);
    }
    assert!(models.join("milo-pokemon-dir.npz").exists(), "directories are skipped like statSync().isFile()");
    assert_eq!(rep.removed_art, 2, "orphan.png + ORPHAN2.PNG only");
    assert!(art.join("mtg-keep.png").exists());
    assert!(art.join("mtg-coll.png").exists());
    assert!(art.join("notpng.txt").exists());
    assert!(!art.join("orphan.png").exists());
    assert!(!art.join("ORPHAN2.PNG").exists());
}

#[test]
fn printing_check_matcher_vs_js_regex() {
    // Positives: CHECK\s*\(\s*printing\s+IN\s*\(\s*'Normal'\s*,\s*'Holofoil'\s*\)\s*\)/i
    assert!(has_mtg_printing_check("CREATE TABLE t (printing TEXT CHECK(printing IN ('Normal', 'Holofoil')))"));
    assert!(has_mtg_printing_check("CHECK ( PRINTING IN ( 'NORMAL' , 'HOLOFOIL' ) )"));
    assert!(has_mtg_printing_check("blah CHECK\n\t(printing\nIN\n(\n'Normal'\n,\n'Holofoil'\n)\n) trailing"));
    assert!(has_mtg_printing_check("xmycheck(printing in ('normal','holofoil'))y")); // unanchored, no word boundary
    // Negatives
    assert!(!has_mtg_printing_check("CHECK(printing IN ('Normal'))"));
    assert!(!has_mtg_printing_check("CHECK(printing IN ('Normal', 'Reverse Holofoil'))"));
    assert!(!has_mtg_printing_check("CHECK(printing IN ('Normal', 'Holofoil', '1st Edition'))"));
    assert!(!has_mtg_printing_check("CHECK(printing IN ('Normal', 'Holofoil')"));
    assert!(!has_mtg_printing_check("CHECK (printing IN ('Normal', 'Holofoil') x)"));
    assert!(!has_mtg_printing_check("chek(printing in ('normal','holofoil'))"));
    assert!(!has_mtg_printing_check("CHECK(printing IN ('Normal' , 'Holofoil' , ) )"));
}

#[test]
fn retired_catalog_name_vs_js_regex() {
    assert!(retired_catalog_name("milo-pokemon.npz"));
    assert!(retired_catalog_name("milo-pokemon-.bin"));
    assert!(retired_catalog_name("MILO-POKEMON-JA.JSON"));
    assert!(retired_catalog_name("milo-pokemon-any thing.npz.tmp"));
    assert!(!retired_catalog_name("milo-pokemonfoo.npz"));
    assert!(!retired_catalog_name("milo-pokemon"));
    assert!(!retired_catalog_name("milo-pokemon.npzx"));
    assert!(!retired_catalog_name("xmilo-pokemon.npz"));
    assert!(!retired_catalog_name("milo-pokemon.npz.tmpx"));
}

#[test]
fn boot_tail_splits_stacked_collection_copies() {
    // server.js boot twin of splitStackedEntries (utils/collectionHelpers.js),
    // ported lane-B iter-28 (found by bench/manabox-db-diff.py: a qty>1 row
    // survived the Rust boot that Node would have split — unit 7 Node-free
    // installs would drift). main.rs calls this at EVERY startup, outside the
    // migrate()/initDb twin (mirrors server.js 274-277), so the test drives
    // the public entry point directly on a migrated DB.
    let dir = tmp_dir("split");
    let db = dir.join("bindarr.db");
    run_migrate(&db, &MigOpts::default());
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO users (username, password_hash, role, share_token) VALUES ('u','x','member','t1')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO card_cache (id, name) VALUES ('mtg-a','A')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO collection (card_id, quantity, condition, printing, language, purchase_price, user_id, source, added_at)
             VALUES ('mtg-a', 3, 'Near Mint', 'Normal', 'English', 2.5, 1, 'manabox', '2020-01-01 00:00:00')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO collection (card_id, quantity, user_id) VALUES ('mtg-a', 1, 1)", [])
            .unwrap();
        let created = scrybox_server::migrations::split_stacked_entries(&conn).unwrap();
        assert_eq!(created, 2, "quantity 3 must yield two qty-1 siblings");
        let rows: Vec<(i64, Option<String>, Option<String>, Option<f64>)> = conn
            .prepare("SELECT quantity, source, added_at, purchase_price FROM collection ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|(q, ..)| *q == 1));
        // Original keeps source/added_at; siblings are born source NULL with
        // CURRENT_TIMESTAMP added_at — faithful to Node's INSERT column list.
        assert_eq!(rows[0].1.as_deref(), Some("manabox"));
        assert_eq!(rows[0].2.as_deref(), Some("2020-01-01 00:00:00"));
        // ids: original=1, second-seeded=2, then the two siblings (3, 4).
        for sib in &rows[2..4] {
            assert_eq!(sib.1, None);
            assert!(sib.2.as_deref().map(|s| !s.is_empty()).unwrap_or(false));
            assert_eq!(sib.3, Some(2.5)); // purchase_price carried over
        }
        // Idempotent: nothing stacked left, the next boot splits zero.
        let again = scrybox_server::migrations::split_stacked_entries(&conn).unwrap();
        assert_eq!(again, 0);
    }
    // migrate() (the initDb twin) must NOT split — the Node goldens were
    // generated from initDb alone, split included in the boot or not.
    let db2 = dir.join("nogoldenshift.db");
    std::fs::write(&db2, []).unwrap();
    run_migrate(&db2, &MigOpts::default());
    let conn = Connection::open(&db2).unwrap();
    conn.execute("INSERT INTO card_cache (id, name) VALUES ('mtg-b','B')", []).unwrap();
    conn.execute("INSERT INTO collection (card_id, quantity) VALUES ('mtg-b', 3)", []).unwrap();
    drop(conn);
    let rep = run_migrate(&db2, &MigOpts::default());
    assert!(rep.skipped);
    let conn = Connection::open(&db2).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM collection WHERE card_id='mtg-b' AND quantity=3", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "migrate() must leave stacked rows alone (boot step only)");
    let _ = std::fs::remove_dir_all(&dir);
}
