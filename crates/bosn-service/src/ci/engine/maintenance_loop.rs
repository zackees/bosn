//! Cancellable scheduling independent of workflow engines.
use super::{DockerActBackend, HelperCleanupRetry, MaintenanceHelperAttempt};
use crate::{RegistryActor, ci::cache_policy::CachePolicy};
use kernal_api::async_engine::{self, CancellationToken};
use std::time::Duration;

pub struct MaintenanceTick {
    pub persistence: Result<(), String>,
    /// Recovery advances fairly even if a prior helper cannot be observed.
    pub recovery: Result<HelperCleanupRetry, String>,
    /// Separate maintenance and cleanup results are preserved inside the attempt.
    pub attempt: Result<MaintenanceHelperAttempt, String>,
}

impl DockerActBackend {
    /// The caller supplies an enrolled machine policy and consumes every tick.
    /// No repository configuration or legacy record authorizes this loop.
    /// Restart runs immediately; subsequent ticks wait after the previous pass.
    pub async fn supervise_cache_maintenance(
        &self,
        registry: &RegistryActor,
        owner: &str,
        policy: CachePolicy,
        stop: &CancellationToken,
        reports: &async_engine::Sender<MaintenanceTick>,
    ) {
        let mut cursor = None;
        while !stop.is_cancelled() {
            let recovery = match async_engine::cancellable(
                stop,
                async_engine::timeout(
                    Duration::from_secs(200),
                    self.retry_measurements(registry, owner, cursor.clone()),
                ),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err("maintenance helper recovery deadline exceeded".into()),
                Err(_) => return,
            };
            if let Ok(report) = &recovery {
                cursor.clone_from(&report.next_nonce);
            }
            let attempt = match async_engine::cancellable(
                stop,
                async_engine::timeout(
                    Duration::from_secs(600),
                    self.maintain_cache_with_helper(registry, owner, policy),
                ),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(
                    "maintenance pass deadline exceeded; helper journal needs reconciliation"
                        .into(),
                ),
                Err(_) => return,
            };
            let mut tick = MaintenanceTick {
                recovery,
                attempt,
                persistence: Ok(()),
            };
            tick.persistence = match async_engine::cancellable(
                stop,
                super::maintenance_reporting::persist(registry, &tick),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => return,
            };
            // Backpressure prevents an unbounded history or unsupervised next
            // pass. Shutdown also interrupts a disconnected/stalled consumer.
            if !matches!(
                async_engine::cancellable(stop, reports.send(tick)).await,
                Ok(Ok(()))
            ) {
                return;
            }
            if async_engine::cancellable(
                stop,
                async_engine::sleep(Duration::from_secs(policy.maintenance_interval_secs)),
            )
            .await
            .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::lifecycle::tests::{OWNER, with_registry};
    use kernal_api::async_engine::CancellationSource;

    #[test]
    #[ignore = "requires isolated private Docker with shared verified act2.7 cache"]
    fn periodic_maintenance_runs_without_workflows_and_restarts_immediately() {
        assert!(
            std::env::var("DOCKER_HOST")
                .unwrap()
                .contains("bosn-456-live-v2-engine")
        );
        with_registry(|registry, _directory| async move {
            let backend = DockerActBackend::default();
            let policy: CachePolicy = toml::from_str("repository_max_bytes=104857600\naggregate_max_bytes=209715200\nmax_age_secs=2592000\nunused_age_secs=604800\nmaintenance_interval_secs=60\n").unwrap();
            let (reports, mut receiver) = async_engine::channel(1);
            let stop = CancellationSource::new();
            let token = stop.token();
            let worker =
                backend.supervise_cache_maintenance(&registry, OWNER, policy, &token, &reports);
            let observer = async {
                let mut ids = Vec::new();
                for index in 0..2 {
                    let report = async_engine::timeout(
                        Duration::from_secs(if index == 0 { 15 } else { 75 }),
                        receiver.recv(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    assert!(report.recovery.is_ok());
                    assert!(report.persistence.is_ok(), "{:?}", report.persistence);
                    let attempt = report.attempt.unwrap();
                    assert!(attempt.cleanup.is_ok(), "{:?}", attempt.cleanup);
                    attempt.outcome.unwrap().require_complete().unwrap();
                    backend
                        .confirm_measurement_absent(&attempt.container_id)
                        .await
                        .unwrap();
                    ids.push(attempt.container_id);
                }
                assert_ne!(ids[0], ids[1]);
                stop.cancel();
            };
            async_engine::join(worker, observer).await;
            // A new supervisor has no stale in-memory next-run deadline.
            let restart = CancellationSource::new();
            let token = restart.token();
            let worker =
                backend.supervise_cache_maintenance(&registry, OWNER, policy, &token, &reports);
            let observer = async {
                let report = async_engine::timeout(Duration::from_secs(15), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(report.persistence.is_ok(), "{:?}", report.persistence);
                let snapshot =
                    bosn_registry::Registry::open_read_only(_directory.join("registry.sqlite3"))
                        .unwrap()
                        .latest_cache_maintenance()
                        .unwrap()
                        .unwrap();
                assert!(matches!(
                    snapshot.outcome,
                    bosn_registry::cache_maintenance::MaintenanceOutcome::Observed {
                        partial: false,
                        ..
                    }
                ));
                let attempt = report.attempt.unwrap();
                assert_eq!(snapshot.helper.unwrap().container_id, attempt.container_id);
                assert!(attempt.cleanup.is_ok());
                attempt.outcome.unwrap().require_complete().unwrap();
                restart.cancel();
            };
            async_engine::join(worker, observer).await;
            backend
                .verify_measured_volume(super::super::CACHE_VOLUME)
                .await
                .unwrap();
        });
    }
}
