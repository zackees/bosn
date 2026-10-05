//! Daemon-only Docker observations for the private Act engine boundary.
//! OCI manifest/config identities and Docker's store-dependent image ID are
//! observed separately before trusting container ownership labels.

use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};
use bosn_engine::{DockerEngine, RunOptions};
use bosn_registry::act::{
    ActEngineCreationProfile, ActEngineIntent, ActEngineObservation, ActEngineTmpfsPolicy,
};
use serde_json::Value;
use std::{collections::BTreeMap, fmt};

/// The engine image every new engine is created from: the publisher's
/// `docker:29.7.2` linux/amd64 manifest, with its config, both shipped below.
pub(crate) const ENGINE_MANIFEST: &str =
    "sha256:6acc6aaf783ac1c1100822e542534c3dab3f1d38782760b0bdcb688280574d9e";
const ENGINE_CONFIG: &str =
    "sha256:8cdb6d492106752d557cda50e628b88e7bb303a7eaea91a10bdf672b95ad4f52";

/// The pinned publisher bytes, reached through one base directory.
macro_rules! engine_data {
    ($name:literal) => {
        include_bytes!(concat!("act_engine_data/", $name))
    };
}

/// Publisher bytes shipped with this daemon, independently pinned from Docker
/// inspection and persistent intent data. Unknown historical images remain
/// cleanup-required until their publisher proof is available.
pub(crate) fn bundled_engine_manifests()
-> Result<BTreeMap<String, VerifiedEngineManifest>, ActEngineError> {
    let proof = VerifiedEngineManifest::verify(
        engine_data!("engine-manifest.json"),
        ENGINE_MANIFEST,
        engine_data!("engine-config.json"),
        ENGINE_CONFIG,
    )?;
    Ok(BTreeMap::from([(ENGINE_MANIFEST.into(), proof)]))
}

mod budgets;
pub(crate) use budgets::{CLEANUP_BUDGET, CLEANUP_PASS_BUDGET, removal_reserve};
mod cache;
pub(crate) use cache::*;
mod manifest;
pub use manifest::*;
mod create;
pub use create::*;
mod observe;
pub use observe::*;
#[cfg(test)]
mod tests;

mod source_stop;
pub(crate) use source_stop::{source_retained_by_recovery, stop_source_writers};

mod storage_volume;
pub(crate) use storage_volume::{ensure_storage_volume, remove_storage_volume};
