//! The read-only registry handle used by previews and diagnostics.

use super::*;

pub struct ReadOnlyRegistry {
    pub(crate) connection: Connection,
    // Present only for the offline reconciliation preview, whose existing
    // contract excludes a second writer for the complete inspection.
    pub(crate) _writer_lock: Option<kernal_api::platform::fs::OwnedFileLock>,
    pub(crate) _prior_writer_locks: Vec<kernal_api::platform::fs::OwnedFileLock>,
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
            "SELECT id,at,kind,detail FROM events WHERE kind LIKE 'setup.ensure.%' OR kind LIKE 'manifest.recovery.%' OR kind LIKE 'manifest.autostart.%' ORDER BY id DESC LIMIT ? OFFSET ?",
            offset,
            limit,
            event,
        )
    }
}
pub(crate) fn meta(c: &Connection, key: &str) -> Result<Option<String>, Error> {
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
pub(crate) fn page<T>(
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
