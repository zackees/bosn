use super::*;
use kernal_api::async_engine::RuntimeBuilder;

/// Run the production secret path against a fake `docker` (a shell that
/// reports its own ps-visible argv and environment, then echoes the token
/// whole and split across two writes). Returns (daemon log lines, stdout).
#[cfg(unix)]
fn run_fake_docker_with_secrets(state: &Path, declared: &[String]) -> (Vec<String>, String) {
    const SCRIPT: &str = r#"printf 'argv:'; tr '\0' ' ' < /proc/$$/cmdline; echo
printf 'env=%s\n' "${GITHUB_TOKEN-unset}"
if [ -n "${GITHUB_TOKEN-}" ]; then
  printf 'whole %s end\n' "$GITHUB_TOKEN"
  printf '%s' "$(printf %s "$GITHUB_TOKEN" | cut -c1-9)" >&2
  sleep 0.2
  printf '%s tail\n' "$(printf %s "$GITHUB_TOKEN" | cut -c10-)" >&2
fi
"#;
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.run(async {
        let secrets = load_manifest_task_secrets(state, declared).unwrap();
        let base = DockerEngine::synthetic_for_test("/bin/sh", ["-c", SCRIPT, "fake-docker"]);
        let (engine, passthrough_env) = secrets.docker_engine(&base);
        let mut masker = SecretMasker::new(secrets.values.iter().map(|(_, value)| value));
        let (text_logs, mut log_receiver) = async_engine::channel::<String>(1024);
        let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
        let (events, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
        let forwarder = async_engine::launch(async move {
            while let Some(event) = receiver.recv().await {
                let event = mask_engine_event(&mut masker, event);
                forward_engine_event(&logs, event).await?;
            }
            for event in [
                EngineEvent::Stdout(masker.finish(MaskStream::Stdout)),
                EngineEvent::Stderr(masker.finish(MaskStream::Stderr)),
            ] {
                forward_engine_event(&logs, event).await?;
            }
            Ok::<(), String>(())
        });
        let cancellation = async_engine::CancellationSource::new();
        let result = bosn_setup::SetupAppTaskEngine::stream(
            &engine,
            bosn_setup::SetupAppTaskCommand::Exec {
                container_name: "bosn-setup-test".into(),
                task_token: "0123456789abcdef0123456789abcdef".into(),
                passthrough_env,
                command: "true".into(),
            },
            RunOptions::streaming(Duration::from_secs(10), 64 * 1024),
            &cancellation.token(),
            &events,
        )
        .await
        .unwrap();
        drop(events);
        forwarder.await.unwrap().unwrap();
        let mut lines = Vec::new();
        while let Some(line) = log_receiver.recv().await {
            lines.push(line);
        }
        (lines, String::from_utf8_lossy(&result.stdout).into_owned())
    })
}

#[cfg(unix)]
#[test]
fn declared_github_token_reaches_the_task_env_but_never_argv_or_logs() {
    const CANARY: &str = "ghp_CANARY308abcdefghijklmnop0123456789";
    let state = tempfile::tempdir().unwrap();
    secrets::write_secret(state.path(), "github_token", CANARY.as_bytes()).unwrap();
    let (lines, raw_stdout) =
        run_fake_docker_with_secrets(state.path(), &["github_token".to_owned()]);
    let joined = lines.join("\n");
    // The docker client's ps-visible argv forwards the name only.
    assert!(
        raw_stdout.contains("--env GITHUB_TOKEN bosn-setup-test"),
        "{raw_stdout}"
    );
    let argv_line = raw_stdout
        .lines()
        .find(|line| line.starts_with("argv:"))
        .unwrap();
    assert!(!argv_line.contains(CANARY), "{argv_line}");
    // The value did reach the task (proved by the raw, unmasked capture)...
    assert!(raw_stdout.contains(&format!("env={CANARY}")));
    // ...but every relayed daemon log line is masked, whole and split.
    assert!(!joined.contains(CANARY), "{joined}");
    assert!(
        !joined.contains(&CANARY[..9]) || !joined.contains(&CANARY[9..]),
        "{joined}"
    );
    assert!(joined.contains("env=***"), "{joined}");
    assert!(joined.contains("whole *** end"), "{joined}");
    assert!(joined.contains("*** tail"), "{joined}");
}

#[cfg(unix)]
#[test]
fn undeclared_task_gets_no_github_token_even_when_the_secret_exists() {
    const CANARY: &str = "ghp_CANARY308undeclared0123456789";
    let state = tempfile::tempdir().unwrap();
    secrets::write_secret(state.path(), "github_token", CANARY.as_bytes()).unwrap();
    // Ambient daemon env must not leak into the task either.
    let (lines, raw_stdout) = run_fake_docker_with_secrets(state.path(), &[]);
    assert!(raw_stdout.contains("env=unset"), "{raw_stdout}");
    // The only forwarded variable is the per-exec stop marker (#357).
    assert_eq!(raw_stdout.matches("--env").count(), 1, "{raw_stdout}");
    assert!(
        raw_stdout.contains("--env BOSN_TASK_TOKEN="),
        "{raw_stdout}"
    );
    assert!(!lines.join("\n").contains(CANARY));
}

#[cfg(unix)]
#[test]
fn refused_or_missing_secret_is_reported_without_the_value() {
    use std::os::unix::fs::PermissionsExt;
    const CANARY: &str = "ghp_CANARY308refused0123456789";
    let state = tempfile::tempdir().unwrap();
    let declared = ["github_token".to_owned()];
    let missing = load_manifest_task_secrets(state.path(), &declared).unwrap();
    assert!(missing.values.is_empty());
    assert_eq!(missing.missing, declared);
    let warning =
        manifest_task_github_preflight(&declared, &missing.missing, "true", false).unwrap();
    assert!(warning.contains("60/hour") && warning.contains("bosn secret set github_token"));
    secrets::write_secret(state.path(), "github_token", CANARY.as_bytes()).unwrap();
    std::fs::set_permissions(
        secrets::secrets_dir(state.path()).join("github_token"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let Err(error) = load_manifest_task_secrets(state.path(), &declared) else {
        panic!("loose secret was accepted");
    };
    assert!(!error.contains(CANARY));
}

#[test]
fn act_tasks_without_a_declared_token_get_a_quota_warning() {
    assert!(manifest_task_github_preflight(&[], &[], "sh ci/act_ci.sh test", false).is_some());
    assert!(manifest_task_github_preflight(&[], &[], "sh ci/act_ci.sh test", true).is_none());
    assert!(manifest_task_github_preflight(&[], &[], "cargo test --workspace", false).is_none());
    assert!(
        manifest_task_github_preflight(&["github_token".into()], &[], "act -j lint", false)
            .is_none()
    );
}
use std::{
    collections::{BTreeMap, VecDeque},
    future::{Ready, ready},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
pub(crate) mod act_live_probe;
mod daemon;
mod daemon_jobs;
mod guest;
mod manifest_ensure;
mod manifest_plan;
mod python_v4;
mod reconcile_wire;
mod setup_ensure;
mod setup_rollover;

fn setup_ensure_execution(
    workspace: &str,
    generation: &str,
    image_identity: &str,
) -> SetupEnsureExecution {
    SetupEnsureExecution {
        receipt: format!("ensured {generation}"),
        resource: SetupEnsureResource {
            id: format!("setup-container:{generation}"),
            name: format!("bosn-setup-{generation}"),
            stack: "setup".into(),
            generation: format!("sha256:{generation}"),
            workspace: workspace.into(),
        },
        image: SetupEnsureImageResource {
            id: format!("setup-image:{image_identity}"),
            name: format!("setup-image:{image_identity}"),
            stack: "setup".into(),
            generation: image_identity.into(),
            workspace: workspace.into(),
        },
        volumes: Vec::new(),
        manifest_autostart: false,
    }
}

fn manifest_ensure_execution(
    workspace: &str,
    stack: &str,
    generation: &str,
    image_identity: &str,
) -> SetupEnsureExecution {
    SetupEnsureExecution {
        receipt: format!("ensured manifest {stack} {generation}"),
        resource: SetupEnsureResource {
            id: format!("manifest-container:{stack}:{generation}"),
            name: format!("bosn-setup-{generation}"),
            stack: stack.into(),
            generation: format!("sha256:{generation}"),
            workspace: workspace.into(),
        },
        image: SetupEnsureImageResource {
            id: format!("manifest-image:{image_identity}"),
            name: format!("manifest-image:{image_identity}"),
            stack: stack.into(),
            generation: image_identity.into(),
            workspace: workspace.into(),
        },
        volumes: Vec::new(),
        manifest_autostart: false,
    }
}

fn test_manifest_recovery_contract(execution: &SetupEnsureExecution) -> ManifestRecoveryContract {
    ManifestRecoveryContract {
        resource_id: execution.resource.id.clone(),
        name: execution.resource.name.clone(),
        workspace: execution.resource.workspace.clone(),
        stack: execution.resource.stack.clone(),
        generation: execution.resource.generation.clone(),
        manifest: "bosn.toml".into(),
        image_identity: execution.image.generation.clone(),
        guest: false,
        intent_id: "job-1".into(),
        autostart: true,
    }
}

fn manifest_volume_resource(workspace: &str, stack: &str) -> ManifestVolumeResource {
    let name = "bosn-v-stack-aaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
    ManifestVolumeResource {
        id: format!("manifest-volume:{name}"),
        name: name.clone(),
        stack: stack.into(),
        generation: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .into(),
        scope: Scope::Stack,
        workspace: workspace.into(),
        retention: Retention::Pinned,
        target: "/var/lib/app".into(),
        labels: BTreeMap::from([
            ("com.zackees.bosn.setup-managed".into(), "v1".into()),
            (
                "com.zackees.bosn.setup-content-sha256".into(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            ),
            ("com.zackees.bosn.setup-container".into(), name),
        ]),
    }
}

#[test]
fn setup_gc_token_is_exact_and_rejects_tampering() {
    let candidate = bosn_registry::SetupGcCandidate {
        id: "setup-container:abc".into(),
        name: "bosn-setup-abc".into(),
        generation: "sha256:abc".into(),
    };
    let token = setup_gc_token(&candidate);
    assert_eq!(
        parse_setup_gc_token(&token).unwrap(),
        (candidate.id, candidate.name, candidate.generation)
    );
    assert!(parse_setup_gc_token(&(token + "00")).is_err());
    assert!(validate_setup_gc_apply_input("/work", "sgc1-00", false).is_err());
}

#[test]
fn manifest_volume_gc_token_is_exact_and_cannot_be_used_as_setup_token() {
    let candidate = bosn_registry::ManifestVolumeGcCandidate {
        id: "manifest-volume:abc".into(),
        name: "bosn-v-spec-abc".into(),
        generation: "sha256:abc".into(),
    };
    let token = manifest_volume_gc_token(&candidate);
    assert_eq!(
        parse_manifest_volume_gc_token(&token).unwrap(),
        (candidate.id, candidate.name, candidate.generation)
    );
    assert!(parse_manifest_volume_gc_token(&(token.clone() + "00")).is_err());
    assert!(parse_setup_gc_token(&token).is_err());
    assert!(validate_manifest_volume_gc_apply_input("/work", &token, false).is_err());
}

#[test]
fn manifest_volume_release_token_is_exact_and_separate_from_gc() {
    let candidate = bosn_registry::ManifestVolumeGcCandidate {
        id: "manifest-volume:durable".into(),
        name: "bosn-v-machine-durable".into(),
        generation: "sha256:durable".into(),
    };
    let token = manifest_volume_release_token(&candidate);
    assert_eq!(
        parse_manifest_volume_release_token(&token).unwrap(),
        (candidate.id, candidate.name, candidate.generation)
    );
    assert!(parse_manifest_volume_release_token(&(token.clone() + "00")).is_err());
    assert!(parse_manifest_volume_gc_token(&token).is_err());
    assert!(validate_manifest_volume_release_apply_input("/work", &token, false).is_err());
}

struct SlowFakeSetupExecutor {
    started: AtomicUsize,
    cancelled: AtomicUsize,
}
impl SlowFakeSetupExecutor {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
        }
    }
}
impl SetupPrepareExecutor for SlowFakeSetupExecutor {
    fn execute<'a>(
        &'a self,
        _request: SetupPrepareRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            logs.send("[fake] preparation started".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            for _ in 0..100 {
                if cancellation.is_cancelled() {
                    self.cancelled.fetch_add(1, Ordering::SeqCst);
                    return Err("fake observed cancellation".into());
                }
                async_engine::sleep(Duration::from_millis(10)).await;
            }
            Ok("fake prepared sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
        })
    }
}

struct FakeDoctorExecutor {
    report: DockerDoctorReport,
    calls: AtomicUsize,
}
impl FakeDoctorExecutor {
    fn ready() -> Self {
        Self {
            report: DockerDoctorReport {
                state: DockerDoctorState::Ready,
                client_version: Some("29.0.1".into()),
                server_version: Some("29.0.1".into()),
            },
            calls: AtomicUsize::new(0),
        }
    }
}
impl DoctorExecutor for FakeDoctorExecutor {
    fn doctor<'a>(&'a self) -> Pin<Box<dyn Future<Output = DockerDoctorReport> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(ready(self.report.clone()))
    }
}

struct SlowDoctorExecutor;
impl DoctorExecutor for SlowDoctorExecutor {
    fn doctor<'a>(&'a self) -> Pin<Box<dyn Future<Output = DockerDoctorReport> + Send + 'a>> {
        Box::pin(async move {
            async_engine::sleep(Duration::from_secs(10)).await;
            DockerDoctorReport {
                state: DockerDoctorState::Ready,
                client_version: Some("never".into()),
                server_version: Some("never".into()),
            }
        })
    }
}

struct FakeSetupTaskExecutor {
    started: AtomicUsize,
    cancelled: AtomicUsize,
    stages: Mutex<Vec<String>>,
}
impl FakeSetupTaskExecutor {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
            stages: Mutex::new(Vec::new()),
        }
    }
    fn stages(&self) -> Vec<String> {
        self.stages.lock().unwrap().clone()
    }
}
impl SetupTaskExecutor for FakeSetupTaskExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.stages
                .lock()
                .unwrap()
                .push(format!("plan:{}", request.task_name));
            logs.send("[fake] plan receipt accepted".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            self.stages
                .lock()
                .unwrap()
                .push(format!("prepare:{}", request.task_name));
            logs.send("[fake] image prepared".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            if request.task_name == "prepare-fail" {
                return Err("fake preparation failed".into());
            }
            if request.task_name == "wait" {
                for _ in 0..100 {
                    if cancellation.is_cancelled() {
                        self.cancelled.fetch_add(1, Ordering::SeqCst);
                        return Err("fake task observed cancellation".into());
                    }
                    async_engine::sleep(Duration::from_millis(10)).await;
                }
            }
            if cancellation.is_cancelled() {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                return Err("fake task observed cancellation".into());
            }
            self.stages
                .lock()
                .unwrap()
                .push(format!("task:{}", request.task_name));
            logs.send(format!("[fake] task output {}", "x".repeat(4 * 1024)))
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            Ok(format!("fake task {} complete", request.task_name))
        })
    }
}

/// Holds a fake executor inside its run until the test releases it, so the
/// test can act while the job is provably in flight (#299). Coalescing joins
/// only active jobs, so a coalescing assertion must not race the executor.
struct ExecutionGate {
    entered: async_engine::Sender<()>,
    release: Mutex<Option<async_engine::Receiver<()>>>,
}
/// The test's side of an [`ExecutionGate`].
struct GateControl {
    entered: async_engine::Receiver<()>,
    release: async_engine::Sender<()>,
}
fn execution_gate() -> (ExecutionGate, GateControl) {
    let (entered, entered_wait) = async_engine::channel(1);
    let (release, release_wait) = async_engine::channel(1);
    (
        ExecutionGate {
            entered,
            release: Mutex::new(Some(release_wait)),
        },
        GateControl {
            entered: entered_wait,
            release,
        },
    )
}
impl ExecutionGate {
    /// Report entry, then wait for the release. A second run fails: a gated
    /// fake executes once.
    async fn hold(&self) -> Result<(), String> {
        self.entered
            .send(())
            .await
            .map_err(|_| "test entry observer closed".to_owned())?;
        let mut release = self
            .release
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| "test task executed twice".to_owned())?;
        release
            .recv()
            .await
            .ok_or_else(|| "test release closed".to_owned())
    }
}
impl GateControl {
    async fn entered(&mut self) {
        async_engine::timeout(Duration::from_secs(10), self.entered.recv())
            .await
            .expect("gated executor did not start")
            .expect("gated executor dropped its gate");
    }
    async fn release(&self) {
        self.release.send(()).await.unwrap();
    }
}

struct FakeSetupAppTaskExecutor {
    started: AtomicUsize,
    observed: Mutex<Vec<String>>,
    gate: Option<ExecutionGate>,
}
impl FakeSetupAppTaskExecutor {
    fn new(gate: Option<ExecutionGate>) -> Self {
        Self {
            started: AtomicUsize::new(0),
            observed: Mutex::new(Vec::new()),
            gate,
        }
    }
}
impl SetupAppTaskExecutor for FakeSetupAppTaskExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupAppTaskJobRequest,
        _cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        session: &'a dyn SetupAppTaskSessionRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.observed
                .lock()
                .unwrap()
                .push(request.task_name.clone());
            session.begin("owned-container-id".into()).await?;
            if let Some(gate) = &self.gate {
                gate.hold().await?;
            }
            logs.send("[fake] declared app task executed".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            session.finish("succeeded").await?;
            Ok("fake app task complete".into())
        })
    }
}

struct FakeManifestAppTaskExecutor {
    observed: Mutex<Vec<ManifestAppTaskJobRequest>>,
    gate: ExecutionGate,
}
impl FakeManifestAppTaskExecutor {
    fn new(gate: ExecutionGate) -> Self {
        Self {
            observed: Mutex::new(Vec::new()),
            gate,
        }
    }
}
impl ManifestAppTaskExecutor for FakeManifestAppTaskExecutor {
    fn execute<'a>(
        &'a self,
        request: ManifestAppTaskJobRequest,
        _cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        session: &'a dyn ManifestAppTaskSessionRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.observed.lock().unwrap().push(request.clone());
            session.begin("bosn-setup-manifest-identity".into()).await?;
            self.gate.hold().await?;
            logs.send("[fake] manifest declared task executed".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            session.finish("succeeded").await?;
            Ok("fake manifest app task complete".into())
        })
    }
}

struct FakeSetupEnsureExecutor {
    started: AtomicUsize,
    cancelled: AtomicUsize,
    stages: Mutex<Vec<String>>,
}
impl FakeSetupEnsureExecutor {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
            stages: Mutex::new(Vec::new()),
        }
    }
    fn stages(&self) -> Vec<String> {
        self.stages.lock().unwrap().clone()
    }
}
impl SetupEnsureExecutor for FakeSetupEnsureExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupEnsureJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.stages.lock().unwrap().push("plan".into());
            logs.send("[fake] plan receipt accepted".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            self.stages.lock().unwrap().push("prepare".into());
            logs.send("[fake] image prepared".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            if request.config.contains("prepare-fail") {
                return Err("fake preparation failed".into());
            }
            self.stages.lock().unwrap().push("ensure".into());
            if request.config.contains("ensure-mismatch") {
                return Err("fake ownership mismatch".into());
            }
            if request.config.contains("wait") {
                // Stay blocked until cancellation: callers prove submission is
                // asynchronous without racing a wall-clock fake completion.
                loop {
                    if cancellation.is_cancelled() {
                        self.cancelled.fetch_add(1, Ordering::SeqCst);
                        return Err("fake ensure observed cancellation".into());
                    }
                    async_engine::sleep(Duration::from_millis(10)).await;
                }
            }
            if cancellation.is_cancelled() {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                return Err("fake ensure observed cancellation".into());
            }
            self.stages.lock().unwrap().push("mutate".into());
            logs.send(format!("[fake] ensured {}", "x".repeat(4 * 1024)))
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            Ok(SetupEnsureExecution {
                receipt: "fake ensured container".into(),
                resource: SetupEnsureResource {
                    id: "setup-container:fake".into(),
                    name: "bosn-setup-fake".into(),
                    stack: "setup".into(),
                    generation: "sha256:fake".into(),
                    workspace: request.workspace.to_string_lossy().into_owned(),
                },
                image: SetupEnsureImageResource {
                    id: "setup-image:sha256:fake".into(),
                    name: "setup-image:sha256:fake".into(),
                    stack: "setup".into(),
                    generation: "sha256:fake".into(),
                    workspace: request.workspace.to_string_lossy().into_owned(),
                },
                volumes: Vec::new(),
                manifest_autostart: false,
            })
        })
    }
}

#[derive(Default)]
struct FakeManifestEnsureExecutor {
    calls: Mutex<Vec<ManifestEnsureJobRequest>>,
    fail_stack: Mutex<Option<String>>,
}
impl ManifestEnsureExecutor for FakeManifestEnsureExecutor {
    fn execute<'a>(
        &'a self,
        request: ManifestEnsureJobRequest,
        _cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        _registry: &'a RegistryActor,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        Box::pin(async move {
            logs.send("[fake] manifest stack ensured".into())
                .await
                .map_err(|_| "fake manifest log consumer closed".to_owned())?;
            self.calls.lock().unwrap().push(request.clone());
            if self
                .fail_stack
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|stack| stack == request.stack)
            {
                return Err("injected manifest stack failure".into());
            }
            let workspace = request.workspace.to_string_lossy().into_owned();
            let generation = request
                .manifest
                .strip_suffix(".toml")
                .unwrap_or(&request.manifest)
                .to_owned();
            Ok(SetupEnsureExecution {
                receipt: "fake manifest ensured".into(),
                resource: SetupEnsureResource {
                    id: format!("manifest-container:{}:{generation}", request.stack),
                    name: format!("bosn-setup-{}-{generation}", request.stack),
                    stack: request.stack.clone(),
                    generation: format!("sha256:{generation}"),
                    workspace: workspace.clone(),
                },
                image: SetupEnsureImageResource {
                    id: format!("manifest-image:{}:sha256:{generation}", request.stack),
                    name: format!("manifest-image:{}:sha256:{generation}", request.stack),
                    stack: request.stack,
                    generation: format!("sha256:{generation}"),
                    workspace,
                },
                volumes: Vec::new(),
                manifest_autostart: false,
            })
        })
    }
}

struct PipelineFakeEngine {
    image_calls: Mutex<Vec<(bosn_setup::SetupImageCommand, RunOptions)>>,
    ensure_calls: Mutex<Vec<(bosn_setup::SetupEnsureCommand, RunOptions)>>,
    image_results: Mutex<VecDeque<Result<bosn_engine::CommandResult, bosn_engine::CommandError>>>,
    ensure_results:
        Mutex<VecDeque<Result<bosn_setup::SetupEnsureResponse, bosn_engine::CommandError>>>,
}
impl PipelineFakeEngine {
    fn new(
        image_results: impl IntoIterator<
            Item = Result<bosn_engine::CommandResult, bosn_engine::CommandError>,
        >,
        ensure_results: impl IntoIterator<
            Item = Result<bosn_setup::SetupEnsureResponse, bosn_engine::CommandError>,
        >,
    ) -> Self {
        Self {
            image_calls: Mutex::new(Vec::new()),
            ensure_calls: Mutex::new(Vec::new()),
            image_results: Mutex::new(image_results.into_iter().collect()),
            ensure_results: Mutex::new(ensure_results.into_iter().collect()),
        }
    }
}
impl SetupImageEngine for PipelineFakeEngine {
    type StreamFuture<'a> = Ready<Result<bosn_engine::CommandResult, bosn_engine::CommandError>>;
    fn stream<'a>(
        &'a self,
        command: bosn_setup::SetupImageCommand,
        options: RunOptions,
        _cancellation: &'a async_engine::CancellationToken,
        _events: &'a async_engine::Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a> {
        self.image_calls.lock().unwrap().push((command, options));
        ready(self.image_results.lock().unwrap().pop_front().unwrap())
    }
}
impl SetupEnsureEngine for PipelineFakeEngine {
    type StreamFuture<'a> =
        Ready<Result<bosn_setup::SetupEnsureResponse, bosn_engine::CommandError>>;
    fn stream<'a>(
        &'a self,
        command: bosn_setup::SetupEnsureCommand,
        options: RunOptions,
        _cancellation: &'a async_engine::CancellationToken,
        _events: &'a async_engine::Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a> {
        self.ensure_calls.lock().unwrap().push((command, options));
        ready(self.ensure_results.lock().unwrap().pop_front().unwrap())
    }
}

const TEST_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TEST_IDENTITY: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TEST_CONTAINER_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
fn pipeline_plan(workspace: &Path) -> SetupPlan {
    let image = format!("registry.example/team/app@sha256:{TEST_HASH}");
    SetupPlan {
        source_kind: bosn_setup::SetupSourceKind::LocalFile,
        content_sha256: TEST_HASH.into(),
        schema_version: 1,
        workspace_root: kernal_api::platform::fs::canonical_context_path(workspace).unwrap(),
        asset_root: None,
        task_names: Vec::new(),
        app: bosn_core::SetupApp {
            source: bosn_core::SetupSource::PinnedImage(image.clone()),
            environment: BTreeMap::new(),
            workdir: None,
            command: None,
            mounts: Vec::new(),
        },
        tasks: BTreeMap::new(),
        app_source: bosn_setup::SetupPlanAppSource::PinnedImage { image },
        named_volumes: Vec::new(),
        tmpfs: Vec::new(),
        host_docker_socket: None,
        macos_guest: None,
    }
}
fn command_result(exit_code: i32, stdout: impl Into<Vec<u8>>) -> bosn_engine::CommandResult {
    bosn_engine::CommandResult {
        exit_code,
        stdout: stdout.into(),
        stderr: Vec::new(),
    }
}

struct FakeGuestSshTransport {
    calls: Mutex<Vec<GuestSshCommand>>,
    scp_calls: Mutex<Vec<GuestScpCommand>>,
    sequence: Mutex<Vec<String>>,
    results: Mutex<VecDeque<Result<CommandResult, CommandError>>>,
}
impl FakeGuestSshTransport {
    fn new(results: impl IntoIterator<Item = Result<CommandResult, CommandError>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            scp_calls: Mutex::new(Vec::new()),
            sequence: Mutex::new(Vec::new()),
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}
impl GuestSshTaskTransport for FakeGuestSshTransport {
    fn stream<'a>(
        &'a self,
        command: GuestSshCommand,
        _options: RunOptions,
        _cancellation: &'a async_engine::CancellationToken,
        _events: &'a async_engine::Sender<EngineEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>> {
        self.sequence
            .lock()
            .unwrap()
            .push(format!("ssh:{}", command.command));
        self.calls.lock().unwrap().push(command);
        Box::pin(ready(self.results.lock().unwrap().pop_front().unwrap()))
    }
    fn stream_scp<'a>(
        &'a self,
        command: GuestScpCommand,
        _options: RunOptions,
        _cancellation: &'a async_engine::CancellationToken,
        _events: &'a async_engine::Sender<EngineEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>> {
        self.sequence
            .lock()
            .unwrap()
            .push(format!("scp:{}", command.destination));
        self.scp_calls.lock().unwrap().push(command);
        Box::pin(ready(self.results.lock().unwrap().pop_front().unwrap()))
    }
}

#[derive(Default)]
struct FakeManifestGuestSession {
    events: Mutex<Vec<String>>,
}
impl ManifestAppTaskSessionRecorder for FakeManifestGuestSession {
    fn begin<'a>(
        &'a self,
        identity: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        self.events
            .lock()
            .unwrap()
            .push(format!("begin:{identity}"));
        Box::pin(ready(Ok(())))
    }
    fn finish<'a>(
        &'a self,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        self.events
            .lock()
            .unwrap()
            .push(format!("finish:{outcome}"));
        Box::pin(ready(Ok(())))
    }
}

async fn send_raw(state: &Path, bytes: Vec<u8>) {
    let mut stream = AsyncStream::connect(&endpoint(state).unwrap())
        .await
        .unwrap();
    // A daemon can reject a malformed or oversized frame before accepting
    // the whole write. Either outcome is expected; the caller proves the
    // healthy peer remains available afterwards.
    let _ = stream.write_all(&bytes).await;
}

async fn stopped(server: async_engine::Task<Result<(), Error>>) {
    async_engine::timeout(Duration::from_secs(5), server)
        .await
        .expect("service did not stop in time")
        .expect("service task failed")
        .expect("service returned error");
}

async fn wait_for_client(state: &Path) -> Client {
    let client = Client::for_state(state).unwrap();
    // Keep the fast 20 ms poll, but give a saturated CI runner time to
    // start the daemon. The old 30 polls allowed only 600 ms and made
    // unrelated service tests fail before their assertions ran.
    let started = std::time::Instant::now();
    loop {
        let error = match client.ping().await {
            Ok(_) => return client,
            Err(error) => error,
        };
        if started.elapsed() >= Duration::from_secs(5) {
            panic!("daemon did not become ready within 5s; last ping error: {error}");
        }
        async_engine::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for(predicate: impl Fn() -> bool) {
    for _ in 0..100 {
        if predicate() {
            return;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition did not become true");
}

async fn wait_for_job_state(client: &Client, id: u64, wanted: &str) {
    for _ in 0..100 {
        if client.job_status(id).await.unwrap().state == wanted {
            return;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} did not reach {wanted}");
}

async fn wait_for_logs(client: &Client, id: u64) -> JobLogPage {
    for _ in 0..100 {
        let page = client.job_logs(id, 0, 16).await.unwrap();
        if !page.records.is_empty() {
            return page;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} did not produce a log record");
}

mod manifest_recovery;
use manifest_recovery::*;
