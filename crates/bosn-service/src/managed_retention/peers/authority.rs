//! Publish stable writer authority before the daemon accepts any workload.

use super::*;
mod pending;

pub(super) fn promote(
    registry: &mut Registry,
    state: &Path,
    directory: &Path,
) -> Result<(), String> {
    let original = state.join("registry.sqlite3");
    let alias = directory.join("registry.sqlite3");
    let stable_dir = directory.join("authority");
    crate::ipc::ensure_owner_private_directory(&stable_dir).map_err(|error| error.to_string())?;
    let stable = std::fs::canonicalize(stable_dir)
        .map_err(|error| error.to_string())?
        .join("registry.sqlite3");
    let published = alias
        .with_extension("authority.json")
        .try_exists()
        .map_err(|error| error.to_string())?;
    if published {
        let target = Registry::resolve_authority(&alias).map_err(|error| error.to_string())?;
        if target != stable {
            return Err("machine registry authority location mismatch".into());
        }
        if Registry::resolve_authority(&original).map_err(|error| error.to_string())? != stable {
            registry
                .resume_authoritative_writer(&stable)
                .map_err(|error| error.to_string())?;
        }
    } else {
        if Registry::resolve_authority(&original).map_err(|error| error.to_string())? != original {
            return Err("workspace authority exists but machine publication is missing".into());
        }
        pending::prepare(
            directory,
            &original,
            &stable,
            &registry.registry_id().map_err(|error| error.to_string())?,
        )?;
        if !alias.try_exists().map_err(|error| error.to_string())? {
            registry
                .backup_ownership(&alias)
                .map_err(|error| error.to_string())?;
        }
        if stable.try_exists().map_err(|error| error.to_string())? {
            registry
                .refresh_unpublished_copy(&stable)
                .map_err(|error| error.to_string())?;
        } else {
            registry
                .relocate_writer(&stable)
                .map_err(|error| error.to_string())?;
        }
        // Publish machine recovery proof first. If the second publication
        // fails, restart resumes this database rather than copying old state.
        registry
            .publish_authority(&alias, &stable)
            .map_err(|error| error.to_string())?;
    }
    registry
        .publish_authority(&original, &stable)
        .map_err(|error| error.to_string())?;
    recovery::remember_configuration(directory, state)?;
    pending::finish(
        directory,
        &original,
        &stable,
        &registry.registry_id().map_err(|error| error.to_string())?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_recovers_each_completed_authority_publication_boundary() {
        for boundary in 0..=6 {
            let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
            let state = root.path().join("workspace");
            let machine = root.path().join("machine");
            let stable_dir = machine.join("authority");
            std::fs::create_dir(&state).unwrap();
            std::fs::create_dir_all(&stable_dir).unwrap();
            let original = state.join("registry.sqlite3");
            let alias = machine.join("registry.sqlite3");
            let stable = stable_dir.join("registry.sqlite3");
            let owner = "11111111-2222-4333-8444-555555555555";
            let mut writer = Registry::create_writer(&original, owner).unwrap();
            let mut transaction = writer.begin_immediate().unwrap();
            transaction
                .append_event(1.0, "before.handoff", "ownership receipt")
                .unwrap();
            transaction.commit().unwrap();
            if boundary >= 1 {
                pending::prepare(&machine, &original, &stable, owner).unwrap();
            }
            if boundary >= 2 {
                writer.backup_ownership(&alias).unwrap();
            }
            if boundary >= 3 {
                writer.relocate_writer(&stable).unwrap();
            }
            if boundary >= 4 {
                writer.publish_authority(&alias, &stable).unwrap();
                let mut transaction = writer.begin_immediate().unwrap();
                transaction
                    .append_event(2.0, "stable.commit", "published ownership")
                    .unwrap();
                transaction.commit().unwrap();
            }
            if boundary >= 5 {
                writer.publish_authority(&original, &stable).unwrap();
            }
            if boundary >= 6 {
                recovery::remember_configuration(&machine, &state).unwrap();
            }
            drop(writer);
            let mut restarted = Registry::open_writer(&original).unwrap();
            promote(&mut restarted, &state, &machine).unwrap();
            assert_eq!(
                restarted.registry_id().unwrap(),
                owner,
                "boundary {boundary}"
            );
            assert_eq!(Registry::resolve_authority(&original).unwrap(), stable);
            assert_eq!(Registry::resolve_authority(&alias).unwrap(), stable);
            let events = Registry::open_read_only(&alias)
                .unwrap()
                .events(0, 64)
                .unwrap();
            assert!(
                events
                    .items
                    .iter()
                    .any(|event| event.kind == "before.handoff"),
                "boundary {boundary}"
            );
            if boundary >= 4 {
                assert!(
                    events
                        .items
                        .iter()
                        .any(|event| event.kind == "stable.commit"),
                    "boundary {boundary}"
                );
            }
            assert!(Registry::open_writer(&original).is_err());
            assert!(Registry::open_writer(&stable).is_err());
            assert!(pending::prepare(&machine, &original, &stable, owner).is_err());
        }
    }

    #[test]
    fn interrupted_unpublished_copy_is_refreshed_from_latest_original_commit() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("workspace");
        let machine = root.path().join("machine");
        let stable_dir = machine.join("authority");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir_all(&stable_dir).unwrap();
        let original = state.join("registry.sqlite3");
        let stable = stable_dir.join("registry.sqlite3");
        let alias = machine.join("registry.sqlite3");
        let owner = "11111111-2222-4333-8444-555555555555";
        let mut writer = Registry::create_writer(&original, owner).unwrap();
        pending::prepare(&machine, &original, &stable, owner).unwrap();
        writer.backup_ownership(&alias).unwrap();
        writer.backup_ownership(&stable).unwrap();
        let mut transaction = writer.begin_immediate().unwrap();
        transaction
            .append_event(1.0, "original.newer", "must survive retry")
            .unwrap();
        transaction.commit().unwrap();
        drop(writer);
        let mut restarted = Registry::open_writer(&original).unwrap();
        promote(&mut restarted, &state, &machine).unwrap();
        assert!(
            Registry::open_read_only(&alias)
                .unwrap()
                .events(0, 64)
                .unwrap()
                .items
                .iter()
                .any(|event| event.kind == "original.newer")
        );
        assert!(pending::prepare(&machine, &original, &stable, owner).is_err());
    }

    #[test]
    fn restart_finishes_workspace_redirect_without_losing_stable_commits() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("workspace");
        let machine = root.path().join("machine");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&machine).unwrap();
        let original = state.join("registry.sqlite3");
        let alias = machine.join("registry.sqlite3");
        let authority = machine.join("authority");
        std::fs::create_dir(&authority).unwrap();
        let stable = authority.join("registry.sqlite3");
        let owner = "11111111-2222-4333-8444-555555555555";
        let mut writer = Registry::create_writer(&original, owner).unwrap();
        writer.backup_ownership(&alias).unwrap();
        writer.relocate_writer(&stable).unwrap();
        writer.publish_authority(&alias, &stable).unwrap();
        let mut transaction = writer.begin_immediate().unwrap();
        transaction
            .append_event(1.0, "authority.commit", "survives restart")
            .unwrap();
        transaction.commit().unwrap();
        drop(writer);
        let mut restarted = Registry::open_writer(&original).unwrap();
        promote(&mut restarted, &state, &machine).unwrap();
        let reader = Registry::open_read_only(&original).unwrap();
        assert!(
            reader
                .events(0, 64)
                .unwrap()
                .items
                .iter()
                .any(|event| event.kind == "authority.commit")
        );
        assert!(Registry::open_writer(&original).is_err());
        assert!(Registry::open_writer(&stable).is_err());
        drop(restarted);
        let mut restarted = Registry::open_writer(&original).unwrap();
        promote(&mut restarted, &state, &machine).unwrap();
        assert!(
            Registry::open_read_only(&alias)
                .unwrap()
                .events(0, 64)
                .unwrap()
                .items
                .iter()
                .any(|event| event.kind == "authority.commit")
        );
    }
}
