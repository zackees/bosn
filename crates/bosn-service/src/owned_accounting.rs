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
}

/// Every valid registry is included: this is a machine sample, not one registry's GC plan.
pub fn summarize(artifacts: &[ObservedArtifact], scan_partial: bool) -> OwnedStorage {
    let mut report = OwnedStorage {
        bytes_approximate: true,
        partial: scan_partial,
        invalid_label_volumes: 0,
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
        let class = if labels.stack == "ci-cache"
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
        };
        let row = report
            .classes
            .iter_mut()
            .find(|row| row.class == class)
            .unwrap();
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
    report
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
}
