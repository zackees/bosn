//! Diagnostic read failures remain visible in the public runner reply.

use super::*;

#[test]
fn failed_cache_measurement_is_unknown_and_partial_in_the_runner_reply() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            cache_measure: true,
            ..Default::default()
        }));
        let runtime = CiRuntime::start(&dir, registry, backend, 1);
        let reply: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::CacheUsage,
            },
        )
        .await;
        let cache = reply.cache.unwrap();
        assert_eq!(
            reply.maintenance,
            Some(crate::ci::maintenance_status::MaintenanceStatus::NeverObserved)
        );
        assert!(cache.partial);
        assert_eq!(cache.bytes, None);
        assert!(cache.errors[0].contains("Docker unavailable"));
    });
}

#[test]
fn runner_cache_reports_unknown_maintenance_without_exposing_diagnostics() {
    with_registry(|registry, dir| async move {
        use crate::act_registry::{ActRegistryCommand, ActRegistryReply};
        use crate::ci::maintenance_status::{MaintenanceResult, MaintenanceStatus};
        use bosn_registry::cache_maintenance::{MaintenanceOutcome, MaintenanceSnapshot};
        let snapshot = MaintenanceSnapshot {
            schema_version: 1,
            observed_at: 123.0,
            helper: None,
            outcome: MaintenanceOutcome::Unknown {
                diagnostic: "private diagnostic".into(),
            },
            recovery_error: Some("private recovery output".into()),
            action_outcome: None,
            image_outcome: None,
            tool_outcome: None,
        };
        assert!(matches!(
            registry
                .act_registry(ActRegistryCommand::MaintenanceRecord(snapshot))
                .await
                .unwrap(),
            ActRegistryReply::Committed
        ));
        let runtime = CiRuntime::start(&dir, registry, Arc::new(FakeBackend::default()), 1);
        let reply: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::CacheUsage,
            },
        )
        .await;
        assert_eq!(
            reply.maintenance,
            Some(MaintenanceStatus::Recorded {
                observed_at: 123.0,
                recovery_failed: true,
                cleanup_failed: false,
                outcome: MaintenanceResult::Unknown,
                tool_maintenance: None,
                action_maintenance: None,
                image_maintenance: None,
            })
        );
        assert!(!reply.to_json().contains("private"));
    });
}
