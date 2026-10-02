//! Daemon-owned acquisition of one immutable Linux/amd64 Act artifact graph.
//! No client URL, image tag, credential or pin is accepted. This is artifact
//! integrity, not execution authority or native-platform coverage.
//!
//! The current HTTP facade inherits proxy configuration. Acquisition therefore
//! refuses nonempty ambient HTTP_PROXY/HTTPS_PROXY/ALL_PROXY (case insensitive)
//! instead of forwarding ambient proxy credentials or changing daemon globals.
#![cfg(target_os = "linux")]

use crate::{
    act_archive::ActArchiveBlob,
    act_image::{ActImagePackage, package_act_image},
};
use kernal_api::{
    async_engine::{self, CancellationToken},
    hash::Sha256Hasher,
    http,
    platform::{fs, ipc},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    pin::Pin,
    time::{Duration, Instant},
};

const JSON: u64 = 1 << 20;
const TOKEN: u64 = 16 << 10;
const BINARY: u64 = 64 << 20;
const RELEASE: u64 = 16 << 20;
const AGGREGATE: u64 = 1 << 30;
const MANIFEST_URL: &str = "https://ghcr.io/v2/catthehacker/ubuntu/manifests/";
const BLOB_URL: &str = "https://ghcr.io/v2/catthehacker/ubuntu/blobs/";
const TOKEN_URL: &str =
    "https://ghcr.io/token?service=ghcr.io&scope=repository%3Acatthehacker%2Fubuntu%3Apull";
const ACT_URL: &str =
    "https://github.com/nektos/act/releases/download/v0.2.88/act_Linux_x86_64.tar.gz";
const RUNNER: &str = "sha256:be3b065b90a7a029ea30aa8ce897a62bfc8bd4d6698951b2527e1f11ba70cc6c";
const CONFIG: &str = "sha256:0385872e2126185df5bef04f9b47c04d81b59d48b8d98ee95bac0928adc08c85";
const ACT_ARCHIVE: &str = "sha256:1eb9996682dfcc053ac8f3f90f2ec50376f0cdfc229712d82da03d673c63a2b3";
const ACT_BINARY: &str = "sha256:a76aa7627c633f5e9e9b06407d6eb1069213b1ee984599381b84ad4e7bd894f0";

#[derive(Clone, Copy, Debug)]
pub struct ActArtifactAcquisitionOptions {
    /// Network operations are timed and cancellable. Native filesystem and
    /// verified extraction operations are joined; their deadline is checked
    /// before publication, not a claim that kernel filesystem I/O can be killed.
    pub deadline: Duration,
}
impl Default for ActArtifactAcquisitionOptions {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(600),
        }
    }
}
/// Verified bytes for the existing borrowed OCI writer. Compressed base bytes
/// total at most 1 GiB; the package can additionally hold two 64 MiB Act copies
/// plus bounded JSON. Cached file payloads have a separate 1 GiB ceiling
/// (plus filesystem overhead), with at most 128 entries.
pub struct VerifiedActArtifacts {
    package: ActImagePackage,
    blobs: BTreeMap<String, Vec<u8>>,
    receipt: ActArtifactAcquisitionReceipt,
}
/// Integrity provenance only; no successful execution or platform claim.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ActArtifactAcquisitionReceipt {
    pub schema_version: u32,
    pub act_version: String,
    pub runner_manifest_digest: String,
    pub runner_config_digest: String,
    pub act_archive_digest: String,
    pub act_binary_digest: String,
    pub unique_layer_bytes: u64,
    pub unique_layer_count: usize,
}
impl VerifiedActArtifacts {
    pub fn receipt(&self) -> &ActArtifactAcquisitionReceipt {
        &self.receipt
    }
    pub fn package(&self) -> &ActImagePackage {
        &self.package
    }
    pub fn archive_blobs(&self) -> Vec<ActArchiveBlob<'_>> {
        self.blobs
            .iter()
            .map(|(digest, bytes)| ActArchiveBlob { digest, bytes })
            .collect()
    }
}
fn refused(reason: &'static str) -> io::Error {
    io::Error::other(reason)
}
#[cfg(test)]
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(bytes))
}
fn digest_name(value: &str) -> io::Result<&str> {
    value
        .strip_prefix("sha256:")
        .filter(|v| {
            v.len() == 64
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| refused("artifact digest is invalid"))
}

mod acquisition;
pub use acquisition::*;
mod verification;
use verification::*;
#[cfg(test)]
mod tests;
