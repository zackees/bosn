//! Durable handoff phase; a published authority must never be overwritten.

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Preparing,
    Published,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handoff {
    schema: u32,
    registry_id: String,
    original: PathBuf,
    stable: PathBuf,
    phase: Phase,
}

pub(super) fn prepare(
    directory: &Path,
    original: &Path,
    stable: &Path,
    owner: &str,
) -> Result<(), String> {
    let path = directory.join("handoff.json");
    if path.try_exists().map_err(|error| error.to_string())? {
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 {
            return Err("registry handoff proof is not a bounded regular file".into());
        }
        let handoff: Handoff =
            serde_json::from_slice(&std::fs::read(path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if handoff.schema != 1
            || handoff.registry_id != owner
            || handoff.original != original
            || handoff.stable != stable
            || handoff.phase != Phase::Preparing
        {
            return Err("registry handoff identity or phase mismatch".into());
        }
        Ok(())
    } else {
        if stable.try_exists().map_err(|error| error.to_string())? {
            return Err("existing stable database has no unpublished handoff proof".into());
        }
        write(directory, original, stable, owner, Phase::Preparing)
    }
}

pub(super) fn finish(
    directory: &Path,
    original: &Path,
    stable: &Path,
    owner: &str,
) -> Result<(), String> {
    write(directory, original, stable, owner, Phase::Published)
}

fn write(
    directory: &Path,
    original: &Path,
    stable: &Path,
    owner: &str,
    phase: Phase,
) -> Result<(), String> {
    let handoff = Handoff {
        schema: 1,
        registry_id: owner.into(),
        original: original.into(),
        stable: stable.into(),
        phase,
    };
    let bytes = serde_json::to_vec(&handoff).map_err(|error| error.to_string())?;
    if bytes.len() > 16 * 1024 {
        return Err("registry handoff proof too large".into());
    }
    let staging = kernal_api::platform::fs::TemporaryDirectory::in_directory(directory, "handoff-")
        .map_err(|error| error.to_string())?;
    let path = staging.path().join("record");
    let mut file =
        kernal_api::platform::fs::create_private_file(&path).map_err(|error| error.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    drop(file);
    std::fs::rename(path, directory.join("handoff.json")).map_err(|error| error.to_string())?;
    kernal_api::platform::fs::sync_directory(directory).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_same_unpublished_handoff_can_be_retried() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let original = root.path().join("original.sqlite3");
        let stable = root.path().join("stable.sqlite3");
        let owner = "11111111-2222-4333-8444-555555555555";
        prepare(root.path(), &original, &stable, owner).unwrap();
        prepare(root.path(), &original, &stable, owner).unwrap();
        assert!(prepare(root.path(), &original, &stable, "different").is_err());
        finish(root.path(), &original, &stable, owner).unwrap();
        assert!(prepare(root.path(), &original, &stable, owner).is_err());
    }
}
