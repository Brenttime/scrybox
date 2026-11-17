//! Hot-swap memory probe: boot an index, then build a second one while the
//! first is still alive (what the daily refresh does), drop the old one,
//! trim, and report RSS / high-water mark at each step.
//!
//!   SCRYFALL_DATA_DIR=... cargo run --release --example swapprobe
use local_scryfall::boot::{self, BootOpts};

fn rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("VmRSS") || l.starts_with("VmHWM"))
        .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(" ")
}

fn trim() {
    extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    unsafe {
        malloc_trim(0);
    }
}

fn main() {
    let o = BootOpts::from_env();
    let a = boot::load_index(&o).expect("cards");
    println!("BOOT  {}", rss());
    let b = boot::load_index(&o).expect("cards");
    println!("BUILT2 (both alive) {}", rss());
    drop(a);
    println!("SWAPPED (no trim) {}", rss());
    trim();
    println!("SWAPPED {}", rss());
    drop(b);
}
