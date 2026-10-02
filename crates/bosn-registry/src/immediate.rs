//! Immediate (write) transactions and every durable registry mutation.

use super::*;

pub struct Immediate<'a> {
    pub(crate) transaction: Transaction<'a>,
}
impl<'a> Immediate<'a> {
    /// Atomically records the complete offline verification set and removes
    /// the import gate.  It repeats the durable safety predicates while the
    /// immediate transaction is held: a caller cannot clear the gate from a
    /// stale preview or while legacy liveness/session/creation-intent facts
    /// remain.  This transition intentionally never changes a resource, use,
    /// lease, session, or volume intent.
    pub fn complete_python_v4_reconciliation(
        &mut self,
        proofs: &[ReconciliationProof],
        at: f64,
    ) -> Result<(), Error> {
        if !at.is_finite() {
            return Err(Error::BadRow("reconciliation time"));
        }
        let gate = self.transaction.query(
            "SELECT value FROM meta WHERE key=?",
            &[Value::Text(RECONCILIATION_REQUIRED.into())],
            QueryLimits {
                max_rows: 2,
                max_bytes: 64,
            },
        )?;
        if !matches!(gate.as_slice(), [row] if matches!(row.get(0), Some(Value::Text(value)) if value == "true"))
        {
            return Err(Error::ReconciliationNotRequired);
        }
        let active = self.transaction.query(
            "SELECT id FROM resources WHERE state='active' ORDER BY id",
            &[],
            QueryLimits {
                max_rows: 10_000,
                max_bytes: 1_048_576,
            },
        )?;
        let actual: BTreeSet<String> = active
            .iter()
            .map(|row| text(row, 0))
            .collect::<Result<_, _>>()?;
        let supplied: BTreeSet<String> = proofs
            .iter()
            .map(|proof| proof.resource_id.clone())
            .collect();
        if actual != supplied
            || proofs.len() != supplied.len()
            || proofs
                .iter()
                .any(|proof| proof.engine_id.is_empty() || proof.engine_id.len() > 1024)
        {
            return Err(Error::BadRow("reconciliation proof set"));
        }
        for table in ["leases", "execution_sessions", "volume_creation_intents"] {
            let rows = self.transaction.query(
                &format!("SELECT 1 FROM {table} LIMIT 1"),
                &[],
                QueryLimits {
                    max_rows: 1,
                    max_bytes: 64,
                },
            )?;
            if !rows.is_empty() {
                return Err(Error::BadRow("reconciliation blocker"));
            }
        }
        for proof in proofs {
            let detail =
                serde_json::json!({"resource_id": proof.resource_id, "engine_id": proof.engine_id})
                    .to_string();
            self.append_event(at, "migration.reconcile.verified", &detail)?;
        }
        let inactive = self.transaction.query(
            "SELECT id FROM resources WHERE state<>'active' ORDER BY id",
            &[],
            QueryLimits {
                max_rows: 10_000,
                max_bytes: 1_048_576,
            },
        )?;
        for row in inactive {
            let detail =
                serde_json::json!({"resource_id": text(&row, 0)?, "outcome": "inactive_retained"})
                    .to_string();
            self.append_event(at, "migration.reconcile.explicit", &detail)?;
        }
        self.append_event(
            at,
            "migration.reconcile.completed",
            "all_active_resources_verified",
        )?;
        self.transaction.execute(
            "DELETE FROM meta WHERE key=?",
            &[Value::Text(RECONCILIATION_REQUIRED.into())],
        )?;
        Ok(())
    }
    /// Mark active setup ownership in one exact canonical workspace as done.
    ///
    /// This is intentionally a narrow product transition rather than a
    /// general resource state setter.  It only changes `setup` use rows in
    /// the selected workspace. A machine resource is marked done only after
    /// the transition leaves it with no active uses anywhere, so shared or
    /// foreign ownership remains active. Callers commit this transaction only
    /// when at least one use changed, making repeated completion idempotent.
    pub fn complete_setup_workspace(
        &mut self,
        workspace: &str,
        at: f64,
    ) -> Result<SetupDone, Error> {
        let active = ResourceState::Active.as_str();
        let done = ResourceState::Done.as_str();
        // First capture exactly the affected identities. The temporary table
        // lives only for this transaction/connection and avoids a broad
        // resource-state update after the use rows have changed.
        self.transaction.execute(
            "CREATE TEMP TABLE IF NOT EXISTS bosn_setup_done_ids (id TEXT PRIMARY KEY)",
            &[],
        )?;
        self.transaction
            .execute("DELETE FROM bosn_setup_done_ids", &[])?;
        self.transaction.execute(
            "INSERT INTO bosn_setup_done_ids(id) \
             SELECT DISTINCT resource_id FROM resource_uses \
             WHERE workspace=? AND stack='setup' AND state=?",
            &[Value::Text(workspace.into()), Value::Text(active.into())],
        )?;
        let uses = self.transaction.execute(
            "UPDATE resource_uses SET state=?,last_used=? \
             WHERE workspace=? AND stack='setup' AND state=?",
            &[
                Value::Text(done.into()),
                Value::Real(at),
                Value::Text(workspace.into()),
                Value::Text(active.into()),
            ],
        )?;
        if uses == 0 {
            return Ok(SetupDone::default());
        }
        let resources = self.transaction.execute(
            "UPDATE resources SET state=?,last_used=? \
             WHERE id IN (SELECT id FROM bosn_setup_done_ids) AND state=? \
               AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=resources.id AND u.state=?)",
            &[Value::Text(done.into()), Value::Real(at), Value::Text(active.into()), Value::Text(active.into())],
        )?;
        self.append_event(at, "setup.done", "workspace_setup_completed")?;
        Ok(SetupDone {
            uses_completed: uses as u64,
            resources_completed: resources as u64,
        })
    }
    /// Recheck and remove one exact retired Bosn setup-container candidate.
    ///
    /// This is deliberately not a generic resource deletion operation.  The
    /// caller supplies facts obtained from a bounded GC preview, but the
    /// immediate transaction repeats every protection predicate before it
    /// removes the row (and cascading retired use rows).  A false result is a
    /// stale preview or a newly protected resource, never permission to
    /// broaden selection.
    pub fn finalize_setup_gc_candidate(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
        at: f64,
        event_kind: &str,
    ) -> Result<bool, Error> {
        if !setup_gc_candidate_exists(&mut self.transaction, workspace, id, name, generation)? {
            return Ok(false);
        }
        self.transaction.execute(
            "DELETE FROM resources WHERE id=?",
            &[Value::Text(id.into())],
        )?;
        self.append_event(at, event_kind, "retired_managed_setup_container")?;
        Ok(true)
    }
    /// Recheck one exact retired setup-container candidate and append the
    /// deliberately small stopped event without changing resource lifecycle
    /// state.  The container remains retired and is therefore still eligible
    /// for the separate, confirmation-gated GC apply operation.
    pub fn confirm_setup_retired_container_stopped(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
        at: f64,
    ) -> Result<bool, Error> {
        if !setup_gc_candidate_exists(&mut self.transaction, workspace, id, name, generation)? {
            return Ok(false);
        }
        self.append_event(
            at,
            "setup.ensure.retired_stopped",
            "retired_managed_setup_container",
        )?;
        Ok(true)
    }
    /// Retire one exact, currently-active managed setup container/use after
    /// the daemon has independently proved the exact Docker container absent.
    ///
    /// The predicate is deliberately stricter than a general resource update:
    /// one active `setup` use at the supplied workspace/generation is required,
    /// every other use is refused, and leases/sessions protect the record. The
    /// state transition and compact event are one immediate transaction.
    pub fn repair_missing_setup_container(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
        at: f64,
    ) -> Result<Option<SetupMissingRepair>, Error> {
        if setup_missing_repair_candidate_exists(
            &mut self.transaction,
            workspace,
            id,
            name,
            generation,
            ResourceState::Active.as_str(),
        )? {
            let retired = ResourceState::Retired.as_str();
            let active = ResourceState::Active.as_str();
            let uses = self.transaction.execute(
                "UPDATE resource_uses SET state=?,last_used=? WHERE resource_id=? AND workspace=? AND stack='setup' AND generation=? AND state=?",
                &[
                    Value::Text(retired.into()), Value::Real(at), Value::Text(id.into()),
                    Value::Text(workspace.into()), Value::Text(generation.into()), Value::Text(active.into()),
                ],
            )?;
            let resources = self.transaction.execute(
                "UPDATE resources SET state=?,last_used=? WHERE id=? AND state=?",
                &[
                    Value::Text(retired.into()),
                    Value::Real(at),
                    Value::Text(id.into()),
                    Value::Text(active.into()),
                ],
            )?;
            if uses != 1 || resources != 1 {
                return Err(Error::BadRow("missing setup repair transition"));
            }
            self.append_event(
                at,
                "setup.reconcile.missing_repaired",
                "missing_managed_setup_container",
            )?;
            return Ok(Some(SetupMissingRepair::Repaired));
        }
        // A repeated token is harmless.  We only report this idempotent result
        // when the same narrow ownership shape is already retired; all other
        // mutations, including a new lease/session or foreign use, stay stale.
        if setup_missing_repair_candidate_exists(
            &mut self.transaction,
            workspace,
            id,
            name,
            generation,
            ResourceState::Retired.as_str(),
        )? {
            return Ok(Some(SetupMissingRepair::AlreadyRepaired));
        }
        Ok(None)
    }
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
    /// Retire superseded Bosn setup application-container ownership for one
    /// exact workspace/stack generation boundary.
    ///
    /// This deliberately has no engine effects.  It is only durable registry
    /// accounting and is intended to run in the same immediate transaction
    /// that records the succeeding generation.  Images are intentionally
    /// excluded: an inspected Docker image can be shared by unrelated setup
    /// documents and workspaces.  The `setup-container:` identity namespace
    /// prevents this product-specific rollover from changing arbitrary
    /// container records which happen to use the same stack name.
    pub fn retire_prior_setup_container_generations(
        &mut self,
        workspace: &str,
        stack: &str,
        generation: &str,
    ) -> Result<(), Error> {
        // This is a product-specific transition, not a generic container
        // lifecycle API. An executor seam or future caller cannot extend it
        // to another stack merely by supplying a different string.
        if stack != "setup" {
            return Ok(());
        }
        let retired = ResourceState::Retired.as_str();
        let active = ResourceState::Active.as_str();
        let container = ResourceKind::Container.as_str();

        // Retire the use records first while the candidate set is still the
        // active, exact-scope set. A container with an active use outside
        // this exact workspace/stack is deliberately excluded altogether:
        // `resources.state` is machine-scoped, so changing it would leak this
        // local rollover into another workspace. Both writes remain invisible
        // unless the caller commits the enclosing immediate transaction.
        self.transaction.execute(
            "UPDATE resource_uses SET state=? \
             WHERE workspace=? AND stack=? AND generation<>? AND state=? \
             AND resource_id IN ( \
                SELECT id FROM resources \
                WHERE kind=? AND stack=? AND workspace=? AND generation<>? \
                  AND state=? AND id GLOB 'setup-container:*' \
                  AND NOT EXISTS ( \
                    SELECT 1 FROM resource_uses AS other \
                    WHERE other.resource_id=resources.id AND other.state=? \
                      AND (other.workspace<>? OR other.stack<>?) \
                  ) \
             )",
            &[
                Value::Text(retired.into()),
                Value::Text(workspace.into()),
                Value::Text(stack.into()),
                Value::Text(generation.into()),
                Value::Text(active.into()),
                Value::Text(container.into()),
                Value::Text(stack.into()),
                Value::Text(workspace.into()),
                Value::Text(generation.into()),
                Value::Text(active.into()),
                Value::Text(active.into()),
                Value::Text(workspace.into()),
                Value::Text(stack.into()),
            ],
        )?;
        self.transaction.execute(
            "UPDATE resources SET state=? \
             WHERE kind=? AND stack=? AND workspace=? AND generation<>? \
               AND state=? AND id GLOB 'setup-container:*' \
               AND NOT EXISTS ( \
                 SELECT 1 FROM resource_uses AS other \
                 WHERE other.resource_id=resources.id AND other.state=? \
                   AND (other.workspace<>? OR other.stack<>?) \
               )",
            &[
                Value::Text(retired.into()),
                Value::Text(container.into()),
                Value::Text(stack.into()),
                Value::Text(workspace.into()),
                Value::Text(generation.into()),
                Value::Text(active.into()),
                Value::Text(active.into()),
                Value::Text(workspace.into()),
                Value::Text(stack.into()),
            ],
        )?;
        Ok(())
    }
    /// Retire superseded native-manifest application-container ownership for
    /// one exact workspace/stack generation boundary.
    ///
    /// This has the same deliberately narrow, registry-only semantics as the
    /// setup-document rollover above, but it is kept in a separate namespace
    /// so a manifest stack can never retire a setup document (or vice versa).
    /// The caller must have durably upserted the succeeding container and
    /// image first in this immediate transaction. Images are intentionally not
    /// retired: an inspected immutable image can be shared by stacks and
    /// workspaces. Execution sessions remain attached to their retired
    /// container record and consequently continue to protect it from the
    /// conservative GC predicate.
    pub fn retire_prior_manifest_container_generations(
        &mut self,
        workspace: &str,
        stack: &str,
        generation: &str,
    ) -> Result<(), Error> {
        let retired = ResourceState::Retired.as_str();
        let active = ResourceState::Active.as_str();
        let container = ResourceKind::Container.as_str();

        // Do not modify a machine-scoped resource if it has an active use
        // outside this exact manifest stack. The `manifest-container:`
        // namespace is written solely by the native manifest executor; it
        // prevents this transition from becoming a generic container API.
        self.transaction.execute(
            "UPDATE resource_uses SET state=? \
             WHERE workspace=? AND stack=? AND generation<>? AND state=? \
             AND resource_id IN ( \
                SELECT id FROM resources \
                WHERE kind=? AND stack=? AND workspace=? AND generation<>? \
                  AND state=? AND (id GLOB 'manifest-container:*' OR id GLOB 'manifest-guest:*') \
                  AND NOT EXISTS ( \
                    SELECT 1 FROM resource_uses AS other \
                    WHERE other.resource_id=resources.id AND other.state=? \
                      AND (other.workspace<>? OR other.stack<>?) \
                  ) \
             )",
            &[
                Value::Text(retired.into()),
                Value::Text(workspace.into()),
                Value::Text(stack.into()),
                Value::Text(generation.into()),
                Value::Text(active.into()),
                Value::Text(container.into()),
                Value::Text(stack.into()),
                Value::Text(workspace.into()),
                Value::Text(generation.into()),
                Value::Text(active.into()),
                Value::Text(active.into()),
                Value::Text(workspace.into()),
                Value::Text(stack.into()),
            ],
        )?;
        self.transaction.execute(
            "UPDATE resources SET state=? \
             WHERE kind=? AND stack=? AND workspace=? AND generation<>? \
               AND state=? AND (id GLOB 'manifest-container:*' OR id GLOB 'manifest-guest:*') \
               AND NOT EXISTS ( \
                 SELECT 1 FROM resource_uses AS other \
                 WHERE other.resource_id=resources.id AND other.state=? \
                   AND (other.workspace<>? OR other.stack<>?) \
               )",
            &[
                Value::Text(retired.into()),
                Value::Text(container.into()),
                Value::Text(stack.into()),
                Value::Text(workspace.into()),
                Value::Text(generation.into()),
                Value::Text(active.into()),
                Value::Text(active.into()),
                Value::Text(workspace.into()),
                Value::Text(stack.into()),
            ],
        )?;
        Ok(())
    }
    /// Retire only superseded disposable manifest volume ownership.  Durable
    /// `stack`/`machine` scope and `pinned` retention are intentionally never
    /// transitioned here: their removal needs a separate explicit release
    /// contract.  This method has no engine effect.
    pub fn retire_prior_manifest_warm_spec_volume_generations(
        &mut self,
        workspace: &str,
        stack: &str,
        keep_names: &[String],
    ) -> Result<(), Error> {
        let retired = ResourceState::Retired.as_str();
        let active = ResourceState::Active.as_str();
        let volume = ResourceKind::Volume.as_str();
        self.transaction.execute(
            "CREATE TEMP TABLE IF NOT EXISTS bosn_manifest_volume_keep (name TEXT PRIMARY KEY)",
            &[],
        )?;
        self.transaction
            .execute("DELETE FROM bosn_manifest_volume_keep", &[])?;
        for name in keep_names {
            self.transaction.execute(
                "INSERT INTO bosn_manifest_volume_keep(name) VALUES(?)",
                &[Value::Text(name.clone())],
            )?;
        }
        self.transaction.execute(
            "UPDATE resource_uses SET state=? WHERE workspace=? AND stack=? AND state=? AND resource_id IN (SELECT id FROM resources WHERE kind=? AND stack=? AND workspace=? AND state=? AND scope='spec' AND retention='warm' AND id GLOB 'manifest-volume:*' AND name NOT IN (SELECT name FROM bosn_manifest_volume_keep) AND NOT EXISTS (SELECT 1 FROM resource_uses AS other WHERE other.resource_id=resources.id AND other.state=? AND (other.workspace<>? OR other.stack<>?)))",
            &[Value::Text(retired.into()), Value::Text(workspace.into()), Value::Text(stack.into()), Value::Text(active.into()), Value::Text(volume.into()), Value::Text(stack.into()), Value::Text(workspace.into()), Value::Text(active.into()), Value::Text(active.into()), Value::Text(workspace.into()), Value::Text(stack.into())],
        )?;
        self.transaction.execute(
            "UPDATE resources SET state=? WHERE kind=? AND stack=? AND workspace=? AND state=? AND scope='spec' AND retention='warm' AND id GLOB 'manifest-volume:*' AND name NOT IN (SELECT name FROM bosn_manifest_volume_keep) AND NOT EXISTS (SELECT 1 FROM resource_uses AS other WHERE other.resource_id=resources.id AND other.state=? AND (other.workspace<>? OR other.stack<>?))",
            &[Value::Text(retired.into()), Value::Text(volume.into()), Value::Text(stack.into()), Value::Text(workspace.into()), Value::Text(active.into()), Value::Text(active.into()), Value::Text(workspace.into()), Value::Text(stack.into())],
        )?;
        Ok(())
    }
    /// Recheck and remove one exact disposable manifest-volume candidate.
    pub fn finalize_manifest_volume_gc_candidate(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
        at: f64,
        event_kind: &str,
    ) -> Result<bool, Error> {
        if !manifest_volume_gc_candidate_exists(
            &mut self.transaction,
            workspace,
            id,
            name,
            generation,
        )? {
            return Ok(false);
        }
        self.transaction.execute(
            "DELETE FROM resources WHERE id=?",
            &[Value::Text(id.into())],
        )?;
        self.append_event(at, event_kind, "retired_manifest_warm_spec_volume")?;
        Ok(true)
    }
    /// Recheck and remove one exact durable manifest-volume candidate after a
    /// caller has separately proved the fixed Docker ownership contract.
    pub fn finalize_manifest_volume_release_candidate(
        &mut self,
        workspace: &str,
        id: &str,
        name: &str,
        generation: &str,
        at: f64,
        event_kind: &str,
    ) -> Result<bool, Error> {
        if !manifest_volume_release_candidate_exists(
            &mut self.transaction,
            workspace,
            id,
            name,
            generation,
        )? {
            return Ok(false);
        }
        self.transaction.execute(
            "DELETE FROM resources WHERE id=?",
            &[Value::Text(id.into())],
        )?;
        self.append_event(at, event_kind, "explicit_durable_manifest_volume_release")?;
        Ok(true)
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
    pub(crate) fn put_event_with_id(&mut self, value: &Event) -> Result<(), Error> {
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
