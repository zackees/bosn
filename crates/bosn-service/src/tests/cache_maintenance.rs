//! Normal daemon startup maintains an existing cohort without workflow jobs.
use super::*;

#[test]
#[ignore = "requires isolated private Docker with agreed policy and verified act2.7 cache"]
fn idle_daemon_discovers_shared_policy_and_persists_maintenance_without_jobs() {
    assert_eq!(
        std::env::var("DOCKER_HOST").unwrap(),
        "tcp://bosn-456-live-v2-engine:2375"
    );
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("config.toml"), "[engine]\nspares = 0\n").unwrap();
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let observed = async_engine::timeout(Duration::from_secs(30), async {
                loop {
                    let registry =
                        Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
                    if let Some(snapshot) = registry.latest_cache_maintenance().unwrap() {
                        break snapshot;
                    }
                    async_engine::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            client.shutdown().await.unwrap();
            stopped(server).await;
            let snapshot = observed.expect("idle daemon did not record maintenance");
            assert!(
                matches!(
                    snapshot.outcome,
                    bosn_registry::cache_maintenance::MaintenanceOutcome::Observed {
                        exit_code: 0,
                        partial: false,
                        budget_met: Some(true),
                        ..
                    }
                ),
                "{:?}",
                snapshot.outcome
            );
            assert!(snapshot.recovery_error.is_none());
            let helper = snapshot.helper.unwrap();
            assert!(helper.cleanup_error.is_none());
            let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
            let record = registry.cache_helper(&helper.nonce).unwrap().unwrap();
            assert_eq!(
                record.container_id.as_deref(),
                Some(helper.container_id.as_str())
            );
            assert_eq!(
                record.state,
                bosn_registry::cache_helper::CacheHelperState::Removed
            );
        });
}
