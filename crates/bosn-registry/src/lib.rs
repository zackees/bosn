//! Durable Bosn registry schema and typed, bounded SQLite access.
//!
//! This is deliberately a foundation, not the Python-v4 importer or daemon.
//! In particular a v4 database is refused for writing until the explicit,
//! quiesced import/reconciliation milestone lands.

use std::{
    collections::BTreeMap,
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
    SourceOwnershipLive(u32),
    SourceOwnershipUnknown(u32),
    ImportTargetExists(PathBuf),
    ReconciliationRequired,
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

const CUTOVER_MARKER: &str = "rust-cutover-v1.json";
const RECONCILIATION_REQUIRED: &str = "migration.reconciliation_required";
const IMPORT_BATCH: usize = 1_000;

/// Import a bridge-quiesced Python schema-v4 registry into an unpublished v5
/// destination. The destination appears only after the staged v5 database has
/// committed and passed integrity checking; no lifecycle writer may open it
/// until a future engine reconciliation clears the reserved gate.
pub fn import_python_v4(
    state_dir: impl AsRef<Path>,
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<ImportReport, Error> {
    import_python_v4_inner(
        state_dir.as_ref(),
        source.as_ref(),
        destination.as_ref(),
        None,
    )
}

// The callback is test-only plumbing for a deterministic replacement race.
// Production callers always use the public wrapper above.
fn import_python_v4_inner(
    state_dir: &Path,
    source: &Path,
    destination: &Path,
    after_identity_capture: Option<&dyn Fn()>,
) -> Result<ImportReport, Error> {
    let _guard = acquire_legacy_migration_guard(state_dir)?;
    let expected_source = state_dir.join("registry.sqlite3");
    let expected_identity = fs::path_identity(&expected_source)?;
    let source_identity = fs::path_identity(source)?;
    if expected_identity.is_none() || source_identity != expected_identity {
        return Err(Error::InvalidSchema);
    }
    let marker = fs::read_private_regular_file_bounded(&state_dir.join(CUTOVER_MARKER), 4096)
        .map_err(Error::Io)?;
    let marker: serde_json::Value =
        serde_json::from_slice(&marker).map_err(|_| Error::InvalidCutoverMarker)?;
    let marker_id = marker
        .get("registry_id")
        .and_then(serde_json::Value::as_str)
        .filter(|_| marker.get("protocol") == Some(&serde_json::Value::from(1)))
        .ok_or(Error::InvalidCutoverMarker)?;
    if !is_uuid(marker_id) {
        return Err(Error::InvalidCutoverMarker);
    }
    if let Some(callback) = after_identity_capture {
        callback();
    }

    // Open only the database anchored to the guarded state directory. The
    // caller's alias is retained solely for the identity rechecks below.
    let source_connection = Connection::open_read_only_with_busy_timeout(
        &expected_source,
        std::time::Duration::from_secs(5),
    )?;
    source_connection.integrity_check()?;
    if fs::path_identity(&expected_source)? != expected_identity
        || fs::path_identity(source)? != source_identity
    {
        return Err(Error::ReplacedPath(expected_source));
    }
    // A consistent backup captures committed WAL content without writing source.
    let scratch = fs::TemporaryDirectory::new()?;
    ipc::ensure_owner_private_directory(scratch.path()).map_err(Error::Io)?;
    let snapshot = scratch.path().join("python-v4-snapshot.sqlite");
    source_connection.backup_to(&snapshot)?;
    if fs::path_identity(&expected_source)? != expected_identity
        || fs::path_identity(source)? != source_identity
    {
        return Err(Error::ReplacedPath(expected_source));
    }
    drop(source_connection);
    let snapshot_connection =
        Connection::open_read_only_with_busy_timeout(&snapshot, std::time::Duration::from_secs(5))?;
    snapshot_connection.integrity_check()?;
    validate_v4_schema(&snapshot_connection)?;
    let source_meta = import_meta(&snapshot_connection)?;
    let source_id = source_meta.get("registry_id").ok_or(Error::InvalidSchema)?;
    let version = source_meta
        .get("schema_version")
        .ok_or(Error::InvalidSchema)?
        .parse::<u32>()
        .map_err(|_| Error::BadRow("schema_version"))?;
    if version < 4 {
        return Err(Error::LegacyImportRequired(version));
    }
    if version > 4 {
        return Err(Error::UnsupportedSchema(version));
    }
    if source_id != marker_id {
        return Err(Error::CutoverRegistryMismatch);
    }
    let rows = ImportRows::read(&snapshot_connection)?;
    rows.validate_owners()?;

    let staged = scratch.path().join("imported-v5.sqlite");
    let mut registry = Registry::create_writer(&staged, source_id)?;
    let mut transaction = registry.begin_immediate()?;
    for (key, value) in &rows.meta {
        if !matches!(key.as_str(), "schema_version" | "registry_id") {
            transaction.set_meta(key, value)?;
        }
    }
    transaction.transaction.execute(
        "INSERT INTO meta(key,value) VALUES(?,?)",
        &[
            Value::Text(RECONCILIATION_REQUIRED.into()),
            Value::Text("true".into()),
        ],
    )?;
    for value in &rows.resources {
        transaction.put_resource(value)?;
    }
    for value in &rows.resource_uses {
        transaction.put_resource_use(value)?;
    }
    for value in &rows.leases {
        transaction.put_lease(value)?;
    }
    for value in &rows.sessions {
        transaction.put_execution_session(value)?;
    }
    for value in &rows.intents {
        transaction.put_volume_creation_intent(value)?;
    }
    for value in &rows.generations {
        transaction.put_generation(value)?;
    }
    for value in &rows.events {
        transaction.put_event_with_id(value)?;
    }
    if let Some(sequence) = rows.event_sequence {
        let updated = transaction.transaction.execute(
            "UPDATE sqlite_sequence SET seq=? WHERE name='events'",
            &[Value::Integer(sequence)],
        )?;
        if updated == 0 {
            transaction.transaction.execute(
                "INSERT INTO sqlite_sequence(name,seq) VALUES('events',?)",
                &[Value::Integer(sequence)],
            )?;
        }
    }
    transaction.commit()?;
    drop(registry);
    let staged_connection =
        Connection::open_read_only_with_busy_timeout(&staged, std::time::Duration::from_secs(5))?;
    staged_connection.integrity_check()?;
    let destination_parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !ipc::owner_private_directory(destination_parent).map_err(Error::Io)? {
        return Err(Error::InsecureDirectory(destination_parent.into()));
    }
    staged_connection
        .backup_to(destination)
        .map_err(|error| match error {
            SqlError::AlreadyExists(path) => Error::ImportTargetExists(path),
            other => Error::Sql(other),
        })?;
    fs::sync_directory(destination_parent)?;
    Ok(ImportReport {
        source: source.into(),
        destination: destination.into(),
        registry_id: source_id.clone(),
        table_counts: rows.counts(),
        reconciliation_required: true,
    })
}

/// Acquire the Rust half of the Python-v4 cooperative cutover protocol.
///
/// This intentionally does not claim that a pre-bridge Python release was
/// fenced: callers must validate the durable bridge cutover proof before
/// treating this guard as source quiescence. It does prove that all writers
/// which implement the bridge's shared lock have closed.
pub fn acquire_legacy_migration_guard(
    state_dir: impl AsRef<Path>,
) -> Result<LegacyMigrationGuard, Error> {
    let path = state_dir.as_ref().join("registry.migration.lock");
    let file = fs::open_lock_file(&path)?;
    let lock = fs::try_lock_exclusive_owned(file).map_err(|error| {
        if fs::is_lock_conflict(&error) {
            Error::MigrationGuardHeld(path)
        } else {
            Error::Io(error)
        }
    })?;
    Ok(LegacyMigrationGuard { _lock: lock })
}

#[derive(Default)]
struct ImportRows {
    meta: BTreeMap<String, String>,
    resources: Vec<Resource>,
    resource_uses: Vec<ResourceUse>,
    leases: Vec<Lease>,
    sessions: Vec<ExecutionSession>,
    intents: Vec<VolumeCreationIntent>,
    generations: Vec<Generation>,
    events: Vec<Event>,
    event_sequence: Option<i64>,
}
impl ImportRows {
    fn read(c: &Connection) -> Result<Self, Error> {
        Ok(Self {
            meta: import_meta(c)?,
            resources: import_all(
                c,
                "SELECT id,kind,name,stack,generation,scope,workspace,created_at,last_used,state,retention FROM resources ORDER BY id",
                resource,
            )?,
            resource_uses: import_all(
                c,
                "SELECT resource_id,workspace,stack,generation,last_used,state FROM resource_uses ORDER BY resource_id,workspace,stack,generation",
                resource_use,
            )?,
            leases: import_all(
                c,
                "SELECT id,resource_id,pid,proc_start,acquired_at,heartbeat_at,ttl_seconds FROM leases ORDER BY id",
                lease,
            )?,
            sessions: import_all(
                c,
                "SELECT id,container_id,engine_binary,client_pid,client_start,lease_ids FROM execution_sessions ORDER BY id",
                session,
            )?,
            intents: import_all(
                c,
                "SELECT name,labels,stack,generation,scope,workspace FROM volume_creation_intents ORDER BY name",
                intent,
            )?,
            generations: import_all(
                c,
                "SELECT workspace,stack,digest,created_at,superseded_at FROM generations ORDER BY workspace,stack,digest",
                generation,
            )?,
            events: import_all(c, "SELECT id,at,kind,detail FROM events ORDER BY id", event)?,
            event_sequence: event_sequence(c)?,
        })
    }
    fn validate_owners(&self) -> Result<(), Error> {
        for pid in self
            .leases
            .iter()
            .map(|value| value.pid)
            .chain(self.sessions.iter().map(|value| value.client_pid))
        {
            match process::capture_identity(pid) {
                ProcessIdentityCapture::Exited => {}
                ProcessIdentityCapture::Found(_) => return Err(Error::SourceOwnershipLive(pid)),
                ProcessIdentityCapture::Unavailable(_) | ProcessIdentityCapture::Error(_) => {
                    return Err(Error::SourceOwnershipUnknown(pid));
                }
            }
        }
        Ok(())
    }
    fn counts(&self) -> BTreeMap<String, usize> {
        BTreeMap::from([
            ("meta".into(), self.meta.len()),
            ("resources".into(), self.resources.len()),
            ("resource_uses".into(), self.resource_uses.len()),
            ("leases".into(), self.leases.len()),
            ("execution_sessions".into(), self.sessions.len()),
            ("volume_creation_intents".into(), self.intents.len()),
            ("generations".into(), self.generations.len()),
            ("events".into(), self.events.len()),
        ])
    }
}
fn import_all<T>(
    c: &Connection,
    sql: &str,
    parse: fn(&Row) -> Result<T, Error>,
) -> Result<Vec<T>, Error> {
    let mut output = Vec::new();
    let mut offset = 0i64;
    loop {
        let rows = c.query(
            &format!("{sql} LIMIT ? OFFSET ?"),
            &[
                Value::Integer((IMPORT_BATCH + 1) as i64),
                Value::Integer(offset),
            ],
            QueryLimits {
                max_rows: IMPORT_BATCH + 1,
                max_bytes: 4 * 1024 * 1024,
            },
        )?;
        let count = rows.len();
        output.extend(
            rows.into_iter()
                .take(IMPORT_BATCH)
                .map(|row| parse(&row))
                .collect::<Result<Vec<_>, _>>()?,
        );
        if count <= IMPORT_BATCH {
            return Ok(output);
        }
        offset = offset
            .checked_add(IMPORT_BATCH as i64)
            .ok_or(Error::BadRow("import offset"))?;
    }
}
fn import_meta(c: &Connection) -> Result<BTreeMap<String, String>, Error> {
    let pairs = import_all(c, "SELECT key,value FROM meta ORDER BY key", |row| {
        Ok((text(row, 0)?, text(row, 1)?))
    })?;
    let meta = pairs.iter().cloned().collect::<BTreeMap<_, _>>();
    if meta.len() != pairs.len() {
        return Err(Error::InvalidSchema);
    }
    Ok(meta)
}
fn event_sequence(c: &Connection) -> Result<Option<i64>, Error> {
    let rows = c.query(
        "SELECT seq FROM sqlite_sequence WHERE name='events'",
        &[],
        QueryLimits {
            max_rows: 2,
            max_bytes: 128,
        },
    )?;
    if rows.len() > 1 {
        return Err(Error::InvalidSchema);
    }
    let sequence = rows.first().map(|row| integer(row, 0)).transpose()?;
    if let Some(sequence) = sequence {
        let maximum = c.query(
            "SELECT max(id) FROM events",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 128,
            },
        )?;
        let maximum = maximum
            .first()
            .and_then(|row| row.get(0))
            .and_then(|value| match value {
                Value::Integer(value) => Some(*value),
                Value::Null => Some(0),
                _ => None,
            })
            .ok_or(Error::InvalidSchema)?;
        if sequence < 0 || sequence < maximum {
            return Err(Error::InvalidSchema);
        }
    }
    Ok(sequence)
}
fn validate_v4_schema(c: &Connection) -> Result<(), Error> {
    let tables = import_all(
        c,
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
        |row| text(row, 0),
    )?;
    let expected = [
        ("meta", &["key", "value"][..]),
        (
            "resources",
            &[
                "id",
                "kind",
                "name",
                "stack",
                "generation",
                "scope",
                "workspace",
                "created_at",
                "last_used",
                "state",
                "retention",
            ][..],
        ),
        (
            "resource_uses",
            &[
                "resource_id",
                "workspace",
                "stack",
                "generation",
                "last_used",
                "state",
            ][..],
        ),
        (
            "leases",
            &[
                "id",
                "resource_id",
                "pid",
                "proc_start",
                "acquired_at",
                "heartbeat_at",
                "ttl_seconds",
            ][..],
        ),
        (
            "execution_sessions",
            &[
                "id",
                "container_id",
                "engine_binary",
                "client_pid",
                "client_start",
                "lease_ids",
            ][..],
        ),
        (
            "volume_creation_intents",
            &[
                "name",
                "labels",
                "stack",
                "generation",
                "scope",
                "workspace",
            ][..],
        ),
        (
            "generations",
            &[
                "workspace",
                "stack",
                "digest",
                "created_at",
                "superseded_at",
            ][..],
        ),
        ("events", &["id", "at", "kind", "detail"][..]),
    ];
    for (table, columns) in expected {
        if !tables.iter().any(|found| found == table) {
            return Err(Error::InvalidSchema);
        }
        let actual = import_all(
            c,
            &format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"),
            |row| text(row, 0),
        )?;
        if actual.iter().map(String::as_str).collect::<Vec<_>>() != columns {
            return Err(Error::InvalidSchema);
        }
    }
    for (table, key) in [
        ("meta", "key"),
        ("resources", "id"),
        ("leases", "id"),
        ("execution_sessions", "id"),
        ("volume_creation_intents", "name"),
        ("events", "id"),
    ] {
        let duplicates = c.query(
            &format!("SELECT 1 FROM {table} GROUP BY {key} HAVING count(*) > 1 LIMIT 1"),
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 64,
            },
        )?;
        if !duplicates.is_empty() {
            return Err(Error::InvalidSchema);
        }
    }
    for (table, keys) in [
        ("resource_uses", "resource_id,workspace,stack,generation"),
        ("generations", "workspace,stack,digest"),
    ] {
        let duplicates = c.query(
            &format!("SELECT 1 FROM {table} GROUP BY {keys} HAVING count(*) > 1 LIMIT 1"),
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 64,
            },
        )?;
        if !duplicates.is_empty() {
            return Err(Error::InvalidSchema);
        }
    }
    let foreign = c.query(
        "SELECT 1 FROM pragma_foreign_key_check LIMIT 1",
        &[],
        QueryLimits {
            max_rows: 1,
            max_bytes: 1024,
        },
    )?;
    if !foreign.is_empty() {
        return Err(Error::InvalidSchema);
    }
    Ok(())
}
pub struct Immediate<'a> {
    transaction: Transaction<'a>,
}
impl<'a> Immediate<'a> {
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<(), Error> {
        if matches!(key, "schema_version" | "registry_id") || key == RECONCILIATION_REQUIRED {
            return Err(Error::ReservedMeta("schema_version or registry_id"));
        }
        self.transaction.execute("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", &[Value::Text(key.into()), Value::Text(value.into())])?;
        Ok(())
    }
    pub fn put_resource(&mut self, v: &Resource) -> Result<(), Error> {
        let rows = self.transaction.query(
            "SELECT id FROM resources WHERE kind=? AND name=?",
            &[
                Value::Text(v.kind.as_str().into()),
                Value::Text(v.name.clone()),
            ],
            QueryLimits {
                max_rows: 2,
                max_bytes: 4096,
            },
        )?;
        if let Some(row) = rows.first()
            && text(row, 0)? != v.id
        {
            return Err(Error::ResourceIdentityConflict);
        }
        self.transaction.execute("INSERT INTO resources(id,kind,name,stack,generation,scope,workspace,created_at,last_used,state,retention) VALUES(?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(kind,name) DO UPDATE SET stack=excluded.stack,generation=excluded.generation,scope=excluded.scope,workspace=excluded.workspace,last_used=excluded.last_used,state=excluded.state,retention=excluded.retention", &[Value::Text(v.id.clone()),Value::Text(v.kind.as_str().into()),Value::Text(v.name.clone()),Value::Text(v.stack.clone()),Value::Text(v.generation.clone()),Value::Text(v.scope.as_str().into()),Value::Text(v.workspace.clone()),Value::Real(v.created_at),Value::Real(v.last_used),Value::Text(v.state.as_str().into()),Value::Text(v.retention.as_str().into())])?;
        Ok(())
    }
    pub fn put_resource_use(&mut self, v: &ResourceUse) -> Result<(), Error> {
        self.transaction.execute("INSERT INTO resource_uses(resource_id,workspace,stack,generation,last_used,state) VALUES(?,?,?,?,?,?) ON CONFLICT(resource_id,workspace,stack,generation) DO UPDATE SET last_used=excluded.last_used,state=excluded.state", &[Value::Text(v.resource_id.clone()),Value::Text(v.workspace.clone()),Value::Text(v.stack.clone()),Value::Text(v.generation.clone()),Value::Real(v.last_used),Value::Text(v.state.as_str().into())])?;
        Ok(())
    }
    pub fn put_lease(&mut self, v: &Lease) -> Result<(), Error> {
        self.transaction.execute("INSERT INTO leases(id,resource_id,pid,proc_start,acquired_at,heartbeat_at,ttl_seconds) VALUES(?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET resource_id=excluded.resource_id,pid=excluded.pid,proc_start=excluded.proc_start,acquired_at=excluded.acquired_at,heartbeat_at=excluded.heartbeat_at,ttl_seconds=excluded.ttl_seconds", &[Value::Text(v.id.clone()),Value::Text(v.resource_id.clone()),Value::Integer(i64::from(v.pid)),optional_value(v.proc_start),Value::Real(v.acquired_at),Value::Real(v.heartbeat_at),Value::Real(v.ttl_seconds)])?;
        Ok(())
    }
    pub fn put_execution_session(&mut self, v: &ExecutionSession) -> Result<(), Error> {
        self.transaction.execute("INSERT INTO execution_sessions(id,container_id,engine_binary,client_pid,client_start,lease_ids) VALUES(?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET container_id=excluded.container_id,engine_binary=excluded.engine_binary,client_pid=excluded.client_pid,client_start=excluded.client_start,lease_ids=excluded.lease_ids", &[Value::Text(v.id.clone()),Value::Text(v.container_id.clone()),Value::Text(v.engine_binary.clone()),Value::Integer(i64::from(v.client_pid)),optional_value(v.client_start),Value::Text(serde_json::to_string(&v.lease_ids).map_err(|_| Error::BadRow("lease ids"))?)])?;
        Ok(())
    }
    pub fn put_volume_creation_intent(&mut self, v: &VolumeCreationIntent) -> Result<(), Error> {
        self.transaction.execute("INSERT INTO volume_creation_intents(name,labels,stack,generation,scope,workspace) VALUES(?,?,?,?,?,?) ON CONFLICT(name) DO UPDATE SET labels=excluded.labels,stack=excluded.stack,generation=excluded.generation,scope=excluded.scope,workspace=excluded.workspace", &[Value::Text(v.name.clone()),Value::Text(serde_json::to_string(&v.labels).map_err(|_| Error::BadRow("labels"))?),Value::Text(v.stack.clone()),Value::Text(v.generation.clone()),Value::Text(v.scope.as_str().into()),Value::Text(v.workspace.clone())])?;
        Ok(())
    }
    pub fn put_generation(&mut self, v: &Generation) -> Result<(), Error> {
        self.transaction.execute("INSERT INTO generations(workspace,stack,digest,created_at,superseded_at) VALUES(?,?,?,?,?) ON CONFLICT(workspace,stack,digest) DO UPDATE SET created_at=excluded.created_at,superseded_at=excluded.superseded_at", &[Value::Text(v.workspace.clone()),Value::Text(v.stack.clone()),Value::Text(v.digest.clone()),Value::Real(v.created_at),optional_value(v.superseded_at)])?;
        Ok(())
    }
    pub fn append_event(&mut self, at: f64, kind: &str, detail: &str) -> Result<(), Error> {
        self.transaction.execute(
            "INSERT INTO events(at,kind,detail) VALUES(?,?,?)",
            &[
                Value::Real(at),
                Value::Text(kind.into()),
                Value::Text(detail.into()),
            ],
        )?;
        Ok(())
    }
    fn put_event_with_id(&mut self, value: &Event) -> Result<(), Error> {
        self.transaction.execute(
            "INSERT INTO events(id,at,kind,detail) VALUES(?,?,?,?)",
            &[
                Value::Integer(value.id),
                Value::Real(value.at),
                Value::Text(value.kind.clone()),
                Value::Text(value.detail.clone()),
            ],
        )?;
        Ok(())
    }
    pub fn delete_resource(&mut self, id: &str) -> Result<(), Error> {
        self.transaction.execute(
            "DELETE FROM resources WHERE id = ?",
            &[Value::Text(id.into())],
        )?;
        Ok(())
    }
    pub fn delete_lease(&mut self, id: &str) -> Result<(), Error> {
        self.transaction
            .execute("DELETE FROM leases WHERE id = ?", &[Value::Text(id.into())])?;
        Ok(())
    }
    pub fn delete_execution_session(&mut self, id: &str) -> Result<(), Error> {
        self.transaction.execute(
            "DELETE FROM execution_sessions WHERE id = ?",
            &[Value::Text(id.into())],
        )?;
        Ok(())
    }
    pub fn delete_volume_creation_intent(&mut self, name: &str) -> Result<(), Error> {
        self.transaction.execute(
            "DELETE FROM volume_creation_intents WHERE name = ?",
            &[Value::Text(name.into())],
        )?;
        Ok(())
    }
    pub fn delete_generation(
        &mut self,
        workspace: &str,
        stack: &str,
        digest: &str,
    ) -> Result<(), Error> {
        self.transaction.execute(
            "DELETE FROM generations WHERE workspace=? AND stack=? AND digest=?",
            &[
                Value::Text(workspace.into()),
                Value::Text(stack.into()),
                Value::Text(digest.into()),
            ],
        )?;
        Ok(())
    }
    pub fn commit(self) -> Result<(), Error> {
        Ok(self.transaction.commit()?)
    }
}
fn optional_value(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::Real)
}

impl Registry {
    /// Opens a fully initialized v5 registry for its sole writer.  The lock is
    /// held for the Registry lifetime, including any caller-held immediate tx.
    pub fn open_writer(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        // This probe is intentionally before any read-write SQLite open: SQLite's
        // normal open creates a missing file and may alter WAL bookkeeping.
        // Fresh databases must use create_writer's explicit create-new path.
        let probe =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate(&probe, path)?;
        // Lock the database inode itself.  A sibling lock name would allow
        // relative/symlink aliases to acquire separate locks.
        let file = fs::open_lock_file(path)?;
        let writer = fs::try_lock_exclusive_owned(file).map_err(|e| {
            if fs::is_lock_conflict(&e) {
                Error::WriterAlreadyHeld(path.to_path_buf())
            } else {
                Error::Io(e)
            }
        })?;
        if fs::path_identity(path)? != fs::file_identity(writer.file())? {
            return Err(Error::ReplacedPath(path.to_path_buf()));
        }
        let connection =
            Connection::open_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate(&connection, path)?;
        if fs::path_identity(path)? != fs::file_identity(writer.file())? {
            return Err(Error::ReplacedPath(path.to_path_buf()));
        }
        Ok(Self {
            connection,
            _writer: writer,
        })
    }
    /// Atomically reserves a new database path, initializes v5, and retains
    /// writer exclusion. The caller supplies the secure UUID because the
    /// kernel random facade is async and this synchronous registry must not
    /// invent an executor.
    pub fn create_writer(path: impl AsRef<Path>, registry_id: &str) -> Result<Self, Error> {
        if !is_uuid(registry_id) {
            return Err(Error::BadRow("registry_id"));
        }
        let path = path.as_ref();
        let file = fs::create_private_file(path)?;
        let writer = fs::try_lock_exclusive_owned(file).map_err(Error::Io)?;
        if fs::path_identity(path)? != fs::file_identity(writer.file())? {
            return Err(Error::ReplacedPath(path.to_path_buf()));
        }
        let mut connection =
            Connection::open_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        let mut transaction = connection.begin_immediate()?;
        for statement in SCHEMA.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            transaction.execute(statement, &[])?;
        }
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES (?, ?), (?, ?)",
            &[
                Value::Text("schema_version".into()),
                Value::Text(SCHEMA_VERSION.to_string()),
                Value::Text("registry_id".into()),
                Value::Text(registry_id.into()),
            ],
        )?;
        transaction.commit()?;
        if fs::path_identity(path)? != fs::file_identity(writer.file())? {
            return Err(Error::ReplacedPath(path.to_path_buf()));
        }
        Ok(Self {
            connection,
            _writer: writer,
        })
    }
    /// Opens diagnostics only. It never initializes, migrates, or takes a writer lock.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<ReadOnlyRegistry, Error> {
        let path = path.as_ref();
        let connection =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate(&connection, path)?;
        Ok(ReadOnlyRegistry { connection })
    }
    /// Verify SQLite's internal consistency through the already-open sole
    /// writer. This is a read-only integrity operation: it neither migrates
    /// nor initializes a registry and does not begin a transaction.
    pub fn integrity_check(&self) -> Result<(), Error> {
        Ok(self.connection.integrity_check()?)
    }
    fn validate(connection: &Connection, path: &Path) -> Result<(), Error> {
        let version = meta(connection, "schema_version")?
            .ok_or_else(|| Error::Uninitialized(path.to_path_buf()))?;
        let version: u32 = version
            .parse()
            .map_err(|_| Error::BadRow("schema_version"))?;
        if version < SCHEMA_VERSION {
            return Err(Error::LegacyImportRequired(version));
        }
        if version > SCHEMA_VERSION {
            return Err(Error::UnsupportedSchema(version));
        }
        let registry_id = meta(connection, "registry_id")?.ok_or(Error::BadRow("registry_id"))?;
        if meta(connection, RECONCILIATION_REQUIRED)?.as_deref() == Some("true") {
            return Err(Error::ReconciliationRequired);
        }
        if !is_uuid(&registry_id) {
            return Err(Error::BadRow("registry_id"));
        }
        let tables = connection.query(
            "SELECT name FROM sqlite_master WHERE type = 'table'",
            &[],
            QueryLimits {
                max_rows: 16,
                max_bytes: 4096,
            },
        )?;
        for required in [
            "meta",
            "resources",
            "resource_uses",
            "leases",
            "execution_sessions",
            "volume_creation_intents",
            "generations",
            "events",
        ] {
            if !tables
                .iter()
                .any(|row| matches!(row.get(0), Some(Value::Text(name)) if name == required))
            {
                return Err(Error::InvalidSchema);
            }
        }
        let columns = connection.query(
            "PRAGMA table_info(resources)",
            &[],
            QueryLimits {
                max_rows: 32,
                max_bytes: 4096,
            },
        )?;
        for required in [
            "id",
            "kind",
            "name",
            "stack",
            "generation",
            "scope",
            "workspace",
            "created_at",
            "last_used",
            "state",
            "retention",
        ] {
            if !columns
                .iter()
                .any(|r| matches!(r.get(1), Some(Value::Text(name)) if name == required))
            {
                return Err(Error::InvalidSchema);
            }
        }
        let indexes = connection.query(
            "PRAGMA index_list(resources)",
            &[],
            QueryLimits {
                max_rows: 32,
                max_bytes: 4096,
            },
        )?;
        if !indexes.iter().any(|r| matches!((r.get(1), r.get(2)), (Some(Value::Text(name)), Some(Value::Integer(1))) if name == "idx_resources_engine_identity")) { return Err(Error::InvalidSchema); }
        let identity = connection.query(
            "PRAGMA index_info(idx_resources_engine_identity)",
            &[],
            QueryLimits {
                max_rows: 4,
                max_bytes: 1024,
            },
        )?;
        if identity.len() != 2
            || !matches!(identity[0].get(2), Some(Value::Text(name)) if name == "kind")
            || !matches!(identity[1].get(2), Some(Value::Text(name)) if name == "name")
        {
            return Err(Error::InvalidSchema);
        }
        for (table, required) in [
            (
                "resource_uses",
                &[
                    "resource_id",
                    "workspace",
                    "stack",
                    "generation",
                    "last_used",
                    "state",
                ][..],
            ),
            (
                "leases",
                &[
                    "id",
                    "resource_id",
                    "pid",
                    "proc_start",
                    "acquired_at",
                    "heartbeat_at",
                    "ttl_seconds",
                ][..],
            ),
            (
                "execution_sessions",
                &[
                    "id",
                    "container_id",
                    "engine_binary",
                    "client_pid",
                    "client_start",
                    "lease_ids",
                ][..],
            ),
            (
                "volume_creation_intents",
                &[
                    "name",
                    "labels",
                    "stack",
                    "generation",
                    "scope",
                    "workspace",
                ][..],
            ),
            (
                "generations",
                &[
                    "workspace",
                    "stack",
                    "digest",
                    "created_at",
                    "superseded_at",
                ][..],
            ),
            ("events", &["id", "at", "kind", "detail"][..]),
        ] {
            let rows = connection.query(
                &format!("PRAGMA table_info({table})"),
                &[],
                QueryLimits {
                    max_rows: 32,
                    max_bytes: 4096,
                },
            )?;
            if required.iter().any(|column| {
                !rows
                    .iter()
                    .any(|r| matches!(r.get(1), Some(Value::Text(name)) if name == column))
            }) {
                return Err(Error::InvalidSchema);
            }
        }
        for table in ["resource_uses", "leases"] {
            let fks = connection.query(
                &format!("PRAGMA foreign_key_list({table})"),
                &[],
                QueryLimits {
                    max_rows: 8,
                    max_bytes: 1024,
                },
            )?;
            if fks.len() != 1
                || !matches!((fks[0].get(2), fks[0].get(3), fks[0].get(4), fks[0].get(6)), (Some(Value::Text(target)), Some(Value::Text(from)), Some(Value::Text(to)), Some(Value::Text(action))) if target == "resources" && from == "resource_id" && to == "id" && action.eq_ignore_ascii_case("cascade"))
            {
                return Err(Error::InvalidSchema);
            }
        }
        Ok(())
    }
    /// The exact v5 schema installed by [`Self::create_writer`].
    pub fn schema_sql() -> &'static str {
        SCHEMA
    }
    pub fn registry_id(&self) -> Result<String, Error> {
        meta(&self.connection, "registry_id")?.ok_or(Error::BadRow("registry_id"))
    }
    pub fn status(&self) -> Result<RegistryStatus, Error> {
        let count = |table: &str| -> Result<u64, Error> {
            let rows = self.connection.query(
                &format!("SELECT COUNT(*) FROM {table}"),
                &[],
                QueryLimits {
                    max_rows: 1,
                    max_bytes: 64,
                },
            )?;
            match rows.first().and_then(|row| row.get(0)) {
                Some(Value::Integer(value)) if *value >= 0 => Ok(*value as u64),
                _ => Err(Error::BadRow("status count")),
            }
        };
        Ok(RegistryStatus {
            registry_id: self.registry_id()?,
            schema_version: SCHEMA_VERSION,
            resources: count("resources")?,
            leases: count("leases")?,
            sessions: count("execution_sessions")?,
            reconciliation_required: self.meta(RECONCILIATION_REQUIRED)?.as_deref() == Some("true"),
        })
    }
    pub fn meta(&self, key: &str) -> Result<Option<String>, Error> {
        meta(&self.connection, key)
    }
    pub fn begin_immediate(&mut self) -> Result<Immediate<'_>, Error> {
        Ok(Immediate {
            transaction: self.connection.begin_immediate()?,
        })
    }
    pub fn resources(&self, offset: usize, limit: usize) -> Result<Page<Resource>, Error> {
        page(
            &self.connection,
            "SELECT id,kind,name,stack,generation,scope,workspace,created_at,last_used,state,retention FROM resources ORDER BY id LIMIT ? OFFSET ?",
            offset,
            limit,
            resource,
        )
    }
    pub fn resource_uses(&self, offset: usize, limit: usize) -> Result<Page<ResourceUse>, Error> {
        page(
            &self.connection,
            "SELECT resource_id,workspace,stack,generation,last_used,state FROM resource_uses ORDER BY resource_id,workspace,stack,generation LIMIT ? OFFSET ?",
            offset,
            limit,
            resource_use,
        )
    }
    pub fn leases(&self, offset: usize, limit: usize) -> Result<Page<Lease>, Error> {
        page(
            &self.connection,
            "SELECT id,resource_id,pid,proc_start,acquired_at,heartbeat_at,ttl_seconds FROM leases ORDER BY id LIMIT ? OFFSET ?",
            offset,
            limit,
            lease,
        )
    }
    pub fn execution_sessions(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Page<ExecutionSession>, Error> {
        page(
            &self.connection,
            "SELECT id,container_id,engine_binary,client_pid,client_start,lease_ids FROM execution_sessions ORDER BY id LIMIT ? OFFSET ?",
            offset,
            limit,
            session,
        )
    }
    pub fn volume_creation_intents(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Page<VolumeCreationIntent>, Error> {
        page(
            &self.connection,
            "SELECT name,labels,stack,generation,scope,workspace FROM volume_creation_intents ORDER BY name LIMIT ? OFFSET ?",
            offset,
            limit,
            intent,
        )
    }
    pub fn generations(&self, offset: usize, limit: usize) -> Result<Page<Generation>, Error> {
        page(
            &self.connection,
            "SELECT workspace,stack,digest,created_at,superseded_at FROM generations ORDER BY workspace,stack,digest LIMIT ? OFFSET ?",
            offset,
            limit,
            generation,
        )
    }
    pub fn events(&self, offset: usize, limit: usize) -> Result<Page<Event>, Error> {
        page(
            &self.connection,
            "SELECT id,at,kind,detail FROM events ORDER BY id LIMIT ? OFFSET ?",
            offset,
            limit,
            event,
        )
    }
    /// Return bounded setup-ensure diagnostics newest first.  This deliberate
    /// allowlist prevents product front ends from treating the registry event
    /// table as an unbounded raw audit export.
    pub fn setup_ensure_events(&self, offset: usize, limit: usize) -> Result<Page<Event>, Error> {
        page(
            &self.connection,
            "SELECT id,at,kind,detail FROM events WHERE kind LIKE 'setup.ensure.%' ORDER BY id DESC LIMIT ? OFFSET ?",
            offset,
            limit,
            event,
        )
    }
}
fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}
pub struct ReadOnlyRegistry {
    connection: Connection,
}
impl ReadOnlyRegistry {
    pub fn meta(&self, key: &str) -> Result<Option<String>, Error> {
        meta(&self.connection, key)
    }
    pub fn registry_id(&self) -> Result<String, Error> {
        meta(&self.connection, "registry_id")?.ok_or(Error::BadRow("registry_id"))
    }
    pub fn integrity_check(&self) -> Result<(), Error> {
        Ok(self.connection.integrity_check()?)
    }
    pub fn resources(&self, offset: usize, limit: usize) -> Result<Page<Resource>, Error> {
        page(
            &self.connection,
            "SELECT id,kind,name,stack,generation,scope,workspace,created_at,last_used,state,retention FROM resources ORDER BY id LIMIT ? OFFSET ?",
            offset,
            limit,
            resource,
        )
    }
    pub fn resource_uses(&self, offset: usize, limit: usize) -> Result<Page<ResourceUse>, Error> {
        page(
            &self.connection,
            "SELECT resource_id,workspace,stack,generation,last_used,state FROM resource_uses ORDER BY resource_id,workspace,stack,generation LIMIT ? OFFSET ?",
            offset,
            limit,
            resource_use,
        )
    }
    pub fn leases(&self, offset: usize, limit: usize) -> Result<Page<Lease>, Error> {
        page(
            &self.connection,
            "SELECT id,resource_id,pid,proc_start,acquired_at,heartbeat_at,ttl_seconds FROM leases ORDER BY id LIMIT ? OFFSET ?",
            offset,
            limit,
            lease,
        )
    }
    pub fn execution_sessions(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Page<ExecutionSession>, Error> {
        page(
            &self.connection,
            "SELECT id,container_id,engine_binary,client_pid,client_start,lease_ids FROM execution_sessions ORDER BY id LIMIT ? OFFSET ?",
            offset,
            limit,
            session,
        )
    }
    pub fn volume_creation_intents(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Page<VolumeCreationIntent>, Error> {
        page(
            &self.connection,
            "SELECT name,labels,stack,generation,scope,workspace FROM volume_creation_intents ORDER BY name LIMIT ? OFFSET ?",
            offset,
            limit,
            intent,
        )
    }
    pub fn generations(&self, offset: usize, limit: usize) -> Result<Page<Generation>, Error> {
        page(
            &self.connection,
            "SELECT workspace,stack,digest,created_at,superseded_at FROM generations ORDER BY workspace,stack,digest LIMIT ? OFFSET ?",
            offset,
            limit,
            generation,
        )
    }
    pub fn events(&self, offset: usize, limit: usize) -> Result<Page<Event>, Error> {
        page(
            &self.connection,
            "SELECT id,at,kind,detail FROM events ORDER BY id LIMIT ? OFFSET ?",
            offset,
            limit,
            event,
        )
    }
    /// Read-only counterpart to [`Registry::setup_ensure_events`].
    pub fn setup_ensure_events(&self, offset: usize, limit: usize) -> Result<Page<Event>, Error> {
        page(
            &self.connection,
            "SELECT id,at,kind,detail FROM events WHERE kind LIKE 'setup.ensure.%' ORDER BY id DESC LIMIT ? OFFSET ?",
            offset,
            limit,
            event,
        )
    }
}
fn meta(c: &Connection, key: &str) -> Result<Option<String>, Error> {
    c.query(
        "SELECT value FROM meta WHERE key = ?",
        &[Value::Text(key.into())],
        QueryLimits {
            max_rows: 2,
            max_bytes: 4096,
        },
    )?
    .into_iter()
    .next()
    .map(|r| text(&r, 0))
    .transpose()
}
fn page<T>(
    c: &Connection,
    sql: &str,
    offset: usize,
    limit: usize,
    parse: fn(&Row) -> Result<T, Error>,
) -> Result<Page<T>, Error> {
    let limit = limit.clamp(1, MAX_PAGE_SIZE);
    let query_limit = limit.checked_add(1).ok_or(Error::BadRow("page limit"))?;
    let sql_limit = i64::try_from(query_limit).map_err(|_| Error::BadRow("page limit"))?;
    let sql_offset = i64::try_from(offset).map_err(|_| Error::BadRow("page offset"))?;
    let rows = c.query(
        sql,
        &[Value::Integer(sql_limit), Value::Integer(sql_offset)],
        QueryLimits {
            max_rows: query_limit,
            max_bytes: 1_048_576,
        },
    )?;
    let more = rows.len() > limit;
    let next_offset = if more {
        Some(
            offset
                .checked_add(limit)
                .ok_or(Error::BadRow("page offset"))?,
        )
    } else {
        None
    };
    let items = rows
        .into_iter()
        .take(limit)
        .map(|r| parse(&r))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page { items, next_offset })
}
fn text(r: &Row, i: usize) -> Result<String, Error> {
    match r.get(i) {
        Some(Value::Text(v)) => Ok(v.clone()),
        _ => Err(Error::BadRow("text")),
    }
}
fn real(r: &Row, i: usize) -> Result<f64, Error> {
    let value = match r.get(i) {
        Some(Value::Real(v)) => Ok(*v),
        Some(Value::Integer(v)) => Ok(*v as f64),
        _ => Err(Error::BadRow("real")),
    }?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(Error::BadRow("finite real"))
    }
}
fn integer(r: &Row, i: usize) -> Result<i64, Error> {
    match r.get(i) {
        Some(Value::Integer(v)) => Ok(*v),
        _ => Err(Error::BadRow("integer")),
    }
}
fn optional_real(r: &Row, i: usize) -> Result<Option<f64>, Error> {
    let value = match r.get(i) {
        Some(Value::Null) => Ok(None),
        Some(Value::Real(v)) => Ok(Some(*v)),
        Some(Value::Integer(v)) => Ok(Some(*v as f64)),
        _ => Err(Error::BadRow("optional real")),
    }?;
    if value.is_none_or(f64::is_finite) {
        Ok(value)
    } else {
        Err(Error::BadRow("finite optional real"))
    }
}
fn kind(v: String) -> Result<ResourceKind, Error> {
    match v.as_str() {
        "container" => Ok(ResourceKind::Container),
        "volume" => Ok(ResourceKind::Volume),
        "image" => Ok(ResourceKind::Image),
        "builder" => Ok(ResourceKind::Builder),
        "network" => Ok(ResourceKind::Network),
        _ => Err(Error::BadRow("kind")),
    }
}
fn scope(v: String) -> Result<Scope, Error> {
    match v.as_str() {
        "spec" => Ok(Scope::Spec),
        "stack" => Ok(Scope::Stack),
        "machine" => Ok(Scope::Machine),
        _ => Err(Error::BadRow("scope")),
    }
}
fn state(v: String) -> Result<ResourceState, Error> {
    match v.as_str() {
        "active" => Ok(ResourceState::Active),
        "adopted" => Ok(ResourceState::Adopted),
        "done" => Ok(ResourceState::Done),
        "retired" => Ok(ResourceState::Retired),
        _ => Err(Error::BadRow("state")),
    }
}
fn retention(v: String) -> Result<Retention, Error> {
    match v.as_str() {
        "warm" => Ok(Retention::Warm),
        "pinned" => Ok(Retention::Pinned),
        _ => Err(Error::BadRow("retention")),
    }
}
fn resource(r: &Row) -> Result<Resource, Error> {
    Ok(Resource {
        id: text(r, 0)?,
        kind: kind(text(r, 1)?)?,
        name: text(r, 2)?,
        stack: text(r, 3)?,
        generation: text(r, 4)?,
        scope: scope(text(r, 5)?)?,
        workspace: text(r, 6)?,
        created_at: real(r, 7)?,
        last_used: real(r, 8)?,
        state: state(text(r, 9)?)?,
        retention: retention(text(r, 10)?)?,
    })
}
fn resource_use(r: &Row) -> Result<ResourceUse, Error> {
    Ok(ResourceUse {
        resource_id: text(r, 0)?,
        workspace: text(r, 1)?,
        stack: text(r, 2)?,
        generation: text(r, 3)?,
        last_used: real(r, 4)?,
        state: state(text(r, 5)?)?,
    })
}
fn lease(r: &Row) -> Result<Lease, Error> {
    Ok(Lease {
        id: text(r, 0)?,
        resource_id: text(r, 1)?,
        pid: u32::try_from(integer(r, 2)?).map_err(|_| Error::BadRow("pid"))?,
        proc_start: optional_real(r, 3)?,
        acquired_at: real(r, 4)?,
        heartbeat_at: real(r, 5)?,
        ttl_seconds: real(r, 6)?,
    })
}
fn session(r: &Row) -> Result<ExecutionSession, Error> {
    Ok(ExecutionSession {
        id: text(r, 0)?,
        container_id: text(r, 1)?,
        engine_binary: text(r, 2)?,
        client_pid: u32::try_from(integer(r, 3)?).map_err(|_| Error::BadRow("client pid"))?,
        client_start: optional_real(r, 4)?,
        lease_ids: serde_json::from_str(&text(r, 5)?).map_err(|_| Error::BadRow("lease ids"))?,
    })
}
fn intent(r: &Row) -> Result<VolumeCreationIntent, Error> {
    Ok(VolumeCreationIntent {
        name: text(r, 0)?,
        labels: serde_json::from_str(&text(r, 1)?).map_err(|_| Error::BadRow("labels"))?,
        stack: text(r, 2)?,
        generation: text(r, 3)?,
        scope: scope(text(r, 4)?)?,
        workspace: text(r, 5)?,
    })
}
fn generation(r: &Row) -> Result<Generation, Error> {
    Ok(Generation {
        workspace: text(r, 0)?,
        stack: text(r, 1)?,
        digest: text(r, 2)?,
        created_at: real(r, 3)?,
        superseded_at: optional_real(r, 4)?,
    })
}
fn event(r: &Row) -> Result<Event, Error> {
    Ok(Event {
        id: integer(r, 0)?,
        at: real(r, 1)?,
        kind: text(r, 2)?,
        detail: text(r, 3)?,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    // This runs before the read-only SQLite open, so the rename is portable:
    // Windows does not permit replacing a file with an already-open handle.
    #[test]
    fn v4_import_refuses_source_replacement_after_identity_capture() {
        let directory = fs::TemporaryDirectory::new().unwrap();
        let source = directory.path().join("registry.sqlite3");
        let replacement = directory.path().join("replacement.sqlite3");
        let destination = directory.path().join("destination.sqlite3");
        let marker = directory.path().join(CUTOVER_MARKER);
        let registry_id = "11111111-2222-4333-8444-555555555555";

        let mut marker_file = fs::create_private_file(&marker).unwrap();
        marker_file
            .write_all(format!(r#"{{"protocol":1,"registry_id":"{registry_id}"}}"#).as_bytes())
            .unwrap();
        marker_file.sync_all().unwrap();
        drop(marker_file);

        for path in [&source, &replacement] {
            drop(fs::create_private_file(path).unwrap());
            let connection = Connection::open(path).unwrap();
            connection
                .execute("CREATE TABLE marker(value TEXT)", &[])
                .unwrap();
        }

        let error = import_python_v4_inner(
            directory.path(),
            &source,
            &destination,
            Some(&|| std::fs::rename(&replacement, &source).unwrap()),
        )
        .unwrap_err();

        assert!(matches!(error, Error::ReplacedPath(path) if path == source));
        assert!(!destination.exists());
    }
}
