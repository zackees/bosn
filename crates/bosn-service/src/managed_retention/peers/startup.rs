//! Serialize identity creation and locator restoration before owner registration.

use super::*;

pub(crate) fn acquire(state: &Path) -> Result<OwnerGuard, String> {
    let root = machine_root().unwrap_or_else(|| state.to_path_buf());
    acquire_at(&root)
}

fn acquire_at(root: &Path) -> Result<OwnerGuard, String> {
    crate::ipc::ensure_owner_private_directory(root).map_err(|error| error.to_string())?;
    let file = kernal_api::platform::fs::open_lock_file(&root.join("registry-startup.lock"))
        .map_err(|error| error.to_string())?;
    file.try_lock()
        .map_err(|error| format!("registry startup already in progress or unavailable: {error}"))?;
    Ok(OwnerGuard(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn competing_startups_are_excluded_until_authority_handoff_finishes() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let first = acquire_at(root.path()).unwrap();
        assert!(acquire_at(root.path()).is_err());
        drop(first);
        let restarted = acquire_at(root.path()).unwrap();
        assert!(acquire_at(root.path()).is_err());
        drop(restarted);
        assert!(acquire_at(root.path()).is_ok());
    }
}
