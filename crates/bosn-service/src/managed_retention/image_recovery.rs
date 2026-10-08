//! Reconcile only freshly inspected exports carrying durable preparation proof.

use super::*;
use bosn_registry::{ImageIntentSource, Registry};

#[derive(Default)]
pub(crate) struct Report {
    pub(crate) held_count: u64,
    pub(crate) held: Vec<String>,
}

impl Report {
    fn hold(&mut self, reference: &str, reason: &str) {
        self.held_count += 1;
        details::push(
            &mut self.held,
            format!("pending image preparation {reference}: {reason}"),
        );
    }
}

pub(crate) fn reconcile_until(
    engine: &DockerEngine,
    registry: &mut Registry,
    deadline: std::time::Instant,
) -> Result<Report, String> {
    let _budget =
        budget::Guard::start(deadline.saturating_duration_since(std::time::Instant::now()));
    reconcile(engine, registry)
}

/// The caller holds machine admission and this registry's owner/writer fences.
/// Missing images retain intent: an external builder may still export later.
pub(super) fn reconcile(engine: &DockerEngine, registry: &mut Registry) -> Result<Report, String> {
    let owner = registry.registry_id().map_err(|error| error.to_string())?;
    let intents = registry
        .image_creation_intents()
        .map_err(|error| error.to_string())?;
    let mut report = deletion_recovery::reconcile(engine, registry)?;
    for intent in intents {
        budget::check()?;
        let observation = parse_read::<ImageDetail>(engine.inspect_images(
            std::slice::from_ref(&intent.reference),
            budget::options(RunOptions::bounded(
                RETENTION_READ_DEADLINE,
                RETENTION_OUTPUT_LIMIT,
            )),
        ));
        let Some(entries) = observation else {
            report.hold(
                &intent.reference,
                "export absent or inspection incomplete; intent retained",
            );
            continue;
        };
        let [image] = entries.as_slice() else {
            report.hold(
                &intent.reference,
                "inspection did not identify exactly one export; intent retained",
            );
            continue;
        };
        let verification = match intent.source {
            ImageIntentSource::Build => {
                intent.verify_build_export(&image.id, &image.labels(), &owner)
            }
            ImageIntentSource::Pull => intent.verify_pull_export(
                &image.id,
                image.repo_digests.as_deref().unwrap_or_default(),
                &image.labels(),
                &owner,
            ),
        };
        let proof = match verification {
            Ok(proof) => proof,
            Err(error) => {
                report.hold(
                    &intent.reference,
                    &format!("ownership verification refused: {error}"),
                );
                continue;
            }
        };
        let mut transaction = registry
            .begin_immediate()
            .map_err(|error| error.to_string())?;
        transaction
            .reconcile_image_export(proof)
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
    }
    registry
        .publish_ownership_backup()
        .map_err(|error| error.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pulled_image_recovers_only_with_exact_repository_digest() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            root.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let intent = bosn_registry::ImageCreationIntent {
            reference: format!("alpine@sha256:{}", "a".repeat(64)),
            content_sha256: "b".repeat(64),
            workspace: "/workspace".into(),
            stack: "app".into(),
            owner: bosn_registry::ImageIntentOwner::Setup,
            source: ImageIntentSource::Pull,
            created_at: 1.0,
        };
        let mut transaction = registry.begin_immediate().unwrap();
        transaction.put_image_creation_intent(&intent).unwrap();
        transaction.commit().unwrap();
        for valid in [false, true] {
            let references = if valid {
                vec![intent.reference.clone()]
            } else {
                vec!["alpine:latest".into()]
            };
            let document = serde_json::json!([{"Id": format!("sha256:{}", "c".repeat(64)),
                "RepoDigests": references, "Created": "2026-10-01T00:00:00Z", "Config": {"Labels": {}}}]).to_string();
            let engine = DockerEngine::synthetic_for_test(
                "/bin/sh",
                ["-c", "printf '%s' \"$BOSN_RECOVERY_DOCUMENT\"", "fixture"],
            )
            .env("BOSN_RECOVERY_DOCUMENT", document);
            let report = reconcile(&engine, &mut registry).unwrap();
            assert_eq!(report.held_count, u64::from(!valid));
            assert_eq!(
                registry.image_creation_intents().unwrap().len(),
                usize::from(!valid)
            );
            assert_eq!(
                registry.resources(0, 64).unwrap().items.len(),
                usize::from(valid)
            );
        }
    }
    #[test]
    fn exported_image_recovers_but_missing_proof_retains_intent() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            root.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let intent = bosn_registry::ImageCreationIntent {
            reference: format!("bosn-setup:{}", "a".repeat(64)),
            content_sha256: "a".repeat(64),
            workspace: "/workspace".into(),
            stack: "app".into(),
            owner: bosn_registry::ImageIntentOwner::Manifest,
            source: ImageIntentSource::Build,
            created_at: 1.0,
        };
        let mut transaction = registry.begin_immediate().unwrap();
        transaction.put_image_creation_intent(&intent).unwrap();
        transaction.commit().unwrap();
        for valid in [false, true] {
            let labels = if valid {
                BTreeMap::from([(
                    bosn_setup::IMAGE_INTENT_LABEL,
                    intent.ownership_proof().unwrap(),
                )])
            } else {
                BTreeMap::new()
            };
            let document = serde_json::json!([{"Id": format!("sha256:{}", "b".repeat(64)),
                "Created": "2026-10-01T00:00:00Z", "Config": {"Labels": labels}}])
            .to_string();
            let engine = DockerEngine::synthetic_for_test(
                "/bin/sh",
                ["-c", "printf '%s' \"$BOSN_RECOVERY_DOCUMENT\"", "fixture"],
            )
            .env("BOSN_RECOVERY_DOCUMENT", document);
            let report = reconcile(&engine, &mut registry).unwrap();
            assert_eq!(report.held_count, u64::from(!valid));
            assert_eq!(
                registry.image_creation_intents().unwrap().len(),
                usize::from(!valid)
            );
            assert_eq!(
                registry.resources(0, 64).unwrap().items.len(),
                usize::from(valid)
            );
        }
    }
}
