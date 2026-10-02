//! Local CI: `bosn ci` runs a repository's CI workflows on a daemon-owned,
//! isolated engine, with a provider-neutral run model, a machine-wide queue,
//! and bounded, cursor-based logs for agents and humans.
//!
//! Modules, one responsibility each:
//! - [`wire`]: request/record types shared by daemon and clients;
//! - [`reply`]: one typed reply per request, parsed eagerly by clients;
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

#[macro_use]
mod vocabulary {
    /// A closed vocabulary enum: each variant's one word is its wire spelling,
    /// its `as_str`, and what `parse` accepts (the CLI parses eagerly with it).
    macro_rules! vocabulary {
    ($(#[$meta:meta])* $name:ident, $what:literal {
        $($(#[$vmeta:meta])* $variant:ident => $word:literal),+ $(,)?
    }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $word)] $variant),+
        }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $word),+
                }
            }
            pub fn parse(value: &str) -> Result<Self, String> {
                match value {
                    $($word => Ok(Self::$variant),)+
                    _ => Err(format!(
                        "unknown {} {:?} (expected {})",
                        $what,
                        value,
                        [$($word),+].join(", ")
                    )),
                }
            }
        }
    };
}
}

pub mod checkout;
pub mod client;
pub mod config;
pub mod engine;
pub mod events;
pub mod lifecycle;
pub mod limits;
pub mod mcp;
pub mod model;
pub mod pins;
pub mod provider;
pub mod reply;
pub mod report;
pub mod runtime;
pub mod scheduler;
pub mod schema;
pub mod snapshot;
pub mod store;
pub mod ui;
pub mod widget;
pub mod wire;
pub mod workflow;

pub use client::{SubmitOptions, detect_actor, plan, stage_submission};
pub use reply::*;
pub use runtime::CiRuntime;
pub use wire::*;

/// Where clients stage snapshots for the daemon at `state_dir`.
pub fn staging_root(state_dir: &std::path::Path) -> std::path::PathBuf {
    store::Store::new(state_dir).staging_root()
}

#[cfg(test)]
mod tests;
