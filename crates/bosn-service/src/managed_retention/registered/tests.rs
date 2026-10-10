//! #545: setup-labelled objects enter managed retention only through a matching registry record.

use std::collections::BTreeMap;

use bosn_core::retention::{HoldReason, RetentionPolicy, classify_managed};
use bosn_core::{ObservedArtifact, ResourceKind, ResourceState, Retention, Scope, Signals};
use bosn_registry::{Registry, Resource};

use super::{RegisteredOwnership, setup_record_id};

const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const NOW: f64 = 10_000_000.0;
const DAY: f64 = 86_400.0;

fn digest() -> String {
    "a".repeat(64)
}

/// The labels setup ensure and manifest volume creation actually emit.
fn setup_labels(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("com.zackees.bosn.setup-managed".to_owned(), "v1".to_owned()),
        ("com.zackees.bosn.setup-content-sha256".to_owned(), digest()),
        (
            "com.zackees.bosn.setup-container".to_owned(),
            name.to_owned(),
        ),
    ])
}

fn record(id: &str, kind: ResourceKind, name: &str, retention: Retention) -> Resource {
    Resource {
        id: id.into(),
        kind,
        name: name.into(),
        stack: "app".into(),
        generation: format!("sha256:{}", digest()),
        scope: Scope::Machine,
        workspace: "/workspace".into(),
        created_at: NOW - 40.0 * DAY,
        last_used: NOW - 20.0 * DAY,
        state: ResourceState::Active,
        retention,
    }
}

fn state_with(records: &[Resource]) -> kernal_api::platform::fs::TemporaryDirectory {
    let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry =
        Registry::create_writer(root.path().join("registry.sqlite3"), OWNER).unwrap();
    let mut transaction = registry.begin_immediate().unwrap();
    for resource in records {
        transaction.put_resource(resource).unwrap();
    }
    transaction.commit().unwrap();
    root
}

fn hold(kind: ResourceKind, labels: BTreeMap<String, String>, age: f64) -> Option<HoldReason> {
    let artifact = ObservedArtifact {
        id: "object".into(),
        kind,
        labels,
        signals: Signals::default(),
        bytes: Some(1),
        age_seconds: Some(age),
    };
    classify_managed(&artifact, Some(OWNER), RetentionPolicy::default()).hold
}

#[test]
fn a_recorded_setup_container_is_proven_and_reclaimable() {
    let name = "bosn-setup-v2-abc";
    let root = state_with(&[record(
        &format!("setup-container:sha256:{}", digest()),
        ResourceKind::Container,
        name,
        // Every setup ensure writes Pinned; it is bookkeeping, not a promise.
        Retention::Pinned,
    )]);
    let ownership = RegisteredOwnership::load(root.path()).unwrap();
    let labels = setup_labels(name);
    assert_eq!(
        hold(ResourceKind::Container, labels.clone(), 30.0 * DAY),
        Some(HoldReason::IncompleteLabels),
        "without the registry the setup label set alone proves nothing"
    );
    let proof = ownership
        .normalize(ResourceKind::Container, name, &labels, NOW)
        .expect("the production label set plus its record proves ownership");
    assert!(!proof.protected);
    assert!((proof.idle_seconds - 20.0 * DAY).abs() < 1.0);
    assert_eq!(
        hold(ResourceKind::Container, proof.labels, 30.0 * DAY),
        None
    );
}

#[test]
fn a_manifest_volume_keeps_its_declared_pin() {
    let name = "bosn-v-stack-0123456789abcdef01234567";
    let root = state_with(&[record(
        &format!("manifest-volume:{name}"),
        ResourceKind::Volume,
        name,
        Retention::Pinned,
    )]);
    let ownership = RegisteredOwnership::load(root.path()).unwrap();
    let proof = ownership
        .normalize(ResourceKind::Volume, name, &setup_labels(name), NOW)
        .unwrap();
    assert_eq!(
        hold(ResourceKind::Volume, proof.labels, 30.0 * DAY),
        Some(HoldReason::Pinned),
        "a manifest's declared pin is a human promise"
    );
}

#[test]
fn name_digest_and_namespace_must_all_agree() {
    let name = "bosn-setup-v2-abc";
    let root = state_with(&[record(
        "setup-container:x",
        ResourceKind::Container,
        name,
        Retention::Warm,
    )]);
    let ownership = RegisteredOwnership::load(root.path()).unwrap();
    // The engine name differs from the label.
    assert!(
        ownership
            .normalize(
                ResourceKind::Container,
                "bosn-setup-v2-other",
                &setup_labels(name),
                NOW
            )
            .is_none()
    );
    // A different digest.
    let mut wrong = setup_labels(name);
    wrong.insert(
        "com.zackees.bosn.setup-content-sha256".into(),
        "b".repeat(64),
    );
    assert!(
        ownership
            .normalize(ResourceKind::Container, name, &wrong, NOW)
            .is_none()
    );
    // No record of this name at all: a name is never evidence.
    assert!(
        ownership
            .normalize(
                ResourceKind::Container,
                "bosn-setup-v2-unrecorded",
                &setup_labels("bosn-setup-v2-unrecorded"),
                NOW
            )
            .is_none()
    );
    // Canonical ownership speaks for itself and is never reinterpreted.
    let mut canonical = setup_labels(name);
    canonical.insert(bosn_core::LABEL_REGISTRY.into(), "someone-else".into());
    assert!(
        ownership
            .normalize(ResourceKind::Container, name, &canonical, NOW)
            .is_none()
    );
    assert!(!setup_record_id(ResourceKind::Volume, "setup-container:x"));
    assert!(!setup_record_id(ResourceKind::Image, "setup-image:x"));
}

#[test]
fn a_lease_or_session_protects_the_object() {
    let name = "bosn-v-stack-leased";
    let id = format!("manifest-volume:{name}");
    let root = state_with(&[record(&id, ResourceKind::Volume, name, Retention::Warm)]);
    {
        let mut registry = Registry::open_writer(root.path().join("registry.sqlite3")).unwrap();
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .put_lease(&bosn_registry::Lease {
                id: "lease-1".into(),
                resource_id: id.clone(),
                pid: std::process::id(),
                proc_start: None,
                acquired_at: NOW,
                heartbeat_at: NOW,
                ttl_seconds: 60.0,
            })
            .unwrap();
        transaction.commit().unwrap();
    }
    let ownership = RegisteredOwnership::load(root.path()).unwrap();
    let proof = ownership
        .normalize(ResourceKind::Volume, name, &setup_labels(name), NOW)
        .unwrap();
    assert!(proof.protected, "a leased object is in use");
}
