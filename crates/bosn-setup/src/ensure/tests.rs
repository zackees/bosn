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

#[cfg(all(unix, feature = "native-test-helper"))]
#[test]
fn structured_inspection_environment_is_captured_privately_not_forwarded() {
    let engine = DockerEngine::synthetic_for_test(
        "/bin/sh",
        [
            "-c",
            "printf '%s' '{\"Config\":{\"Env\":[\"SECRET=private-canary\"]}}'",
            "sh",
        ],
    );
    let cancellation = CancellationSource::new();
    let (events, mut receiver) = channel(8);
    let result = runtime()
        .run(SetupEnsureEngine::stream(
            &engine,
            SetupEnsureCommand::ImageInspect {
                image_identity: IDENTITY.into(),
            },
            RunOptions::streaming(Duration::from_secs(2), 4096),
            &cancellation.token(),
            &events,
        ))
        .unwrap();
    let SetupEnsureResponse::Command(result) = result else {
        panic!("expected private command receipt");
    };
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("private-canary")
    );
    drop(events);
    assert!(runtime().run(receiver.recv()).is_none());
}

#[derive(Default)]
struct FakeEngine {
    calls: Mutex<Vec<SetupEnsureCommand>>,
    results: Mutex<VecDeque<Result<SetupEnsureResponse, CommandError>>>,
    created: Mutex<Option<serde_json::Value>>,
}

impl FakeEngine {
    fn with_results(
        results: impl IntoIterator<Item = Result<SetupEnsureResponse, CommandError>>,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(results.into_iter().collect()),
            created: Mutex::new(None),
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
        let command = self.calls.lock().unwrap().last().unwrap().clone();
        if matches!(command, SetupEnsureCommand::ImageInspect { .. }) {
            return ready(Ok(SetupEnsureResponse::Command(result(
                0,
                serde_json::to_vec(&fixture_image()).unwrap(),
                [],
            ))));
        }
        if matches!(command, SetupEnsureCommand::Inspect { .. })
            && let Some(value) = self.created.lock().unwrap().as_ref()
        {
            return ready(Ok(SetupEnsureResponse::Inspection(
                Some(parse_inspection(&serde_json::to_vec(value).unwrap()).unwrap()),
                result(0, [], []),
            )));
        }
        if matches!(command, SetupEnsureCommand::Create { .. }) {
            *self.created.lock().unwrap() = Some(fixture_configuration(&command, false));
        }
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
    let image = prepared(plan);
    let derived = derive_creation(plan, &plan.workspace_root, &image).unwrap();
    SetupEnsureObservedContainer {
        container_id: CONTAINER_ID.into(),
        running,
        image_identity: IDENTITY.into(),
        labels: derived.labels.clone(),
        configuration: fixture_configuration(&derived.create_command(), running),
    }
}

fn fixture_image() -> serde_json::Value {
    serde_json::json!({"Id":IDENTITY,"Config":{
        "Env":["PATH=/usr/bin:/bin"], "Cmd":["sh"],"Entrypoint":null,
        "User":"","WorkingDir":"","Volumes":null
    }})
}

/// The mode a create asked for with `flag`, or `default`: this fake models a daemon whose
/// own defaults are `default-cgroupns-mode: host` and `default-ipc-mode: shareable` (a cgroup
/// v1 host), so only an explicit request yields a private namespace (#561).
fn requested(command: &SetupEnsureCommand, flag: &str, default: &str) -> String {
    let args = command.docker_args();
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .map_or_else(|| default.to_owned(), Clone::clone)
}

fn fixture_configuration(command: &SetupEnsureCommand, running: bool) -> serde_json::Value {
    let cgroupns = requested(command, "--cgroupns", "host");
    let ipc = requested(command, "--ipc", "shareable");
    let SetupEnsureCommand::Create {
        image_identity,
        mounts,
        volumes,
        tmpfs,
        host_docker_socket,
        environment,
        workdir,
        command,
        labels,
        macos_guest,
        ..
    } = command
    else {
        panic!("fixture requires create");
    };
    let mut config = fixture_image()["Config"].clone();
    for key in ["Tty", "OpenStdin", "StdinOnce", "AttachStdin"] {
        config[key] = false.into();
    }
    let mut env = environment.clone();
    env.entry("PATH".into())
        .or_insert_with(|| "/usr/bin:/bin".into());
    if let Some(guest) = macos_guest.as_ref() {
        env.extend([
            ("VERSION".into(), guest.version.clone()),
            ("RAM_SIZE".into(), guest.ram_size.clone()),
            ("DISK_SIZE".into(), guest.disk_size.clone()),
            ("CPU_CORES".into(), guest.cpu_cores.to_string()),
        ]);
        config["StopTimeout"] = 120.into();
    } else if let Some(command) = command {
        config["Cmd"] = serde_json::json!(crate::shell::login_shell_args(command));
    }
    config["Env"] = serde_json::json!(
        env.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
    );
    config["WorkingDir"] = serde_json::json!(workdir.as_deref().unwrap_or(""));
    config["Labels"] = serde_json::json!(labels);
    let mut actual_mounts: Vec<_> = mounts.iter().map(|m| serde_json::json!({"Type":"bind","Propagation":"rprivate","Source":m.source,"Destination":m.target,"RW":!m.readonly})).collect();
    actual_mounts.extend(volumes.iter().map(
        |v| serde_json::json!({"Type":"volume","Driver":"local","Name":v.name,"Source":format!("/var/lib/docker/volumes/{}/_data",v.name),"Destination":v.target,"RW":true}),
    ));
    if let Some(socket) = host_docker_socket.as_ref() {
        actual_mounts.push(serde_json::json!({"Type":"bind","Propagation":"rprivate","Source":socket.source.host_path(),"Destination":socket.target,"RW":!socket.readonly}));
        if let Some(dir) = &socket.proxy_dir {
            actual_mounts.push(serde_json::json!({"Type":"bind","Propagation":"rprivate","Source":dir,"Destination":dir,"RW":true}));
        }
    }
    let tmpfs: BTreeMap<_, _> = tmpfs
        .iter()
        .map(|m| {
            (
                m.target.clone(),
                tmpfs_docker_value(m).split_once(':').unwrap().1.to_owned(),
            )
        })
        .collect();
    let declared_mounts: Vec<_> = actual_mounts.iter().map(|m| serde_json::json!({"Type":m["Type"],
        "Source":if m["Type"] == "volume" { &m["Name"] } else { &m["Source"] }, "Target":m["Destination"],"ReadOnly":!m["RW"].as_bool().unwrap()})).collect();
    let mut host = serde_json::json!({"Mounts":declared_mounts,"VolumeDriver":"","Privileged":false,"NetworkMode":"default","Binds":null,"VolumesFrom":null,"DeviceRequests":null,
        "SecurityOpt":null,"GroupAdd":null,"DeviceCgroupRules":null,"PublishAllPorts":false,"AutoRemove":false,"CgroupnsMode":cgroupns,"RestartPolicy":{"Name":"no","MaximumRetryCount":0},"ReadonlyRootfs":false,"PidMode":"","UTSMode":"","UsernsMode":"","IpcMode":ipc,
        "Devices":[],"CapAdd":null,"CapDrop":null,"PortBindings":{},"Tmpfs":tmpfs});
    if let Some(guest) = macos_guest.as_ref() {
        host["Devices"] = serde_json::json!([{"PathOnHost":"/dev/kvm","PathInContainer":"/dev/kvm","CgroupPermissions":"rwm"},
            {"PathOnHost":"/dev/net/tun","PathInContainer":"/dev/net/tun","CgroupPermissions":"rwm"}]);
        host["CapAdd"] = serde_json::json!(["NET_ADMIN"]);
        host["PortBindings"] = serde_json::json!({"22/tcp":[{"HostIp":"127.0.0.1","HostPort":guest.ssh_port.to_string()}],
            "8006/tcp":[{"HostIp":"127.0.0.1","HostPort":guest.web_port.to_string()}]});
    }
    serde_json::json!({"Id":CONTAINER_ID,"Name":format!("/{}",labels[LABEL_CONTAINER_NAME]),"Image":image_identity,"State":{"Running":running},"Config":config,"HostConfig":host,"Mounts":actual_mounts})
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
            [
                SetupEnsureCommand::Inspect { .. },
                SetupEnsureCommand::ImageInspect { .. }
            ]
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
fn inspection_is_complete_bounded_json_and_legacy_or_duplicate_keys_refuse() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("src")).unwrap();
    let plan = plan(workspace.path());
    let observed = observed(&plan, true);
    assert_eq!(
        SetupEnsureCommand::Inspect {
            container_name: observed.labels[LABEL_CONTAINER_NAME].clone()
        }
        .docker_args()[3],
        "{{json .}}"
    );
    assert_eq!(
        parse_inspection(&serde_json::to_vec(&observed.configuration).unwrap()).unwrap(),
        observed
    );
    assert!(parse_inspection(b"legacy\tlabels\tonly").is_err());
    assert!(crate::creation::bounded_json(br#"{"Config":{"Env":[],"Env":["foreign"]}}"#).is_err());
    assert!(
        crate::creation::bounded_json(&vec![b' '; crate::creation::MAX_OBSERVATION_BYTES + 1])
            .is_err()
    );
}

#[test]
fn volume_inspect_requests_complete_private_metadata() {
    let volume_name = format!("bosn-v-stack-{HASH}");
    let command = SetupEnsureCommand::VolumeInspect {
        volume_name: volume_name.clone(),
    };
    let args = command.docker_args();
    assert_eq!(args[0], "volume");
    assert_eq!(args[1], "inspect");
    assert_eq!(args[2], "--format");
    assert_eq!(args[3], "{{json .}}");
    assert_eq!(args[4], volume_name);
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
            container_name: setup_container_name(&plan, &workspace, &image).unwrap(),
            container_id: CONTAINER_ID.into(),
            image_identity: IDENTITY.into(),
            created: true,
            started: true,
            running: true,
        }
    );
    let calls = engine.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 5);
    assert!(
        matches!(&calls[0], SetupEnsureCommand::Inspect { container_name } if container_name == &setup_container_name(&plan, &workspace, &image).unwrap())
    );
    assert_eq!(
        calls[4],
        SetupEnsureCommand::Start {
            container_name: setup_container_name(&plan, &workspace, &image).unwrap()
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
            SetupEnsureCommand::ImageInspect { .. },
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
        [
            SetupEnsureCommand::Inspect { .. },
            SetupEnsureCommand::ImageInspect { .. }
        ]
    ));
}

#[test]
fn matching_container_reuse_reinspects_volume_metadata_without_mutation() {
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
    let volume = &plan.named_volumes[0];
    let receipt = serde_json::json!({"Name":volume.name,"Driver":"local","Scope":"local","Options":null,"Labels":volume.labels,
        "Mountpoint":format!("/var/lib/docker/volumes/{}/_data",volume.name)});
    let engine = FakeEngine::with_results([
        inspection(&plan, true),
        command(serde_json::to_vec(&receipt).unwrap()),
    ]);

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
        [
            SetupEnsureCommand::Inspect { .. },
            SetupEnsureCommand::ImageInspect { .. },
            SetupEnsureCommand::VolumeInspect { .. }
        ]
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
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
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
            SetupEnsureCommand::ImageInspect { .. },
            SetupEnsureCommand::Start { .. }
        ]
    ));
}

mod creation;

mod runtime_options;
