//! The crate's integration tests, linked as one binary (bosn#503).
//!
//! Each former `tests/<name>.rs` target is a module here, so an edit relinks
//! one test binary instead of one per file. Run one module with
//! `cargo test -p bosn-registry --test integration -- <module>::`.

mod act_lifecycle;
mod act_spare;
mod common;
mod foundation;
mod registry_gc;
mod registry_v4_import;
