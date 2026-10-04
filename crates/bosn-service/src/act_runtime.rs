//! Daemon-only work on owned Act engines: startup recovery and retirement of
//! engines a previous daemon left, and proving an image loaded into an engine
//! is its pinned one. Public RPC must never accept these trusted observations.
use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};
use bosn_engine::{DockerEngine, RunOptions};
use kernal_api::{async_engine, hash::Sha256Hasher};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(bytes))
}
pub(crate) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |t| t.as_secs_f64())
}
fn error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

mod startup;
pub use startup::*;
mod image;
pub use image::*;
#[cfg(test)]
mod tests;
