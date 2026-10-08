//! The sole-writer registry: open, migrate, read and transaction entry points.

use super::*;

impl Registry {
    /// Actual SQLite authority path, including any completed registry relocation.
    pub fn database_path(&self) -> Result<PathBuf, Error> {
        let rows = self.connection.query(
            "PRAGMA database_list",
            &[],
            QueryLimits {
                max_rows: 8,
                max_bytes: 16 * 1024,
            },
        )?;
        for row in rows {
            if text(&row, 1)? == "main" {
                let path = PathBuf::from(text(&row, 2)?);
                if path.is_absolute() {
                    return Ok(path);
                }
            }
        }
        Err(Error::BadRow("registry authority path"))
    }
    /// Export committed ownership through SQLite's consistent backup API.
    /// The destination must be new; callers publish the snapshot only after
    /// fencing further writes. Copying the database file alone would omit WAL
    /// commits and could lose pins, leases, or execution sessions.
    pub fn backup_ownership(&self, destination: impl AsRef<Path>) -> Result<(), Error> {
        self.connection.backup_to(destination)?;
        Ok(())
    }

    /// Opens a fully initialized v5 registry for its sole writer.  The lock is
    /// held for the Registry lifetime, including any caller-held immediate tx.
    pub fn open_writer(path: impl AsRef<Path>) -> Result<Self, Error> {
        let original = path.as_ref();
        let resolved = Self::resolve_authority(original)?;
        let path = resolved.as_path();
        let mut prior_writers = Vec::new();
        if path != original && original.try_exists()? {
            let identity = fs::path_identity(original)?;
            prior_writers.push(acquire_writer_lock(original)?);
            verify_database_identity(original, identity)?;
        }
        // This probe is intentionally before any read-write SQLite open: SQLite's
        // normal open creates a missing file and may alter WAL bookkeeping.
        // Fresh databases must use create_writer's explicit create-new path.
        let probe =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate(&probe, path)?;
        let identity = fs::path_identity(path)?;
        let writer = acquire_writer_lock(path)?;
        verify_database_identity(path, identity)?;
        let connection =
            Connection::open_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate(&connection, path)?;
        verify_database_identity(path, identity)?;
        Ok(Self {
            connection,
            _writer: writer,
            prior_writers,
            ownership_backup: None,
            ownership_backup_dirty: false,
        })
    }
    /// Opens the one deliberately offline writer permitted to inspect and
    /// complete an imported Python-v4 registry.  Normal writers must keep
    /// refusing the gate; this method still takes the database-inode lock and
    /// requires the gate to be present, so it cannot be used as a generic
    /// escape hatch for ordinary registry mutation.
    pub fn open_reconciliation_writer(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let probe =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate_for_reconciliation(&probe, path)?;
        if meta(&probe, RECONCILIATION_REQUIRED)?.as_deref() != Some("true") {
            return Err(Error::ReconciliationNotRequired);
        }
        let identity = fs::path_identity(path)?;
        let writer = acquire_writer_lock(path)?;
        verify_database_identity(path, identity)?;
        let connection =
            Connection::open_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate_for_reconciliation(&connection, path)?;
        if meta(&connection, RECONCILIATION_REQUIRED)?.as_deref() != Some("true") {
            return Err(Error::ReconciliationNotRequired);
        }
        verify_database_identity(path, identity)?;
        Ok(Self {
            connection,
            _writer: writer,
            prior_writers: Vec::new(),
            ownership_backup: None,
            ownership_backup_dirty: false,
        })
    }
    /// Atomically reserves a new database path, initializes v5, and retains
    /// writer exclusion. The caller supplies the secure UUID because the
    /// kernel random facade is async and this synchronous registry must not
    /// invent an executor.
    pub fn create_writer(path: impl AsRef<Path>, registry_id: &str) -> Result<Self, Error> {
        if Self::resolve_authority(path.as_ref())? != path.as_ref() {
            return Err(Error::BadRow("registry authority already exists"));
        }
        if !is_uuid(registry_id) {
            return Err(Error::BadRow("registry_id"));
        }
        let path = path.as_ref();
        let database = fs::create_private_file(path)?;
        let identity = fs::file_identity(&database)?;
        drop(database);
        let writer = acquire_writer_lock(path)?;
        verify_database_identity(path, identity)?;
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
        verify_database_identity(path, identity)?;
        Ok(Self {
            connection,
            _writer: writer,
            prior_writers: Vec::new(),
            ownership_backup: None,
            ownership_backup_dirty: false,
        })
    }
    /// Opens diagnostics only. It never initializes, migrates, or takes a writer lock.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<ReadOnlyRegistry, Error> {
        let resolved = Self::resolve_authority(path.as_ref())?;
        let path = resolved.as_path();
        let connection =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate(&connection, path)?;
        Ok(ReadOnlyRegistry {
            connection,
            _writer_lock: None,
            _prior_writer_locks: Vec::new(),
        })
    }
    /// Inspect a reconciliation-gated registry without any SQLite write.
    /// This is the preview counterpart to the explicit reconciliation writer.
    pub fn open_reconciliation_preview(path: impl AsRef<Path>) -> Result<ReadOnlyRegistry, Error> {
        Self::open_reconciliation_preview_inner(path.as_ref(), None)
    }

    /// Read a valid registry while excluding its daemon writer. Peer retention
    /// holds this guard across Docker observations and removals. This neither
    /// creates a database nor changes its SQLite contents during preview.
    pub fn open_retention_snapshot(path: impl AsRef<Path>) -> Result<ReadOnlyRegistry, Error> {
        let original = path.as_ref();
        let resolved = Self::resolve_authority(original)?;
        let path = resolved.as_path();
        let mut prior_writers = Vec::new();
        if path != original && original.try_exists()? {
            let identity = fs::path_identity(original)?;
            prior_writers.push(acquire_writer_lock(original)?);
            verify_database_identity(original, identity)?;
        }
        let identity = fs::path_identity(path)?;
        let probe =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate(&probe, path)?;
        drop(probe);
        let writer = acquire_writer_lock(path)?;
        verify_database_identity(path, identity)?;
        let connection =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate(&connection, path)?;
        verify_database_identity(path, identity)?;
        Ok(ReadOnlyRegistry {
            connection,
            _writer_lock: Some(writer),
            _prior_writer_locks: prior_writers,
        })
    }
    // The callback is test-only plumbing for a deterministic pre-lock race.
    pub(crate) fn open_reconciliation_preview_inner(
        path: &Path,
        after_probe: Option<&dyn Fn()>,
    ) -> Result<ReadOnlyRegistry, Error> {
        let identity = fs::path_identity(path)?;
        let probe =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate_for_reconciliation(&probe, path)?;
        if meta(&probe, RECONCILIATION_REQUIRED)?.as_deref() != Some("true") {
            return Err(Error::ReconciliationNotRequired);
        }
        verify_database_identity(path, identity)?;
        drop(probe);
        if let Some(callback) = after_probe {
            callback();
        }
        let writer = acquire_writer_lock(path)?;
        verify_database_identity(path, identity)?;
        // A writer may have changed the gate or schema between probe and lock.
        // Reopen read-only under the fence so no earlier SQLite snapshot is reused.
        let connection =
            Connection::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))?;
        Self::validate_for_reconciliation(&connection, path)?;
        if meta(&connection, RECONCILIATION_REQUIRED)?.as_deref() != Some("true") {
            return Err(Error::ReconciliationNotRequired);
        }
        verify_database_identity(path, identity)?;
        Ok(ReadOnlyRegistry {
            connection,
            _writer_lock: Some(writer),
            _prior_writer_locks: Vec::new(),
        })
    }
    /// Verify SQLite's internal consistency through the already-open sole
    /// writer. This is a read-only integrity operation: it neither migrates
    /// nor initializes a registry and does not begin a transaction.
    pub fn integrity_check(&self) -> Result<(), Error> {
        Ok(self.connection.integrity_check()?)
    }
    pub(crate) fn validate(connection: &Connection, path: &Path) -> Result<(), Error> {
        Self::validate_inner(connection, path, false)
    }
    pub(crate) fn validate_for_reconciliation(
        connection: &Connection,
        path: &Path,
    ) -> Result<(), Error> {
        Self::validate_inner(connection, path, true)
    }
    #[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
    pub(crate) fn validate_inner(
        connection: &Connection,
        path: &Path,
        allow_reconciliation: bool,
    ) -> Result<(), Error> {
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
        if !allow_reconciliation
            && meta(connection, RECONCILIATION_REQUIRED)?.as_deref() == Some("true")
        {
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
        self.mark_ownership_backup_dirty()?;
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
    /// Return one exact logical resource identity. This deliberately does not
    /// accept a selector or wildcard and is used by daemon-owned adoption to
    /// refuse incompatible pre-existing ownership before any write.
    pub fn resource_by_kind_name(
        &self,
        kind: ResourceKind,
        name: &str,
    ) -> Result<Option<Resource>, Error> {
        let rows = self.connection.query(
            "SELECT id,kind,name,stack,generation,scope,workspace,created_at,last_used,state,retention FROM resources WHERE kind=? AND name=?",
            &[Value::Text(kind.as_str().into()), Value::Text(name.into())],
            QueryLimits { max_rows: 2, max_bytes: 8192 },
        )?;
        match rows.as_slice() {
            [] => Ok(None),
            [row] => Ok(Some(resource(row)?)),
            _ => Err(Error::BadRow("duplicate resource identity")),
        }
    }
    /// Preview only retired, Bosn-managed setup containers for one exact
    /// workspace. This makes no SQLite writes and never contacts an engine.
    pub fn setup_gc_preview(
        &self,
        workspace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<SetupGcPreview, Error> {
        setup_gc_preview(&self.connection, workspace, offset, limit)
    }
    pub fn manifest_volume_gc_preview(
        &self,
        workspace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<ManifestVolumeGcPreview, Error> {
        manifest_volume_gc_preview(&self.connection, workspace, offset, limit)
    }
    /// Preview only explicitly releasable durable manifest volumes.  This is
    /// deliberately separate from automatic GC, which must never include
    /// stack/machine or pinned rows.
    pub fn manifest_volume_release_preview(
        &self,
        workspace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Page<ManifestVolumeGcCandidate>, Error> {
        manifest_volume_release_preview(&self.connection, workspace, offset, limit)
    }
    /// Read only the durable setup-container facts for one exact workspace.
    /// This is deliberately narrower than the general diagnostics page: a
    /// reconciler must never discover ownership from Docker names or labels.
    pub fn setup_reconcile_containers(
        &self,
        workspace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Page<Resource>, Error> {
        page_setup_workspace(
            &self.connection,
            workspace,
            "container",
            offset,
            limit,
            resource,
        )
    }
    /// The image identities that may prove one observed setup container. The
    /// caller still fails closed if the bounded set cannot prove its image.
    pub fn setup_reconcile_images(&self, workspace: &str) -> Result<Page<Resource>, Error> {
        page_setup_workspace(
            &self.connection,
            workspace,
            "image",
            0,
            MAX_PAGE_SIZE,
            resource,
        )
    }
    /// Re-read one exact preview candidate.  This is used by the daemon before
    /// any engine mutation; it intentionally accepts no selector or glob.
    pub fn setup_gc_candidate(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
    ) -> Result<Option<SetupGcCandidate>, Error> {
        setup_gc_candidate(&mut self.connection, workspace, id, name, generation)
    }
    pub fn manifest_volume_gc_candidate(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
    ) -> Result<Option<ManifestVolumeGcCandidate>, Error> {
        if !manifest_volume_gc_candidate_exists(
            &mut self.connection,
            workspace,
            id,
            name,
            generation,
        )? {
            return Ok(None);
        }
        Ok(Some(ManifestVolumeGcCandidate {
            id: id.into(),
            name: name.into(),
            generation: generation.into(),
        }))
    }
    pub fn manifest_volume_release_candidate(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
    ) -> Result<Option<ManifestVolumeGcCandidate>, Error> {
        if !manifest_volume_release_candidate_exists(
            &mut self.connection,
            workspace,
            id,
            name,
            generation,
        )? {
            return Ok(None);
        }
        Ok(Some(ManifestVolumeGcCandidate {
            id: id.into(),
            name: name.into(),
            generation: generation.into(),
        }))
    }
    /// Re-read one exact active managed setup container eligible for the
    /// missing-container repair path.  This is a registry-only authorization
    /// predicate; the daemon must still prove Docker reports it absent.
    pub fn setup_missing_repair_candidate(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
    ) -> Result<bool, Error> {
        setup_missing_repair_candidate_exists(
            &mut self.connection,
            workspace,
            id,
            name,
            generation,
            ResourceState::Active.as_str(),
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
    /// Return the bounded daemon-written native-manifest recovery contracts,
    /// newest first.  Contracts deliberately live in the append-only audit
    /// log rather than an unversioned side database: an existing v5 registry
    /// remains readable, and a malformed or older record is simply not a
    /// recovery authorization.  The service still re-proves every detail
    /// against the active resource rows and current manifest before it can
    /// start an engine object.
    pub fn manifest_recovery_contract_details(&self, limit: usize) -> Result<Vec<String>, Error> {
        let limit = limit.clamp(1, MAX_PAGE_SIZE);
        let rows = self.connection.query(
            "SELECT detail FROM events WHERE kind='manifest.recovery.contract' ORDER BY id DESC LIMIT ?",
            &[Value::Integer(i64::try_from(limit).map_err(|_| Error::BadRow("page limit"))?)],
            QueryLimits {
                max_rows: limit,
                max_bytes: 1_048_576,
            },
        )?;
        rows.into_iter().map(|row| text(&row, 0)).collect()
    }
    /// Exact durable veto lookup for one daemon-written native-manifest
    /// desired-state record. The caller supplies a canonical, bounded detail
    /// key and this intentionally does no JSON/prefix interpretation.
    pub fn manifest_autostart_intent_disabled(&self, detail: &str) -> Result<bool, Error> {
        let rows = self.connection.query(
            "SELECT 1 FROM events WHERE kind='manifest.autostart.disabled' AND detail=? LIMIT 1",
            &[Value::Text(detail.into())],
            QueryLimits {
                max_rows: 1,
                max_bytes: 64,
            },
        )?;
        Ok(!rows.is_empty())
    }
    /// Exact, registry-only authorization for restart recovery of one native
    /// manifest container.  This discovers nothing from Docker: the service
    /// supplies a previously daemon-written contract and must independently
    /// prove the current manifest and engine labels before any start.
    pub fn manifest_recovery_container_active(
        &self,
        id: &str,
        name: &str,
        stack: &str,
        generation: &str,
        workspace: &str,
        disabled_intent_detail: &str,
    ) -> Result<bool, Error> {
        let rows = self.connection.query(
            "SELECT 1 FROM resources AS r WHERE r.id=? AND r.name=? AND r.stack=? AND r.generation=? AND r.workspace=? \
               AND r.kind='container' AND r.scope='machine' AND r.state='active' \
               AND (r.id GLOB 'manifest-container:*' OR r.id GLOB 'manifest-guest:*') \
               AND EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND u.workspace=? AND u.stack=? AND u.generation=? AND u.state='active') \
               AND NOT EXISTS (SELECT 1 FROM volume_creation_intents AS v WHERE v.workspace=? AND v.stack=?) \
               AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s WHERE s.container_id=r.id OR s.container_id=r.name) \
               AND NOT EXISTS (SELECT 1 FROM events AS e WHERE e.kind='manifest.autostart.disabled' AND e.detail=?) LIMIT 1",
            &[
                Value::Text(id.into()), Value::Text(name.into()), Value::Text(stack.into()),
                Value::Text(generation.into()), Value::Text(workspace.into()),
                Value::Text(workspace.into()), Value::Text(stack.into()), Value::Text(generation.into()),
                Value::Text(workspace.into()), Value::Text(stack.into()),
                Value::Text(disabled_intent_detail.into()),
            ],
            QueryLimits { max_rows: 1, max_bytes: 128 },
        )?;
        Ok(!rows.is_empty())
    }
    /// Return bounded native lifecycle diagnostics newest first. This
    /// deliberate allowlist includes setup ensure plus manifest restart
    /// recovery outcomes, but prevents product front ends from treating the
    /// registry event table as an unbounded raw audit export.
    pub fn setup_ensure_events(&self, offset: usize, limit: usize) -> Result<Page<Event>, Error> {
        page(
            &self.connection,
            "SELECT id,at,kind,detail FROM events WHERE kind LIKE 'setup.ensure.%' OR kind LIKE 'manifest.recovery.%' OR kind LIKE 'manifest.autostart.%' OR kind LIKE 'manifest.volume_gc.%' ORDER BY id DESC LIMIT ? OFFSET ?",
            offset,
            limit,
            event,
        )
    }
}
pub(crate) fn page_setup_workspace<T>(
    c: &Connection,
    workspace: &str,
    kind: &str,
    offset: usize,
    limit: usize,
    parse: fn(&Row) -> Result<T, Error>,
) -> Result<Page<T>, Error> {
    let limit = limit.clamp(1, MAX_PAGE_SIZE);
    let query_limit = limit.checked_add(1).ok_or(Error::BadRow("page limit"))?;
    let rows = c.query(
        "SELECT id,kind,name,stack,generation,scope,workspace,created_at,last_used,state,retention FROM resources WHERE kind=? AND stack='setup' AND workspace=? ORDER BY id LIMIT ? OFFSET ?",
        &[Value::Text(kind.into()), Value::Text(workspace.into()), Value::Integer(query_limit as i64), Value::Integer(offset as i64)],
        QueryLimits { max_rows: query_limit, max_bytes: 1_048_576 },
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
    Ok(Page {
        items: rows
            .into_iter()
            .take(limit)
            .map(|row| parse(&row))
            .collect::<Result<_, _>>()?,
        next_offset,
    })
}
pub(crate) fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}
