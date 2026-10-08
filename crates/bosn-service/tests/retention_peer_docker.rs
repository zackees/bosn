//! #545: a crashed workspace daemon must leave reclaimable machine-owned resources.

mod support;
use bosn_core::retention::RetentionPolicy;
use bosn_service::{ManifestAppTaskJobRequest, ManifestEnsureJobRequest};
use support::setup_docker::*;

#[test]
#[ignore = "requires local Docker and the pinned Alpine image"]
#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "sequential end-to-end lifecycle assertions keep creation, protection and reclamation evidence together"
)]
fn machine_gc_preserves_foreign_work_and_reclaims_an_offline_registry() {
    let engine = DockerEngine::docker();
    pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().unwrap();
    let machine = root.path().join("machine");
    let primary = root.path().join("primary");
    let peer = root.path().join("peer");
    let workspace = root.path().join("workspace");
    for (state, owner) in [
        (&primary, "00000000-0000-4000-8000-000000000547"),
        (&peer, "00000000-0000-4000-8000-000000000548"),
    ] {
        std::fs::create_dir(state).unwrap();
        drop(Registry::create_writer(state.join("registry.sqlite3"), owner).unwrap());
    }
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("Dockerfile"),
        format!(
            "FROM {PINNED_ALPINE}\nLABEL bosn.peer.fixture='{}'\n",
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
         [task.work]\nstack = 'app'\ncmd = 'until test -f /cache/release; do sleep 0.1; done'\n",
    )
    .unwrap();
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut primary_daemon = DaemonChild::start_with_retention_root(&primary, &machine);
    let primary_client = wait_for_client(&runtime, &mut primary_daemon, &primary);
    let mut peer_daemon = DaemonChild::start_with_retention_root(&peer, &machine);
    let peer_client = wait_for_client(&runtime, &mut peer_daemon, &peer);
    let ensure = runtime
        .run(
            peer_client.submit_manifest_ensure(ManifestEnsureJobRequest {
                workspace: workspace.clone(),
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: JOB_DEADLINE,
                output_limit: OUTPUT_LIMIT,
            }),
        )
        .unwrap();
    wait_for_success(&runtime, &peer_client, ensure);
    let registry = Registry::open_read_only(peer.join("registry.sqlite3")).unwrap();
    let resources = registry.resources(0, 64).unwrap().items;
    let cleanup = ExactManifestCleanup::new(&engine, &resources);
    let ownership =
        machine.join("retention-registries/00000000-0000-4000-8000-000000000548.ownership");
    assert_eq!(
        Registry::resolve_authority(&ownership.join("registry.sqlite3")).unwrap(),
        ownership.join("authority/registry.sqlite3")
    );
    let snapshot = Registry::open_read_only(ownership.join("registry.sqlite3")).unwrap();
    let saved = snapshot.resources(0, 64).unwrap().items;
    assert_eq!(saved.len(), resources.len());
    for resource in &resources {
        let saved = saved.iter().find(|saved| saved.id == resource.id).unwrap();
        assert_eq!(saved.retention, resource.retention);
        assert_eq!(saved.generation, resource.generation);
    }
    drop(snapshot);
    let container = resources
        .iter()
        .find(|row| row.kind == ResourceKind::Container)
        .unwrap();
    let image = resources
        .iter()
        .find(|row| row.kind == ResourceKind::Image)
        .unwrap();
    let shared_ensure = runtime
        .run(
            primary_client.submit_manifest_ensure(ManifestEnsureJobRequest {
                workspace: workspace.clone(),
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: JOB_DEADLINE,
                output_limit: OUTPUT_LIMIT,
            }),
        )
        .unwrap();
    wait_for_success(&runtime, &primary_client, shared_ensure);
    let primary_registry = Registry::open_read_only(primary.join("registry.sqlite3")).unwrap();
    assert!(
        primary_registry
            .resources(0, 64)
            .unwrap()
            .items
            .iter()
            .any(|row| row.kind == ResourceKind::Container && row.name == container.name),
        "the two production ensure paths did not share the physical container"
    );
    let work = runtime
        .run(
            peer_client.submit_manifest_app_task(ManifestAppTaskJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                task_name: "work".into(),
                deadline: JOB_DEADLINE,
                output_limit: OUTPUT_LIMIT,
            }),
        )
        .unwrap();
    let started = Instant::now();
    while registry.execution_sessions(0, 64).unwrap().items.is_empty() {
        assert!(
            started.elapsed() < READY_DEADLINE,
            "foreign task never acquired its session"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let policy = RetentionPolicy {
        container_ttl: Duration::ZERO,
        volume_ttl: Duration::ZERO,
        image_ttl: Duration::ZERO,
        max_bytes: None,
    };
    let blocked = runtime
        .run(primary_client.managed_retention(policy, true))
        .unwrap();
    assert!(
        blocked
            .refused
            .as_deref()
            .is_some_and(|reason| reason.contains("admission busy")),
        "foreign execution did not fence machine GC: {blocked:?}"
    );
    assert!(
        inspect_container(&engine, &container.name)
            .unwrap()
            .unwrap()
            .running
    );
    // Expire the primary's own idle clock while foreign work stays admitted.
    std::thread::sleep(Duration::from_secs(11));
    assert!(
        docker_capture(
            &engine,
            ["exec", container.name.as_str(), "touch", "/cache/release"]
        )
        .ok()
    );
    wait_for_success(&runtime, &peer_client, work);
    let recent = runtime
        .run(primary_client.managed_retention(
            RetentionPolicy {
                container_ttl: Duration::from_secs(10),
                ..policy
            },
            true,
        ))
        .unwrap();
    assert_eq!(
        recent.removed, 0,
        "foreign completion did not protect shared recent use: {recent:?}"
    );
    assert!(
        inspect_container(&engine, &container.name)
            .unwrap()
            .unwrap()
            .running
    );
    // Abrupt death leaves no final actor drain or snapshot publication.
    // Completed resource and pin commits must already be machine-durable.
    peer_daemon.child.kill().unwrap();
    assert!(!peer_daemon.wait_for_exit().success());
    std::fs::write(peer.join("retention.toml"), "auto_retention = false\n").unwrap();
    let opted_out = runtime
        .run(primary_client.managed_retention(policy, true))
        .unwrap();
    assert!(
        opted_out
            .held
            .iter()
            .any(|reason| reason.contains("automatic retention disabled")),
        "{opted_out:?}"
    );
    assert!(
        inspect_container(&engine, &container.name)
            .unwrap()
            .unwrap()
            .running
    );
    let retired = root.path().join("retired-peer");
    std::fs::rename(&peer, &retired).unwrap();
    let lost_opt_out = runtime
        .run(primary_client.managed_retention(policy, true))
        .unwrap();
    assert!(lost_opt_out.refused.is_none(), "{lost_opt_out:?}");
    assert_eq!(lost_opt_out.removed, 0, "{lost_opt_out:?}");
    assert!(
        lost_opt_out
            .held
            .iter()
            .any(|reason| reason.contains("automatic retention disabled")),
        "{lost_opt_out:?}"
    );
    assert!(
        inspect_container(&engine, &container.name)
            .unwrap()
            .unwrap()
            .running
    );
    // A native daemon restarting at the now-empty original path must recover
    // the published identity and saved opt-out before it accepts requests.
    std::fs::create_dir(&peer).unwrap();
    let mut recovered_daemon = DaemonChild::start_with_retention_root(&peer, &machine);
    let recovered_client = wait_for_client(&runtime, &mut recovered_daemon, &peer);
    let recovered = Registry::open_read_only(peer.join("registry.sqlite3")).unwrap();
    assert_eq!(
        recovered.registry_id().unwrap(),
        "00000000-0000-4000-8000-000000000548"
    );
    assert_eq!(
        recovered.resources(0, 64).unwrap().items.len(),
        resources.len()
    );
    assert!(!bosn_service::managed_retention::automatic_retention_enabled(&peer));
    assert!(
        !peer.join("registry.sqlite3").exists(),
        "restart minted a replacement database"
    );
    runtime.run(recovered_client.shutdown()).unwrap();
    assert!(recovered_daemon.wait_for_exit().success());
    drop(recovered);
    std::fs::rename(&peer, root.path().join("recovered-peer")).unwrap();
    std::fs::rename(&retired, &peer).unwrap();
    std::fs::write(peer.join("retention.toml"), "auto_retention = true\n").unwrap();
    let refreshed = runtime
        .run(primary_client.managed_retention(policy, false))
        .unwrap();
    assert!(refreshed.refused.is_none(), "{refreshed:?}");
    // Remove the original catalog path while retaining it for fixture teardown.
    // Collection must use stable authority, including its pinned volume.
    std::fs::rename(&peer, root.path().join("retired-peer")).unwrap();
    let collected = runtime
        .run(primary_client.managed_retention(policy, true))
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
        let exists = docker_capture(&engine, ["volume", "inspect", volume.name.as_str()]).ok();
        let _ = docker_capture(&engine, ["volume", "rm", volume.name.as_str()]);
        assert_eq!(
            exists,
            volume.retention == bosn_core::Retention::Pinned,
            "{collected:?}"
        );
    }
    let snapshot = Registry::open_read_only(ownership.join("registry.sqlite3")).unwrap();
    let remaining = snapshot.resources(0, 64).unwrap().items;
    assert_eq!(
        remaining.len(),
        1,
        "offline registry retained reclaimed objects: {remaining:?}"
    );
    assert_eq!(remaining[0].retention, bosn_core::Retention::Pinned);
    assert!(
        snapshot
            .volume_creation_intents(0, 64)
            .unwrap()
            .items
            .is_empty()
    );
    let repeated = runtime
        .run(primary_client.managed_retention(policy, true))
        .unwrap();
    assert!(
        repeated.refused.is_none(),
        "repeated GC treated proven absence as failure: {repeated:?}"
    );
    assert_eq!(repeated.removed, 0);
    runtime.run(primary_client.shutdown()).unwrap();
    assert!(primary_daemon.wait_for_exit().success());
    drop(cleanup);
}

#[test]
#[ignore = "requires local Docker and the native daemon"]
fn native_startup_exclusion_preserves_identity_and_allows_retry() {
    let root = tempfile::tempdir().unwrap();
    let machine = root.path().join("machine");
    let state = root.path().join("state");
    std::fs::create_dir(&machine).unwrap();
    std::fs::create_dir(&state).unwrap();
    let owner = "00000000-0000-4000-8000-000000000549";
    drop(Registry::create_writer(state.join("registry.sqlite3"), owner).unwrap());
    let startup =
        kernal_api::platform::fs::open_lock_file(&machine.join("registry-startup.lock")).unwrap();
    startup.try_lock().unwrap();
    let mut first = DaemonChild::start_with_retention_root(&state, &machine);
    let mut second = DaemonChild::start_with_retention_root(&state, &machine);
    assert!(!first.wait_for_exit().success());
    assert!(!second.wait_for_exit().success());
    assert_eq!(
        Registry::open_read_only(state.join("registry.sqlite3"))
            .unwrap()
            .registry_id()
            .unwrap(),
        owner
    );
    assert!(!state.join("registry.authority.json").exists());
    startup.unlock().unwrap();
    drop(startup);
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut recovered = DaemonChild::start_with_retention_root(&state, &machine);
    let client = wait_for_client(&runtime, &mut recovered, &state);
    let mut duplicate = DaemonChild::start_with_retention_root(&state, &machine);
    assert!(!duplicate.wait_for_exit().success());
    assert_eq!(
        Registry::open_read_only(state.join("registry.sqlite3"))
            .unwrap()
            .registry_id()
            .unwrap(),
        owner
    );
    runtime.run(client.shutdown()).unwrap();
    assert!(recovered.wait_for_exit().success());
}
