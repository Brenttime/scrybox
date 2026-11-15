//! ONE-PARSER rule: every Scryfall-syntax decision in Scrybox goes through
//! local-scryfall's parser (`local_scryfall::query::parse_syntax`). The
//! hybrid-mode gates in collq.rs (walk_base / local_plan /
//! definitely_invalid) used to run their own token scanners; these pin the
//! verdicts that changed when they moved onto the parser — each one now
//! agrees with Node's shared/scryfallQuery.js.

use scrybox_server::{collq, scryq};

#[test]
fn no_second_tokenizer_in_scrybox() {
    let scryq_src = include_str!("../src/scryq.rs");
    let collq_src = include_str!("../src/collq.rs");
    for (name, src) in [("scryq.rs", scryq_src), ("collq.rs", collq_src)] {
        for needle in ["fn tokenize", "fn read_quoted", "fn read_bare", "fn op_match", "fn op_shape", "fn local_leaf", "struct Parser"] {
            assert!(!src.contains(needle), "{name} still contains `{needle}`");
        }
    }
    assert!(scryq_src.contains("lsq::parse_syntax"), "scryq must parse via local-scryfall");
}

#[test]
fn local_plan_accepts_lorehold_like_node() {
    // Node's COLOR_WORDS has `lorehold`; the old collq table misspelled it
    // `loreheld`, refusing the real word and accepting the typo.
    assert!(collq::local_plan(1, "c:lorehold", false, 1, 60, true, None, false).is_some());
    assert!(collq::local_plan(1, "c:loreheld", false, 1, 60, true, None, false).is_none());
}

#[test]
fn local_plan_accepts_doubled_and_like_node() {
    // parseAnd consumes `and and` between two terms (corpus: `a and and b`).
    assert!(scryq::parse("bolt and and t:elf").is_ok());
    assert!(collq::local_plan(1, "bolt and and t:elf", false, 1, 60, true, None, false).is_some());
}

#[test]
fn walk_base_refuses_queries_node_cannot_parse() {
    // parseColorValue throws on `white-blue` → Node walks the RAW string;
    // the old gate re-serialised it (`is:land` → `t:land`), a walk key Node
    // never sends.
    assert!(scryq::parse("is:land c:white-blue").is_err());
    assert_eq!(collq::walk_base("is:land c:white-blue"), None);
    assert_eq!(collq::walk_base("is:land c:g").as_deref(), Some("t:land c:g"));
}

#[test]
fn definitely_invalid_is_the_parser_verdict() {
    // a quote inside a bare word is a literal character for Node's tokenizer
    assert!(!collq::definitely_invalid("kodama\"s reach"));
    // every QuerySyntaxError, not just the paren/quote subset
    assert!(collq::definitely_invalid("c:zz"));
    assert!(collq::definitely_invalid("(r:c"));
    assert!(collq::definitely_invalid("\"unterminated"));
    assert!(!collq::definitely_invalid("t:elf c:g"));
}
