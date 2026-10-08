//! Restore a lost workspace locator from published machine authority only.

use super::*;

pub(super) fn restore(root: &Path, state: &Path) -> Result<(), String> {
    let original = state.join("registry.sqlite3");
    if original.try_exists().map_err(|error| error.to_string())?
        || original
            .with_extension("authority.json")
            .try_exists()
            .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    let state = std::fs::canonicalize(state).map_err(|error| error.to_string())?;
    // The catalog directory is not a workspace; using it as the exclusion
    // retains entries for the exact workspace path being restored.
    let entries = catalog_entries(root, root)?;
    let matches: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.state_dir == state)
        .collect();
    if matches.is_empty() {
        return Ok(());
    }
    if matches.len() != 1 {
        return Err("multiple registry identities catalog the lost state directory".into());
    }
    let entry = &matches[0];
    let _owner = owner_lock(root, &entry.registry_id)?;
    let backup = root.join(format!("{}.ownership", entry.registry_id));
    let machine_marker = backup.join("registry.authority.json");
    if !machine_marker
        .try_exists()
        .map_err(|error| error.to_string())?
    {
        return Err("lost state directory has no published machine authority".into());
    }
    let configuration = recovery::resolve(root, entry)?;
    let target = Registry::resolve_authority(&backup.join("registry.sqlite3"))
        .map_err(|error| error.to_string())?;
    let registry = Registry::open_retention_snapshot(&target).map_err(|error| error.to_string())?;
    if registry.registry_id().map_err(|error| error.to_string())? != entry.registry_id {
        return Err("restored machine authority identity mismatch".into());
    }
    let staging = kernal_api::platform::fs::TemporaryDirectory::in_directory(&state, "restore-")
        .map_err(|error| error.to_string())?;
    // Preserve the last verified explicit opt-out when its workspace file was
    // lost. An existing operator configuration always takes precedence.
    let config = state.join("retention.toml");
    if !config.try_exists().map_err(|error| error.to_string())? {
        publish(
            staging.path(),
            &config,
            &std::fs::read(configuration.join("retention.toml"))
                .map_err(|error| error.to_string())?,
        )?;
    }
    publish(
        staging.path(),
        &original.with_extension("authority.json"),
        &std::fs::read(machine_marker).map_err(|error| error.to_string())?,
    )?;
    Ok(())
}

fn publish(staging: &Path, destination: &Path, bytes: &[u8]) -> Result<(), String> {
    let staged = staging.join("record");
    let mut file = kernal_api::platform::fs::create_private_file(&staged)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    drop(file);
    std::fs::rename(staged, destination).map_err(|error| error.to_string())?;
    kernal_api::platform::fs::sync_directory(
        destination
            .parent()
            .ok_or("restore destination has no parent")?,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_workspace_without_published_authority_cannot_be_reinitialized() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("workspace");
        let catalog = root.path().join("catalog");
        std::fs::create_dir(&state).unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let writer = Registry::create_writer(state.join("registry.sqlite3"), owner).unwrap();
        register_at(&catalog, &state, owner).unwrap();
        drop(writer);
        std::fs::rename(&state, root.path().join("lost-workspace")).unwrap();
        std::fs::create_dir(&state).unwrap();
        let error = restore(&catalog, &state).unwrap_err();
        assert!(error.contains("no published machine authority"), "{error}");
        assert!(!state.join("registry.sqlite3").exists());
        assert!(!state.join("registry.authority.json").exists());
        assert!(!state.join("retention.toml").exists());
    }

    #[test]
    fn conflicting_catalog_identities_cannot_restore_the_same_workspace() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("workspace");
        let catalog = root.path().join("catalog");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&catalog).unwrap();
        for owner in [
            "11111111-2222-4333-8444-555555555555",
            "11111111-2222-4333-8444-666666666666",
        ] {
            let entry = Entry {
                schema: 1,
                registry_id: owner.into(),
                state_dir: state.clone(),
            };
            std::fs::write(
                catalog.join(format!("{owner}.json")),
                serde_json::to_vec(&entry).unwrap(),
            )
            .unwrap();
        }
        let error = restore(&catalog, &state).unwrap_err();
        assert!(error.contains("multiple registry identities"), "{error}");
        assert!(!state.join("registry.sqlite3").exists());
        assert!(!state.join("registry.authority.json").exists());
        assert!(!state.join("retention.toml").exists());
    }

    #[test]
    fn lost_workspace_recovers_published_identity_and_explicit_opt_out() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("workspace");
        let catalog = root.path().join("catalog");
        std::fs::create_dir(&state).unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let mut writer = Registry::create_writer(state.join("registry.sqlite3"), owner).unwrap();
        register_at(&catalog, &state, owner).unwrap();
        let backup = catalog.join(format!("{owner}.ownership"));
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(state.join("retention.toml"), "auto_retention = false\n").unwrap();
        authority::promote(&mut writer, &state, &backup).unwrap();
        // Losing the whole directory also loses its local authority marker.
        std::fs::rename(&state, root.path().join("lost-workspace")).unwrap();
        std::fs::create_dir(&state).unwrap();
        assert!(
            restore(&catalog, &state).is_err(),
            "active writer must be protected"
        );
        drop(writer);
        restore(&catalog, &state).unwrap();
        assert!(!automatic_retention_enabled(&state));
        let restarted = Registry::open_writer(state.join("registry.sqlite3")).unwrap();
        assert_eq!(restarted.registry_id().unwrap(), owner);
        assert!(!state.join("registry.sqlite3").exists());
        drop(restarted);
        restore(&catalog, &state).unwrap();
    }
}
