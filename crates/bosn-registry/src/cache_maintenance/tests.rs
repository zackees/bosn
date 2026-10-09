use super::*;
use crate::cache_helper::{CacheHelperIntent, CacheHelperRole};
const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const NONCE: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

#[test]
fn latest_snapshot_survives_restart_and_requires_real_helper_cleanup_evidence() {
    let directory = fs::TemporaryDirectory::new().unwrap();
    let path = directory.path().join("registry.sqlite3");
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let id = "1".repeat(64);
    let intent = CacheHelperIntent {
        registry_id: OWNER.into(),
        nonce: NONCE.into(),
        volume: "bosn-ci-cache-v1".into(),
        image: format!("docker.io/library/docker@sha256:{id}"),
        created_at: 1.0,
        role: Some(CacheHelperRole::MaintenanceV1),
    };
    let snapshot = MaintenanceSnapshot {
        schema_version: 1,
        observed_at: 4.0,
        helper: Some(MaintenanceHelper {
            nonce: NONCE.into(),
            container_id: id.clone(),
            cleanup_error: None,
        }),
        outcome: MaintenanceOutcome::Observed {
            exit_code: 0,
            partial: false,
            budget_bytes: 100,
            remaining_completed_bytes: Some(50),
            protected_bytes: Some(10),
            budget_met: Some(true),
            reclaimed_archive_bytes: Some(80),
        },
        recovery_error: None,
        action_outcome: Some(action_observed()),
        image_outcome: Some(action_observed()),
        tool_outcome: Some(ToolMaintenanceOutcome::Observed {
            stats: ToolMaintenanceStats {
                allocated_before: 80,
                allocated_after: 50,
                retired_generations: 1,
                retired_objects: 1,
            },
        }),
    };
    let mut partial = snapshot.clone();
    if let MaintenanceOutcome::Observed { partial, .. } = &mut partial.outcome {
        *partial = true;
    }
    assert!(
        partial.validate().is_err(),
        "partial root cannot claim total reclamation"
    );
    if let MaintenanceOutcome::Observed {
        reclaimed_archive_bytes,
        ..
    } = &mut partial.outcome
    {
        *reclaimed_archive_bytes = None;
    }
    partial.validate().unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_cache_helper(&intent).unwrap();
    tx.register_cache_helper(NONCE, &id, 2.0).unwrap();
    assert!(tx.record_cache_maintenance(&snapshot).is_err());
    tx.finish_cache_helper(NONCE, &id, 3.0).unwrap();
    tx.record_cache_maintenance(&snapshot).unwrap();
    tx.commit().unwrap();
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    assert_eq!(
        registry.latest_cache_maintenance().unwrap(),
        Some(snapshot.clone())
    );
    let mut invalid = snapshot.clone();
    invalid.helper.as_mut().unwrap().container_id = "2".repeat(64);
    let mut tx = registry.begin_immediate().unwrap();
    assert!(tx.record_cache_maintenance(&invalid).is_err());
    tx.commit().unwrap();
    assert_eq!(registry.latest_cache_maintenance().unwrap(), Some(snapshot));
    let unknown = MaintenanceSnapshot {
        schema_version: 1,
        observed_at: 5.0,
        helper: None,
        outcome: MaintenanceOutcome::Unknown {
            diagnostic: "transport timeout; totals unknown".into(),
        },
        recovery_error: Some("cleanup remains pending".into()),
        action_outcome: None,
        image_outcome: None,
        tool_outcome: None,
    };
    let mut tx = registry.begin_immediate().unwrap();
    tx.record_cache_maintenance(&unknown).unwrap();
    tx.commit().unwrap();
    drop(registry);
    assert_eq!(
        Registry::open_read_only(&path)
            .unwrap()
            .latest_cache_maintenance()
            .unwrap(),
        Some(unknown)
    );
}

#[test]
fn action_evidence_requires_a_helper_and_a_met_budget() {
    let mut snapshot = MaintenanceSnapshot {
        schema_version: 1,
        observed_at: 1.0,
        helper: None,
        outcome: MaintenanceOutcome::Unknown {
            diagnostic: "archive unavailable".into(),
        },
        recovery_error: None,
        image_outcome: None,
        tool_outcome: None,
        action_outcome: Some(ActionMaintenanceOutcome::Held {
            diagnostic: "reader lease busy".into(),
        }),
    };
    assert!(snapshot.validate().is_err());
    snapshot.helper = Some(MaintenanceHelper {
        nonce: NONCE.into(),
        container_id: "a".repeat(64),
        cleanup_error: None,
    });
    snapshot.validate().unwrap();
    snapshot.action_outcome = Some(ActionMaintenanceOutcome::Observed {
        stats: ActionMaintenanceStats {
            allocated_before: 8192,
            allocated_after: 8192,
            budget_bytes: 4096,
            retired_classes: 0,
        },
    });
    assert!(snapshot.validate().is_err());
    snapshot.action_outcome = None;
    let mut old = serde_json::to_value(&snapshot).unwrap();
    old.as_object_mut().unwrap().remove("action_outcome");
    let decoded: MaintenanceSnapshot = serde_json::from_value(old).unwrap();
    assert_eq!(decoded.action_outcome, None);
    decoded.validate().unwrap();
}

fn action_observed() -> ActionMaintenanceOutcome {
    ActionMaintenanceOutcome::Observed {
        stats: ActionMaintenanceStats {
            allocated_before: 8192,
            allocated_after: 0,
            budget_bytes: 4096,
            retired_classes: 1,
        },
    }
}
