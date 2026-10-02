//! Explicit, ignored real-Docker probe. Inputs and all evidence are retained.
//! This is implementation evidence, not a public API or fleet/native CI proof.
#![cfg(target_os = "linux")]

use super::*;
use crate::act_archive::ActArchiveBlob;
use crate::act_engine::{
    ActEngineLimits, VerifiedEngineManifest, create_owned_engine_from_manifest, observe_engine,
    observe_engine_image_from_manifest, remove_owned_engine,
};
use crate::act_image::{ActBaseLayer, ActImagePackage, package_act_image};
use crate::act_registry::{ActRegistryCommand, ActRegistryReply};
use crate::act_runtime::{ActRuntimeRequest, run_registered_act};
use bosn_registry::act::{
    ActEngineIntent, ActEngineObservation, ActEngineRemovalProof, ActEngineState, ActRunOutcome,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RUNNER: &str = "sha256:be3b065b90a7a029ea30aa8ce897a62bfc8bd4d6698951b2527e1f11ba70cc6c";
const RUNNER_CONFIG: &str =
    "sha256:0385872e2126185df5bef04f9b47c04d81b59d48b8d98ee95bac0928adc08c85";
const ACT_BINARY: &str = "sha256:a76aa7627c633f5e9e9b06407d6eb1069213b1ee984599381b84ad4e7bd894f0";
const ENGINE: &str = "sha256:6acc6aaf783ac1c1100822e542534c3dab3f1d38782760b0bdcb688280574d9e";
const ENGINE_CONFIG: &str =
    "sha256:8cdb6d492106752d557cda50e628b88e7bb303a7eaea91a10bdf672b95ad4f52";
const OUTPUT: usize = 8 << 20;
const MARKER: &[u8] = b"BOSN_REAL_CANCEL_STARTED";

fn fail(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}
fn at() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(bytes))
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
const LAYER_BYTES: u64 = 1 << 30;
fn read_layers(input: &Path, layers: &[ActBaseLayer]) -> std::io::Result<Vec<Vec<u8>>> {
    let total = layers
        .iter()
        .try_fold(0u64, |total, layer| total.checked_add(layer.size))
        .ok_or_else(|| fail("aggregate layer byte ceiling overflow"))?;
    if total > LAYER_BYTES {
        return Err(fail("aggregate layer byte ceiling exceeded"));
    }
    layers
        .iter()
        .map(|layer| {
            let ceiling = usize::try_from(layer.size)
                .map_err(|_| fail("layer size exceeds address space"))?;
            let bytes = bounded_file(
                &input.join("blobs/sha256").join(&layer.digest[7..]),
                ceiling,
            )?;
            if bytes.len() as u64 != layer.size || digest(&bytes) != layer.digest {
                return Err(fail("layer digest or size mismatch"));
            }
            Ok(bytes)
        })
        .collect()
}
struct ProbeObserver {
    stop: CancellationSource,
    task: Option<async_engine::Task<std::io::Result<bool>>>,
}
impl ProbeObserver {
    fn new() -> Self {
        Self {
            stop: CancellationSource::new(),
            task: None,
        }
    }
    async fn stop_and_join(&mut self) -> std::io::Result<bool> {
        self.stop.cancel();
        match self.task.take() {
            Some(task) => task.await.map_err(|e| fail(e.to_string()))?,
            None => Ok(false),
        }
    }
}
impl Drop for ProbeObserver {
    fn drop(&mut self) {
        self.stop.cancel();
    }
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
        nano_cpus: 2_000_000_000,
        pids: 1024,
    }
}

mod diagnostics;
pub(crate) use diagnostics::*;
mod fixture;
use fixture::*;
mod cases;
mod offline;
