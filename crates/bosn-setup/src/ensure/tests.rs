use std::{
    collections::{BTreeMap, VecDeque},
    future::{Ready, ready},
    sync::Mutex,
    time::Duration,
};

use super::*;
use kernal_api::async_engine::{CancellationSource, RuntimeBuilder, channel};
mod macos_guest;

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const IDENTITY: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CONTAINER_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

#[derive(Default)]
struct FakeEngine {
    calls: Mutex<Vec<SetupEnsureCommand>>,
    results: Mutex<VecDeque<Result<SetupEnsureResponse, CommandError>>>,
}

impl FakeEngine {
    fn with_results(
        results: impl IntoIterator<Item = Result<SetupEnsureResponse, CommandError>>,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}

impl SetupEnsureEngine for FakeEngine {
    type StreamFuture<'a> = Ready<Result<SetupEnsureResponse, CommandError>>;

    fn stream<'a>(
        &'a self,
        command: SetupEnsureCommand,
        _options: RunOptions,
        _cancellation: &'a CancellationToken,
        _events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a> {
        self.calls.lock().unwrap().push(command);
        ready(
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .expect("configured ensure result"),
        )
    }
}

fn runtime() -> kernal_api::async_engine::Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn result(exit_code: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> CommandResult {
    CommandResult {
        exit_code,
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

fn absent() -> Result<SetupEnsureResponse, CommandError> {
    Ok(SetupEnsureResponse::Inspection(
        None,
        result(1, [], b"Error response from daemon: No such container"),
    ))
}

fn observed(plan: &SetupPlan, running: bool) -> SetupEnsureObservedContainer {
    let name = format!("bosn-setup-{}", plan.content_sha256);
    SetupEnsureObservedContainer {
        container_id: CONTAINER_ID.into(),
        running,
        image_identity: IDENTITY.into(),
        labels: BTreeMap::from([
            (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
            (LABEL_CONTENT_SHA256.into(), plan.content_sha256.clone()),
            (LABEL_CONTAINER_NAME.into(), name),
        ]),
    }
}

fn inspection(plan: &SetupPlan, running: bool) -> Result<SetupEnsureResponse, CommandError> {
    Ok(SetupEnsureResponse::Inspection(
        Some(observed(plan, running)),
        result(0, [], []),
    ))
}

fn command(stdout: impl Into<Vec<u8>>) -> Result<SetupEnsureResponse, CommandError> {
    Ok(SetupEnsureResponse::Command(result(0, stdout, [])))
}

fn plan(workspace: &Path) -> SetupPlan {
    let image = format!("registry.example/team/app@sha256:{HASH}");
    SetupPlan {
        source_kind: crate::SetupSourceKind::LocalFile,
        content_sha256: HASH.into(),
        schema_version: 1,
        workspace_root: fs::canonical_context_path(workspace).unwrap(),
        asset_root: None,
        task_names: Vec::new(),
        app: bosn_core::SetupApp {
            source: bosn_core::SetupSource::PinnedImage(image.clone()),
            environment: BTreeMap::from([("APP_MODE".into(), "production".into())]),
            workdir: Some("src".into()),
            command: Some("./serve --port 8080".into()),
            mounts: vec![bosn_core::WorkspaceMount {
                source: ".".into(),
                target: "/workspace".into(),
                readonly: false,
            }],
        },
        tasks: BTreeMap::new(),
        app_source: SetupPlanAppSource::PinnedImage { image },
        named_volumes: Vec::new(),
        tmpfs: Vec::new(),
        host_docker_socket: None,
        macos_guest: None,
    }
}

fn prepared(plan: &SetupPlan) -> PreparedImage {
    let SetupPlanAppSource::PinnedImage { image } = &plan.app_source else {
        unreachable!()
    };
    PreparedImage {
        setup_content_sha256: plan.content_sha256.clone(),
        kind: PreparedImageKind::PinnedImage {
            image: image.clone(),
        },
        reference: image.clone(),
        observed_identity: IDENTITY.into(),
    }
}

fn macos_guest_plan(workspace: &Path) -> SetupPlan {
    let mut plan = plan(workspace);
    let image = format!("dockurr/macos@sha256:{HASH}");
    plan.app.source = bosn_core::SetupSource::PinnedImage(image.clone());
    plan.app_source = SetupPlanAppSource::PinnedImage { image };
    plan.app.mounts.clear();
    plan.app.workdir = None;
    plan.app.command = None;
    let storage_volume = "bosn-v-machine-macos-storage".to_owned();
    plan.named_volumes = vec![crate::SetupNamedVolume {
        name: storage_volume.clone(),
        target: "/storage".into(),
        labels: BTreeMap::from([
            (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
            (LABEL_CONTENT_SHA256.into(), HASH.into()),
            (LABEL_CONTAINER_NAME.into(), storage_volume.clone()),
        ]),
    }];
    plan.macos_guest = Some(crate::SetupMacosGuest {
        ssh_port: 2222,
        web_port: 8006,
        version: "ventura".into(),
        ram_size: "8G".into(),
        disk_size: "128G".into(),
        cpu_cores: 1,
        storage_volume,
        storage_scope: Scope::Machine,
        storage_retention: Retention::Pinned,
    });
    plan
}

fn run(
    engine: &FakeEngine,
    plan: &SetupPlan,
    workspace: &Path,
    image: &PreparedImage,
    cancellation: &CancellationToken,
    options: RunOptions,
) -> Result<SetupEnsureResult, SetupEnsureError> {
    let (events, _receiver) = channel(8);
    runtime().run(ensure_setup_app(
        engine,
        SetupEnsureRequest {
            plan,
            workspace_root: workspace.into(),
            prepared_image: image,
            options,
            cancellation,
            events: &events,
        },
    ))
}
fn run_adopt(
    engine: &FakeEngine,
    plan: &SetupPlan,
    workspace: &Path,
    image: &PreparedImage,
    cancellation: &CancellationToken,
    options: RunOptions,
) -> Result<SetupEnsureResult, SetupEnsureError> {
    let (events, _receiver) = channel(8);
    runtime().run(adopt_setup_app(
        engine,
        SetupEnsureRequest {
            plan,
            workspace_root: workspace.into(),
            prepared_image: image,
            options,
            cancellation,
            events: &events,
        },
    ))
}

#[test]
fn adoption_proves_exact_running_or_stopped_candidate_without_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    for running in [true, false] {
        let engine = FakeEngine::with_results([inspection(&plan, running)]);
        let source = CancellationSource::new();
        let result = run_adopt(
            &engine,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096),
        )
        .unwrap();
        assert!(!result.created && !result.started);
        assert!(matches!(
            engine.calls.lock().unwrap().as_slice(),
            [SetupEnsureCommand::Inspect { .. }]
        ));
    }
    let mut bad = observed(&plan, true);
    bad.labels.insert(LABEL_MANAGED.into(), "foreign".into());
    let engine = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
        Some(bad),
        result(0, [], []),
    ))]);
    let source = CancellationSource::new();
    assert!(matches!(
        run_adopt(
            &engine,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::OwnershipMismatch)
    ));
    assert!(matches!(
        engine.calls.lock().unwrap().as_slice(),
        [SetupEnsureCommand::Inspect { .. }]
    ));
    let cancelled = FakeEngine::default();
    let source = CancellationSource::new();
    source.cancel();
    assert!(matches!(
        run_adopt(
            &cancelled,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::Cancelled)
    ));
    assert!(cancelled.calls.lock().unwrap().is_empty());
    let deadline = FakeEngine::default();
    let source = CancellationSource::new();
    assert!(matches!(
        run_adopt(
            &deadline,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::ZERO, 4096)
        ),
        Err(SetupEnsureError::Deadline)
    ));
    assert!(deadline.calls.lock().unwrap().is_empty());
}

#[test]
fn inspect_format_uses_actual_tabs_that_the_response_parser_accepts() {
    let container_name = format!("bosn-setup-{HASH}");
    let command = SetupEnsureCommand::Inspect {
        container_name: container_name.clone(),
    };
    let args = command.docker_args();
    assert_eq!(args[0], "container");
    assert_eq!(args[1], "inspect");
    assert_eq!(args[2], "--format");
    assert!(args[3].contains('\t'));
    assert!(!args[3].contains("\\\\t"));
    assert_eq!(args[4], container_name);

    let observed = parse_inspection(
        format!("{CONTAINER_ID}\ttrue\t{IDENTITY}\t{MANAGED_VALUE}\t{HASH}\tbosn-setup-{HASH}\n")
            .as_bytes(),
    )
    .expect("inspect output using the generated delimiter contract parses");
    assert_eq!(observed.container_id, CONTAINER_ID);
    assert!(observed.running);
    assert_eq!(observed.image_identity, IDENTITY);
    assert_eq!(
        observed.labels.get(LABEL_MANAGED).map(String::as_str),
        Some(MANAGED_VALUE)
    );
    assert_eq!(
        observed
            .labels
            .get(LABEL_CONTENT_SHA256)
            .map(String::as_str),
        Some(HASH)
    );
    assert_eq!(
        observed
            .labels
            .get(LABEL_CONTAINER_NAME)
            .map(String::as_str),
        Some(container_name.as_str())
    );
}

#[test]
fn volume_inspect_format_uses_actual_tabs_that_match_the_label_receipt() {
    let volume_name = format!("bosn-v-stack-{HASH}");
    let command = SetupEnsureCommand::VolumeInspect {
        volume_name: volume_name.clone(),
    };
    let args = command.docker_args();
    assert_eq!(args[0], "volume");
    assert_eq!(args[1], "inspect");
    assert_eq!(args[2], "--format");
    assert!(args[3].contains('\t'));
    assert!(!args[3].contains("\\\\t"));
    assert_eq!(args[4], volume_name);
}

#[test]
fn typed_tmpfs_is_emitted_without_a_raw_option_channel() {
    let command = SetupEnsureCommand::Create {
        container_name: "bosn-setup-test".into(),
        image_identity: IDENTITY.into(),
        mounts: Vec::new(),
        volumes: Vec::new(),
        tmpfs: vec![SetupEnsureTmpfs {
            target: "/run/cache".into(),
            readonly: true,
            size: Some(crate::SetupTmpfsSize {
                value: 64,
                unit: crate::SetupTmpfsSizeUnit::Mebibytes,
            }),
            exec: None,
            mode: None,
        }],
        host_docker_socket: None,
        environment: BTreeMap::new(),
        workdir: None,
        command: None,
        labels: BTreeMap::new(),
        macos_guest: Box::new(None),
    };
    let args = command.docker_args();
    assert_eq!(
        args.windows(2)
            .find(|pair| pair[0] == "--tmpfs")
            .map(|pair| pair[1].as_str()),
        Some("/run/cache:ro,size=64m")
    );
}

#[test]
fn typed_tmpfs_exec_mode_and_host_docker_socket_are_emitted_from_typed_fields() {
    let command = SetupEnsureCommand::Create {
        container_name: "bosn-setup-test".into(),
        image_identity: IDENTITY.into(),
        mounts: Vec::new(),
        volumes: Vec::new(),
        tmpfs: vec![SetupEnsureTmpfs {
            target: "/mount-probe".into(),
            readonly: false,
            size: None,
            exec: Some(true),
            mode: Some(0o1777),
        }],
        host_docker_socket: Some(crate::SetupHostDockerSocket {
            source: crate::SetupHostDockerSocketSource::VarRun,
            target: "/var/run/docker.sock".into(),
            readonly: false,
        }),
        environment: BTreeMap::new(),
        workdir: None,
        command: None,
        labels: BTreeMap::new(),
        macos_guest: Box::new(None),
    };
    let args = command.docker_args();
    assert_eq!(
        args.windows(2)
            .find(|pair| pair[0] == "--tmpfs")
            .map(|pair| pair[1].as_str()),
        Some("/mount-probe:exec,mode=1777")
    );
    assert!(args.windows(2).any(|pair| pair[0] == "--mount"
        && pair[1] == "type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock"));
    let noexec = SetupEnsureTmpfs {
        target: "/t".into(),
        readonly: true,
        size: None,
        exec: Some(false),
        mode: Some(0o700),
    };
    assert_eq!(tmpfs_docker_value(&noexec), "/t:ro,noexec,mode=700");
}

#[test]
fn host_docker_socket_target_cannot_collide_or_enter_a_guest() {
    let temporary = tempfile::tempdir().unwrap();
    let mut plan = plan(temporary.path());
    plan.host_docker_socket = Some(crate::SetupHostDockerSocket {
        source: crate::SetupHostDockerSocketSource::Run,
        target: plan.app.mounts[0].target.clone(),
        readonly: false,
    });
    assert!(matches!(
        validate_plan_shape(&plan),
        Err(SetupEnsureError::InvalidRequest(
            "host Docker socket receipt was modified"
        ))
    ));
    plan.host_docker_socket = Some(crate::SetupHostDockerSocket {
        source: crate::SetupHostDockerSocketSource::Run,
        target: "/var/run/docker.sock".into(),
        readonly: false,
    });
    assert!(validate_plan_shape(&plan).is_ok());

    let mut guest = macos_guest_plan(temporary.path());
    assert!(validate_plan_shape(&guest).is_ok());
    guest.host_docker_socket = plan.host_docker_socket.clone();
    assert!(matches!(
        validate_plan_shape(&guest),
        Err(SetupEnsureError::InvalidRequest(
            "host Docker socket receipt was modified"
        ))
    ));
}

#[test]
fn tmpfs_target_cannot_collide_with_a_bind_or_be_modified() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let mut plan = plan(&workspace);
    plan.tmpfs.push(crate::SetupTmpfs {
        target: "/workspace".into(),
        readonly: false,
        size: None,
        exec: None,
        mode: None,
    });
    let image = prepared(&plan);
    let engine = FakeEngine::with_results([]);
    let cancellation = CancellationSource::new();
    assert!(matches!(
        run(
            &engine,
            &plan,
            &workspace,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096),
        ),
        Err(SetupEnsureError::InvalidRequest(
            "tmpfs receipt was modified"
        ))
    ));
    assert!(engine.calls.lock().unwrap().is_empty());
}

#[test]
fn absent_container_is_created_then_started_with_only_plan_data() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let engine =
        FakeEngine::with_results([absent(), command(format!("{CONTAINER_ID}\n")), command([])]);
    let cancellation = CancellationSource::new();
    let receipt = run(
        &engine,
        &plan,
        &workspace,
        &image,
        &cancellation.token(),
        RunOptions::streaming(Duration::from_secs(2), 4096),
    )
    .unwrap();

    assert_eq!(
        receipt,
        SetupEnsureResult {
            container_name: format!("bosn-setup-{HASH}"),
            container_id: CONTAINER_ID.into(),
            image_identity: IDENTITY.into(),
            created: true,
            started: true,
            running: true,
        }
    );
    let calls = engine.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    assert!(
        matches!(&calls[0], SetupEnsureCommand::Inspect { container_name } if container_name == &format!("bosn-setup-{HASH}"))
    );
    assert_eq!(
        calls[2],
        SetupEnsureCommand::Start {
            container_name: format!("bosn-setup-{HASH}")
        }
    );
    let SetupEnsureCommand::Create {
        image_identity,
        mounts,
        environment,
        workdir,
        command,
        labels,
        ..
    } = &calls[1]
    else {
        panic!("expected create")
    };
    assert_eq!(image_identity, IDENTITY);
    assert_eq!(
        mounts,
        &vec![SetupEnsureMount {
            source: fs::canonical_context_path(&workspace).unwrap(),
            target: "/workspace".into(),
            readonly: false
        }]
    );
    assert_eq!(
        environment,
        &BTreeMap::from([("APP_MODE".into(), "production".into())])
    );
    assert_eq!(workdir.as_deref(), Some("/workspace/src"));
    assert_eq!(command.as_deref(), Some("./serve --port 8080"));
    assert_eq!(
        labels.get(LABEL_CONTENT_SHA256).map(String::as_str),
        Some(HASH)
    );
}

#[test]
fn workdir_backed_by_a_file_is_refused_before_any_engine_command() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("not-a-directory"), "proof").unwrap();
    let mut plan = plan(&workspace);
    plan.app.workdir = Some("not-a-directory".into());
    plan.app.mounts[0].source = "not-a-directory".into();
    let image = prepared(&plan);
    let engine = FakeEngine::with_results([]);
    let cancellation = CancellationSource::new();
    assert!(
        run(
            &engine,
            &plan,
            &workspace,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096),
        )
        .is_err()
    );
    assert!(engine.calls.lock().unwrap().is_empty());
}

#[test]
fn matching_stopped_container_is_only_started_and_running_one_is_reused() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let cancellation = CancellationSource::new();
    let stopped = FakeEngine::with_results([inspection(&plan, false), command([])]);
    assert!(
        run(
            &stopped,
            &plan,
            &workspace,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        )
        .unwrap()
        .started
    );
    assert!(matches!(
        stopped.calls.lock().unwrap().as_slice(),
        [
            SetupEnsureCommand::Inspect { .. },
            SetupEnsureCommand::Start { .. }
        ]
    ));
    let running = FakeEngine::with_results([inspection(&plan, true)]);
    let receipt = run(
        &running,
        &plan,
        &workspace,
        &image,
        &cancellation.token(),
        RunOptions::streaming(Duration::from_secs(2), 4096),
    )
    .unwrap();
    assert!(!receipt.created && !receipt.started);
    assert!(matches!(
        running.calls.lock().unwrap().as_slice(),
        [SetupEnsureCommand::Inspect { .. }]
    ));
}

#[test]
fn matching_container_reuse_does_not_reinspect_or_mutate_declared_volumes() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let mut plan = plan(&workspace);
    let volume_name = format!("bosn-v-stack-{HASH}");
    plan.named_volumes.push(crate::SetupNamedVolume {
        name: volume_name.clone(),
        target: "/var/lib/app".into(),
        labels: BTreeMap::from([
            (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
            (LABEL_CONTENT_SHA256.into(), HASH.into()),
            (LABEL_CONTAINER_NAME.into(), volume_name),
        ]),
    });
    let image = prepared(&plan);
    let cancellation = CancellationSource::new();
    let engine = FakeEngine::with_results([inspection(&plan, true)]);

    let receipt = run(
        &engine,
        &plan,
        &workspace,
        &image,
        &cancellation.token(),
        RunOptions::streaming(Duration::from_secs(2), 4096),
    )
    .unwrap();

    assert!(!receipt.created && !receipt.started);
    assert!(matches!(
        engine.calls.lock().unwrap().as_slice(),
        [SetupEnsureCommand::Inspect { .. }]
    ));
}

#[test]
fn foreign_or_mismatched_existing_container_is_refused_before_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let cancellation = CancellationSource::new();
    let mut foreign = observed(&plan, false);
    foreign
        .labels
        .insert(LABEL_MANAGED.into(), "foreign".into());
    let engine = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
        Some(foreign),
        result(0, [], []),
    ))]);
    assert!(matches!(
        run(
            &engine,
            &plan,
            &workspace,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::OwnershipMismatch)
    ));
    assert!(matches!(
        engine.calls.lock().unwrap().as_slice(),
        [SetupEnsureCommand::Inspect { .. }]
    ));
}

#[test]
fn tampered_inputs_refuse_before_engine_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    let other = temporary.path().join("other");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    std::fs::create_dir(&other).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let cancellation = CancellationSource::new();
    for mutated in [
        {
            let mut p = plan.clone();
            p.content_sha256 = "bad".into();
            p
        },
        {
            let mut p = plan.clone();
            p.app.mounts[0].target = "/bad//path".into();
            p
        },
        {
            let mut p = plan.clone();
            p.app.environment.insert("BAD-NAME".into(), "x".into());
            p
        },
        {
            let mut p = plan.clone();
            p.app.workdir = Some("../escape".into());
            p
        },
        {
            let mut p = plan.clone();
            p.app.command = Some("\0".into());
            p
        },
    ] {
        let engine = FakeEngine::default();
        assert!(matches!(
            run(
                &engine,
                &mutated,
                &workspace,
                &image,
                &cancellation.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::InvalidRequest(_))
        ));
        assert!(engine.calls.lock().unwrap().is_empty());
    }
    let engine = FakeEngine::default();
    assert!(matches!(
        run(
            &engine,
            &plan,
            &other,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::InvalidRequest(_))
    ));
    assert!(engine.calls.lock().unwrap().is_empty());
    let mut wrong_image = image.clone();
    wrong_image.reference = "registry.example/other@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
    let engine = FakeEngine::default();
    assert!(matches!(
        run(
            &engine,
            &plan,
            &workspace,
            &wrong_image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::InvalidRequest(_))
    ));
    assert!(engine.calls.lock().unwrap().is_empty());
}

#[test]
fn cancellation_deadline_output_and_nonzero_never_continue_to_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let source = CancellationSource::new();
    source.cancel();
    let cancelled = FakeEngine::default();
    assert!(matches!(
        run(
            &cancelled,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::Cancelled)
    ));
    assert!(cancelled.calls.lock().unwrap().is_empty());
    let deadline = FakeEngine::default();
    let source = CancellationSource::new();
    assert!(matches!(
        run(
            &deadline,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::ZERO, 4096)
        ),
        Err(SetupEnsureError::Deadline)
    ));
    assert!(deadline.calls.lock().unwrap().is_empty());
    let output = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
        Some(observed(&plan, true)),
        result(0, b"12345", []),
    ))]);
    let source = CancellationSource::new();
    assert!(matches!(
        run(
            &output,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::from_secs(2), 4)
        ),
        Err(SetupEnsureError::Transport(
            CommandError::OutputLimit { .. }
        ))
    ));
    assert!(matches!(
        output.calls.lock().unwrap().as_slice(),
        [SetupEnsureCommand::Inspect { .. }]
    ));
    let failed = FakeEngine::with_results([
        absent(),
        Ok(SetupEnsureResponse::Command(result(
            19,
            [],
            b"create failed",
        ))),
    ]);
    let source = CancellationSource::new();
    assert!(matches!(
        run(
            &failed,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::ActionFailed {
            action: "container create",
            ..
        })
    ));
    assert!(matches!(
        failed.calls.lock().unwrap().as_slice(),
        [
            SetupEnsureCommand::Inspect { .. },
            SetupEnsureCommand::Create { .. }
        ]
    ));

    let inspect_failed = FakeEngine::with_results([Ok(SetupEnsureResponse::Command(result(
        7,
        [],
        b"inspect failed",
    )))]);
    let source = CancellationSource::new();
    assert!(matches!(
        run(
            &inspect_failed,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::ActionFailed {
            action: "container inspect",
            ..
        })
    ));
    assert!(matches!(
        inspect_failed.calls.lock().unwrap().as_slice(),
        [SetupEnsureCommand::Inspect { .. }]
    ));

    let start_failed = FakeEngine::with_results([
        inspection(&plan, false),
        Ok(SetupEnsureResponse::Command(result(
            11,
            [],
            b"start failed",
        ))),
    ]);
    let source = CancellationSource::new();
    assert!(matches!(
        run(
            &start_failed,
            &plan,
            &workspace,
            &image,
            &source.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096)
        ),
        Err(SetupEnsureError::ActionFailed {
            action: "container start",
            ..
        })
    ));
    assert!(matches!(
        start_failed.calls.lock().unwrap().as_slice(),
        [
            SetupEnsureCommand::Inspect { .. },
            SetupEnsureCommand::Start { .. }
        ]
    ));
}
