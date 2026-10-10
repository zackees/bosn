//! The crate's integration tests, linked as one binary (bosn#503).
//!
//! Each former `tests/<name>.rs` target is a module here, so an edit relinks
//! one test binary instead of one per file. Run one module with
//! `cargo test -p bosn-core --test integration -- <module>::`.

mod compose;
mod config;
mod domain;
mod manifest;
mod setup;
