//! Opt-in, live-Docker proof for one daemon-owned declared setup task.
//!
//! The test creates no managed application container and does not pull or
//! delete images.  The task's short-lived `docker run --rm` container is
//! identified only from its own documented artifact, then inspected after the
//! job to prove it has already been removed.

use std::{
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bosn_engine::{CommandResult, DockerEngine, RunOptions};
use bosn_service::{Client, SetupPreparePolicy, SetupTaskJobRequest, jobs::MAX_LOG_LINE_BYTES};
use kernal_api::async_engine::{Runtime, RuntimeBuilder};

const PINNED_ALPINE: &str =
    "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b";
const READY_DEADLINE: Duration = Duration::from_secs(10);
const JOB_DEADLINE: Duration = Duration::from_secs(90);
const DOCKER_DEADLINE: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// A child daemon is reaped even when a live-test assertion fails.
struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn start(state: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["daemon", "serve", "--state-dir"])
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start production Bosn daemon");
        Self { child }
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + READY_DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().expect("observe daemon") {
                return status;
            }
            assert!(Instant::now() < deadline, "daemon did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn docker_capture(
    engine: &DockerEngine,
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
) -> CommandResult {
    engine
        .with_args(args)
        .capture(RunOptions::bounded(DOCKER_DEADLINE, 64 * 1024))
        .expect("run bounded kernal-api Docker command")
}

fn require_pinned_alpine(engine: &DockerEngine) {
    let result = docker_capture(
        engine,
        ["image", "inspect", "--format", "{{.Id}}", PINNED_ALPINE],
    );
    assert!(
        result.ok(),
        "Docker image {PINNED_ALPINE:?} is unavailable: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&result.stdout).trim().is_empty(),
        "pinned Alpine image has no identity"
    );
}

fn wait_for_client(runtime: &Runtime, daemon: &mut DaemonChild, state: &Path) -> Client {
    let client = Client::for_state(state).expect("construct daemon client");
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        if runtime.run(client.ping()).is_ok() {
            return client;
        }
        assert!(
            daemon.child.try_wait().expect("observe daemon").is_none(),
            "daemon exited before becoming ready"
        );
        assert!(Instant::now() < deadline, "daemon did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_success_and_logs(runtime: &Runtime, client: &Client, job_id: u64) -> Vec<String> {
    let deadline = Instant::now() + JOB_DEADLINE;
    loop {
        let status = runtime
            .run(client.job_status(job_id))
            .expect("read setup task job status");
        match status.state.as_str() {
            "Succeeded" => {
                let logs = runtime
                    .run(client.job_logs(job_id, 0, 256))
                    .expect("read setup task job logs");
                assert!(
                    logs.records
                        .iter()
                        .all(|record| record.line.len() <= MAX_LOG_LINE_BYTES),
                    "daemon returned an unbounded task log record"
                );
                return logs.records.into_iter().map(|record| record.line).collect();
            }
            "Failed" | "Cancelled" | "Superseded" => {
                let logs = runtime
                    .run(client.job_logs(job_id, 0, 256))
                    .map(|page| {
                        page.records
                            .into_iter()
                            .map(|record| record.line)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                panic!(
                    "setup task job {job_id} ended {}: {:?}; logs: {logs:?}",
                    status.state, status.error,
                );
            }
            "Queued" | "Running" | "Cancelling" => {}
            unexpected => panic!("unexpected setup task job state {unexpected}"),
        }
        assert!(
            Instant::now() < deadline,
            "setup task job {job_id} timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn test_unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn assert_removed_task_container(engine: &DockerEngine, short_id: &str) {
    assert!(
        short_id.len() == 12 && short_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "declared task did not record an expected Docker short container id: {short_id:?}"
    );
    let result = docker_capture(
        engine,
        ["container", "inspect", "--format", "{{.Id}}", short_id],
    );
    assert_eq!(
        result.exit_code,
        1,
        "declared task container {short_id} was retained; setup tasks must use docker run --rm: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test setup_task_docker --locked -- --ignored --exact live_docker_setup_task_runs_only_the_declared_one_file_task`
///
/// The production daemon receives only a [`SetupTaskJobRequest`]. It can pick
/// the named document task, policy, deadline, and output budget; it has no
/// caller-provided command, image, mount, workdir, environment, container, or
/// raw Docker controls. This test requires a usable Docker daemon and the
/// exact pre-pulled `PINNED_ALPINE` image. It neither creates a persistent app
/// nor deletes images or containers.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_setup_task_runs_only_the_declared_one_file_task() {
    let engine = DockerEngine::docker();
    require_pinned_alpine(&engine);
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let config_root = root.path().join("config");
    std::fs::create_dir(&workspace).expect("create workspace");
    std::fs::create_dir(&config_root).expect("create config directory");
    let config = config_root.join("setup.toml");
    let unique = test_unique_suffix();
    let stdout_marker = format!("bosn-task-stdout-{unique}");
    let stderr_marker = format!("bosn-task-stderr-{unique}");
    std::fs::write(
        &config,
        format!(
            "version = 1\n\
             [app]\n\
             image = '{PINNED_ALPINE}'\n\
             workdir = '.'\n\
             [app.environment]\n\
             APP_ONLY = 'from-app'\n\
             OVERRIDE = 'from-app'\n\
             [[app.mount]]\n\
             source = '.'\n\
             target = '/workspace'\n\
             readonly = false\n\
             [task.prove]\n\
             command = '''printf '%s\\n' \"$PWD\" > task-proof.txt\n\
             printf '%s\\n' \"$APP_ONLY\" >> task-proof.txt\n\
             printf '%s\\n' \"$OVERRIDE\" >> task-proof.txt\n\
             printf '%s\\n' \"$TASK_ONLY\" >> task-proof.txt\n\
             printf '%s\\n' \"$HOSTNAME\" > task-container-id.txt\n\
             printf '%s\\n' '{stdout_marker}'\n\
             printf '%s\\n' '{stderr_marker}' >&2\n\
             '''\n\
             workdir = '.'\n\
             [task.prove.environment]\n\
             OVERRIDE = 'from-task'\n\
             TASK_ONLY = 'from-task'\n"
        ),
    )
    .expect("write single self-contained setup document");

    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("construct kernal-api runtime");
    let request = SetupTaskJobRequest {
        workspace: workspace.clone(),
        config: config.to_string_lossy().into_owned(),
        policy: SetupPreparePolicy::Refresh,
        task_name: "prove".into(),
        deadline: JOB_DEADLINE,
        output_limit: OUTPUT_LIMIT,
    };

    let mut first_daemon = DaemonChild::start(&state);
    let first_client = wait_for_client(&runtime, &mut first_daemon, &state);
    let first_job = runtime
        .run(first_client.submit_setup_task(request))
        .expect("submit production declared-task job");
    let first_logs = wait_for_success_and_logs(&runtime, &first_client, first_job);
    assert!(
        first_logs
            .iter()
            .any(|line| line == "[setup] preparing application image"),
        "task job did not report image preparation: {first_logs:?}"
    );
    assert!(
        first_logs
            .iter()
            .any(|line| line == "[setup] running declared task prove"),
        "task job did not report its declared task: {first_logs:?}"
    );
    assert!(
        first_logs.iter().any(|line| line.contains(&stdout_marker)),
        "declared task stdout was not retained in bounded job logs: {first_logs:?}"
    );
    assert!(
        first_logs.iter().any(|line| line.contains(&stderr_marker)),
        "declared task stderr was not retained in bounded job logs: {first_logs:?}"
    );
    assert!(
        first_logs
            .iter()
            .any(|line| line.contains("completed declared task prove with image")),
        "task success receipt was not retained: {first_logs:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("task-proof.txt"))
            .expect("read declared-task workspace artifact"),
        "/workspace\nfrom-app\nfrom-task\nfrom-task\n",
        "only the document-derived mount, workdir, and environment reached Docker"
    );
    let first_container_id = std::fs::read_to_string(workspace.join("task-container-id.txt"))
        .expect("read declared-task container identity");
    assert_removed_task_container(&engine, first_container_id.trim());

    runtime
        .run(first_client.shutdown())
        .expect("shut down first daemon");
    assert!(
        first_daemon.wait_for_exit().success(),
        "first daemon failed"
    );

    // Tasks have no persistent container to reuse. A new daemon nevertheless
    // must be able to plan from the cached one-file receipt and run the same
    // declared task offline after the source TOML has disappeared.
    std::fs::remove_file(&config).expect("remove source setup document after cache fill");
    let mut second_daemon = DaemonChild::start(&state);
    let second_client = wait_for_client(&runtime, &mut second_daemon, &state);
    let second_job = runtime
        .run(second_client.submit_setup_task(SetupTaskJobRequest {
            workspace: workspace.clone(),
            config: config.to_string_lossy().into_owned(),
            policy: SetupPreparePolicy::Offline,
            task_name: "prove".into(),
            deadline: JOB_DEADLINE,
            output_limit: OUTPUT_LIMIT,
        }))
        .expect("submit offline declared-task job after daemon restart");
    let second_logs = wait_for_success_and_logs(&runtime, &second_client, second_job);
    assert!(
        second_logs
            .iter()
            .any(|line| line == "[setup] running declared task prove"),
        "restarted daemon did not run cached declared task: {second_logs:?}"
    );
    let second_container_id = std::fs::read_to_string(workspace.join("task-container-id.txt"))
        .expect("read restarted declared-task container identity");
    assert_ne!(
        second_container_id.trim(),
        first_container_id.trim(),
        "separate ephemeral task invocations unexpectedly reported the same container"
    );
    assert_removed_task_container(&engine, second_container_id.trim());
    assert_eq!(
        std::fs::read_to_string(workspace.join("task-proof.txt"))
            .expect("read restarted declared-task artifact"),
        "/workspace\nfrom-app\nfrom-task\nfrom-task\n"
    );

    runtime
        .run(second_client.shutdown())
        .expect("shut down second daemon");
    assert!(
        second_daemon.wait_for_exit().success(),
        "second daemon failed"
    );
}
