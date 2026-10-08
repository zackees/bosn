//! Keep machine authority outside the default directory it protects.
use super::*;

pub(super) fn root(default_state: &Path) -> PathBuf {
    // The native default ends in `bosn` on every supported platform. A sibling
    // keeps authority and admission locks intact when that whole tree is lost.
    default_state.with_file_name("bosn-retention")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_of_default_state_preserves_authority_and_explicit_opt_out() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("bosn");
        let catalog = root(&state).join("retention-registries");
        std::fs::create_dir(&state).unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let mut writer = Registry::create_writer(state.join("registry.sqlite3"), owner).unwrap();
        register_at(&catalog, &state, owner).unwrap();
        let backup = catalog.join(format!("{owner}.ownership"));
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(state.join("retention.toml"), "auto_retention = false\n").unwrap();
        authority::promote(&mut writer, &state, &backup).unwrap();
        drop(writer);
        // Relocation represents losing the complete directory, including all
        // local registry locators. Recovery cannot read the relocated copy.
        std::fs::rename(&state, temporary.path().join("lost-state")).unwrap();
        std::fs::create_dir(&state).unwrap();
        assert!(
            catalog.exists(),
            "default-state loss removed the machine catalog"
        );
        restore::restore(&catalog, &state).unwrap();
        assert!(!automatic_retention_enabled(&state));
        let restarted = Registry::open_writer(state.join("registry.sqlite3")).unwrap();
        assert_eq!(restarted.registry_id().unwrap(), owner);
    }
}
