use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::{
    LABEL_CREATED, LABEL_GENERATION, LABEL_KIND, LABEL_REGISTRY, LABEL_RETENTION, LABEL_SCOPE,
    LABEL_STACK, LABEL_WORKSPACE, REQUIRED_LABELS, Signals,
};

const REGISTRY: &str = "11111111-2222-3333-4444-555555555555";

fn labels(overrides: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for key in REQUIRED_LABELS {
        map.insert(key.to_string(), "v".to_string());
    }
    map.insert(LABEL_REGISTRY.to_string(), REGISTRY.to_string());
    for (key, value) in overrides {
        map.insert((*key).to_string(), (*value).to_string());
    }
    map
}

fn artifact(
    id: &str,
    kind: ResourceKind,
    age: Option<f64>,
    in_use: bool,
    overrides: &[(&str, &str)],
) -> ObservedArtifact {
    ObservedArtifact {
        id: id.to_string(),
        kind,
        labels: labels(overrides),
        signals: Signals {
            in_use,
            dangling: false,
            anonymous: false,
        },
        bytes: Some(1024),
        age_seconds: age,
    }
}

fn old_container() -> ObservedArtifact {
    artifact(
        "c-old",
        ResourceKind::Container,
        Some(DEFAULT_CONTAINER_TTL.as_secs_f64() + 1.0),
        false,
        &[],
    )
}

#[test]
fn an_old_stopped_owned_container_is_reclaimable() {
    let verdict = classify_managed(&old_container(), Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(
        verdict.hold, None,
        "an idle owned container past its gate is reclaimable"
    );
    assert!(verdict.is_reclaimable());
}

#[test]
fn a_container_within_its_gate_is_held() {
    let fresh = artifact("c-new", ResourceKind::Container, Some(60.0), false, &[]);
    let verdict = classify_managed(&fresh, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::WithinTtl));
}

#[test]
fn an_in_use_container_is_held_even_when_old() {
    // The leak being fixed: a running engine must never be reaped by age.
    let running = artifact(
        "c-run",
        ResourceKind::Container,
        Some(DEFAULT_CONTAINER_TTL.as_secs_f64() * 10.0),
        true,
        &[],
    );
    let verdict = classify_managed(&running, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::InUse));
}

#[test]
fn an_in_use_volume_is_held_even_when_old() {
    // The volume a live container still mounts must survive its own age gate.
    let mounted = artifact(
        "v-live",
        ResourceKind::Volume,
        Some(DEFAULT_VOLUME_TTL.as_secs_f64() * 2.0),
        true,
        &[],
    );
    let verdict = classify_managed(&mounted, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::InUse));
}

#[test]
fn pinned_is_never_overridden_by_age() {
    let pinned = artifact(
        "v-pinned",
        ResourceKind::Volume,
        Some(DEFAULT_VOLUME_TTL.as_secs_f64() * 100.0),
        false,
        &[(LABEL_RETENTION, "pinned")],
    );
    let verdict = classify_managed(&pinned, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::Pinned));
}

#[test]
fn a_missing_retention_label_does_not_pin_but_does_not_reclaim_either() {
    // An absent label is not a pin, and must not be read as consent either: the volume is
    // reclaimable only because it is also within its own much longer gate.
    let unlabeled = artifact(
        "v-nolabel",
        ResourceKind::Volume,
        Some(DEFAULT_VOLUME_TTL.as_secs_f64() + 1.0),
        false,
        &[],
    );
    let verdict = classify_managed(&unlabeled, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, None, "a missing retention label must not pin");
}

#[test]
fn an_unmeasured_age_fails_closed() {
    let unknown = artifact("c-unknown", ResourceKind::Container, None, false, &[]);
    let verdict = classify_managed(&unknown, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::AgeUnknown));
}

#[test]
fn incomplete_labels_are_never_reclaimed() {
    let mut partial = BTreeMap::new();
    partial.insert(LABEL_REGISTRY.to_string(), REGISTRY.to_string());
    partial.insert(LABEL_CREATED.to_string(), "v".to_string());
    let artifact = ObservedArtifact {
        id: "c-partial".into(),
        kind: ResourceKind::Container,
        labels: partial,
        signals: Signals {
            in_use: false,
            dangling: false,
            anonymous: false,
        },
        bytes: Some(1),
        age_seconds: Some(1e12),
    };
    let verdict = classify_managed(&artifact, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::IncompleteLabels));
}

#[test]
fn another_registry_is_never_reclaimed() {
    let foreign = artifact(
        "c-foreign",
        ResourceKind::Container,
        Some(1e12),
        false,
        &[(LABEL_REGISTRY, "99999999-9999-9999-9999-999999999999")],
    );
    let verdict = classify_managed(&foreign, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::ForeignRegistry));
}

#[test]
fn an_unlabelled_object_is_never_ours_to_reclaim() {
    let artifact = ObservedArtifact {
        id: "foreign".into(),
        kind: ResourceKind::Container,
        labels: BTreeMap::new(),
        signals: Signals {
            in_use: false,
            dangling: false,
            anonymous: false,
        },
        bytes: Some(1),
        age_seconds: Some(1e12),
    };
    let verdict = classify_managed(&artifact, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::NotBosnOwned));
}

#[test]
fn networks_and_builders_are_out_of_scope() {
    let network = artifact("n", ResourceKind::Network, Some(1e12), false, &[]);
    let verdict = classify_managed(&network, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(verdict.hold, Some(HoldReason::UnsupportedKind));
}

#[test]
fn an_unknown_registry_proves_nothing_and_reclaims_nothing() {
    // Without our own registry id, "ours" cannot be established for any object.
    let verdict = classify_managed(&old_container(), None, RetentionPolicy::default());
    assert!(
        verdict.hold.is_some(),
        "an unknown registry id must never authorize a removal"
    );
}

#[test]
fn the_plan_orders_containers_before_volumes_before_images() {
    let artifacts = vec![
        artifact(
            "img",
            ResourceKind::Image,
            Some(DEFAULT_IMAGE_TTL.as_secs_f64() + 1.0),
            false,
            &[],
        ),
        artifact(
            "vol",
            ResourceKind::Volume,
            Some(DEFAULT_VOLUME_TTL.as_secs_f64() + 1.0),
            false,
            &[],
        ),
        old_container(),
    ];
    let plan = plan_managed(&artifacts, Some(REGISTRY), RetentionPolicy::default());
    let ids: Vec<&str> = plan.candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        ["c-old", "vol", "img"],
        "removal order must unblock volumes first"
    );
}

#[test]
fn within_a_kind_the_oldest_goes_first() {
    // Both are past the container gate; the older one must be taken first so a capped pass
    // spends its budget on the bytes that have been idle longest.
    let gate = DEFAULT_CONTAINER_TTL.as_secs_f64();
    let artifacts = vec![
        artifact(
            "c-young",
            ResourceKind::Container,
            Some(gate + 20_000.0),
            false,
            &[],
        ),
        artifact(
            "c-ancient",
            ResourceKind::Container,
            Some(gate + 90_000.0),
            false,
            &[],
        ),
    ];
    let plan = plan_managed(&artifacts, Some(REGISTRY), RetentionPolicy::default());
    let ids: Vec<&str> = plan.candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["c-ancient", "c-young"]);
}

#[test]
fn held_objects_are_reported_with_their_reason() {
    let artifacts = vec![
        old_container(),
        artifact("c-live", ResourceKind::Container, Some(1e12), true, &[]),
    ];
    let plan = plan_managed(&artifacts, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(plan.candidates.len(), 1);
    let held = plan.held.iter().find(|h| h.id == "c-live").expect("held");
    assert_eq!(held.hold, Some(HoldReason::InUse));
}

#[test]
fn a_byte_ceiling_defers_rather_than_failing() {
    let gate = DEFAULT_CONTAINER_TTL.as_secs_f64();
    let mut artifacts = Vec::new();
    for index in 0..5 {
        let mut item = artifact(
            &format!("c-{index}"),
            ResourceKind::Container,
            Some(gate + 20_000.0 + f64::from(index)),
            false,
            &[],
        );
        item.bytes = Some(1000);
        artifacts.push(item);
    }
    let policy = RetentionPolicy {
        max_bytes: Some(2500),
        ..RetentionPolicy::default()
    };
    let plan = plan_managed(&artifacts, Some(REGISTRY), policy);
    assert_eq!(
        plan.candidates.len(),
        2,
        "2500 bytes fits two 1000-byte objects"
    );
    assert_eq!(plan.deferred, 3, "the rest is deferred, not lost");
    assert_eq!(plan.bytes, 2000);
}

#[test]
fn the_object_cap_bounds_one_pass() {
    let gate = DEFAULT_CONTAINER_TTL.as_secs_f64();
    let artifacts: Vec<ObservedArtifact> = (0..MAX_MANAGED_REMOVALS + 10)
        .map(|index| {
            artifact(
                &format!("c-{index}"),
                ResourceKind::Container,
                Some(gate + 20_000.0),
                false,
                &[],
            )
        })
        .collect();
    let plan = plan_managed(&artifacts, Some(REGISTRY), RetentionPolicy::default());
    assert_eq!(plan.candidates.len(), MAX_MANAGED_REMOVALS);
    assert_eq!(plan.deferred, 10);
}

#[test]
fn an_unmeasured_size_cannot_escape_the_byte_ceiling() {
    let mut unknown = old_container();
    unknown.bytes = None;
    let policy = RetentionPolicy {
        max_bytes: Some(0),
        ..RetentionPolicy::default()
    };
    let plan = plan_managed(&[unknown], Some(REGISTRY), policy);
    assert!(
        plan.candidates.is_empty(),
        "unknown size must not bypass a ceiling"
    );
}

#[test]
fn a_zero_ttl_policy_reclaims_everything_past_the_gate() {
    let policy = RetentionPolicy {
        container_ttl: Duration::ZERO,
        volume_ttl: Duration::ZERO,
        image_ttl: Duration::ZERO,
        max_bytes: None,
    };
    let fresh = artifact("c", ResourceKind::Container, Some(1.0), false, &[]);
    let plan = plan_managed(&[fresh], Some(REGISTRY), policy);
    assert_eq!(plan.candidates.len(), 1);
}

#[test]
fn age_seconds_rejects_a_clock_before_the_epoch() {
    let before = UNIX_EPOCH - Duration::from_secs(10);
    assert_eq!(age_seconds(before), None);
    let after = UNIX_EPOCH + Duration::from_secs(10);
    assert_eq!(age_seconds(after), Some(10.0));
}

#[test]
fn labels_and_scope_constants_used_by_the_policy_are_real() {
    // Guards against a rename silently making every object IncompleteLabels.
    let map = labels(&[
        (LABEL_KIND, "container"),
        (LABEL_SCOPE, "machine"),
        (LABEL_STACK, "s"),
    ]);
    assert_eq!(
        classify_ownership(&map, Some(REGISTRY)),
        crate::OwnershipClass::Ours
    );
    assert_eq!(map.get(LABEL_WORKSPACE).map(String::as_str), Some("v"));
    assert_eq!(map.get(LABEL_GENERATION).map(String::as_str), Some("v"));
    assert!(SystemTime::now().duration_since(UNIX_EPOCH).is_ok());
}
