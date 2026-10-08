//! Accounting replay requires complete fresh absence evidence, never inspect errors.
use super::*;

pub(super) fn reconcile(
    engine: &DockerEngine,
    registry: &mut bosn_registry::Registry,
) -> Result<image_recovery::Report, String> {
    let database = registry
        .database_path()
        .map_err(|error| error.to_string())?;
    let state = database
        .parent()
        .ok_or("registry authority has no parent")?;
    let owner = registry.registry_id().map_err(|error| error.to_string())?;
    let mut report = image_recovery::Report::default();
    for receipt in deletion_intents::load(state)? {
        budget::check()?;
        if receipt.labels.registry != owner {
            return Err("deletion intent has foreign registry identity".into());
        }
        if !absent(engine, &receipt)? {
            report.held_count += 1;
            details::push(
                &mut report.held,
                format!(
                    "pending deletion {}: physical object remains",
                    receipt.physical_id
                ),
            );
            continue;
        }
        let mut transaction = registry
            .begin_immediate()
            .map_err(|error| error.to_string())?;
        let removed = transaction
            .delete_removed_ownership(
                &receipt.labels,
                &receipt.physical_name,
                &receipt.physical_id,
                receipt.observed_at,
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        registry
            .publish_ownership_backup()
            .map_err(|error| error.to_string())?;
        let remains = registry
            .has_ownership_incarnation(
                &receipt.labels,
                &receipt.physical_name,
                &receipt.physical_id,
            )
            .map_err(|error| error.to_string())?;
        if removed > 0 || !remains {
            deletion_intents::acknowledge(&receipt)?;
        } else {
            report.held_count += 1;
            details::push(
                &mut report.held,
                format!(
                    "pending deletion {}: ownership accounting remains protected",
                    receipt.physical_id
                ),
            );
        }
    }
    Ok(report)
}

fn absent(engine: &DockerEngine, receipt: &DeletionReceipt) -> Result<bool, String> {
    let args = match receipt.labels.kind {
        ResourceKind::Container => vec!["ps", "-a", "-q", "--no-trunc"],
        ResourceKind::Volume => vec!["volume", "ls", "-q"],
        ResourceKind::Image => vec!["image", "ls", "-a", "-q", "--no-trunc"],
        _ => return Err("unsupported deletion intent kind".into()),
    };
    let result = engine
        .with_args(args)
        .capture(budget::options(RunOptions::bounded(
            RETENTION_READ_DEADLINE,
            RETENTION_OUTPUT_LIMIT,
        )))
        .map_err(|error| error.to_string())?;
    if !result.ok() {
        return Err("deletion absence inventory failed".into());
    }
    let text = std::str::from_utf8(&result.stdout).map_err(|error| error.to_string())?;
    for identity in text.lines() {
        let valid = match receipt.labels.kind {
            ResourceKind::Container => {
                identity.len() == 64 && identity.bytes().all(|byte| byte.is_ascii_hexdigit())
            }
            ResourceKind::Image => identity.strip_prefix("sha256:").is_some_and(|hash| {
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            }),
            ResourceKind::Volume => {
                !identity.is_empty()
                    && identity
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            }
            _ => false,
        };
        if !valid {
            return Err("deletion inventory contains malformed identity".into());
        }
        if identity == receipt.physical_id {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_survives_reopen_and_preserves_protection_or_replacement() {
        for case in 0..7 {
            let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
            let database = root.path().join("registry.sqlite3");
            let owner = "11111111-2222-4333-8444-555555555555";
            let mut registry = bosn_registry::Registry::create_writer(&database, owner).unwrap();
            let resource = bosn_registry::Resource {
                id: "container:fixture".into(),
                kind: ResourceKind::Container,
                name: "fixture".into(),
                stack: "app".into(),
                generation: "generation".into(),
                scope: bosn_core::Scope::Machine,
                workspace: "/workspace".into(),
                created_at: if case == 3 { 2.0 } else { 1.0 },
                last_used: 2.0,
                state: bosn_core::ResourceState::Active,
                retention: if case == 1 {
                    bosn_core::Retention::Pinned
                } else {
                    bosn_core::Retention::Warm
                },
            };
            if case != 2 {
                let mut transaction = registry.begin_immediate().unwrap();
                transaction.put_resource(&resource).unwrap();
                transaction.commit().unwrap();
            }
            let receipt = DeletionReceipt {
                state_dir: root.path().to_path_buf(),
                physical_id: "a".repeat(64),
                physical_name: "fixture".into(),
                labels: bosn_core::ResourceLabels::new(
                    owner,
                    ResourceKind::Container,
                    "app",
                    "generation",
                    bosn_core::Scope::Machine,
                    "/workspace",
                    "1",
                    Some(bosn_core::Retention::Warm),
                )
                .unwrap(),
                observed_at: 3.0,
            };
            deletion_intents::record(&receipt).unwrap();
            drop(registry);
            let mut registry = bosn_registry::Registry::open_writer(&database).unwrap();
            let inventory = match case {
                4 => format!("{}\n", receipt.physical_id),
                5 => "invalid\n".into(),
                _ => String::new(),
            };
            let engine = DockerEngine::synthetic_for_test(
                "/bin/sh",
                [
                    "-c",
                    "printf '%s' \"$BOSN_DELETION_INVENTORY\"; exit \"$BOSN_DELETION_EXIT\"",
                    "fixture",
                ],
            )
            .env("BOSN_DELETION_INVENTORY", inventory)
            .env("BOSN_DELETION_EXIT", if case == 6 { "1" } else { "0" });
            let result = reconcile(&engine, &mut registry);
            assert_eq!(result.is_err(), case >= 5);
            let cleared = matches!(case, 0 | 2 | 3);
            assert_eq!(
                deletion_intents::load(root.path()).unwrap().is_empty(),
                cleared
            );
            assert_eq!(
                registry.resources(0, 64).unwrap().items.len(),
                usize::from(!matches!(case, 0 | 2))
            );
            if case == 1 || case == 4 {
                assert_eq!(result.unwrap().held_count, 1);
            }
        }
    }
}
