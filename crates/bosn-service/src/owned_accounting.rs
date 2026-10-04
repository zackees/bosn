//! Read-only, machine-wide Docker volume accounting; never deletion authority.
use bosn_core::{ObservedArtifact, ResourceKind, ResourceLabels, Retention, Scope};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageClass {
    SharedCiCache,
    PrivateCiStorage,
    OtherBosnVolume,
}

impl StorageClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SharedCiCache => "shared CI cache",
            Self::PrivateCiStorage => "private CI storage",
            Self::OtherBosnVolume => "other Bosn volumes",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StorageSummary {
    pub class: StorageClass,
    pub objects: u64,
    pub attached_objects: u64,
    pub detached_objects: u64,
    pub unknown_size_objects: u64,
    /// Docker's rounded size sum, not allocated filesystem bytes.
    pub approximate_bytes: Option<i128>,
}

#[derive(Debug, Serialize)]
pub struct OwnedStorage {
    pub bytes_approximate: bool,
    pub partial: bool,
    pub invalid_label_volumes: u64,
    pub classes: Vec<StorageSummary>,
    pub private_volumes: Vec<PrivateVolume>,
    pub private_volumes_omitted: usize,
}

#[derive(Debug, Serialize)]
pub struct PrivateVolume {
    pub volume: String,
    pub registry: String,
    pub intent_run: String,
    pub bound_run: Option<String>,
    pub approximate_bytes: Option<i128>,
    pub attached: bool,
    pub lifecycle: LifecycleRead,
    pub state: Option<bosn_registry::act::ActEngineState>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleRead {
    Unavailable,
    OtherRegistry,
    Missing,
    Unreadable,
    IdentityMismatch,
    Matched,
}

impl OwnedStorage {
    /// Correlate only this read-only registry; foreign or absent history stays explicit.
    pub fn correlate(&mut self, registry: Option<&bosn_registry::ReadOnlyRegistry>) {
        let Some(registry) = registry else {
            return;
        };
        let Ok(owner) = registry.registry_id() else {
            return;
        };
        for volume in &mut self.private_volumes {
            if volume.registry != owner {
                volume.lifecycle = LifecycleRead::OtherRegistry;
                continue;
            }
            match registry.act_engine(&volume.intent_run) {
                Ok(Some(record))
                    if record.intent.storage_volume_name().as_deref() == Some(&volume.volume) =>
                {
                    volume.state = Some(record.state);
                    volume.bound_run = record.binding.map(|binding| binding.run_id);
                    volume.lifecycle = LifecycleRead::Matched;
                }
                Ok(Some(_)) => volume.lifecycle = LifecycleRead::IdentityMismatch,
                Ok(None) => volume.lifecycle = LifecycleRead::Missing,
                Err(_) => volume.lifecycle = LifecycleRead::Unreadable,
            }
        }
    }
}

/// Every valid registry is included: this is a machine sample, not one registry's GC plan.
pub fn summarize(artifacts: &[ObservedArtifact], scan_partial: bool) -> OwnedStorage {
    let mut report = OwnedStorage {
        bytes_approximate: true,
        partial: scan_partial,
        invalid_label_volumes: 0,
        private_volumes: Vec::new(),
        private_volumes_omitted: 0,
        classes: [
            StorageClass::SharedCiCache,
            StorageClass::PrivateCiStorage,
            StorageClass::OtherBosnVolume,
        ]
        .into_iter()
        .map(|class| StorageSummary {
            class,
            objects: 0,
            attached_objects: 0,
            detached_objects: 0,
            unknown_size_objects: 0,
            approximate_bytes: Some(0),
        })
        .collect(),
    };
    for artifact in artifacts.iter().filter(|a| a.kind == ResourceKind::Volume) {
        let labels = match ResourceLabels::parse(&artifact.labels) {
            Ok(labels) if labels.kind == ResourceKind::Volume => labels,
            _ => {
                if artifact
                    .labels
                    .keys()
                    .any(|key| bosn_core::unmanaged::is_bosn_label(key))
                {
                    report.invalid_label_volumes += 1;
                    report.partial = true;
                }
                continue;
            }
        };
        let class = storage_class(artifact, &labels);
        let row = report
            .classes
            .iter_mut()
            .find(|row| row.class == class)
            .unwrap();
        if class == StorageClass::PrivateCiStorage {
            report.private_volumes.push(PrivateVolume {
                volume: artifact.id.clone(),
                registry: labels.registry,
                intent_run: labels.generation,
                bound_run: None,
                approximate_bytes: artifact.bytes.filter(|bytes| *bytes >= 0),
                attached: artifact.signals.in_use,
                lifecycle: LifecycleRead::Unavailable,
                state: None,
            });
        }
        row.objects += 1;
        if artifact.signals.in_use {
            row.attached_objects += 1;
        } else {
            row.detached_objects += 1;
        }
        match artifact.bytes.filter(|bytes| *bytes >= 0) {
            Some(bytes) => {
                row.approximate_bytes =
                    row.approximate_bytes.and_then(|sum| sum.checked_add(bytes));
                if row.approximate_bytes.is_none() {
                    report.partial = true;
                }
            }
            None => {
                row.unknown_size_objects += 1;
                row.approximate_bytes = None;
                report.partial = true;
            }
        }
    }
    report.private_volumes.sort_by(|a, b| {
        b.approximate_bytes
            .unwrap_or(i128::MAX)
            .cmp(&a.approximate_bytes.unwrap_or(i128::MAX))
            .then_with(|| a.volume.cmp(&b.volume))
    });
    report.private_volumes_omitted = report.private_volumes.len().saturating_sub(64);
    report.private_volumes.truncate(64);
    report
}

fn storage_class(artifact: &ObservedArtifact, labels: &ResourceLabels) -> StorageClass {
    if labels.stack == "ci-cache"
        && labels.scope == Scope::Machine
        && labels.retention == Retention::Pinned
        && labels.generation == "v1"
        && labels.workspace == "machine"
        && artifact.id == "bosn-ci-cache-v1"
    {
        StorageClass::SharedCiCache
    } else if labels.stack == "act-storage"
        && labels.scope == Scope::Spec
        && labels.workspace == labels.generation
        && artifact.id == format!("bosn-act-storage-{}", labels.generation)
    {
        StorageClass::PrivateCiStorage
    } else {
        StorageClass::OtherBosnVolume
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn volume(owner: &str, size: Option<i128>, attached: bool) -> ObservedArtifact {
        ObservedArtifact {
            id: format!("bosn-act-storage-{owner}"),
            kind: ResourceKind::Volume,
            labels: ResourceLabels::new(
                owner,
                ResourceKind::Volume,
                "act-storage",
                owner,
                Scope::Spec,
                owner,
                "1",
                Some(Retention::Warm),
            )
            .unwrap()
            .to_map()
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect(),
            signals: bosn_core::Signals {
                in_use: attached,
                dangling: false,
                anonymous: false,
            },
            bytes: size,
            age_seconds: Some(1.0),
        }
    }
    #[test]
    fn all_registries_and_detached_storage_are_counted_once() {
        let report = summarize(
            &[volume("a", Some(10), true), volume("b", Some(20), false)],
            false,
        );
        let private = &report.classes[1];
        assert_eq!(
            (
                private.objects,
                private.attached_objects,
                private.detached_objects
            ),
            (2, 1, 1)
        );
        assert_eq!(private.approximate_bytes, Some(30));
        assert!(!report.partial);
    }
    #[test]
    fn unknown_sizes_do_not_become_zero_or_a_known_subtotal() {
        let report = summarize(
            &[volume("a", None, false), volume("b", Some(20), true)],
            false,
        );
        assert_eq!(report.classes[1].approximate_bytes, None);
        assert_eq!(report.classes[1].unknown_size_objects, 1);
        assert!(report.partial);
    }
    #[test]
    fn name_never_proves_ownership() {
        let mut artifact = volume("a", Some(10), false);
        artifact.labels.clear();
        let report = summarize(&[artifact], true);
        assert_eq!(report.classes[1].objects, 0);
        assert!(report.partial);
    }

    #[test]
    fn incomplete_bosn_labels_are_unknown_owned_storage() {
        let mut artifact = volume("a", Some(10), false);
        artifact.labels.remove(bosn_core::LABEL_SCOPE);
        let report = summarize(&[artifact], false);
        assert_eq!(report.invalid_label_volumes, 1);
        assert!(report.partial);
        assert!(report.classes.iter().all(|row| row.objects == 0));
    }

    #[test]
    fn details_are_bounded_without_losing_totals_and_unknowns_stay_visible() {
        let mut artifacts: Vec<_> = (0..70)
            .map(|n| volume(&format!("owner{n}"), Some(n), false))
            .collect();
        artifacts.push(volume("unknown", None, false));
        let report = summarize(&artifacts, false);
        assert_eq!(report.classes[1].objects, 71);
        assert_eq!(report.private_volumes.len(), 64);
        assert_eq!(report.private_volumes_omitted, 7);
        assert_eq!(report.private_volumes[0].approximate_bytes, None);
    }

    #[test]
    fn correlation_reads_persisted_cleanup_state_and_keeps_foreign_history_unknown() {
        use bosn_registry::act::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.sqlite3");
        let owner = "11111111-2222-4333-8444-555555555555";
        let run = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let intent = ActEngineIntent {
            run_id: run.into(),
            workspace: "/source".into(),
            candidate_sha: "a".repeat(40),
            payload_sha256: "b".repeat(64),
            snapshot_sha256: "c".repeat(64),
            act_version: "test".into(),
            act_image_digest: format!("sha256:{}", "d".repeat(64)),
            engine_image_digest: format!("sha256:{}", "e".repeat(64)),
            runner_image_digest: format!("sha256:{}", "f".repeat(64)),
            created_at: 1.0,
            spare: false,
            creation_profile: Some(ActEngineCreationProfile {
                memory_bytes: 8 << 30,
                storage_bytes: 4 << 30,
                nano_cpus: 2_000_000_000,
                pids: 1024,
                run_tmpfs_bytes: 16 << 20,
                tmp_tmpfs_bytes: 64 << 20,
                tmpfs_policy: ActEngineTmpfsPolicy::NamedDiskStorageRunTmpNoexecV2,
                init_command_sha256: "a".repeat(64),
                cache_volume: None,
            }),
        };
        let mut writer = bosn_registry::Registry::create_writer(&path, owner).unwrap();
        let mut tx = writer.begin_immediate().unwrap();
        tx.begin_act_engine(&intent).unwrap();
        tx.request_act_cleanup(run, ActRunOutcome::Interrupted, 2.0)
            .unwrap();
        tx.commit().unwrap();
        drop(writer);
        let registry = bosn_registry::Registry::open_read_only(&path).unwrap();
        let mut artifact = volume(owner, Some(10), false);
        artifact.id = intent.storage_volume_name().unwrap();
        artifact
            .labels
            .insert(bosn_core::LABEL_GENERATION.into(), run.into());
        artifact
            .labels
            .insert(bosn_core::LABEL_WORKSPACE.into(), run.into());
        let mut report = summarize(
            &[
                artifact,
                volume(owner, Some(2), false),
                volume("foreign", Some(1), true),
            ],
            false,
        );
        assert!(matches!(
            report.private_volumes[0].lifecycle,
            LifecycleRead::Unavailable
        ));
        report.correlate(Some(&registry));
        assert!(matches!(
            report.private_volumes[0].lifecycle,
            LifecycleRead::Matched
        ));
        assert_eq!(
            report.private_volumes[0].state,
            Some(ActEngineState::CleanupRequired)
        );
        assert!(matches!(
            report.private_volumes[2].lifecycle,
            LifecycleRead::OtherRegistry
        ));
        assert!(matches!(
            report.private_volumes[1].lifecycle,
            LifecycleRead::Missing
        ));
        assert_eq!(report.private_volumes[1].state, None);
    }
}
