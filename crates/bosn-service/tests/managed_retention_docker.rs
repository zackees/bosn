//! Opt-in, live-Docker proof that managed retention reclaims what the real setup path creates
//! (#545): container first, then its Bosn-built image, with exact disappearance and measured
//! bytes. Every object it touches is created by this test from a unique document; the pass runs
//! against this test's own temporary registry, so every other object on the engine is foreign
//! to it and held.
//!
//! Run with:
//! `soldr cargo test -j1 -p bosn-service --test managed_retention_docker --locked -- --ignored --exact live_docker_managed_retention_reclaims_a_real_setup_app`

mod support;

use support::setup_docker::*;

#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_managed_retention_reclaims_a_real_setup_app() {
    let engine = DockerEngine::docker();
    let base = pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let config = root.path().join("setup.toml");
    let unique = test_unique_suffix();
    own_registry(&state);
    let dockerfile =
        format!("FROM {PINNED_ALPINE}\nRUN printf '%s\\n' bosn-retention-{unique} > /proof\n");
    std::fs::write(
        &config,
        format!(
            "version = 1\n[app]\ndockerfile = '''{dockerfile}'''\ncommand = 'exec sleep 600'\n"
        ),
    )
    .expect("write inline setup document");
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("construct runtime");
    let plan = runtime
        .run(plan_setup(SetupPlanRequest {
            state_dir: state.clone(),
            workspace: workspace.clone(),
            locator: config.to_string_lossy().into_owned(),
            policy: SetupAcquirePolicy::OnlineRefresh,
        }))
        .expect("plan inline setup document");
    let image_tag = format!("bosn-setup:{}", plan.content_sha256);
    assert!(
        setup_container_for(&engine, &plan.content_sha256).is_none(),
        "unique test container already exists; refusing to touch it"
    );
    let _cleanup = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: String::new(),
        content_sha256: plan.content_sha256.clone(),
    };

    // Real production creation: a daemon runs setup ensure.
    let mut daemon = DaemonChild::start(&state);
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let job = runtime
        .run(client.submit_setup_ensure(SetupEnsureJobRequest {
            workspace: workspace.clone(),
            config: config.to_string_lossy().into_owned(),
            policy: SetupPreparePolicy::Refresh,
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
        }))
        .expect("submit setup ensure");
    wait_for_success(&runtime, &client, job);
    runtime.run(client.shutdown()).expect("shut down daemon");
    assert!(daemon.wait_for_exit().success(), "daemon failed");
    let container =
        setup_container_for(&engine, &plan.content_sha256).expect("the ensured app container");
    let image = image_identity_for(&engine, &image_tag);
    assert_ne!(image, base, "the inline build produced its own image");

    let zero = bosn_core::retention::RetentionPolicy {
        container_ttl: Duration::ZERO,
        volume_ttl: Duration::ZERO,
        image_ttl: Duration::ZERO,
        max_bytes: None,
    };
    let before = inspect_container(&engine, &container)
        .unwrap()
        .expect("container exists");
    assert!(before.running, "the ensured app is running");
    // Running: in use, nothing of ours is removed.
    let held = bosn_service::managed_retention::managed_retention_pass(&engine, &state, zero, true);
    assert_eq!(held.summary.refused, None, "the read was complete");
    eprintln!("running pass: {:?}", held.summary);
    assert!(inspect_container(&engine, &container).unwrap().is_some());
    assert!(docker_capture(&engine, ["image", "inspect", image_tag.as_str()]).ok());

    // Stopped (as idle retirement leaves a keepalive): container, then image, go.
    assert!(docker_capture(&engine, ["stop", "--time", "1", container.as_str()]).ok());
    let first =
        bosn_service::managed_retention::managed_retention_pass(&engine, &state, zero, true);
    assert_eq!(first.summary.refused, None);
    assert!(
        inspect_container(&engine, &container).unwrap().is_none(),
        "the stopped setup container was reclaimed: {:?}",
        first.summary
    );
    // The image was still referenced when the first pass read it; the next pass takes it.
    let second =
        bosn_service::managed_retention::managed_retention_pass(&engine, &state, zero, true);
    assert_eq!(second.summary.refused, None);
    assert!(
        !docker_capture(&engine, ["image", "inspect", image.as_str()]).ok(),
        "the Bosn-built image was reclaimed: {:?}",
        second.summary
    );
    let removed = first.summary.removed + second.summary.removed;
    let bytes = first.summary.removed_bytes + second.summary.removed_bytes;
    assert!(removed >= 2, "container and image removed, got {removed}");
    assert!(bytes > 0, "released storage is measured");
    // The shared base image was never ours to remove.
    assert!(docker_capture(&engine, ["image", "inspect", base.as_str()]).ok());
    eprintln!("managed retention released {removed} object(s), {bytes} bytes");
}

/// This test's own registry: on a machine that already has Bosn objects, a daemon refuses to
/// mint an identity in an empty state directory (#515), and the pass must judge every
/// pre-existing object as foreign to it.
fn own_registry(state: &Path) {
    std::fs::create_dir_all(state).expect("create state directory");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let registry_id = format!(
        "{:08x}-0000-4000-8000-{:012x}",
        (nanos >> 48) as u32,
        nanos & 0xffff_ffff_ffff
    );
    drop(Registry::create_writer(state.join("registry.sqlite3"), &registry_id).expect("registry"));
}
