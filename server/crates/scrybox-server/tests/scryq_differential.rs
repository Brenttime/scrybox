//! Differential test: local-scryfall's parser + scryq's AST transforms against
//! NODE's own parser + SQL compiler (shared/scryfallQuery.js,
//! backend/src/utils/rawQuerySql.js). Expectations come from running Node
//! over the same fixture DB (`node tests/fixtures/scryq_gen.js`); this test
//! needs no Node at runtime.
//!
//! Per query: syntax-error verdict, analyze() mode/printing, the oracle-tag
//! local-leg gate, toScryfall (full + directive-stripped walk base), and —
//! for every query a SQL leg can answer — the exact owned rows (entry ids,
//! in order) + COUNT of compileCollectionQuery for two users × eligibleOnly,
//! a LIMIT/OFFSET window, and compileRawQuery's card ids for two languages.

use rusqlite::{params_from_iter, Connection};
use scrybox_server::scryq;
use serde_json::Value;

fn db() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(include_str!("fixtures/scryq_fixture.sql")).unwrap();
    c
}

fn ids(c: &Connection, sql: &str, p: &[rusqlite::types::Value]) -> Vec<Value> {
    let mut st = c.prepare(sql).unwrap();
    let mut rows = st.query(params_from_iter(p.iter())).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        let v: rusqlite::types::Value = r.get(0).unwrap();
        out.push(match v {
            rusqlite::types::Value::Integer(i) => Value::from(i),
            rusqlite::types::Value::Text(s) => Value::from(s),
            other => panic!("unexpected id {other:?}"),
        });
    }
    out
}

fn count(c: &Connection, sql: &str, p: &[rusqlite::types::Value]) -> i64 {
    c.query_row(sql, params_from_iter(p.iter()), |r| r.get(0)).unwrap()
}

#[test]
fn scryq_matches_node_on_corpus() {
    let corpus: Vec<Value> = serde_json::from_str(include_str!("fixtures/scryq_expected.json")).unwrap();
    assert!(corpus.len() >= 150, "corpus too small: {}", corpus.len());
    let c = db();
    let mut failures: Vec<String> = Vec::new();
    let mut sql_checked = 0;
    for rec in &corpus {
        let q = rec["q"].as_str().unwrap();
        let mut fail = |what: &str, got: String, want: String| {
            failures.push(format!("{q:?}: {what}: rust={got} node={want}"));
        };
        let parsed = scryq::analyze(q);
        let want_err = rec["error"].as_bool().unwrap();
        if parsed.is_err() != want_err {
            fail("syntax error", format!("{:?}", parsed.as_ref().err()), want_err.to_string());
            continue;
        }
        let Ok(a) = parsed else { continue };
        let mode = if a.local { "local" } else { "catalog" };
        if mode != rec["mode"] {
            fail("mode", mode.into(), rec["mode"].to_string());
        }
        if a.printing != rec["printing"].as_bool().unwrap() {
            fail("printing", a.printing.to_string(), rec["printing"].to_string());
        }
        let sl = scryq::supports_local_collection_query(&a);
        if sl != rec["supportsLocal"].as_bool().unwrap() {
            fail("supportsLocal", sl.to_string(), rec["supportsLocal"].to_string());
        }
        let ts = scryq::to_scryfall(&a.ast);
        if ts != rec["toScryfall"] {
            fail("toScryfall", ts, rec["toScryfall"].to_string());
        }
        let wb = scryq::strip_directives(&a.ast).map(|n| scryq::to_scryfall(&n));
        let want_wb = rec["walkBase"].as_str().map(|s| s.to_string());
        if wb != want_wb {
            fail("walkBase", format!("{wb:?}"), format!("{want_wb:?}"));
        }
        let Some(coll) = rec.get("collection") else { continue };
        sql_checked += 1;
        for user in [1i64, 2] {
            for elig in [false, true] {
                let key = format!("{user}:{}", elig as i32);
                let (sql, p, csql, cp) = scryq::compile_collection_query(&a.ast, user, None, elig).unwrap();
                let rows = ids(&c, &sql, &p);
                let n = count(&c, &csql, &cp);
                if Value::from(rows.clone()) != coll[&key]["rows"] {
                    fail(&format!("collection rows {key}"), format!("{rows:?}"), coll[&key]["rows"].to_string());
                }
                if n != coll[&key]["n"].as_i64().unwrap() {
                    fail(&format!("collection count {key}"), n.to_string(), coll[&key]["n"].to_string());
                }
            }
        }
        let (sql, p, _, _) = scryq::compile_collection_query(&a.ast, 1, Some((3, 2)), false).unwrap();
        let win = ids(&c, &sql, &p);
        if Value::from(win.clone()) != rec["window"] {
            fail("window", format!("{win:?}"), rec["window"].to_string());
        }
        for lang in ["English", "Japanese"] {
            let (sql, p, csql, cp) = scryq::compile_raw_query(&a.ast, lang, 1000, 0).unwrap();
            let rows = ids(&c, &sql, &p);
            let n = count(&c, &csql, &cp);
            if Value::from(rows.clone()) != rec["raw"][lang]["rows"] {
                fail(&format!("raw rows {lang}"), format!("{rows:?}"), rec["raw"][lang]["rows"].to_string());
            }
            if n != rec["raw"][lang]["n"].as_i64().unwrap() {
                fail(&format!("raw count {lang}"), n.to_string(), rec["raw"][lang]["n"].to_string());
            }
        }
    }
    assert!(sql_checked >= 150, "too few SQL-checked queries: {sql_checked}");
    assert!(failures.is_empty(), "{} divergences:\n{}", failures.len(), failures.join("\n"));
}
