//! Transactional accounting cleanup after an engine confirmed deletion.

use super::*;

impl Registry {
    /// Whether the exact old incarnation still requires accounting cleanup.
    pub fn has_ownership_incarnation(
        &self,
        labels: &bosn_core::ResourceLabels,
        physical_name: &str,
        physical_id: &str,
    ) -> Result<bool, Error> {
        let Ok(created) = labels.created.parse::<f64>() else {
            return Ok(false);
        };
        if !created.is_finite() {
            return Err(Error::BadRow("ownership creation time"));
        }
        let rows = self.connection.query(
            "SELECT r.id FROM resources r WHERE EXISTS (SELECT 1 FROM meta WHERE key='registry_id' AND value=?) \
             AND r.kind=? AND r.stack=? AND r.generation=? AND r.scope=? AND r.workspace=? AND r.created_at=? \
             AND ((r.kind='image' AND r.generation=?) OR (r.kind<>'image' AND r.name=?)) LIMIT 1",
            &[Value::Text(labels.registry.clone()), Value::Text(labels.kind.as_str().into()),
                Value::Text(labels.stack.clone()), Value::Text(labels.generation.clone()),
                Value::Text(labels.scope.as_str().into()), Value::Text(labels.workspace.clone()), Value::Real(created),
                Value::Text(physical_id.into()), Value::Text(physical_name.into())],
            QueryLimits { max_rows: 1, max_bytes: 16 * 1024 })?;
        Ok(!rows.is_empty())
    }
}

impl Immediate<'_> {
    /// Deletes only the unchanged ownership incarnation observed before removal.
    /// The caller must hold machine admission until this transaction commits.
    pub fn delete_removed_ownership(
        &mut self,
        labels: &bosn_core::ResourceLabels,
        physical_name: &str,
        physical_id: &str,
        observed_at: f64,
    ) -> Result<usize, Error> {
        let Ok(created_at) = labels.created.parse::<f64>() else {
            return Ok(0);
        };
        if !observed_at.is_finite()
            || !created_at.is_finite()
            || labels.retention == Retention::Pinned
        {
            return Ok(0);
        }
        let rows = self.transaction.query(
            "SELECT r.id,r.name FROM resources AS r WHERE \
             EXISTS (SELECT 1 FROM meta WHERE key='registry_id' AND value=?) \
             AND r.kind=? AND r.stack=? AND r.generation=? AND r.scope=? \
             AND r.workspace=? AND r.created_at=? AND r.last_used<=? \
             AND r.retention<>'pinned' \
             AND ((r.kind='image' AND r.generation=?) OR (r.kind<>'image' AND r.name=?)) \
             AND NOT EXISTS (SELECT 1 FROM leases AS l WHERE l.resource_id=r.id) \
             AND NOT EXISTS (SELECT 1 FROM execution_sessions AS s \
                 WHERE s.container_id=r.id OR s.container_id=r.name OR s.container_id=?) \
             AND NOT EXISTS (SELECT 1 FROM resource_uses AS u WHERE u.resource_id=r.id \
                 AND (u.last_used>? OR u.workspace<>r.workspace OR u.stack<>r.stack \
                      OR u.generation<>r.generation)) ORDER BY r.id",
            &[
                Value::Text(labels.registry.clone()),
                Value::Text(labels.kind.as_str().into()),
                Value::Text(labels.stack.clone()),
                Value::Text(labels.generation.clone()),
                Value::Text(labels.scope.as_str().into()),
                Value::Text(labels.workspace.clone()),
                Value::Real(created_at),
                Value::Real(observed_at),
                Value::Text(physical_id.into()),
                Value::Text(physical_name.into()),
                Value::Text(physical_id.into()),
                Value::Real(observed_at),
            ],
            QueryLimits {
                max_rows: 1024,
                max_bytes: 1_048_576,
            },
        )?;
        for row in &rows {
            if labels.kind == ResourceKind::Volume {
                self.transaction.execute(
                    "DELETE FROM volume_creation_intents WHERE name=? AND stack=? \
                     AND generation=? AND scope=? AND workspace=?",
                    &[
                        Value::Text(text(row, 1)?),
                        Value::Text(labels.stack.clone()),
                        Value::Text(labels.generation.clone()),
                        Value::Text(labels.scope.as_str().into()),
                        Value::Text(labels.workspace.clone()),
                    ],
                )?;
            }
            self.delete_resource(&text(row, 0)?)?;
        }
        Ok(rows.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_receipt_rechecks_pin_use_incarnation_and_owner() {
        let directory = fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            directory.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let labels = bosn_core::ResourceLabels::new(
            "11111111-2222-4333-8444-555555555555",
            ResourceKind::Container,
            "stack",
            "generation",
            Scope::Machine,
            "workspace",
            "1",
            Some(Retention::Warm),
        )
        .unwrap();
        let mut resource = Resource {
            id: "container:example".into(),
            kind: ResourceKind::Container,
            name: "example".into(),
            stack: "stack".into(),
            generation: "generation".into(),
            scope: Scope::Machine,
            workspace: "workspace".into(),
            created_at: 1.0,
            last_used: 2.0,
            state: ResourceState::Active,
            retention: Retention::Warm,
        };
        for (retention, last_used, created_at, owner) in [
            (
                Retention::Pinned,
                2.0,
                1.0,
                "11111111-2222-4333-8444-555555555555",
            ),
            (
                Retention::Warm,
                4.0,
                1.0,
                "11111111-2222-4333-8444-555555555555",
            ),
            (
                Retention::Warm,
                2.0,
                2.0,
                "11111111-2222-4333-8444-555555555555",
            ),
            (Retention::Warm, 2.0, 1.0, "foreign"),
            (
                Retention::Warm,
                2.0,
                1.0,
                "11111111-2222-4333-8444-555555555555",
            ),
        ] {
            resource.retention = retention;
            resource.last_used = last_used;
            // put_resource preserves original creation time: recreate to test incarnation.
            let mut transaction = registry.begin_immediate().unwrap();
            transaction.delete_resource(&resource.id).unwrap();
            resource.created_at = created_at;
            transaction.put_resource(&resource).unwrap();
            let mut proof = labels.clone();
            proof.registry = owner.into();
            let removed = transaction
                .delete_removed_ownership(&proof, "example", "physical", 3.0)
                .unwrap();
            let expected = usize::from(
                retention == Retention::Warm
                    && last_used == 2.0
                    && created_at == 1.0
                    && owner == "11111111-2222-4333-8444-555555555555",
            );
            assert_eq!(removed, expected);
            transaction.commit().unwrap();
            assert_eq!(
                registry
                    .resource_by_kind_name(ResourceKind::Container, "example")
                    .unwrap()
                    .is_none(),
                expected == 1
            );
        }
    }
    #[test]
    fn removal_receipt_preserves_leased_or_session_owned_rows() {
        let directory = fs::TemporaryDirectory::new().unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let mut registry =
            Registry::create_writer(directory.path().join("registry.sqlite3"), owner).unwrap();
        let labels = bosn_core::ResourceLabels::new(
            owner,
            ResourceKind::Container,
            "stack",
            "generation",
            Scope::Machine,
            "workspace",
            "1",
            None,
        )
        .unwrap();
        let resource = Resource {
            id: "container:example".into(),
            kind: ResourceKind::Container,
            name: "example".into(),
            stack: "stack".into(),
            generation: "generation".into(),
            scope: Scope::Machine,
            workspace: "workspace".into(),
            created_at: 1.0,
            last_used: 2.0,
            state: ResourceState::Active,
            retention: Retention::Warm,
        };
        let mut transaction = registry.begin_immediate().unwrap();
        transaction.put_resource(&resource).unwrap();
        transaction
            .put_lease(&Lease {
                id: "lease".into(),
                resource_id: resource.id.clone(),
                pid: 123,
                proc_start: None,
                acquired_at: 1.0,
                heartbeat_at: 1.0,
                ttl_seconds: 1.0,
            })
            .unwrap();
        assert_eq!(
            transaction
                .delete_removed_ownership(&labels, "example", "physical", 3.0)
                .unwrap(),
            0
        );
        transaction.delete_lease("lease").unwrap();
        transaction
            .put_execution_session(&ExecutionSession {
                id: "session".into(),
                container_id: "physical".into(),
                engine_binary: "docker".into(),
                client_pid: 123,
                client_start: None,
                lease_ids: vec![],
            })
            .unwrap();
        assert_eq!(
            transaction
                .delete_removed_ownership(&labels, "example", "physical", 3.0)
                .unwrap(),
            0
        );
        transaction.delete_execution_session("session").unwrap();
        assert_eq!(
            transaction
                .delete_removed_ownership(&labels, "example", "physical", 3.0)
                .unwrap(),
            1
        );
        transaction.commit().unwrap();
    }
}
