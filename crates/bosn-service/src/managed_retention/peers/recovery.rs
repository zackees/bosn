//! Recover ownership only from a clean export with the exact catalog identity.

use super::*;

pub(super) fn resolve(root: &Path, entry: &Entry) -> Result<PathBuf, String> {
    let backup = root.join(format!("{}.ownership", entry.registry_id));
    let machine_database = backup.join("registry.sqlite3");
    if machine_database
        .with_extension("authority.json")
        .try_exists()
        .map_err(|error| error.to_string())?
    {
        // A published authority is the live SQLite database, not a possibly
        // stale export. SQLite recovery preserves committed WAL transactions
        // even if the daemon died before marking an old export clean.
        let database =
            Registry::resolve_authority(&machine_database).map_err(|error| error.to_string())?;
        let registry = Registry::open_read_only(database).map_err(|error| error.to_string())?;
        if registry.registry_id().map_err(|error| error.to_string())? != entry.registry_id {
            return Err("machine authority registry identity mismatch".into());
        }
        if entry
            .state_dir
            .join("registry.sqlite3")
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            let original =
                std::fs::canonicalize(&entry.state_dir).map_err(|error| error.to_string())?;
            let source = Registry::open_read_only(original.join("registry.sqlite3"))
                .map_err(|error| error.to_string())?;
            if original != entry.state_dir
                || source.registry_id().map_err(|error| error.to_string())? != entry.registry_id
            {
                return Err("original machine authority identity changed".into());
            }
            remember_configuration(&backup, &original)?;
        }
        validate_configuration(&backup)?;
        return Ok(backup);
    }
    if entry
        .state_dir
        .join("registry.sqlite3")
        .try_exists()
        .map_err(|error| error.to_string())?
    {
        let canonical =
            std::fs::canonicalize(&entry.state_dir).map_err(|error| error.to_string())?;
        if canonical != entry.state_dir {
            return Err(format!(
                "registry {}: state directory identity changed",
                entry.registry_id
            ));
        }
        let registry = Registry::open_read_only(canonical.join("registry.sqlite3"))
            .map_err(|error| error.to_string())?;
        if registry.registry_id().map_err(|error| error.to_string())? != entry.registry_id {
            return Err(format!(
                "registry {}: catalog identity mismatch",
                entry.registry_id
            ));
        }
        if backup.is_dir() {
            remember_configuration(&backup, &canonical)?;
        }
        return Ok(canonical);
    }
    let status = backup.join("ownership-status");
    let metadata = std::fs::symlink_metadata(&status).map_err(|error| {
        format!(
            "registry {}: missing ownership snapshot: {error}",
            entry.registry_id
        )
    })?;
    if !metadata.is_file()
        || metadata.len() != 6
        || std::fs::read(status).map_err(|error| error.to_string())? != b"clean\n"
    {
        return Err(format!(
            "registry {}: ownership snapshot dirty or invalid; reconciliation required",
            entry.registry_id
        ));
    }
    let registry = Registry::open_read_only(backup.join("registry.sqlite3"))
        .map_err(|error| error.to_string())?;
    if registry.registry_id().map_err(|error| error.to_string())? != entry.registry_id {
        return Err(format!(
            "registry {}: ownership snapshot identity mismatch",
            entry.registry_id
        ));
    }
    validate_configuration(&backup)?;
    Ok(backup)
}

fn validate_configuration(backup: &Path) -> Result<(), String> {
    let path = backup.join("retention.toml");
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("snapshot retention setting unavailable: {error}"))?;
    if !metadata.is_file() || metadata.len() > 23 {
        return Err("snapshot retention setting is not a bounded regular file".into());
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    if bytes != b"auto_retention = true\n" && bytes != b"auto_retention = false\n" {
        return Err("snapshot retention setting invalid; reconciliation required".into());
    }
    Ok(())
}

pub(super) fn remember_configuration(backup: &Path, state: &Path) -> Result<(), String> {
    let value = if automatic_retention_enabled(state) {
        b"auto_retention = true\n".as_slice()
    } else {
        b"auto_retention = false\n".as_slice()
    };
    let staging =
        kernal_api::platform::fs::TemporaryDirectory::in_directory(backup, "configuration-")
            .map_err(|error| error.to_string())?;
    let path = staging.path().join("retention.toml");
    let mut file =
        kernal_api::platform::fs::create_private_file(&path).map_err(|error| error.to_string())?;
    file.write_all(value)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    drop(file);
    std::fs::rename(path, backup.join("retention.toml")).map_err(|error| error.to_string())?;
    kernal_api::platform::fs::sync_directory(backup).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_machine_authority_survives_original_loss_and_dirty_export() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("original");
        std::fs::create_dir(&state).unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let source = state.join("registry.sqlite3");
        let mut writer = Registry::create_writer(&source, owner).unwrap();
        let backup = root.path().join(format!("{owner}.ownership"));
        std::fs::create_dir(&backup).unwrap();
        let alias = backup.join("registry.sqlite3");
        writer.backup_ownership(&alias).unwrap();
        let stable_dir = backup.join("authority");
        std::fs::create_dir(&stable_dir).unwrap();
        let stable = stable_dir.join("registry.sqlite3");
        writer.relocate_writer(&stable).unwrap();
        writer.publish_authority(&alias, &stable).unwrap();
        std::fs::write(backup.join("ownership-status"), b"dirty\n").unwrap();
        std::fs::write(backup.join("retention.toml"), b"auto_retention = false\n").unwrap();
        drop(writer);
        std::fs::rename(&state, root.path().join("gone")).unwrap();
        let entry = Entry {
            schema: 1,
            registry_id: owner.into(),
            state_dir: state,
        };
        assert_eq!(resolve(root.path(), &entry).unwrap(), backup);
        assert!(!automatic_retention_enabled(&backup));
        assert_eq!(Registry::resolve_authority(&alias).unwrap(), stable);
    }

    #[test]
    fn recovered_configuration_requires_exact_recorded_policy() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        assert!(validate_configuration(root.path()).is_err());
        let path = root.path().join("retention.toml");
        for value in ["auto_retention = true\n", "auto_retention = false\n"] {
            std::fs::write(&path, value).unwrap();
            validate_configuration(root.path()).unwrap();
        }
        for value in [
            "",
            "auto_retention = fals\n",
            "auto_retention = true\n# changed\n",
        ] {
            std::fs::write(&path, value).unwrap();
            assert!(validate_configuration(root.path()).is_err());
        }
    }
}
