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
    pub other_volumes: Vec<OwnedVolume>,
    pub other_volumes_omitted: usize,
}

/// Advisory contributor details. Labels and attachment state do not prove
/// lifecycle/release eligibility; no candidate token or deletion authority.
#[derive(Debug, Serialize)]
pub struct OwnedVolume {
    pub volume: String,
    pub registry: String,
    pub workspace: String,
    pub stack: String,
    pub generation: String,
    pub scope: &'static str,
    pub retention: &'static str,
    pub approximate_bytes: Option<i128>,
    pub attached: bool,
    pub inspection: VolumeInspection,
}

impl OwnedVolume {
    fn observed(artifact: &ObservedArtifact, labels: &ResourceLabels) -> Self {
        Self {
            volume: artifact.id.clone(),
            registry: labels.registry.clone(),
            workspace: labels.workspace.clone(),
            stack: labels.stack.clone(),
            generation: labels.generation.clone(),
            scope: labels.scope.as_str(),
            retention: labels.retention.as_str(),
            approximate_bytes: artifact.bytes.filter(|bytes| *bytes >= 0),
            attached: artifact.signals.in_use,
            inspection: if matches!(labels.scope, Scope::Stack | Scope::Machine)
                || labels.retention == Retention::Pinned
            {
                VolumeInspection::DurableReleasePreview
            } else {
                VolumeInspection::RegistryHistory
            },
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeInspection {
    /// Inspect owning registry history. Foreign registries remain unknown.
    RegistryHistory,
    /// Run the existing read-only manifest volume-release preview in the
    /// owning registry/workspace; it alone establishes eligible candidates.
    DurableReleasePreview,
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
    /// Size warnings share the existing census; they never authorize removal.
    pub fn warning_lines(&self, threshold: bosn_core::WarningThreshold) -> Vec<String> {
        let objects: u64 = self.classes.iter().map(|row| row.objects).sum();
        if objects == 0 && self.invalid_label_volumes == 0 {
            return Vec::new();
        }
        let bytes = self.classes.iter().try_fold(0_i128, |total, row| {
            total.checked_add(row.approximate_bytes?)
        });
        if !self.partial
            && objects < threshold.objects
            && bytes.is_some_and(|bytes| bytes < threshold.bytes)
        {
            return Vec::new();
        }
        let mut lines = vec![format!(
            "Bosn-owned Docker volume footprint (approximate sizes{})",
            if self.partial {
                "; accounting incomplete"
            } else {
                ""
            }
        )];
        for row in self.classes.iter().filter(|row| row.objects > 0) {
            let size = row
                .approximate_bytes
                .map_or_else(|| "unknown".into(), crate::unmanaged::human_bytes);
            lines.push(format!(
                "  {}: {} objects, {} attached, {} detached, {size}",
                row.class.as_str(),
                row.objects,
                row.attached_objects,
                row.detached_objects
            ));
        }
        if self.invalid_label_volumes > 0 {
            lines.push(format!(
                "  {} volumes have incomplete or invalid Bosn ownership labels",
                self.invalid_label_volumes
            ));
        }
        lines.push("  inspect owned storage: bosn scan --json".into());
        if self
            .classes
            .iter()
            .any(|row| row.class == StorageClass::SharedCiCache && row.objects > 0)
        {
            lines.push("  measure shared cache blocks: bosn ci runners cache".into());
        }
        lines.push("  retained volumes require explicit release; attachment state alone is not removal authority".into());
        lines
    }

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
        other_volumes: Vec::new(),
        other_volumes_omitted: 0,
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
        if class == StorageClass::OtherBosnVolume {
            report
                .other_volumes
                .push(OwnedVolume::observed(artifact, &labels));
        }
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
    report.other_volumes.sort_by(|a, b| {
        b.approximate_bytes
            .unwrap_or(i128::MAX)
            .cmp(&a.approximate_bytes.unwrap_or(i128::MAX))
            .then_with(|| a.volume.cmp(&b.volume))
    });
    report.other_volumes_omitted = report.other_volumes.len().saturating_sub(64);
    report.other_volumes.truncate(64);
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
    fn retained_owned_contributors_are_visible_without_authorizing_release() {
        let mut artifact = volume("owner", Some(100), false);
        artifact.id = "bosn-v-machine-retained".into();
        artifact
            .labels
            .insert(bosn_core::LABEL_SCOPE.into(), "machine".into());
        artifact
            .labels
            .insert(bosn_core::LABEL_RETENTION.into(), "pinned".into());
        let report = summarize(&[artifact], false);
        assert_eq!(report.other_volumes.len(), 1);
        let detail = &report.other_volumes[0];
        assert_eq!(detail.volume, "bosn-v-machine-retained");
        assert_eq!(detail.registry, "owner");
        assert_eq!(detail.scope, "machine");
        assert_eq!(detail.retention, "pinned");
        assert!(!detail.attached);
        assert_eq!(detail.inspection, VolumeInspection::DurableReleasePreview);
        assert_eq!(report.classes[2].approximate_bytes, Some(100));
        let json = serde_json::to_value(report).unwrap();
        let contributors = json.get("other_volumes").and_then(|v| v.as_array());
        assert!(
            contributors.is_some_and(|rows| rows.len() == 1),
            "retained owned bytes need a size-ranked contributor, not only a class sum"
        );
    }

    #[test]
    fn other_volume_details_preserve_unknowns_and_totals_with_bounded_output() {
        let mut artifacts: Vec<_> = (0..70)
            .map(|n| {
                let mut artifact = volume(&format!("owner{n}"), Some(n), n % 2 == 0);
                artifact.id = format!("bosn-other-{n}");
                artifact
            })
            .collect();
        let mut unknown = volume("unknown", None, false);
        unknown.id = "bosn-other-unknown".into();
        artifacts.push(unknown);
        let report = summarize(&artifacts, false);
        assert_eq!(report.other_volumes.len(), 64);
        assert_eq!(report.other_volumes_omitted, 7);
        assert_eq!(report.other_volumes[0].volume, "bosn-other-unknown");
        assert_eq!(report.other_volumes[0].approximate_bytes, None);
        assert_eq!(report.other_volumes[1].approximate_bytes, Some(69));
        assert_eq!(
            report.other_volumes[1].inspection,
            VolumeInspection::RegistryHistory
        );
        assert_eq!(report.classes[2].objects, 71);
        assert_eq!(report.classes[2].unknown_size_objects, 1);
        assert_eq!(report.classes[2].approximate_bytes, None);
        assert!(report.partial);
        assert!(report.private_volumes.is_empty());
    }

    #[test]
    fn large_owned_storage_warns_even_without_unmanaged_objects() {
        let report = summarize(&[volume("owner", Some(100), true)], false);
        let lines = report.warning_lines(bosn_core::WarningThreshold {
            bytes: 80,
            objects: 10,
        });
        assert!(lines.iter().any(|line| line.contains("private CI storage")));
        assert!(lines.iter().any(|line| line.contains("1 attached")));
        assert!(lines.iter().any(|line| line.contains("bosn scan --json")));
        assert!(lines.iter().all(|line| !line.contains("--apply")));
    }

    #[test]
    fn owned_unknown_sizes_warn_but_empty_or_small_complete_storage_does_not() {
        let threshold = bosn_core::WarningThreshold {
            bytes: 80,
            objects: 10,
        };
        let unknown = summarize(&[volume("owner", None, false)], false);
        let lines = unknown.warning_lines(threshold);
        assert!(lines.iter().any(|line| line.contains("unknown")));
        assert!(lines.iter().any(|line| line.contains("incomplete")));
        assert!(summarize(&[], true).warning_lines(threshold).is_empty());
        assert!(
            summarize(&[volume("owner", Some(10), false)], false)
                .warning_lines(threshold)
                .is_empty()
        );
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
                cache_coordination: None,
                tool_generation: None,
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
