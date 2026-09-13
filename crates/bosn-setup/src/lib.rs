//! Bounded, inert acquisition and durable cache records for Bosn setup documents.
//!
//! This crate deliberately stops at a validated [`bosn_core::SetupDocument`].
//! It does not select a workspace, materialize companion files, invoke Docker,
//! or decide whether a newly refreshed document should be applied.  Remote
//! refresh is explicit and never falls back to a cache; offline reuse is a
//! separate, explicit policy.

use std::{
    future::Future,
    io::{self, Write as _},
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use bosn_core::{
    MAX_SETUP_DOCUMENT_BYTES, SETUP_DOCUMENT_VERSION, SetupConfigLocator, SetupDocument,
    parse_and_translate_compose_yaml, parse_setup_config_locator, parse_setup_document_toml,
};
use kernal_api::{
    hash::sha256_bytes,
    http,
    platform::{fs, ipc},
};

mod materialize;
pub use materialize::{
    MAX_ASSET_RECEIPT_BYTES, MaterializedAsset, MaterializedSetupPlan, MaterializedSetupSource,
    SetupAssetStore, SetupMaterializeError,
};

mod plan;
pub use plan::{
    SetupPlan, SetupPlanAppSource, SetupPlanError, SetupPlanRequest, plan_setup,
    plan_setup_with_transport,
};

mod prepare;
pub use prepare::{
    PreparedImage, PreparedImageKind, SetupImageCommand, SetupImageEngine, SetupPrepareError,
    prepare_setup_image,
};

mod ensure;
pub use ensure::{
    SetupEnsureCommand, SetupEnsureEngine, SetupEnsureError, SetupEnsureMount,
    SetupEnsureObservedContainer, SetupEnsureRequest, SetupEnsureResponse, SetupEnsureResult,
    adopt_setup_app, ensure_setup_app,
};

mod task;
pub use task::{
    SetupTaskCommand, SetupTaskEngine, SetupTaskError, SetupTaskMount, SetupTaskRequest,
    SetupTaskResult, execute_setup_task,
};

/// The current durable cache-record wire version.  It is intentionally
/// independent of the setup-document schema version.
pub const CACHE_RECORD_VERSION: u16 = 1;
/// A cache record contains the source document as well as bounded provenance.
pub const MAX_CACHE_RECORD_BYTES: usize = MAX_SETUP_DOCUMENT_BYTES + 32 * 1024;
/// Do not let corrupt metadata make an offline read allocate arbitrarily.
pub const MAX_PROVENANCE_BYTES: usize = 8 * 1024;

const CACHE_MAGIC: &[u8; 11] = b"BOSN-SETUP\0";
const CACHE_FILE_PREFIX: &str = "setup-v1-";
const CACHE_FILE_SUFFIX: &str = ".record";
const TEMPORARY_ATTEMPTS: u8 = 32;

/// Explicit source-selection policy.  `OnlineRefresh` never treats a cache
/// record as a fallback: an unavailable or malformed remote source fails
/// closed instead of silently applying a prior document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupAcquirePolicy {
    /// Use only an existing, validated cache record.  Neither a local path nor
    /// a remote URL is read.
    OfflineCacheOnly,
    /// Read a local file or make an HTTPS request, validate the new bytes, and
    /// atomically replace the cache record after validation.
    OnlineRefresh,
}

/// Source type captured in cache provenance without exposing the raw bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupSourceKind {
    LocalFile,
    Https,
}

/// Provenance returned with a validated document and stored beside its bytes.
/// Locator strings are redacted before persistence and diagnostics never echo
/// an unredacted caller locator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupProvenance {
    pub requested_locator: String,
    pub resolved_locator: Option<String>,
    pub content_sha256: String,
    pub schema_version: u64,
    pub fetched_at_unix_seconds: i64,
    pub source_kind: SetupSourceKind,
}

/// A validated setup document plus the immutable receipt for its exact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSetupDocument {
    pub document: SetupDocument,
    pub provenance: SetupProvenance,
}

/// A bounded HTTPS result supplied by a transport implementation.
///
/// `resolved_locator` is optional because kernel HTTP intentionally does not
/// expose a backend URL.  The default kernel transport disables redirects, so
/// its final locator is the requested locator.  Custom transports may report a
/// final HTTPS URL, which is validated before it can enter provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteSetupResponse {
    pub bytes: Vec<u8>,
    pub resolved_locator: Option<String>,
}

/// Redacted failure information from a remote transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteTransportError {
    Unavailable,
    Status(u16),
    Rejected,
}

/// A testable HTTPS boundary.  Production uses [`KernalHttpTransport`], whose
/// only network implementation is `kernal-api`'s bounded HTTP facade.
pub trait SetupRemoteTransport {
    type FetchFuture<'a>: Future<Output = Result<RemoteSetupResponse, RemoteTransportError>>
        + Send
        + 'a
    where
        Self: 'a;

    /// The implementation must reject body data above `max_bytes` rather than
    /// returning a truncated representation.
    fn fetch<'a>(&'a self, locator: &'a str, max_bytes: usize) -> Self::FetchFuture<'a>;
}

/// Production transport configured for a one-file setup document.
#[derive(Clone)]
pub struct KernalHttpTransport {
    client: http::Client,
}

impl KernalHttpTransport {
    /// Create an HTTPS transport with finite deadlines, a one-mebibyte body
    /// cap, and redirects disabled.  Redirect behavior is intentionally not a
    /// hidden configuration mechanism for a durable setup receipt.
    pub fn new() -> Result<Self, SetupAcquireError> {
        let limits = http::Limits {
            max_redirects: 0,
            max_request_bytes: 0,
            max_url_bytes: MAX_PROVENANCE_BYTES,
            max_body_bytes: MAX_SETUP_DOCUMENT_BYTES as u64,
            max_header_bytes: 16 * 1024,
            max_header_count: 64,
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(10),
        };
        Ok(Self {
            client: http::Client::new(limits).map_err(|_| SetupAcquireError::TransportRejected)?,
        })
    }
}

impl SetupRemoteTransport for KernalHttpTransport {
    type FetchFuture<'a> = Pin<
        Box<dyn Future<Output = Result<RemoteSetupResponse, RemoteTransportError>> + Send + 'a>,
    >;

    fn fetch<'a>(&'a self, locator: &'a str, _max_bytes: usize) -> Self::FetchFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .get(locator)
                .await
                .map_err(|_| RemoteTransportError::Unavailable)?;
            if response.status() != 200 {
                return Err(RemoteTransportError::Status(response.status()));
            }
            let bytes = response
                .into_bytes()
                .await
                .map_err(|_| RemoteTransportError::Rejected)?;
            Ok(RemoteSetupResponse {
                bytes,
                resolved_locator: Some(locator.into()),
            })
        })
    }
}

/// Why setup acquisition stopped.  Display text deliberately does not include
/// caller-controlled local paths, URLs, response bodies, or cache bytes.
#[derive(Debug)]
pub enum SetupAcquireError {
    InvalidLocator,
    OfflineCacheUnavailable,
    CacheCorrupt,
    InputTooLarge,
    InputNotUtf8,
    DocumentInvalid,
    ResolvedLocatorInvalid,
    TransportUnavailable,
    TransportStatus(u16),
    TransportRejected,
    Filesystem(io::Error),
    BlockingTask,
}

impl std::fmt::Display for SetupAcquireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLocator => formatter.write_str("setup config locator is invalid"),
            Self::OfflineCacheUnavailable => {
                formatter.write_str("setup config cache is unavailable offline")
            }
            Self::CacheCorrupt => formatter.write_str("setup config cache record is corrupt"),
            Self::InputTooLarge => formatter.write_str("setup config input exceeds its byte limit"),
            Self::InputNotUtf8 => formatter.write_str("setup config input is not UTF-8"),
            Self::DocumentInvalid => formatter.write_str("setup config document is invalid"),
            Self::ResolvedLocatorInvalid => {
                formatter.write_str("setup config resolved locator is invalid")
            }
            Self::TransportUnavailable => {
                formatter.write_str("setup config HTTPS transport is unavailable")
            }
            Self::TransportStatus(status) => write!(
                formatter,
                "setup config HTTPS request returned status {status}"
            ),
            Self::TransportRejected => {
                formatter.write_str("setup config HTTPS response was rejected")
            }
            Self::Filesystem(_) => {
                formatter.write_str("setup config cache filesystem operation failed")
            }
            Self::BlockingTask => {
                formatter.write_str("setup config blocking operation did not complete")
            }
        }
    }
}
impl std::error::Error for SetupAcquireError {}
impl From<io::Error> for SetupAcquireError {
    fn from(error: io::Error) -> Self {
        Self::Filesystem(error)
    }
}

/// An owner-private directory containing only setup cache records.  Callers
/// pass a Bosn state directory explicitly; this constructor creates and
/// hardens both it and the `setup-cache` child through kernal-api.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupCache {
    directory: PathBuf,
}

impl SetupCache {
    /// Use `<state_dir>/setup-cache`, creating both private directories through
    /// the kernel IPC/filesystem boundary.
    pub fn under_state_dir(state_dir: impl AsRef<Path>) -> Result<Self, SetupAcquireError> {
        let state_dir = state_dir.as_ref();
        ipc::ensure_owner_private_directory(state_dir)?;
        let directory = state_dir.join("setup-cache");
        ipc::ensure_owner_private_directory(&directory)?;
        Ok(Self { directory })
    }

    /// The explicit private directory holding durable records.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn record_path(&self, raw_locator: &str) -> PathBuf {
        let key = sha256_bytes(raw_locator.as_bytes()).to_hex();
        self.directory
            .join(format!("{CACHE_FILE_PREFIX}{key}{CACHE_FILE_SUFFIX}"))
    }

    fn read(&self, raw_locator: &str) -> Result<ResolvedSetupDocument, SetupAcquireError> {
        let path = self.record_path(raw_locator);
        let bytes = match fs::read_private_regular_file_bounded(&path, MAX_CACHE_RECORD_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(SetupAcquireError::OfflineCacheUnavailable);
            }
            Err(error) => return Err(SetupAcquireError::Filesystem(error)),
        };
        CachedRecord::decode(&bytes, raw_locator)?.validated(raw_locator)
    }

    fn write(&self, raw_locator: &str, record: &CachedRecord) -> Result<(), SetupAcquireError> {
        let target = self.record_path(raw_locator);
        let bytes = record.encode()?;
        for attempt in 0..TEMPORARY_ATTEMPTS {
            let temporary = self.directory.join(format!(
                ".{}-{attempt}",
                target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("setup.record")
            ));
            let mut file = match fs::create_private_file(&temporary) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(SetupAcquireError::Filesystem(error)),
            };
            if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                return Err(SetupAcquireError::Filesystem(error));
            }
            drop(file);
            fs::replace_file(&temporary, &target)?;
            fs::sync_directory(&self.directory)?;
            return Ok(());
        }
        Err(SetupAcquireError::Filesystem(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "setup cache temporary-name space exhausted",
        )))
    }
}

/// Acquire one inert setup document according to an explicit source policy.
///
/// All filesystem work is placed on kernal-api's blocking lane.  Callers must
/// drive this future on a kernel-owned async runtime when they use
/// [`KernalHttpTransport`].
pub async fn acquire_setup_document<T: SetupRemoteTransport>(
    cache: &SetupCache,
    transport: &T,
    locator: &str,
    policy: SetupAcquirePolicy,
) -> Result<ResolvedSetupDocument, SetupAcquireError> {
    let parsed_locator =
        parse_setup_config_locator(locator).map_err(|_| SetupAcquireError::InvalidLocator)?;
    match policy {
        SetupAcquirePolicy::OfflineCacheOnly => {
            let cache = cache.clone();
            let locator = locator.to_owned();
            kernal_api::async_engine::launch_blocking(move || cache.read(&locator))
                .await
                .map_err(|_| SetupAcquireError::BlockingTask)?
        }
        SetupAcquirePolicy::OnlineRefresh => {
            let (bytes, source_kind, resolved_locator) = match parsed_locator {
                SetupConfigLocator::LocalPath(path) => {
                    let path = PathBuf::from(path);
                    let observation = kernal_api::async_engine::launch_blocking(move || {
                        fs::read_context_regular_file_bounded(&path, MAX_SETUP_DOCUMENT_BYTES)
                    })
                    .await
                    .map_err(|_| SetupAcquireError::BlockingTask)??;
                    (observation.bytes, SetupSourceKind::LocalFile, None)
                }
                SetupConfigLocator::HttpsUrl(_) => {
                    let response = transport
                        .fetch(locator, MAX_SETUP_DOCUMENT_BYTES)
                        .await
                        .map_err(map_transport_error)?;
                    if response.bytes.len() > MAX_SETUP_DOCUMENT_BYTES {
                        return Err(SetupAcquireError::InputTooLarge);
                    }
                    let resolved_locator = response
                        .resolved_locator
                        .map(|value| validate_resolved_locator(&value))
                        .transpose()?;
                    (response.bytes, SetupSourceKind::Https, resolved_locator)
                }
            };
            let record = CachedRecord::from_fresh(locator, bytes, source_kind, resolved_locator)?;
            let validated = record.clone().validated(locator)?;
            let cache = cache.clone();
            let locator = locator.to_owned();
            let record_for_cache = record.clone();
            kernal_api::async_engine::launch_blocking(move || {
                cache.write(&locator, &record_for_cache)
            })
            .await
            .map_err(|_| SetupAcquireError::BlockingTask)??;
            Ok(validated)
        }
    }
}

fn map_transport_error(error: RemoteTransportError) -> SetupAcquireError {
    match error {
        RemoteTransportError::Unavailable => SetupAcquireError::TransportUnavailable,
        RemoteTransportError::Status(status) => SetupAcquireError::TransportStatus(status),
        RemoteTransportError::Rejected => SetupAcquireError::TransportRejected,
    }
}

fn validate_resolved_locator(value: &str) -> Result<String, SetupAcquireError> {
    match parse_setup_config_locator(value)
        .map_err(|_| SetupAcquireError::ResolvedLocatorInvalid)?
    {
        SetupConfigLocator::HttpsUrl(value) => Ok(redact_locator(&value)),
        SetupConfigLocator::LocalPath(_) => Err(SetupAcquireError::ResolvedLocatorInvalid),
    }
}

fn redact_locator(locator: &str) -> String {
    // Core already rejects URL userinfo.  Preserve an inert local path exactly,
    // while making future provenance robust against accidental query secrets.
    let Some((prefix, query)) = locator.split_once('?') else {
        return locator.into();
    };
    let mut redacted = String::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if !redacted.is_empty() {
            redacted.push('&');
        }
        redacted.push_str(key);
        if !value.is_empty() {
            redacted.push('=');
            let lower = key.to_ascii_lowercase();
            if [
                "token",
                "secret",
                "password",
                "apikey",
                "api_key",
                "auth",
                "credential",
            ]
            .iter()
            .any(|needle| lower.contains(needle))
            {
                redacted.push_str("[redacted]");
            } else {
                redacted.push_str(value);
            }
        }
    }
    format!("{prefix}?{redacted}")
}

/// Source syntax is selected once from the validated locator, never by trying
/// one parser after another.  This preserves legacy TOML behavior for every
/// locator other than the explicitly documented YAML suffixes, including
/// extensionless local paths and HTTPS URLs.
fn parse_setup_document(raw_locator: &str, text: &str) -> Result<SetupDocument, SetupAcquireError> {
    match setup_document_syntax(raw_locator)? {
        SetupDocumentSyntax::Toml => {
            parse_setup_document_toml(text).map_err(|_| SetupAcquireError::DocumentInvalid)
        }
        SetupDocumentSyntax::ComposeYaml => parse_and_translate_compose_yaml(text)
            .map(|plan| plan.setup)
            .map_err(|_| SetupAcquireError::DocumentInvalid),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupDocumentSyntax {
    Toml,
    ComposeYaml,
}

fn setup_document_syntax(raw_locator: &str) -> Result<SetupDocumentSyntax, SetupAcquireError> {
    let path = match parse_setup_config_locator(raw_locator)
        .map_err(|_| SetupAcquireError::InvalidLocator)?
    {
        SetupConfigLocator::LocalPath(path) => path,
        SetupConfigLocator::HttpsUrl(url) => url
            .split_once('?')
            .map_or(url.as_str(), |(path, _)| path)
            .into(),
    };
    let lower = path.to_ascii_lowercase();
    Ok(if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        SetupDocumentSyntax::ComposeYaml
    } else {
        SetupDocumentSyntax::Toml
    })
}

#[derive(Clone, Debug)]
struct CachedRecord {
    locator_key: [u8; 32],
    requested_locator: String,
    resolved_locator: Option<String>,
    content_sha256: [u8; 32],
    schema_version: u64,
    fetched_at_unix_seconds: i64,
    source_kind: SetupSourceKind,
    bytes: Vec<u8>,
}

impl CachedRecord {
    fn from_fresh(
        raw_locator: &str,
        bytes: Vec<u8>,
        source_kind: SetupSourceKind,
        resolved_locator: Option<String>,
    ) -> Result<Self, SetupAcquireError> {
        if bytes.len() > MAX_SETUP_DOCUMENT_BYTES {
            return Err(SetupAcquireError::InputTooLarge);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| SetupAcquireError::InputNotUtf8)?;
        let document = parse_setup_document(raw_locator, text)?;
        let locator_key = *sha256_bytes(raw_locator.as_bytes()).as_bytes();
        Ok(Self {
            locator_key,
            requested_locator: redact_locator(raw_locator),
            resolved_locator,
            content_sha256: *sha256_bytes(&bytes).as_bytes(),
            schema_version: document.version,
            fetched_at_unix_seconds: fs::FileTime::now().unix_seconds(),
            source_kind,
            bytes,
        })
    }

    fn validated(self, raw_locator: &str) -> Result<ResolvedSetupDocument, SetupAcquireError> {
        if self.bytes.len() > MAX_SETUP_DOCUMENT_BYTES
            || self.schema_version != SETUP_DOCUMENT_VERSION
        {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        if *sha256_bytes(&self.bytes).as_bytes() != self.content_sha256 {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        let text = std::str::from_utf8(&self.bytes).map_err(|_| SetupAcquireError::CacheCorrupt)?;
        let document =
            parse_setup_document(raw_locator, text).map_err(|_| SetupAcquireError::CacheCorrupt)?;
        if document.version != self.schema_version {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        Ok(ResolvedSetupDocument {
            document,
            provenance: SetupProvenance {
                requested_locator: self.requested_locator,
                resolved_locator: self.resolved_locator,
                content_sha256: sha256_bytes(&self.bytes).to_hex(),
                schema_version: self.schema_version,
                fetched_at_unix_seconds: self.fetched_at_unix_seconds,
                source_kind: self.source_kind,
            },
        })
    }

    fn encode(&self) -> Result<Vec<u8>, SetupAcquireError> {
        let requested = self.requested_locator.as_bytes();
        let resolved = self.resolved_locator.as_deref().unwrap_or("").as_bytes();
        if requested.len() > MAX_PROVENANCE_BYTES
            || resolved.len() > MAX_PROVENANCE_BYTES
            || self.bytes.len() > MAX_SETUP_DOCUMENT_BYTES
        {
            return Err(SetupAcquireError::InputTooLarge);
        }
        let mut output = Vec::with_capacity(
            CACHE_MAGIC.len()
                + 2
                + 1
                + 32
                + 32
                + 8
                + 8
                + 4 * 3
                + requested.len()
                + resolved.len()
                + self.bytes.len(),
        );
        output.extend_from_slice(CACHE_MAGIC);
        put_u16(&mut output, CACHE_RECORD_VERSION);
        output.push(match self.source_kind {
            SetupSourceKind::LocalFile => 0,
            SetupSourceKind::Https => 1,
        });
        output.push(u8::from(self.resolved_locator.is_some()));
        output.extend_from_slice(&self.locator_key);
        output.extend_from_slice(&self.content_sha256);
        put_u64(&mut output, self.schema_version);
        put_i64(&mut output, self.fetched_at_unix_seconds);
        put_bytes(&mut output, requested)?;
        put_bytes(&mut output, resolved)?;
        put_bytes(&mut output, &self.bytes)?;
        if output.len() > MAX_CACHE_RECORD_BYTES {
            return Err(SetupAcquireError::InputTooLarge);
        }
        Ok(output)
    }

    fn decode(input: &[u8], raw_locator: &str) -> Result<Self, SetupAcquireError> {
        if input.len() > MAX_CACHE_RECORD_BYTES {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        let mut reader = WireReader::new(input);
        if reader.take(CACHE_MAGIC.len())? != CACHE_MAGIC || reader.u16()? != CACHE_RECORD_VERSION {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        let source_kind = match reader.byte()? {
            0 => SetupSourceKind::LocalFile,
            1 => SetupSourceKind::Https,
            _ => return Err(SetupAcquireError::CacheCorrupt),
        };
        let expected_source_kind = match parse_setup_config_locator(raw_locator) {
            Ok(SetupConfigLocator::LocalPath(_)) => SetupSourceKind::LocalFile,
            Ok(SetupConfigLocator::HttpsUrl(_)) => SetupSourceKind::Https,
            Err(_) => return Err(SetupAcquireError::CacheCorrupt),
        };
        if source_kind != expected_source_kind {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        let has_resolved = match reader.byte()? {
            0 => false,
            1 => true,
            _ => return Err(SetupAcquireError::CacheCorrupt),
        };
        let locator_key: [u8; 32] = reader
            .take(32)?
            .try_into()
            .map_err(|_| SetupAcquireError::CacheCorrupt)?;
        if locator_key != *sha256_bytes(raw_locator.as_bytes()).as_bytes() {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        let content_sha256: [u8; 32] = reader
            .take(32)?
            .try_into()
            .map_err(|_| SetupAcquireError::CacheCorrupt)?;
        let schema_version = reader.u64()?;
        let fetched_at_unix_seconds = reader.i64()?;
        let requested_locator = reader.string(MAX_PROVENANCE_BYTES)?;
        let resolved = reader.string(MAX_PROVENANCE_BYTES)?;
        let resolved_locator = has_resolved
            .then_some(resolved)
            .filter(|value| !value.is_empty());
        if has_resolved != resolved_locator.is_some()
            || requested_locator != redact_locator(raw_locator)
            || (source_kind == SetupSourceKind::LocalFile && resolved_locator.is_some())
        {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        if let Some(resolved_locator) = &resolved_locator
            && !matches!(
                validate_resolved_locator(resolved_locator),
                Ok(ref normalized) if normalized == resolved_locator
            )
        {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        let bytes = reader.bytes(MAX_SETUP_DOCUMENT_BYTES)?;
        if !reader.done() {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        Ok(Self {
            locator_key,
            requested_locator,
            resolved_locator,
            content_sha256,
            schema_version,
            fetched_at_unix_seconds,
            source_kind,
            bytes,
        })
    }
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn put_i64(output: &mut Vec<u8>, value: i64) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn put_bytes(output: &mut Vec<u8>, value: &[u8]) -> Result<(), SetupAcquireError> {
    let length = u32::try_from(value.len()).map_err(|_| SetupAcquireError::InputTooLarge)?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(value);
    Ok(())
}

struct WireReader<'a> {
    input: &'a [u8],
    offset: usize,
}
impl<'a> WireReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, offset: 0 }
    }
    fn take(&mut self, length: usize) -> Result<&'a [u8], SetupAcquireError> {
        let end = self
            .offset
            .checked_add(length)
            .filter(|end| *end <= self.input.len())
            .ok_or(SetupAcquireError::CacheCorrupt)?;
        let value = &self.input[self.offset..end];
        self.offset = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8, SetupAcquireError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, SetupAcquireError> {
        Ok(u16::from_le_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| SetupAcquireError::CacheCorrupt)?,
        ))
    }
    fn u64(&mut self) -> Result<u64, SetupAcquireError> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| SetupAcquireError::CacheCorrupt)?,
        ))
    }
    fn i64(&mut self) -> Result<i64, SetupAcquireError> {
        Ok(i64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| SetupAcquireError::CacheCorrupt)?,
        ))
    }
    fn bytes(&mut self, maximum: usize) -> Result<Vec<u8>, SetupAcquireError> {
        let length = u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| SetupAcquireError::CacheCorrupt)?,
        ) as usize;
        if length > maximum {
            return Err(SetupAcquireError::CacheCorrupt);
        }
        Ok(self.take(length)?.to_vec())
    }
    fn string(&mut self, maximum: usize) -> Result<String, SetupAcquireError> {
        String::from_utf8(self.bytes(maximum)?).map_err(|_| SetupAcquireError::CacheCorrupt)
    }
    fn done(&self) -> bool {
        self.offset == self.input.len()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::{Ready, ready},
        sync::Mutex,
    };

    use kernal_api::async_engine::RuntimeBuilder;

    use super::*;

    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[derive(Default)]
    struct ScriptedTransport {
        responses: Mutex<VecDeque<Result<RemoteSetupResponse, RemoteTransportError>>>,
    }
    impl ScriptedTransport {
        fn with(response: Result<RemoteSetupResponse, RemoteTransportError>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from([response])),
            }
        }
        fn push(&self, response: Result<RemoteSetupResponse, RemoteTransportError>) {
            self.responses.lock().unwrap().push_back(response);
        }
    }
    impl SetupRemoteTransport for ScriptedTransport {
        type FetchFuture<'a> = Ready<Result<RemoteSetupResponse, RemoteTransportError>>;

        fn fetch<'a>(&'a self, _locator: &'a str, _max_bytes: usize) -> Self::FetchFuture<'a> {
            ready(
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Err(RemoteTransportError::Unavailable)),
            )
        }
    }

    fn source(digest: &str) -> Vec<u8> {
        format!("version = 1\n[app]\nimage = 'registry.example/demo@sha256:{digest}'\n")
            .into_bytes()
    }
    fn run<T>(future: impl Future<Output = T>) -> T {
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(future)
    }
    fn cache(root: &Path) -> SetupCache {
        SetupCache::under_state_dir(root).unwrap()
    }

    #[test]
    fn local_refresh_is_bounded_cached_and_offline_reuse_is_explicit() {
        let state = tempfile::tempdir().unwrap();
        let input = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input.path(), source(DIGEST_A)).unwrap();
        let locator = input.path().to_string_lossy().into_owned();
        let cache = cache(state.path());
        let transport = ScriptedTransport::default();

        let first = run(acquire_setup_document(
            &cache,
            &transport,
            &locator,
            SetupAcquirePolicy::OnlineRefresh,
        ))
        .unwrap();
        assert_eq!(first.provenance.source_kind, SetupSourceKind::LocalFile);
        assert_eq!(
            first.provenance.content_sha256,
            sha256_bytes(&source(DIGEST_A)).to_hex()
        );

        std::fs::write(input.path(), source(DIGEST_B)).unwrap();
        let offline = run(acquire_setup_document(
            &cache,
            &transport,
            &locator,
            SetupAcquirePolicy::OfflineCacheOnly,
        ))
        .unwrap();
        assert_eq!(offline, first);

        let refreshed = run(acquire_setup_document(
            &cache,
            &transport,
            &locator,
            SetupAcquirePolicy::OnlineRefresh,
        ))
        .unwrap();
        assert_eq!(
            refreshed.provenance.content_sha256,
            sha256_bytes(&source(DIGEST_B)).to_hex()
        );
        assert_ne!(
            refreshed.provenance.content_sha256,
            offline.provenance.content_sha256
        );
    }

    #[test]
    fn remote_refresh_never_silently_falls_back_to_cache() {
        let state = tempfile::tempdir().unwrap();
        let cache = cache(state.path());
        let locator = "https://configs.example/bosn.toml?revision=1";
        let transport = ScriptedTransport::with(Ok(RemoteSetupResponse {
            bytes: source(DIGEST_A),
            resolved_locator: Some(locator.into()),
        }));
        let first = run(acquire_setup_document(
            &cache,
            &transport,
            locator,
            SetupAcquirePolicy::OnlineRefresh,
        ))
        .unwrap();
        assert_eq!(first.provenance.source_kind, SetupSourceKind::Https);
        assert_eq!(first.provenance.resolved_locator.as_deref(), Some(locator));

        transport.push(Err(RemoteTransportError::Unavailable));
        assert!(matches!(
            run(acquire_setup_document(
                &cache,
                &transport,
                locator,
                SetupAcquirePolicy::OnlineRefresh
            )),
            Err(SetupAcquireError::TransportUnavailable)
        ));
        let offline = run(acquire_setup_document(
            &cache,
            &transport,
            locator,
            SetupAcquirePolicy::OfflineCacheOnly,
        ))
        .unwrap();
        assert_eq!(offline, first);
    }

    #[test]
    fn offline_cache_is_checked_for_tampering_and_locator_binding() {
        let state = tempfile::tempdir().unwrap();
        let cache = cache(state.path());
        let locator = "https://configs.example/bosn.toml?revision=1";
        let transport = ScriptedTransport::with(Ok(RemoteSetupResponse {
            bytes: source(DIGEST_A),
            resolved_locator: None,
        }));
        run(acquire_setup_document(
            &cache,
            &transport,
            locator,
            SetupAcquirePolicy::OnlineRefresh,
        ))
        .unwrap();
        std::fs::write(cache.record_path(locator), b"not a setup cache record").unwrap();
        assert!(matches!(
            run(acquire_setup_document(
                &cache,
                &transport,
                locator,
                SetupAcquirePolicy::OfflineCacheOnly
            )),
            Err(SetupAcquireError::CacheCorrupt)
        ));

        let other = "https://configs.example/other.toml";
        assert!(matches!(
            run(acquire_setup_document(
                &cache,
                &transport,
                other,
                SetupAcquirePolicy::OfflineCacheOnly
            )),
            Err(SetupAcquireError::OfflineCacheUnavailable)
        ));
    }

    #[test]
    fn provenance_redacts_query_credentials_and_rejects_bad_final_urls() {
        let state = tempfile::tempdir().unwrap();
        let cache = cache(state.path());
        let locator = "https://configs.example/bosn.toml?token=top-secret&revision=1";
        let transport = ScriptedTransport::with(Ok(RemoteSetupResponse {
            bytes: source(DIGEST_A),
            resolved_locator: Some(locator.into()),
        }));
        let document = run(acquire_setup_document(
            &cache,
            &transport,
            locator,
            SetupAcquirePolicy::OnlineRefresh,
        ))
        .unwrap();
        assert!(!document.provenance.requested_locator.contains("top-secret"));
        assert!(
            document
                .provenance
                .requested_locator
                .contains("token=[redacted]")
        );

        let bad = ScriptedTransport::with(Ok(RemoteSetupResponse {
            bytes: source(DIGEST_B),
            resolved_locator: Some("http://configs.example/bosn.toml".into()),
        }));
        assert!(matches!(
            run(acquire_setup_document(
                &cache,
                &bad,
                "https://configs.example/new.toml",
                SetupAcquirePolicy::OnlineRefresh
            )),
            Err(SetupAcquireError::ResolvedLocatorInvalid)
        ));
    }

    #[test]
    fn oversized_and_non_utf8_remote_documents_fail_before_cache_write() {
        let state = tempfile::tempdir().unwrap();
        let cache = cache(state.path());
        let locator = "https://configs.example/bosn.toml";
        let large = ScriptedTransport::with(Ok(RemoteSetupResponse {
            bytes: vec![b'x'; MAX_SETUP_DOCUMENT_BYTES + 1],
            resolved_locator: None,
        }));
        assert!(matches!(
            run(acquire_setup_document(
                &cache,
                &large,
                locator,
                SetupAcquirePolicy::OnlineRefresh
            )),
            Err(SetupAcquireError::InputTooLarge)
        ));
        assert!(matches!(
            run(acquire_setup_document(
                &cache,
                &large,
                locator,
                SetupAcquirePolicy::OfflineCacheOnly
            )),
            Err(SetupAcquireError::OfflineCacheUnavailable)
        ));

        let non_utf8 = ScriptedTransport::with(Ok(RemoteSetupResponse {
            bytes: vec![0xff],
            resolved_locator: None,
        }));
        assert!(matches!(
            run(acquire_setup_document(
                &cache,
                &non_utf8,
                locator,
                SetupAcquirePolicy::OnlineRefresh
            )),
            Err(SetupAcquireError::InputNotUtf8)
        ));
    }
}
