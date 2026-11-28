//! Memory attribution for the local-scryfall index.
//!
//! Counting global allocator: reports live heap bytes and peak during the
//! load, then frees each `Index` field one at a time (`mem::take`) and
//! prints the live-byte delta — an exact per-field attribution.
//!
//!   SCRYFALL_DATA_DIR=... cargo run --release --example memreport
use local_scryfall::boot::{self, BootOpts};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged; the atomics only add bookkeeping and never touch the memory.
unsafe impl GlobalAlloc for Counting {
    // SAFETY: caller upholds GlobalAlloc::alloc's contract, forwarded to System.
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let n = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(n, Relaxed);
        }
        p
    }
    // SAFETY: p/l came from this allocator (hence System), per the GlobalAlloc contract.
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        LIVE.fetch_sub(l.size(), Relaxed);
    }
    // SAFETY: p/l came from this allocator (hence System); n is a valid size per contract.
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = System.realloc(p, l, n);
        if !q.is_null() {
            if n >= l.size() {
                let v = LIVE.fetch_add(n - l.size(), Relaxed) + n - l.size();
                PEAK.fetch_max(v, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - n, Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

fn mb(n: usize) -> f64 {
    n as f64 / 1048576.0
}
fn rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("VmRSS") || l.starts_with("VmHWM"))
        .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let base = LIVE.load(Relaxed);
    let mut ix = boot::load_index(&BootOpts::from_env()).expect("cards");
    // the lazy all-cards side index: force it like a first query would
    let live = LIVE.load(Relaxed) - base;
    println!("LOAD live={:.1}MB peak={:.1}MB {}", mb(live), mb(PEAK.load(Relaxed)), rss());
    macro_rules! take {
        ($($f:ident),*) => {{
            let mut rows: Vec<(&str, usize)> = Vec::new();
            $({
                let b = LIVE.load(Relaxed);
                let v = std::mem::take(&mut ix.$f);
                drop(v);
                rows.push((stringify!($f), b - LIVE.load(Relaxed)));
            })*
            rows
        }};
    }
    extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    // SAFETY: glibc malloc_trim has no preconditions.
    unsafe { malloc_trim(0) };
    println!("TRIM {}", rss());
    if std::env::var("MI").is_ok() {
        extern "C" {
            fn malloc_info(o: i32, f: *mut std::ffi::c_void) -> i32;
            fn fdopen(fd: i32, m: *const i8) -> *mut std::ffi::c_void;
            fn fflush(f: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: fd 2 (stderr) is open; the mode is a NUL-terminated literal;
        // f is only used by malloc_info/fflush and is intentionally not fclose'd.
        unsafe {
            let f = fdopen(2, b"w\0".as_ptr() as *const i8);
            malloc_info(0, f);
            fflush(f);
        }
    }
    {
        let mut seen = std::collections::HashSet::new();
        let mut rep: Vec<(&str, usize)> = Vec::new();
        macro_rules! cf { ($($f:ident),*) => {$({
            let mut b = 0usize;
            for c in &ix.cards { if (c.$f as usize) < ix.strings.len() && seen.insert(c.$f) { b += ix.strings[c.$f as usize].len(); } }
            rep.push((stringify!($f), b));
        })*}}
        macro_rules! vf { ($($f:ident),*) => {$({
            let mut b = 0usize;
            for &o in &ix.$f { if (o as usize) < ix.strings.len() && seen.insert(o) { b += ix.strings[o as usize].len(); } }
            rep.push((stringify!($f), b));
        })*}}
        cf!(id, oracle_id, name, set, set_name, set_type, collector_number, cn_key, type_line, mana_cost, oracle_text, artist, keywords, layout, image_small, lang, flavor, watermark, illustration, ptypes);
        vf!(name_lc, name_text, type_lc, oracle_lc, otext_lc, cost_all, artist_lc, set_name_lc, mana_norm);
        let tot: usize = ix.strings.iter().map(|s| s.len()).sum();
        let claimed: usize = rep.iter().map(|r| r.1).sum();
        rep.push(("(unclaimed)", tot - claimed));
        rep.sort_by(|a, b| b.1.cmp(&a.1));
        println!("STRINGS total {:.1} MB in {} strings", mb(tot), ix.strings.len());
        for (n, b) in rep { if b > 512 * 1024 { println!("   str {:>7.1} MB  {}", mb(b), n); } }
    }
    {
        let n = ix.jsons.len();
        let t = std::time::Instant::now();
        let mut tot = 0usize;
        for k in 0..20000usize { tot += ix.jsons.get((k * 7919) % n).len(); }
        println!("BENCH get(): {:.2} us/doc ({} bytes)", t.elapsed().as_secs_f64() * 1e6 / 20000.0, tot);
        let t = std::time::Instant::now();
        let mut rd = ix.jsons.reader();
        let mut out = String::new();
        for k in 0..20000usize { out.clear(); rd.append_to((k * 7919) % n, &mut out); }
        println!("BENCH reader reuse: {:.2} us/doc", t.elapsed().as_secs_f64() * 1e6 / 20000.0);
    }
    let mut rows = take!(
        jsons, strings, id_map, cards, name_lc, name_text, type_lc, oracle_lc, otext_lc, cost_all,
        artist_lc, set_name_lc, mana_norm, pip, name_tokens, type_tokens, text_tokens, otext_tokens,
        keyword_tokens, keyword_phrase, kw_enum, prefix, color_bits, color_incl, ci_bits, subsets,
        exacts, ci_subsets, ci_exacts, rarity_idx, set_idx, layout_idx, name_tok_list,
        name_tok_buckets, type_tok_list, type_tok_buckets, oracle_stats, oracle_prints, in_sets,
        in_set_types, in_games, in_langs, oracle_lang_idx, in_rarities, settype_idx, border_idx,
        frame_idx, games_idx, stamp_idx, ffx_idx, ptype_idx, lang_idx, block_idx, block_setname,
        group_idx, land_cycles, card_oracle, oracle_id_off, oracle_vis, oracle_ci, oracle_id_vis,
        id_vis_bits, oracle_intern, side, otags, otag_slug, otag_alias, sets, set_detail_json,
        universe
    );
    let rest = LIVE.load(Relaxed) - base;
    drop(ix);
    rows.push(("(other fields)", rest - (LIVE.load(Relaxed) - base)));
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    for (n, b) in rows {
        if b > 256 * 1024 {
            println!("{:>8.1} MB  {}", mb(b), n);
        }
    }
}
