//! Small, authenticated Rust daemon foundation. Product protobuf remains private.

use bosn_core::{ResourceKind, ResourceState, Retention, Scope};
use bosn_engine::{DockerEngine, EngineEvent, RunOptions};
use bosn_registry::{Registry, RegistryStatus, Resource, ResourceUse};
#[cfg(test)]
use bosn_setup::PreparedImageKind;
use bosn_setup::{
    PreparedImage, SetupAcquirePolicy, SetupEnsureEngine,
    SetupEnsureRequest as CoreSetupEnsureRequest, SetupImageEngine, SetupPlan, SetupPlanRequest,
    SetupTaskRequest, ensure_setup_app, execute_setup_task, plan_setup, prepare_setup_image,
};
use jobs::{Jobs, Submission};
use kernal_api::{
    async_engine::{self, CancellationSource},
    daemon_frame_v1::{
        DaemonFrame, DaemonFrameCodec, DaemonFrameDecode, DaemonFrameKind, DaemonPayloadEncoding,
    },
    platform::ipc::{self, AsyncListener, AsyncStream, Endpoint, EndpointAddressCandidates},
};
use prost::Message;
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub mod jobs;
pub mod mcp;

pub const PROTOCOL_VERSION: u32 = 1;
const PAYLOAD_PROTOCOL: u32 = 0x4253_4e01;
const MAX_FRAME: usize = 1024 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(3);
const SETUP_PREPARE_MAX_DEADLINE: Duration = Duration::from_secs(5 * 60);
const SETUP_PREPARE_MAX_OUTPUT: usize = 8 * 1024 * 1024;
const SETUP_PREPARE_COMMAND_QUEUE: usize = 64;
const SETUP_PREPARE_EVENT_QUEUE: usize = 16;

/// Explicit policy for one daemon-owned setup image preparation request.
/// State is selected by [`Client::for_state`] and then owned by the daemon;
/// callers cannot substitute a state root in the RPC itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupPreparePolicy {
    Refresh,
    Offline,
}
impl SetupPreparePolicy {
    fn wire(self) -> u32 {
        match self {
            Self::Refresh => 1,
            Self::Offline => 2,
        }
    }
    fn from_wire(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Refresh),
            2 => Some(Self::Offline),
            _ => None,
        }
    }
    fn acquire_policy(self) -> SetupAcquirePolicy {
        match self {
            Self::Refresh => SetupAcquirePolicy::OnlineRefresh,
            Self::Offline => SetupAcquirePolicy::OfflineCacheOnly,
        }
    }
}

/// All immutable caller inputs for a semantic setup-image job.  This is not
/// Docker argv and has no container, mount, or task-execution controls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupPrepareRequest {
    pub workspace: PathBuf,
    pub config: String,
    pub policy: SetupPreparePolicy,
    pub deadline: Duration,
    pub output_limit: usize,
}

/// All immutable caller inputs for one daemon-owned setup task.  The task's
/// command, mounts, image, environment, and working directory remain solely
/// in the validated setup document; this request can select only one declared
/// task by name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupTaskJobRequest {
    pub workspace: PathBuf,
    pub config: String,
    pub policy: SetupPreparePolicy,
    pub task_name: String,
    pub deadline: Duration,
    pub output_limit: usize,
}

/// All immutable caller inputs for one daemon-owned setup application ensure.
/// The application name, image, command, mounts, labels, environment, and
/// engine options are all derived from the validated setup document and its
/// prepared-image receipt. Callers cannot select any of them here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureJobRequest {
    pub workspace: PathBuf,
    pub config: String,
    pub policy: SetupPreparePolicy,
    pub deadline: Duration,
    pub output_limit: usize,
}

/// Testable daemon execution boundary.  Production uses
/// [`DockerSetupPrepareExecutor`]; tests can provide a deterministic runner
/// without starting Docker.  Log records are bounded by the actor before they
/// reach IPC.
pub trait SetupPrepareExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: SetupPrepareRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

/// Testable daemon boundary for the complete plan, prepare, and declared-task
/// pipeline. Production uses [`DockerSetupTaskExecutor`].  It deliberately
/// receives no Docker controls: only the bounded semantic request above.
pub trait SetupTaskExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: SetupTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

/// Testable daemon boundary for the complete plan, image-prepare, and
/// ownership-safe application ensure pipeline. Production uses
/// [`DockerSetupEnsureExecutor`]. It exposes no container lifecycle controls:
/// the core primitive may only create an absent container or start a matching
/// stopped one, and refuses every other existing candidate.
pub trait SetupEnsureExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: SetupEnsureJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>>;
}

/// A validated fact to be durably recorded after a successful setup-app
/// ensure. Production constructs this only after plan, image preparation, and
/// ownership-safe ensure all succeed. It is public solely because the
/// executor trait is a cross-crate test seam; it is not an RPC request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureExecution {
    pub receipt: String,
    /// The managed application container fact.
    pub resource: SetupEnsureResource,
    /// The image fact verified during the same successful ensure pipeline.
    /// This is executor output, never an RPC-controlled image selector.
    pub image: SetupEnsureImageResource,
}

/// Logical identity facts for a daemon-owned setup container. The registry
/// actor supplies timestamps, state, and retention rather than accepting them
/// from the executor or an RPC caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureResource {
    pub id: String,
    pub name: String,
    pub stack: String,
    pub generation: String,
    pub workspace: String,
}

/// Logical identity facts for a prepared application image used by a
/// daemon-owned setup ensure. `generation` is the inspected Docker image ID,
/// while `id` and `name` are derived from that content address rather than
/// from a caller-supplied reference or tag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureImageResource {
    pub id: String,
    pub name: String,
    pub stack: String,
    pub generation: String,
    pub workspace: String,
}

#[derive(Clone)]
pub struct DockerSetupPrepareExecutor {
    state_dir: PathBuf,
    engine: DockerEngine,
}
impl DockerSetupPrepareExecutor {
    fn new(state_dir: PathBuf) -> Self {
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
    state_dir: PathBuf,
    engine: DockerEngine,
}
impl DockerSetupEnsureExecutor {
    fn new(state_dir: PathBuf) -> Self {
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
                    id: format!("setup-container:{}", plan.content_sha256),
                    name: ensured.container_name,
                    stack: "setup".into(),
                    generation: format!("sha256:{}", plan.content_sha256),
                    workspace: plan.workspace_root.to_string_lossy().into_owned(),
                },
                image: setup_ensure_image_resource(
                    &prepared,
                    &plan.workspace_root.to_string_lossy(),
                ),
            })
        })
    }
}

/// Execute already-planned app preparation and ensure under one absolute
/// deadline and one aggregate output budget. This generic helper preserves
/// the finite core engine commands for deterministic no-Docker tests.
struct SetupEnsurePipeline<'a> {
    plan: &'a SetupPlan,
    workspace: PathBuf,
    deadline: &'a async_engine::Deadline,
    prepare_output: usize,
    ensure_output: usize,
}

struct PreparedSetupEnsure {
    prepared: PreparedImage,
    ensured: bosn_setup::SetupEnsureResult,
}

async fn execute_setup_ensure_pipeline<E: SetupImageEngine + SetupEnsureEngine>(
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

fn setup_ensure_image_resource(
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

/// Docker-backed implementation for a single setup task.  The daemon plans,
/// prepares, and then invokes the task primitive itself; no front end can
/// insert an arbitrary container operation between those stages.
#[derive(Clone)]
pub struct DockerSetupTaskExecutor {
    state_dir: PathBuf,
    engine: DockerEngine,
}
impl DockerSetupTaskExecutor {
    fn new(state_dir: PathBuf) -> Self {
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
        logs: &'a async_engine::Sender<String>,
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
            let prepared = prepare_setup_image(
                &self.engine,
                &plan,
                RunOptions::streaming(remaining, prepare_output),
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

async fn forward_engine_event(
    logs: &async_engine::Sender<String>,
    event: EngineEvent,
) -> Result<(), String> {
    let (stream, bytes) = match event {
        EngineEvent::Stdout(bytes) => ("stdout", bytes),
        EngineEvent::Stderr(bytes) => ("stderr", bytes),
    };
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub registry_id: String,
    pub schema_version: u32,
    pub resources: u64,
    pub leases: u64,
    pub sessions: u64,
    pub reconciliation_required: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStatus {
    pub id: u64,
    pub state: String,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobLogRecord {
    pub cursor: u64,
    pub line: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobLogPage {
    pub retained_from: u64,
    pub next: u64,
    pub gap: bool,
    pub records: Vec<JobLogRecord>,
}
impl From<RegistryStatus> for Status {
    fn from(v: RegistryStatus) -> Self {
        Self {
            registry_id: v.registry_id,
            schema_version: v.schema_version,
            resources: v.resources,
            leases: v.leases,
            sessions: v.sessions,
            reconciliation_required: v.reconciliation_required,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    state_dir: PathBuf,
}
impl Client {
    pub fn for_state(state: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            state_dir: state.as_ref().to_path_buf(),
        })
    }
    pub async fn ping(&self) -> Result<(), Error> {
        match self.call(Request::operation(1)).await? {
            Reply::Pong => Ok(()),
            _ => Err(Error::Protocol("unexpected ping response")),
        }
    }
    pub async fn status(&self) -> Result<Status, Error> {
        match self.call(Request::operation(2)).await? {
            Reply::Status(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected status response")),
        }
    }
    pub async fn shutdown(&self) -> Result<(), Error> {
        match self.call(Request::operation(3)).await? {
            Reply::Shutdown => Ok(()),
            _ => Err(Error::Protocol("unexpected shutdown response")),
        }
    }
    pub async fn submit_job(
        &self,
        workspace: &str,
        stack: &str,
        digest: &str,
    ) -> Result<u64, Error> {
        match self
            .call(Request {
                protocol_version: PROTOCOL_VERSION,
                operation: 4,
                workspace: workspace.into(),
                stack: stack.into(),
                digest: digest.into(),
                job_id: 0,
                log_after: 0,
                log_limit: 0,
                setup_config: String::new(),
                setup_policy: 0,
                setup_deadline_ms: 0,
                setup_output_limit: 0,
                setup_task_name: String::new(),
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected submit response")),
        }
    }
    pub async fn job_status(&self, id: u64) -> Result<JobStatus, Error> {
        match self
            .call(Request {
                job_id: id,
                ..Request::operation(5)
            })
            .await?
        {
            Reply::JobStatus(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected job status response")),
        }
    }
    pub async fn cancel_job(&self, id: u64) -> Result<(), Error> {
        match self
            .call(Request {
                job_id: id,
                ..Request::operation(6)
            })
            .await?
        {
            Reply::Cancelled => Ok(()),
            _ => Err(Error::Protocol("unexpected job cancel response")),
        }
    }
    pub async fn job_logs(&self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error> {
        match self
            .call(Request {
                job_id: id,
                log_after: after,
                log_limit: limit,
                ..Request::operation(7)
            })
            .await?
        {
            Reply::JobLogs(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected job logs response")),
        }
    }
    /// Submit a daemon-owned plan-and-image-prepare operation. The response is
    /// only the durable job ID; Docker work happens after the authenticated
    /// reply and is observed through status/log polling.
    pub async fn submit_setup_prepare(&self, request: SetupPrepareRequest) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("setup workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_prepare_wire(
            &workspace,
            &request.config,
            request.policy,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(8)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected setup prepare response")),
        }
    }
    /// Submit one daemon-owned plan, image-prepare, and declared-task job.
    /// The response is the durable job ID; callers observe the bounded work
    /// through the existing status/log/cancel methods.
    pub async fn submit_setup_task(&self, request: SetupTaskJobRequest) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("setup workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_task_wire(
            &workspace,
            &request.config,
            request.policy,
            &request.task_name,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_task_name: request.task_name,
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(9)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected setup task response")),
        }
    }
    /// Submit one daemon-owned plan, image-prepare, and ownership-safe
    /// application ensure job. The returned ID can only be observed with the
    /// existing bounded status/log/cancel APIs.
    pub async fn submit_setup_ensure(&self, request: SetupEnsureJobRequest) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("setup workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_ensure_wire(
            &workspace,
            &request.config,
            request.policy,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(10)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected setup ensure response")),
        }
    }
    async fn call(&self, request: Request) -> Result<Reply, Error> {
        // Resolve on every call: a Client may have been constructed while a
        // fresh daemon was still creating its registry, before an inode-based
        // alias-stable endpoint name existed.
        let ep = endpoint(&self.state_dir)?;
        let mut stream = async_engine::timeout(IO_DEADLINE, AsyncStream::connect(&ep))
            .await
            .map_err(|_| Error::Deadline)??;
        if !peer_is_authorized(&stream.peer_identity()?.user_id, &ipc::current_user_id()?) {
            return Err(Error::Unauthorized);
        }
        let mut payload = Vec::new();
        request
            .encode(&mut payload)
            .map_err(|_| Error::Protocol("encode"))?;
        write_frame(
            &mut stream,
            DaemonFrame::request(PAYLOAD_PROTOCOL, payload).with_request_id(1),
        )
        .await?;
        let frame = read_frame(&mut stream).await?;
        decode_response_frame(frame, 1)
    }
}

pub struct Service {
    state_dir: PathBuf,
    stop: CancellationSource,
    setup_executor: Arc<dyn SetupPrepareExecutor>,
    setup_task_executor: Arc<dyn SetupTaskExecutor>,
    setup_ensure_executor: Arc<dyn SetupEnsureExecutor>,
}

#[cfg(test)]
struct SetupEnsureRecordGate {
    entered: async_engine::Sender<()>,
    release: async_engine::Receiver<()>,
}
#[derive(Clone)]
struct RegistryActor {
    sender: async_engine::Sender<DbCommand>,
}
enum DbCommand {
    Status(async_engine::OneshotSender<Result<Status, Error>>),
    RecordSetupEnsure {
        execution: Box<SetupEnsureExecution>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    Stop(async_engine::OneshotSender<()>),
}
#[derive(Clone)]
struct JobActor {
    sender: async_engine::Sender<JobCommand>,
}
enum JobCommand {
    Submit {
        workspace: String,
        stack: String,
        digest: String,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    Status {
        id: u64,
        reply: async_engine::OneshotSender<Result<jobs::Job, Error>>,
    },
    Cancel {
        id: u64,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    Logs {
        id: u64,
        after: u64,
        limit: usize,
        reply: async_engine::OneshotSender<Result<jobs::LogPage, Error>>,
    },
    SubmitSetupPrepare {
        request: SetupPrepareRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitSetupTask {
        request: SetupTaskJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitSetupEnsure {
        request: SetupEnsureJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    /// The job actor, rather than an executor task, owns the transition from
    /// a cancellable running job to a durably recorded successful ensure.
    /// It deliberately awaits the registry transaction before it processes a
    /// later Cancel command, then settles the job before replying.
    PersistSetupEnsure {
        id: u64,
        execution: SetupEnsureExecution,
        reply: async_engine::OneshotSender<Result<(), String>>,
    },
    Log {
        id: u64,
        line: String,
    },
    Completed {
        id: u64,
        kind: SetupJobKind,
        result: Result<String, String>,
    },
    Stop(async_engine::OneshotSender<()>),
}

#[derive(Clone, Copy)]
enum SetupJobKind {
    Prepare,
    Task,
    Ensure,
}

enum SetupJobRequest {
    Prepare(SetupPrepareRequest),
    Task(SetupTaskJobRequest),
    Ensure(SetupEnsureJobRequest),
}

#[derive(Clone)]
struct SetupExecutors {
    prepare: Arc<dyn SetupPrepareExecutor>,
    task: Arc<dyn SetupTaskExecutor>,
    ensure: Arc<dyn SetupEnsureExecutor>,
}
impl JobActor {
    async fn submit(&self, workspace: String, stack: String, digest: String) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::Submit {
                workspace,
                stack,
                digest,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn status(&self, id: u64) -> Result<jobs::Job, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::Status { id, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn cancel(&self, id: u64) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::Cancel { id, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn logs(&self, id: u64, after: u64, limit: usize) -> Result<jobs::LogPage, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::Logs {
                id,
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn submit_setup_prepare(&self, request: SetupPrepareRequest) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitSetupPrepare { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn submit_setup_task(&self, request: SetupTaskJobRequest) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitSetupTask { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn submit_setup_ensure(&self, request: SetupEnsureJobRequest) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitSetupEnsure { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn stop(&self) {
        let (reply, wait) = async_engine::oneshot_channel();
        if self.sender.send(JobCommand::Stop(reply)).await.is_ok() {
            let _ = wait.await;
        }
    }
}
async fn job_actor(
    mut jobs: Jobs,
    mut receiver: async_engine::Receiver<JobCommand>,
    executors: SetupExecutors,
    sender: async_engine::Sender<JobCommand>,
    registry: RegistryActor,
) {
    let mut requests: BTreeMap<u64, SetupJobRequest> = BTreeMap::new();
    let mut cancellations: BTreeMap<u64, CancellationSource> = BTreeMap::new();
    let mut tasks = async_engine::TaskGroup::new();
    let mut stopping = None;
    while let Some(command) = receiver.recv().await {
        while matches!(
            async_engine::timeout(Duration::ZERO, tasks.join_next()).await,
            Ok(Some(_))
        ) {}
        match command {
            JobCommand::Submit {
                workspace,
                stack,
                digest,
                reply,
            } => {
                let result = jobs
                    .submit(&workspace, &stack, &digest)
                    .map(|s| match s {
                        Submission::Started(id)
                        | Submission::Queued(id)
                        | Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => replacement,
                    })
                    .map_err(|_| Error::Protocol("job admission"));
                let _ = reply.send(result);
            }
            JobCommand::Status { id, reply } => {
                let _ = reply.send(jobs.job(id).map_err(|_| Error::Protocol("unknown job")));
            }
            JobCommand::Cancel { id, reply } => {
                let result = jobs.cancel(id).map_err(|_| Error::Protocol("job cancel"));
                if result.is_ok() {
                    if let Some(cancellation) = cancellations.get(&id) {
                        cancellation.cancel();
                    } else if jobs.job(id).is_ok_and(|job| job.state.terminal()) {
                        requests.remove(&id);
                    }
                }
                let _ = reply.send(result);
            }
            JobCommand::Logs {
                id,
                after,
                limit,
                reply,
            } => {
                let _ = reply.send(
                    jobs.log_page(id, after, limit)
                        .map_err(|_| Error::Protocol("unknown job")),
                );
            }
            JobCommand::SubmitSetupPrepare { request, reply } => {
                let digest = setup_prepare_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = jobs
                    .submit(&workspace, "setup-prepare", &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::Prepare(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::Prepare(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("setup job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                );
            }
            JobCommand::SubmitSetupTask { request, reply } => {
                let digest = setup_task_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = jobs
                    .submit(&workspace, "setup-task", &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::Task(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::Task(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("setup task job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                );
            }
            JobCommand::SubmitSetupEnsure { request, reply } => {
                let digest = setup_ensure_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = jobs
                    .submit(&workspace, "setup-ensure", &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::Ensure(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::Ensure(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("setup ensure job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                );
            }
            JobCommand::PersistSetupEnsure {
                id,
                execution,
                reply,
            } => {
                let result = if jobs
                    .job(id)
                    .is_ok_and(|job| job.state == jobs::JobState::Running)
                {
                    let receipt = execution.receipt.clone();
                    registry
                        .record_setup_ensure(execution)
                        .await
                        .map_err(|error| format!("setup ensure registry recording failed: {error}"))
                        .map(|()| {
                            // Settle before accepting another command. A
                            // cancellation processed before this command has
                            // already changed the state to Cancelling and is
                            // rejected above; a later cancellation observes a
                            // terminal success and cannot be accepted.
                            cancellations.remove(&id);
                            let _ = jobs.log(id, bounded_log_line(&receipt));
                            let _ = jobs.settle_with_error(id, true, None);
                        })
                } else {
                    Err("setup ensure cancelled".into())
                };
                let _ = reply.send(result);
            }
            JobCommand::Log { id, line } => {
                // A full log record is never permitted to block daemon IPC;
                // bounded engine output instead applies back-pressure upstream.
                let _ = jobs.log(id, bounded_log_line(&line));
            }
            JobCommand::Completed { id, kind, result } => {
                cancellations.remove(&id);
                match result {
                    Ok(receipt) => {
                        let _ = jobs.log(id, bounded_log_line(&receipt));
                        let _ = jobs.settle_with_error(id, true, None);
                    }
                    Err(error) => {
                        let error = bounded_log_line(&error);
                        let operation = match kind {
                            SetupJobKind::Prepare => "setup prepare",
                            SetupJobKind::Task => "setup task",
                            SetupJobKind::Ensure => "setup ensure",
                        };
                        let _ = jobs.log(id, format!("{operation} failed: {error}"));
                        let _ = jobs.settle_with_error(id, false, Some(error));
                    }
                }
            }
            JobCommand::Stop(reply) => {
                jobs.close();
                for (&id, cancellation) in &cancellations {
                    // Preserve cancellation semantics in durable status while
                    // the executor owns direct-child reaping.
                    let _ = jobs.cancel(id);
                    cancellation.cancel();
                }
                stopping = Some(reply);
            }
        }
        // The completion path may have freed a slot. Launching only here
        // makes task ownership explicit and preserves the scheduler cap.
        if stopping.is_none() {
            launch_started_setup_jobs(
                &mut jobs,
                &mut requests,
                &mut cancellations,
                &mut tasks,
                &executors,
                sender.clone(),
            );
        }
        if stopping.is_some() && cancellations.is_empty() {
            while tasks.join_next().await.is_some() {}
            if let Some(reply) = stopping.take() {
                let _ = reply.send(());
            }
            return;
        }
    }
}

fn launch_started_setup_jobs(
    jobs: &mut Jobs,
    requests: &mut BTreeMap<u64, SetupJobRequest>,
    cancellations: &mut BTreeMap<u64, CancellationSource>,
    tasks: &mut async_engine::TaskGroup<()>,
    executors: &SetupExecutors,
    sender: async_engine::Sender<JobCommand>,
) {
    for id in jobs.take_started() {
        // Legacy generic jobs have no daemon executor.  Only the new semantic
        // operation may enter this branch, so no arbitrary command can escape
        // the typed setup boundary.
        let Some(request) = requests.remove(&id) else {
            continue;
        };
        let cancellation = CancellationSource::new();
        let token = cancellation.token();
        cancellations.insert(id, cancellation);
        let task_sender = sender.clone();
        let prepare_executor = Arc::clone(&executors.prepare);
        let task_executor = Arc::clone(&executors.task);
        let ensure_executor = Arc::clone(&executors.ensure);
        tasks.spawn(async move {
            let (logs, mut log_receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
            let log_sender = task_sender.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(line) = log_receiver.recv().await {
                    if log_sender.send(JobCommand::Log { id, line }).await.is_err() {
                        return;
                    }
                }
            });
            let completion = match request {
                SetupJobRequest::Prepare(request) => Some((
                    SetupJobKind::Prepare,
                    prepare_executor.execute(request, &token, &logs).await,
                )),
                SetupJobRequest::Task(request) => Some((
                    SetupJobKind::Task,
                    task_executor.execute(request, &token, &logs).await,
                )),
                SetupJobRequest::Ensure(request) => {
                    let result = ensure_executor.execute(request, &token, &logs).await;
                    match result {
                        Ok(execution) => {
                            let (reply, wait) = async_engine::oneshot_channel();
                            let persisted = if task_sender
                                .send(JobCommand::PersistSetupEnsure {
                                    id,
                                    execution,
                                    reply,
                                })
                                .await
                                .is_err()
                            {
                                Err("setup ensure registry actor stopped".to_owned())
                            } else {
                                match wait.await {
                                    Ok(result) => result,
                                    Err(_) => Err("setup ensure registry actor stopped".to_owned()),
                                }
                            };
                            persisted
                                .err()
                                .map(|error| (SetupJobKind::Ensure, Err(error)))
                        }
                        Err(error) => Some((SetupJobKind::Ensure, Err(error))),
                    }
                }
            };
            drop(logs);
            let _ = forwarder.await;
            if let Some((kind, result)) = completion {
                let _ = task_sender
                    .send(JobCommand::Completed { id, kind, result })
                    .await;
            }
        });
    }
}

fn bounded_log_line(value: &str) -> String {
    if value.len() <= jobs::MAX_LOG_LINE_BYTES {
        return value.to_owned();
    }
    let mut end = jobs::MAX_LOG_LINE_BYTES.saturating_sub(3);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = value[..end].to_owned();
    bounded.push_str("...");
    bounded
}

fn setup_prepare_digest(request: &SetupPrepareRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let policy = request.policy.wire().to_le_bytes();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.setup-prepare.v1".as_slice(),
        workspace.as_bytes(),
        request.config.as_bytes(),
        &policy,
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "setup:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}

fn setup_task_digest(request: &SetupTaskJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let policy = request.policy.wire().to_le_bytes();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.setup-task.v1".as_slice(),
        workspace.as_bytes(),
        request.config.as_bytes(),
        &policy,
        request.task_name.as_bytes(),
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "setup:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}

fn setup_ensure_digest(request: &SetupEnsureJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let policy = request.policy.wire().to_le_bytes();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.setup-ensure.v1".as_slice(),
        workspace.as_bytes(),
        request.config.as_bytes(),
        &policy,
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "setup:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}
impl RegistryActor {
    async fn status(&self) -> Result<Status, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::Status(reply))
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn record_setup_ensure(&self, execution: SetupEnsureExecution) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RecordSetupEnsure {
                execution: Box::new(execution),
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn stop(&self) {
        let (reply, wait) = async_engine::oneshot_channel();
        if self.sender.send(DbCommand::Stop(reply)).await.is_ok() {
            let _ = wait.await;
        }
    }
}
async fn registry_actor(
    mut registry: Registry,
    mut receiver: async_engine::Receiver<DbCommand>,
    #[cfg(test)] mut setup_ensure_record_gate: Option<SetupEnsureRecordGate>,
) {
    while let Some(command) = receiver.recv().await {
        match command {
            DbCommand::Status(reply) => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.status().map(Status::from);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::RecordSetupEnsure { execution, reply } => {
                #[cfg(test)]
                if let Some(gate) = &mut setup_ensure_record_gate
                    && (gate.entered.send(()).await.is_err() || gate.release.recv().await.is_none())
                {
                    let _ = reply.send(Err(Error::ActorClosed));
                    continue;
                }
                let worker = async_engine::launch_blocking(move || {
                    let result = record_setup_ensure(&mut registry, &execution);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::Stop(reply) => {
                let _ = reply.send(());
                return;
            }
        }
    }
}

fn record_setup_ensure(
    registry: &mut Registry,
    execution: &SetupEnsureExecution,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    let container = &execution.resource;
    let image = &execution.image;
    transaction.put_resource(&Resource {
        id: container.id.clone(),
        kind: ResourceKind::Container,
        name: container.name.clone(),
        stack: container.stack.clone(),
        generation: container.generation.clone(),
        // Setup app container names are machine-global and content-addressed.
        scope: Scope::Machine,
        workspace: container.workspace.clone(),
        created_at: now,
        last_used: now,
        state: ResourceState::Active,
        retention: Retention::Pinned,
    })?;
    transaction.put_resource_use(&ResourceUse {
        resource_id: container.id.clone(),
        workspace: container.workspace.clone(),
        stack: container.stack.clone(),
        generation: container.generation.clone(),
        last_used: now,
        state: ResourceState::Active,
    })?;
    transaction.put_resource(&Resource {
        id: image.id.clone(),
        kind: ResourceKind::Image,
        name: image.name.clone(),
        stack: image.stack.clone(),
        generation: image.generation.clone(),
        // The inspected image ID identifies a machine-local Docker image.
        scope: Scope::Machine,
        workspace: image.workspace.clone(),
        created_at: now,
        last_used: now,
        state: ResourceState::Active,
        retention: Retention::Pinned,
    })?;
    transaction.put_resource_use(&ResourceUse {
        resource_id: image.id.clone(),
        workspace: image.workspace.clone(),
        stack: image.stack.clone(),
        generation: image.generation.clone(),
        last_used: now,
        state: ResourceState::Active,
    })?;
    transaction.commit()
}
impl Service {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            setup_executor: Arc::new(DockerSetupPrepareExecutor::new(state_dir.clone())),
            setup_task_executor: Arc::new(DockerSetupTaskExecutor::new(state_dir.clone())),
            setup_ensure_executor: Arc::new(DockerSetupEnsureExecutor::new(state_dir.clone())),
            state_dir,
            stop: CancellationSource::new(),
        }
    }
    /// Substitute only the semantic setup executor. This is primarily an
    /// integration-test seam; production callers retain the Docker adapter.
    pub fn with_setup_prepare_executor(mut self, executor: Arc<dyn SetupPrepareExecutor>) -> Self {
        self.setup_executor = executor;
        self
    }
    /// Substitute only the complete semantic setup-task executor. This is an
    /// integration-test seam; it cannot add arbitrary Docker controls.
    pub fn with_setup_task_executor(mut self, executor: Arc<dyn SetupTaskExecutor>) -> Self {
        self.setup_task_executor = executor;
        self
    }
    /// Substitute only the complete semantic setup-ensure executor. This is a
    /// test seam; it does not add a caller-controlled container operation.
    pub fn with_setup_ensure_executor(mut self, executor: Arc<dyn SetupEnsureExecutor>) -> Self {
        self.setup_ensure_executor = executor;
        self
    }
    /// Foreground lifecycle: acquires the sole registry writer before binding.
    pub async fn serve(self) -> Result<(), Error> {
        ipc::ensure_owner_private_directory(&self.state_dir)?;
        let db = self.state_dir.join("registry.sqlite3");
        let registry = match kernal_api::platform::fs::path_identity(&db) {
            Ok(Some(_)) => async_engine::launch_blocking(move || Registry::open_writer(&db))
                .await
                .map_err(|_| Error::ActorClosed)??,
            Ok(None) => return Err(Error::Protocol("registry identity unavailable")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let bytes = kernal_api::random::SecureRandom::new(1, IO_DEADLINE)
                    .map_err(|_| Error::Random)?
                    .bytes(16)
                    .await
                    .map_err(|_| Error::Random)?;
                async_engine::launch_blocking(move || Registry::create_writer(&db, &uuid(&bytes)))
                    .await
                    .map_err(|_| Error::ActorClosed)??
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let ep = endpoint(&self.state_dir)?;
        if ep.target_exists()? {
            return Err(Error::EndpointOccupied(ep.display().into()));
        }
        let listener = AsyncListener::bind_owner_only(&ep)?;
        let (sender, receiver) = async_engine::channel(16);
        let actor = RegistryActor { sender };
        let (job_sender, job_receiver) = async_engine::channel(SETUP_PREPARE_COMMAND_QUEUE);
        let jobs = JobActor {
            sender: job_sender.clone(),
        };
        let job_worker = async_engine::launch(job_actor(
            Jobs::new(1),
            job_receiver,
            SetupExecutors {
                prepare: Arc::clone(&self.setup_executor),
                task: Arc::clone(&self.setup_task_executor),
                ensure: Arc::clone(&self.setup_ensure_executor),
            },
            job_sender.clone(),
            actor.clone(),
        ));
        let worker = async_engine::launch(registry_actor(
            registry,
            receiver,
            #[cfg(test)]
            None,
        ));
        let mut clients = async_engine::TaskGroup::new();
        while !self.stop.is_cancelled() {
            // TaskGroup retains completed tasks until collected.  Reap only
            // ready completions: awaiting one here would let 32 slow peers
            // prevent the accept loop from serving everyone else.
            while matches!(
                async_engine::timeout(Duration::ZERO, clients.join_next()).await,
                Ok(Some(_))
            ) {}
            let accepted =
                async_engine::timeout(Duration::from_millis(100), listener.accept()).await;
            let stream = match accepted {
                Ok(Ok(stream)) => stream,
                Ok(Err(_)) => {
                    // A persistent listener failure must not turn this foreground
                    // process into a hot loop.  The listener remains owned, so
                    // yield before observing it again.
                    async_engine::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                Err(_) => continue,
            };
            if clients.len() >= 32 {
                // Admission is bounded. Dropping this newly accepted stream is
                // intentional; do not wait for a slow peer to make capacity.
                continue;
            }
            let actor = actor.clone();
            let jobs = jobs.clone();
            let stop = self.stop.clone();
            clients.spawn(async move { handle(stream, actor, jobs, stop).await });
        }
        while clients.join_next().await.is_some() {}
        actor.stop().await;
        jobs.stop().await;
        drop(jobs);
        let _ = worker.await;
        let _ = job_worker.await;
        Ok(())
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Registry(bosn_registry::Error),
    Deadline,
    Unauthorized,
    Random,
    EndpointOccupied(String),
    ActorClosed,
    Protocol(&'static str),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<bosn_registry::Error> for Error {
    fn from(e: bosn_registry::Error) -> Self {
        Self::Registry(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

fn endpoint(state: &Path) -> Result<Endpoint, Error> {
    // A database inode is stable across spelling aliases (including the
    // Windows namespace rules supplied by kernal-api).  Before the database
    // exists, retain the supplied path spelling so two fresh state roots do
    // not collide.  The current user is always part of the namespace.
    let db = state.join("registry.sqlite3");
    let mut identity = ipc::current_user_id()?.into_bytes();
    identity.push(0);
    match kernal_api::platform::fs::path_identity(&db) {
        Ok(Some(file)) => {
            identity.extend_from_slice(&file.device.to_le_bytes());
            identity.extend_from_slice(&file.file.to_le_bytes());
        }
        Ok(None) => {
            identity.extend_from_slice(&kernal_api::platform::ipc::endpoint_scope_bytes(state));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            identity.extend_from_slice(&kernal_api::platform::ipc::endpoint_scope_bytes(state));
        }
        Err(error) => return Err(Error::Io(error)),
    }
    let name = format!(
        "com.zackees.bosn.{}",
        kernal_api::hash::blake3_bytes(&identity).to_hex()
    );
    let address = EndpointAddressCandidates::new(Some(name), Some(state.join("bosn-rs.sock")))
        .select()
        .ok_or(Error::Protocol("no local IPC transport"))?;
    Ok(Endpoint::new(address)?)
}
fn uuid(bytes: &[u8]) -> String {
    let mut b = [0_u8; 16];
    b.copy_from_slice(&bytes[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}
async fn handle(
    mut s: AsyncStream,
    actor: RegistryActor,
    jobs: JobActor,
    stop: CancellationSource,
) -> Result<(), Error> {
    if !peer_is_authorized(&s.peer_identity()?.user_id, &ipc::current_user_id()?) {
        return Err(Error::Unauthorized);
    }
    let f = read_frame(&mut s).await?;
    if f.payload_protocol() != PAYLOAD_PROTOCOL
        || f.kind_classification() != DaemonFrameKind::Request
        || f.payload_encoding_classification() != DaemonPayloadEncoding::None
    {
        return Err(Error::Protocol("request frame"));
    }
    let r = Request::decode(f.payload()).map_err(|_| Error::Protocol("request decode"))?;
    let reply = if r.protocol_version != PROTOCOL_VERSION {
        ReplyWire {
            code: 1,
            ..Default::default()
        }
    } else {
        match r.operation {
            1 => ReplyWire {
                code: 10,
                ..Default::default()
            },
            2 => {
                let status = actor.status().await?;
                ReplyWire {
                    code: 20,
                    registry_id: status.registry_id,
                    schema_version: status.schema_version,
                    resources: status.resources,
                    leases: status.leases,
                    sessions: status.sessions,
                    reconciliation_required: status.reconciliation_required,
                    ..Default::default()
                }
            }
            3 => {
                stop.cancel();
                ReplyWire {
                    code: 30,
                    ..Default::default()
                }
            }
            4 => match jobs.submit(r.workspace, r.stack, r.digest).await {
                Ok(job_id) => ReplyWire {
                    code: 40,
                    job_id,
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            5 => match jobs.status(r.job_id).await {
                Ok(job) => ReplyWire {
                    code: 50,
                    job_id: job.id,
                    job_state: format!("{:?}", job.state),
                    job_error: job.error.unwrap_or_default(),
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            6 => match jobs.cancel(r.job_id).await {
                Ok(()) => ReplyWire {
                    code: 60,
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            7 => match jobs.logs(r.job_id, r.log_after, r.log_limit as usize).await {
                Ok(page) => ReplyWire {
                    code: 70,
                    retained_from: page.retained_from,
                    next_log_cursor: page.next,
                    log_gap: page.gap,
                    logs: page
                        .records
                        .into_iter()
                        .map(|(cursor, line)| LogRecordWire { cursor, line })
                        .collect(),
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            8 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_prepare_wire(
                        &r.workspace,
                        &r.setup_config,
                        policy,
                        r.setup_deadline_ms,
                        r.setup_output_limit,
                    )
                    .ok()
                    .map(|()| SetupPrepareRequest {
                        workspace: PathBuf::from(r.workspace),
                        config: r.setup_config,
                        policy,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                });
                match request {
                    Some(request) => match jobs.submit_setup_prepare(request).await {
                        Ok(job_id) => ReplyWire {
                            code: 40,
                            job_id,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    },
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            9 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_task_wire(
                        &r.workspace,
                        &r.setup_config,
                        policy,
                        &r.setup_task_name,
                        r.setup_deadline_ms,
                        r.setup_output_limit,
                    )
                    .ok()
                    .map(|()| SetupTaskJobRequest {
                        workspace: PathBuf::from(r.workspace),
                        config: r.setup_config,
                        policy,
                        task_name: r.setup_task_name,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                });
                match request {
                    Some(request) => match jobs.submit_setup_task(request).await {
                        Ok(job_id) => ReplyWire {
                            code: 40,
                            job_id,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    },
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            10 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_ensure_request_wire(&r, policy)
                        .ok()
                        .map(|()| SetupEnsureJobRequest {
                            workspace: PathBuf::from(r.workspace),
                            config: r.setup_config,
                            policy,
                            deadline: Duration::from_millis(r.setup_deadline_ms),
                            output_limit: r.setup_output_limit as usize,
                        })
                });
                match request {
                    Some(request) => match jobs.submit_setup_ensure(request).await {
                        Ok(job_id) => ReplyWire {
                            code: 40,
                            job_id,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    },
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            _ => ReplyWire {
                code: 2,
                ..Default::default()
            },
        }
    };
    let mut p = Vec::new();
    reply
        .encode(&mut p)
        .map_err(|_| Error::Protocol("reply encode"))?;
    write_frame(&mut s, DaemonFrame::response_to(&f, p)).await
}
fn peer_is_authorized(peer_user_id: &str, expected_user_id: &str) -> bool {
    !peer_user_id.is_empty() && peer_user_id == expected_user_id
}
async fn read_frame(s: &mut AsyncStream) -> Result<DaemonFrame, Error> {
    let mut b = Vec::new();
    let deadline = async_engine::Deadline::after(IO_DEADLINE);
    loop {
        if b.len() > MAX_FRAME {
            return Err(Error::Protocol("frame too large"));
        }
        match DaemonFrameCodec::decode(&b).map_err(|_| Error::Protocol("bad frame"))? {
            DaemonFrameDecode::Frame { frame, consumed } if consumed <= MAX_FRAME => {
                return Ok(frame);
            }
            DaemonFrameDecode::Frame { .. } => return Err(Error::Protocol("frame too large")),
            DaemonFrameDecode::NeedMoreBytes => {
                let mut chunk = [0; 4096];
                let n = async_engine::timeout_at(deadline, s.read(&mut chunk))
                    .await
                    .map_err(|_| Error::Deadline)??;
                if n == 0 {
                    return Err(Error::Protocol("eof"));
                }
                b.extend_from_slice(&chunk[..n]);
            }
        }
    }
}
async fn write_frame(s: &mut AsyncStream, f: DaemonFrame) -> Result<(), Error> {
    let b = DaemonFrameCodec::encode(&f).map_err(|_| Error::Protocol("frame encode"))?;
    if b.len() > MAX_FRAME {
        return Err(Error::Protocol("frame too large"));
    }
    async_engine::timeout(IO_DEADLINE, s.write_all(&b))
        .await
        .map_err(|_| Error::Deadline)??;
    Ok(())
}
fn decode_response_frame(frame: DaemonFrame, request_id: u64) -> Result<Reply, Error> {
    if frame.request_id() != request_id
        || frame.kind_classification() != DaemonFrameKind::Response
        || frame.payload_protocol() != PAYLOAD_PROTOCOL
        || frame.payload_encoding_classification() != DaemonPayloadEncoding::None
    {
        return Err(Error::Protocol("response frame"));
    }
    let reply = ReplyWire::decode(frame.payload()).map_err(|_| Error::Protocol("reply decode"))?;
    decode_reply(reply)
}
#[derive(Message)]
struct Request {
    #[prost(uint32, tag = "1")]
    protocol_version: u32,
    #[prost(uint32, tag = "2")]
    operation: u32,
    #[prost(string, tag = "3")]
    workspace: String,
    #[prost(string, tag = "4")]
    stack: String,
    #[prost(string, tag = "5")]
    digest: String,
    #[prost(uint64, tag = "6")]
    job_id: u64,
    #[prost(uint64, tag = "7")]
    log_after: u64,
    #[prost(uint32, tag = "8")]
    log_limit: u32,
    #[prost(string, tag = "9")]
    setup_config: String,
    #[prost(uint32, tag = "10")]
    setup_policy: u32,
    #[prost(uint64, tag = "11")]
    setup_deadline_ms: u64,
    #[prost(uint32, tag = "12")]
    setup_output_limit: u32,
    #[prost(string, tag = "13")]
    setup_task_name: String,
}
impl Request {
    fn operation(operation: u32) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            operation,
            workspace: String::new(),
            stack: String::new(),
            digest: String::new(),
            job_id: 0,
            log_after: 0,
            log_limit: 0,
            setup_config: String::new(),
            setup_policy: 0,
            setup_deadline_ms: 0,
            setup_output_limit: 0,
            setup_task_name: String::new(),
        }
    }
}

fn validate_setup_prepare_wire(
    workspace: &str,
    config: &str,
    _policy: SetupPreparePolicy,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    const MAX_TEXT: usize = 8 * 1024;
    if workspace.is_empty()
        || workspace.len() > MAX_TEXT
        || config.is_empty()
        || config.len() > MAX_TEXT
        || workspace.bytes().any(|byte| byte == 0)
        || config.bytes().any(|byte| byte == 0)
    {
        return Err(Error::Protocol("invalid setup request text"));
    }
    let deadline = Duration::from_millis(deadline_ms);
    if deadline.is_zero() || deadline > SETUP_PREPARE_MAX_DEADLINE {
        return Err(Error::Protocol("invalid setup deadline"));
    }
    let output_limit = output_limit as usize;
    if output_limit == 0 || output_limit > SETUP_PREPARE_MAX_OUTPUT {
        return Err(Error::Protocol("invalid setup output limit"));
    }
    Ok(())
}

fn validate_setup_task_wire(
    workspace: &str,
    config: &str,
    policy: SetupPreparePolicy,
    task_name: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_setup_prepare_wire(workspace, config, policy, deadline_ms, output_limit)?;
    if task_name.is_empty()
        || task_name.len() > 64
        || !task_name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        })
        || !task_name.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(Error::Protocol("invalid setup task name"));
    }
    Ok(())
}

fn validate_setup_ensure_wire(
    workspace: &str,
    config: &str,
    policy: SetupPreparePolicy,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_setup_prepare_wire(workspace, config, policy, deadline_ms, output_limit)
}

/// The operation reuses the compact private protobuf envelope, but accepts no
/// legacy job or task fields. Rejecting rather than ignoring these values
/// makes the semantic surface exactly the five documented immutable inputs.
fn validate_setup_ensure_request_wire(
    request: &Request,
    policy: SetupPreparePolicy,
) -> Result<(), Error> {
    validate_setup_ensure_wire(
        &request.workspace,
        &request.setup_config,
        policy,
        request.setup_deadline_ms,
        request.setup_output_limit,
    )?;
    if !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_task_name.is_empty()
    {
        return Err(Error::Protocol("nonsemantic setup ensure fields"));
    }
    Ok(())
}
#[derive(Message)]
struct ReplyWire {
    #[prost(uint32, tag = "1")]
    code: u32,
    #[prost(string, tag = "2")]
    registry_id: String,
    #[prost(uint32, tag = "3")]
    schema_version: u32,
    #[prost(uint64, tag = "4")]
    resources: u64,
    #[prost(uint64, tag = "5")]
    leases: u64,
    #[prost(uint64, tag = "6")]
    sessions: u64,
    #[prost(bool, tag = "7")]
    reconciliation_required: bool,
    #[prost(uint64, tag = "8")]
    job_id: u64,
    #[prost(string, tag = "9")]
    job_state: String,
    #[prost(string, tag = "10")]
    job_error: String,
    #[prost(message, repeated, tag = "11")]
    logs: Vec<LogRecordWire>,
    #[prost(uint64, tag = "12")]
    retained_from: u64,
    #[prost(uint64, tag = "13")]
    next_log_cursor: u64,
    #[prost(bool, tag = "14")]
    log_gap: bool,
}
#[derive(Message)]
struct LogRecordWire {
    #[prost(uint64, tag = "1")]
    cursor: u64,
    #[prost(string, tag = "2")]
    line: String,
}
enum Reply {
    Pong,
    Status(Status),
    Shutdown,
    Job(u64),
    JobStatus(JobStatus),
    Cancelled,
    JobLogs(JobLogPage),
}
fn decode_reply(v: ReplyWire) -> Result<Reply, Error> {
    match v.code {
        10 => Ok(Reply::Pong),
        20 => Ok(Reply::Status(Status {
            registry_id: v.registry_id,
            schema_version: v.schema_version,
            resources: v.resources,
            leases: v.leases,
            sessions: v.sessions,
            reconciliation_required: v.reconciliation_required,
        })),
        30 => Ok(Reply::Shutdown),
        40 => Ok(Reply::Job(v.job_id)),
        50 => Ok(Reply::JobStatus(JobStatus {
            id: v.job_id,
            state: v.job_state,
            error: (!v.job_error.is_empty()).then_some(v.job_error),
        })),
        60 => Ok(Reply::Cancelled),
        70 => Ok(Reply::JobLogs(JobLogPage {
            retained_from: v.retained_from,
            next: v.next_log_cursor,
            gap: v.log_gap,
            records: v
                .logs
                .into_iter()
                .map(|record| JobLogRecord {
                    cursor: record.cursor,
                    line: record.line,
                })
                .collect(),
        })),
        1 => Err(Error::Protocol("unsupported protocol")),
        2 => Err(Error::Protocol("unknown operation")),
        _ => Err(Error::Protocol("daemon error")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::async_engine::RuntimeBuilder;
    use std::{
        collections::{BTreeMap, VecDeque},
        future::{Ready, ready},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

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
            logs: &'a async_engine::Sender<String>,
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
            logs: &'a async_engine::Sender<String>,
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
            logs: &'a async_engine::Sender<String>,
        ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>>
        {
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
                    for _ in 0..100 {
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
                })
            })
        }
    }

    struct PipelineFakeEngine {
        image_calls: Mutex<Vec<(bosn_setup::SetupImageCommand, RunOptions)>>,
        ensure_calls: Mutex<Vec<(bosn_setup::SetupEnsureCommand, RunOptions)>>,
        image_results:
            Mutex<VecDeque<Result<bosn_engine::CommandResult, bosn_engine::CommandError>>>,
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
        type StreamFuture<'a> =
            Ready<Result<bosn_engine::CommandResult, bosn_engine::CommandError>>;
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
    const TEST_CONTAINER_ID: &str =
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
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
        }
    }
    fn command_result(exit_code: i32, stdout: impl Into<Vec<u8>>) -> bosn_engine::CommandResult {
        bosn_engine::CommandResult {
            exit_code,
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn fresh_daemon_serves_typed_client_and_releases_writer() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let first = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            client.ping().await.unwrap();
            let before = client.status().await.unwrap();
            assert_eq!(before.schema_version, 5);
            assert!(!before.registry_id.is_empty());
            client.shutdown().await.unwrap();
            stopped(first).await;
            let second = async_engine::launch(Service::new(state.clone()).serve());
            let after = wait_for_client(&state).await.status().await.unwrap();
            assert_eq!(after.registry_id, before.registry_id);
            wait_for_client(&state).await.shutdown().await.unwrap();
            stopped(second).await;
        });
    }

    #[test]
    fn typed_job_submission_is_authenticated_and_coalesces() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(Service::new(state.clone()).serve());
                let client = wait_for_client(&state).await;
                let one = client
                    .submit_job("workspace", "stack", "sha256:abc")
                    .await
                    .unwrap();
                let two = client
                    .submit_job("workspace", "stack", "sha256:abc")
                    .await
                    .unwrap();
                assert_eq!(one, two);
                let status = client.job_status(one).await.unwrap();
                assert_eq!(status.id, one);
                assert_eq!(status.state, "Running");
                assert_eq!(
                    client.job_logs(one, 0, 16).await.unwrap(),
                    JobLogPage {
                        retained_from: 0,
                        next: 0,
                        gap: false,
                        records: Vec::new(),
                    }
                );
                client.cancel_job(one).await.unwrap();
                assert_eq!(client.job_status(one).await.unwrap().state, "Cancelling");
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn setup_prepare_job_is_prompt_coalesced_logged_and_cancellable_without_docker() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(SlowFakeSetupExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_prepare_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let request = SetupPrepareRequest {
                    workspace: workspace.clone(),
                    config: "https://example.invalid/setup.toml".into(),
                    policy: SetupPreparePolicy::Refresh,
                    deadline: Duration::from_secs(2),
                    output_limit: 4 * 1024,
                };
                let submitted = std::time::Instant::now();
                let first = client.submit_setup_prepare(request.clone()).await.unwrap();
                assert!(submitted.elapsed() < Duration::from_millis(250));
                assert_eq!(
                    first,
                    client.submit_setup_prepare(request.clone()).await.unwrap()
                );
                wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
                // Slow execution does not occupy the daemon request actor.
                client.ping().await.unwrap();
                let logs = wait_for_logs(&client, first).await;
                assert_eq!(logs.records[0].line, "[fake] preparation started");

                let changed = SetupPrepareRequest {
                    config: "https://example.invalid/other.toml".into(),
                    ..request
                };
                let second = client.submit_setup_prepare(changed).await.unwrap();
                assert_ne!(first, second);
                client.cancel_job(first).await.unwrap();
                wait_for_job_state(&client, first, "Cancelled").await;
                wait_for(|| fake.started.load(Ordering::SeqCst) == 2).await;
                client.cancel_job(second).await.unwrap();
                wait_for_job_state(&client, second, "Cancelled").await;
                assert_eq!(fake.cancelled.load(Ordering::SeqCst), 2);
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn setup_prepare_coalescing_digest_covers_every_immutable_input() {
        let base = SetupPrepareRequest {
            workspace: PathBuf::from("/workspace"),
            config: "https://example.invalid/setup.toml".into(),
            policy: SetupPreparePolicy::Refresh,
            deadline: Duration::from_secs(2),
            output_limit: 4 * 1024,
        };
        let variants = [
            SetupPrepareRequest {
                workspace: PathBuf::from("/other"),
                ..base.clone()
            },
            SetupPrepareRequest {
                config: "https://example.invalid/other.toml".into(),
                ..base.clone()
            },
            SetupPrepareRequest {
                policy: SetupPreparePolicy::Offline,
                ..base.clone()
            },
            SetupPrepareRequest {
                deadline: Duration::from_secs(3),
                ..base.clone()
            },
            SetupPrepareRequest {
                output_limit: 8 * 1024,
                ..base.clone()
            },
        ];
        for variant in variants {
            assert_ne!(setup_prepare_digest(&base), setup_prepare_digest(&variant));
        }
    }

    #[test]
    fn setup_task_job_is_prompt_coalesced_bounded_and_cancellable_without_docker() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(FakeSetupTaskExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_task_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let request = SetupTaskJobRequest {
                    workspace: workspace.clone(),
                    config: "https://example.invalid/setup.toml".into(),
                    policy: SetupPreparePolicy::Refresh,
                    task_name: "wait".into(),
                    deadline: Duration::from_secs(2),
                    output_limit: 4 * 1024,
                };
                let submitted = std::time::Instant::now();
                let first = client.submit_setup_task(request.clone()).await.unwrap();
                assert!(submitted.elapsed() < Duration::from_millis(250));
                assert_eq!(
                    first,
                    client.submit_setup_task(request.clone()).await.unwrap()
                );
                wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
                client.ping().await.unwrap();
                let logs = wait_for_logs(&client, first).await;
                assert!(
                    logs.records
                        .iter()
                        .all(|record| record.line.len() <= jobs::MAX_LOG_LINE_BYTES)
                );

                let changed = SetupTaskJobRequest {
                    task_name: "build".into(),
                    ..request
                };
                let second = client.submit_setup_task(changed).await.unwrap();
                assert_ne!(first, second);
                client.cancel_job(first).await.unwrap();
                wait_for_job_state(&client, first, "Cancelled").await;
                wait_for(|| fake.started.load(Ordering::SeqCst) == 2).await;
                wait_for_job_state(&client, second, "Succeeded").await;
                let completed_logs = client.job_logs(second, 0, 16).await.unwrap();
                assert!(
                    completed_logs
                        .records
                        .iter()
                        .any(|record| record.line.len() == jobs::MAX_LOG_LINE_BYTES)
                );
                assert_eq!(fake.cancelled.load(Ordering::SeqCst), 1);
                assert_eq!(
                    fake.stages(),
                    vec![
                        "plan:wait",
                        "prepare:wait",
                        "plan:build",
                        "prepare:build",
                        "task:build",
                    ]
                );
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn setup_task_stops_after_prepare_failure_before_running_task() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(FakeSetupTaskExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_task_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let job = client
                    .submit_setup_task(SetupTaskJobRequest {
                        workspace,
                        config: "https://example.invalid/setup.toml".into(),
                        policy: SetupPreparePolicy::Offline,
                        task_name: "prepare-fail".into(),
                        deadline: Duration::from_secs(2),
                        output_limit: 4 * 1024,
                    })
                    .await
                    .unwrap();
                wait_for_job_state(&client, job, "Failed").await;
                let logs = wait_for_logs(&client, job).await;
                assert!(
                    logs.records
                        .iter()
                        .any(|record| record.line.contains("setup task failed"))
                );
                assert_eq!(
                    fake.stages(),
                    vec!["plan:prepare-fail", "prepare:prepare-fail"]
                );
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn setup_task_coalescing_digest_covers_every_immutable_input() {
        let base = SetupTaskJobRequest {
            workspace: PathBuf::from("/workspace"),
            config: "https://example.invalid/setup.toml".into(),
            policy: SetupPreparePolicy::Refresh,
            task_name: "build".into(),
            deadline: Duration::from_secs(2),
            output_limit: 4 * 1024,
        };
        let variants = [
            SetupTaskJobRequest {
                workspace: PathBuf::from("/other"),
                ..base.clone()
            },
            SetupTaskJobRequest {
                config: "https://example.invalid/other.toml".into(),
                ..base.clone()
            },
            SetupTaskJobRequest {
                policy: SetupPreparePolicy::Offline,
                ..base.clone()
            },
            SetupTaskJobRequest {
                task_name: "test".into(),
                ..base.clone()
            },
            SetupTaskJobRequest {
                deadline: Duration::from_secs(3),
                ..base.clone()
            },
            SetupTaskJobRequest {
                output_limit: 8 * 1024,
                ..base.clone()
            },
        ];
        for variant in variants {
            assert_ne!(setup_task_digest(&base), setup_task_digest(&variant));
        }
    }

    #[test]
    fn setup_task_wire_validation_bounds_and_rejects_nonsemantic_names() {
        let valid = || {
            validate_setup_task_wire(
                "/workspace",
                "https://example.invalid/setup.toml",
                SetupPreparePolicy::Refresh,
                "build-1",
                1,
                1,
            )
        };
        assert!(valid().is_ok());
        for task_name in ["", "-build", "build/task", &"a".repeat(65)] {
            assert!(
                validate_setup_task_wire(
                    "/workspace",
                    "https://example.invalid/setup.toml",
                    SetupPreparePolicy::Offline,
                    task_name,
                    1,
                    1,
                )
                .is_err()
            );
        }
        for (deadline, output) in [(0, 1), (300_001, 1), (1, 0), (1, 8_388_609)] {
            assert!(
                validate_setup_task_wire(
                    "/workspace",
                    "https://example.invalid/setup.toml",
                    SetupPreparePolicy::Refresh,
                    "build",
                    deadline,
                    output,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn setup_ensure_job_is_prompt_coalesced_bounded_and_cancellable_without_docker() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(FakeSetupEnsureExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_ensure_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let request = SetupEnsureJobRequest {
                    workspace: workspace.clone(),
                    config: "https://example.invalid/wait.toml".into(),
                    policy: SetupPreparePolicy::Refresh,
                    deadline: Duration::from_secs(2),
                    output_limit: 4 * 1024,
                };
                let submitted = std::time::Instant::now();
                let first = client.submit_setup_ensure(request.clone()).await.unwrap();
                assert!(submitted.elapsed() < Duration::from_millis(250));
                assert_eq!(
                    first,
                    client.submit_setup_ensure(request.clone()).await.unwrap()
                );
                wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
                client.ping().await.unwrap();
                let logs = wait_for_logs(&client, first).await;
                assert!(
                    logs.records
                        .iter()
                        .all(|record| record.line.len() <= jobs::MAX_LOG_LINE_BYTES)
                );

                let changed = SetupEnsureJobRequest {
                    config: "https://example.invalid/other.toml".into(),
                    ..request
                };
                let second = client.submit_setup_ensure(changed).await.unwrap();
                assert_ne!(first, second);
                client.cancel_job(first).await.unwrap();
                wait_for_job_state(&client, first, "Cancelled").await;
                wait_for(|| fake.started.load(Ordering::SeqCst) == 2).await;
                wait_for_job_state(&client, second, "Succeeded").await;
                assert_eq!(fake.cancelled.load(Ordering::SeqCst), 1);
                assert_eq!(
                    fake.stages(),
                    vec![
                        "plan", "prepare", "ensure", "plan", "prepare", "ensure", "mutate"
                    ]
                );
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn setup_ensure_stops_after_prepare_failure_or_ownership_mismatch_without_mutation() {
        for config in [
            "https://example.invalid/prepare-fail.toml",
            "https://example.invalid/ensure-mismatch.toml",
        ] {
            let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
            let state = temporary.path().join("state");
            let workspace = temporary.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let fake = Arc::new(FakeSetupEnsureExecutor::new());
            RuntimeBuilder::multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .run(async {
                    let server = async_engine::launch(
                        Service::new(state.clone())
                            .with_setup_ensure_executor(fake.clone())
                            .serve(),
                    );
                    let client = wait_for_client(&state).await;
                    let job = client
                        .submit_setup_ensure(SetupEnsureJobRequest {
                            workspace,
                            config: config.into(),
                            policy: SetupPreparePolicy::Offline,
                            deadline: Duration::from_secs(2),
                            output_limit: 4 * 1024,
                        })
                        .await
                        .unwrap();
                    wait_for_job_state(&client, job, "Failed").await;
                    let logs = wait_for_logs(&client, job).await;
                    assert!(
                        logs.records
                            .iter()
                            .any(|record| record.line.contains("setup ensure failed"))
                    );
                    assert!(!fake.stages().contains(&"mutate".into()));
                    if config.contains("prepare-fail") {
                        assert_eq!(fake.stages(), vec!["plan", "prepare"]);
                    } else {
                        assert_eq!(fake.stages(), vec!["plan", "prepare", "ensure"]);
                    }
                    client.shutdown().await.unwrap();
                    stopped(server).await;
                });
            let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
            assert!(registry.resources(0, 10).unwrap().items.is_empty());
            assert!(registry.resource_uses(0, 10).unwrap().items.is_empty());
        }
    }

    #[test]
    fn setup_ensure_persists_container_and_content_addressed_image_across_daemon_restart() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let request = SetupEnsureJobRequest {
            workspace: workspace.clone(),
            config: "https://example.invalid/setup.toml".into(),
            policy: SetupPreparePolicy::Refresh,
            deadline: Duration::from_secs(2),
            output_limit: 4 * 1024,
        };

        for _ in 0..2 {
            let fake = Arc::new(FakeSetupEnsureExecutor::new());
            RuntimeBuilder::multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .run(async {
                    let server = async_engine::launch(
                        Service::new(state.clone())
                            .with_setup_ensure_executor(fake.clone())
                            .serve(),
                    );
                    let client = wait_for_client(&state).await;
                    let job = client.submit_setup_ensure(request.clone()).await.unwrap();
                    wait_for_job_state(&client, job, "Succeeded").await;
                    client.shutdown().await.unwrap();
                    stopped(server).await;
                });
        }

        let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        let resources = registry.resources(0, 10).unwrap().items;
        assert_eq!(resources.len(), 2);
        let container = resources
            .iter()
            .find(|resource| resource.kind == ResourceKind::Container)
            .unwrap();
        assert_eq!(container.id, "setup-container:fake");
        assert_eq!(container.name, "bosn-setup-fake");
        assert_eq!(container.stack, "setup");
        assert_eq!(container.generation, "sha256:fake");
        assert_eq!(container.scope, Scope::Machine);
        assert_eq!(container.workspace, workspace.to_string_lossy());
        assert_eq!(container.state, ResourceState::Active);
        assert_eq!(container.retention, Retention::Pinned);
        let image = resources
            .iter()
            .find(|resource| resource.kind == ResourceKind::Image)
            .unwrap();
        assert_eq!(image.id, "setup-image:sha256:fake");
        assert_eq!(image.name, "setup-image:sha256:fake");
        // Image generation preserves the verified inspected identity; it is
        // never a mutable tag or a caller-provided registry value.
        assert_eq!(image.generation, "sha256:fake");
        assert_eq!(image.scope, Scope::Machine);
        assert_eq!(image.workspace, workspace.to_string_lossy());
        assert_eq!(image.state, ResourceState::Active);
        assert_eq!(image.retention, Retention::Pinned);
        let uses = registry.resource_uses(0, 10).unwrap().items;
        assert_eq!(uses.len(), 2);
        for use_record in uses {
            assert_eq!(use_record.workspace, workspace.to_string_lossy());
            assert_eq!(use_record.stack, "setup");
            assert_eq!(use_record.state, ResourceState::Active);
        }
    }

    #[test]
    fn setup_image_registry_identity_is_content_addressed_for_pinned_and_inline_forms() {
        let workspace = "/verified/workspace";
        let identity = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let pinned = PreparedImage {
            setup_content_sha256: "pinned-document".into(),
            kind: PreparedImageKind::PinnedImage {
                image:
                    "alpine@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .into(),
            },
            reference:
                "alpine@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .into(),
            observed_identity: identity.into(),
        };
        let inline = PreparedImage {
            setup_content_sha256: "inline-document".into(),
            kind: PreparedImageKind::InlineDockerfile {
                tag: "bosn-setup:inline-document".into(),
            },
            reference: "bosn-setup:inline-document".into(),
            observed_identity: identity.into(),
        };
        let pinned_resource = setup_ensure_image_resource(&pinned, workspace);
        let inline_resource = setup_ensure_image_resource(&inline, workspace);
        // An immutable pulled image and an inline build which inspect to the
        // same local Docker image share exactly one machine resource. Mutable
        // references/tags never enter the durable identity.
        assert_eq!(pinned_resource, inline_resource);
        assert_eq!(pinned_resource.id, format!("setup-image:{identity}"));
        assert_eq!(pinned_resource.name, format!("setup-image:{identity}"));
        assert_eq!(pinned_resource.generation, identity);
        assert_eq!(pinned_resource.workspace, workspace);
    }

    #[test]
    fn setup_ensure_registry_recording_is_atomic_when_image_identity_conflicts() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let workspace = "/verified/workspace";
        let image_name =
            "setup-image:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .put_resource(&Resource {
                id: "foreign-image-row".into(),
                kind: ResourceKind::Image,
                name: image_name.into(),
                stack: "foreign".into(),
                generation: "sha256:foreign".into(),
                scope: Scope::Machine,
                workspace: workspace.into(),
                created_at: 1.0,
                last_used: 1.0,
                state: ResourceState::Active,
                retention: Retention::Pinned,
            })
            .unwrap();
        transaction.commit().unwrap();

        let execution = SetupEnsureExecution {
            receipt: "ensured container".into(),
            resource: SetupEnsureResource {
                id: "setup-container:document".into(),
                name: "bosn-setup-document".into(),
                stack: "setup".into(),
                generation: "sha256:document".into(),
                workspace: workspace.into(),
            },
            image: SetupEnsureImageResource {
                id: "setup-image:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                name: image_name.into(),
                stack: "setup".into(),
                generation: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                workspace: workspace.into(),
            },
        };
        assert!(matches!(
            record_setup_ensure(&mut registry, &execution),
            Err(bosn_registry::Error::ResourceIdentityConflict)
        ));
        // The failed image upsert rolls back the preceding container and both
        // use rows; only the deliberate pre-existing conflicting row remains.
        let resources = registry.resources(0, 10).unwrap().items;
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].id, "foreign-image-row");
        assert!(registry.resource_uses(0, 10).unwrap().items.is_empty());
    }

    #[test]
    fn cancelled_setup_ensure_does_not_persist_a_resource() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(FakeSetupEnsureExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_ensure_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let job = client
                    .submit_setup_ensure(SetupEnsureJobRequest {
                        workspace,
                        config: "https://example.invalid/wait.toml".into(),
                        policy: SetupPreparePolicy::Refresh,
                        deadline: Duration::from_secs(2),
                        output_limit: 4 * 1024,
                    })
                    .await
                    .unwrap();
                wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
                client.cancel_job(job).await.unwrap();
                wait_for_job_state(&client, job, "Cancelled").await;
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
        let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        assert!(registry.resources(0, 10).unwrap().items.is_empty());
        assert!(registry.resource_uses(0, 10).unwrap().items.is_empty());
    }

    #[test]
    fn cancellation_queued_at_registry_handoff_is_rejected_after_persisted_success() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let registry = Registry::create_writer(
                    state.join("registry.sqlite3"),
                    "11111111-2222-4333-8444-555555555555",
                )
                .unwrap();
                let (entered, mut entered_wait) = async_engine::channel(1);
                let (release, release_wait) = async_engine::channel(1);
                let (registry_sender, registry_receiver) = async_engine::channel(4);
                let registry_handle = RegistryActor {
                    sender: registry_sender,
                };
                let registry_task = async_engine::launch(registry_actor(
                    registry,
                    registry_receiver,
                    Some(SetupEnsureRecordGate {
                        entered,
                        release: release_wait,
                    }),
                ));
                let (job_sender, job_receiver) = async_engine::channel(4);
                let jobs = JobActor {
                    sender: job_sender.clone(),
                };
                let fake = Arc::new(FakeSetupEnsureExecutor::new());
                let job_task = async_engine::launch(job_actor(
                    Jobs::new(1),
                    job_receiver,
                    SetupExecutors {
                        prepare: Arc::new(SlowFakeSetupExecutor::new()),
                        task: Arc::new(FakeSetupTaskExecutor::new()),
                        ensure: fake,
                    },
                    job_sender.clone(),
                    registry_handle.clone(),
                ));
                let id = jobs
                    .submit_setup_ensure(SetupEnsureJobRequest {
                        workspace: workspace.clone(),
                        config: "https://example.invalid/setup.toml".into(),
                        policy: SetupPreparePolicy::Refresh,
                        deadline: Duration::from_secs(2),
                        output_limit: 4 * 1024,
                    })
                    .await
                    .unwrap();
                assert!(entered_wait.recv().await.is_some());

                // This command is now definitely queued behind an in-flight
                // actor-owned registry transaction, not merely racing a token
                // check in a worker task.
                let (reply, wait) = async_engine::oneshot_channel();
                assert!(
                    job_sender
                        .send(JobCommand::Cancel { id, reply })
                        .await
                        .is_ok()
                );
                release.send(()).await.unwrap();
                assert!(wait.await.unwrap().is_err());
                assert_eq!(
                    jobs.status(id).await.unwrap().state,
                    jobs::JobState::Succeeded
                );

                jobs.stop().await;
                registry_handle.stop().await;
                job_task.await.unwrap();
                registry_task.await.unwrap();
            });
        let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        assert_eq!(registry.resources(0, 10).unwrap().items.len(), 2);
        assert_eq!(registry.resource_uses(0, 10).unwrap().items.len(), 2);
    }

    #[test]
    fn setup_ensure_coalescing_digest_covers_every_immutable_input() {
        let base = SetupEnsureJobRequest {
            workspace: PathBuf::from("/workspace"),
            config: "https://example.invalid/setup.toml".into(),
            policy: SetupPreparePolicy::Refresh,
            deadline: Duration::from_secs(2),
            output_limit: 4 * 1024,
        };
        let variants = [
            SetupEnsureJobRequest {
                workspace: PathBuf::from("/other"),
                ..base.clone()
            },
            SetupEnsureJobRequest {
                config: "https://example.invalid/other.toml".into(),
                ..base.clone()
            },
            SetupEnsureJobRequest {
                policy: SetupPreparePolicy::Offline,
                ..base.clone()
            },
            SetupEnsureJobRequest {
                deadline: Duration::from_secs(3),
                ..base.clone()
            },
            SetupEnsureJobRequest {
                output_limit: 8 * 1024,
                ..base.clone()
            },
        ];
        for variant in variants {
            assert_ne!(setup_ensure_digest(&base), setup_ensure_digest(&variant));
        }
    }

    #[test]
    fn setup_ensure_wire_validation_preserves_prepare_bounds() {
        assert!(
            validate_setup_ensure_wire(
                "/workspace",
                "https://example.invalid/setup.toml",
                SetupPreparePolicy::Refresh,
                1,
                1,
            )
            .is_ok()
        );
        for (deadline, output) in [(0, 1), (300_001, 1), (1, 0), (1, 8_388_609)] {
            assert!(
                validate_setup_ensure_wire(
                    "/workspace",
                    "https://example.invalid/setup.toml",
                    SetupPreparePolicy::Offline,
                    deadline,
                    output,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn setup_ensure_rejects_all_legacy_job_and_task_wire_fields() {
        let request = || Request {
            workspace: "/workspace".into(),
            setup_config: "https://example.invalid/setup.toml".into(),
            setup_policy: SetupPreparePolicy::Refresh.wire(),
            setup_deadline_ms: 1,
            setup_output_limit: 1,
            ..Request::operation(10)
        };
        assert!(
            validate_setup_ensure_request_wire(&request(), SetupPreparePolicy::Refresh).is_ok()
        );
        for invalid in [
            Request {
                stack: "stack".into(),
                ..request()
            },
            Request {
                digest: "digest".into(),
                ..request()
            },
            Request {
                job_id: 1,
                ..request()
            },
            Request {
                log_after: 1,
                ..request()
            },
            Request {
                log_limit: 1,
                ..request()
            },
            Request {
                setup_task_name: "task".into(),
                ..request()
            },
        ] {
            assert!(
                validate_setup_ensure_request_wire(&invalid, SetupPreparePolicy::Refresh).is_err()
            );
        }
    }

    #[test]
    fn ensure_pipeline_does_not_reset_budget_and_never_mutates_after_prepare_or_ownership_failure()
    {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let plan = pipeline_plan(&workspace);
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();

        // A failed image action prevents all ensure inspection/create/start
        // operations, so no container mutation can follow preparation failure.
        let prepare_failure =
            PipelineFakeEngine::new([Ok(command_result(1, b"pull failed".to_vec()))], []);
        runtime.run(async {
            let deadline = async_engine::Deadline::after(Duration::from_secs(1));
            let cancellation = CancellationSource::new();
            let (events, _event_receiver) = async_engine::channel(8);
            let (logs, _log_receiver) = async_engine::channel(8);
            let pipeline = SetupEnsurePipeline {
                plan: &plan,
                workspace: workspace.clone(),
                deadline: &deadline,
                prepare_output: 512,
                ensure_output: 512,
            };
            assert!(
                execute_setup_ensure_pipeline(
                    &prepare_failure,
                    &pipeline,
                    &cancellation.token(),
                    &events,
                    &logs,
                )
                .await
                .is_err()
            );
        });
        assert!(prepare_failure.ensure_calls.lock().unwrap().is_empty());

        // A mismatching observed candidate fails in ensure_setup_app before
        // any create/start command. The fake has no later mutation response
        // configured, making an accidental mutation an immediate test failure.
        let mismatch = PipelineFakeEngine::new(
            [
                Ok(command_result(0, Vec::new())),
                Ok(command_result(0, format!("{TEST_IDENTITY}\n"))),
            ],
            [Ok(bosn_setup::SetupEnsureResponse::Inspection(
                Some(bosn_setup::SetupEnsureObservedContainer {
                    container_id: TEST_CONTAINER_ID.into(),
                    running: false,
                    image_identity:
                        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                            .into(),
                    labels: BTreeMap::new(),
                }),
                command_result(0, Vec::new()),
            ))],
        );
        runtime.run(async {
            let deadline = async_engine::Deadline::after(Duration::from_secs(1));
            let cancellation = CancellationSource::new();
            let (events, _event_receiver) = async_engine::channel(8);
            let (logs, _log_receiver) = async_engine::channel(8);
            let pipeline = SetupEnsurePipeline {
                plan: &plan,
                workspace: workspace.clone(),
                deadline: &deadline,
                prepare_output: 512,
                ensure_output: 512,
            };
            assert!(
                execute_setup_ensure_pipeline(
                    &mismatch,
                    &pipeline,
                    &cancellation.token(),
                    &events,
                    &logs,
                )
                .await
                .is_err()
            );
        });
        assert_eq!(mismatch.ensure_calls.lock().unwrap().len(), 1);

        let success = PipelineFakeEngine::new(
            [
                Ok(command_result(0, Vec::new())),
                Ok(command_result(0, format!("{TEST_IDENTITY}\n"))),
            ],
            [
                Ok(bosn_setup::SetupEnsureResponse::Inspection(
                    None,
                    command_result(1, Vec::new()),
                )),
                Ok(bosn_setup::SetupEnsureResponse::Command(command_result(
                    0,
                    format!("{TEST_CONTAINER_ID}\n"),
                ))),
                Ok(bosn_setup::SetupEnsureResponse::Command(command_result(
                    0,
                    Vec::new(),
                ))),
            ],
        );
        runtime.run(async {
            let deadline = async_engine::Deadline::after(Duration::from_secs(1));
            let cancellation = CancellationSource::new();
            let (events, _event_receiver) = async_engine::channel(8);
            let (logs, _log_receiver) = async_engine::channel(8);
            let pipeline = SetupEnsurePipeline {
                plan: &plan,
                workspace,
                deadline: &deadline,
                prepare_output: 1024,
                ensure_output: 1025,
            };
            execute_setup_ensure_pipeline(
                &success,
                &pipeline,
                &cancellation.token(),
                &events,
                &logs,
            )
            .await
            .unwrap();
        });
        // The prepare helper uses its one 1024-byte allocation for pull and
        // inspect; ensure receives the disjoint 1025-byte remainder, never
        // the caller's full 2049-byte cap again.
        assert_eq!(success.image_calls.lock().unwrap()[0].1.output_limit, 1024);
        assert_eq!(success.ensure_calls.lock().unwrap()[0].1.output_limit, 1025);
    }

    #[test]
    fn second_daemon_is_refused_while_first_holds_writer() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let first = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let second = Service::new(state.clone()).serve().await;
            assert!(matches!(
                second,
                Err(Error::Registry(bosn_registry::Error::WriterAlreadyHeld(_)))
            ));
            client.shutdown().await.unwrap();
            stopped(first).await;
        });
    }

    #[test]
    fn status_actor_serves_concurrent_typed_requests_before_clean_stop() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let mut requests = async_engine::TaskGroup::new();
            for _ in 0..8 {
                let client = client.clone();
                requests.spawn(async move { client.status().await });
            }
            while let Some(result) = requests.join_next().await {
                let status = result.unwrap().unwrap();
                assert_eq!(status.schema_version, 5);
            }
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn response_envelope_rejects_wrong_correlation_protocol_kind_and_encoding() {
        let mut payload = Vec::new();
        ReplyWire {
            code: 10,
            ..Default::default()
        }
        .encode(&mut payload)
        .unwrap();
        let request = DaemonFrame::request(PAYLOAD_PROTOCOL, Vec::new()).with_request_id(7);
        assert!(matches!(
            decode_response_frame(DaemonFrame::response_to(&request, payload.clone()), 7),
            Ok(Reply::Pong)
        ));
        for frame in [
            DaemonFrame::response_to(&request, payload.clone()).with_request_id(8),
            DaemonFrame::request(PAYLOAD_PROTOCOL, payload.clone()).with_request_id(7),
            DaemonFrame::response_to(&request, payload.clone()).with_raw_payload_encoding(1),
            DaemonFrame::response_to(
                &DaemonFrame::request(PAYLOAD_PROTOCOL + 1, Vec::new()),
                payload,
            ),
        ] {
            assert!(matches!(
                decode_response_frame(frame, 7),
                Err(Error::Protocol("response frame"))
            ));
        }
    }

    #[test]
    fn peer_authorization_fails_closed_for_empty_or_other_user() {
        assert!(peer_is_authorized("current-user", "current-user"));
        assert!(!peer_is_authorized("", "current-user"));
        assert!(!peer_is_authorized("other-user", "current-user"));
    }

    #[test]
    fn unsupported_request_protocol_returns_typed_error_response() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let mut payload = Vec::new();
            Request {
                protocol_version: PROTOCOL_VERSION + 1,
                operation: 1,
                workspace: String::new(),
                stack: String::new(),
                digest: String::new(),
                job_id: 0,
                log_after: 0,
                log_limit: 0,
                setup_config: String::new(),
                setup_policy: 0,
                setup_deadline_ms: 0,
                setup_output_limit: 0,
                setup_task_name: String::new(),
            }
            .encode(&mut payload)
            .unwrap();
            let mut stream = AsyncStream::connect(&endpoint(&state).unwrap())
                .await
                .unwrap();
            write_frame(
                &mut stream,
                DaemonFrame::request(PAYLOAD_PROTOCOL, payload).with_request_id(42),
            )
            .await
            .unwrap();
            let response = read_frame(&mut stream).await.unwrap();
            assert_eq!(response.request_id(), 42);
            assert!(matches!(
                decode_response_frame(response, 42),
                Err(Error::Protocol("unsupported protocol"))
            ));
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn malformed_oversized_and_stalled_clients_do_not_block_a_healthy_client() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;

            // Each bad peer is handled independently and dropped.  In
            // particular, partial input has an absolute per-frame deadline;
            // it cannot keep resetting a deadline by dribbling bytes.
            send_raw(&state, vec![0xff, 0xff]).await;
            send_raw(
                &state,
                DaemonFrameCodec::encode(
                    &DaemonFrame::request(PAYLOAD_PROTOCOL, Vec::new())
                        .with_raw_payload_encoding(1),
                )
                .unwrap(),
            )
            .await;
            send_raw(
                &state,
                DaemonFrameCodec::encode(&DaemonFrame::request(
                    PAYLOAD_PROTOCOL,
                    vec![0; MAX_FRAME],
                ))
                .unwrap(),
            )
            .await;
            let _stalled = AsyncStream::connect(&endpoint(&state).unwrap())
                .await
                .unwrap();

            async_engine::timeout(Duration::from_millis(500), client.ping())
                .await
                .expect("stalled peer blocked ping")
                .unwrap();
            async_engine::timeout(Duration::from_millis(500), client.status())
                .await
                .expect("stalled peer blocked status")
                .unwrap();
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn slow_drip_frame_has_one_absolute_deadline() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let bytes =
                DaemonFrameCodec::encode(&DaemonFrame::request(PAYLOAD_PROTOCOL, vec![0; 64]))
                    .unwrap();
            let mut stream = AsyncStream::connect(&endpoint(&state).unwrap())
                .await
                .unwrap();
            for byte in bytes.iter().take(5) {
                let _ = stream.write_all(&[*byte]).await;
                async_engine::sleep(Duration::from_millis(800)).await;
            }
            // More than IO_DEADLINE elapsed since the first byte. A per-chunk
            // timeout would retain this client; the absolute deadline releases it.
            async_engine::timeout(Duration::from_millis(500), client.ping())
                .await
                .expect("slow drip blocked ping")
                .unwrap();
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn existing_database_aliases_share_endpoint_identity() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let alias = state.join(".");
            let alias_client = Client::for_state(&alias).unwrap();
            assert_eq!(
                alias_client.status().await.unwrap().registry_id,
                client.status().await.unwrap().registry_id
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn regular_preexisting_endpoint_is_preserved_and_writer_is_released() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        // Create the database first so this uses the same inode-keyed endpoint
        // the service would select after acquiring its writer.
        std::fs::create_dir_all(&state).unwrap();
        let db = state.join("registry.sqlite3");
        let registry =
            Registry::create_writer(&db, "00000000-0000-4000-8000-000000000001").unwrap();
        drop(registry);
        let ep = endpoint(&state).unwrap();
        std::fs::write(ep.display(), b"do not remove").unwrap();

        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            assert!(matches!(
                Service::new(state.clone()).serve().await,
                Err(Error::EndpointOccupied(_))
            ));
        });
        assert_eq!(std::fs::read(ep.display()).unwrap(), b"do not remove");
        drop(Registry::open_writer(&db).expect("failed startup retained writer"));
    }

    #[test]
    fn legacy_or_reconciliation_gated_registry_refuses_before_listening_or_mutation() {
        for reconciliation_required in [false, true] {
            let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
            let state = temporary.path().join("state");
            std::fs::create_dir_all(&state).unwrap();
            let db = state.join("registry.sqlite3");
            if reconciliation_required {
                let registry =
                    Registry::create_writer(&db, "00000000-0000-4000-8000-000000000002").unwrap();
                drop(registry);
                let connection = kernal_api::sqlite::Connection::open(&db).unwrap();
                connection.execute("INSERT INTO meta(key,value) VALUES('migration.reconciliation_required','true')", &[]).unwrap();
            } else {
                let connection = kernal_api::sqlite::Connection::open(&db).unwrap();
                connection
                    .execute(
                        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                        &[],
                    )
                    .unwrap();
                connection
                    .execute("INSERT INTO meta VALUES ('schema_version','4')", &[])
                    .unwrap();
            }
            let before = std::fs::read(&db).unwrap();
            let ep = endpoint(&state).unwrap();
            let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
            runtime.run(async {
                let result = Service::new(state.clone()).serve().await;
                if reconciliation_required {
                    assert!(matches!(
                        result,
                        Err(Error::Registry(
                            bosn_registry::Error::ReconciliationRequired
                        ))
                    ));
                } else {
                    assert!(matches!(
                        result,
                        Err(Error::Registry(bosn_registry::Error::LegacyImportRequired(
                            4
                        )))
                    ));
                }
            });
            assert!(!ep.target_exists().unwrap());
            assert_eq!(std::fs::read(&db).unwrap(), before);
        }
    }

    #[test]
    fn independent_state_directories_serve_concurrently() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let left = temporary.path().join("left");
        let right = temporary.path().join("right");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let left_server = async_engine::launch(Service::new(left.clone()).serve());
            let right_server = async_engine::launch(Service::new(right.clone()).serve());
            let left_client = wait_for_client(&left).await;
            let right_client = wait_for_client(&right).await;
            assert_ne!(
                left_client.status().await.unwrap().registry_id,
                right_client.status().await.unwrap().registry_id
            );
            left_client.shutdown().await.unwrap();
            right_client.shutdown().await.unwrap();
            stopped(left_server).await;
            stopped(right_server).await;
        });
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
        for _ in 0..30 {
            if client.ping().await.is_ok() {
                return client;
            }
            async_engine::sleep(Duration::from_millis(20)).await;
        }
        panic!("daemon did not become ready")
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
}
