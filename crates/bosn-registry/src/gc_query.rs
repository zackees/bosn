//! Read-only garbage-collection and repair candidate queries.

use super::*;

pub(crate) fn setup_gc_preview(
    connection: &Connection,
    workspace: &str,
    offset: usize,
    limit: usize,
) -> Result<SetupGcPreview, Error> {
    // A managed Bosn app container has both a product-owned logical namespace
    // and the engine-name convention written by the typed setup/manifest
    // ensure paths. Requiring the matching retired use row avoids acting on
    // partially imported or otherwise incomplete ownership state. Any
    // active/done/adopted use, foreign scope, lease, or session protects the
    // record. Manifest containers deliberately use a separate namespace, so
    // this remains an explicit finite set rather than a generic GC selector.
    let candidate_sql = "SELECT r.id,r.name,r.generation FROM resources AS r \
        WHERE r.kind='container' AND r.workspace=? \
          AND r.state='retired' AND r.scope='machine' \
          AND ((r.stack='setup' AND r.id GLOB 'setup-container:*') \
               OR r.id GLOB 'manifest-container:*' OR r.id GLOB 'manifest-guest:*') \
          AND r.name GLOB 'bosn-setup-*' \
          AND EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id \
             AND u.workspace=? AND u.stack=r.stack AND u.state='retired') \
          AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id \
             AND (u.workspace<>? OR u.stack<>r.stack OR u.state<>'retired')) \
          AND NOT EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id) \
          AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s \
             WHERE s.container_id=r.id OR s.container_id=r.name) \
        ORDER BY r.id LIMIT ? OFFSET ?";
    let limit = limit.clamp(1, MAX_PAGE_SIZE);
    let query_limit = limit.checked_add(1).ok_or(Error::BadRow("page limit"))?;
    let rows = connection.query(
        candidate_sql,
        &[
            Value::Text(workspace.into()),
            Value::Text(workspace.into()),
            Value::Text(workspace.into()),
            Value::Integer(i64::try_from(query_limit).map_err(|_| Error::BadRow("page limit"))?),
            Value::Integer(i64::try_from(offset).map_err(|_| Error::BadRow("page offset"))?),
        ],
        QueryLimits {
            max_rows: query_limit,
            max_bytes: 1_048_576,
        },
    )?;
    let more = rows.len() > limit;
    let candidates = Page {
        items: rows
            .into_iter()
            .take(limit)
            .map(|row| {
                Ok(SetupGcCandidate {
                    id: text(&row, 0)?,
                    name: text(&row, 1)?,
                    generation: text(&row, 2)?,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?,
        next_offset: more
            .then(|| {
                offset
                    .checked_add(limit)
                    .ok_or(Error::BadRow("page offset"))
            })
            .transpose()?,
    };
    let count = |predicate: &str| -> Result<u64, Error> {
        let sql = format!(
            "SELECT COUNT(*) FROM resources AS r WHERE r.kind='container' AND r.workspace=? AND {predicate}"
        );
        let row = connection.query(
            &sql,
            &[Value::Text(workspace.into())],
            QueryLimits {
                max_rows: 1,
                max_bytes: 1024,
            },
        )?;
        match row.first().and_then(|row| row.get(0)) {
            Some(Value::Integer(value)) if *value >= 0 => Ok(*value as u64),
            _ => Err(Error::BadRow("setup gc count")),
        }
    };
    let managed = "r.scope='machine' AND r.name GLOB 'bosn-setup-*' AND \
        ((r.stack='setup' AND r.id GLOB 'setup-container:*') OR r.id GLOB 'manifest-container:*' OR r.id GLOB 'manifest-guest:*')";
    let counts = SetupGcPreviewCounts {
        protected_not_retired: count(&format!("{managed} AND r.state<>'retired'"))?,
        protected_ambiguous_use: count(&format!(
            "{managed} AND r.state='retired' AND (NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND u.workspace=r.workspace AND u.stack=r.stack AND u.state='retired') OR EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND (u.workspace<>r.workspace OR u.stack<>r.stack OR u.state<>'retired')) )"
        ))?,
        protected_lease: count(&format!(
            "{managed} AND r.state='retired' AND EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id)"
        ))?,
        protected_session: count(&format!(
            "{managed} AND r.state='retired' AND EXISTS (SELECT 1 FROM execution_sessions AS s WHERE s.container_id=r.id OR s.container_id=r.name)"
        ))?,
        excluded_unmanaged: count(&format!("NOT ({managed})"))?,
    };
    Ok(SetupGcPreview { candidates, counts })
}

pub(crate) fn setup_gc_candidate(
    connection: &mut Connection,
    workspace: &str,
    id: &str,
    name: &str,
    generation: &str,
) -> Result<Option<SetupGcCandidate>, Error> {
    if !setup_gc_candidate_exists(connection, workspace, id, name, generation)? {
        return Ok(None);
    }
    Ok(Some(SetupGcCandidate {
        id: id.into(),
        name: name.into(),
        generation: generation.into(),
    }))
}

/// The exact predicate shared by preview revalidation and finalization. Keep
/// this deliberately explicit: a newly-created lease/session or a use from a
/// different workspace/stack makes a formerly eligible candidate ineligible.
pub(crate) fn setup_gc_candidate_exists(
    connection: &mut impl SetupGcQuery,
    workspace: &str,
    id: &str,
    name: &str,
    generation: &str,
) -> Result<bool, Error> {
    let rows = connection.setup_gc_query(
        "SELECT 1 FROM resources AS r WHERE r.id=? AND r.name=? AND r.generation=? \
         AND r.kind='container' AND r.workspace=? \
         AND r.state='retired' AND r.scope='machine' \
         AND ((r.stack='setup' AND r.id GLOB 'setup-container:*') \
              OR r.id GLOB 'manifest-container:*' OR r.id GLOB 'manifest-guest:*') \
         AND r.name GLOB 'bosn-setup-*' \
         AND EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id \
            AND u.workspace=? AND u.stack=r.stack AND u.state='retired') \
         AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id \
            AND (u.workspace<>? OR u.stack<>r.stack OR u.state<>'retired')) \
         AND NOT EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id) \
         AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s \
            WHERE s.container_id=r.id OR s.container_id=r.name) LIMIT 1",
        &[
            Value::Text(id.into()),
            Value::Text(name.into()),
            Value::Text(generation.into()),
            Value::Text(workspace.into()),
            Value::Text(workspace.into()),
            Value::Text(workspace.into()),
        ],
    )?;
    Ok(!rows.is_empty())
}

pub(crate) fn manifest_volume_gc_preview(
    connection: &Connection,
    workspace: &str,
    offset: usize,
    limit: usize,
) -> Result<ManifestVolumeGcPreview, Error> {
    // Only a superseded, disposable native manifest volume can be previewed.
    // In particular machine/stack scope and pinned data never enter this set.
    let predicate = "r.kind='volume' AND r.workspace=? AND r.state='retired' AND r.scope='spec' AND r.retention='warm' AND r.id GLOB 'manifest-volume:*' AND r.name GLOB 'bosn-v-spec-*' AND EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND u.workspace=? AND u.stack=r.stack AND u.generation=r.generation AND u.state='retired') AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND (u.workspace<>? OR u.stack<>r.stack OR u.generation<>r.generation OR u.state<>'retired')) AND NOT EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id) AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s WHERE s.container_id=r.id OR s.container_id=r.name) AND NOT EXISTS (SELECT 1 FROM volume_creation_intents AS v WHERE v.name=r.name)";
    let limit = limit.clamp(1, MAX_PAGE_SIZE);
    let query_limit = limit.checked_add(1).ok_or(Error::BadRow("page limit"))?;
    let rows = connection.query(&format!("SELECT r.id,r.name,r.generation FROM resources AS r WHERE {predicate} ORDER BY r.id LIMIT ? OFFSET ?"), &[Value::Text(workspace.into()), Value::Text(workspace.into()), Value::Text(workspace.into()), Value::Integer(i64::try_from(query_limit).map_err(|_| Error::BadRow("page limit"))?), Value::Integer(i64::try_from(offset).map_err(|_| Error::BadRow("page offset"))?)], QueryLimits { max_rows: query_limit, max_bytes: 1_048_576 })?;
    let more = rows.len() > limit;
    let candidates = Page {
        items: rows
            .into_iter()
            .take(limit)
            .map(|r| {
                Ok(ManifestVolumeGcCandidate {
                    id: text(&r, 0)?,
                    name: text(&r, 1)?,
                    generation: text(&r, 2)?,
                })
            })
            .collect::<Result<_, Error>>()?,
        next_offset: more
            .then(|| {
                offset
                    .checked_add(limit)
                    .ok_or(Error::BadRow("page offset"))
            })
            .transpose()?,
    };
    let count = |predicate: &str| -> Result<u64, Error> {
        let row = connection.query(&format!("SELECT COUNT(*) FROM resources AS r WHERE r.kind='volume' AND r.workspace=? AND {predicate}"), &[Value::Text(workspace.into())], QueryLimits { max_rows: 1, max_bytes: 1024 })?;
        match row.first().and_then(|r| r.get(0)) {
            Some(Value::Integer(v)) if *v >= 0 => Ok(*v as u64),
            _ => Err(Error::BadRow("manifest volume gc count")),
        }
    };
    let managed = "r.id GLOB 'manifest-volume:*' AND r.name GLOB 'bosn-v-*'";
    Ok(ManifestVolumeGcPreview {
        candidates,
        counts: ManifestVolumeGcPreviewCounts {
            protected_not_retired: count(&format!("{managed} AND r.state<>'retired'"))?,
            protected_policy: count(&format!(
                "{managed} AND r.state='retired' AND (r.scope<>'spec' OR r.retention<>'warm')"
            ))?,
            protected_ambiguous_use: count(&format!(
                "{managed} AND r.state='retired' AND (NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND u.workspace=r.workspace AND u.stack=r.stack AND u.generation=r.generation AND u.state='retired') OR EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND (u.workspace<>r.workspace OR u.stack<>r.stack OR u.generation<>r.generation OR u.state<>'retired')) )"
            ))?,
            protected_lease: count(&format!(
                "{managed} AND r.state='retired' AND EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id)"
            ))?,
            protected_session: count(&format!(
                "{managed} AND r.state='retired' AND EXISTS (SELECT 1 FROM execution_sessions AS s WHERE s.container_id=r.id OR s.container_id=r.name)"
            ))?,
            protected_intent: count(&format!(
                "{managed} AND r.state='retired' AND EXISTS (SELECT 1 FROM volume_creation_intents AS v WHERE v.name=r.name)"
            ))?,
            excluded_unmanaged: count(&format!("NOT ({managed})"))?,
        },
    })
}

pub(crate) fn manifest_volume_gc_candidate_exists(
    connection: &mut impl SetupGcQuery,
    workspace: &str,
    id: &str,
    name: &str,
    generation: &str,
) -> Result<bool, Error> {
    let rows = connection.setup_gc_query(
        "SELECT 1 FROM resources AS r WHERE r.id=? AND r.name=? AND r.generation=? AND r.kind='volume' AND r.workspace=? AND r.state='retired' AND r.scope='spec' AND r.retention='warm' AND r.id GLOB 'manifest-volume:*' AND r.name GLOB 'bosn-v-spec-*' AND EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND u.workspace=? AND u.stack=r.stack AND u.generation=r.generation AND u.state='retired') AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND (u.workspace<>? OR u.stack<>r.stack OR u.generation<>r.generation OR u.state<>'retired')) AND NOT EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id) AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s WHERE s.container_id=r.id OR s.container_id=r.name) AND NOT EXISTS (SELECT 1 FROM volume_creation_intents AS v WHERE v.name=r.name) LIMIT 1",
        &[Value::Text(id.into()), Value::Text(name.into()), Value::Text(generation.into()), Value::Text(workspace.into()), Value::Text(workspace.into()), Value::Text(workspace.into())],
    )?;
    Ok(!rows.is_empty())
}

/// Explicit release is the only path which can remove durable manifest data.
/// It still requires one unambiguous active local ownership/use relationship:
/// a shared, retired, leased, session-owned, or in-progress volume is not a
/// release candidate.  The service re-runs this predicate immediately before
/// each fixed Docker inspection and finalization.
pub(crate) fn manifest_volume_release_preview(
    connection: &Connection,
    workspace: &str,
    offset: usize,
    limit: usize,
) -> Result<Page<ManifestVolumeGcCandidate>, Error> {
    let predicate = "r.kind='volume' AND r.workspace=? AND r.state='active' AND (r.scope IN ('stack','machine') OR r.retention='pinned') AND r.id GLOB 'manifest-volume:*' AND r.name GLOB 'bosn-v-*' AND EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND u.workspace=? AND u.stack=r.stack AND u.generation=r.generation AND u.state='active') AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND (u.workspace<>? OR u.stack<>r.stack OR u.generation<>r.generation OR u.state<>'active')) AND NOT EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id) AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s WHERE s.container_id=r.id OR s.container_id=r.name) AND NOT EXISTS (SELECT 1 FROM volume_creation_intents AS v WHERE v.name=r.name)";
    let limit = limit.clamp(1, MAX_PAGE_SIZE);
    let query_limit = limit.checked_add(1).ok_or(Error::BadRow("page limit"))?;
    let rows = connection.query(
        &format!("SELECT r.id,r.name,r.generation FROM resources AS r WHERE {predicate} ORDER BY r.id LIMIT ? OFFSET ?"),
        &[
            Value::Text(workspace.into()),
            Value::Text(workspace.into()),
            Value::Text(workspace.into()),
            Value::Integer(i64::try_from(query_limit).map_err(|_| Error::BadRow("page limit"))?),
            Value::Integer(i64::try_from(offset).map_err(|_| Error::BadRow("page offset"))?),
        ],
        QueryLimits { max_rows: query_limit, max_bytes: 1_048_576 },
    )?;
    let more = rows.len() > limit;
    Ok(Page {
        items: rows
            .into_iter()
            .take(limit)
            .map(|r| {
                Ok(ManifestVolumeGcCandidate {
                    id: text(&r, 0)?,
                    name: text(&r, 1)?,
                    generation: text(&r, 2)?,
                })
            })
            .collect::<Result<_, Error>>()?,
        next_offset: more
            .then(|| {
                offset
                    .checked_add(limit)
                    .ok_or(Error::BadRow("page offset"))
            })
            .transpose()?,
    })
}

pub(crate) fn manifest_volume_release_candidate_exists(
    connection: &mut impl SetupGcQuery,
    workspace: &str,
    id: &str,
    name: &str,
    generation: &str,
) -> Result<bool, Error> {
    let rows = connection.setup_gc_query(
        "SELECT 1 FROM resources AS r WHERE r.id=? AND r.name=? AND r.generation=? AND r.kind='volume' AND r.workspace=? AND r.state='active' AND (r.scope IN ('stack','machine') OR r.retention='pinned') AND r.id GLOB 'manifest-volume:*' AND r.name GLOB 'bosn-v-*' AND EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND u.workspace=? AND u.stack=r.stack AND u.generation=r.generation AND u.state='active') AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id AND (u.workspace<>? OR u.stack<>r.stack OR u.generation<>r.generation OR u.state<>'active')) AND NOT EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id) AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s WHERE s.container_id=r.id OR s.container_id=r.name) AND NOT EXISTS (SELECT 1 FROM volume_creation_intents AS v WHERE v.name=r.name) LIMIT 1",
        &[
            Value::Text(id.into()), Value::Text(name.into()), Value::Text(generation.into()),
            Value::Text(workspace.into()), Value::Text(workspace.into()), Value::Text(workspace.into()),
        ],
    )?;
    Ok(!rows.is_empty())
}

/// Exact predicate shared by missing-drift preview revalidation and its
/// transactional repair. Unlike GC, this covers only the live active
/// generation which has exactly one local setup use. A resource with another
/// use, lease, or execution session is ambiguous and therefore protected.
pub(crate) fn setup_missing_repair_candidate_exists(
    connection: &mut impl SetupGcQuery,
    workspace: &str,
    id: &str,
    name: &str,
    generation: &str,
    state: &str,
) -> Result<bool, Error> {
    let rows = connection.setup_gc_query(
        "SELECT 1 FROM resources AS r WHERE r.id=? AND r.name=? AND r.generation=? \
         AND r.kind='container' AND r.stack='setup' AND r.workspace=? \
         AND r.state=? AND r.scope='machine' \
         AND r.id GLOB 'setup-container:*' AND r.name GLOB 'bosn-setup-*' \
         AND EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id \
            AND u.workspace=? AND u.stack='setup' AND u.generation=? AND u.state=?) \
         AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id \
            AND (u.workspace<>? OR u.stack<>'setup' OR u.generation<>? OR u.state<>?)) \
         AND NOT EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id) \
         AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s \
            WHERE s.container_id=r.id OR s.container_id=r.name) LIMIT 1",
        &[
            Value::Text(id.into()),
            Value::Text(name.into()),
            Value::Text(generation.into()),
            Value::Text(workspace.into()),
            Value::Text(state.into()),
            Value::Text(workspace.into()),
            Value::Text(generation.into()),
            Value::Text(state.into()),
            Value::Text(workspace.into()),
            Value::Text(generation.into()),
            Value::Text(state.into()),
        ],
    )?;
    Ok(!rows.is_empty())
}

pub(crate) trait SetupGcQuery {
    fn setup_gc_query(&mut self, sql: &str, values: &[Value]) -> Result<Vec<Row>, Error>;
}
impl SetupGcQuery for Connection {
    fn setup_gc_query(&mut self, sql: &str, values: &[Value]) -> Result<Vec<Row>, Error> {
        self.query(
            sql,
            values,
            QueryLimits {
                max_rows: 1,
                max_bytes: 1024,
            },
        )
        .map_err(Into::into)
    }
}
impl SetupGcQuery for Transaction<'_> {
    fn setup_gc_query(&mut self, sql: &str, values: &[Value]) -> Result<Vec<Row>, Error> {
        self.query(
            sql,
            values,
            QueryLimits {
                max_rows: 1,
                max_bytes: 1024,
            },
        )
        .map_err(Into::into)
    }
}
