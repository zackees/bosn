//! Include runtime teardown when checking uncertain maintenance cancellation.
use super::*;
use kernal_api::async_engine::{self, CancellationSource, RuntimeBuilder};

#[test]
fn stalled_maintenance_client_does_not_delay_runtime_shutdown() {
    let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let marker = directory.path().join("entered");
    let script = directory.path().join("docker.py");
    std::fs::write(
        &script,
        "import os, pathlib, sys, time\nassert 'prune-cohort' in sys.argv\npathlib.Path(sys.argv[1]).write_text(str(os.getpid()))\ntime.sleep(12)\nsys.exit(1)\n",
    )
    .unwrap();
    let backend = DockerActBackend::new(bosn_engine::DockerEngine::synthetic_for_test(
        "python3",
        [script.into_os_string(), marker.clone().into_os_string()],
    ));
    let policy = toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").unwrap();
    let cancelled_at = std::sync::Mutex::new(None);
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let source = CancellationSource::new();
            let token = source.token();
            let attempt = async_engine::cancellable(
                &token,
                backend.maintain_cache_cohort("verified-test-helper", policy),
            );
            let observer = async {
                async_engine::timeout(Duration::from_secs(5), async {
                    while !marker.exists() {
                        async_engine::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("maintenance command must start before cancellation");
                *cancelled_at.lock().unwrap() = Some(std::time::Instant::now());
                source.cancel();
            };
            let (result, ()) = async_engine::join(attempt, observer).await;
            assert!(result.is_err());
        });
    let latency = cancelled_at.lock().unwrap().unwrap().elapsed();
    eprintln!("stalled maintenance runtime shutdown: {latency:?}");
    assert!(
        latency < Duration::from_secs(5),
        "shutdown took {latency:?}"
    );
    let pid = std::fs::read_to_string(marker).unwrap();
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
        "owned maintenance client survived"
    );
}
