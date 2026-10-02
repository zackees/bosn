use std::{
    collections::{BTreeMap, VecDeque},
    future::{Ready, ready},
    sync::Mutex,
    time::Duration,
};

use super::*;
use kernal_api::async_engine::{CancellationSource, RuntimeBuilder, channel};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const IDENTITY: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Default)]
struct FakeEngine {
    calls: Mutex<Vec<SetupTaskCommand>>,
    results: Mutex<VecDeque<Result<CommandResult, CommandError>>>,
}

impl FakeEngine {
    fn with_results(
        results: impl IntoIterator<Item = Result<CommandResult, CommandError>>,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}

impl SetupTaskEngine for FakeEngine {
    type StreamFuture<'a> = Ready<Result<CommandResult, CommandError>>;

    fn stream<'a>(
        &'a self,
        command: SetupTaskCommand,
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
                .expect("configured task result"),
        )
    }
}

#[derive(Default)]
struct FakeAppEngine {
    calls: Mutex<Vec<SetupAppTaskCommand>>,
    results: Mutex<VecDeque<Result<CommandResult, CommandError>>>,
}
impl FakeAppEngine {
    fn with_results(
        results: impl IntoIterator<Item = Result<CommandResult, CommandError>>,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}
impl SetupAppTaskEngine for FakeAppEngine {
    type StreamFuture<'a> = Ready<Result<CommandResult, CommandError>>;
    fn stream<'a>(
        &'a self,
        command: SetupAppTaskCommand,
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
                .expect("configured app task result"),
        )
    }
}

fn runtime() -> kernal_api::async_engine::Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn command_result(
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<CommandResult, CommandError> {
    Ok(CommandResult {
        exit_code,
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

fn plan(workspace: &Path) -> SetupPlan {
    let image = format!("registry.example/team/app@sha256:{HASH}");
    let app_environment = BTreeMap::from([
        ("APP_ONLY".into(), "from-app".into()),
        ("OVERRIDE".into(), "from-app".into()),
    ]);
    let task_environment = BTreeMap::from([
        ("OVERRIDE".into(), "from-task".into()),
        ("TASK_ONLY".into(), "yes".into()),
    ]);
    let task = SetupTask {
        command: "cargo test --locked".into(),
        workdir: Some("src".into()),
        environment: task_environment,
    };
    SetupPlan {
        source_kind: crate::SetupSourceKind::LocalFile,
        content_sha256: HASH.into(),
        schema_version: 1,
        workspace_root: fs::canonical_context_path(workspace).unwrap(),
        asset_root: None,
        task_names: vec!["check".into()],
        app: bosn_core::SetupApp {
            source: bosn_core::SetupSource::PinnedImage(image.clone()),
            environment: app_environment,
            workdir: Some(".".into()),
            command: None,
            mounts: vec![bosn_core::WorkspaceMount {
                source: ".".into(),
                target: "/workspace".into(),
                readonly: true,
            }],
        },
        tasks: BTreeMap::from([("check".into(), task)]),
        app_source: SetupPlanAppSource::PinnedImage { image },
        named_volumes: Vec::new(),
        tmpfs: Vec::new(),
        host_docker_socket: None,
        macos_guest: None,
    }
}

fn prepared(plan: &SetupPlan) -> PreparedImage {
    let SetupPlanAppSource::PinnedImage { image } = &plan.app_source else {
        unreachable!();
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

fn run(
    engine: &FakeEngine,
    plan: &SetupPlan,
    workspace: &Path,
    task_name: &str,
    image: &PreparedImage,
    cancellation: &CancellationToken,
    options: RunOptions,
) -> Result<SetupTaskResult, SetupTaskError> {
    let (events, _receiver) = channel(8);
    runtime().run(execute_setup_task(
        engine,
        SetupTaskRequest {
            plan,
            workspace_root: workspace.to_path_buf(),
            task_name: task_name.into(),
            prepared_image: image,
            options,
            cancellation,
            events: &events,
        },
    ))
}

#[test]
fn declared_task_becomes_only_a_semantic_bounded_run_command() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let engine = FakeEngine::with_results([command_result(0, b"ok\n", b"")]);
    let cancellation = CancellationSource::new();

    let result = run(
        &engine,
        &plan,
        &workspace,
        "check",
        &image,
        &cancellation.token(),
        RunOptions::streaming(Duration::from_secs(2), 4096),
    )
    .unwrap();

    assert_eq!(
        result,
        SetupTaskResult {
            task_name: "check".into(),
            image_identity: IDENTITY.into(),
            exit_code: 0,
        }
    );
    let expected_source = fs::canonical_context_path(&workspace).unwrap();
    let expected = SetupTaskCommand::Run {
        image_identity: IDENTITY.into(),
        mounts: vec![SetupTaskMount {
            source: expected_source.clone(),
            target: "/workspace".into(),
            readonly: true,
        }],
        environment: BTreeMap::from([
            ("APP_ONLY".into(), "from-app".into()),
            ("OVERRIDE".into(), "from-task".into()),
            ("TASK_ONLY".into(), "yes".into()),
        ]),
        workdir: Some("/workspace/src".into()),
        command: "cargo test --locked".into(),
    };
    assert_eq!(*engine.calls.lock().unwrap(), vec![expected.clone()]);
    let source = expected_source.to_string_lossy();
    assert_eq!(
        expected.docker_args(),
        vec![
            "run",
            "--rm",
            "--mount",
            &format!("type=bind,src={source},dst=/workspace,readonly"),
            "--env",
            "APP_ONLY=from-app",
            "--env",
            "OVERRIDE=from-task",
            "--env",
            "TASK_ONLY=yes",
            "--workdir",
            "/workspace/src",
            IDENTITY,
        ]
        .into_iter()
        .map(String::from)
        .chain(crate::shell::login_shell_args("cargo test --locked"))
        .collect::<Vec<_>>()
    );
}

#[test]
fn declared_app_task_has_only_the_content_addressed_exec_shape() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let engine = FakeAppEngine::with_results([command_result(0, b"ok", b"")]);
    let cancellation = CancellationSource::new();
    let (events, _receiver) = channel(8);
    let result = runtime()
        .run(execute_setup_app_task(
            &engine,
            SetupAppTaskRequest {
                plan: &plan,
                workspace_root: workspace,
                task_name: "check".into(),
                passthrough_env: Vec::new(),
                prepared_image: &image,
                options: RunOptions::streaming(Duration::from_secs(2), 4096),
                cancellation: &cancellation.token(),
                events: &events,
            },
        ))
        .unwrap();
    assert_eq!(result.task_name, "check");
    assert_eq!(
        *engine.calls.lock().unwrap(),
        vec![SetupAppTaskCommand::Exec {
            container_name: format!("bosn-setup-{HASH}"),
            passthrough_env: Vec::new(),
            command: "cargo test --locked".into(),
        }]
    );
    assert_eq!(
        SetupAppTaskCommand::Exec {
            container_name: format!("bosn-setup-{HASH}"),
            passthrough_env: Vec::new(),
            command: "cargo test --locked".into(),
        }
        .docker_args(),
        vec!["container", "exec", &format!("bosn-setup-{HASH}"),]
            .into_iter()
            .map(String::from)
            .chain(crate::shell::login_shell_args("cargo test --locked"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn app_task_secret_env_is_forwarded_by_name_only() {
    let args = SetupAppTaskCommand::Exec {
        container_name: format!("bosn-setup-{HASH}"),
        passthrough_env: vec!["GITHUB_TOKEN".into()],
        command: "true".into(),
    }
    .docker_args();
    assert_eq!(
        args,
        vec![
            "container",
            "exec",
            "--env",
            "GITHUB_TOKEN",
            &format!("bosn-setup-{HASH}"),
        ]
        .into_iter()
        .map(String::from)
        .chain(crate::shell::login_shell_args("true"))
        .collect::<Vec<_>>()
    );
    assert!(args.iter().all(|arg| !arg.contains("GITHUB_TOKEN=")));
}

#[test]
fn app_task_refuses_a_passthrough_name_that_could_carry_a_value() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let engine = FakeAppEngine::with_results([]);
    let cancellation = CancellationSource::new();
    let (events, _receiver) = channel(8);
    assert!(
        runtime()
            .run(execute_setup_app_task(
                &engine,
                SetupAppTaskRequest {
                    plan: &plan,
                    workspace_root: workspace,
                    task_name: "check".into(),
                    passthrough_env: vec!["GITHUB_TOKEN=canary".into()],
                    prepared_image: &image,
                    options: RunOptions::streaming(Duration::from_secs(2), 4096),
                    cancellation: &cancellation.token(),
                    events: &events,
                },
            ))
            .is_err()
    );
    assert!(engine.calls.lock().unwrap().is_empty());
}

#[test]
fn app_task_rechecks_that_its_workdir_bind_is_a_directory_before_exec() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("not-a-directory"), "proof").unwrap();
    let mut plan = plan(&workspace);
    plan.app.workdir = Some("not-a-directory".into());
    plan.tasks.get_mut("check").unwrap().workdir = None;
    plan.app.mounts[0].source = "not-a-directory".into();
    let image = prepared(&plan);
    let engine = FakeAppEngine::with_results([]);
    let cancellation = CancellationSource::new();
    let (events, _receiver) = channel(8);
    assert!(
        runtime()
            .run(execute_setup_app_task(
                &engine,
                SetupAppTaskRequest {
                    plan: &plan,
                    workspace_root: workspace,
                    task_name: "check".into(),
                    passthrough_env: Vec::new(),
                    prepared_image: &image,
                    options: RunOptions::streaming(Duration::from_secs(2), 4096),
                    cancellation: &cancellation.token(),
                    events: &events,
                },
            ))
            .is_err()
    );
    assert!(engine.calls.lock().unwrap().is_empty());
}

#[test]
fn unknown_or_tampered_inputs_are_refused_before_engine_execution() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let engine = FakeEngine::default();
    let cancellation = CancellationSource::new();
    let options = RunOptions::streaming(Duration::from_secs(1), 1024);

    assert!(matches!(
        run(
            &engine,
            &plan,
            &workspace,
            "missing",
            &image,
            &cancellation.token(),
            options,
        ),
        Err(SetupTaskError::UnknownTask)
    ));
    let mut tampered = image.clone();
    tampered.observed_identity = "sha256:not-a-digest".into();
    assert!(matches!(
        run(
            &engine,
            &plan,
            &workspace,
            "check",
            &tampered,
            &cancellation.token(),
            options,
        ),
        Err(SetupTaskError::InvalidRequest(_))
    ));
    let other_workspace = temporary.path().join("other");
    std::fs::create_dir(&other_workspace).unwrap();
    assert!(matches!(
        run(
            &engine,
            &plan,
            &other_workspace,
            "check",
            &image,
            &cancellation.token(),
            options,
        ),
        Err(SetupTaskError::InvalidRequest(_))
    ));
    assert!(engine.calls.lock().unwrap().is_empty());
}

#[test]
fn nonzero_cancellation_deadline_and_output_budget_are_terminal_and_bounded() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let plan = plan(&workspace);
    let image = prepared(&plan);
    let options = RunOptions::streaming(Duration::from_secs(1), 1024);

    let failed = FakeEngine::with_results([command_result(17, b"", b"failed")]);
    let active = CancellationSource::new();
    assert!(matches!(
        run(
            &failed,
            &plan,
            &workspace,
            "check",
            &image,
            &active.token(),
            options,
        ),
        Err(SetupTaskError::TaskFailed { exit_code: 17, .. })
    ));
    assert_eq!(failed.calls.lock().unwrap().len(), 1);

    let cancelled = CancellationSource::new();
    cancelled.cancel();
    let no_call = FakeEngine::default();
    assert!(matches!(
        run(
            &no_call,
            &plan,
            &workspace,
            "check",
            &image,
            &cancelled.token(),
            options,
        ),
        Err(SetupTaskError::Cancelled)
    ));
    assert!(no_call.calls.lock().unwrap().is_empty());

    let deadline = FakeEngine::with_results([Err(CommandError::Deadline {
        reaped_pid: None,
        cleanup: None,
    })]);
    assert!(matches!(
        run(
            &deadline,
            &plan,
            &workspace,
            "check",
            &image,
            &active.token(),
            RunOptions::streaming(Duration::from_secs(1), 4),
        ),
        Err(SetupTaskError::Deadline)
    ));

    let oversized = FakeEngine::with_results([command_result(0, b"12345", b"")]);
    assert!(matches!(
        run(
            &oversized,
            &plan,
            &workspace,
            "check",
            &image,
            &active.token(),
            RunOptions::streaming(Duration::from_secs(1), 4),
        ),
        Err(SetupTaskError::Transport(CommandError::OutputLimit {
            limit: 4,
            ..
        }))
    ));
    let zero_budget = FakeEngine::default();
    assert!(matches!(
        run(
            &zero_budget,
            &plan,
            &workspace,
            "check",
            &image,
            &active.token(),
            RunOptions::streaming(Duration::from_secs(1), 0),
        ),
        Err(SetupTaskError::InvalidRequest("output budget is zero"))
    ));
    assert!(zero_budget.calls.lock().unwrap().is_empty());
}
