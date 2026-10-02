//! Local CI: `bosn ci` runs a repository's CI workflows on a daemon-owned,
//! isolated engine, with a provider-neutral run model, a machine-wide queue,
//! and bounded, cursor-based logs for agents and humans.
//!
//! Modules, one responsibility each:
//! - [`wire`]: request/record types shared by daemon and clients;
//! - [`client`]: client-side planning, snapshotting and submission;
//! - [`runtime`]: the daemon's scheduler-driven executor and handlers;
//! - [`store`]: on-disk run records, logs and sources;
//! - [`lifecycle`] / [`engine`]: one run on one isolated engine;
//! - [`model`]: the run tree and the act `--json` parser;
//! - [`provider`], [`scheduler`], [`snapshot`]: pure policy and copying.
//!
//! Trust: requests arrive over the owner-only daemon socket, so the client is
//! the same user. Clients name a staging directory only by UUID; the daemon
//! resolves it under its own state directory.

pub mod client;
pub mod engine;
pub mod lifecycle;
pub mod model;
pub mod provider;
pub mod report;
pub mod runtime;
pub mod scheduler;
pub mod snapshot;
pub mod store;
pub mod wire;

pub use client::{SubmitOptions, detect_actor, plan, stage_submission};
pub use runtime::CiRuntime;
pub use wire::*;

/// Where clients stage snapshots for the daemon at `state_dir`.
pub fn staging_root(state_dir: &std::path::Path) -> std::path::PathBuf {
    store::Store::new(state_dir).staging_root()
}

#[cfg(test)]
mod tests;
