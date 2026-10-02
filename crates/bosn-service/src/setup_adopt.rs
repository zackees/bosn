//! Adopting an existing setup app, and the shared prepare-then-ensure pipeline.

use super::*;

/// Docker-backed restoration. Preparation is deliberately retained because it
/// produces the validated inspected image identity used to prove the existing
/// candidate; it never creates, starts, stops, removes, or replaces Docker.
#[derive(Clone)]
pub struct DockerSetupAdoptExecutor {
    pub(crate) state_dir: PathBuf,
    pub(crate) engine: DockerEngine,
}
impl DockerSetupAdoptExecutor {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            engine: DockerEngine::docker(),
        }
    }
}
impl SetupAdoptExecutor for DockerSetupAdoptExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupAdoptRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        Box::pin(async move {
            if !request.confirm {
                return Err("setup adoption requires confirmation".into());
            }
            let prepare_output = request.output_limit / 2;
            let inspect_output = request.output_limit.saturating_sub(prepare_output);
            if prepare_output == 0 || inspect_output == 0 {
                return Err(
                    "setup adoption output budget cannot fund preparation and inspection".into(),
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
            .map_err(|_| "setup adoption cancelled".to_owned())?
            .map_err(|_| "setup adoption planning exceeded its deadline".to_owned())?
            .map_err(|e| e.to_string())?;
            let (events, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
            let forwarded = logs.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(event) = receiver.recv().await {
                    forward_engine_event(&forwarded, event).await?;
                }
                Ok::<(), String>(())
            });
            logs.send("[setup] preparing application image for adoption".into())
                .await
                .map_err(|_| "setup log consumer closed".to_owned())?;
            let prepared = prepare_setup_image(
                &self.engine,
                &plan,
                RunOptions::streaming(deadline.remaining(), prepare_output),
                cancellation,
                &events,
            )
            .await
            .map_err(|e| e.to_string())?;
            logs.send("[setup] proving existing managed application ownership".into())
                .await
                .map_err(|_| "setup log consumer closed".to_owned())?;
            let adopted = adopt_setup_app(
                &self.engine,
                CoreSetupEnsureRequest {
                    plan: &plan,
                    workspace_root: request.workspace,
                    prepared_image: &prepared,
                    options: RunOptions::streaming(deadline.remaining(), inspect_output),
                    cancellation,
                    events: &events,
                },
            )
            .await
            .map_err(|e| e.to_string());
            drop(events);
            forwarder
                .await
                .map_err(|_| "setup log forwarder stopped".to_owned())??;
            let adopted = adopted?;
            if adopted.image_identity != prepared.observed_identity {
                return Err("setup adoption result image does not match prepared image".into());
            }
            Ok(SetupEnsureExecution {
                receipt: format!(
                    "adopted {} as {}",
                    adopted.container_name, adopted.container_id
                ),
                resource: SetupEnsureResource {
                    id: setup_container_resource_id(
                        "setup-container",
                        "setup",
                        &adopted.container_name,
                    ),
                    name: adopted.container_name,
                    stack: "setup".into(),
                    generation: format!("sha256:{}", plan.content_sha256),
                    workspace: plan.workspace_root.to_string_lossy().into_owned(),
                },
                image: setup_ensure_image_resource(
                    &prepared,
                    &plan.workspace_root.to_string_lossy(),
                ),
                volumes: Vec::new(),
                manifest_autostart: false,
            })
        })
    }
}

/// Execute already-planned app preparation and ensure under one absolute
/// deadline and one aggregate output budget. This generic helper preserves
/// the finite core engine commands for deterministic no-Docker tests.
pub(crate) struct SetupEnsurePipeline<'a> {
    pub(crate) plan: &'a SetupPlan,
    pub(crate) workspace: PathBuf,
    pub(crate) deadline: &'a async_engine::Deadline,
    pub(crate) prepare_output: usize,
    pub(crate) ensure_output: usize,
}

pub(crate) struct PreparedSetupEnsure {
    pub(crate) prepared: PreparedImage,
    pub(crate) ensured: bosn_setup::SetupEnsureResult,
}

pub(crate) async fn execute_setup_ensure_pipeline<E: SetupImageEngine + SetupEnsureEngine>(
    engine: &E,
    pipeline: &SetupEnsurePipeline<'_>,
    cancellation: &async_engine::CancellationToken,
    events: &async_engine::Sender<EngineEvent>,
    logs: &async_engine::Sender<String>,
) -> Result<PreparedSetupEnsure, String> {
    if cancellation.is_cancelled() {
        return Err("setup ensure cancelled".into());
    }
    let remaining = pipeline.deadline.remaining();
    if remaining.is_zero() {
        return Err("setup ensure exceeded its deadline".into());
    }
    logs.send("[setup] preparing application image".into())
        .await
        .map_err(|_| "setup log consumer closed".to_owned())?;
    let prepared = prepare_setup_image(
        engine,
        pipeline.plan,
        RunOptions::streaming(remaining, pipeline.prepare_output),
        cancellation,
        events,
    )
    .await
    .map_err(|error| error.to_string())?;
    if cancellation.is_cancelled() {
        return Err("setup ensure cancelled".into());
    }
    let remaining = pipeline.deadline.remaining();
    if remaining.is_zero() {
        return Err("setup ensure exceeded its deadline".into());
    }
    logs.send("[setup] ensuring application container".into())
        .await
        .map_err(|_| "setup log consumer closed".to_owned())?;
    // `ensure_setup_app` validates the image receipt against the plan before
    // inspection. Any mismatch fails closed before create/start can occur.
    let ensured = ensure_setup_app(
        engine,
        CoreSetupEnsureRequest {
            plan: pipeline.plan,
            workspace_root: pipeline.workspace.clone(),
            prepared_image: &prepared,
            options: RunOptions::streaming(remaining, pipeline.ensure_output),
            cancellation,
            events,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(PreparedSetupEnsure { prepared, ensured })
}

pub(crate) fn setup_ensure_image_resource(
    prepared: &PreparedImage,
    workspace: &str,
) -> SetupEnsureImageResource {
    // `prepared.observed_identity` is accepted only from `prepare_setup_image`, which
    // validates Docker's canonical sha256 image ID.  Do not use the document
    // reference here: references/tags are not durable resource identities.
    let logical_identity = format!("setup-image:{}", prepared.observed_identity);
    SetupEnsureImageResource {
        id: logical_identity.clone(),
        name: logical_identity,
        stack: "setup".into(),
        generation: prepared.observed_identity.clone(),
        workspace: workspace.into(),
    }
}
