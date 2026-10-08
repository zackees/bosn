//! Discover registered state directories; require writer exclusion before peer GC.
//!
//! A catalog entry locates ownership proof; it never replaces that proof. Missing
//! databases and changed identities are diagnosed, never adopted or deleted.

use super::*;
use bosn_registry::Registry;
use serde::{Deserialize, Serialize};
use std::{io::Write, path::PathBuf};

const MAX_CATALOG_ENTRIES: usize = 1024;
const MAX_ENTRY_BYTES: u64 = 16 * 1024;
mod authority;
mod bootstrap;

pub(crate) fn promote_authority(
    registry: &mut Registry,
    state: &Path,
    directory: &Path,
) -> Result<(), String> {
    authority::promote(registry, state, directory)
}
mod recovery;
mod restore;
pub(crate) mod startup;

pub(crate) fn restore_lost_state(state: &Path) -> Result<(), String> {
    let Some(root) = catalog_root() else {
        return Ok(());
    };
    if !root.try_exists().map_err(|error| error.to_string())? {
        return Ok(());
    }
    restore::restore(&root, state)
}

pub(crate) fn proves_prior_ownership(
    current: &Path,
    prior: &crate::PriorIdentity,
) -> Result<bool, String> {
    let Some(root) = catalog_root() else {
        return Ok(false);
    };
    bootstrap::proves_at(&root, current, prior)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    schema: u32,
    registry_id: String,
    state_dir: PathBuf,
}

pub(super) fn machine_root() -> Option<PathBuf> {
    // Unit/live Docker fixtures must not enroll their temporary registries in
    // the developer's machine catalog. Catalog tests use explicit roots below.
    if cfg!(test) {
        None
    } else if std::env::var_os("BOSN_TEST_ISOLATED").is_some() {
        std::env::var_os("BOSN_TEST_RETENTION_ROOT").map(PathBuf::from)
    } else {
        Some(crate::mcp::machine_state_dir())
    }
}

fn catalog_root() -> Option<PathBuf> {
    machine_root().map(|root| root.join("retention-registries"))
}

pub(crate) struct OwnerGuard(std::fs::File);

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        // A concurrently spawned child may inherit the open-file description
        // until exec. Explicit unlock ends this owner's authority immediately.
        let _ = self.0.unlock();
    }
}

pub(crate) fn register(state_dir: &Path, owner: &str) -> Result<Option<OwnerGuard>, String> {
    let Some(root) = catalog_root() else {
        return Ok(None);
    };
    register_at(&root, state_dir, owner)?;
    owner_lock(&root, owner).map(Some)
}

fn owner_lock(root: &Path, owner: &str) -> Result<OwnerGuard, String> {
    crate::ipc::ensure_owner_private_directory(root).map_err(|error| error.to_string())?;
    let file = kernal_api::platform::fs::open_lock_file(&root.join(format!("{owner}.lock")))
        .map_err(|error| error.to_string())?;
    file.try_lock().map_err(|error| {
        format!("registry {owner}: machine ownership lock held or unavailable: {error}")
    })?;
    Ok(OwnerGuard(file))
}

pub(crate) fn backup_directory(state: &Path, owner: &str) -> Result<Option<PathBuf>, String> {
    let Some(root) = catalog_root() else {
        return Ok(None);
    };
    let directory = root.join(format!("{owner}.ownership"));
    crate::ipc::ensure_owner_private_directory(&directory).map_err(|error| error.to_string())?;
    recovery::remember_configuration(&directory, state)?;
    Ok(Some(directory))
}

fn register_at(root: &Path, state_dir: &Path, owner: &str) -> Result<(), String> {
    let registry = Registry::open_read_only(state_dir.join("registry.sqlite3"))
        .map_err(|error| error.to_string())?;
    if registry.registry_id().map_err(|error| error.to_string())? != owner {
        return Err("catalog registration identity mismatch".into());
    }
    // Use the validated registry UUID, never a caller-controlled path component.
    crate::ipc::ensure_owner_private_directory(root).map_err(|error| error.to_string())?;
    let path = root.join(format!("{owner}.json"));
    let state_dir = std::fs::canonicalize(state_dir).map_err(|error| error.to_string())?;
    if path.exists() {
        let previous = read_entry(&path)?;
        if previous.registry_id == owner && previous.state_dir == state_dir {
            return Ok(());
        }
        return Err("registry identity already catalogs a different state directory".into());
    }
    let count = std::fs::read_dir(root)
        .map_err(|error| error.to_string())?
        .filter(|entry| {
            entry.as_ref().is_ok_and(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            })
        })
        .count();
    if count >= MAX_CATALOG_ENTRIES {
        return Err("retention registry catalog reached its entry ceiling".into());
    }
    let entry = Entry {
        schema: 1,
        registry_id: owner.into(),
        state_dir,
    };
    let staging = kernal_api::platform::fs::TemporaryDirectory::in_directory(root, "registration-")
        .map_err(|error| error.to_string())?;
    let staged = staging.path().join("entry.json");
    let bytes = serde_json::to_vec(&entry).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return Err("retention registry catalog entry too large".into());
    }
    let mut file = std::fs::File::create(&staged).map_err(|error| error.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    drop(file);
    std::fs::rename(&staged, path).map_err(|error| error.to_string())
}

fn read_entry(path: &Path) -> Result<Entry, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_ENTRY_BYTES {
        return Err("retention catalog entry is not a bounded regular file".into());
    }
    let entry: Entry =
        serde_json::from_slice(&std::fs::read(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    if entry.schema != 1
        || !entry.state_dir.is_absolute()
        || path.file_name().and_then(|value| value.to_str())
            != Some(&format!("{}.json", entry.registry_id))
    {
        return Err("retention catalog identity or schema mismatch".into());
    }
    Ok(entry)
}

#[cfg(test)]
fn with_offline_peer<T>(
    root: &Path,
    entry: &Entry,
    action: impl FnOnce(&Path) -> T,
) -> Result<T, String> {
    with_offline_peer_registry(root, entry, false, |state, _| action(state))
}

fn with_offline_peer_registry<T>(
    root: &Path,
    entry: &Entry,
    apply: bool,
    action: impl FnOnce(&Path, Option<&mut Registry>) -> T,
) -> Result<T, String> {
    // This machine-stable lock survives loss of the original state directory.
    // The database writer fence below also protects older daemons.
    let _owner = owner_lock(root, &entry.registry_id)?;
    let canonical = recovery::resolve(root, entry)?;
    // This guard spans every observation, idle stop, and removal. An active
    // daemon keeps its writer lock, and a new daemon cannot start during GC.
    let original = entry.state_dir.join("registry.sqlite3");
    let database = if original.try_exists().map_err(|error| error.to_string())? {
        original
    } else {
        canonical.join("registry.sqlite3")
    };
    if apply {
        let mut registry = Registry::open_writer(database)
            .map_err(|error| format!("registry {} protected: {error}", entry.registry_id))?;
        if registry.registry_id().map_err(|error| error.to_string())? != entry.registry_id {
            return Err(format!(
                "registry {}: database identity changed",
                entry.registry_id
            ));
        }
        return Ok(action(&canonical, Some(&mut registry)));
    }
    let registry = Registry::open_retention_snapshot(database)
        .map_err(|error| format!("registry {} protected: {error}", entry.registry_id))?;
    if registry.registry_id().map_err(|error| error.to_string())? != entry.registry_id {
        return Err(format!(
            "registry {}: database identity changed",
            entry.registry_id
        ));
    }
    let result = action(&canonical, None);
    drop(registry);
    Ok(result)
}

pub(super) fn sweep(
    engine: &DockerEngine,
    current: &Path,
    policy: RetentionPolicy,
    apply: bool,
    outcome: &mut ManagedRetentionOutcome,
) {
    let Some(root) = catalog_root() else { return };
    if let Err(error) = sweep_at(&root, engine, current, policy, apply, outcome) {
        outcome.summary.refused = Some(error);
    }
}

fn sweep_at(
    root: &Path,
    engine: &DockerEngine,
    current: &Path,
    policy: RetentionPolicy,
    apply: bool,
    outcome: &mut ManagedRetentionOutcome,
) -> Result<(), String> {
    let entries = catalog_entries(root, current)?;
    for entry in entries {
        budget::check()?;
        let configuration = recovery::resolve(root, &entry)?;
        if !automatic_retention_enabled(&configuration) {
            outcome.summary.held_total += 1;
            details::push(
                &mut outcome.summary.held,
                format!(
                    "registry {}: automatic retention disabled; use explicit GC in that state directory",
                    entry.registry_id
                ),
            );
            outcome.summary.held.truncate(64);
            continue;
        }
        let prior_receipts = outcome.deletion_receipts.clone();
        let remaining = bosn_core::retention::MAX_MANAGED_REMOVALS
            .saturating_sub(outcome.plan.candidates.len());
        if remaining == 0 && prior_receipts.is_empty() {
            outcome.summary.deferred += 1;
            continue;
        }
        let mut peer_policy = policy;
        peer_policy.max_bytes = policy.max_bytes.map(|ceiling| {
            ceiling
                .saturating_sub(outcome.plan.bytes.max(outcome.summary.removed_bytes))
                .max(0)
        });
        let peer = with_offline_peer_registry(root, &entry, apply, |state, mut registry| {
            let recovery = match registry.as_deref_mut() {
                Some(registry) => match image_recovery::reconcile(engine, registry) {
                    Ok(report) => report,
                    Err(error) => {
                        return refused_outcome(
                            format!("image ownership recovery failed: {error}"),
                            SetupContainerReport::default(),
                        );
                    }
                },
                None => image_recovery::Report::default(),
            };
            let mut peer = idle::run(engine, state, peer_policy, apply, remaining);
            peer.summary.held_total += recovery.held_count;
            for reason in recovery.held {
                details::push(&mut peer.summary.held, reason);
            }
            if let Some(registry) = registry {
                let result = prune_peer_receipts(registry, &prior_receipts)
                    .and_then(|()| prune_peer_receipts(registry, &peer.deletion_receipts));
                if let Err(error) = result {
                    peer.summary.refused = Some(format!(
                        "physical cleanup succeeded but peer registry cleanup failed: {error}"
                    ));
                }
            }
            peer
        });
        match peer {
            Ok(peer) => {
                if let Some(refusal) = &peer.summary.refused {
                    outcome.summary.held_total += 1;
                    details::push(
                        &mut outcome.summary.held,
                        format!("registry {}: {refusal}", entry.registry_id),
                    );
                }
                staged::merge(outcome, peer);
            }
            Err(reason) => {
                outcome.summary.held_total += 1;
                details::push(&mut outcome.summary.held, reason);
            }
        }
        outcome.summary.held.truncate(64);
    }
    Ok(())
}

fn prune_peer_receipts(
    registry: &mut Registry,
    receipts: &[DeletionReceipt],
) -> Result<(), bosn_registry::Error> {
    if receipts.is_empty() {
        return Ok(());
    }
    let owner = registry.registry_id()?;
    let mut resources = Vec::new();
    let mut offset = 0;
    loop {
        let page = registry.resources(offset, 64)?;
        resources.extend(page.items);
        if resources.len() > 65_536 {
            return Err(bosn_registry::Error::BadRow("receipt inventory limit"));
        }
        let Some(next) = page.next_offset else { break };
        offset = next;
    }
    let mut transaction = registry.begin_immediate()?;
    for receipt in receipts {
        for row in resources.iter().filter(|row| {
            row.kind == receipt.labels.kind
                && row.stack == receipt.labels.stack
                && row.generation == receipt.labels.generation
                && row.scope == receipt.labels.scope
                && row.workspace == receipt.labels.workspace
                && row.created_at.is_finite()
                && row.created_at <= receipt.observed_at
                && (row.kind == ResourceKind::Image || row.name == receipt.physical_name)
        }) {
            // Shared physical objects have independent checkpoints in each
            // registry. Exact recorded metadata proves the shared identity;
            // transaction predicates recheck its protection and observed age.
            let mut labels = receipt.labels.clone();
            labels.registry = owner.clone();
            labels.created = row.created_at.to_string();
            transaction.delete_removed_ownership(
                &labels,
                &receipt.physical_name,
                &receipt.physical_id,
                receipt.observed_at,
            )?;
        }
    }
    transaction.commit()?;
    registry.publish_ownership_backup()?;
    for receipt in receipts {
        if receipt.labels.registry == owner {
            deletion_intents::acknowledge(receipt)
                .map_err(|error| bosn_registry::Error::Io(std::io::Error::other(error)))?;
        }
    }
    Ok(())
}

pub(super) fn read_ownership(
    current: &Path,
) -> Result<Vec<registered::RegisteredOwnership>, String> {
    let Some(root) = catalog_root() else {
        return Ok(Vec::new());
    };
    let mut count = 0usize;
    catalog_entries(&root, current)?
        .into_iter()
        .map(|entry| {
            budget::check()?;
            let original = entry.state_dir.join("registry.sqlite3");
            let _owner = if !original.try_exists().map_err(|error| error.to_string())? {
                Some(owner_lock(&root, &entry.registry_id)?)
            } else {
                None
            };
            let state = recovery::resolve(&root, &entry)?;
            let mut ownership = registered::RegisteredOwnership::load_local(&state)?;
            if ownership.owner != entry.registry_id {
                return Err(format!(
                    "registry {}: catalog identity mismatch",
                    entry.registry_id
                ));
            }
            if !automatic_retention_enabled(&state) {
                ownership.protect_all();
            }
            count = count.saturating_add(ownership.record_count());
            registered::check_record_count(count)?;
            Ok(ownership)
        })
        .collect()
}

fn catalog_entries(root: &Path, current: &Path) -> Result<Vec<Entry>, String> {
    let listing = match std::fs::read_dir(root) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("retention catalog unavailable: {error}")),
    };
    let current = std::fs::canonicalize(current).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for path in listing {
        let path = path.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        entries.push(read_entry(&path)?);
        if entries.len() > MAX_CATALOG_ENTRIES {
            return Err("retention catalog census exceeded its ceiling".into());
        }
    }
    entries.sort_by(|left, right| left.registry_id.cmp(&right.registry_id));
    Ok(entries
        .into_iter()
        .filter(|entry| {
            entry.state_dir != current
                && root.join(format!("{}.ownership", entry.registry_id)) != current
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    const OWNER: &str = "11111111-2222-4333-8444-555555555555";

    #[test]
    fn catalog_requires_durable_identity_and_excludes_active_writers() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let writer = Registry::create_writer(state.join("registry.sqlite3"), OWNER).unwrap();
        let catalog = root.path().join("catalog");
        register_at(&catalog, &state, OWNER).unwrap();
        let entry = read_entry(&catalog.join(format!("{OWNER}.json"))).unwrap();
        assert!(
            with_offline_peer(&catalog, &entry, |_| panic!(
                "active foreign registry admitted"
            ))
            .is_err()
        );
        drop(writer);
        with_offline_peer(&catalog, &entry, |state| {
            assert!(
                Registry::open_writer(state.join("registry.sqlite3")).is_err(),
                "a daemon acquired the peer while collection was in progress"
            );
        })
        .unwrap();
        assert!(register_at(&catalog, &state, "00000000-0000-4000-8000-000000000545").is_err());
        let mut changed = entry;
        changed.registry_id = "00000000-0000-4000-8000-000000000545".into();
        assert!(
            with_offline_peer(&catalog, &changed, |_| panic!("changed registry admitted")).is_err()
        );
    }

    #[test]
    fn machine_owner_lock_survives_state_directory_relocation() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let catalog = root.path().join("catalog");
        let owner = owner_lock(&catalog, OWNER).unwrap();
        std::fs::rename(&state, root.path().join("relocated")).unwrap();
        assert!(owner_lock(&catalog, OWNER).is_err());
        drop(owner);
        assert!(owner_lock(&catalog, OWNER).is_ok());
    }

    #[test]
    fn owner_release_is_immediate_even_with_an_inherited_descriptor() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let owner = owner_lock(root.path(), OWNER).unwrap();
        let inherited = owner.0.try_clone().unwrap();
        assert!(owner_lock(root.path(), OWNER).is_err());
        drop(owner);
        let next = owner_lock(root.path(), OWNER).unwrap();
        drop(inherited);
        assert!(owner_lock(root.path(), OWNER).is_err());
        drop(next);
        assert!(owner_lock(root.path(), OWNER).is_ok());
    }

    #[test]
    fn missing_state_recovers_clean_identity_and_refuses_dirty_or_live_owners() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let catalog = root.path().join("catalog");
        let mut writer = Registry::create_writer(state.join("registry.sqlite3"), OWNER).unwrap();
        register_at(&catalog, &state, OWNER).unwrap();
        let entry = read_entry(&catalog.join(format!("{OWNER}.json"))).unwrap();
        let backup = catalog.join(format!("{OWNER}.ownership"));
        std::fs::create_dir(&backup).unwrap();
        recovery::remember_configuration(&backup, &state).unwrap();
        writer.enable_ownership_backup(backup.clone()).unwrap();
        let active = owner_lock(&catalog, OWNER).unwrap();
        drop(writer);
        std::fs::rename(&state, root.path().join("retired")).unwrap();
        assert!(with_offline_peer(&catalog, &entry, |_| panic!("live owner admitted")).is_err());
        drop(active);
        with_offline_peer(&catalog, &entry, |resolved| assert_eq!(resolved, backup)).unwrap();
        std::fs::write(backup.join("ownership-status"), "dirty\n").unwrap();
        let error =
            with_offline_peer(&catalog, &entry, |_| panic!("dirty snapshot admitted")).unwrap_err();
        assert!(error.contains("dirty or invalid"), "{error}");
    }
}
