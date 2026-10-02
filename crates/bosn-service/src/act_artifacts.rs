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
fn proxy_environment(values: impl IntoIterator<Item = (OsString, OsString)>) -> io::Result<()> {
    if values.into_iter().any(|(key, value)| {
        !value.is_empty()
            && ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"]
                .iter()
                .any(|name| key.to_string_lossy().eq_ignore_ascii_case(name))
    }) {
        return Err(refused(
            "Act acquisition unavailable with ambient proxy configuration",
        ));
    }
    Ok(())
}
struct Budget {
    deadline: Instant,
    cancellation: CancellationToken,
}
impl Budget {
    fn remaining(&self) -> io::Result<Duration> {
        if self.cancellation.is_cancelled() {
            return Err(refused("Act acquisition cancelled"));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(refused("Act acquisition deadline exceeded"));
        }
        Ok(remaining)
    }
}
type Pending<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;
trait Body: Send {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> Pending<'a, usize>;
}
impl Body for http::Response {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> Pending<'a, usize> {
        Box::pin(self.read(buffer))
    }
}
struct Response {
    status: u16,
    location: Option<Vec<u8>>,
    length: Option<Vec<u8>>,
    encoding: Option<Vec<u8>>,
    body: Box<dyn Body>,
}
trait Transport: Sync {
    fn request<'a>(
        &'a self,
        url: &'a str,
        auth: Option<&'a str>,
        ceiling: u64,
        remaining: Duration,
    ) -> Pending<'a, Response>;
}
struct PublisherHttp;
impl Transport for PublisherHttp {
    fn request<'a>(
        &'a self,
        url: &'a str,
        auth: Option<&'a str>,
        ceiling: u64,
        remaining: Duration,
    ) -> Pending<'a, Response> {
        Box::pin(async move {
            // Recheck before every request; do not alter process environment.
            proxy_environment(std::env::vars_os())?;
            let client = http::Client::new(http::Limits {
                max_redirects: 0,
                max_request_bytes: 0,
                max_url_bytes: 8192,
                max_body_bytes: ceiling,
                max_header_bytes: 16 << 10,
                max_header_count: 64,
                connect_timeout: remaining.min(Duration::from_secs(10)),
                total_timeout: remaining.min(Duration::from_secs(120)),
                read_timeout: remaining.min(Duration::from_secs(15)),
            })?;
            let mut headers = vec![
                (
                    "Accept",
                    "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json",
                ),
                ("Accept-Encoding", "identity"),
            ];
            if let Some(auth) = auth {
                headers.push(("Authorization", auth));
            }
            let response = client
                .execute(http::Request {
                    method: http::Method::Get,
                    url,
                    headers: &headers,
                    body: &[],
                })
                .await?;
            Ok(Response {
                status: response.status(),
                location: response.header("location").map(<[u8]>::to_vec),
                length: response.header("content-length").map(<[u8]>::to_vec),
                encoding: response.header("content-encoding").map(<[u8]>::to_vec),
                body: Box::new(response),
            })
        })
    }
}
fn publisher_host(url: &str) -> io::Result<&str> {
    if url.len() > 8192 || url.chars().any(|c| c.is_control() || c == '\\') || url.contains('#') {
        return Err(refused("artifact publisher URL is invalid"));
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| refused("artifact publisher requires HTTPS"))?;
    let host = rest.split('/').next().unwrap_or("");
    if !matches!(
        host,
        "ghcr.io"
            | "github.com"
            | "release-assets.githubusercontent.com"
            | "pkg-containers.githubusercontent.com"
    ) {
        return Err(refused("artifact redirect publisher is not allowed"));
    }
    Ok(host)
}
async fn open_response(
    transport: &impl Transport,
    initial: &str,
    token: Option<&str>,
    ceiling: u64,
    budget: &Budget,
) -> io::Result<Response> {
    let mut url = initial.to_owned();
    let mut token_allowed = true;
    for hop in 0..=3 {
        let host = publisher_host(&url)?;
        let auth = (token_allowed && host == "ghcr.io")
            .then_some(token)
            .flatten();
        let remaining = budget.remaining()?;
        let response = async_engine::cancellable(
            &budget.cancellation,
            async_engine::timeout(remaining, transport.request(&url, auth, ceiling, remaining)),
        )
        .await
        .map_err(|_| refused("Act acquisition cancelled"))?
        .map_err(|_| refused("artifact HTTP deadline exceeded"))??;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            if hop == 3 {
                return Err(refused("artifact redirect ceiling exceeded"));
            }
            let next = std::str::from_utf8(
                response
                    .location
                    .as_deref()
                    .ok_or_else(|| refused("artifact redirect has no location"))?,
            )
            .map_err(|_| refused("artifact redirect location is invalid"))?;
            let next_host = publisher_host(next)?;
            // Even a later redirect back to ghcr cannot regain authorization.
            if next_host != host {
                token_allowed = false;
            }
            url = next.into();
            continue;
        }
        if response.status != 200 {
            return Err(refused("artifact publisher returned unsuccessful status"));
        }
        if response
            .encoding
            .as_deref()
            .is_some_and(|v| v != b"identity")
        {
            return Err(refused("artifact HTTP content encoding is not identity"));
        }
        if let Some(length) = response.length.as_ref() {
            let n = std::str::from_utf8(length)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| refused("artifact HTTP length is invalid"))?;
            if n > ceiling {
                return Err(refused("artifact HTTP advertised size exceeds ceiling"));
            }
        }
        return Ok(response);
    }
    Err(refused("artifact redirect ceiling exceeded"))
}
async fn copy_body(
    mut response: Response,
    output: &mut impl Write,
    expected_size: Option<u64>,
    ceiling: u64,
    expected_digest: Option<&str>,
    budget: &Budget,
) -> io::Result<u64> {
    let advertised = response
        .length
        .as_deref()
        .map(|v| {
            std::str::from_utf8(v)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| refused("artifact HTTP length is invalid"))
        })
        .transpose()?;
    let mut bytes = 0u64;
    let mut hasher = Sha256Hasher::new();
    let mut buffer = vec![0u8; 65536];
    loop {
        let count = async_engine::cancellable(
            &budget.cancellation,
            async_engine::timeout(budget.remaining()?, response.body.read(&mut buffer)),
        )
        .await
        .map_err(|_| refused("Act acquisition cancelled"))?
        .map_err(|_| refused("artifact body deadline exceeded"))??;
        if count > buffer.len() {
            return Err(refused("artifact transport returned invalid chunk size"));
        }
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .filter(|n| *n <= ceiling)
            .ok_or_else(|| refused("artifact body exceeds ceiling"))?;
        if expected_size.is_some_and(|n| bytes > n) {
            return Err(refused("artifact body exceeds descriptor size"));
        }
        hasher.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    budget.remaining()?;
    if advertised.is_some_and(|n| bytes != n) {
        return Err(refused("artifact HTTP body length mismatch"));
    }
    if expected_size.is_some_and(|n| bytes != n) {
        return Err(refused("artifact body descriptor size mismatch"));
    }
    if expected_digest.is_some_and(|d| format!("sha256:{}", hasher.finalize()) != d) {
        return Err(refused("artifact body digest mismatch"));
    }
    Ok(bytes)
}
fn private_cache(root: &Path) -> io::Result<u32> {
    if !root.is_absolute() {
        return Err(refused("artifact cache must be absolute"));
    }
    let user = ipc::current_user_id()?
        .parse::<u32>()
        .map_err(|_| refused("artifact cache user identity is invalid"))?;
    let mut current = PathBuf::new();
    let components = root.components().collect::<Vec<_>>();
    for (index, part) in components.iter().enumerate() {
        if !matches!(part, Component::RootDir | Component::Normal(_)) {
            return Err(refused("artifact cache path contains unsafe components"));
        }
        current.push(part.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                if metadata.uid() != 0 && metadata.uid() != user {
                    return Err(refused("artifact cache ancestor belongs to another user"));
                }
                // A sticky shared /tmp parent is allowed; replaceable parents
                // without that protection are not an ownership boundary.
                if metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
                    return Err(refused(
                        "artifact cache ancestor is writable by another user",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && index + 1 == components.len() => {
                ipc::ensure_owner_private_directory(&current)?;
            }
            _ => return Err(refused("artifact cache ancestor is not a real directory")),
        }
    }
    let metadata = std::fs::symlink_metadata(root)?;
    if metadata.uid().to_string() != ipc::current_user_id()? || metadata.mode() & 0o077 != 0 {
        return Err(refused("artifact cache is not owner private"));
    }
    Ok(metadata.uid())
}
fn open_private(path: &Path, owner: u32) -> io::Result<std::fs::File> {
    // Linux UAPI O_NOFOLLOW and O_NONBLOCK: reject a link or FIFO without
    // following it or waiting for a writer, then validate the opened handle.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x20000 | 0x800)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(refused(
            "artifact cache entry is not a private regular file",
        ));
    }
    Ok(file)
}
fn cache_lock(path: &Path, owner: u32) -> io::Result<fs::OwnedFileLock> {
    let file = match fs::create_private_file(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0x20000 | 0x800)
            .open(path)?,
        Err(e) => return Err(e),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(refused(
            "artifact acquisition lock is not private and empty",
        ));
    }
    fs::try_lock_exclusive_owned(file)
}
fn cache_usage(root: &Path, owner: u32) -> io::Result<u64> {
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        count += 1;
        if count > 128 {
            return Err(refused("artifact cache entry ceiling exceeded"));
        }
        let name = entry.file_name();
        if name == ".acquire.lock" {
            continue;
        }
        // Crash leftovers are retained and stop acquisition; do not remove a
        // path whose current producer ownership cannot be established.
        if entry.file_type()?.is_dir() {
            return Err(refused(
                "artifact cache contains retained staging requiring recovery",
            ));
        }
        if name.to_str().is_none_or(|s| {
            s.len() != 64
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(refused("artifact cache contains an unexpected entry"));
        }
        let file = open_private(&entry.path(), owner)?;
        total = total
            .checked_add(file.metadata()?.len())
            .filter(|n| *n <= AGGREGATE)
            .ok_or_else(|| refused("artifact cache disk ceiling exceeded"))?;
    }
    Ok(total)
}
fn verified_file(
    path: &Path,
    owner: u32,
    pin: &str,
    size: Option<u64>,
    ceiling: u64,
) -> io::Result<Vec<u8>> {
    let mut file = open_private(path, owner)?;
    let metadata = file.metadata()?;
    if metadata.len() > ceiling || size.is_some_and(|size| size != metadata.len()) {
        return Err(refused("cached artifact size mismatch"));
    }
    // Reserve the verified descriptor length exactly; do not let geometric
    // Vec growth double the advertised aggregate in-memory byte ceiling.
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| refused("cached artifact cannot fit memory"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0u8; 65536];
    let mut hasher = Sha256Hasher::new();
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if bytes.len().checked_add(count).is_none_or(|n| n > capacity) {
            return Err(refused("cached artifact grew beyond descriptor length"));
        }
        hasher.update(&buffer[..count]);
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() != capacity
        || size.is_some_and(|size| size != bytes.len() as u64)
        || format!("sha256:{}", hasher.finalize()) != pin
    {
        return Err(refused("cached artifact digest mismatch"));
    }
    Ok(bytes)
}
struct ArtifactSpec<'a> {
    pin: &'a str,
    size: Option<u64>,
    ceiling: u64,
}
async fn fetch_cached(
    transport: &impl Transport,
    root: &Path,
    owner: u32,
    url: &str,
    token: Option<&str>,
    artifact: ArtifactSpec<'_>,
    budget: &Budget,
) -> io::Result<PathBuf> {
    let ArtifactSpec { pin, size, ceiling } = artifact;
    let path = root.join(digest_name(pin)?);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            verified_file(&path, owner, pin, size, ceiling)?;
            budget.remaining()?;
            return Ok(path);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if cache_usage(root, owner)?
        .checked_add(size.unwrap_or(ceiling))
        .is_none_or(|n| n > AGGREGATE)
    {
        return Err(refused("artifact cache has insufficient bounded space"));
    }
    let staging = fs::TemporaryDirectory::in_directory(root, "stage-")?;
    let pending = staging.path().join("download");
    let mut file = fs::create_private_file(&pending)?;
    let response = open_response(transport, url, token, ceiling, budget).await?;
    copy_body(response, &mut file, size, ceiling, Some(pin), budget).await?;
    file.sync_all()?;
    drop(file);
    budget.remaining()?;
    // Atomic create-only publication: hard_link never replaces an existing
    // name, even if a noncooperating process races the advisory lock.
    std::fs::hard_link(&pending, &path)?;
    std::fs::File::open(root)?.sync_all()?;
    staging.close()?;
    Ok(path)
}
struct Pins<'a> {
    manifest: &'a str,
    config: &'a str,
    archive: &'a str,
    binary: &'a str,
    binary_size: u64,
}
const PINS: Pins<'static> = Pins {
    manifest: RUNNER,
    config: CONFIG,
    archive: ACT_ARCHIVE,
    binary: ACT_BINARY,
    binary_size: 21098680,
};

/// Acquire the fixed graph into a server-selected private cache. Do not expose
/// the cache path or private transport seam as client-supplied authority.
pub async fn acquire_pinned_act_artifacts(
    root: &Path,
    options: ActArtifactAcquisitionOptions,
    cancellation: &CancellationToken,
) -> io::Result<VerifiedActArtifacts> {
    if !cfg!(target_arch = "x86_64") {
        return Err(refused("Act acquisition supports native Linux x86_64 only"));
    }
    proxy_environment(std::env::vars_os())?;
    acquire(&PublisherHttp, root, options, cancellation, &PINS).await
}
async fn acquire(
    transport: &impl Transport,
    root: &Path,
    options: ActArtifactAcquisitionOptions,
    cancellation: &CancellationToken,
    pins: &Pins<'_>,
) -> io::Result<VerifiedActArtifacts> {
    if options.deadline.is_zero() || options.deadline > Duration::from_secs(600) {
        return Err(refused("Act acquisition deadline is outside bounds"));
    }
    let budget = Budget {
        deadline: Instant::now()
            .checked_add(options.deadline)
            .ok_or_else(|| refused("Act acquisition deadline is invalid"))?,
        cancellation: cancellation.clone(),
    };
    budget.remaining()?;
    let owner = private_cache(root)?;
    let lock_path = root.join(".acquire.lock");
    match std::fs::symlink_metadata(&lock_path) {
        Ok(_) => {
            open_private(&lock_path, owner)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let lock = cache_lock(&lock_path, owner)?;
    cache_usage(root, owner)?;
    // Anonymous, public pull capability. Never consult Docker config, netrc,
    // GitHub tokens or caller-controlled challenge realms/scopes.
    let manifest_cached = root.join(digest_name(pins.manifest)?);
    let needs_registry = match std::fs::symlink_metadata(&manifest_cached) {
        Ok(_) => {
            let bytes = verified_file(&manifest_cached, owner, pins.manifest, None, JSON)?;
            let value: Value = serde_json::from_slice(&bytes)?;
            let layers = value["layers"]
                .as_array()
                .filter(|v| v.len() <= 32)
                .ok_or_else(|| refused("cached runner layer graph is invalid"))?;
            let mut missing = false;
            for pin in std::iter::once(pins.config)
                .chain(layers.iter().map(|v| v["digest"].as_str().unwrap_or("")))
            {
                let path = root.join(digest_name(pin)?);
                match std::fs::symlink_metadata(path) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => missing = true,
                    Err(e) => return Err(e),
                }
            }
            missing
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(e) => return Err(e),
    };
    let bearer = if needs_registry {
        let response = open_response(transport, TOKEN_URL, None, TOKEN, &budget).await?;
        let mut token = Vec::new();
        copy_body(response, &mut token, None, TOKEN, None, &budget).await?;
        let token: Value = serde_json::from_slice(&token)?;
        let token = token
            .get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && t.len() <= 8192 && t.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or_else(|| refused("anonymous registry token is invalid"))?;
        format!("Bearer {token}")
    } else {
        String::new()
    };
    let manifest_path = fetch_cached(
        transport,
        root,
        owner,
        &format!("{MANIFEST_URL}{}", pins.manifest),
        Some(&bearer),
        ArtifactSpec {
            pin: pins.manifest,
            size: None,
            ceiling: JSON,
        },
        &budget,
    )
    .await?;
    let manifest = verified_file(&manifest_path, owner, pins.manifest, None, JSON)?;
    let value: Value = serde_json::from_slice(&manifest)?;
    let config_size = value["config"]["size"]
        .as_u64()
        .filter(|s| *s > 0 && *s <= JSON)
        .ok_or_else(|| refused("runner config size is invalid"))?;
    if value["config"]["digest"] != pins.config {
        return Err(refused("runner manifest config pin mismatch"));
    }
    let config_path = fetch_cached(
        transport,
        root,
        owner,
        &format!("{BLOB_URL}{}", pins.config),
        Some(&bearer),
        ArtifactSpec {
            pin: pins.config,
            size: Some(config_size),
            ceiling: JSON,
        },
        &budget,
    )
    .await?;
    let config = verified_file(&config_path, owner, pins.config, Some(config_size), JSON)?;
    let binary_path = root.join(digest_name(pins.binary)?);
    match std::fs::symlink_metadata(&binary_path) {
        Ok(_) => {
            verified_file(
                &binary_path,
                owner,
                pins.binary,
                Some(pins.binary_size),
                BINARY,
            )?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let archive = fetch_cached(
                transport,
                root,
                owner,
                ACT_URL,
                None,
                ArtifactSpec {
                    pin: pins.archive,
                    size: None,
                    ceiling: RELEASE,
                },
                &budget,
            )
            .await?;
            if cache_usage(root, owner)?
                .checked_add(BINARY)
                .is_none_or(|n| n > AGGREGATE)
            {
                return Err(refused("artifact extraction exceeds cache disk ceiling"));
            }
            let staging = fs::TemporaryDirectory::in_directory(root, "stage-")?;
            let destination = staging.path().join("act");
            // The worker owns staging until it finishes. A dropped caller cannot
            // delete files under a running extractor or publish its output.
            let (staging, result) = async_engine::launch_blocking(move || {
                let result = kernal_api::archive::extract_member(
                    &archive,
                    "act",
                    &destination,
                    kernal_api::archive::ArchiveFormat::TarGzip,
                    kernal_api::archive::ExtractionLimits {
                        max_input_bytes: RELEASE,
                        max_output_bytes: BINARY,
                        max_entry_bytes: BINARY,
                        max_entries: 16,
                        max_metadata_bytes: 64 << 10,
                        max_path_bytes: 256,
                        max_link_steps: 16,
                        ..Default::default()
                    },
                );
                (staging, result)
            })
            .await
            .map_err(|_| refused("Act extraction worker failed"))?;
            result?;
            budget.remaining()?;
            // The archive backend preserves mode, so enforce private cache mode
            // before applying the regular-file identity/hash boundary.
            use std::os::unix::fs::PermissionsExt;
            let extracted = staging.path().join("act");
            std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o600))?;
            verified_file(
                &extracted,
                owner,
                pins.binary,
                Some(pins.binary_size),
                BINARY,
            )?;
            std::fs::hard_link(&extracted, &binary_path)?;
            std::fs::File::open(root)?.sync_all()?;
            staging.close()?;
        }
        Err(e) => return Err(e),
    }
    let binary = verified_file(
        &binary_path,
        owner,
        pins.binary,
        Some(pins.binary_size),
        BINARY,
    )?;
    let package = package_act_image(
        &manifest,
        pins.manifest,
        &config,
        pins.config,
        &binary,
        pins.binary,
        "0.2.88",
    )
    .map_err(|_| refused("verified Act image graph is invalid"))?;
    drop(binary);
    if package.base_layers.len() > 32 {
        return Err(refused("runner layer count exceeds ceiling"));
    }
    let mut descriptors = BTreeMap::new();
    let mut total = 0u64;
    for layer in &package.base_layers {
        digest_name(&layer.digest)?;
        if let Some(previous) = descriptors.insert(layer.digest.clone(), layer.size) {
            if previous != layer.size {
                return Err(refused("runner duplicate layer descriptors conflict"));
            }
        } else {
            total = total
                .checked_add(layer.size)
                .filter(|n| *n <= AGGREGATE)
                .ok_or_else(|| refused("runner aggregate layer bytes exceed ceiling"))?;
        }
    }
    for (pin, size) in &descriptors {
        fetch_cached(
            transport,
            root,
            owner,
            &format!("{BLOB_URL}{pin}"),
            Some(&bearer),
            ArtifactSpec {
                pin,
                size: Some(*size),
                ceiling: *size,
            },
            &budget,
        )
        .await?;
    }
    let mut blobs = BTreeMap::new();
    for (pin, size) in descriptors {
        budget.remaining()?;
        let bytes = verified_file(
            &root.join(digest_name(&pin)?),
            owner,
            &pin,
            Some(size),
            size,
        )?;
        blobs.insert(pin, bytes);
    }
    budget.remaining()?;
    drop(lock);
    let receipt = ActArtifactAcquisitionReceipt {
        schema_version: 1,
        act_version: "0.2.88".into(),
        runner_manifest_digest: pins.manifest.into(),
        runner_config_digest: pins.config.into(),
        act_archive_digest: pins.archive.into(),
        act_binary_digest: pins.binary.into(),
        unique_layer_bytes: total,
        unique_layer_count: blobs.len(),
    };
    Ok(VerifiedActArtifacts {
        package,
        blobs,
        receipt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct Reply {
        status: u16,
        bytes: Vec<u8>,
        location: Option<Vec<u8>>,
        length: Option<Vec<u8>>,
        encoding: Option<Vec<u8>>,
        chunk: usize,
    }
    impl Reply {
        fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                status: 200,
                bytes: bytes.into(),
                location: None,
                length: None,
                encoding: None,
                chunk: 3,
            }
        }
        fn redirect(url: &str) -> Self {
            let mut value = Self::bytes(Vec::new());
            value.status = 302;
            value.location = Some(url.as_bytes().to_vec());
            value
        }
    }
    struct FakeBody {
        reply: Reply,
        offset: usize,
        cancel: Option<Arc<async_engine::CancellationSource>>,
    }
    impl Body for FakeBody {
        fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> Pending<'a, usize> {
            Box::pin(async move {
                let count = (self.reply.bytes.len() - self.offset)
                    .min(self.reply.chunk)
                    .min(buffer.len());
                buffer[..count]
                    .copy_from_slice(&self.reply.bytes[self.offset..self.offset + count]);
                self.offset += count;
                if let Some(cancel) = self.cancel.take() {
                    cancel.cancel();
                }
                Ok(count)
            })
        }
    }
    #[derive(Default)]
    struct FakeTransport {
        replies: Mutex<BTreeMap<String, Reply>>,
        calls: Mutex<Vec<(String, Option<String>)>>,
        cancel: Option<Arc<async_engine::CancellationSource>>,
    }
    impl Transport for FakeTransport {
        fn request<'a>(
            &'a self,
            url: &'a str,
            auth: Option<&'a str>,
            _ceiling: u64,
            _remaining: Duration,
        ) -> Pending<'a, Response> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((url.into(), auth.map(str::to_owned)));
                let reply = self
                    .replies
                    .lock()
                    .unwrap()
                    .get(url)
                    .cloned()
                    .ok_or_else(|| refused("unexpected fixture network request"))?;
                Ok(Response {
                    status: reply.status,
                    location: reply.location.clone(),
                    length: reply.length.clone(),
                    encoding: reply.encoding.clone(),
                    body: Box::new(FakeBody {
                        reply,
                        offset: 0,
                        cancel: self.cancel.clone(),
                    }),
                })
            })
        }
    }
    fn runtime() -> async_engine::Runtime {
        async_engine::RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn budget() -> Budget {
        Budget {
            deadline: Instant::now() + Duration::from_secs(10),
            cancellation: async_engine::CancellationSource::new().token(),
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        cache: PathBuf,
        manifest: String,
        config: String,
        archive: String,
        binary: String,
        binary_size: u64,
        layer: String,
        transport: FakeTransport,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cache = dir.path().join("cache");
            let source = dir.path().join("source");
            std::fs::create_dir(&source).unwrap();
            let binary = b"pinned synthetic Act binary bytes";
            std::fs::write(source.join("act"), binary).unwrap();
            std::fs::set_permissions(source.join("act"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            let archive_file = dir.path().join("act.tar.gz");
            assert!(
                std::process::Command::new("tar")
                    .args([
                        "--format=ustar",
                        "--mtime=@0",
                        "--owner=0",
                        "--group=0",
                        "-czf"
                    ])
                    .arg(&archive_file)
                    .arg("-C")
                    .arg(&source)
                    .arg("act")
                    .status()
                    .unwrap()
                    .success()
            );
            let archive = std::fs::read(archive_file).unwrap();
            let layer = b"verified base layer bytes";
            let layer_pin = digest(layer);
            let config=serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{"Volumes":{}},"rootfs":{"type":"layers","diff_ids":[layer_pin]}})).unwrap();
            let config_pin = digest(&config);
            let manifest=serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_pin,"size":config.len()},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":layer_pin,"size":layer.len()}]})).unwrap();
            let manifest_pin = digest(&manifest);
            let transport = FakeTransport::default();
            transport.replies.lock().unwrap().extend([
                (
                    TOKEN_URL.into(),
                    Reply::bytes(br#"{"token":"anonymous-fixture-pull"}"#.to_vec()),
                ),
                (
                    format!("{MANIFEST_URL}{manifest_pin}"),
                    Reply::bytes(manifest),
                ),
                (format!("{BLOB_URL}{config_pin}"), Reply::bytes(config)),
                (
                    format!("{BLOB_URL}{layer_pin}"),
                    Reply::bytes(layer.to_vec()),
                ),
                (
                    ACT_URL.into(),
                    Reply::redirect("https://release-assets.githubusercontent.com/fixture"),
                ),
                (
                    "https://release-assets.githubusercontent.com/fixture".into(),
                    Reply::bytes(archive.clone()),
                ),
            ]);
            Self {
                dir,
                cache,
                manifest: manifest_pin,
                config: config_pin,
                archive: digest(&archive),
                binary: digest(binary),
                binary_size: binary.len() as u64,
                layer: layer_pin,
                transport,
            }
        }
        fn pins(&self) -> Pins<'_> {
            Pins {
                manifest: &self.manifest,
                config: &self.config,
                archive: &self.archive,
                binary: &self.binary,
                binary_size: self.binary_size,
            }
        }
    }
    #[test]
    fn full_acquisition_streams_verified_graph_and_rehashes_offline_cache() {
        let fixture = Fixture::new();
        let cancellation = async_engine::CancellationSource::new();
        runtime().run(async {
            let artifacts = acquire(
                &fixture.transport,
                &fixture.cache,
                Default::default(),
                &cancellation.token(),
                &fixture.pins(),
            )
            .await
            .unwrap();
            assert_eq!(artifacts.package().runner_manifest_digest, fixture.manifest);
            assert_eq!(artifacts.package().runner_config_digest, fixture.config);
            assert_eq!(artifacts.package().binary_digest, fixture.binary);
            assert_eq!(artifacts.archive_blobs()[0].digest, fixture.layer);
            let mut archive = Vec::new();
            crate::act_archive::write_act_oci_archive(
                artifacts.package(),
                &artifacts.archive_blobs(),
                "bosn-act",
                2 << 30,
                &mut archive,
            )
            .unwrap();
            assert!(archive.windows(10).any(|v| v == b"oci-layout"));
            let calls = fixture.transport.calls.lock().unwrap().clone();
            assert!(
                calls
                    .iter()
                    .filter(|(url, _)| !url.starts_with("https://ghcr.io/"))
                    .all(|(_, auth)| auth.is_none())
            );
            assert!(
                calls
                    .iter()
                    .filter(|(url, _)| url.starts_with(BLOB_URL))
                    .all(|(_, auth)| auth.as_deref() == Some("Bearer anonymous-fixture-pull"))
            );
            fixture.transport.replies.lock().unwrap().clear();
            let count = calls.len();
            let cached = acquire(
                &fixture.transport,
                &fixture.cache,
                Default::default(),
                &cancellation.token(),
                &fixture.pins(),
            )
            .await
            .unwrap();
            assert_eq!(cached.package(), artifacts.package());
            assert_eq!(fixture.transport.calls.lock().unwrap().len(), count);
            let layer = fixture.cache.join(digest_name(&fixture.layer).unwrap());
            let corrupt = vec![b'!'; std::fs::metadata(&layer).unwrap().len() as usize];
            std::fs::write(&layer, &corrupt).unwrap();
            assert!(
                acquire(
                    &fixture.transport,
                    &fixture.cache,
                    Default::default(),
                    &cancellation.token(),
                    &fixture.pins()
                )
                .await
                .is_err()
            );
            assert_eq!(
                fixture.transport.calls.lock().unwrap().len(),
                count,
                "corrupt cache cannot fall back to network"
            );
            assert_eq!(std::fs::read(layer).unwrap(), corrupt);
        });
    }
    #[test]
    fn redirects_are_bounded_and_never_restore_registry_authorization() {
        let transport = FakeTransport::default();
        transport.replies.lock().unwrap().extend([
            (
                "https://ghcr.io/first".into(),
                Reply::redirect("https://pkg-containers.githubusercontent.com/second"),
            ),
            (
                "https://pkg-containers.githubusercontent.com/second".into(),
                Reply::redirect("https://ghcr.io/final"),
            ),
            ("https://ghcr.io/final".into(), Reply::bytes(b"ok".to_vec())),
        ]);
        runtime().run(async {
            let response = open_response(
                &transport,
                "https://ghcr.io/first",
                Some("Bearer owned-public-token"),
                2,
                &budget(),
            )
            .await
            .unwrap();
            let mut bytes = Vec::new();
            copy_body(response, &mut bytes, Some(2), 2, None, &budget())
                .await
                .unwrap();
            assert_eq!(bytes, b"ok");
        });
        let calls = transport.calls.lock().unwrap();
        assert!(calls[0].1.is_some());
        assert!(calls[1].1.is_none());
        assert!(calls[2].1.is_none());
        drop(calls);
        for bad in [
            "http://ghcr.io/insecure",
            "https://evil.invalid/blob",
            "https://ghcr.io@evil.invalid/blob",
            "https://ghcr.io:443/blob",
            "https://github.com/blob#fragment",
            "https://github.com/\\evil",
        ] {
            assert!(publisher_host(bad).is_err(), "{bad}");
        }
        transport.replies.lock().unwrap().insert(
            "https://ghcr.io/loop".into(),
            Reply::redirect("https://ghcr.io/loop"),
        );
        runtime().run(async {
            assert!(
                open_response(&transport, "https://ghcr.io/loop", None, 1, &budget())
                    .await
                    .is_err()
            );
        });
        assert_eq!(
            transport
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(url, _)| url.ends_with("/loop"))
                .count(),
            4
        );
    }
    #[test]
    fn body_size_hash_length_and_cancellation_refuse_publication() {
        for mode in ["hash", "over", "short", "length", "cancel"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("cache");
            let owner = private_cache(&root).unwrap();
            let transport = FakeTransport::default();
            let mut reply = Reply::bytes(b"abc".to_vec());
            let source = Arc::new(async_engine::CancellationSource::new());
            let pin = if mode == "hash" {
                digest(b"xyz")
            } else {
                digest(b"abc")
            };
            let (size, ceiling) = match mode {
                "over" => (None, 2),
                "short" => (Some(4), 4),
                _ => (Some(3), 3),
            };
            if mode == "length" {
                reply.length = Some(b"2".to_vec());
            }
            transport
                .replies
                .lock()
                .unwrap()
                .insert("https://github.com/test".into(), reply);
            let transport = FakeTransport {
                cancel: if mode == "cancel" {
                    Some(source.clone())
                } else {
                    None
                },
                ..transport
            };
            let budget = Budget {
                deadline: Instant::now() + Duration::from_secs(10),
                cancellation: source.token(),
            };
            runtime().run(async {
                assert!(
                    fetch_cached(
                        &transport,
                        &root,
                        owner,
                        "https://github.com/test",
                        None,
                        ArtifactSpec {
                            pin: &pin,
                            size,
                            ceiling
                        },
                        &budget
                    )
                    .await
                    .is_err(),
                    "{mode}"
                );
            });
            assert!(!root.join(digest_name(&pin).unwrap()).exists());
            assert_eq!(
                std::fs::read_dir(&root).unwrap().count(),
                0,
                "staging cleanup after {mode}"
            );
        }
    }
    #[test]
    fn cache_ownership_and_exclusive_lock_refuse_foreign_paths() {
        let fixture = Fixture::new();
        let owner = private_cache(&fixture.cache).unwrap();
        let file = fixture.dir.path().join("foreign");
        std::fs::write(&file, b"foreign bytes").unwrap();
        let link = fixture.cache.join(digest_name(&fixture.manifest).unwrap());
        symlink(&file, &link).unwrap();
        assert!(verified_file(&link, owner, &fixture.manifest, None, JSON).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"foreign bytes");
        let alias = fixture.dir.path().join("alias");
        symlink(&fixture.cache, &alias).unwrap();
        assert!(private_cache(&alias).is_err());
        let parent_alias = fixture.dir.path().join("parent-alias");
        symlink(fixture.dir.path(), &parent_alias).unwrap();
        assert!(private_cache(&parent_alias.join("new-cache")).is_err());
        let lock = fs::try_lock_exclusive_owned(
            fs::open_lock_file(&fixture.cache.join(".acquire.lock")).unwrap(),
        )
        .unwrap();
        let cancellation = async_engine::CancellationSource::new();
        runtime().run(async {
            assert!(
                acquire(
                    &fixture.transport,
                    &fixture.cache,
                    Default::default(),
                    &cancellation.token(),
                    &fixture.pins()
                )
                .await
                .is_err()
            );
        });
        assert!(fixture.transport.calls.lock().unwrap().is_empty());
        drop(lock);
    }
    #[test]
    fn proxy_and_publisher_failures_do_not_expose_ambient_credentials() {
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "hTtPs_PrOxY",
            "ALL_PROXY",
            "all_proxy",
        ] {
            let failure =
                proxy_environment([(name.into(), "https://user:secret@proxy.invalid".into())])
                    .unwrap_err();
            assert!(!failure.to_string().contains("secret"));
        }
        proxy_environment([
            ("NO_PROXY".into(), "localhost".into()),
            ("HTTP_PROXY".into(), "".into()),
            ("GITHUB_TOKEN".into(), "secret".into()),
        ])
        .unwrap();
        let transport = FakeTransport::default();
        let mut reply = Reply::bytes(Vec::new());
        reply.status = 401;
        transport
            .replies
            .lock()
            .unwrap()
            .insert(TOKEN_URL.into(), reply);
        runtime().run(async {
            assert!(
                open_response(&transport, TOKEN_URL, None, TOKEN, &budget())
                    .await
                    .is_err()
            );
        });
        assert!(transport.calls.lock().unwrap()[0].1.is_none());
    }
    #[test]
    fn verified_archive_still_refuses_duplicate_link_traversal_and_bomb() {
        for mode in ["duplicate", "symlink", "traversal", "oversized"] {
            let mut fixture = Fixture::new();
            let source = fixture.dir.path().join("source");
            if mode == "symlink" {
                std::fs::rename(source.join("act"), source.join("original")).unwrap();
                symlink("../foreign", source.join("act")).unwrap();
            }
            if mode == "oversized" {
                std::fs::File::create(source.join("act"))
                    .unwrap()
                    .set_len(BINARY + 1)
                    .unwrap();
            }
            let archive_file = fixture.dir.path().join("unsafe.tar.gz");
            let mut command = std::process::Command::new("tar");
            command
                .args([
                    "--format=ustar",
                    "--mtime=@0",
                    "--owner=0",
                    "--group=0",
                    "-czf",
                ])
                .arg(&archive_file)
                .arg("-C")
                .arg(&source);
            if mode == "traversal" {
                command.arg("--transform=s,^act$,../escape,");
            }
            command.arg("act");
            if mode == "duplicate" {
                command.arg("act");
            }
            assert!(command.status().unwrap().success());
            let archive = std::fs::read(archive_file).unwrap();
            fixture.archive = digest(&archive);
            fixture.transport.replies.lock().unwrap().insert(
                "https://release-assets.githubusercontent.com/fixture".into(),
                Reply::bytes(archive),
            );
            let cancellation = async_engine::CancellationSource::new();
            runtime().run(async {
                assert!(
                    acquire(
                        &fixture.transport,
                        &fixture.cache,
                        Default::default(),
                        &cancellation.token(),
                        &fixture.pins()
                    )
                    .await
                    .is_err(),
                    "{mode}"
                );
            });
            assert!(
                !fixture
                    .cache
                    .join(digest_name(&fixture.binary).unwrap())
                    .exists(),
                "{mode}"
            );
            assert!(!fixture.dir.path().join("escape").exists());
            assert!(
                std::fs::read_dir(&fixture.cache).unwrap().all(|e| !e
                    .unwrap()
                    .file_type()
                    .unwrap()
                    .is_dir()),
                "extractor joined before staging cleanup"
            );
            assert!(
                !fixture
                    .transport
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(url, _)| url.ends_with(&fixture.layer)),
                "invalid archive cannot fetch runner layers"
            );
        }
    }
    struct BlockedBody;
    impl Body for BlockedBody {
        fn read<'a>(&'a mut self, _buffer: &'a mut [u8]) -> Pending<'a, usize> {
            Box::pin(std::future::pending())
        }
    }
    struct BlockedTransport;
    impl Transport for BlockedTransport {
        fn request<'a>(
            &'a self,
            _url: &'a str,
            _auth: Option<&'a str>,
            _ceiling: u64,
            _remaining: Duration,
        ) -> Pending<'a, Response> {
            Box::pin(std::future::pending())
        }
    }
    #[test]
    fn cancellation_interrupts_blocked_request_and_body_and_deadline_refuses() {
        runtime().run(async {
            for phase in ["request", "body"] {
                let source = async_engine::CancellationSource::new();
                let budget = Budget {
                    deadline: Instant::now() + Duration::from_secs(10),
                    cancellation: source.token(),
                };
                let cancel = async_engine::launch(async move {
                    async_engine::sleep(Duration::from_millis(20)).await;
                    source.cancel();
                });
                let mut output = Vec::new();
                let operation = async {
                    if phase == "request" {
                        open_response(&BlockedTransport, ACT_URL, None, 8, &budget)
                            .await
                            .map(|_| ())
                    } else {
                        copy_body(
                            Response {
                                status: 200,
                                location: None,
                                length: None,
                                encoding: None,
                                body: Box::new(BlockedBody),
                            },
                            &mut output,
                            None,
                            8,
                            None,
                            &budget,
                        )
                        .await
                        .map(|_| ())
                    }
                };
                let error = async_engine::timeout(Duration::from_secs(2), operation)
                    .await
                    .expect("token must interrupt blocked HTTP before overall deadline")
                    .unwrap_err();
                assert!(error.to_string().contains("cancelled"));
                assert!(output.is_empty());
                cancel.await.unwrap();
            }
            let source = async_engine::CancellationSource::new();
            let budget = Budget {
                deadline: Instant::now() + Duration::from_millis(20),
                cancellation: source.token(),
            };
            let error = open_response(&BlockedTransport, ACT_URL, None, 8, &budget)
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("deadline"));
        });
    }
    #[test]
    fn aggregate_descriptor_ceiling_refuses_before_blob_download_or_allocation() {
        let mut fixture = Fixture::new();
        let mut manifest: Value = serde_json::from_slice(
            &fixture.transport.replies.lock().unwrap()
                [&format!("{MANIFEST_URL}{}", fixture.manifest)]
                .bytes,
        )
        .unwrap();
        manifest["layers"][0]["size"] = json!(AGGREGATE + 1);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fixture.manifest = digest(&bytes);
        fixture.transport.replies.lock().unwrap().insert(
            format!("{MANIFEST_URL}{}", fixture.manifest),
            Reply::bytes(bytes),
        );
        let cancellation = async_engine::CancellationSource::new();
        runtime().run(async {
            let result = acquire(
                &fixture.transport,
                &fixture.cache,
                Default::default(),
                &cancellation.token(),
                &fixture.pins(),
            )
            .await;
            let failure = result.err().expect("oversized descriptors must refuse");
            assert!(failure.to_string().contains("aggregate layer bytes"));
        });
        assert!(
            !fixture
                .transport
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(url, _)| url.ends_with(&fixture.layer))
        );
        assert!(
            !fixture
                .cache
                .join(digest_name(&fixture.layer).unwrap())
                .exists()
        );
    }
    /// Explicit network evidence only. The operator supplies a fresh owned
    /// cache; downloaded bytes remain there and are never automatically removed.
    #[test]
    #[ignore = "requires explicit private BOSN_ACT_ARTIFACT_PROBE_CACHE and publisher network access"]
    fn real_fixed_publisher_acquisition_and_cached_reread() {
        assert_eq!(
            std::env::consts::ARCH,
            "x86_64",
            "native Linux AMD64 acquisition only"
        );
        let root = PathBuf::from(
            std::env::var_os("BOSN_ACT_ARTIFACT_PROBE_CACHE")
                .expect("explicit private artifact cache path required"),
        );
        assert!(root.is_absolute(), "artifact probe cache must be absolute");
        match std::fs::symlink_metadata(&root) {
            Ok(metadata) => {
                assert!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "fresh real directory required"
                );
                assert_eq!(
                    std::fs::read_dir(&root).unwrap().count(),
                    0,
                    "real acquisition requires a fresh empty cache"
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("artifact cache metadata unavailable: {error}"),
        }
        let cancellation = async_engine::CancellationSource::new();
        runtime().run(async {
            let first=acquire_pinned_act_artifacts(&root,Default::default(),&cancellation.token()).await.expect("fixed publisher acquisition refused");
            assert_eq!(first.package().runner_manifest_digest,RUNNER);
            assert_eq!(first.package().runner_config_digest,CONFIG);
            assert_eq!(first.package().binary_digest,ACT_BINARY);
            assert_eq!(first.receipt().act_archive_digest,ACT_ARCHIVE);
            assert_eq!(first.receipt().unique_layer_bytes,546285712);
            assert_eq!(first.receipt().unique_layer_count,6);
            let package=first.package().clone();
            let receipt=serde_json::to_value(first.receipt()).unwrap();
            let identities=first.archive_blobs().iter().map(|b| (b.digest.to_owned(),digest(b.bytes),b.bytes.len())).collect::<Vec<_>>();
            drop(first); // Do not hold two complete compressed graphs at once.
            let cached=acquire_pinned_act_artifacts(&root,Default::default(),&cancellation.token()).await.expect("verified cached reread refused");
            assert_eq!(cached.package(),&package);
            assert_eq!(serde_json::to_value(cached.receipt()).unwrap(),receipt);
            assert_eq!(cached.archive_blobs().iter().map(|b| (b.digest.to_owned(),digest(b.bytes),b.bytes.len())).collect::<Vec<_>>(),identities);
            let archive_bytes=crate::act_archive::write_act_oci_archive(cached.package(),&cached.archive_blobs(),"bosn-act",2<<30,&mut std::io::sink()).unwrap();
            println!("{}",serde_json::to_string(&json!({"scope":"fixed publisher acquisition and OCI graph validation only","receipt":receipt,"oci_archive_bytes":archive_bytes,"cache_retained":true})).unwrap());
        });
    }
}
