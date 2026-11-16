//! Local Scryfall search engine — pure Rust, memory-resident, no SQLite.
//!
//! Library form of the former `scryfall-local` sidecar. scrybox-server links
//! it and serves the identical router in-process; `src/main.rs` keeps the
//! standalone `scryfall-api` binary (port 8420) for anyone who still runs
//! the engine as its own container.

pub mod api;
pub mod boot;
pub mod eval;
pub mod index;
pub mod prints;
pub mod query;
pub mod store;

pub use boot::{load_index, router, state, BootOpts};
