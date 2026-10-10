//! A machine catalog of every registry that ever ran here, so abandoned ones can be reclaimed
//! (#545).
//!
//! Every daemon enrolls its registry id and canonical state directory under the native machine
//! state directory, whatever `--state-dir` it was given. A temporary state directory that is later
//! deleted leaves Docker objects whose complete labels name a registry nothing will ever open
//! again; the current registry correctly treats them as foreign, so before this they leaked
//! forever.
//!
//! Only the machine daemon (the one whose state directory *is* the native one) acts on the
//! catalog, and only for a registry whose cataloged state directory no longer holds a
//! `registry.sqlite3`. Such objects are then judged as if this registry owned them, under every
//! ordinary gate: running or mounted objects, pins and age gates still hold them. A registry whose
//! database still exists is a live peer, and its objects stay foreign. An object naming a registry
//! the catalog never saw (older than this catalog) stays foreign too: a label is not proof that
//! its registry is gone.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const SCHEMA: u32 = 1;
/// The catalog is pruned as abandoned registries empty out; this only bounds a pathological one.
const MAX_ENTRIES: usize = 4096;
const MAX_ENTRY_BYTES: u64 = 16 * 1024;
const DIRECTORY: &str = "registries";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    schema: u32,
    registry_id: String,
    state_dir: PathBuf,
}

/// The machine catalog, or `None` in unit tests, which must never enroll in the developer's.
fn machine_root() -> Option<PathBuf> {
    if cfg!(test) {
        None
    } else {
        Some(crate::mcp::native_state_dir().join(DIRECTORY))
    }
}

/// Record this daemon's registry in the machine catalog.
pub(crate) fn enroll(state_dir: &Path, registry_id: &str) -> Result<(), String> {
    match machine_root() {
        Some(root) => enroll_at(&root, state_dir, registry_id),
        None => Ok(()),
    }
}

fn enroll_at(root: &Path, state_dir: &Path, registry_id: &str) -> Result<(), String> {
    if !valid_registry_id(registry_id) {
        return Err(format!("registry id {registry_id:?} is not catalogable"));
    }
    let state_dir = std::fs::canonicalize(state_dir).map_err(|error| error.to_string())?;
    let path = root.join(format!("{registry_id}.json"));
    if let Some(existing) = read_entry(&path)
        && existing.state_dir == state_dir
    {
        return Ok(());
    }
    std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let entry = Entry {
        schema: SCHEMA,
        registry_id: registry_id.to_owned(),
        state_dir,
    };
    let text = serde_json::to_vec(&entry).map_err(|error| error.to_string())?;
    // Write-then-rename so a reader never sees half an entry.
    let staging = root.join(format!(".{registry_id}.{}.tmp", std::process::id()));
    std::fs::write(&staging, text).map_err(|error| error.to_string())?;
    std::fs::rename(&staging, &path).map_err(|error| {
        let _ = std::fs::remove_file(&staging);
        error.to_string()
    })
}

/// Registries this pass may reclaim for: empty unless `state_dir` is the machine state directory.
pub(super) fn abandoned(state_dir: &Path, our_registry: Option<&str>) -> BTreeSet<String> {
    let (Some(root), Some(ours)) = (machine_root(), our_registry) else {
        return BTreeSet::new();
    };
    let machine = crate::mcp::native_state_dir();
    let is_machine = matches!(
        (std::fs::canonicalize(state_dir), std::fs::canonicalize(&machine)),
        (Ok(left), Ok(right)) if left == right
    );
    if is_machine {
        abandoned_at(&root, ours)
    } else {
        BTreeSet::new()
    }
}

fn abandoned_at(root: &Path, ours: &str) -> BTreeSet<String> {
    let Ok(listing) = std::fs::read_dir(root) else {
        return BTreeSet::new();
    };
    let mut found = BTreeSet::new();
    for item in listing.flatten().take(MAX_ENTRIES) {
        let path = item.path();
        let Some(stem) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        let Some(entry) = read_entry(&path) else {
            continue;
        };
        // The file name, the entry and the label must all name the same registry.
        if entry.registry_id != stem || entry.registry_id == ours || entry.schema != SCHEMA {
            continue;
        }
        // Only a definite "not there" abandons it; an unreadable path is not evidence.
        if matches!(
            entry.state_dir.join("registry.sqlite3").try_exists(),
            Ok(false)
        ) {
            found.insert(entry.registry_id);
        }
    }
    found
}

/// Drop each abandoned registry that no container, volume or image names any more.
///
/// Every read must succeed and come back empty; anything else keeps the entry for next time.
pub(super) fn forget_emptied(engine: &bosn_engine::DockerEngine, abandoned: &BTreeSet<String>) {
    let Some(root) = machine_root() else {
        return;
    };
    let options = bosn_engine::RunOptions::bounded(super::RETENTION_READ_DEADLINE, 64 * 1024);
    for registry_id in abandoned {
        let filter = format!("{}={registry_id}", bosn_core::LABEL_REGISTRY);
        let empty = |read: Result<bosn_engine::CensusRead, bosn_engine::CommandError>| matches!(read, Ok(bosn_engine::CensusRead::Document(text)) if text.trim().is_empty());
        if empty(engine.container_ids_with_label(&filter, options))
            && empty(engine.volume_names_with_label(&filter, options))
            && empty(engine.image_ids_with_label(&filter, options))
        {
            forget_at(&root, registry_id);
        }
    }
}

fn forget_at(root: &Path, registry_id: &str) {
    if valid_registry_id(registry_id) {
        let _ = std::fs::remove_file(root.join(format!("{registry_id}.json")));
    }
}

fn read_entry(path: &Path) -> Option<Entry> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ENTRY_BYTES {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A registry id is a UUID; anything else never becomes a file name.
fn valid_registry_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = "11111111-2222-4333-8444-555555555555";
    const GONE: &str = "aaaaaaaa-2222-4333-8444-555555555555";
    const LIVE: &str = "bbbbbbbb-2222-4333-8444-555555555555";

    fn registry_dir(parent: &Path, name: &str, id: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        bosn_registry::Registry::create_writer(dir.join("registry.sqlite3"), id).unwrap();
        dir
    }

    #[test]
    fn only_a_registry_whose_database_is_gone_is_abandoned() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let root = temporary.path().join("catalog");
        let gone = registry_dir(temporary.path(), "gone", GONE);
        let live = registry_dir(temporary.path(), "live", LIVE);
        let ours = registry_dir(temporary.path(), "ours", OURS);
        for (dir, id) in [(&gone, GONE), (&live, LIVE), (&ours, OURS)] {
            enroll_at(&root, dir, id).unwrap();
        }
        assert!(
            abandoned_at(&root, OURS).is_empty(),
            "every registry is alive"
        );
        std::fs::remove_dir_all(&gone).unwrap();
        assert_eq!(abandoned_at(&root, OURS), BTreeSet::from([GONE.to_owned()]));
        // Our own entry is never abandoned, even if its path were wrong.
        std::fs::remove_dir_all(&ours).unwrap();
        assert_eq!(abandoned_at(&root, OURS), BTreeSet::from([GONE.to_owned()]));
        forget_at(&root, GONE);
        assert!(abandoned_at(&root, OURS).is_empty());
    }

    #[test]
    fn a_mismatched_or_malformed_entry_proves_nothing() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let root = temporary.path().join("catalog");
        std::fs::create_dir_all(&root).unwrap();
        let missing = temporary.path().join("missing");
        // The file name and the entry disagree.
        std::fs::write(
            root.join(format!("{GONE}.json")),
            serde_json::json!({"schema": 1, "registry_id": LIVE, "state_dir": missing}).to_string(),
        )
        .unwrap();
        // Unknown fields are refused.
        std::fs::write(
            root.join(format!("{LIVE}.json")),
            serde_json::json!({"schema": 1, "registry_id": LIVE, "state_dir": missing, "x": 1})
                .to_string(),
        )
        .unwrap();
        assert!(abandoned_at(&root, OURS).is_empty());
        assert!(enroll_at(&root, temporary.path(), "../escape").is_err());
    }
}
