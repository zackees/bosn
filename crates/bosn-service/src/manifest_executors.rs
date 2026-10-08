//! Docker executors for manifest ensure and manifest app tasks (including guest SSH tasks).

use super::*;

pub(crate) fn setup_container_resource_id(kind: &str, stack: &str, creation_name: &str) -> String {
    format!("{kind}:{stack}:{creation_name}")
}

/// Docker-backed implementation of the deliberately narrow legacy-manifest
/// runtime bridge. It translates only the accepted typed manifest shape into
/// the existing finite image-prepare and ownership-safe ensure primitives;
/// it never invokes a generic Compose/Docker runner.
#[derive(Clone)]
pub struct DockerManifestEnsureExecutor {
    pub(crate) state_dir: PathBuf,
    pub(crate) engine: DockerEngine,
}
impl DockerManifestEnsureExecutor {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            engine: DockerEngine::docker(),
        }
    }
}
impl ManifestEnsureExecutor for DockerManifestEnsureExecutor {
    fn execute<'a>(
        &'a self,
        request: ManifestEnsureJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        registry: &'a RegistryActor,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        Box::pin(async move {
            let prepare_output = request.output_limit / 2;
            let ensure_output = request.output_limit.saturating_sub(prepare_output);
            if prepare_output == 0 || ensure_output == 0 {
                return Err(
                    "manifest ensure output budget cannot fund preparation and ensure".into(),
                );
            }
            let deadline = async_engine::Deadline::after(request.deadline);
            let runtime = async_engine::cancellable(
                cancellation,
                async_engine::timeout_at(
                    deadline,
                    manifest_stack_setup_plan_at(&request, Some(&self.state_dir)),
                ),
            )
            .await
            .map_err(|_| "manifest ensure cancelled".to_owned())?
            .map_err(|_| "manifest ensure planning exceeded its deadline".to_owned())??;
            // Persist the exact volume contract before Docker can create a
            // volume. A crash after creation therefore leaves a narrow,
            // labelled intent that the next ensure can prove and recover.
            registry
                .put_manifest_volume_intents(runtime.volumes.clone())
                .await
                .map_err(|error| format!("manifest volume intent unavailable: {error}"))?;
            let ManifestRuntimePlan {
                plan,
                generation,
                volumes,
                is_guest,
                autostart,
                ..
            } = runtime;
            let (events, mut receiver) = async_engine::channel(MANIFEST_ENGINE_EVENT_QUEUE);
            let forwarded_logs = logs.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(event) = receiver.recv().await {
                    forward_engine_event(&forwarded_logs, event).await?;
                }
                Ok::<(), String>(())
            });
            let images = ActorSetupImageRecorder {
                actor: registry.clone(),
            };
            let pipeline = SetupEnsurePipeline {
                plan: &plan,
                workspace: request.workspace.clone(),
                deadline: &deadline,
                prepare_output,
                ensure_output,
                images: Some((&images, PreparedImageOwner::Manifest(&request.stack))),
            };
            logs.send("[manifest] preparing immutable application image".into())
                .await
                .map_err(|_| "manifest log consumer closed".to_owned())?;
            let result =
                execute_setup_ensure_pipeline(&self.engine, &pipeline, cancellation, &events, logs)
                    .await;
            drop(events);
            forwarder
                .await
                .map_err(|_| "manifest log forwarder stopped".to_owned())??;
            let result = result?;
            if result.ensured.image_identity != result.prepared.observed_identity {
                return Err("manifest ensure image receipt does not match ensured app".into());
            }
            let workspace = plan.workspace_root.to_string_lossy().into_owned();
            Ok(SetupEnsureExecution {
                receipt: format!(
                    "ensured manifest {} {} as {}",
                    if is_guest { "guest" } else { "stack" },
                    request.stack,
                    result.ensured.container_name
                ),
                resource: SetupEnsureResource {
                    id: setup_container_resource_id(
                        if is_guest {
                            "manifest-guest"
                        } else {
                            "manifest-container"
                        },
                        &request.stack,
                        &result.ensured.container_name,
                    ),
                    name: result.ensured.container_name,
                    stack: request.stack.clone(),
                    generation: generation.clone(),
                    workspace: workspace.clone(),
                },
                image: PreparedImageOwner::Manifest(&request.stack)
                    .resource(&result.prepared, &workspace),
                volumes,
                manifest_autostart: autostart,
            })
        })
    }
}

/// Docker-backed execution of one declared task in an already ensured
/// manifest stack.  It deliberately re-derives the complete accepted stack
/// plan and image receipt, then performs an inspect-only ownership proof
/// immediately before the one fixed `container exec` operation.
#[derive(Clone)]
pub struct DockerManifestAppTaskExecutor {
    pub(crate) state_dir: PathBuf,
    pub(crate) engine: DockerEngine,
    pub(crate) guest_ssh: Arc<dyn GuestSshTaskTransport>,
    /// `github_api = "proxy"` (#308): the host `gh` used for the credential,
    /// the upstream, and the ETag cache shared by every task's proxy.
    pub(crate) gh_program: std::ffi::OsString,
    pub(crate) github_upstream: String,
    pub(crate) github_cache: Arc<github_proxy::ResponseCache>,
}
impl DockerManifestAppTaskExecutor {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            engine: DockerEngine::docker(),
            guest_ssh: Arc::new(NativeGuestSshTaskTransport),
            gh_program: "gh".into(),
            github_upstream: github_proxy::GITHUB_API_UPSTREAM.into(),
            github_cache: Arc::new(github_proxy::ResponseCache::default()),
        }
    }
}
impl ManifestAppTaskExecutor for DockerManifestAppTaskExecutor {
    #[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
    fn execute<'a>(
        &'a self,
        request: ManifestAppTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        session: &'a dyn ManifestAppTaskSessionRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let quarter = request.output_limit / 4;
            let exec_output = request.output_limit.saturating_sub(quarter * 3);
            if quarter == 0 || exec_output == 0 {
                return Err(
                    "manifest app task output budget cannot fund validation and execution".into(),
                );
            }
            let deadline = async_engine::Deadline::after(request.deadline);
            let runtime = async_engine::cancellable(
                cancellation,
                async_engine::timeout_at(
                    deadline,
                    manifest_stack_task_setup_plan_at(&request, Some(&self.state_dir)),
                ),
            )
            .await
            .map_err(|_| "manifest app task cancelled before ownership inspection".to_owned())?
            .map_err(|_| "manifest app task planning exceeded its deadline".to_owned())??;
            let guest_task = runtime.guest_task;
            let job_caches = runtime.job_caches;
            let plan = runtime.plan;
            let task_command = plan
                .tasks
                .get(&request.task_name)
                .map(|task| task.command.clone())
                .unwrap_or_default();
            let task_secrets = load_manifest_task_secrets(&self.state_dir, &runtime.secrets)?;
            let (mut engine, mut passthrough_env) = task_secrets.docker_engine(&self.engine);
            let mut masked: Vec<String> = task_secrets
                .values
                .iter()
                .map(|(_, value)| value.clone())
                .collect();
            // The proxy lives exactly as long as this task run; dropping it
            // at the end of `execute` stops the listener.
            let github_api = if runtime.github_api_proxy {
                if guest_task.is_some() {
                    return Err(
                        "github_api = \"proxy\" is not supported for macOS guest tasks".into(),
                    );
                }
                let credential =
                    github_proxy::resolve_credential(&self.state_dir, &self.gh_program).await?;
                if let Some(value) = credential.secret_value() {
                    masked.push(value.to_owned());
                }
                let source = credential.source();
                let proxy = github_proxy::GithubApiProxy::start(
                    &self.github_upstream,
                    credential,
                    self.github_cache.clone(),
                    Some((**logs).clone()),
                )
                .await
                .map_err(|_| "GitHub API proxy could not start".to_owned())?;
                // The URL's nonce is a capability for this run: persisted
                // logs show `***` in its place.
                if let Some(nonce) = proxy.url().rsplit('/').next() {
                    masked.push(nonce.to_owned());
                }
                engine = engine.env("GITHUB_API_URL", proxy.url());
                passthrough_env.push("GITHUB_API_URL".into());
                Some((proxy, source))
            } else {
                None
            };
            let error_masker = SecretMasker::new(masked.iter());
            let mut stream_masker = SecretMasker::new(masked.iter());
            let (events, mut receiver) = async_engine::channel(MANIFEST_ENGINE_EVENT_QUEUE);
            let forwarded_logs = logs.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(event) = receiver.recv().await {
                    let event = mask_engine_event(&mut stream_masker, event);
                    forward_engine_event(&forwarded_logs, event).await?;
                }
                for event in [
                    EngineEvent::Stdout(stream_masker.finish(MaskStream::Stdout)),
                    EngineEvent::Stderr(stream_masker.finish(MaskStream::Stderr)),
                ] {
                    forward_engine_event(&forwarded_logs, event).await?;
                }
                Ok::<(), String>(())
            });
            let preflight = manifest_task_github_preflight(
                &runtime.secrets,
                &task_secrets.missing,
                &task_command,
                runtime.github_api_proxy,
            );
            let result = async {
                if let Some((_, source)) = &github_api {
                    logs.send(format!(
                        "[manifest-app-task] read-only GitHub API proxy on loopback is GITHUB_API_URL for this task; credential: {}",
                        source.label()
                    ))
                    .await
                    .map_err(|_| "manifest app task log consumer closed".to_owned())?;
                }
                if let Some(warning) = preflight {
                    logs.send(warning)
                        .await
                        .map_err(|_| "manifest app task log consumer closed".to_owned())?;
                }
                if guest_task.is_some() && !passthrough_env.is_empty() {
                    return Err(
                        "manifest task secrets are not supported for macOS guest tasks".into(),
                    );
                }
                let remaining = deadline.remaining();
                if cancellation.is_cancelled() || remaining.is_zero() {
                    return Err("manifest app task ended before image verification".into());
                }
                logs.send("[manifest-app-task] verifying immutable application image".into())
                    .await
                    .map_err(|_| "manifest app task log consumer closed".to_owned())?;
                let preparation_intent = PreparedImageOwner::Manifest(&request.stack).preparation_intent(&plan)?;
                session.checkpoint_preparation(preparation_intent.clone(), false).await?;
                let prepared = prepare_setup_image(
                    &bosn_setup::ImagePreparationEngine::new(&self.engine,
                    &preparation_intent.ownership_proof().map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?,
                    &plan,
                    RunOptions::streaming(deadline.remaining(), quarter),
                    cancellation,
                    &events,
                )
                .await
                .map_err(|error| error.to_string())?;
                session.record_image(PreparedImageOwner::Manifest(&request.stack).resource(
                    &prepared, &plan.workspace_root.to_string_lossy(),
                )).await?;
                session.checkpoint_preparation(preparation_intent, true).await?;
                let remaining = deadline.remaining();
                if cancellation.is_cancelled() || remaining.is_zero() {
                    return Err("manifest app task ended before ownership inspection".into());
                }
                logs.send("[manifest-app-task] proving exact managed application ownership".into())
                    .await
                    .map_err(|_| "manifest app task log consumer closed".to_owned())?;
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
                .await
                .map_err(|error| error.to_string())?;
                if !observed.running {
                    return Err(
                        "manifest app task requires the exact managed application to be running"
                            .into(),
                    );
                }
                // #314: name the verified container and the checkout its
                // binds were proven against, so a run in one worktree can be
                // seen not to target another worktree's tree.
                logs.send(format!(
                    "[manifest-app-task] verified {} binds workspace {}",
                    observed.container_name,
                    plan.workspace_root.display()
                ))
                .await
                .map_err(|_| "manifest app task log consumer closed".to_owned())?;
                if cancellation.is_cancelled() || deadline.remaining().is_zero() {
                    return Err(
                        "manifest app task ended before exec; remote command was not started"
                            .into(),
                    );
                }
                if let Some(guest_task) = guest_task {
                    return execute_manifest_guest_ssh_task(
                        self.guest_ssh.as_ref(),
                        &self.state_dir,
                        &plan.workspace_root,
                        &observed,
                        &guest_task,
                        &request.task_name,
                        &deadline,
                        exec_output,
                        cancellation,
                        logs,
                        &events,
                        session,
                    )
                    .await;
                }
                // Runner slot (#358): limits on this container, and the
                // job's Docker proxy when the stack drives the host engine.
                let runner = match session.run_context() {
                    Some(context) => Some(
                        task_runner::attach(
                            context,
                            &observed.container_name,
                            &plan.workspace_root,
                            plan.host_docker_socket
                                .as_ref()
                                .and_then(|socket| socket.proxy_dir.as_deref()),
                            &job_caches,
                            logs,
                        )
                        .await,
                    ),
                    None => None,
                };
                if let Some(runner) = &runner {
                    for (key, value) in &runner.env {
                        engine = engine.env(key, value);
                        passthrough_env.push(key.clone());
                    }
                }
                logs.send(format!(
                    "[manifest-app-task] running declared task {}",
                    request.task_name
                ))
                .await
                .map_err(|_| "manifest app task log consumer closed".to_owned())?;
                // A retired generation's daemons share this stack's volumes
                // (#383): stop the idle ones before this task uses them.
                let workspace = plan.workspace_root.to_string_lossy();
                stop_retired_generations(session, &workspace, &request.stack, logs).await?;
                session
                    .begin(manifest_app_task_session_container_identity(&observed))
                    .await
                    .map_err(|_| "manifest app task ownership recording unavailable".to_owned())?;
                let result = execute_setup_app_task(
                    &engine,
                    SetupAppTaskRequest {
                        plan: &plan,
                        workspace_root: request.workspace.clone(),
                        task_name: request.task_name.clone(),
                        passthrough_env: passthrough_env.clone(),
                        prepared_image: &prepared,
                        options: RunOptions::streaming(deadline.remaining(), exec_output),
                        cancellation,
                        events: &events,
                    },
                )
                .await;
                // Whatever the outcome (success, failure, cancel, stall or
                // a lost client), remove what the job created.
                if let Some(runner) = runner {
                    runner.finish(logs).await;
                }
                // A confirmed in-container stop (#357) is a known terminal
                // outcome; only an unconfirmed one stays uncertain.
                let outcome = match &result {
                    Ok(_) => "succeeded",
                    Err(bosn_setup::SetupTaskError::TaskFailed { .. }) => "failed",
                    Err(bosn_setup::SetupTaskError::RemoteStopped(_)) => "stopped",
                    Err(_) => "uncertain",
                };
                session
                    .finish(outcome)
                    .await
                    .map_err(|_| "manifest app task completion recording unavailable".to_owned())?;
                // The last task to leave a retired container stops it.
                stop_retired_generations(session, &workspace, &request.stack, logs).await?;
                match result {
                    Ok(value) => Ok(format!(
                        "completed declared manifest task {} in managed container {} with image {}",
                        value.task_name, observed.container_name, value.image_identity
                    )),
                    Err(error @ bosn_setup::SetupTaskError::RemoteStopped(_)) => {
                        Err(format!("manifest app task ended early: {error}"))
                    }
                    Err(bosn_setup::SetupTaskError::Cancelled)
                    | Err(bosn_setup::SetupTaskError::Deadline) => Err(
                        "manifest app task exec client ended; remote command completion is unknown"
                            .into(),
                    ),
                    Err(error) => Err(error.to_string()),
                }
            }
            .await;
            drop(github_api);
            drop(events);
            forwarder
                .await
                .map_err(|_| "manifest app task log forwarder stopped".to_owned())??;
            // Error text can quote task stdout/stderr; it is stored as the
            // job error and returned to clients, so it is masked too.
            result
                .map(|text| error_masker.mask_text(&text))
                .map_err(|text| error_masker.mask_text(&text))
        })
    }
}

async fn stop_retired_generations(
    session: &dyn ManifestAppTaskSessionRecorder,
    workspace: &str,
    stack: &str,
    logs: &crate::raw_run_log::JobLogSink,
) -> Result<(), String> {
    for line in session.stop_retired_generations(workspace, stack).await {
        logs.send(line)
            .await
            .map_err(|_| "manifest app task log consumer closed".to_owned())?;
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum ManifestGuestTaskOutcome {
    Failed { exit_code: i32, detail: String },
    Uncertain(String),
}

/// Run one already-authorized guest task only after the regular manifest
/// executor has freshly proved the deterministic dockurr container is exact
/// and running.  The SSH endpoint is never taken from the caller or ambient
/// SSH configuration: [`GuestSshCommand`] itself hard-codes loopback and this
/// function derives every remaining field from the current manifest receipt.
#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
pub(crate) async fn execute_manifest_guest_ssh_task(
    transport: &dyn GuestSshTaskTransport,
    state_dir: &Path,
    workspace: &Path,
    observed: &SetupEnsureResult,
    guest_task: &ManifestGuestTask,
    task_name: &str,
    deadline: &async_engine::Deadline,
    output_limit: usize,
    cancellation: &async_engine::CancellationToken,
    logs: &crate::raw_run_log::JobLogSink,
    events: &async_engine::Sender<EngineEvent>,
    session: &dyn ManifestAppTaskSessionRecorder,
) -> Result<String, String> {
    let identity_file = manifest_guest_ssh_identity_file(state_dir)?;
    let command = guest_remote_command(&guest_task.command, guest_task.workdir.as_deref())?;
    let ready_output = output_limit / 4;
    let payload_output = if guest_task.payload.is_some() {
        output_limit / 4
    } else {
        0
    };
    let task_output = output_limit.saturating_sub(ready_output + payload_output);
    if ready_output == 0
        || task_output == 0
        || (guest_task.payload.is_some() && payload_output == 0)
    {
        return Err(
            "manifest guest app task output budget cannot fund readiness, payload, and execution"
                .into(),
        );
    }
    let guest_command = |command| GuestSshCommand {
        user: guest_task.ssh_user.clone(),
        port: guest_task.ssh_port,
        identity_file: identity_file.clone(),
        command,
    };
    if cancellation.is_cancelled() || deadline.remaining().is_zero() {
        return Err(
            "manifest guest app task ended before SSH readiness; remote task was not started"
                .into(),
        );
    }
    logs.send("[manifest-guest-app-task] verifying guest SSH readiness".into())
        .await
        .map_err(|_| "manifest guest app task log consumer closed".to_owned())?;
    let ready = transport
        .stream(
            guest_command("true".into()),
            RunOptions::streaming(deadline.remaining(), ready_output),
            cancellation,
            events,
        )
        .await
        .map_err(|error| format!("manifest guest SSH readiness transport failed: {error}"))?;
    if !ready.ok() {
        return Err(format!(
            "manifest guest SSH readiness failed with exit {}; the declared task was not started",
            ready.exit_code
        ));
    }
    if let Some(payload) = &guest_task.payload {
        if cancellation.is_cancelled() || deadline.remaining().is_zero() {
            return Err(
                "manifest guest app task ended before SCP payload upload; remote task was not started"
                    .into(),
            );
        }
        // The payload may be a fresh build output, so prove it again at the
        // last responsible point rather than trusting planning-time facts.
        let source = manifest_guest_payload_file(workspace, &payload.source)?;
        logs.send(
            "[manifest-guest-app-task] copying declared payload through verified guest SCP".into(),
        )
        .await
        .map_err(|_| "manifest guest app task log consumer closed".to_owned())?;
        let copied = transport
            .stream_scp(
                GuestScpCommand {
                    user: guest_task.ssh_user.clone(),
                    port: guest_task.ssh_port,
                    identity_file: identity_file.clone(),
                    source,
                    destination: payload.destination.clone(),
                },
                RunOptions::streaming(deadline.remaining(), payload_output),
                cancellation,
                events,
            )
            .await
            .map_err(|error| {
                format!(
                    "manifest guest SCP payload upload failed; declared task was not started: {error}"
                )
            })?;
        if !copied.ok() {
            return Err(format!(
                "manifest guest SCP payload upload failed with exit {}; declared task was not started: {}",
                copied.exit_code,
                bounded_guest_failure_detail(&copied)
            ));
        }
    }
    if cancellation.is_cancelled() || deadline.remaining().is_zero() {
        return Err(
            "manifest guest app task ended before SSH execution; remote task was not started"
                .into(),
        );
    }
    logs.send(format!(
        "[manifest-guest-app-task] running declared task {task_name} through verified guest SSH"
    ))
    .await
    .map_err(|_| "manifest guest app task log consumer closed".to_owned())?;
    session
        .begin(manifest_app_task_session_container_identity(observed))
        .await
        .map_err(|_| "manifest guest app task ownership recording unavailable".to_owned())?;
    let result = match transport
        .stream(
            guest_command(command),
            RunOptions::streaming(deadline.remaining(), task_output),
            cancellation,
            events,
        )
        .await
    {
        Ok(result) if result.ok() => Ok(()),
        // OpenSSH reserves 255 for connection/protocol failure, but a remote
        // command may also return it. Either way the daemon cannot prove
        // whether a command reached or completed on the VM.
        Ok(result) if result.exit_code == 255 => Err(ManifestGuestTaskOutcome::Uncertain(
            "guest SSH exited 255; remote task completion is unknown".into(),
        )),
        Ok(result) => Err(ManifestGuestTaskOutcome::Failed {
            exit_code: result.exit_code,
            detail: bounded_guest_failure_detail(&result),
        }),
        Err(error) => Err(ManifestGuestTaskOutcome::Uncertain(format!(
            "guest SSH client ended; remote task completion is unknown: {error}"
        ))),
    };
    let outcome = match &result {
        Ok(()) => "succeeded",
        Err(ManifestGuestTaskOutcome::Failed { .. }) => "failed",
        Err(ManifestGuestTaskOutcome::Uncertain(_)) => "uncertain",
    };
    session
        .finish(outcome)
        .await
        .map_err(|_| "manifest guest app task completion recording unavailable".to_owned())?;
    match result {
        Ok(()) => Ok(format!(
            "completed declared manifest guest task {task_name} through managed container {} with image {}",
            observed.container_name, observed.image_identity
        )),
        Err(ManifestGuestTaskOutcome::Failed { exit_code, detail }) => Err(format!(
            "declared manifest guest task exited with {exit_code}: {detail}"
        )),
        Err(ManifestGuestTaskOutcome::Uncertain(detail)) => Err(detail),
    }
}

pub(crate) fn manifest_guest_ssh_identity_file(state_dir: &Path) -> Result<PathBuf, String> {
    let root = fs::canonical_context_path(state_dir)
        .map_err(|_| "manifest guest SSH state directory cannot be canonicalized".to_owned())?;
    if fs::context_path_metadata_no_follow(&root)
        .map_err(|_| "manifest guest SSH state directory cannot be inspected".to_owned())?
        .kind
        != fs::ContextPathKind::Directory
    {
        return Err("manifest guest SSH state root is not a directory".into());
    }
    let identity = root.join("guest-ssh").join("id_ed25519");
    if fs::context_path_metadata_no_follow(&identity)
        .map_err(|_| "manifest guest SSH identity is not provisioned in daemon state".to_owned())?
        .kind
        != fs::ContextPathKind::RegularFile
    {
        return Err("manifest guest SSH identity is not a regular daemon-state file".into());
    }
    let canonical = fs::canonical_context_path(&identity)
        .map_err(|_| "manifest guest SSH identity cannot be canonicalized".to_owned())?;
    // Reject every symlink (including a parent component) rather than merely
    // proving that its target happens to lie below state today.
    if canonical != identity || !canonical.starts_with(&root) {
        return Err("manifest guest SSH identity must not traverse a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = std::fs::metadata(&identity)
            .map_err(|_| "manifest guest SSH identity cannot be inspected".to_owned())?
            .mode();
        if mode & 0o077 != 0 {
            return Err("manifest guest SSH identity must not be group- or world-readable".into());
        }
    }
    Ok(identity)
}

// SCP is a streaming process, rather than a read-into-memory operation, but a
// finite manifest payload still needs a product-level upper bound.
pub(crate) const MAX_MANIFEST_GUEST_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Re-prove that the one manifest-declared source is a regular, bounded file
/// directly beneath the selected canonical workspace. This is intentionally
/// performed immediately before SCP because payloads are normally build
/// outputs and can change after manifest planning.
pub(crate) fn manifest_guest_payload_file(
    workspace: &Path,
    source: &str,
) -> Result<PathBuf, String> {
    if !safe_manifest_relative_path(source) {
        return Err("manifest guest payload must be a safe workspace-relative path".into());
    }
    let root = fs::canonical_context_path(workspace)
        .map_err(|_| "manifest guest payload workspace cannot be canonicalized".to_owned())?;
    if fs::context_path_metadata_no_follow(&root)
        .map_err(|_| "manifest guest payload workspace cannot be inspected".to_owned())?
        .kind
        != fs::ContextPathKind::Directory
    {
        return Err("manifest guest payload workspace is not a directory".into());
    }
    let candidate = root.join(source);
    let metadata = fs::context_path_metadata_no_follow(&candidate)
        .map_err(|_| "declared manifest guest payload does not exist".to_owned())?;
    if metadata.kind != fs::ContextPathKind::RegularFile {
        return Err("declared manifest guest payload is not a regular file".into());
    }
    let size = metadata
        .len
        .ok_or_else(|| "declared manifest guest payload size cannot be determined".to_owned())?;
    if size > MAX_MANIFEST_GUEST_PAYLOAD_BYTES {
        return Err(format!(
            "declared manifest guest payload exceeds the {} byte limit",
            MAX_MANIFEST_GUEST_PAYLOAD_BYTES
        ));
    }
    let canonical = fs::canonical_context_path(&candidate)
        .map_err(|_| "declared manifest guest payload cannot be canonicalized".to_owned())?;
    // Equality, rather than only starts_with, rejects a symlink in every
    // component below the selected workspace as well as a symlinked leaf.
    if canonical != candidate || !canonical.starts_with(&root) {
        return Err("declared manifest guest payload must not traverse a symlink".into());
    }
    if candidate.to_str().is_none() {
        return Err("declared manifest guest payload path is not UTF-8".into());
    }
    Ok(candidate)
}

/// Accept only a stable guest file pathname. SCP's remote target has its own
/// colon grammar, so punctuation accepted by a shell or a general remote-path
/// API is deliberately not accepted here.
pub(crate) fn normalize_manifest_guest_payload_destination(value: &str) -> Result<String, String> {
    if value.len() > 4096
        || value.is_empty()
        || !value.is_ascii()
        || value.contains(['\0', '\\', ':'])
    {
        return Err("manifest guest payload_destination is unsafe".into());
    }
    let suffix = if let Some(value) = value.strip_prefix("~/") {
        value
    } else if let Some(value) = value.strip_prefix('/') {
        value
    } else {
        return Err(
            "manifest guest payload_destination must be a normalized absolute or ~/ path".into(),
        );
    };
    if suffix.is_empty()
        || suffix
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'~' | b'_' | b'-' | b'.')
        })
    {
        return Err("manifest guest payload_destination is not normalized".into());
    }
    Ok(value.into())
}

pub(crate) fn guest_remote_command(command: &str, workdir: Option<&str>) -> Result<String, String> {
    if command.is_empty() || command.len() > 16 * 1024 || command.contains('\0') {
        return Err("manifest guest task command is unsafe".into());
    }
    let Some(workdir) = workdir else {
        return Ok(command.into());
    };
    validate_manifest_guest_workdir(workdir)?;
    Ok(format!("cd {} && {command}", shell_single_quote(workdir)))
}

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\\"'\\\"'"))
}

pub(crate) fn bounded_guest_failure_detail(result: &CommandResult) -> String {
    let mut detail = String::from_utf8_lossy(&result.stderr).trim().to_owned();
    if detail.is_empty() {
        detail = String::from_utf8_lossy(&result.stdout).trim().to_owned();
    }
    bounded_log_line(&detail)
}
