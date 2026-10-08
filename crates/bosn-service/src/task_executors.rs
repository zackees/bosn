//! Docker executors for declared setup tasks and setup-app tasks, with secret masking.

use super::*;

/// Docker-backed implementation for a single setup task.  The daemon plans,
/// prepares, and then invokes the task primitive itself; no front end can
/// insert an arbitrary container operation between those stages.
#[derive(Clone)]
pub struct DockerSetupTaskExecutor {
    pub(crate) state_dir: PathBuf,
    pub(crate) engine: DockerEngine,
}
impl DockerSetupTaskExecutor {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            engine: DockerEngine::docker(),
        }
    }
}
impl SetupTaskExecutor for DockerSetupTaskExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.execute_recorded(request, cancellation, logs, &ReceiptOnlyImageRecorder)
                .await
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "sequential preparation and task pipeline"
    )]
    fn execute_recorded<'a>(
        &'a self,
        request: SetupTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        images: &'a dyn SetupImageRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            // Both engine stages are sequential. Partitioning this one input
            // budget prevents an image pull/build plus task from ever emitting
            // more than the caller allowed, even if either stage is noisy.
            // A one-byte request is syntactically valid but cannot safely fund
            // both required bounded stages, so reject it before planning or
            // Docker work starts.
            let prepare_output = request.output_limit / 2;
            let task_output = request.output_limit.saturating_sub(prepare_output);
            if prepare_output == 0 || task_output == 0 {
                return Err("setup task output budget cannot fund preparation and task".into());
            }
            let deadline = async_engine::Deadline::after(request.deadline);
            let plan = async_engine::cancellable(
                cancellation,
                async_engine::timeout_at(
                    deadline,
                    plan_setup(SetupPlanRequest {
                        state_dir: self.state_dir.clone(),
                        workspace: request.workspace.clone(),
                        locator: request.config,
                        policy: request.policy.acquire_policy(),
                    }),
                ),
            )
            .await
            .map_err(|_| "setup task cancelled".to_owned())?
            .map_err(|_| "setup task planning exceeded its deadline".to_owned())?
            .map_err(|error| error.to_string())?;
            if cancellation.is_cancelled() {
                return Err("setup task cancelled".into());
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err("setup task exceeded its deadline".into());
            }

            let (events, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
            let forwarded_logs = logs.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(event) = receiver.recv().await {
                    forward_engine_event(&forwarded_logs, event).await?;
                }
                Ok::<(), String>(())
            });

            logs.send("[setup] preparing application image".into())
                .await
                .map_err(|_| "setup log consumer closed".to_owned())?;
            let preparation_intent = PreparedImageOwner::Setup.preparation_intent(&plan)?;
            images
                .record_preparation(preparation_intent.clone())
                .await?;
            let prepared = prepare_setup_image(
                &bosn_setup::ImagePreparationEngine::new(
                    &self.engine,
                    &preparation_intent
                        .ownership_proof()
                        .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?,
                &plan,
                RunOptions::streaming(deadline.remaining(), prepare_output),
                cancellation,
                &events,
            )
            .await;
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    drop(events);
                    forwarder
                        .await
                        .map_err(|_| "setup log forwarder stopped".to_owned())??;
                    return Err(error.to_string());
                }
            };
            let image =
                setup_ensure_image_resource(&prepared, &plan.workspace_root.to_string_lossy());
            images.record(image.clone()).await?;
            images.complete_preparation(preparation_intent).await?;
            if cancellation.is_cancelled() {
                drop(events);
                forwarder
                    .await
                    .map_err(|_| "setup log forwarder stopped".to_owned())??;
                return Err("setup task cancelled".into());
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                drop(events);
                forwarder
                    .await
                    .map_err(|_| "setup log forwarder stopped".to_owned())??;
                return Err("setup task exceeded its deadline".into());
            }
            logs.send(format!(
                "[setup] running declared task {}",
                request.task_name
            ))
            .await
            .map_err(|_| "setup log consumer closed".to_owned())?;
            let result = execute_setup_task(
                &self.engine,
                SetupTaskRequest {
                    plan: &plan,
                    workspace_root: request.workspace,
                    task_name: request.task_name,
                    prepared_image: &prepared,
                    options: RunOptions::streaming(remaining, task_output),
                    cancellation,
                    events: &events,
                },
            )
            .await;
            images.record(image).await?;
            drop(events);
            forwarder
                .await
                .map_err(|_| "setup log forwarder stopped".to_owned())??;
            let result = result.map_err(|error| error.to_string())?;
            Ok(format!(
                "completed declared task {} with image {}",
                result.task_name, result.image_identity
            ))
        })
    }
}

/// Docker-backed implementation for one declared task in the persistent setup
/// app. The app is never created, started, stopped, or replaced here: a fresh
/// ownership inspection must prove the exact content-addressed app already
/// exists before the fixed `docker container exec NAME sh -lc DECLARED` call.
#[derive(Clone)]
pub struct DockerSetupAppTaskExecutor {
    pub(crate) state_dir: PathBuf,
    pub(crate) engine: DockerEngine,
}
impl DockerSetupAppTaskExecutor {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            engine: DockerEngine::docker(),
        }
    }
}
impl SetupAppTaskExecutor for DockerSetupAppTaskExecutor {
    #[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
    fn execute<'a>(
        &'a self,
        request: SetupAppTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        session: &'a dyn SetupAppTaskSessionRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            // Plan/prepare/ownership-inspect/exec share one caller budget.
            // Four positive partitions make it impossible for a noisy early
            // stage to leave the final exec with an unbounded output channel.
            let quarter = request.output_limit / 4;
            let exec_output = request.output_limit.saturating_sub(quarter * 3);
            if quarter == 0 || exec_output == 0 {
                return Err(
                    "setup app task output budget cannot fund validation and execution".into(),
                );
            }
            let deadline = async_engine::Deadline::after(request.deadline);
            let plan = async_engine::cancellable(
                cancellation,
                async_engine::timeout_at(
                    deadline,
                    plan_setup(SetupPlanRequest {
                        state_dir: self.state_dir.clone(),
                        workspace: request.workspace.clone(),
                        locator: request.config,
                        policy: request.policy.acquire_policy(),
                    }),
                ),
            )
            .await
            .map_err(|_| "setup app task cancelled before ownership inspection".to_owned())?
            .map_err(|_| "setup app task planning exceeded its deadline".to_owned())?
            .map_err(|error| error.to_string())?;
            if cancellation.is_cancelled() {
                return Err("setup app task cancelled before ownership inspection".into());
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err("setup app task exceeded its deadline".into());
            }
            let (events, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
            let forwarded_logs = logs.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(event) = receiver.recv().await {
                    forward_engine_event(&forwarded_logs, event).await?;
                }
                Ok::<(), String>(())
            });
            logs.send("[setup-app-task] verifying application image".into())
                .await
                .map_err(|_| "setup app task log consumer closed".to_owned())?;
            let preparation_intent = PreparedImageOwner::Setup.preparation_intent(&plan)?;
            session
                .checkpoint_preparation(preparation_intent.clone(), false)
                .await?;
            let prepared = prepare_setup_image(
                &bosn_setup::ImagePreparationEngine::new(
                    &self.engine,
                    &preparation_intent
                        .ownership_proof()
                        .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?,
                &plan,
                RunOptions::streaming(deadline.remaining(), quarter),
                cancellation,
                &events,
            )
            .await;
            let prepared = match prepared {
                Ok(value) => value,
                Err(error) => {
                    drop(events);
                    forwarder
                        .await
                        .map_err(|_| "setup app task log forwarder stopped".to_owned())??;
                    return Err(error.to_string());
                }
            };
            session
                .record_image(setup_ensure_image_resource(
                    &prepared,
                    &plan.workspace_root.to_string_lossy(),
                ))
                .await?;
            session
                .checkpoint_preparation(preparation_intent, true)
                .await?;
            let remaining = deadline.remaining();
            if cancellation.is_cancelled() || remaining.is_zero() {
                drop(events);
                forwarder
                    .await
                    .map_err(|_| "setup app task log forwarder stopped".to_owned())??;
                return Err("setup app task ended before ownership inspection".into());
            }
            logs.send("[setup-app-task] proving exact managed application ownership".into())
                .await
                .map_err(|_| "setup app task log consumer closed".to_owned())?;
            let observed = adopt_setup_app(
                &self.engine,
                CoreSetupEnsureRequest {
                    plan: &plan,
                    workspace_root: request.workspace.clone(),
                    prepared_image: &prepared,
                    options: RunOptions::streaming(deadline.remaining(), quarter),
                    cancellation,
                    events: &events,
                },
            )
            .await;
            let observed = match observed {
                Ok(value) => value,
                Err(error) => {
                    drop(events);
                    forwarder
                        .await
                        .map_err(|_| "setup app task log forwarder stopped".to_owned())??;
                    return Err(error.to_string());
                }
            };
            if !observed.running {
                drop(events);
                forwarder
                    .await
                    .map_err(|_| "setup app task log forwarder stopped".to_owned())??;
                return Err(
                    "setup app task requires the exact managed application to be running".into(),
                );
            }
            if cancellation.is_cancelled() {
                drop(events);
                forwarder
                    .await
                    .map_err(|_| "setup app task log forwarder stopped".to_owned())??;
                return Err(
                    "setup app task cancelled before exec; remote command was not started".into(),
                );
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                drop(events);
                forwarder
                    .await
                    .map_err(|_| "setup app task log forwarder stopped".to_owned())??;
                return Err("setup app task exceeded its deadline before exec".into());
            }
            logs.send(format!(
                "[setup-app-task] running declared task {}",
                request.task_name
            ))
            .await
            .map_err(|_| "setup app task log consumer closed".to_owned())?;
            session
                .begin(setup_app_task_session_container_identity(&observed))
                .await
                .map_err(|_| "setup app task ownership recording unavailable".to_owned())?;
            let result = execute_setup_app_task(
                &self.engine,
                SetupAppTaskRequest {
                    plan: &plan,
                    workspace_root: request.workspace,
                    task_name: request.task_name,
                    passthrough_env: Vec::new(),
                    prepared_image: &prepared,
                    options: RunOptions::streaming(remaining, exec_output),
                    cancellation,
                    events: &events,
                },
            )
            .await;
            drop(events);
            // A normal nonzero exit is a known terminal outcome. Conversely,
            // a killed/timed-out direct Docker client cannot establish that
            // its in-container process stopped, so that row must survive for
            // restart recovery and conservative GC protection.
            let outcome = match &result {
                Ok(_) => "succeeded",
                Err(bosn_setup::SetupTaskError::TaskFailed { .. }) => "failed",
                Err(bosn_setup::SetupTaskError::RemoteStopped(_)) => "stopped",
                Err(_) => "uncertain",
            };
            let finished = session.finish(outcome).await;
            forwarder
                .await
                .map_err(|_| "setup app task log forwarder stopped".to_owned())??;
            if finished.is_err() {
                return Err("setup app task completion recording unavailable".into());
            }
            match result {
                Ok(result) => Ok(format!(
                    "completed declared app task {} in managed container {} with image {}",
                    result.task_name, observed.container_name, result.image_identity
                )),
                Err(error @ bosn_setup::SetupTaskError::RemoteStopped(_)) => {
                    Err(format!("setup app task ended early: {error}"))
                }
                // `docker exec` cancellation kills the local client only. Do
                // not report that this stopped the command in the app.
                Err(bosn_setup::SetupTaskError::Cancelled)
                | Err(bosn_setup::SetupTaskError::Deadline) => Err(
                    "setup app task exec client ended; remote command completion is unknown".into(),
                ),
                Err(error) => Err(error.to_string()),
            }
        })
    }
}

/// Values of the secrets one manifest task declared and that are provisioned.
pub(crate) struct ManifestTaskSecrets {
    pub(crate) values: Vec<(&'static str, String)>,
    pub(crate) missing: Vec<String>,
}

impl ManifestTaskSecrets {
    /// The Docker client carrying each value in its process environment, and
    /// the names to forward with a bare `--env NAME` (never `NAME=value`).
    pub(crate) fn docker_engine(&self, base: &DockerEngine) -> (DockerEngine, Vec<String>) {
        let engine = self
            .values
            .iter()
            .fold(base.clone(), |engine, (env, value)| engine.env(*env, value));
        let names = self
            .values
            .iter()
            .map(|(env, _)| (*env).to_owned())
            .collect();
        (engine, names)
    }
}

/// Resolve declared secrets from daemon state. A refused secret (symlink,
/// loose mode, bad content) fails the task; a missing one runs without it.
pub(crate) fn load_manifest_task_secrets(
    state_dir: &Path,
    declared: &[String],
) -> Result<ManifestTaskSecrets, String> {
    let mut values = Vec::new();
    let mut missing = Vec::new();
    for name in declared {
        let env = secrets::secret_env_name(name)
            .ok_or_else(|| "manifest task declares an unknown secret".to_owned())?;
        match secrets::read_secret(state_dir, name)? {
            Some(value) => values.push((env, value)),
            None => missing.push(name.clone()),
        }
    }
    Ok(ManifestTaskSecrets { values, missing })
}

/// One-line warning when a task will call GitHub anonymously: it declared
/// `github_token` but none is provisioned, or it looks like an act run and
/// declared nothing.
pub(crate) fn manifest_task_github_preflight(
    declared: &[String],
    missing: &[String],
    command: &str,
    github_api_proxy: bool,
) -> Option<String> {
    const REMEDY: &str = "anonymous GitHub API calls are limited to 60/hour per IP; declare `github_api = \"proxy\"` on the task (reads through the host `gh` login, token never enters the container) or run `bosn secret set github_token` (a fine-grained token with no scopes is enough)";
    if missing.iter().any(|name| name == "github_token") {
        return Some(format!(
            "[manifest-app-task] warning: secret github_token is not provisioned, so GITHUB_TOKEN is unset; {REMEDY}"
        ));
    }
    let looks_like_act = command
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word == "act");
    if looks_like_act && !github_api_proxy && !declared.iter().any(|name| name == "github_token") {
        return Some(format!(
            "[manifest-app-task] warning: this task looks like an act run but does not declare secrets = [\"github_token\"]; {REMEDY}"
        ));
    }
    None
}

pub(crate) fn mask_engine_event(masker: &mut SecretMasker, event: EngineEvent) -> EngineEvent {
    if masker.is_empty() {
        return event;
    }
    match event {
        EngineEvent::Stdout(bytes) => EngineEvent::Stdout(masker.push(MaskStream::Stdout, &bytes)),
        EngineEvent::Stderr(bytes) => EngineEvent::Stderr(masker.push(MaskStream::Stderr, &bytes)),
    }
}

pub(crate) async fn forward_engine_event(
    logs: &crate::raw_run_log::JobLogSink,
    event: EngineEvent,
) -> Result<(), String> {
    logs.append(&event)?;
    let (stream, bytes) = match event {
        EngineEvent::Stdout(bytes) => ("stdout", bytes),
        EngineEvent::Stderr(bytes) => ("stderr", bytes),
    };
    if bytes.is_empty() {
        return Ok(());
    }
    let prefix = format!("[{stream}] ");
    // `bosn-engine` bounds source chunks at 8 KiB. Split after lossy text
    // conversion so every daemon record is valid UTF-8 and frame-safe.
    let text = String::from_utf8_lossy(&bytes);
    let body = jobs::MAX_LOG_LINE_BYTES.saturating_sub(prefix.len()).max(1);
    for chunk in text.as_bytes().chunks(body) {
        let line = format!("{prefix}{}", String::from_utf8_lossy(chunk));
        logs.send(line)
            .await
            .map_err(|_| "setup log consumer closed".to_owned())?;
    }
    Ok(())
}
