//! One-way import of a Python bosn v4 registry.

use super::*;

pub(crate) const CUTOVER_MARKER: &str = "rust-cutover-v1.json";
pub(crate) const RECONCILIATION_REQUIRED: &str = "migration.reconciliation_required";
pub(crate) const IMPORT_BATCH: usize = 1_000;

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
pub(crate) fn import_python_v4_inner(
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
    // An importer is a copy/cutover operation, never an in-place schema
    // mutation.  Refuse spelling aliases of the source before opening SQLite
    // so a caller cannot turn a failed or interrupted import into source
    // damage by choosing the source as its target.
    let destination_identity = match fs::path_identity(destination) {
        Ok(identity) => identity,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(Error::Io(error)),
    };
    if destination_identity == expected_identity {
        return Err(Error::SourceDestinationAliased(destination.into()));
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
pub(crate) struct ImportRows {
    pub(crate) meta: BTreeMap<String, String>,
    pub(crate) resources: Vec<Resource>,
    pub(crate) resource_uses: Vec<ResourceUse>,
    pub(crate) leases: Vec<Lease>,
    pub(crate) sessions: Vec<ExecutionSession>,
    pub(crate) intents: Vec<VolumeCreationIntent>,
    pub(crate) generations: Vec<Generation>,
    pub(crate) events: Vec<Event>,
    pub(crate) event_sequence: Option<i64>,
}
impl ImportRows {
    pub(crate) fn read(c: &Connection) -> Result<Self, Error> {
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
    pub(crate) fn validate_owners(&self) -> Result<(), Error> {
        // `lease_ids` is JSON rather than a SQLite foreign key in v4.  Do not
        // promote an orphaned session into the native registry: it would make
        // a later reconciliation reason about an ownership relationship that
        // never existed durably in the source.
        let known_leases = self
            .leases
            .iter()
            .map(|lease| lease.id.as_str())
            .collect::<BTreeSet<_>>();
        for session in &self.sessions {
            let mut seen = BTreeSet::new();
            for lease_id in &session.lease_ids {
                if !seen.insert(lease_id.as_str()) || !known_leases.contains(lease_id.as_str()) {
                    return Err(Error::InvalidSchema);
                }
            }
        }
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
    pub(crate) fn counts(&self) -> BTreeMap<String, usize> {
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
pub(crate) fn import_all<T>(
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
pub(crate) fn import_meta(c: &Connection) -> Result<BTreeMap<String, String>, Error> {
    let pairs = import_all(c, "SELECT key,value FROM meta ORDER BY key", |row| {
        Ok((text(row, 0)?, text(row, 1)?))
    })?;
    let meta = pairs.iter().cloned().collect::<BTreeMap<_, _>>();
    if meta.len() != pairs.len() {
        return Err(Error::InvalidSchema);
    }
    Ok(meta)
}
pub(crate) fn event_sequence(c: &Connection) -> Result<Option<i64>, Error> {
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
pub(crate) fn validate_v4_schema(c: &Connection) -> Result<(), Error> {
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
    // v4 is deliberately a closed import contract.  An extra application
    // table can encode lifecycle state this importer does not understand;
    // accepting it and calling the result a complete cutover would be lossy.
    // SQLite owns `sqlite_sequence` for the AUTOINCREMENT events table.
    let expected_tables = expected
        .iter()
        .map(|(name, _)| *name)
        .chain(std::iter::once("sqlite_sequence"))
        .collect::<BTreeSet<_>>();
    if tables
        .iter()
        .any(|found| !expected_tables.contains(found.as_str()))
    {
        return Err(Error::InvalidSchema);
    }
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
