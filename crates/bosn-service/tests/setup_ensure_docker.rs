//! Opt-in, live-Docker proof for the daemon-owned setup-app ensure path.
//!
//! This test is intentionally ignored: it creates one short-lived container
//! through the production daemon and therefore needs a Docker daemon and the
//! pinned Alpine image documented below.  Its drop guard removes only the
//! exact deterministic container after re-checking Bosn's ownership labels.

mod support;

use support::setup_docker::*;

/// Run with:
/// `cargo test -p bosn-service --test setup_ensure_docker -- --ignored --exact live_docker_setup_ensure_creates_and_reuses_one_managed_app`
///
/// It needs a usable local Docker daemon and the exact `PINNED_ALPINE` image.
/// The test does not pull an unpinned image, and its cleanup refuses to remove
/// any candidate whose three expected Bosn ownership labels do not match.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn live_docker_setup_ensure_creates_and_reuses_one_managed_app() {
    let engine = DockerEngine::docker();
    let expected_image = pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let config_root = root.path().join("config");
    std::fs::create_dir_all(&workspace).expect("create empty workspace");
    std::fs::create_dir_all(&config_root).expect("create config directory");
    let config = config_root.join("setup.toml");
    let unique = test_unique_suffix();
    std::fs::write(
        &config,
        format!(
            "version = 1\n[app]\nimage = '{PINNED_ALPINE}'\ncommand = 'exec sleep 120 # bosn-live-{unique}'\n"
        ),
    )
    .expect("write setup document");

    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("construct kernal-api runtime");
    // Plan through the public setup API before the daemon starts so the test
    // can derive its exact deterministic name for ownership-checked cleanup.
    let plan = runtime
        .run(plan_setup(SetupPlanRequest {
            state_dir: state.clone(),
            workspace: workspace.clone(),
            locator: config.to_string_lossy().into_owned(),
            policy: SetupAcquirePolicy::OnlineRefresh,
        }))
        .expect("plan pinned setup document without Docker");
    assert!(
        setup_container_for(&engine, &plan.content_sha256).is_none(),
        "unique test container name already exists; refusing to touch it"
    );
    let cleanup = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: String::new(),
        content_sha256: plan.content_sha256.clone(),
    };
    let request = SetupEnsureJobRequest {
        workspace: workspace.clone(),
        config: config.to_string_lossy().into_owned(),
        policy: SetupPreparePolicy::Refresh,
        deadline: JOB_DEADLINE,
        output_limit: OUTPUT_LIMIT,
    };

    let mut first_daemon = DaemonChild::start(&state);
    let first_client = wait_for_client(&runtime, &mut first_daemon, &state);
    let first_job = runtime
        .run(first_client.submit_setup_ensure(request.clone()))
        .expect("submit first production setup ensure job");
    wait_for_success(&runtime, &first_client, first_job);
    let container_name =
        setup_container_for(&engine, &plan.content_sha256).expect("the ensured app container");
    let first = inspect_container(&engine, &container_name)
        .expect("inspect first setup app")
        .expect("first setup app exists");
    assert!(first.running, "first setup app is not running");
    assert_eq!(first.image, expected_image, "managed app image identity");
    assert_eq!(first.managed, "v1", "managed ownership label");
    assert_eq!(
        first.content_sha256, plan.content_sha256,
        "content ownership label"
    );
    assert_eq!(
        first.container_name, container_name,
        "container-name ownership label"
    );

    runtime
        .run(first_client.shutdown())
        .expect("shut down first daemon");
    assert!(
        first_daemon.wait_for_exit().success(),
        "first daemon failed"
    );

    // A new daemon has an empty in-memory scheduler.  The same request must
    // still reuse the inspected, matching container rather than replacing it.
    let mut second_daemon = DaemonChild::start(&state);
    let second_client = wait_for_client(&runtime, &mut second_daemon, &state);
    let second_job = runtime
        .run(second_client.submit_setup_ensure(request))
        .expect("submit second production setup ensure job");
    wait_for_success(&runtime, &second_client, second_job);
    let second = inspect_container(&engine, &container_name)
        .expect("inspect reused setup app")
        .expect("reused setup app exists");
    assert!(second.running, "reused setup app is not running");
    assert_eq!(
        second.id, first.id,
        "matching app was replaced instead of reused"
    );
    assert_eq!(second.image, expected_image, "reused app image identity");
    assert_eq!(second.managed, "v1", "reused managed ownership label");
    assert_eq!(
        second.content_sha256, plan.content_sha256,
        "reused content label"
    );
    assert_eq!(
        second.container_name, container_name,
        "reused container-name label"
    );

    runtime
        .run(second_client.shutdown())
        .expect("shut down second daemon");
    assert!(
        second_daemon.wait_for_exit().success(),
        "second daemon failed"
    );
    assert!(
        std::fs::read_dir(&workspace)
            .expect("read workspace")
            .next()
            .is_none(),
        "setup ensure wrote into the selected workspace"
    );
    drop(cleanup);
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect exact container after cleanup")
            .is_none(),
        "exact live-test container remained after cleanup"
    );
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test setup_ensure_docker --locked -- --ignored --exact live_docker_setup_reconcile_repair_missing_retires_then_ensure_recreates_app`
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn live_docker_setup_reconcile_repair_missing_retires_then_ensure_recreates_app() {
    let engine = DockerEngine::docker();
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let config_root = root.path().join("config");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::create_dir_all(&config_root).expect("create config root");
    let config = config_root.join("setup.toml");
    std::fs::write(&config, format!("version = 1\n[app]\nimage = '{PINNED_ALPINE}'\ncommand = 'exec sleep 120 # reconcile-{}'\n", test_unique_suffix())).expect("write config");
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let plan = runtime
        .run(plan_setup(SetupPlanRequest {
            state_dir: state.clone(),
            workspace: workspace.clone(),
            locator: config.to_string_lossy().into_owned(),
            policy: SetupAcquirePolicy::OnlineRefresh,
        }))
        .expect("plan");
    let cleanup = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: String::new(),
        content_sha256: plan.content_sha256.clone(),
    };
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
        .expect("submit ensure");
    wait_for_success(&runtime, &client, job);
    let container_name =
        setup_container_for(&engine, &plan.content_sha256).expect("the ensured app container");
    let matching = runtime
        .run(client.setup_reconcile_preview(&workspace, 0, 1))
        .expect("matching preview");
    assert_eq!(matching.records.len(), 1);
    assert_eq!(matching.records[0].drift, "matching_running");
    let observed = inspect_container(&engine, &container_name)
        .expect("inspect")
        .expect("managed app");
    assert_eq!(observed.managed, "v1");
    assert_eq!(observed.content_sha256, plan.content_sha256);
    assert_eq!(observed.container_name, container_name);
    let removed = docker_capture(
        &engine,
        ["container", "rm", "--force", container_name.as_str()],
    );
    assert!(removed.ok(), "exact managed test removal failed");
    let missing = runtime
        .run(client.setup_reconcile_preview(&workspace, 0, 1))
        .expect("missing preview");
    assert_eq!(missing.records.len(), 1);
    assert_eq!(missing.records[0].drift, "missing");
    let repair_token = missing.records[0]
        .repair_token
        .as_deref()
        .expect("missing active app has opaque repair token");
    let repaired = runtime
        .run(client.setup_reconcile_repair_missing(&workspace, repair_token, true))
        .expect("repair exact missing app");
    assert!(repaired.repaired);
    assert!(!repaired.already_repaired);
    let repeated = runtime
        .run(client.setup_reconcile_repair_missing(&workspace, repair_token, true))
        .expect("repeat exact missing repair");
    assert!(!repeated.repaired);
    assert!(repeated.already_repaired);
    assert!(
        inspect_container(&engine, &container_name)
            .expect("repair never mutates Docker")
            .is_none(),
        "repair recreated or otherwise changed missing app"
    );
    let second_job = runtime
        .run(client.submit_setup_ensure(SetupEnsureJobRequest {
            workspace: workspace.clone(),
            config: config.to_string_lossy().into_owned(),
            policy: SetupPreparePolicy::Offline,
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
        }))
        .expect("submit recreation ensure");
    wait_for_success(&runtime, &client, second_job);
    let recreated = inspect_container(&engine, &container_name)
        .expect("inspect recreated app")
        .expect("ensure recreated managed app");
    assert_ne!(
        recreated.id, observed.id,
        "ensure did not recreate missing app"
    );
    assert_eq!(
        recreated.image, observed.image,
        "current image was retained"
    );
    runtime.run(client.shutdown()).expect("shutdown daemon");
    assert!(daemon.wait_for_exit().success());
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).expect("open registry");
    assert!(
        registry
            .resources(0, 32)
            .expect("resources")
            .items
            .iter()
            .any(
                |resource| resource.id == format!("setup-container:setup:{container_name}")
                    && resource.state == ResourceState::Active
            )
    );
    drop(cleanup);
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect exact cleanup")
            .is_none(),
        "exact cleanup retained recreated app"
    );
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test setup_ensure_docker --locked -- --ignored --exact live_docker_setup_adopt_restores_lost_registry_without_touching_app`
///
/// This intentionally deletes only the disposable test registry after its
/// daemon is cleanly stopped. It preserves the private setup cache and proves
/// adoption is a registry restoration, not Docker lifecycle control.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn live_docker_setup_adopt_restores_lost_registry_without_touching_app() {
    let engine = DockerEngine::docker();
    let expected_image = pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let config_root = root.path().join("config");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::create_dir_all(&config_root).expect("create config directory");
    let config = config_root.join("setup.toml");
    let unique = test_unique_suffix();
    std::fs::write(
        &config,
        format!(
            "version = 1\n[app]\nimage = '{PINNED_ALPINE}'\ncommand = 'exec sleep 120 # bosn-adopt-{unique}'\n"
        ),
    )
    .expect("write setup document");
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let plan = runtime
        .run(plan_setup(SetupPlanRequest {
            state_dir: state.clone(),
            workspace: workspace.clone(),
            locator: config.to_string_lossy().into_owned(),
            policy: SetupAcquirePolicy::OnlineRefresh,
        }))
        .expect("plan setup");
    assert!(
        setup_container_for(&engine, &plan.content_sha256).is_none(),
        "refuse colliding test name"
    );
    let cleanup = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: String::new(),
        content_sha256: plan.content_sha256.clone(),
    };
    let ensure = SetupEnsureJobRequest {
        workspace: workspace.clone(),
        config: config.to_string_lossy().into_owned(),
        policy: SetupPreparePolicy::Refresh,
        deadline: JOB_DEADLINE,
        output_limit: OUTPUT_LIMIT,
    };
    let mut first_daemon = DaemonChild::start(&state);
    let first_client = wait_for_client(&runtime, &mut first_daemon, &state);
    let job = runtime
        .run(first_client.submit_setup_ensure(ensure))
        .expect("submit ensure");
    wait_for_success(&runtime, &first_client, job);
    let container_name =
        setup_container_for(&engine, &plan.content_sha256).expect("the ensured app container");
    let before = inspect_container(&engine, &container_name)
        .unwrap()
        .expect("managed app exists");
    assert!(before.running);
    assert_eq!(before.image, expected_image);
    assert_eq!(before.managed, "v1");
    assert_eq!(before.content_sha256, plan.content_sha256);
    assert_eq!(before.container_name, container_name);
    runtime
        .run(first_client.shutdown())
        .expect("shutdown first daemon");
    assert!(first_daemon.wait_for_exit().success());
    // Only this temporary registry is removed. No Docker command runs in this
    // transition and the private cache remains for the subsequent refresh.
    let registry_path = state.join("registry.sqlite3");
    assert!(
        registry_path.is_file(),
        "first daemon did not create test registry"
    );
    std::fs::remove_file(&registry_path).expect("remove only temporary registry");
    for suffix in ["-wal", "-shm"] {
        let path = state.join(format!("registry.sqlite3{suffix}"));
        if path.exists() {
            std::fs::remove_file(path).expect("remove only temporary sqlite sidecar");
        }
    }
    assert!(
        inspect_container(&engine, &container_name)
            .unwrap()
            .is_some(),
        "registry loss touched Docker container"
    );
    let mut second_daemon = DaemonChild::start(&state);
    let second_client = wait_for_client(&runtime, &mut second_daemon, &state);
    let adopted = runtime
        .run(second_client.setup_adopt(SetupAdoptRequest {
            workspace: workspace.clone(),
            config: config.to_string_lossy().into_owned(),
            policy: SetupPreparePolicy::Refresh,
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
            confirm: true,
        }))
        .expect("public client adoption");
    assert!(adopted.adopted);
    let after = inspect_container(&engine, &container_name)
        .unwrap()
        .expect("adopted app exists");
    assert!(after.running);
    assert_eq!(after.id, before.id, "adoption replaced the container");
    assert_eq!(after.image, before.image);
    runtime
        .run(second_client.shutdown())
        .expect("shutdown second daemon");
    assert!(second_daemon.wait_for_exit().success());
    let registry = Registry::open_read_only(&registry_path).expect("open restored registry");
    let resources = registry
        .resources(0, 16)
        .expect("read restored resources")
        .items;
    assert!(resources.iter().any(|r| r.kind == ResourceKind::Container
        && r.name == container_name
        && r.state == ResourceState::Active));
    assert!(
        resources
            .iter()
            .any(|r| r.kind == ResourceKind::Image && r.generation == expected_image)
    );
    let uses = registry
        .resource_uses(0, 16)
        .expect("read restored uses")
        .items;
    assert_eq!(uses.len(), 2, "container and image uses restored");
    assert!(
        registry
            .setup_ensure_events(0, 16)
            .expect("read events")
            .items
            .iter()
            .any(|event| event.kind == "setup.ensure.adopted")
    );
    drop(cleanup);
    assert!(
        inspect_container(&engine, &container_name)
            .unwrap()
            .is_none(),
        "exact test container remained after cleanup"
    );
}
