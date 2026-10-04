//! Normal daemon startup maintains an existing cohort without workflow jobs.
use super::*;

#[cfg(unix)]
#[test]
fn daemon_shutdown_does_not_wait_for_stalled_cache_discovery_client() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("config.toml"), "[engine]\nspares = 0\n").unwrap();
    let script = temporary.path().join("docker.py");
    let started = temporary.path().join("probe-started");
    std::fs::write(
        &script,
        "import os, pathlib, sys, time\nif sys.argv[2:] == ['volume', 'inspect', 'bosn-ci-cache-v1']:\n pathlib.Path(sys.argv[1]).write_text(str(os.getpid()))\n time.sleep(12)\nprint('no such volume', file=sys.stderr)\nsys.exit(1)\n",
    )
    .unwrap();
    let backend = ci::engine::DockerActBackend::new(DockerEngine::synthetic_for_test(
        "python3",
        [script.into_os_string(), started.clone().into_os_string()],
    ));
    let shutdown_at = std::sync::Mutex::new(None);
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_act_backend(Arc::new(backend))
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            async_engine::timeout(Duration::from_secs(5), async {
                while !started.exists() {
                    async_engine::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("cache discovery must enter the stalled command before shutdown");
            *shutdown_at.lock().unwrap() = Some(std::time::Instant::now());
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    // Include runtime teardown: returning the service future alone does not
    // prove the daemon process can exit while a blocking client remains.
    let latency = shutdown_at.lock().unwrap().unwrap().elapsed();
    eprintln!("stalled cache discovery daemon shutdown: {latency:?}");
    assert!(
        latency < Duration::from_secs(5),
        "shutdown took {latency:?}"
    );
    let pid = std::fs::read_to_string(started).unwrap();
    let absent = kernal_api::run_bounded_command(
        kernal_api::SpawnSpec::new("python3")
            .arg("-c")
            .arg("import os, sys\ntry: os.kill(int(sys.argv[1]), 0)\nexcept ProcessLookupError: sys.exit(0)\nsys.exit(1)")
            .arg(pid),
        Duration::from_secs(2),
        1024,
    )
    .unwrap();
    assert_eq!(
        absent.exit.raw_code(),
        0,
        "owned Docker client survived shutdown"
    );
}

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
