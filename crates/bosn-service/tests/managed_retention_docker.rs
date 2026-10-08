//! #545: real creation, declared work, idle retirement, and reclamation.

mod support;
use bosn_core::retention::RetentionPolicy;
use bosn_service::{ManifestAppTaskJobRequest, ManifestEnsureJobRequest};
use support::setup_docker::*;

/// Uses the real daemon and immutable fixture image; no handcrafted ownership labels.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
#[expect(
    clippy::too_many_lines,
    reason = "sequential end-to-end lifecycle assertions keep creation, protection and reclamation evidence together"
)]
fn live_manifest_retention_reclaims_idle_container_and_unpinned_volume() {
    let engine = DockerEngine::docker();
    pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("Dockerfile"),
        format!(
            "FROM {PINNED_ALPINE}\nLABEL bosn.retention.fixture='{}'\n",
            root.path().display()
        ),
    )
    .unwrap();
    std::fs::write(
        workspace.join("bosn.toml"),
        "[stack.app]\ndockerfile = 'Dockerfile'\n\
         [stack.app.volumes]\n\
         cache = { scope = 'stack', destination = '/cache' }\n\
         durable = { scope = 'stack', destination = '/durable', retention = 'pinned' }\n\
         [task.populate]\nstack = 'app'\n\
         cmd = 'dd if=/dev/zero of=/cache/payload bs=1048576 count=16 && sync'\n",
    )
    .unwrap();
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    // This is an explicitly initialized isolated registry, not a request to
    // reset the user's machine registry in the presence of existing resources.
    std::fs::create_dir_all(&state).unwrap();
    drop(
        Registry::create_writer(
            state.join("registry.sqlite3"),
            "00000000-0000-4000-8000-000000000545",
        )
        .unwrap(),
    );
    let mut daemon = DaemonChild::start(&state);
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let job = runtime
        .run(client.submit_manifest_ensure(ManifestEnsureJobRequest {
            workspace: workspace.clone(),
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
        }))
        .unwrap();
    wait_for_success(&runtime, &client, job);
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert!(
        registry.image_creation_intents().unwrap().is_empty(),
        "completed image preparation retained its intent"
    );
    let resources = registry.resources(0, 64).unwrap().items;
    let cleanup = ExactManifestCleanup::new(&engine, &resources);
    let container = resources
        .iter()
        .find(|resource| resource.kind == ResourceKind::Container)
        .unwrap();
    let cache = resources
        .iter()
        .find(|resource| {
            resource.kind == ResourceKind::Volume
                && resource.retention == bosn_core::Retention::Warm
        })
        .unwrap();
    let durable = resources
        .iter()
        .find(|resource| {
            resource.kind == ResourceKind::Volume
                && resource.retention == bosn_core::Retention::Pinned
        })
        .unwrap();
    let image = resources
        .iter()
        .find(|resource| resource.kind == ResourceKind::Image)
        .unwrap();
    let task = runtime
        .run(client.submit_manifest_app_task(ManifestAppTaskJobRequest {
            workspace: workspace.clone(),
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            task_name: "populate".into(),
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
        }))
        .unwrap();
    wait_for_success(&runtime, &client, task);
    let allocated = docker_capture(
        &engine,
        [
            "exec",
            container.name.as_str(),
            "du",
            "-k",
            "/cache/payload",
        ],
    );
    assert!(allocated.ok());
    let kib: u64 = std::str::from_utf8(&allocated.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        kib >= 16 * 1024,
        "fixture did not allocate its expected storage"
    );
    assert!(
        inspect_container(&engine, &container.name)
            .unwrap()
            .unwrap()
            .running
    );
    let policy = RetentionPolicy {
        container_ttl: Duration::ZERO,
        volume_ttl: Duration::ZERO,
        image_ttl: Duration::ZERO,
        max_bytes: None,
    };
    let preview = runtime
        .run(managed_retention_when_admitted(&client, policy, false))
        .unwrap();
    assert!(!preview.applied);
    assert!(
        inspect_container(&engine, &container.name)
            .unwrap()
            .unwrap()
            .running
    );
    let applied = runtime
        .run(managed_retention_when_admitted(&client, policy, true))
        .unwrap();
    assert!(applied.refused.is_none(), "{applied:?}");
    assert!(
        inspect_container(&engine, &container.name)
            .unwrap()
            .is_none(),
        "{applied:?}"
    );
    let cache_after = docker_capture(&engine, ["volume", "inspect", cache.name.as_str()]);
    let durable_after = docker_capture(&engine, ["volume", "inspect", durable.name.as_str()]);
    let image_after = docker_capture(&engine, ["image", "inspect", image.generation.as_str()]);
    runtime.run(client.shutdown()).unwrap();
    assert!(daemon.wait_for_exit().success());
    // Remove only our exact pinned fixture after completing the safety assertion.
    let _ = docker_capture(&engine, ["volume", "rm", durable.name.as_str()]);
    let _ = docker_capture(&engine, ["volume", "rm", cache.name.as_str()]);
    let _ = docker_capture(&engine, ["image", "rm", image.generation.as_str()]);
    drop(cleanup);
    assert!(
        !cache_after.ok(),
        "unpinned volume remains after its container was reclaimed: {applied:?}"
    );
    assert!(
        durable_after.ok(),
        "explicitly pinned data was deleted: {applied:?}"
    );
    assert!(
        !image_after.ok(),
        "eligible production image remains: {applied:?}"
    );
}

#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
#[expect(
    clippy::too_many_lines,
    reason = "sequential failure and cleanup evidence from real production creation"
)]
fn failed_manifest_start_leaves_reclaimable_intent_backed_volumes() {
    let engine = DockerEngine::docker();
    pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    drop(
        Registry::create_writer(
            state.join("registry.sqlite3"),
            "00000000-0000-4000-8000-000000000550",
        )
        .unwrap(),
    );
    std::fs::write(workspace.join("Dockerfile"), format!("FROM {PINNED_ALPINE}\nLABEL bosn.retention.failure='{}'\nENTRYPOINT [\"/bosn-fixture-missing-entrypoint\"]\n", root.path().display())).unwrap();
    std::fs::write(workspace.join("bosn.toml"), "[stack.app]\ndockerfile = 'Dockerfile'\n[stack.app.volumes]\ncache = { scope = 'stack', destination = '/cache' }\ndurable = { scope = 'stack', destination = '/durable', retention = 'pinned' }\n").unwrap();
    let _cleanup = FailedEnsureCleanup {
        engine: engine.clone(),
        state: state.clone(),
    };
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut daemon = DaemonChild::start_with_retention_root(&state, &root.path().join("machine"));
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let job = runtime
        .run(client.submit_manifest_ensure(ManifestEnsureJobRequest {
            workspace,
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
        }))
        .unwrap();
    let deadline = Instant::now() + JOB_DEADLINE;
    let failed = loop {
        let status = runtime.run(client.job_status(job)).unwrap();
        if !matches!(status.state.as_str(), "Queued" | "Running" | "Cancelling") {
            break status;
        }
        assert!(Instant::now() < deadline, "failed-start job did not finish");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(failed.state, "Failed", "{failed:?}");
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert!(
        registry.image_creation_intents().unwrap().is_empty(),
        "completed image preparation retained its intent"
    );
    let resources = registry.resources(0, 64).unwrap().items;
    assert_eq!(
        registry.volume_creation_intents(0, 64).unwrap().items.len(),
        2
    );
    let container = resources
        .iter()
        .find(|row| row.kind == ResourceKind::Container)
        .unwrap();
    let observed = inspect_container(&engine, &container.name)
        .unwrap()
        .unwrap();
    assert!(!observed.running);
    let image = resources
        .iter()
        .find(|row| row.kind == ResourceKind::Image)
        .unwrap();
    for volume in resources
        .iter()
        .filter(|row| row.kind == ResourceKind::Volume)
    {
        assert!(docker_capture(&engine, ["volume", "inspect", volume.name.as_str()]).ok());
    }
    let policy = RetentionPolicy {
        container_ttl: Duration::ZERO,
        volume_ttl: Duration::ZERO,
        image_ttl: Duration::ZERO,
        ..RetentionPolicy::default()
    };
    let collected = runtime
        .run(managed_retention_when_admitted(&client, policy, true))
        .unwrap();
    assert!(collected.refused.is_none(), "{collected:?}");
    assert!(
        inspect_container(&engine, &container.name)
            .unwrap()
            .is_none(),
        "{collected:?}"
    );
    assert!(
        !docker_capture(&engine, ["image", "inspect", image.generation.as_str()]).ok(),
        "{collected:?}"
    );
    for volume in resources
        .iter()
        .filter(|row| row.kind == ResourceKind::Volume)
    {
        assert_eq!(
            docker_capture(&engine, ["volume", "inspect", volume.name.as_str()]).ok(),
            volume.retention == bosn_core::Retention::Pinned,
            "{collected:?}"
        );
    }
    let remaining = registry.resources(0, 64).unwrap().items;
    assert_eq!(
        remaining.len(),
        1,
        "deleted engine objects retained ownership rows: {remaining:?}"
    );
    assert_eq!(remaining[0].retention, bosn_core::Retention::Pinned);
    let intents = registry.volume_creation_intents(0, 64).unwrap().items;
    assert_eq!(
        intents.len(),
        1,
        "reclaimed volume retained its creation intent: {intents:?}"
    );
    assert_eq!(intents[0].name, remaining[0].name);
    runtime.run(client.shutdown()).unwrap();
    assert!(daemon.wait_for_exit().success());
}

/// Read exact planned rows at teardown even when failure occurs before the receipt.
struct FailedEnsureCleanup {
    engine: DockerEngine,
    state: std::path::PathBuf,
}

impl Drop for FailedEnsureCleanup {
    fn drop(&mut self) {
        if let Ok(registry) = Registry::open_read_only(self.state.join("registry.sqlite3"))
            && let Ok(resources) = registry.resources(0, 64)
        {
            drop(ExactManifestCleanup::new(&self.engine, &resources.items));
        }
    }
}
