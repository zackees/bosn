//! #545: prepare-only production images must enter managed ownership and GC.

mod support;
use bosn_core::{Retention, retention::RetentionPolicy};
use bosn_service::{
    ManifestAppTaskJobRequest, SetupAdoptRequest, SetupAppTaskJobRequest, SetupPrepareRequest,
    SetupTaskJobRequest,
};
use support::setup_docker::*;

struct FixtureImages {
    engine: DockerEngine,
    root: String,
}

impl Drop for FixtureImages {
    fn drop(&mut self) {
        let filter = format!("label=bosn.prepare.fixture={}", self.root);
        let listed = docker_capture(
            &self.engine,
            ["image", "ls", "--quiet", "--no-trunc", "--filter", &filter],
        );
        if !listed.ok() {
            return;
        }
        let Ok(identities) = std::str::from_utf8(&listed.stdout) else {
            return;
        };
        for identity in identities.lines() {
            let proof = docker_capture(
                &self.engine,
                [
                    "image",
                    "inspect",
                    "--format",
                    "{{index .Config.Labels \"bosn.prepare.fixture\"}}",
                    identity,
                ],
            );
            if proof.ok()
                && std::str::from_utf8(&proof.stdout).is_ok_and(|label| label.trim() == self.root)
            {
                let _ = docker_capture(&self.engine, ["image", "rm", identity]);
            }
        }
    }
}

#[test]
#[ignore = "requires local Docker and the pinned Alpine image"]
fn prepare_only_image_is_recorded_before_success_and_reclaimed() {
    exercise_image_reclamation(Creation::Prepare);
}

#[test]
#[ignore = "requires local Docker and the pinned Alpine image"]
fn failed_declared_task_leaves_a_recorded_reclaimable_image() {
    exercise_image_reclamation(Creation::FailedTask);
}

#[test]
#[ignore = "requires local Docker and the pinned Alpine image"]
fn missing_app_task_leaves_a_recorded_reclaimable_image() {
    exercise_image_reclamation(Creation::MissingAppTask);
}

#[test]
#[ignore = "requires local Docker and the pinned Alpine image"]
fn missing_manifest_app_task_records_its_prepared_image() {
    exercise_image_reclamation(Creation::MissingManifestAppTask);
}

#[test]
#[ignore = "requires local Docker and the pinned Alpine image"]
fn failed_setup_adoption_records_its_prepared_image() {
    exercise_image_reclamation(Creation::MissingAdoption);
}

enum Creation {
    Prepare,
    FailedTask,
    MissingAppTask,
    MissingManifestAppTask,
    MissingAdoption,
}

#[test]
#[ignore = "requires local Docker and the pinned Alpine image"]
fn pending_pull_recovers_real_repository_digest_and_preserves_pin() {
    let engine = DockerEngine::docker();
    let identity = pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let mut registry = Registry::create_writer(
        state.join("registry.sqlite3"),
        &fixture_registry_owner(&state),
    )
    .unwrap();
    let intent = bosn_registry::ImageCreationIntent {
        reference: format!("docker.io/library/{PINNED_ALPINE}"),
        content_sha256: "a".repeat(64),
        workspace: root.path().to_string_lossy().into_owned(),
        stack: "app".into(),
        owner: bosn_registry::ImageIntentOwner::Setup,
        source: bosn_registry::ImageIntentSource::Pull,
        created_at: 1.0,
    };
    let resource = bosn_registry::Resource {
        id: format!("setup-image:{identity}"),
        name: format!("setup-image:{identity}"),
        kind: ResourceKind::Image,
        stack: intent.stack.clone(),
        generation: identity.clone(),
        scope: bosn_core::Scope::Machine,
        workspace: intent.workspace.clone(),
        created_at: 1.0,
        last_used: 20.0,
        state: ResourceState::Active,
        retention: Retention::Pinned,
    };
    let mut transaction = registry.begin_immediate().unwrap();
    transaction.put_resource(&resource).unwrap();
    transaction.put_image_creation_intent(&intent).unwrap();
    transaction.commit().unwrap();
    drop(registry);
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut daemon = DaemonChild::start_with_retention_root(&state, &root.path().join("machine"));
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let deadline = Instant::now() + READY_DEADLINE;
    let summary = loop {
        let summary = runtime
            .run(managed_retention_when_admitted(
                &client,
                RetentionPolicy {
                    container_ttl: Duration::ZERO,
                    volume_ttl: Duration::ZERO,
                    image_ttl: Duration::ZERO,
                    ..RetentionPolicy::default()
                },
                true,
            ))
            .unwrap();
        if summary.refused.as_deref() != Some("machine retention admission busy") {
            break summary;
        }
        assert!(Instant::now() < deadline, "{summary:?}");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(summary.refused.is_none(), "{summary:?}");
    assert_eq!(summary.removed, 0, "{summary:?}");
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert!(
        registry.image_creation_intents().unwrap().is_empty(),
        "{summary:?}"
    );
    let rows = registry.resources(0, 64).unwrap().items;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].retention, Retention::Pinned);
    assert_eq!(rows[0].last_used, 20.0);
    assert!(docker_capture(&engine, ["image", "inspect", &identity]).ok());
    runtime.run(client.shutdown()).unwrap();
    assert!(daemon.wait_for_exit().success());
}

// These crash cases start from an initialized registry. The shared Docker
// host may already contain unrelated Bosn objects; cold-start refusal belongs
// to the startup tests, not these export/deletion recovery scenarios.
fn fixture_registry_owner(state: &Path) -> String {
    let digest = sha256_bytes(state.to_string_lossy().as_bytes()).to_hex();
    format!(
        "{}-{}-4{}-8{}-{}",
        &digest[..8],
        &digest[8..12],
        &digest[13..16],
        &digest[17..20],
        &digest[20..32]
    )
}

fn initialize_fixture_registry(state: &Path) {
    drop(
        Registry::create_writer(
            state.join("registry.sqlite3"),
            &fixture_registry_owner(state),
        )
        .unwrap(),
    );
}

#[cfg(unix)]
struct ExportBarrier(std::path::PathBuf);
#[cfg(unix)]
impl Drop for ExportBarrier {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("release"), "release");
    }
}

#[test]
#[cfg(unix)]
#[ignore = "requires local Docker and the pinned Alpine image"]
#[expect(
    clippy::too_many_lines,
    reason = "real deletion crash plus entire state-path loss and restart"
)]
fn deletion_crash_replays_accounting_after_original_state_is_lost() {
    use std::os::unix::fs::PermissionsExt;
    let engine = DockerEngine::docker();
    pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().unwrap();
    let _cleanup = FixtureImages {
        engine: engine.clone(),
        root: root.path().to_string_lossy().into_owned(),
    };
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let shim = root.path().join("shim");
    for path in [&state, &workspace, &shim] {
        std::fs::create_dir(path).unwrap();
    }
    let _barrier = ExportBarrier(shim.clone());
    std::fs::write(
        shim.join("docker"),
        r#"#!/bin/sh
if [ "$1" = rmi ]; then
  /usr/local/bin/docker "$@" || exit $?
  touch "$(dirname "$0")/deleted"
  remaining=300
  while [ ! -f "$(dirname "$0")/release" ] && [ "$remaining" -gt 0 ]; do
    sleep 0.1
    remaining=$((remaining - 1))
  done
  exit 0
fi
exec /usr/local/bin/docker "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(shim.join("docker"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = workspace.join("setup.toml");
    std::fs::write(&config, format!("version = 1\n[app]\ndockerfile = '''FROM {PINNED_ALPINE}\nLABEL bosn.prepare.fixture='{}'\n'''\ncommand = 'sleep 3600'\n", root.path().display())).unwrap();
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let machine = root.path().join("machine");
    let path = std::env::join_paths(
        std::iter::once(shim.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    initialize_fixture_registry(&state);
    let mut daemon = DaemonChild::start_with_docker_path(&state, &machine, &path);
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let job = runtime
        .run(client.submit_setup_prepare(SetupPrepareRequest {
            workspace,
            config: config.to_string_lossy().into_owned(),
            policy: SetupPreparePolicy::Refresh,
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
        }))
        .unwrap();
    wait_for_success(&runtime, &client, job);
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    let owner = registry.registry_id().unwrap();
    let rows = registry.resources(0, 64).unwrap().items;
    assert_eq!(rows.len(), 1);
    let identity = rows[0].generation.clone();
    drop(registry);
    let authority = Registry::resolve_authority(&state.join("registry.sqlite3")).unwrap();
    let intents = authority.parent().unwrap().join("deletion-intents");
    let request_state = state.clone();
    let request = std::thread::spawn(move || {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = Client::for_state(request_state).unwrap();
        runtime.run(managed_retention_when_admitted(
            &client,
            RetentionPolicy {
                container_ttl: Duration::ZERO,
                volume_ttl: Duration::ZERO,
                image_ttl: Duration::ZERO,
                ..RetentionPolicy::default()
            },
            true,
        ))
    });
    let deadline = Instant::now() + JOB_DEADLINE;
    while !shim.join("deleted").exists() {
        assert!(
            Instant::now() < deadline,
            "Docker deletion boundary was not reached"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!docker_capture(&engine, ["image", "inspect", &identity]).ok());
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert_eq!(registry.resources(0, 64).unwrap().items.len(), 1);
    assert_eq!(
        std::fs::read_dir(&intents)
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|value| value == "json"))
            .count(),
        1
    );
    drop(registry);
    daemon.child.kill().unwrap();
    assert!(!daemon.wait_for_exit().success());
    std::fs::write(shim.join("release"), "release").unwrap();
    assert!(request.join().unwrap().is_err());
    std::fs::rename(&state, root.path().join("lost-state")).unwrap();
    let mut restarted = DaemonChild::start_with_retention_root(&state, &machine);
    let client = wait_for_client(&runtime, &mut restarted, &state);
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        let summary = runtime
            .run(managed_retention_when_admitted(
                &client,
                RetentionPolicy {
                    container_ttl: Duration::ZERO,
                    volume_ttl: Duration::ZERO,
                    image_ttl: Duration::ZERO,
                    ..RetentionPolicy::default()
                },
                true,
            ))
            .unwrap();
        if summary.refused.as_deref() != Some("machine retention admission busy") {
            assert!(summary.refused.is_none(), "{summary:?}");
            assert_eq!(
                summary.removed, 0,
                "replay must not claim another physical removal: {summary:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "{summary:?}");
        std::thread::sleep(Duration::from_millis(25));
    }
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert_eq!(registry.registry_id().unwrap(), owner);
    assert!(registry.resources(0, 64).unwrap().items.is_empty());
    assert_eq!(std::fs::read_dir(intents).unwrap().count(), 0);
    runtime.run(client.shutdown()).unwrap();
    assert!(restarted.wait_for_exit().success());
}

#[test]
#[cfg(unix)]
#[ignore = "requires local Docker and the pinned Alpine image"]
#[expect(
    clippy::too_many_lines,
    reason = "one real daemon crash and recovery lifecycle"
)]
fn interrupted_export_is_recovered_and_reclaimed_after_restart() {
    use std::os::unix::fs::PermissionsExt;
    let engine = DockerEngine::docker();
    pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().unwrap();
    let _cleanup = FixtureImages {
        engine: engine.clone(),
        root: root.path().to_string_lossy().into_owned(),
    };
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let shim = root.path().join("shim");
    for path in [&state, &workspace, &shim] {
        std::fs::create_dir(path).unwrap();
    }
    let _barrier = ExportBarrier(shim.clone());
    // Inspection succeeds before the marker, proving export completed. Only
    // this daemon sees the shim; its bounded wait releases on fixture teardown.
    std::fs::write(
        shim.join("docker"),
        r#"#!/bin/sh
if [ "$1" = image ] && [ "$2" = inspect ] && [ "$3" = --format ] && [ "$4" = '{{.Id}}' ]; then
  case "$5" in bosn-setup:*)
    /usr/local/bin/docker "$@" > "$(dirname "$0")/identity" || exit $?
    touch "$(dirname "$0")/exported"
    remaining=300
    while [ ! -f "$(dirname "$0")/release" ] && [ "$remaining" -gt 0 ]; do
      sleep 0.1
      remaining=$((remaining - 1))
    done
    cat "$(dirname "$0")/identity"
    exit 0
  esac
fi
exec /usr/local/bin/docker "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(shim.join("docker"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = workspace.join("setup.toml");
    std::fs::write(&config, format!("version = 1\n[app]\ndockerfile = '''FROM {PINNED_ALPINE}\nLABEL bosn.prepare.fixture='{}'\n'''\ncommand = 'sleep 3600'\n", root.path().display())).unwrap();
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let machine = root.path().join("machine");
    let path = std::env::join_paths(
        std::iter::once(shim.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    initialize_fixture_registry(&state);
    let mut daemon = DaemonChild::start_with_docker_path(&state, &machine, &path);
    let client = wait_for_client(&runtime, &mut daemon, &state);
    runtime
        .run(client.submit_setup_prepare(SetupPrepareRequest {
            workspace,
            config: config.to_string_lossy().into_owned(),
            policy: SetupPreparePolicy::Refresh,
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
        }))
        .unwrap();
    let deadline = Instant::now() + JOB_DEADLINE;
    while !shim.join("exported").exists() {
        assert!(
            Instant::now() < deadline,
            "preparation never reached exported image boundary"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let identity = std::fs::read_to_string(shim.join("identity"))
        .unwrap()
        .trim()
        .to_owned();
    assert!(identity.starts_with("sha256:"));
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert_eq!(registry.image_creation_intents().unwrap().len(), 1);
    assert!(registry.resources(0, 64).unwrap().items.is_empty());
    drop(registry);
    daemon.child.kill().unwrap();
    assert!(!daemon.wait_for_exit().success());
    std::fs::write(shim.join("release"), "release").unwrap();
    let mut restarted = DaemonChild::start_with_retention_root(&state, &machine);
    let client = wait_for_client(&runtime, &mut restarted, &state);
    let summary = runtime
        .run(managed_retention_when_admitted(
            &client,
            RetentionPolicy {
                container_ttl: Duration::ZERO,
                volume_ttl: Duration::ZERO,
                image_ttl: Duration::ZERO,
                ..RetentionPolicy::default()
            },
            true,
        ))
        .unwrap();
    assert!(summary.refused.is_none(), "{summary:?}");
    assert_eq!(summary.removed, 1, "{summary:?}");
    assert!(!docker_capture(&engine, ["image", "inspect", &identity]).ok());
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert!(registry.image_creation_intents().unwrap().is_empty());
    assert!(registry.resources(0, 64).unwrap().items.is_empty());
    runtime.run(client.shutdown()).unwrap();
    assert!(restarted.wait_for_exit().success());
}

#[expect(
    clippy::too_many_lines,
    reason = "sequential end-to-end lifecycle assertions keep creation, protection and reclamation evidence together"
)]
fn exercise_image_reclamation(creation: Creation) {
    let engine = DockerEngine::docker();
    pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().unwrap();
    let _cleanup = FixtureImages {
        engine: engine.clone(),
        root: root.path().to_string_lossy().into_owned(),
    };
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    drop(
        Registry::create_writer(
            state.join("registry.sqlite3"),
            &fixture_registry_owner(&state),
        )
        .unwrap(),
    );
    let config = workspace.join("setup.toml");
    std::fs::write(&config, format!("version = 1\n[app]\ndockerfile = '''FROM {PINNED_ALPINE}\nLABEL bosn.prepare.fixture='{}'\n'''\ncommand = 'sleep 3600'\n[task.fail]\ncommand = 'exit 7'\n", root.path().display())).unwrap();
    std::fs::write(
        workspace.join("Dockerfile"),
        format!(
            "FROM {PINNED_ALPINE}\nLABEL bosn.prepare.fixture='{}'\n",
            root.path().display()
        ),
    )
    .unwrap();
    std::fs::write(
        workspace.join("bosn.toml"),
        "[stack.app]\ndockerfile = 'Dockerfile'\n[task.fail]\nstack = 'app'\ncmd = 'exit 7'\n",
    )
    .unwrap();
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut daemon = DaemonChild::start_with_retention_root(&state, &root.path().join("machine"));
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let job = match creation {
        Creation::MissingAdoption => {
            let error = runtime
                .run(client.setup_adopt(SetupAdoptRequest {
                    workspace,
                    config: config.to_string_lossy().into_owned(),
                    policy: SetupPreparePolicy::Refresh,
                    deadline: JOB_DEADLINE,
                    output_limit: OUTPUT_LIMIT,
                    confirm: true,
                }))
                .expect_err("missing application must refuse adoption");
            assert!(
                error.to_string().contains("existing container"),
                "unexpected adoption failure: {error}"
            );
            0
        }

        Creation::MissingManifestAppTask => runtime
            .run(client.submit_manifest_app_task(ManifestAppTaskJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                task_name: "fail".into(),
                deadline: JOB_DEADLINE,
                output_limit: OUTPUT_LIMIT,
            }))
            .unwrap(),
        Creation::Prepare => runtime
            .run(client.submit_setup_prepare(SetupPrepareRequest {
                workspace,
                config: config.to_string_lossy().into_owned(),
                policy: SetupPreparePolicy::Refresh,
                deadline: JOB_DEADLINE,
                output_limit: OUTPUT_LIMIT,
            }))
            .unwrap(),
        Creation::FailedTask => runtime
            .run(client.submit_setup_task(SetupTaskJobRequest {
                workspace,
                config: config.to_string_lossy().into_owned(),
                task_name: "fail".into(),
                policy: SetupPreparePolicy::Refresh,
                deadline: JOB_DEADLINE,
                output_limit: OUTPUT_LIMIT,
            }))
            .unwrap(),
        Creation::MissingAppTask => runtime
            .run(client.submit_setup_app_task(SetupAppTaskJobRequest {
                workspace,
                config: config.to_string_lossy().into_owned(),
                task_name: "fail".into(),
                policy: SetupPreparePolicy::Refresh,
                deadline: JOB_DEADLINE,
                output_limit: OUTPUT_LIMIT,
            }))
            .unwrap(),
    };
    match creation {
        Creation::MissingAdoption => {}
        Creation::Prepare => wait_for_success(&runtime, &client, job),
        Creation::FailedTask | Creation::MissingAppTask | Creation::MissingManifestAppTask => {
            let expected = match creation {
                Creation::FailedTask => "declared setup task exited with 7",
                Creation::MissingAppTask | Creation::MissingManifestAppTask => {
                    "existing container is not the expected Bosn-managed setup app"
                }
                Creation::Prepare | Creation::MissingAdoption => unreachable!(),
            };
            let deadline = Instant::now() + JOB_DEADLINE;
            loop {
                let status = runtime.run(client.job_status(job)).unwrap();
                match status.state.as_str() {
                    "Failed" => {
                        assert!(
                            status
                                .error
                                .as_deref()
                                .is_some_and(|error| error.contains(expected)),
                            "{status:?}"
                        );
                        break;
                    }
                    "Queued" | "Running" | "Cancelling" => {}
                    _ => panic!("expected declared task failure: {status:?}"),
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert!(
        registry.image_creation_intents().unwrap().is_empty(),
        "completed preparation retained its intent"
    );
    let resources = registry.resources(0, 64).unwrap().items;
    assert_eq!(
        resources.len(),
        1,
        "prepare-only success omitted image ownership: {resources:?}"
    );
    let image = &resources[0];
    assert_eq!(image.kind, ResourceKind::Image);
    assert_eq!(image.retention, Retention::Warm);
    let namespace = if matches!(creation, Creation::MissingManifestAppTask) {
        "manifest-image"
    } else {
        "setup-image"
    };
    assert_eq!(image.id, format!("{namespace}:{}", image.generation));
    assert!(docker_capture(&engine, ["image", "inspect", &image.generation]).ok());
    let proof = docker_capture(
        &engine,
        [
            "image",
            "inspect",
            "--format",
            "{{index .Config.Labels \"com.zackees.bosn.image-preparation\"}}",
            &image.generation,
        ],
    );
    assert!(proof.ok());
    let proof = std::str::from_utf8(&proof.stdout).unwrap().trim();
    assert!(
        proof.len() == 64
            && proof
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "production image omitted its preparation proof: {proof:?}"
    );

    let summary = runtime
        .run(managed_retention_when_admitted(
            &client,
            RetentionPolicy {
                container_ttl: Duration::ZERO,
                volume_ttl: Duration::ZERO,
                image_ttl: Duration::ZERO,
                ..RetentionPolicy::default()
            },
            true,
        ))
        .unwrap();
    assert!(summary.refused.is_none(), "{summary:?}");
    assert_eq!(summary.removed, 1, "{summary:?}");
    assert!(!docker_capture(&engine, ["image", "inspect", &image.generation]).ok());
    runtime.run(client.shutdown()).unwrap();
    assert!(daemon.wait_for_exit().success());
}
