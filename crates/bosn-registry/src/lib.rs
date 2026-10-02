//! Durable Bosn registry schema and typed, bounded SQLite access.
//!
//! This is deliberately a foundation, not the Python-v4 importer or daemon.
//! In particular a v4 database is refused for writing until the explicit,
//! quiesced import/reconciliation milestone lands.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use bosn_core::{ResourceKind, ResourceState, Retention, Scope};
use kernal_api::{
    platform::{
        fs, ipc,
        process::{self, ProcessIdentityCapture},
    },
    sqlite::{Connection, Error as SqlError, QueryLimits, Row, Transaction, Value},
};

pub mod act;
mod gc_query;
mod immediate;
mod read_only;
mod registry;
mod rows;
use gc_query::*;
pub use immediate::*;
pub use read_only::*;
use registry::*;
use rows::*;

pub const SCHEMA_VERSION: u32 = 5;
pub const DEFAULT_PAGE_SIZE: usize = 100;
pub const MAX_PAGE_SIZE: usize = 1_000;

const SCHEMA: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE resources (
 id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL, stack TEXT NOT NULL,
 generation TEXT NOT NULL, scope TEXT NOT NULL, workspace TEXT NOT NULL,
 created_at REAL NOT NULL, last_used REAL NOT NULL, state TEXT NOT NULL DEFAULT 'active',
 retention TEXT NOT NULL DEFAULT 'warm');
CREATE UNIQUE INDEX idx_resources_engine_identity ON resources(kind, name);
CREATE INDEX idx_resources_stack ON resources(stack);
CREATE INDEX idx_resources_state ON resources(state);
CREATE TABLE resource_uses (
 resource_id TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
 workspace TEXT NOT NULL, stack TEXT NOT NULL, generation TEXT NOT NULL,
 last_used REAL NOT NULL, state TEXT NOT NULL DEFAULT 'active',
 PRIMARY KEY(resource_id, workspace, stack, generation));
CREATE INDEX idx_resource_uses_workspace ON resource_uses(workspace);
CREATE TABLE leases (
 id TEXT PRIMARY KEY, resource_id TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
 pid INTEGER NOT NULL, proc_start REAL, acquired_at REAL NOT NULL,
 heartbeat_at REAL NOT NULL, ttl_seconds REAL NOT NULL);
CREATE INDEX idx_leases_resource ON leases(resource_id);
CREATE TABLE execution_sessions (
 id TEXT PRIMARY KEY, container_id TEXT NOT NULL, engine_binary TEXT NOT NULL,
 client_pid INTEGER NOT NULL, client_start REAL, lease_ids TEXT NOT NULL);
CREATE TABLE volume_creation_intents (
 name TEXT PRIMARY KEY, labels TEXT NOT NULL, stack TEXT NOT NULL, generation TEXT NOT NULL,
 scope TEXT NOT NULL, workspace TEXT NOT NULL);
CREATE TABLE generations (
 workspace TEXT NOT NULL, stack TEXT NOT NULL, digest TEXT NOT NULL, created_at REAL NOT NULL,
 superseded_at REAL, PRIMARY KEY(workspace, stack, digest));
CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL, kind TEXT NOT NULL,
 detail TEXT NOT NULL DEFAULT '');
"#;

#[derive(Debug)]
pub enum Error {
    Sql(SqlError),
    Io(std::io::Error),
    Uninitialized(PathBuf),
    LegacyImportRequired(u32),
    UnsupportedSchema(u32),
    BadRow(&'static str),
    WriterAlreadyHeld(PathBuf),
    MigrationGuardHeld(PathBuf),
    ReplacedPath(PathBuf),
    InvalidSchema,
    ResourceIdentityConflict,
    ReservedMeta(&'static str),
    InvalidCutoverMarker,
    CutoverRegistryMismatch,
    SourceDestinationAliased(PathBuf),
    SourceOwnershipLive(u32),
    SourceOwnershipUnknown(u32),
    ImportTargetExists(PathBuf),
    ReconciliationRequired,
    ReconciliationNotRequired,
    InsecureDirectory(PathBuf),
}
impl From<SqlError> for Error {
    fn from(v: SqlError) -> Self {
        Self::Sql(v)
    }
}
impl From<std::io::Error> for Error {
    fn from(v: std::io::Error) -> Self {
        Self::Io(v)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Debug, PartialEq)]
pub struct Resource {
    pub id: String,
    pub kind: ResourceKind,
    pub name: String,
    pub stack: String,
    pub generation: String,
    pub scope: Scope,
    pub workspace: String,
    pub created_at: f64,
    pub last_used: f64,
    pub state: ResourceState,
    pub retention: Retention,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceUse {
    pub resource_id: String,
    pub workspace: String,
    pub stack: String,
    pub generation: String,
    pub last_used: f64,
    pub state: ResourceState,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub id: String,
    pub resource_id: String,
    pub pid: u32,
    pub proc_start: Option<f64>,
    pub acquired_at: f64,
    pub heartbeat_at: f64,
    pub ttl_seconds: f64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionSession {
    pub id: String,
    pub container_id: String,
    pub engine_binary: String,
    pub client_pid: u32,
    pub client_start: Option<f64>,
    pub lease_ids: Vec<String>,
}
/// A deliberately conservative, read-only candidate for a future setup GC
/// apply operation.  This is registry accounting only; it has no engine
/// effects and is intentionally narrower than a generic container listing.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupGcCandidate {
    pub id: String,
    pub name: String,
    pub generation: String,
}

/// Result of the narrowly-scoped repair for a setup container which Docker
/// has proved absent.  This is intentionally distinct from GC: the durable
/// record is retained, but its exact active use is retired so a later ensure
/// can recreate and record a replacement generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupMissingRepair {
    Repaired,
    AlreadyRepaired,
}

/// Stable summary of why setup containers in one workspace were protected or
/// excluded from a GC preview. Counts can overlap: a doubtful record should
/// remain protected for every reason observed rather than be made eligible by
/// an arbitrary precedence rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupGcPreviewCounts {
    pub protected_not_retired: u64,
    pub protected_ambiguous_use: u64,
    pub protected_lease: u64,
    pub protected_session: u64,
    pub excluded_unmanaged: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SetupGcPreview {
    pub candidates: Page<SetupGcCandidate>,
    pub counts: SetupGcPreviewCounts,
}
/// A deliberately conservative candidate for one native manifest volume.
/// This is an ownership fact only; callers must still prove the exact Docker
/// labels and an empty attachment set before removal.
#[derive(Clone, Debug, PartialEq)]
pub struct ManifestVolumeGcCandidate {
    pub id: String,
    pub name: String,
    pub generation: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestVolumeGcPreviewCounts {
    pub protected_not_retired: u64,
    pub protected_policy: u64,
    pub protected_ambiguous_use: u64,
    pub protected_lease: u64,
    pub protected_session: u64,
    pub protected_intent: u64,
    pub excluded_unmanaged: u64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ManifestVolumeGcPreview {
    pub candidates: Page<ManifestVolumeGcCandidate>,
    pub counts: ManifestVolumeGcPreviewCounts,
}
/// The durable effect of explicitly completing one setup workspace.  Counts
/// are registry rows only; this operation never observes or changes an engine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupDone {
    pub uses_completed: u64,
    pub resources_completed: u64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct VolumeCreationIntent {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub stack: String,
    pub generation: String,
    pub scope: Scope,
    pub workspace: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Generation {
    pub workspace: String,
    pub stack: String,
    pub digest: String,
    pub created_at: f64,
    pub superseded_at: Option<f64>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: i64,
    pub at: f64,
    pub kind: String,
    pub detail: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_offset: Option<usize>,
}

/// Bounded, exact registry facts suitable for a daemon diagnostic response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryStatus {
    pub registry_id: String,
    pub schema_version: u32,
    pub resources: u64,
    pub leases: u64,
    pub sessions: u64,
    pub reconciliation_required: bool,
}

pub struct Registry {
    connection: Connection,
    _writer: kernal_api::platform::fs::OwnedFileLock,
}

/// Exclusive proof that every bridge-capable Python writer has closed its
/// registry connection. The guard names the state-directory lock file, not
/// the SQLite source, and is held by the eventual importer for its complete
/// backup/import interval.
pub struct LegacyMigrationGuard {
    _lock: kernal_api::platform::fs::OwnedFileLock,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub registry_id: String,
    pub table_counts: BTreeMap<String, usize>,
    pub reconciliation_required: bool,
}

/// One engine identity observed by the offline Python-v4 reconciler.  The
/// registry does not invent these facts: callers may complete the migration
/// only after they have inspected the exact durable resource name and labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationProof {
    pub resource_id: String,
    pub engine_id: String,
}

fn optional_value(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::Real)
}

/// The daemon writer fence must not lock the SQLite database itself: macOS
/// treats that advisory lock as contention with SQLite's internal locks.  A
/// private sibling retains the one-writer fence without participating in the
/// database engine's locking protocol.  Canonicalizing the existing database
/// first ensures relative and parent-directory aliases select that same
/// sibling lock.
fn writer_lock_path(database: &Path) -> Result<PathBuf, Error> {
    let database = std::fs::canonicalize(database)?;
    let name = database.file_name().ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "registry database has no file name",
        ))
    })?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".writer.lock");
    Ok(database.with_file_name(lock_name))
}

fn acquire_writer_lock(database: &Path) -> Result<kernal_api::platform::fs::OwnedFileLock, Error> {
    let lock_path = writer_lock_path(database)?;
    let file = fs::open_lock_file(&lock_path)?;
    fs::try_lock_exclusive_owned(file).map_err(|error| {
        if fs::is_lock_conflict(&error) {
            Error::WriterAlreadyHeld(database.to_path_buf())
        } else {
            Error::Io(error)
        }
    })
}

fn verify_database_identity(
    path: &Path,
    expected: Option<kernal_api::platform::fs::FileIdentity>,
) -> Result<(), Error> {
    if fs::path_identity(path)? != expected {
        return Err(Error::ReplacedPath(path.to_path_buf()));
    }
    Ok(())
}

mod import;
#[cfg(test)]
mod tests;
pub use import::*;
