//! Docker executors for setup prepare and setup ensure.

use super::*;

#[derive(Clone)]
pub struct DockerSetupPrepareExecutor {
    pub(crate) state_dir: PathBuf,
    pub(crate) engine: DockerEngine,
}
impl DockerSetupPrepareExecutor {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            engine: DockerEngine::docker(),
        }
    }
}
impl SetupPrepareExecutor for DockerSetupPrepareExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupPrepareRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let deadline = async_engine::Deadline::after(request.deadline);
            let plan = async_engine::cancellable(
                cancellation,
                async_engine::timeout_at(
                    deadline,
                    plan_setup(SetupPlanRequest {
                        state_dir: self.state_dir.clone(),
                        workspace: request.workspace,
                        locator: request.config,
                        policy: request.policy.acquire_policy(),
                    }),
                ),
            )
            .await
            .map_err(|_| "setup preparation cancelled".to_owned())?
            .map_err(|_| "setup planning exceeded its deadline".to_owned())?
            .map_err(|error| error.to_string())?;
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err("setup preparation exceeded its deadline".into());
            }
            let (events, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
            let forwarded_logs = logs.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(event) = receiver.recv().await {
                    forward_engine_event(&forwarded_logs, event).await?;
                }
                Ok::<(), String>(())
            });
            let result = prepare_setup_image(
                &self.engine,
                &plan,
                RunOptions::streaming(remaining, request.output_limit),
                cancellation,
                &events,
            )
            .await;
            drop(events);
            forwarder
                .await
                .map_err(|_| "setup log forwarder stopped".to_owned())??;
            let prepared = result.map_err(|error| error.to_string())?;
            Ok(format!(
                "prepared {} as {}",
                prepared.reference, prepared.observed_identity
            ))
        })
    }
}

/// Docker-backed implementation for the bounded plan, prepare, and
/// create-if-absent/start-if-stopped application ensure pipeline.
#[derive(Clone)]
pub struct DockerSetupEnsureExecutor {
    pub(crate) state_dir: PathBuf,
    pub(crate) engine: DockerEngine,
}
impl DockerSetupEnsureExecutor {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            engine: DockerEngine::docker(),
        }
    }
}
impl SetupEnsureExecutor for DockerSetupEnsureExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupEnsureJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        Box::pin(async move {
            // The two engine stages receive disjoint portions of one caller
            // budget. This is intentionally stricter than letting both stages
            // each consume the full limit; a tiny budget is syntactically
            // valid on the wire but cannot safely fund both stages.
            let prepare_output = request.output_limit / 2;
            let ensure_output = request.output_limit.saturating_sub(prepare_output);
            if prepare_output == 0 || ensure_output == 0 {
                return Err("setup ensure output budget cannot fund preparation and ensure".into());
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
            .map_err(|_| "setup ensure cancelled".to_owned())?
            .map_err(|_| "setup ensure planning exceeded its deadline".to_owned())?
            .map_err(|error| error.to_string())?;
            let (events, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
            let forwarded_logs = logs.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(event) = receiver.recv().await {
                    forward_engine_event(&forwarded_logs, event).await?;
                }
                Ok::<(), String>(())
            });
            let pipeline = SetupEnsurePipeline {
                plan: &plan,
                workspace: request.workspace,
                deadline: &deadline,
                prepare_output,
                ensure_output,
            };
            let result =
                execute_setup_ensure_pipeline(&self.engine, &pipeline, cancellation, &events, logs)
                    .await;
            drop(events);
            forwarder
                .await
                .map_err(|_| "setup log forwarder stopped".to_owned())??;
            let result = result?;
            let prepared = result.prepared;
            let ensured = result.ensured;
            // This is redundant with `ensure_setup_app`'s validation, but it
            // keeps the registry boundary fail-closed if a future core change
            // ever returns a result not tied to the inspected preparation.
            if ensured.image_identity != prepared.observed_identity {
                return Err("setup ensure result image does not match prepared image".into());
            }
            Ok(SetupEnsureExecution {
                receipt: format!(
                    "ensured {} as {}",
                    ensured.container_name, ensured.container_id
                ),
                resource: SetupEnsureResource {
                    // Docker can assign a different opaque ID if a managed
                    // container was externally removed. The content-derived
                    // name is the validated logical identity and keeps this
                    // upsert idempotent across that recovery case.
                    id: setup_container_resource_id(
                        "setup-container",
                        "setup",
                        &ensured.container_name,
                    ),
                    name: ensured.container_name,
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
