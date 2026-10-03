//! Explicit, ignored real-Docker probes of the owned engine lifecycle (startup
//! recovery and retained-engine cleanup). Inputs and all evidence are retained.
//! This is implementation evidence, not a public API or fleet/native CI proof.
#![cfg(target_os = "linux")]

use super::*;
use crate::act_engine::{
    ActEngineLimits, VerifiedEngineManifest, create_owned_engine_from_manifest, observe_engine,
    observe_engine_image_from_manifest, remove_owned_engine,
};
use crate::act_registry::{ActRegistryCommand, ActRegistryReply};
use bosn_registry::act::{ActEngineIntent, ActEngineRemovalProof, ActEngineState, ActRunOutcome};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ENGINE: &str = "sha256:6acc6aaf783ac1c1100822e542534c3dab3f1d38782760b0bdcb688280574d9e";
const ENGINE_CONFIG: &str =
    "sha256:8cdb6d492106752d557cda50e628b88e7bb303a7eaea91a10bdf672b95ad4f52";

fn fail(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}
fn at() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}
fn retain(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn bounded_file(path: &Path, ceiling: usize) -> std::io::Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > ceiling as u64 {
        return Err(fail("probe input is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(ceiling as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > ceiling {
        return Err(fail("probe input grew beyond its bound"));
    }
    Ok(bytes)
}
async fn random_uuid() -> std::io::Result<String> {
    let entropy = kernal_api::random::SecureRandom::new(1, Duration::from_secs(5))
        .map_err(|e| fail(e.to_string()))?;
    Ok(uuid(
        &entropy.bytes(16).await.map_err(|e| fail(e.to_string()))?,
    ))
}
fn limits() -> ActEngineLimits {
    ActEngineLimits {
        memory_bytes: 28 << 30,
        storage_bytes: 20 << 30,
        storage: crate::act_engine::EngineStorage::Memory,
        nano_cpus: 2_000_000_000,
        pids: 1024,
    }
}

mod fixture;
use fixture::*;
mod cases;
