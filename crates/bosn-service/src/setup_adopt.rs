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
        logs: &'a crate::raw_run_log::JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        self.execute_recorded(request, cancellation, logs, &ReceiptOnlyImageRecorder)
    }
    fn execute_recorded<'a>(
        &'a self,
        request: SetupAdoptRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        images: &'a dyn SetupImageRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        Box::pin(async move {
            if !request.confirm {
                return Err("setup adoption requires confirmation".into());
            }
            let (prepare_output, inspect_output) = adoption_output_budget(request.output_limit)?;
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
            .await
            .map_err(|e| e.to_string())?;
            images
                .record(setup_ensure_image_resource(
                    &prepared,
                    &plan.workspace_root.to_string_lossy(),
                ))
                .await?;
            images.complete_preparation(preparation_intent).await?;
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
            adoption_execution(adopted, &prepared, &plan)
        })
    }
}

fn adoption_execution(
    adopted: SetupEnsureResult,
    prepared: &PreparedImage,
    plan: &SetupPlan,
) -> Result<SetupEnsureExecution, String> {
    if adopted.image_identity != prepared.observed_identity {
        return Err("setup adoption result image does not match prepared image".into());
    }
    Ok(SetupEnsureExecution {
        receipt: format!(
            "adopted {} as {}",
            adopted.container_name, adopted.container_id
        ),
        resource: SetupEnsureResource {
            id: setup_container_resource_id("setup-container", "setup", &adopted.container_name),
            name: adopted.container_name,
            stack: "setup".into(),
            generation: format!("sha256:{}", plan.content_sha256),
            workspace: plan.workspace_root.to_string_lossy().into_owned(),
        },
        image: setup_ensure_image_resource(prepared, &plan.workspace_root.to_string_lossy()),
        volumes: Vec::new(),
        manifest_autostart: false,
    })
}

fn adoption_output_budget(output_limit: usize) -> Result<(usize, usize), String> {
    let prepare = output_limit / 2;
    let inspect = output_limit.saturating_sub(prepare);
    if prepare == 0 || inspect == 0 {
        return Err("setup adoption output budget cannot fund preparation and inspection".into());
    }
    Ok((prepare, inspect))
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
    pub(crate) images: Option<(&'a dyn SetupImageRecorder, PreparedImageOwner<'a>)>,
}

pub(crate) struct PreparedSetupEnsure {
    pub(crate) prepared: PreparedImage,
    pub(crate) ensured: bosn_setup::SetupEnsureResult,
}

pub(crate) async fn execute_setup_ensure_pipeline<
    E: SetupImageEngine + SetupEnsureEngine + Sync,
>(
    engine: &E,
    pipeline: &SetupEnsurePipeline<'_>,
    cancellation: &async_engine::CancellationToken,
    events: &async_engine::Sender<EngineEvent>,
    logs: &crate::raw_run_log::JobLogSink,
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
    let preparation_intent = if let Some((images, owner)) = &pipeline.images {
        let intent = owner.preparation_intent(pipeline.plan)?;
        images.record_preparation(intent.clone()).await?;
        Some(intent)
    } else {
        None
    };
    let image_engine = match preparation_intent.as_ref() {
        Some(intent) => bosn_setup::ImagePreparationEngine::new(
            engine,
            &intent
                .ownership_proof()
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?,
        None => bosn_setup::ImagePreparationEngine::unowned(engine),
    };
    let prepared = prepare_setup_image(
        &image_engine,
        pipeline.plan,
        RunOptions::streaming(pipeline.deadline.remaining(), pipeline.prepare_output),
        cancellation,
        events,
    )
    .await
    .map_err(|error| error.to_string())?;
    if let Some((images, owner)) = &pipeline.images {
        images
            .record(owner.resource(&prepared, &pipeline.plan.workspace_root.to_string_lossy()))
            .await?;
        if let Some(intent) = preparation_intent {
            images.complete_preparation(intent).await?;
        }
        let name = bosn_setup::setup_container_name(pipeline.plan, &pipeline.workspace, &prepared)
            .map_err(|error| error.to_string())?;
        let (namespace, stack) = match owner {
            PreparedImageOwner::Setup => ("setup-container", "setup"),
            PreparedImageOwner::Manifest(stack) => (
                if pipeline.plan.macos_guest.is_some() {
                    "manifest-guest"
                } else {
                    "manifest-container"
                },
                *stack,
            ),
        };
        images
            .record_container_intent(SetupEnsureResource {
                id: setup_container_resource_id(namespace, stack, &name),
                name,
                stack: stack.into(),
                generation: format!("sha256:{}", pipeline.plan.content_sha256),
                workspace: pipeline.plan.workspace_root.to_string_lossy().into_owned(),
            })
            .await?;
    }
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
    PreparedImageOwner::Setup.resource(prepared, workspace)
}

pub(crate) enum PreparedImageOwner<'a> {
    Setup,
    Manifest(&'a str),
}

impl PreparedImageOwner<'_> {
    pub(crate) fn preparation_intent(
        &self,
        plan: &SetupPlan,
    ) -> Result<bosn_registry::ImageCreationIntent, String> {
        let intent =
            bosn_setup::image_preparation_intent(plan).map_err(|error| error.to_string())?;
        let (owner, stack) = match self {
            Self::Setup => (bosn_registry::ImageIntentOwner::Setup, "setup"),
            Self::Manifest(stack) => (bosn_registry::ImageIntentOwner::Manifest, *stack),
        };
        Ok(bosn_registry::ImageCreationIntent {
            reference: intent.reference,
            content_sha256: intent.setup_content_sha256,
            workspace: intent.workspace_root.to_string_lossy().into_owned(),
            stack: stack.into(),
            owner,
            source: match intent.kind {
                bosn_setup::PreparedImageKind::PinnedImage { .. } => {
                    bosn_registry::ImageIntentSource::Pull
                }
                bosn_setup::PreparedImageKind::InlineDockerfile { .. } => {
                    bosn_registry::ImageIntentSource::Build
                }
            },
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_secs_f64(),
        })
    }

    pub(crate) fn resource(
        &self,
        prepared: &PreparedImage,
        workspace: &str,
    ) -> SetupEnsureImageResource {
        // `prepared.observed_identity` is accepted only from `prepare_setup_image`, which
        // validates Docker's canonical sha256 image ID.  Do not use the document
        // reference here: references/tags are not durable resource identities.
        let (namespace, stack) = match self {
            Self::Setup => ("setup-image", "setup"),
            Self::Manifest(stack) => ("manifest-image", *stack),
        };
        let logical_identity = format!("{namespace}:{}", prepared.observed_identity);
        SetupEnsureImageResource {
            id: logical_identity.clone(),
            name: logical_identity,
            stack: stack.into(),
            generation: prepared.observed_identity.clone(),
            workspace: workspace.into(),
        }
    }
}
