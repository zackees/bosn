//! Automatic cache maintenance follows the state directory's explicit opt-out.
use super::*;

impl CiRuntime {
    pub(super) async fn maintain_cache_with_policy(
        &self,
        owner: &str,
        stop: &async_engine::CancellationToken,
        interval: Duration,
    ) {
        while !stop.is_cancelled() {
            if crate::managed_retention::automatic_retention_enabled(&self.state_dir) {
                let cycle = CancellationSource::new();
                let token = cycle.token();
                let worker = async {
                    self.backend
                        .maintain_existing_cohort(&self.registry, owner, &token)
                        .await;
                    cycle.cancel();
                };
                let policy = async {
                    loop {
                        if stop.is_cancelled()
                            || token.is_cancelled()
                            || !crate::managed_retention::automatic_retention_enabled(
                                &self.state_dir,
                            )
                        {
                            cycle.cancel();
                            break;
                        }
                        let slept = async_engine::cancellable(
                            stop,
                            async_engine::cancellable(&token, async_engine::sleep(interval)),
                        )
                        .await;
                        if !matches!(slept, Ok(Ok(()))) {
                            cycle.cancel();
                            break;
                        }
                    }
                };
                async_engine::join(worker, policy).await;
            }
            if async_engine::cancellable(stop, async_engine::sleep(interval))
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

    #[test]
    fn live_policy_changes_pause_and_resume_the_cache_supervisor() {
        crate::ci::lifecycle::tests::with_registry(|registry, directory| async move {
            let config = directory.join("retention.toml");
            std::fs::write(&config, "auto_retention = false\n").unwrap();
            let backend = Arc::new(crate::ci::lifecycle::tests::FakeBackend::default());
            let runtime = CiRuntime::start(&directory, registry, backend.clone(), 1);
            let stop = CancellationSource::new();
            let token = stop.token();
            let task = async_engine::launch(async move {
                runtime
                    .maintain_cache_with_policy(
                        crate::ci::lifecycle::tests::OWNER,
                        &token,
                        Duration::from_millis(10),
                    )
                    .await;
            });
            async_engine::sleep(Duration::from_millis(40)).await;
            assert_eq!(*backend.cohort_maintenances.lock().unwrap(), 0);
            std::fs::write(&config, "auto_retention = true\n").unwrap();
            wait_count(&backend, 1).await;
            std::fs::write(&config, "auto_retention = false\n").unwrap();
            async_engine::sleep(Duration::from_millis(50)).await;
            assert_eq!(*backend.cohort_maintenances.lock().unwrap(), 1);
            std::fs::write(&config, "auto_retention = true\n").unwrap();
            wait_count(&backend, 2).await;
            stop.cancel();
            async_engine::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
        });
    }

    async fn wait_count(backend: &crate::ci::lifecycle::tests::FakeBackend, expected: u32) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while *backend.cohort_maintenances.lock().unwrap() < expected {
            assert!(Instant::now() < deadline, "cache supervisor did not resume");
            async_engine::sleep(Duration::from_millis(5)).await;
        }
    }
}
