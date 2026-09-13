//! Opt-in, live-Docker proof for the daemon-owned setup-app ensure path.
//!
//! This test is intentionally ignored: it creates one short-lived container
//! through the production daemon and therefore needs a Docker daemon and the
//! pinned Alpine image documented below.  Its drop guard removes only the
//! exact deterministic container after re-checking Bosn's ownership labels.

mod support;

use std::{
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bosn_core::{ResourceKind, ResourceState};
use bosn_engine::{CommandResult, DockerEngine, RunOptions};
use bosn_registry::Registry;
use bosn_service::{Client, SetupEnsureJobRequest, SetupPreparePolicy};
use bosn_setup::{SetupAcquirePolicy, SetupPlanRequest, plan_setup};
use kernal_api::{async_engine::RuntimeBuilder, hash::sha256_bytes};
use support::tls_setup_server::{TlsSetupServer, certificate_path};

const PINNED_ALPINE: &str =
    "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b";
const READY_DEADLINE: Duration = Duration::from_secs(10);
const JOB_DEADLINE: Duration = Duration::from_secs(90);
const DOCKER_DEADLINE: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: usize = 1024 * 1024;
const MANAGED_LABEL: &str = "com.zackees.bosn.setup-managed";
const CONTENT_LABEL: &str = "com.zackees.bosn.setup-content-sha256";
const NAME_LABEL: &str = "com.zackees.bosn.setup-container";

/// A child daemon is reaped even if an assertion fails before the normal
/// authenticated shutdown path runs.
struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn start(state: &Path) -> Self {
        Self::start_with_certificate(state, None)
    }

    fn start_with_certificate(state: &Path, certificate: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bosn"));
        command
            .args(["daemon", "serve", "--state-dir"])
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(certificate) = certificate {
            command
                .env("SSL_CERT_FILE", certificate)
                .env("NO_PROXY", "localhost,127.0.0.1");
        }
        Self {
            child: command.spawn().expect("start production Bosn daemon"),
        }
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

#[derive(Debug, Eq, PartialEq)]
struct ContainerInspection {
    id: String,
    running: bool,
    image: String,
    managed: String,
    content_sha256: String,
    container_name: String,
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

fn inspect_container(
    engine: &DockerEngine,
    container_name: &str,
) -> Result<Option<ContainerInspection>, String> {
    let inspect_format = format!(
        "{{{{.Id}}}}\t{{{{.State.Running}}}}\t{{{{.Config.Image}}}}\t{{{{index .Config.Labels \"{MANAGED_LABEL}\"}}}}\t{{{{index .Config.Labels \"{CONTENT_LABEL}\"}}}}\t{{{{index .Config.Labels \"{NAME_LABEL}\"}}}}"
    );
    let result = docker_capture(
        engine,
        [
            "container",
            "inspect",
            "--format",
            inspect_format.as_str(),
            container_name,
        ],
    );
    if result.exit_code == 1 {
        return Ok(None);
    }
    if !result.ok() {
        return Err(format!(
            "docker inspect exited {}: {}",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let text = String::from_utf8(result.stdout)
        .map_err(|_| "docker inspect returned non-UTF-8 output".to_owned())?;
    let fields: Vec<_> = text.trim_end_matches(['\r', '\n']).split('\t').collect();
    if fields.len() != 6 || fields.iter().any(|field| field.is_empty()) {
        return Err("docker inspect returned an incomplete Bosn container record".into());
    }
    let running = match fields[1] {
        "true" => true,
        "false" => false,
        _ => return Err("docker inspect returned an invalid running value".into()),
    };
    Ok(Some(ContainerInspection {
        id: fields[0].into(),
        running,
        image: fields[2].into(),
        managed: fields[3].into(),
        content_sha256: fields[4].into(),
        container_name: fields[5].into(),
    }))
}

/// Removes only a container this test can still prove is its own.  It never
/// uses a label selector, prune, or a name supplied by Docker output.
struct ExactContainerCleanup {
    engine: DockerEngine,
    container_name: String,
    content_sha256: String,
}

impl Drop for ExactContainerCleanup {
    fn drop(&mut self) {
        match inspect_container(&self.engine, &self.container_name) {
            Ok(None) => {}
            Ok(Some(observed))
                if observed.managed == "v1"
                    && observed.content_sha256 == self.content_sha256
                    && observed.container_name == self.container_name =>
            {
                let result = docker_capture(
                    &self.engine,
                    ["container", "rm", "--force", self.container_name.as_str()],
                );
                if !result.ok() {
                    eprintln!(
                        "live setup ensure cleanup could not remove {}: {}",
                        self.container_name,
                        String::from_utf8_lossy(&result.stderr)
                    );
                }
            }
            Ok(Some(_)) => eprintln!(
                "live setup ensure cleanup refused unexpected container {}",
                self.container_name
            ),
            Err(error) => eprintln!(
                "live setup ensure cleanup could not inspect {}: {error}",
                self.container_name
            ),
        }
    }
}

fn wait_for_client(
    runtime: &kernal_api::async_engine::Runtime,
    daemon: &mut DaemonChild,
    state: &Path,
) -> Client {
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

fn wait_for_success(runtime: &kernal_api::async_engine::Runtime, client: &Client, job_id: u64) {
    let deadline = Instant::now() + JOB_DEADLINE;
    loop {
        let status = runtime
            .run(client.job_status(job_id))
            .expect("read setup ensure job status");
        match status.state.as_str() {
            "Succeeded" => return,
            "Failed" | "Cancelled" | "Superseded" => {
                let logs = runtime
                    .run(client.job_logs(job_id, 0, 64))
                    .map(|page| {
                        page.records
                            .into_iter()
                            .map(|record| record.line)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                panic!(
                    "setup ensure job {job_id} ended {}: {:?}; logs: {logs:?}",
                    status.state, status.error,
                )
            }
            "Queued" | "Running" | "Cancelling" => {}
            unexpected => panic!("unexpected setup ensure job state {unexpected}"),
        }
        assert!(
            Instant::now() < deadline,
            "setup ensure job {job_id} timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_stopped(engine: &DockerEngine, name: &str) -> ContainerInspection {
    let deadline = Instant::now() + DOCKER_DEADLINE;
    loop {
        let observed = inspect_container(engine, name)
            .expect("inspect exact managed app while waiting for stopped")
            .expect("managed app disappeared before GC preview");
        if !observed.running {
            return observed;
        }
        assert!(
            Instant::now() < deadline,
            "retired test app did not stop before GC"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn image_identity_for(engine: &DockerEngine, reference: &str) -> String {
    let result = docker_capture(
        engine,
        ["image", "inspect", "--format", "{{.Id}}", reference],
    );
    assert!(
        result.ok(),
        "Docker image {reference:?} is unavailable: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let identity = String::from_utf8(result.stdout).expect("image identity is UTF-8");
    let identity = identity.trim().to_owned();
    assert!(!identity.is_empty(), "pinned Alpine image has no identity");
    identity
}

fn pinned_alpine_identity(engine: &DockerEngine) -> String {
    image_identity_for(engine, PINNED_ALPINE)
}

fn test_unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

/// Run with:
/// `cargo test -p bosn-service --test setup_ensure_docker -- --ignored --exact live_docker_setup_ensure_creates_and_reuses_one_managed_app`
///
/// It needs a usable local Docker daemon and the exact `PINNED_ALPINE` image.
/// The test does not pull an unpinned image, and its cleanup refuses to remove
/// any candidate whose three expected Bosn ownership labels do not match.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
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
    let container_name = format!("bosn-setup-{}", plan.content_sha256);
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect deterministic test container")
            .is_none(),
        "unique test container name already exists; refusing to touch it"
    );
    let cleanup = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: container_name.clone(),
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
/// `soldr cargo test -j1 -p bosn-service --test setup_ensure_docker --locked -- --ignored --exact live_docker_setup_gc_apply_removes_only_retired_generation`
///
/// This opt-in observation uses two different one-file documents for one
/// canonical workspace. The preview token may identify only the retired first
/// app; production daemon apply must remove that exact stopped candidate while
/// leaving the current app and its inspected image alone. Cleanup verifies
/// ownership separately for each exact name.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_setup_gc_apply_removes_only_retired_generation() {
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
    let document = |generation: &str| {
        let sleep = if generation == "a" { 1 } else { 120 };
        format!(
            "version = 1\n[app]\nimage = '{PINNED_ALPINE}'\ncommand = 'exec sleep {sleep} # bosn-rollover-{unique}-{generation}'\n"
        )
    };
    std::fs::write(&config, document("a")).expect("write first setup document");
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("construct kernal-api runtime");
    let plan_a = runtime
        .run(plan_setup(SetupPlanRequest {
            state_dir: state.clone(),
            workspace: workspace.clone(),
            locator: config.to_string_lossy().into_owned(),
            policy: SetupAcquirePolicy::OnlineRefresh,
        }))
        .expect("plan first generation");
    std::fs::write(&config, document("b")).expect("write second setup document");
    let plan_b = runtime
        .run(plan_setup(SetupPlanRequest {
            state_dir: state.clone(),
            workspace: workspace.clone(),
            locator: config.to_string_lossy().into_owned(),
            policy: SetupAcquirePolicy::OnlineRefresh,
        }))
        .expect("plan second generation");
    assert_ne!(
        plan_a.content_sha256, plan_b.content_sha256,
        "the two one-file documents must form distinct generations"
    );
    let name_a = format!("bosn-setup-{}", plan_a.content_sha256);
    let name_b = format!("bosn-setup-{}", plan_b.content_sha256);
    for name in [&name_a, &name_b] {
        assert!(
            inspect_container(&engine, name)
                .expect("inspect deterministic test container")
                .is_none(),
            "unique test container name already exists; refusing to touch it"
        );
    }
    let cleanup_a = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: name_a.clone(),
        content_sha256: plan_a.content_sha256.clone(),
    };
    let cleanup_b = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: name_b.clone(),
        content_sha256: plan_b.content_sha256.clone(),
    };

    // Restore the first document before the production daemon observes it.
    std::fs::write(&config, document("a")).expect("restore first setup document");
    let request = SetupEnsureJobRequest {
        workspace: workspace.clone(),
        config: config.to_string_lossy().into_owned(),
        policy: SetupPreparePolicy::Refresh,
        deadline: JOB_DEADLINE,
        output_limit: OUTPUT_LIMIT,
    };
    let mut daemon = DaemonChild::start(&state);
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let first_job = runtime
        .run(client.submit_setup_ensure(request.clone()))
        .expect("submit first generation");
    wait_for_success(&runtime, &client, first_job);
    let first = inspect_container(&engine, &name_a)
        .expect("inspect first generation")
        .expect("first managed app exists");
    assert!(first.running);
    assert_eq!(first.image, expected_image);

    std::fs::write(&config, document("b")).expect("restore second setup document");
    let second_job = runtime
        .run(client.submit_setup_ensure(request))
        .expect("submit second generation");
    wait_for_success(&runtime, &client, second_job);
    let current = inspect_container(&engine, &name_b)
        .expect("inspect current generation")
        .expect("current managed app exists");
    // The test document deliberately exits by itself. GC may only remove a
    // stopped retired container; it must never stop an app as a side effect.
    let old_after_rollover = wait_for_stopped(&engine, &name_a);
    assert!(
        !old_after_rollover.running,
        "old app stops by its declared command"
    );
    assert!(current.running, "current app must be running");
    assert_ne!(old_after_rollover.id, current.id);
    assert_eq!(current.image, expected_image);

    // The public preview gives a token, not a Docker name. It must contain
    // exactly the retired first generation and no current candidate.
    let preview = runtime
        .run(client.setup_gc_preview(&workspace, 0, 16))
        .expect("preview retired setup generation through daemon");
    assert_eq!(
        preview.candidates.len(),
        1,
        "only retired old app is eligible"
    );
    let candidate = &preview.candidates[0];
    assert_eq!(candidate.name, name_a);
    assert_eq!(
        candidate.generation,
        format!("sha256:{}", plan_a.content_sha256)
    );
    assert!(
        !candidate.token.is_empty(),
        "preview returns opaque apply token"
    );
    let applied = runtime
        .run(client.setup_gc_apply(&workspace, &candidate.token, true))
        .expect("apply exact preview candidate through daemon");
    assert!(applied.removed);
    assert!(!applied.reconciled_missing);
    assert!(
        inspect_container(&engine, &name_a)
            .expect("inspect removed old candidate")
            .is_none(),
        "GC apply must remove only the previewed retired container"
    );
    let current_after_gc = inspect_container(&engine, &name_b)
        .expect("inspect current generation after GC")
        .expect("GC must retain current generation");
    assert!(current_after_gc.running, "GC retains current running app");
    assert_eq!(current_after_gc.image, expected_image);
    assert_eq!(
        image_identity_for(&engine, PINNED_ALPINE),
        expected_image,
        "GC must not remove the shared/current image"
    );

    runtime.run(client.shutdown()).expect("shut down daemon");
    assert!(daemon.wait_for_exit().success(), "daemon failed");
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).expect("open registry");
    let resources = registry.resources(0, 16).expect("read resources").items;
    let resource = |name: &str| {
        resources
            .iter()
            .find(|value| value.kind == ResourceKind::Container && value.name == name)
            .expect("managed container registry row")
    };
    assert!(
        resources.iter().all(|value| value.name != name_a),
        "successful exact Docker removal reconciles the retired registry row"
    );
    assert_eq!(resource(&name_b).state, ResourceState::Active);
    assert_eq!(
        resources
            .iter()
            .find(|value| value.id == format!("setup-image:{expected_image}"))
            .expect("shared inspected image registry row")
            .state,
        ResourceState::Active
    );
    assert!(
        registry
            .events(0, 64)
            .expect("read GC event")
            .items
            .iter()
            .any(|event| event.kind == "setup.gc.removed"
                && event.detail == "retired_managed_setup_container"),
        "successful apply records a redacted durable GC event"
    );
    drop(registry);
    drop(cleanup_b);
    drop(cleanup_a);
    for name in [&name_a, &name_b] {
        assert!(
            inspect_container(&engine, name)
                .expect("inspect exact container after cleanup")
                .is_none(),
            "exact live-test container remained after cleanup"
        );
    }
}

/// Run with:
/// `cargo test -p bosn-service --test setup_ensure_docker -- --ignored --exact live_docker_setup_ensure_builds_and_reuses_inline_app`
///
/// This is the one-file setup acceptance path: the TOML carries its whole
/// Dockerfile and no separately-authored Dockerfile, script, or workspace
/// asset participates in the build. It requires a usable local Docker daemon
/// and the exact `PINNED_ALPINE` base image to have been pre-pulled.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_setup_ensure_builds_and_reuses_inline_app() {
    let engine = DockerEngine::docker();
    // Check the base before creating any Bosn state. The inline build is then
    // offline with respect to its only base-image requirement.
    let _base_image = pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let config_root = root.path().join("config");
    std::fs::create_dir_all(&workspace).expect("create empty workspace");
    std::fs::create_dir_all(&config_root).expect("create config directory");
    let config = config_root.join("setup.toml");
    let unique = test_unique_suffix();
    let dockerfile = format!(
        "FROM {PINNED_ALPINE}\nRUN printf '%s\\n' bosn-inline-{unique} > /bosn-inline-proof\nCMD [\"sh\", \"-c\", \"exec sleep 120\"]\n"
    );
    std::fs::write(
        &config,
        format!(
            "version = 1\n[app]\ndockerfile = '''{dockerfile}'''\ncommand = 'exec sleep 120 # bosn-inline-live-{unique}'\n"
        ),
    )
    .expect("write self-contained inline setup document");

    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("construct kernal-api runtime");
    // Planning materializes the Dockerfile into Bosn-owned state only. The
    // test records the exact root before the production daemon is involved.
    let plan = runtime
        .run(plan_setup(SetupPlanRequest {
            state_dir: state.clone(),
            workspace: workspace.clone(),
            locator: config.to_string_lossy().into_owned(),
            policy: SetupAcquirePolicy::OnlineRefresh,
        }))
        .expect("plan self-contained inline setup document");
    let asset_root = plan
        .asset_root
        .as_ref()
        .expect("inline setup plan has generated assets");
    assert_eq!(
        asset_root,
        &state.join("setup-assets").join(&plan.content_sha256),
        "inline assets are content-addressed under Bosn state"
    );
    assert_eq!(
        std::fs::read_to_string(asset_root.join("Dockerfile")).expect("read generated Dockerfile"),
        dockerfile,
        "generated Dockerfile comes entirely from the setup document"
    );
    assert!(
        std::fs::read_dir(&workspace)
            .expect("read untouched workspace before ensure")
            .next()
            .is_none(),
        "inline planning wrote into the selected workspace"
    );
    let image_tag = format!("bosn-setup:{}", plan.content_sha256);
    let container_name = format!("bosn-setup-{}", plan.content_sha256);
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect deterministic inline test container")
            .is_none(),
        "unique test container name already exists; refusing to touch it"
    );
    let cleanup = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: container_name.clone(),
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
        .expect("submit first production inline setup ensure job");
    wait_for_success(&runtime, &first_client, first_job);
    let expected_image = image_identity_for(&engine, &image_tag);
    let first = inspect_container(&engine, &container_name)
        .expect("inspect first inline setup app")
        .expect("first inline setup app exists");
    assert!(first.running, "first inline setup app is not running");
    assert_eq!(
        first.image, expected_image,
        "managed inline app image identity"
    );
    assert_eq!(first.managed, "v1", "managed ownership label");
    assert_eq!(
        first.content_sha256, plan.content_sha256,
        "inline content ownership label"
    );
    assert_eq!(
        first.container_name, container_name,
        "inline container-name ownership label"
    );

    runtime
        .run(first_client.shutdown())
        .expect("shut down first inline daemon");
    assert!(
        first_daemon.wait_for_exit().success(),
        "first inline daemon failed"
    );

    // A fresh daemon must inspect and reuse the already-built, matching
    // container. It must not replace it or author anything in the workspace.
    let mut second_daemon = DaemonChild::start(&state);
    let second_client = wait_for_client(&runtime, &mut second_daemon, &state);
    let second_job = runtime
        .run(second_client.submit_setup_ensure(request))
        .expect("submit second production inline setup ensure job");
    wait_for_success(&runtime, &second_client, second_job);
    let second = inspect_container(&engine, &container_name)
        .expect("inspect reused inline setup app")
        .expect("reused inline setup app exists");
    assert!(second.running, "reused inline setup app is not running");
    assert_eq!(
        second.id, first.id,
        "matching inline app was replaced instead of reused"
    );
    assert_eq!(
        second.image, expected_image,
        "reused inline app image identity"
    );
    assert_eq!(second.managed, "v1", "reused managed ownership label");
    assert_eq!(
        second.content_sha256, plan.content_sha256,
        "reused inline content label"
    );
    assert_eq!(
        second.container_name, container_name,
        "reused inline container-name ownership label"
    );

    runtime
        .run(second_client.shutdown())
        .expect("shut down second inline daemon");
    assert!(
        second_daemon.wait_for_exit().success(),
        "second inline daemon failed"
    );
    assert!(
        std::fs::read_dir(&workspace)
            .expect("read workspace after inline ensure")
            .next()
            .is_none(),
        "inline setup ensure wrote into the selected workspace"
    );
    drop(cleanup);
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect exact inline container after cleanup")
            .is_none(),
        "exact inline live-test container remained after cleanup"
    );
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test setup_ensure_docker --locked -- --ignored --exact live_docker_setup_ensure_fetches_one_https_document_then_reuses_it_offline`
///
/// The Bosn daemons use their normal kernal-api verified HTTPS transport. The
/// public fixture CA is passed only to those child processes. This test needs
/// a usable local Docker daemon and the exact `PINNED_ALPINE` image, while the
/// setup document itself exists only at one HTTPS URL.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_setup_ensure_fetches_one_https_document_then_reuses_it_offline() {
    let engine = DockerEngine::docker();
    let expected_image = pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("create empty workspace");
    let unique = test_unique_suffix();
    let original = format!(
        "version = 1\n[app]\nimage = '{PINNED_ALPINE}'\ncommand = 'exec sleep 120 # bosn-remote-live-{unique}'\n"
    );
    let changed = format!(
        "version = 1\n[app]\nimage = '{PINNED_ALPINE}'\ncommand = 'exec sleep 120 # bosn-remote-changed-{unique}'\n"
    );
    let content_sha256 = sha256_bytes(original.as_bytes()).to_hex();
    let container_name = format!("bosn-setup-{content_sha256}");
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect deterministic HTTPS test container")
            .is_none(),
        "unique HTTPS test container name already exists; refusing to touch it"
    );
    let cleanup = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: container_name.clone(),
        content_sha256: content_sha256.clone(),
    };
    let mut server = TlsSetupServer::start(original.as_bytes());
    let locator = server.url("/docker-linux.toml?token=bosn-live-test-secret");
    let certificate = certificate_path();
    let request = SetupEnsureJobRequest {
        workspace: workspace.clone(),
        config: locator,
        policy: SetupPreparePolicy::Refresh,
        deadline: JOB_DEADLINE,
        output_limit: OUTPUT_LIMIT,
    };
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("construct kernal-api runtime");

    let mut first_daemon = DaemonChild::start_with_certificate(&state, Some(&certificate));
    let first_client = wait_for_client(&runtime, &mut first_daemon, &state);
    let first_job = runtime
        .run(first_client.submit_setup_ensure(request.clone()))
        .expect("submit remote production setup ensure job");
    wait_for_success(&runtime, &first_client, first_job);
    assert_eq!(server.request_count(), 1, "remote document fetched once");
    let first = inspect_container(&engine, &container_name)
        .expect("inspect first remote setup app")
        .expect("first remote setup app exists");
    assert!(first.running, "first remote setup app is not running");
    assert_eq!(first.image, expected_image, "remote app image identity");
    assert_eq!(first.managed, "v1", "remote managed ownership label");
    assert_eq!(first.content_sha256, content_sha256, "remote content label");
    assert_eq!(first.container_name, container_name, "remote name label");
    runtime
        .run(first_client.shutdown())
        .expect("shut down first remote daemon");
    assert!(
        first_daemon.wait_for_exit().success(),
        "first remote daemon failed"
    );

    // Change the still-live endpoint, then take it away entirely. The second
    // explicit offline request has no authority to fetch or apply that change.
    server.replace_body(changed.as_bytes());
    server.stop();
    let mut offline_request = request;
    offline_request.policy = SetupPreparePolicy::Offline;
    let mut second_daemon = DaemonChild::start_with_certificate(&state, Some(&certificate));
    let second_client = wait_for_client(&runtime, &mut second_daemon, &state);
    let second_job = runtime
        .run(second_client.submit_setup_ensure(offline_request))
        .expect("submit cached remote setup ensure job");
    wait_for_success(&runtime, &second_client, second_job);
    assert_eq!(
        server.request_count(),
        1,
        "offline ensure contacted the changed or stopped remote source"
    );
    let second = inspect_container(&engine, &container_name)
        .expect("inspect cached remote setup app")
        .expect("cached remote setup app exists");
    assert!(second.running, "cached remote setup app is not running");
    assert_eq!(
        second.id, first.id,
        "offline remote ensure replaced rather than reused the cached app"
    );
    assert_eq!(second.image, expected_image, "cached remote image identity");
    runtime
        .run(second_client.shutdown())
        .expect("shut down cached remote daemon");
    assert!(
        second_daemon.wait_for_exit().success(),
        "cached remote daemon failed"
    );
    assert!(
        std::fs::read_dir(&workspace)
            .expect("read remote workspace")
            .next()
            .is_none(),
        "remote setup ensure wrote into the selected workspace"
    );
    drop(cleanup);
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect exact remote container after cleanup")
            .is_none(),
        "exact remote live-test container remained after cleanup"
    );
}
