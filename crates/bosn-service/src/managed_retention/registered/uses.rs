//! Every shared-use clock contributes to idle age before physical deletion.
use super::*;

pub(super) fn apply(registry: &ReadOnlyRegistry, resources: &mut [Resource]) -> Result<(), String> {
    let indexes: BTreeMap<_, _> = resources
        .iter()
        .enumerate()
        .map(|(index, resource)| (resource.id.clone(), index))
        .collect();
    let mut offset = 0;
    let mut count = 0usize;
    loop {
        super::super::budget::check()?;
        let page = registry
            .resource_uses(offset, 64)
            .map_err(|error| error.to_string())?;
        count = count.saturating_add(page.items.len());
        check_record_count(resources.len().saturating_add(count))?;
        for usage in page.items {
            let index = indexes
                .get(&usage.resource_id)
                .ok_or("resource use has no ownership row")?;
            let resource = &mut resources[*index];
            if !usage.last_used.is_finite() || !resource.last_used.is_finite() {
                return Err("resource use has invalid activity time".into());
            }
            resource.last_used = resource.last_used.max(usage.last_used);
            // Producers leave Active uses after completion, including previous
            // workspaces sharing an immutable image. Their activity ages out;
            // leases, sessions, pins and machine admission protect live work.
        }
        let Some(next) = page.next_offset else { break };
        offset = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosn_core::ResourceState;
    #[test]
    fn shared_use_activity_ages_without_permanent_context_protection() {
        for (state, workspace, held) in [
            (ResourceState::Active, "workspace", false),
            (ResourceState::Active, "other", false),
            (ResourceState::Adopted, "other", false),
            (ResourceState::Retired, "other", false),
        ] {
            let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
            let mut registry = bosn_registry::Registry::create_writer(
                directory.path().join("registry.sqlite3"),
                "11111111-2222-4333-8444-555555555555",
            )
            .unwrap();
            let resource = Resource {
                id: "manifest-volume:shared".into(),
                kind: ResourceKind::Volume,
                name: "shared".into(),
                stack: "stack".into(),
                generation: "generation".into(),
                scope: bosn_core::Scope::Machine,
                workspace: "workspace".into(),
                created_at: 1.0,
                last_used: 2.0,
                state: ResourceState::Active,
                retention: Retention::Warm,
            };
            let mut transaction = registry.begin_immediate().unwrap();
            transaction.put_resource(&resource).unwrap();
            transaction
                .put_resource_use(&bosn_registry::ResourceUse {
                    resource_id: resource.id.clone(),
                    workspace: workspace.into(),
                    stack: "stack".into(),
                    generation: "generation".into(),
                    last_used: 9.0,
                    state,
                })
                .unwrap();
            transaction.commit().unwrap();
            drop(registry);
            let ownership = RegisteredOwnership::load_local(directory.path()).unwrap();
            assert_eq!(ownership.resources[0].last_used, 9.0);
            assert_eq!(ownership.protected_name("shared"), held);
        }
    }
}
