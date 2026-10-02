//! Daemon-only execution inside an already registered private Act engine.
//! Public RPC must never accept these trusted observations or image bytes.
use crate::{
    RegistryActor,
    act_archive::{ActArchiveBlob, write_act_oci_archive},
    act_image::ActImagePackage,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};
use bosn_engine::{DockerEngine, RunOptions};
use bosn_registry::act::{ActEngineIntent, ActEngineObservation, ActRunOutcome};
use kernal_api::{async_engine, hash::Sha256Hasher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const INPUTS: &str = "/var/lib/docker/bosn-inputs";
fn hash(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(bytes))
}
fn file_hash(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256Hasher::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("sha256:{}", hasher.finalize()))
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |t| t.as_secs_f64())
}
fn error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

mod startup;
pub use startup::*;
mod results;
pub use results::*;
mod image;
pub use image::*;
mod run;
pub use run::*;
#[cfg(test)]
mod tests;
