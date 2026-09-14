//! Small, authenticated Rust daemon foundation. Product protobuf remains private.

use bosn_core::{
    ManifestRoots, ResourceKind, ResourceState, Retention, Scope, SetupApp, SetupSource, SetupTask,
    parse_manifest_toml,
};
use bosn_engine::{DockerDoctorReport, DockerDoctorState, DockerEngine, EngineEvent, RunOptions};
use bosn_generation::{ExternalImageIdentity, collector::CollectorLimits, stack_generation_async};
use bosn_registry::{
    Event, ExecutionSession, Registry, RegistryStatus, Resource, ResourceUse, SetupDone,
    SetupGcPreview, VolumeCreationIntent,
};
#[cfg(test)]
use bosn_setup::PreparedImageKind;
use bosn_setup::{
    PreparedImage, SetupAcquirePolicy, SetupAppTaskRequest, SetupEnsureEngine,
    SetupEnsureRequest as CoreSetupEnsureRequest, SetupEnsureResult, SetupImageEngine,
    SetupNamedVolume, SetupPlan, SetupPlanAppSource, SetupPlanRequest, SetupTaskRequest,
    adopt_setup_app, ensure_setup_app, execute_setup_app_task, execute_setup_task, plan_setup,
    prepare_setup_image,
};
use jobs::{Jobs, Submission};
use kernal_api::{
    async_engine::{self, CancellationSource},
    daemon_frame_v1::{
        DaemonFrame, DaemonFrameCodec, DaemonFrameDecode, DaemonFrameKind, DaemonPayloadEncoding,
    },
    hash::Sha256Hasher,
    platform::{
        fs,
        ipc::{self, AsyncListener, AsyncStream, Endpoint, EndpointAddressCandidates},
    },
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
/// One fixed engine version probe. This is intentionally independent of setup
/// job limits: diagnostic callers cannot select a deadline, output budget, or
/// any Docker command.
const DOCTOR_REGISTRY_DEADLINE: Duration = Duration::from_millis(500);
const DOCTOR_ENGINE_DEADLINE: Duration = Duration::from_millis(1500);
const DOCTOR_ENGINE_OUTPUT: usize = 512;
/// A diagnostic page is deliberately small enough to fit comfortably in the
/// authenticated IPC frame and every public front end.
pub const MAX_REGISTRY_DIAGNOSTIC_PAGE: u32 = 64;

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

/// Immutable inputs for running a named task in an already ensured setup app.
/// This is intentionally distinct from [`SetupTaskJobRequest`]: it never
/// creates an ephemeral `docker run` task container. The daemon re-plans and
/// proves exact ownership of the content-addressed app before one fixed exec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupAppTaskJobRequest {
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
/// One bounded daemon-owned ensure of an explicitly selected legacy Bosn
/// manifest stack. The caller selects neither an image nor Docker controls:
/// those remain in the parsed manifest. `manifest` must be a safe relative
/// path beneath `workspace`; remote manifests are intentionally not a part of
/// this initial runtime slice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestEnsureJobRequest {
    pub workspace: PathBuf,
    pub manifest: String,
    pub stack: String,
    pub deadline: Duration,
    pub output_limit: usize,
}
/// One bounded execution of a task already declared by an ensured, supported
/// manifest stack.  This deliberately carries selectors only: the daemon
/// re-reads the manifest and derives the command, image, and container name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestAppTaskJobRequest {
    pub workspace: PathBuf,
    pub manifest: String,
    pub stack: String,
    pub task_name: String,
    pub deadline: Duration,
    pub output_limit: usize,
}
/// Explicit, confirmed restoration of a lost local registry record for an
/// already-existing Bosn-managed setup application. It has no engine targets:
/// plan, image identity, deterministic name, and labels are all re-derived.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupAdoptRequest {
    pub workspace: PathBuf,
    pub config: String,
    pub policy: SetupPreparePolicy,
    pub deadline: Duration,
    pub output_limit: usize,
    pub confirm: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupAdoptResult {
    pub adopted: bool,
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

/// Test seam for one declared task inside an already ensured setup app. It
/// receives only semantic request fields; it has no container ID, command, or
/// Docker argv control surface.
pub trait SetupAppTaskExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: SetupAppTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
        session: &'a dyn SetupAppTaskSessionRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

/// Daemon-owned durable ownership evidence for a live setup-app task. The
/// executor cannot open SQLite itself; it must bracket the fixed exec through
/// this actor-owned recorder. Its identity is Bosn's verified deterministic
/// managed name, rather than Docker's opaque ID, so registry GC can protect
/// the exact resource after an uncertain local client outcome. A recorder
/// failure fails closed before exec.
pub trait SetupAppTaskSessionRecorder: Send + Sync {
    fn begin<'a>(
        &'a self,
        managed_container_identity: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
    fn finish<'a>(
        &'a self,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
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
/// Testable semantic boundary for a single manifest stack ensure. It receives
/// no raw Docker command, name, label, image, mount, environment, or command.
pub trait ManifestEnsureExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: ManifestEnsureJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
        registry: &'a RegistryActor,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>>;
}
/// Semantic boundary for a named task in an already ensured manifest stack.
/// There are intentionally no raw command, container, or engine controls.
pub trait ManifestAppTaskExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: ManifestAppTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
        session: &'a dyn ManifestAppTaskSessionRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}
/// Durable, conservative ownership evidence for a manifest app task. The
/// identity is the registry-matching deterministic managed container name.
pub trait ManifestAppTaskSessionRecorder: Send + Sync {
    fn begin<'a>(
        &'a self,
        managed_container_identity: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
    fn finish<'a>(
        &'a self,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}
pub trait SetupAdoptExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: SetupAdoptRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>>;
}

/// Testable boundary for Bosn's one fixed, non-mutating engine health probe.
/// It exposes no argv, environment, path, or output controls to the daemon
/// protocol or any public frontend.
pub trait DoctorExecutor: Send + Sync {
    fn doctor<'a>(&'a self) -> Pin<Box<dyn Future<Output = DockerDoctorReport> + Send + 'a>>;
}

#[derive(Clone)]
pub struct DockerDoctorExecutor {
    engine: DockerEngine,
}
impl DockerDoctorExecutor {
    fn new() -> Self {
        Self {
            engine: DockerEngine::docker(),
        }
    }
}
impl DoctorExecutor for DockerDoctorExecutor {
    fn doctor<'a>(&'a self) -> Pin<Box<dyn Future<Output = DockerDoctorReport> + Send + 'a>> {
        Box::pin(async move {
            self.engine
                .doctor_async(RunOptions::bounded(
                    DOCTOR_ENGINE_DEADLINE,
                    DOCTOR_ENGINE_OUTPUT,
                ))
                .await
        })
    }
}

/// Fixed, read-only inspection boundary for setup reconciliation.  It exposes
/// no caller-selected Docker command, output budget, or lifecycle action.
pub trait SetupReconcileExecutor: Send + Sync {
    fn inspect<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<SetupReconcileObserved>, String>> + Send + 'a>>;
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupReconcileObserved {
    pub name: String,
    pub running: bool,
    pub image_identity: String,
    pub managed: String,
    pub content: String,
    pub container: String,
}
#[derive(Clone)]
pub struct DockerSetupReconcileExecutor {
    engine: DockerEngine,
}
impl DockerSetupReconcileExecutor {
    fn new() -> Self {
        Self {
            engine: DockerEngine::docker(),
        }
    }
}
impl SetupReconcileExecutor for DockerSetupReconcileExecutor {
    fn inspect<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<SetupReconcileObserved>, String>> + Send + 'a>>
    {
        Box::pin(async move {
            const FORMAT: &str = "{{.Name}}\t{{.State.Running}}\t{{.Image}}\t{{index .Config.Labels \"com.zackees.bosn.setup-managed\"}}\t{{index .Config.Labels \"com.zackees.bosn.setup-content-sha256\"}}\t{{index .Config.Labels \"com.zackees.bosn.setup-container\"}}";
            let result = self
                .engine
                .with_args(["container", "inspect", "--format", FORMAT, name])
                .capture_async(RunOptions::bounded(Duration::from_secs(3), 4 * 1024))
                .await
                .map_err(|_| "inspect_error".to_owned())?;
            if result.exit_code == 1 {
                return Ok(None);
            }
            if !result.ok() {
                return Err("inspect_error".into());
            }
            let text =
                std::str::from_utf8(&result.stdout).map_err(|_| "inspect_error".to_owned())?;
            let values: Vec<_> = text.trim_end_matches(['\r', '\n']).split('\t').collect();
            if values.len() != 6
                || !matches!(values[1], "true" | "false")
                || values.iter().any(|value| value.len() > 1024)
            {
                return Err("inspect_error".into());
            }
            Ok(Some(SetupReconcileObserved {
                name: values[0].into(),
                running: values[1] == "true",
                image_identity: values[2].into(),
                managed: values[3].into(),
                content: values[4].into(),
                container: values[5].into(),
            }))
        })
    }
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
    /// Manifest-derived named volumes proven/created before the container.
    /// Generic setup-document ensures always leave this empty.
    pub volumes: Vec<ManifestVolumeResource>,
}

/// Exact durable facts for a manifest-owned named volume.  These are executor
/// output from a parsed declaration, never RPC-provided Docker controls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestVolumeResource {
    pub id: String,
    pub name: String,
    pub stack: String,
    pub generation: String,
    pub scope: Scope,
    pub workspace: String,
    pub retention: Retention,
    pub target: String,
    pub labels: BTreeMap<String, String>,
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
                volumes: Vec::new(),
            })
        })
    }
}

/// Docker-backed implementation of the deliberately narrow legacy-manifest
/// runtime bridge. It translates only the accepted typed manifest shape into
/// the existing finite image-prepare and ownership-safe ensure primitives;
/// it never invokes a generic Compose/Docker runner.
#[derive(Clone)]
pub struct DockerManifestEnsureExecutor {
    engine: DockerEngine,
}
impl DockerManifestEnsureExecutor {
    fn new() -> Self {
        Self {
            engine: DockerEngine::docker(),
        }
    }
}
impl ManifestEnsureExecutor for DockerManifestEnsureExecutor {
    fn execute<'a>(
        &'a self,
        request: ManifestEnsureJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
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
                async_engine::timeout_at(deadline, manifest_stack_setup_plan(&request)),
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
            } = runtime;
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
                workspace: request.workspace.clone(),
                deadline: &deadline,
                prepare_output,
                ensure_output,
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
                    "ensured manifest stack {} as {}",
                    request.stack, result.ensured.container_name
                ),
                resource: SetupEnsureResource {
                    id: format!("manifest-container:{}:{}", request.stack, generation),
                    name: result.ensured.container_name,
                    stack: request.stack.clone(),
                    generation: generation.clone(),
                    workspace: workspace.clone(),
                },
                image: SetupEnsureImageResource {
                    id: format!("manifest-image:{}", result.prepared.observed_identity),
                    name: format!("manifest-image:{}", result.prepared.observed_identity),
                    stack: request.stack,
                    generation: result.prepared.observed_identity,
                    workspace,
                },
                volumes,
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
    engine: DockerEngine,
}
impl DockerManifestAppTaskExecutor {
    fn new() -> Self {
        Self {
            engine: DockerEngine::docker(),
        }
    }
}
impl ManifestAppTaskExecutor for DockerManifestAppTaskExecutor {
    fn execute<'a>(
        &'a self,
        request: ManifestAppTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
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
                async_engine::timeout_at(deadline, manifest_stack_task_setup_plan(&request)),
            )
            .await
            .map_err(|_| "manifest app task cancelled before ownership inspection".to_owned())?
            .map_err(|_| "manifest app task planning exceeded its deadline".to_owned())??;
            let plan = runtime.plan;
            let (events, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
            let forwarded_logs = logs.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(event) = receiver.recv().await {
                    forward_engine_event(&forwarded_logs, event).await?;
                }
                Ok::<(), String>(())
            });
            let result = async {
                let remaining = deadline.remaining();
                if cancellation.is_cancelled() || remaining.is_zero() {
                    return Err("manifest app task ended before image verification".into());
                }
                logs.send("[manifest-app-task] verifying immutable application image".into())
                    .await
                    .map_err(|_| "manifest app task log consumer closed".to_owned())?;
                let prepared = prepare_setup_image(
                    &self.engine,
                    &plan,
                    RunOptions::streaming(remaining, quarter),
                    cancellation,
                    &events,
                )
                .await
                .map_err(|error| error.to_string())?;
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
                        options: RunOptions::streaming(remaining, quarter),
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
                if cancellation.is_cancelled() || deadline.remaining().is_zero() {
                    return Err(
                        "manifest app task ended before exec; remote command was not started"
                            .into(),
                    );
                }
                logs.send(format!(
                    "[manifest-app-task] running declared task {}",
                    request.task_name
                ))
                .await
                .map_err(|_| "manifest app task log consumer closed".to_owned())?;
                session
                    .begin(manifest_app_task_session_container_identity(&observed))
                    .await
                    .map_err(|_| "manifest app task ownership recording unavailable".to_owned())?;
                let result = execute_setup_app_task(
                    &self.engine,
                    SetupAppTaskRequest {
                        plan: &plan,
                        workspace_root: request.workspace.clone(),
                        task_name: request.task_name.clone(),
                        prepared_image: &prepared,
                        options: RunOptions::streaming(deadline.remaining(), exec_output),
                        cancellation,
                        events: &events,
                    },
                )
                .await;
                let outcome = match &result {
                    Ok(_) => "succeeded",
                    Err(bosn_setup::SetupTaskError::TaskFailed { .. }) => "failed",
                    Err(_) => "uncertain",
                };
                session
                    .finish(outcome)
                    .await
                    .map_err(|_| "manifest app task completion recording unavailable".to_owned())?;
                match result {
                    Ok(value) => Ok(format!(
                        "completed declared manifest task {} in managed container {} with image {}",
                        value.task_name, observed.container_name, value.image_identity
                    )),
                    Err(bosn_setup::SetupTaskError::Cancelled)
                    | Err(bosn_setup::SetupTaskError::Deadline) => Err(
                        "manifest app task exec client ended; remote command completion is unknown"
                            .into(),
                    ),
                    Err(error) => Err(error.to_string()),
                }
            }
            .await;
            drop(events);
            forwarder
                .await
                .map_err(|_| "manifest app task log forwarder stopped".to_owned())??;
            result
        })
    }
}

/// Translate the strictly supported manifest runtime subset to the existing
/// typed setup receipt. This is intentionally a refusal boundary, not a
/// lossy migration: fields which would need more lifecycle semantics are
/// rejected before Docker is contacted.
async fn manifest_stack_setup_plan(
    request: &ManifestEnsureJobRequest,
) -> Result<ManifestRuntimePlan, String> {
    manifest_stack_plan(&request.workspace, &request.manifest, &request.stack, None).await
}

async fn manifest_stack_task_setup_plan(
    request: &ManifestAppTaskJobRequest,
) -> Result<ManifestRuntimePlan, String> {
    manifest_stack_plan(
        &request.workspace,
        &request.manifest,
        &request.stack,
        Some(&request.task_name),
    )
    .await
}

/// Re-read and validate one exact manifest snapshot before every engine
/// operation.  A selected task is injected only after it is proven to belong
/// to the selected stack; this remains a finite translation to the existing
/// typed setup primitives, not a generic manifest runner.
async fn manifest_stack_plan(
    request_workspace: &Path,
    request_manifest: &str,
    request_stack: &str,
    task_name: Option<&str>,
) -> Result<ManifestRuntimePlan, String> {
    let workspace = fs::canonical_context_path(request_workspace)
        .map_err(|_| "manifest workspace cannot be canonicalized".to_owned())?;
    let metadata = fs::context_path_metadata_no_follow(&workspace)
        .map_err(|_| "manifest workspace is not a directory".to_owned())?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err("manifest workspace is not a directory".into());
    }
    if !safe_manifest_relative_path(request_manifest) {
        return Err("manifest path must be a safe workspace-relative path".into());
    }
    let manifest_path = workspace.join(request_manifest);
    let manifest_path = fs::canonical_context_path(&manifest_path)
        .map_err(|_| "manifest file cannot be canonicalized".to_owned())?;
    if !manifest_path.starts_with(&workspace) {
        return Err("manifest path escapes selected workspace".into());
    }
    let bytes = fs::read_context_regular_file_bounded(&manifest_path, 1024 * 1024)
        .map_err(|_| "manifest file is not a bounded regular UTF-8 file".to_owned())?;
    let source =
        std::str::from_utf8(&bytes.bytes).map_err(|_| "manifest file is not UTF-8".to_owned())?;
    let manifest = parse_manifest_toml(
        source,
        ManifestRoots::new(
            "workspace manifest",
            workspace.to_string_lossy(),
            workspace.to_string_lossy(),
        ),
    )
    .map_err(|_| "manifest is invalid".to_owned())?;
    let stack = manifest
        .stack(request_stack)
        .map_err(|_| "selected manifest stack does not exist".to_owned())?;
    if stack.dockerfile.is_some()
        || stack.kind.is_some()
        || stack.guest.is_some()
        || !stack.tmpfs.is_empty()
    {
        return Err("selected manifest stack uses an unsupported runtime field".into());
    }
    let image = stack
        .image
        .as_deref()
        .filter(|image| valid_manifest_pinned_image(image))
        .ok_or_else(|| {
            "selected manifest stack must use an immutable digest-pinned image".to_owned()
        })?
        .to_owned();
    if stack.env.len() > bosn_core::MAX_ENVIRONMENT_ENTRIES
        || stack.env.iter().any(|(key, value)| {
            key.is_empty()
                || key.contains('=')
                || key.contains('\0')
                || value.contains('\0')
                || value.len() > 16 * 1024
        })
    {
        return Err("selected manifest stack has unsafe environment data".into());
    }
    // The legacy manifest preserves bind-source spelling because it is also
    // used by the old Python executor.  The native runtime does not pass that
    // spelling to Docker.  It resolves it beneath this exact canonical
    // workspace and converts it into the typed setup representation first.
    let mounts = stack
        .mounts
        .iter()
        .map(|mount| manifest_workspace_mount(&workspace, mount))
        .collect::<Result<Vec<_>, _>>()?;
    let workdir = stack
        .workdir
        .as_deref()
        .map(|value| manifest_workdir_to_workspace_relative(&workspace, value, &mounts))
        .transpose()?;
    let digest = image
        .rsplit_once("@sha256:")
        .map(|(_, value)| format!("sha256:{value}"))
        .expect("validated pinned image has digest");
    let base_generation = stack_generation_async(
        &manifest,
        stack,
        &workspace,
        &CollectorLimits::default(),
        &[ExternalImageIdentity {
            reference: image.clone(),
            platform: None,
            identity: Some(digest),
        }],
    )
    .await
    .map_err(|_| "manifest generation could not be derived".to_owned())?;
    // `bosn-generation` deliberately excludes workdir from the historical
    // content identity because legacy `docker exec` supplied it per task.
    // Native setup creates a persistent container with its workdir and binds,
    // so roll it whenever that effective lifecycle shape changes.
    let generation = manifest_runtime_generation(
        &base_generation,
        &mounts,
        workdir.as_deref(),
        &stack.volumes,
    );
    let content_sha256 = generation
        .strip_prefix("sha256:")
        .ok_or_else(|| "manifest generation is invalid".to_owned())?
        .to_owned();
    let mut tasks = BTreeMap::new();
    if let Some(task_name) = task_name {
        let task = manifest
            .task(task_name)
            .map_err(|_| "selected manifest task does not exist".to_owned())?;
        if task.stack != stack.name {
            return Err("selected manifest task does not belong to selected stack".into());
        }
        if task.cmd.len() > 16 * 1024 || task.cmd.contains('\0') {
            return Err("selected manifest task command is unsafe".into());
        }
        tasks.insert(
            task.name.clone(),
            SetupTask {
                command: task.cmd.clone(),
                workdir: None,
                environment: BTreeMap::new(),
            },
        );
    }
    let task_names = tasks.keys().cloned().collect();
    let workspace_string = workspace.to_string_lossy().into_owned();
    let volumes = manifest_named_volumes(stack, &workspace_string, &generation)?;
    let app = SetupApp {
        source: SetupSource::PinnedImage(image.clone()),
        environment: stack.env.clone(),
        workdir,
        command: None,
        mounts,
    };
    Ok(ManifestRuntimePlan {
        plan: SetupPlan {
            source_kind: bosn_setup::SetupSourceKind::LocalFile,
            content_sha256,
            schema_version: bosn_core::SETUP_DOCUMENT_VERSION,
            workspace_root: workspace,
            asset_root: None,
            task_names,
            app,
            tasks,
            app_source: SetupPlanAppSource::PinnedImage { image },
            named_volumes: volumes
                .iter()
                .map(|volume| SetupNamedVolume {
                    name: volume.name.clone(),
                    target: volume.target.clone(),
                    labels: volume.labels.clone(),
                })
                .collect(),
        },
        generation,
        volumes,
    })
}

#[derive(Clone, Debug)]
struct ManifestRuntimePlan {
    plan: SetupPlan,
    generation: String,
    volumes: Vec<ManifestVolumeResource>,
}

fn manifest_named_volumes(
    stack: &bosn_core::manifest::Stack,
    workspace: &str,
    generation: &str,
) -> Result<Vec<ManifestVolumeResource>, String> {
    let mut targets = std::collections::BTreeSet::new();
    stack
        .volumes
        .iter()
        .map(|volume| {
            let target = volume.mount_at();
            if !normalized_container_path(&target) || !targets.insert(target.clone()) {
                return Err("manifest volume target is unsafe or duplicated".into());
            }
            let scope_key = match volume.scope {
                Scope::Spec => format!("{workspace}\0{generation}"),
                Scope::Stack => workspace.into(),
                Scope::Machine => stack.family.clone().unwrap_or_else(|| stack.name.clone()),
            };
            let mut hasher = Sha256Hasher::new();
            manifest_generation_field(&mut hasher, b"bosn-manifest-volume-v1");
            manifest_generation_field(&mut hasher, stack.name.as_bytes());
            manifest_generation_field(&mut hasher, volume.name.as_bytes());
            manifest_generation_field(&mut hasher, scope_key.as_bytes());
            let identity = hasher.finalize().to_string();
            let name = format!("bosn-v-{}-{}", volume.scope.as_str(), &identity[..24]);
            let labels = BTreeMap::from([
                ("com.zackees.bosn.setup-managed".into(), "v1".into()),
                (
                    "com.zackees.bosn.setup-content-sha256".into(),
                    identity.clone(),
                ),
                ("com.zackees.bosn.setup-container".into(), name.clone()),
            ]);
            Ok(ManifestVolumeResource {
                id: format!("manifest-volume:{name}"),
                name,
                stack: stack.name.clone(),
                // Stack/machine volumes retain this stable identity across a
                // container generation rollover; spec identity includes the
                // parent generation in `scope_key` above.
                generation: format!("sha256:{identity}"),
                scope: volume.scope,
                workspace: workspace.into(),
                retention: volume.retention,
                target,
                labels,
            })
        })
        .collect()
}

/// Convert one legacy manifest bind into the narrower workspace-relative setup
/// bind.  Absolute legacy sources are accepted only when their canonical path
/// is inside the selected workspace; callers never get to name a host path at
/// the typed engine boundary.
fn manifest_workspace_mount(
    workspace: &Path,
    mount: &bosn_core::manifest::Mount,
) -> Result<bosn_core::WorkspaceMount, String> {
    let source = manifest_workspace_member(workspace, &mount.source)?;
    if !normalized_container_path(&mount.destination) {
        return Err("manifest mount target is not a normalized absolute path".into());
    }
    Ok(bosn_core::WorkspaceMount {
        source,
        target: mount.destination.clone(),
        readonly: mount.readonly,
    })
}

/// Resolve a legacy source without allowing an absolute path, `..`, or a
/// symlink to redirect the bind outside the selected canonical workspace.
/// The output is canonical workspace-relative spelling suitable for
/// `WorkspaceMount`, not the original caller/manifest spelling.
fn manifest_workspace_member(workspace: &Path, source: &str) -> Result<String, String> {
    if source.is_empty()
        || source.len() > 4096
        || source.contains(['\0', '\\'])
        || is_windows_absolute_path(source)
    {
        return Err("manifest mount source is unsafe".into());
    }
    let raw = Path::new(source);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        if source
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
            && source != "."
        {
            return Err("manifest mount source is not a normalized workspace path".into());
        }
        workspace.join(raw)
    };
    let metadata = fs::context_path_metadata_no_follow(&candidate)
        .map_err(|_| "declared manifest mount source does not exist".to_owned())?;
    if metadata.kind == fs::ContextPathKind::Symlink {
        return Err("declared manifest mount source is a symlink".into());
    }
    let canonical = fs::canonical_context_path(&candidate)
        .map_err(|_| "declared manifest mount source cannot be canonicalized".to_owned())?;
    let relative = canonical
        .strip_prefix(workspace)
        .map_err(|_| "declared manifest mount source escapes workspace".to_owned())?;
    if relative.as_os_str().is_empty() {
        return Ok(".".into());
    }
    let relative = relative
        .to_str()
        .ok_or_else(|| "declared manifest mount source is not UTF-8".to_owned())?;
    if relative.is_empty()
        || relative.contains(['\0', '\\', ','])
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("declared manifest mount source is unsafe".into());
    }
    Ok(relative.into())
}

/// A manifest workdir is an absolute in-container path.  The setup primitive
/// deliberately stores only workspace-relative workdirs, so translate it
/// through the most-specific declared bind and reject image-only workdirs.
fn manifest_workdir_to_workspace_relative(
    workspace: &Path,
    workdir: &str,
    mounts: &[bosn_core::WorkspaceMount],
) -> Result<String, String> {
    if !normalized_container_path(workdir) {
        return Err("manifest workdir is not a normalized absolute path".into());
    }
    let selected = mounts
        .iter()
        .filter(|mount| container_prefix(workdir, &mount.target))
        .max_by_key(|mount| mount.target.len())
        .ok_or_else(|| {
            "manifest workdir is not covered by a declared workspace mount".to_owned()
        })?;
    let suffix = container_relative_suffix(workdir, &selected.target)
        .expect("container_prefix selected the manifest workdir mount");
    let relative = if selected.source == "." {
        if suffix.is_empty() {
            ".".into()
        } else {
            suffix.into()
        }
    } else if suffix.is_empty() {
        selected.source.clone()
    } else {
        format!("{}/{suffix}", selected.source)
    };
    // A bind of a regular file cannot meaningfully be an application working
    // directory. Check it here and the typed setup primitive will canonicalize
    // the same source again immediately before ensure/task application.
    let source = workspace.join(&selected.source);
    let metadata = fs::context_path_metadata_no_follow(&source)
        .map_err(|_| "manifest workdir mount source no longer exists".to_owned())?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err("manifest workdir must map through a directory bind mount".into());
    }
    Ok(relative)
}

fn normalized_container_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.contains(['\0', '\\', ','])
        && value.starts_with('/')
        && (value == "/"
            || !value[1..]
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".."))
}

fn container_prefix(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn container_relative_suffix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix == "/" {
        return Some(path.strip_prefix('/').unwrap_or(path));
    }
    if path == prefix {
        Some("")
    } else {
        path.strip_prefix(prefix)?.strip_prefix('/')
    }
}

fn is_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn manifest_runtime_generation(
    base_generation: &str,
    mounts: &[bosn_core::WorkspaceMount],
    workdir: Option<&str>,
    volumes: &[bosn_core::manifest::Volume],
) -> String {
    let mut hasher = Sha256Hasher::new();
    manifest_generation_field(&mut hasher, b"bosn-manifest-runtime-v1");
    manifest_generation_field(&mut hasher, base_generation.as_bytes());
    manifest_generation_field(&mut hasher, &(mounts.len() as u64).to_be_bytes());
    for mount in mounts {
        manifest_generation_field(&mut hasher, mount.source.as_bytes());
        manifest_generation_field(&mut hasher, mount.target.as_bytes());
        manifest_generation_field(&mut hasher, if mount.readonly { b"1" } else { b"0" });
    }
    manifest_generation_field(&mut hasher, workdir.unwrap_or("").as_bytes());
    manifest_generation_field(&mut hasher, &(volumes.len() as u64).to_be_bytes());
    for volume in volumes {
        manifest_generation_field(&mut hasher, volume.name.as_bytes());
        manifest_generation_field(&mut hasher, volume.scope.as_str().as_bytes());
        manifest_generation_field(&mut hasher, volume.mount_at().as_bytes());
        manifest_generation_field(
            &mut hasher,
            match volume.retention {
                Retention::Warm => b"warm",
                Retention::Pinned => b"pinned",
            },
        );
    }
    format!("sha256:{}", hasher.finalize())
}

fn manifest_generation_field(hasher: &mut Sha256Hasher, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn safe_manifest_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.contains('\0')
        && !value.contains('\\')
        && !value.starts_with('/')
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn valid_manifest_pinned_image(value: &str) -> bool {
    let Some((name, digest)) = value.rsplit_once("@sha256:") else {
        return false;
    };
    !name.is_empty()
        && value.len() <= 512
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Docker-backed restoration. Preparation is deliberately retained because it
/// produces the validated inspected image identity used to prove the existing
/// candidate; it never creates, starts, stops, removes, or replaces Docker.
#[derive(Clone)]
pub struct DockerSetupAdoptExecutor {
    state_dir: PathBuf,
    engine: DockerEngine,
}
impl DockerSetupAdoptExecutor {
    fn new(state_dir: PathBuf) -> Self {
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
                    id: format!("setup-container:{}", plan.content_sha256),
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

/// Docker-backed implementation for one declared task in the persistent setup
/// app. The app is never created, started, stopped, or replaced here: a fresh
/// ownership inspection must prove the exact content-addressed app already
/// exists before the fixed `docker container exec NAME sh -lc DECLARED` call.
#[derive(Clone)]
pub struct DockerSetupAppTaskExecutor {
    state_dir: PathBuf,
    engine: DockerEngine,
}
impl DockerSetupAppTaskExecutor {
    fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            engine: DockerEngine::docker(),
        }
    }
}
impl SetupAppTaskExecutor for DockerSetupAppTaskExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupAppTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a async_engine::Sender<String>,
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
            let prepared = prepare_setup_image(
                &self.engine,
                &plan,
                RunOptions::streaming(remaining, quarter),
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
                    options: RunOptions::streaming(remaining, quarter),
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

/// One credential- and path-safe registry resource diagnostic.  This is not a
/// raw registry row: workspace and scope bindings remain local registry
/// implementation details, while these stable facts identify managed state.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryResourceDiagnostic {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub stack: String,
    pub generation: String,
    pub state: String,
    pub retention: String,
    pub created_at: f64,
    pub last_used: f64,
}

/// Bounded offset-cursor page of safe managed-resource diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryResourcePage {
    pub next: Option<u64>,
    pub records: Vec<RegistryResourceDiagnostic>,
}

/// One safe, logical future-GC candidate. This is preview metadata only: it
/// is never an engine identifier and cannot be used to request deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupGcCandidateDiagnostic {
    pub id: String,
    pub name: String,
    pub generation: String,
    /// Opaque, preview-derived binding. Apply accepts only this complete token
    /// plus the exact workspace; it never accepts a Docker name or selector.
    pub token: String,
    pub reason: String,
}
/// Result of one deliberate setup-GC apply.  `reconciled_missing` means Docker
/// reported the exact previously-owned container absent and the daemon removed
/// only its still-eligible registry record; it never means a broad prune ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupGcApplyResult {
    pub removed: bool,
    pub reconciled_missing: bool,
}
/// Result of stopping one exact retired setup generation.  An already-stopped
/// exact candidate is intentionally idempotent: no Docker mutation or event
/// write occurs, and its retired registry record remains for GC apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupRetiredStopResult {
    pub stopped: bool,
    pub already_stopped: bool,
}
/// Result of an explicit, registry-only setup workspace completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupDoneResult {
    pub uses_completed: u64,
    pub resources_completed: u64,
}
impl From<SetupDone> for SetupDoneResult {
    fn from(value: SetupDone) -> Self {
        Self {
            uses_completed: value.uses_completed,
            resources_completed: value.resources_completed,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupGcPreviewCounts {
    pub protected_not_retired: u64,
    pub protected_ambiguous_use: u64,
    pub protected_lease: u64,
    pub protected_session: u64,
    pub excluded_unmanaged: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupGcPreviewPage {
    pub next: Option<u64>,
    pub candidates: Vec<SetupGcCandidateDiagnostic>,
    pub counts: SetupGcPreviewCounts,
}

/// A bounded, read-only comparison between one durable managed-container
/// record and Docker. It intentionally contains no workspace, URL, engine
/// output, or repair token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupReconcileRecord {
    pub id: String,
    pub name: String,
    pub generation: String,
    /// `matching_running`, `matching_stopped`, `missing`, `name_mismatch`,
    /// `label_mismatch`, `image_mismatch`, `inspect_error`, or `unknown`.
    /// Unknown is conservative and must never be treated as a
    /// repair/GC candidate.
    pub drift: String,
    /// Present only for a previewed, currently repairable `missing` record.
    /// This opaque binding is never a Docker identifier and is revalidated by
    /// the daemon immediately before its registry-only transition.
    pub repair_token: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupReconcilePreviewPage {
    pub next: Option<u64>,
    pub records: Vec<SetupReconcileRecord>,
}
/// Result of confirmation-gated repair of one previewed missing setup app.
/// The repair changes durable registry lifecycle state only; it never mutates
/// Docker. `already_repaired` makes a repeated exact token safely idempotent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupReconcileMissingRepairResult {
    pub repaired: bool,
    pub already_repaired: bool,
}

#[derive(Clone, Debug)]
struct SetupReconcileCandidate {
    resource: Resource,
    image_identities: Vec<String>,
    missing_repairable: bool,
}
type SetupReconcileCandidates = (Option<u64>, Vec<SetupReconcileCandidate>);

fn classify_setup_reconcile(
    candidate: &SetupReconcileCandidate,
    inspected: Result<Option<SetupReconcileObserved>, String>,
) -> &'static str {
    match (
        candidate.resource.generation.strip_prefix("sha256:"),
        inspected,
    ) {
        (_, Ok(None)) => "missing",
        (Some(_), Ok(Some(observed)))
            if observed.name != format!("/{}", candidate.resource.name) =>
        {
            "name_mismatch"
        }
        (Some(content), Ok(Some(observed)))
            if observed.managed != "v1"
                || observed.content != content
                || observed.container != candidate.resource.name =>
        {
            "label_mismatch"
        }
        (Some(_), Ok(Some(observed)))
            if !candidate
                .image_identities
                .iter()
                .any(|image| image == &observed.image_identity) =>
        {
            "image_mismatch"
        }
        (Some(_), Ok(Some(observed))) => {
            if observed.running {
                "matching_running"
            } else {
                "matching_stopped"
            }
        }
        (_, Err(_)) => "inspect_error",
        _ => "unknown",
    }
}

fn setup_gc_preview_diagnostic(value: SetupGcPreview) -> SetupGcPreviewPage {
    SetupGcPreviewPage {
        next: value.candidates.next_offset.map(|value| value as u64),
        candidates: value
            .candidates
            .items
            .into_iter()
            .map(|candidate| SetupGcCandidateDiagnostic {
                token: setup_gc_token(&candidate),
                id: candidate.id,
                name: candidate.name,
                generation: candidate.generation,
                reason: "retired_managed_setup_container".into(),
            })
            .collect(),
        counts: SetupGcPreviewCounts {
            protected_not_retired: value.counts.protected_not_retired,
            protected_ambiguous_use: value.counts.protected_ambiguous_use,
            protected_lease: value.counts.protected_lease,
            protected_session: value.counts.protected_session,
            excluded_unmanaged: value.counts.excluded_unmanaged,
        },
    }
}

fn setup_gc_token(candidate: &bosn_registry::SetupGcCandidate) -> String {
    // Hex makes a delimiter-free opaque transport value without adding a
    // parser-sensitive dependency. It is an identity binding, not a secret:
    // the actor always revalidates the decoded tuple immediately before any
    // engine operation and again while finalizing registry state.
    let mut bytes = Vec::new();
    for value in [&candidate.id, &candidate.name, &candidate.generation] {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    let mut token = String::from("sgc1-");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}

fn setup_reconcile_missing_token(candidate: &SetupReconcileCandidate) -> String {
    let mut bytes = Vec::new();
    for value in [
        &candidate.resource.id,
        &candidate.resource.name,
        &candidate.resource.generation,
    ] {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    let mut token = String::from("srm1-");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}

fn parse_setup_gc_token(token: &str) -> Result<(String, String, String), Error> {
    let encoded = token
        .strip_prefix("sgc1-")
        .ok_or(Error::Protocol("invalid setup gc candidate"))?;
    if encoded.is_empty() || encoded.len() > 24 * 1024 || encoded.len() % 2 != 0 {
        return Err(Error::Protocol("invalid setup gc candidate"));
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for chunk in encoded.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(chunk)
            .map_err(|_| Error::Protocol("invalid setup gc candidate"))?;
        bytes.push(
            u8::from_str_radix(text, 16)
                .map_err(|_| Error::Protocol("invalid setup gc candidate"))?,
        );
    }
    let mut fields = bytes.split(|value| *value == 0);
    let field = |value: Option<&[u8]>| -> Result<String, Error> {
        let value = value.ok_or(Error::Protocol("invalid setup gc candidate"))?;
        if value.is_empty() || value.len() > 8 * 1024 || value.contains(&0) {
            return Err(Error::Protocol("invalid setup gc candidate"));
        }
        std::str::from_utf8(value)
            .map(str::to_owned)
            .map_err(|_| Error::Protocol("invalid setup gc candidate"))
    };
    let result = (
        field(fields.next())?,
        field(fields.next())?,
        field(fields.next())?,
    );
    if fields.next() != Some(&[]) || fields.next().is_some() {
        return Err(Error::Protocol("invalid setup gc candidate"));
    }
    Ok(result)
}

fn parse_setup_reconcile_missing_token(token: &str) -> Result<(String, String, String), Error> {
    let normalized = token
        .strip_prefix("srm1-")
        .ok_or(Error::Protocol("invalid setup reconcile repair candidate"))?;
    parse_setup_gc_token(&format!("sgc1-{normalized}"))
        .map_err(|_| Error::Protocol("invalid setup reconcile repair candidate"))
}

/// One redacted setup-ensure registry event. Event details are authored by the
/// daemon's allowlisted event formatter, not copied from config, engine, or
/// workspace input.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupEnsureEventDiagnostic {
    pub cursor: u64,
    pub at: f64,
    pub kind: String,
    pub detail: String,
}

/// Bounded offset-cursor page of recent setup-ensure history, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupEnsureEventPage {
    pub next: Option<u64>,
    pub records: Vec<SetupEnsureEventDiagnostic>,
}

/// Bounded stable diagnostic result. A daemon-unavailable result is produced
/// by the client without opening a registry; all other fields are authored by
/// the existing daemon after its read-only integrity check and fixed engine
/// probe. No paths, raw process output, endpoint names, or credentials occur.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorReport {
    pub daemon: String,
    pub registry: String,
    pub engine: String,
    pub client_version: Option<String>,
    pub server_version: Option<String>,
}
impl DoctorReport {
    fn daemon_unavailable() -> Self {
        Self {
            daemon: "unavailable".into(),
            registry: "unavailable".into(),
            engine: "unavailable".into(),
            client_version: None,
            server_version: None,
        }
    }
    fn from_engine(registry: &'static str, engine: DockerDoctorReport) -> Self {
        Self {
            daemon: "ready".into(),
            registry: registry.into(),
            engine: match engine.state {
                DockerDoctorState::Ready => "ready",
                DockerDoctorState::Unavailable => "unavailable",
                DockerDoctorState::Deadline => "deadline",
                DockerDoctorState::OutputLimit => "output_limit",
                DockerDoctorState::InvalidResponse => "invalid_response",
            }
            .into(),
            client_version: engine.client_version,
            server_version: engine.server_version,
        }
    }
}

fn resource_diagnostic(value: Resource) -> RegistryResourceDiagnostic {
    RegistryResourceDiagnostic {
        id: value.id,
        kind: value.kind.as_str().into(),
        name: value.name,
        stack: value.stack,
        generation: value.generation,
        state: value.state.as_str().into(),
        retention: value.retention.as_str().into(),
        created_at: value.created_at,
        last_used: value.last_used,
    }
}

fn event_diagnostic(value: Event) -> SetupEnsureEventDiagnostic {
    SetupEnsureEventDiagnostic {
        cursor: u64::try_from(value.id).unwrap_or(0),
        at: value.at,
        kind: value.kind,
        detail: value.detail,
    }
}

fn validate_registry_page(after: u64, limit: u32) -> Result<(), Error> {
    let _ = usize::try_from(after).map_err(|_| Error::Protocol("invalid registry cursor"))?;
    if limit == 0 || limit > MAX_REGISTRY_DIAGNOSTIC_PAGE {
        return Err(Error::Protocol("invalid registry page limit"));
    }
    Ok(())
}

/// Diagnostic operations have no caller-selected files, jobs, setup values, or
/// engine controls. Rejecting stray fields makes the private wire contract as
/// narrow as every public front end rather than silently accepting ambiguity.
fn validate_registry_diagnostics_request_wire(request: &Request) -> Result<(), Error> {
    validate_registry_page(request.diagnostic_after, request.diagnostic_limit)?;
    if !request.workspace.is_empty()
        || !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_config.is_empty()
        || request.setup_policy != 0
        || request.setup_deadline_ms != 0
        || request.setup_output_limit != 0
        || !request.setup_task_name.is_empty()
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic registry diagnostic fields"));
    }
    Ok(())
}

fn validate_setup_gc_preview_request_wire(request: &Request) -> Result<(), Error> {
    validate_registry_page(request.diagnostic_after, request.diagnostic_limit)?;
    if request.workspace.is_empty()
        || request.workspace.len() > 8 * 1024
        || request.workspace.bytes().any(|byte| byte == 0)
        || !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_config.is_empty()
        || request.setup_policy != 0
        || request.setup_deadline_ms != 0
        || request.setup_output_limit != 0
        || !request.setup_task_name.is_empty()
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic setup gc preview fields"));
    }
    Ok(())
}

fn validate_setup_reconcile_preview_request_wire(request: &Request) -> Result<(), Error> {
    validate_setup_gc_preview_request_wire(request)
        .map_err(|_| Error::Protocol("nonsemantic setup reconcile preview fields"))
}

fn validate_setup_reconcile_repair_missing_input(
    workspace: &str,
    token: &str,
    confirm: bool,
) -> Result<(), Error> {
    if workspace.is_empty()
        || workspace.len() > 8 * 1024
        || workspace.bytes().any(|byte| byte == 0)
        || !confirm
    {
        return Err(Error::Protocol("invalid setup reconcile repair request"));
    }
    let _ = parse_setup_reconcile_missing_token(token)?;
    Ok(())
}

fn validate_setup_reconcile_repair_missing_request_wire(request: &Request) -> Result<(), Error> {
    validate_setup_reconcile_repair_missing_input(
        &request.workspace,
        &request.gc_candidate_token,
        request.gc_confirm,
    )?;
    if !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_config.is_empty()
        || request.setup_policy != 0
        || request.setup_deadline_ms != 0
        || request.setup_output_limit != 0
        || !request.setup_task_name.is_empty()
        || request.diagnostic_after != 0
        || request.diagnostic_limit != 0
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic setup reconcile repair fields"));
    }
    Ok(())
}

fn validate_setup_gc_apply_input(workspace: &str, token: &str, confirm: bool) -> Result<(), Error> {
    if workspace.is_empty()
        || workspace.len() > 8 * 1024
        || workspace.bytes().any(|byte| byte == 0)
        || !confirm
    {
        return Err(Error::Protocol("invalid setup gc apply request"));
    }
    let _ = parse_setup_gc_token(token)?;
    Ok(())
}

fn validate_setup_retired_stop_input(
    workspace: &str,
    token: &str,
    confirm: bool,
) -> Result<(), Error> {
    validate_setup_gc_apply_input(workspace, token, confirm)
        .map_err(|_| Error::Protocol("invalid setup retired stop request"))
}

fn validate_setup_gc_apply_request_wire(request: &Request) -> Result<(), Error> {
    validate_setup_gc_apply_input(
        &request.workspace,
        &request.gc_candidate_token,
        request.gc_confirm,
    )?;
    if !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_config.is_empty()
        || request.setup_policy != 0
        || request.setup_deadline_ms != 0
        || request.setup_output_limit != 0
        || !request.setup_task_name.is_empty()
        || request.diagnostic_after != 0
        || request.diagnostic_limit != 0
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic setup gc apply fields"));
    }
    Ok(())
}

fn validate_setup_retired_stop_request_wire(request: &Request) -> Result<(), Error> {
    validate_setup_retired_stop_input(
        &request.workspace,
        &request.gc_candidate_token,
        request.gc_confirm,
    )?;
    if !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_config.is_empty()
        || request.setup_policy != 0
        || request.setup_deadline_ms != 0
        || request.setup_output_limit != 0
        || !request.setup_task_name.is_empty()
        || request.diagnostic_after != 0
        || request.diagnostic_limit != 0
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic setup retired stop fields"));
    }
    Ok(())
}

fn validate_setup_done_input(workspace: &str, confirm: bool) -> Result<(), Error> {
    if workspace.is_empty()
        || workspace.len() > 8 * 1024
        || workspace.bytes().any(|byte| byte == 0)
        || !confirm
    {
        return Err(Error::Protocol("invalid setup done request"));
    }
    Ok(())
}
/// Completion names the same canonical directory spelling recorded by setup
/// planning/ensure. Alias spellings therefore cannot accidentally become a
/// second registry scope. This performs only local path observation before
/// any daemon request and deliberately does not create anything.
fn canonical_setup_done_workspace(path: impl AsRef<Path>) -> Result<String, Error> {
    let canonical = fs::canonical_context_path(path.as_ref())
        .map_err(|_| Error::Protocol("setup done workspace cannot be canonicalized"))?;
    let metadata = fs::context_path_metadata_no_follow(&canonical)
        .map_err(|_| Error::Protocol("setup done workspace cannot be inspected"))?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err(Error::Protocol("setup done workspace is not a directory"));
    }
    let value = canonical.to_string_lossy().into_owned();
    validate_setup_done_input(&value, true)?;
    Ok(value)
}
fn validate_setup_done_request_wire(request: &Request) -> Result<(), Error> {
    validate_setup_done_input(&request.workspace, request.setup_done_confirm)?;
    if !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_config.is_empty()
        || request.setup_policy != 0
        || request.setup_deadline_ms != 0
        || request.setup_output_limit != 0
        || !request.setup_task_name.is_empty()
        || request.diagnostic_after != 0
        || request.diagnostic_limit != 0
        || !request.gc_candidate_token.is_empty()
        || request.gc_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic setup done fields"));
    }
    Ok(())
}

/// `doctor` has no parameters. In particular, it cannot inherit diagnostic
/// pagination, setup policy, filesystem, job, output, or engine controls.
fn validate_doctor_request_wire(request: &Request) -> Result<(), Error> {
    if !request.workspace.is_empty()
        || !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_config.is_empty()
        || request.setup_policy != 0
        || request.setup_deadline_ms != 0
        || request.setup_output_limit != 0
        || !request.setup_task_name.is_empty()
        || request.diagnostic_after != 0
        || request.diagnostic_limit != 0
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic doctor fields"));
    }
    Ok(())
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
    /// Perform the fixed daemon-owned read-only registry integrity and Docker
    /// version checks. An absent/unreachable daemon is a stable outcome, not a
    /// client-side registry open or an unredacted transport error.
    pub async fn doctor(&self) -> Result<DoctorReport, Error> {
        match self.call(Request::operation(13)).await {
            Ok(Reply::Doctor(v)) => Ok(v),
            Ok(_) => Err(Error::Protocol("unexpected doctor response")),
            Err(
                Error::Deadline | Error::Io(_) | Error::ActorClosed | Error::EndpointOccupied(_),
            ) => Ok(DoctorReport::daemon_unavailable()),
            Err(error) => Err(error),
        }
    }
    /// Read a bounded, path-safe page of managed resource diagnostics from the
    /// already-running daemon. This never opens, creates, or migrates a
    /// registry in the client process.
    pub async fn registry_resources(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<RegistryResourcePage, Error> {
        validate_registry_page(after, limit)?;
        match self
            .call(Request {
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(11)
            })
            .await?
        {
            Reply::RegistryResources(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected registry resources response")),
        }
    }
    /// Read a bounded, newest-first page of credential-safe setup ensure
    /// history from the already-running daemon. This has no registry write
    /// path and never initializes state in the client process.
    pub async fn setup_ensure_events(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<SetupEnsureEventPage, Error> {
        validate_registry_page(after, limit)?;
        match self
            .call(Request {
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(12)
            })
            .await?
        {
            Reply::SetupEnsureEvents(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected setup ensure events response")),
        }
    }
    /// Return a read-only, conservative future-GC preview for one exact
    /// workspace. No client-side registry open, Docker call, or mutation is
    /// possible through this method.
    pub async fn setup_gc_preview(
        &self,
        workspace: impl AsRef<Path>,
        after: u64,
        limit: u32,
    ) -> Result<SetupGcPreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        if workspace.is_empty()
            || workspace.len() > 8 * 1024
            || workspace.bytes().any(|byte| byte == 0)
        {
            return Err(Error::Protocol("invalid setup gc preview workspace"));
        }
        match self
            .call(Request {
                workspace,
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(14)
            })
            .await?
        {
            Reply::SetupGcPreview(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected setup gc preview response")),
        }
    }
    /// Compare durable Bosn-managed setup container facts with fixed Docker
    /// inspection. This is read-only: it never creates, opens, writes, or
    /// migrates a registry and has no repair/apply operation.
    pub async fn setup_reconcile_preview(
        &self,
        workspace: impl AsRef<Path>,
        after: u64,
        limit: u32,
    ) -> Result<SetupReconcilePreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        if workspace.is_empty()
            || workspace.len() > 8 * 1024
            || workspace.bytes().any(|byte| byte == 0)
        {
            return Err(Error::Protocol("invalid setup reconcile preview workspace"));
        }
        match self
            .call(Request {
                workspace,
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(19)
            })
            .await?
        {
            Reply::SetupReconcilePreview(v) => Ok(v),
            _ => Err(Error::Protocol(
                "unexpected setup reconcile preview response",
            )),
        }
    }
    /// Retire exactly one previewed active setup app only after fixed Docker
    /// inspection still proves it absent. This never accepts an engine name,
    /// image, argv, mount, or lifecycle control.
    pub async fn setup_reconcile_repair_missing(
        &self,
        workspace: impl AsRef<Path>,
        candidate_token: &str,
        confirm: bool,
    ) -> Result<SetupReconcileMissingRepairResult, Error> {
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        validate_setup_reconcile_repair_missing_input(&workspace, candidate_token, confirm)?;
        match self
            .call(Request {
                workspace,
                gc_candidate_token: candidate_token.into(),
                gc_confirm: true,
                ..Request::operation(20)
            })
            .await?
        {
            Reply::SetupReconcileMissingRepair(value) => Ok(value),
            _ => Err(Error::Protocol(
                "unexpected setup reconcile repair response",
            )),
        }
    }
    /// Apply exactly one opaque candidate returned by a preceding GC preview.
    /// Docker and registry mutation remain daemon-owned; no raw engine target
    /// crosses this boundary.
    pub async fn setup_gc_apply(
        &self,
        workspace: impl AsRef<Path>,
        candidate_token: &str,
        confirm: bool,
    ) -> Result<SetupGcApplyResult, Error> {
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        validate_setup_gc_apply_input(&workspace, candidate_token, confirm)?;
        match self
            .call(Request {
                workspace,
                gc_candidate_token: candidate_token.into(),
                gc_confirm: true,
                ..Request::operation(15)
            })
            .await?
        {
            Reply::SetupGcApply(value) => Ok(value),
            _ => Err(Error::Protocol("unexpected setup gc apply response")),
        }
    }
    /// Stop exactly one opaque retired candidate returned by GC preview. The
    /// daemon revalidates durable ownership and fixed Docker labels; the
    /// caller cannot select a Docker name, image, argv, or timeout.
    pub async fn setup_stop_retired(
        &self,
        workspace: impl AsRef<Path>,
        candidate_token: &str,
        confirm: bool,
    ) -> Result<SetupRetiredStopResult, Error> {
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        validate_setup_retired_stop_input(&workspace, candidate_token, confirm)?;
        match self
            .call(Request {
                workspace,
                gc_candidate_token: candidate_token.into(),
                gc_confirm: true,
                ..Request::operation(18)
            })
            .await?
        {
            Reply::SetupRetiredStop(value) => Ok(value),
            _ => Err(Error::Protocol("unexpected setup retired stop response")),
        }
    }
    /// Explicitly mark this setup workspace's active registry ownership done.
    /// This does not contact Docker or remove any resource. `confirm` is
    /// required so normal inspection cannot accidentally change lifecycle
    /// accounting.
    pub async fn setup_done(
        &self,
        workspace: impl AsRef<Path>,
        confirm: bool,
    ) -> Result<SetupDoneResult, Error> {
        if !confirm {
            return Err(Error::Protocol("invalid setup done request"));
        }
        let workspace = canonical_setup_done_workspace(workspace)?;
        match self
            .call(Request {
                workspace,
                setup_done_confirm: true,
                ..Request::operation(16)
            })
            .await?
        {
            Reply::SetupDone(value) => Ok(value),
            _ => Err(Error::Protocol("unexpected setup done response")),
        }
    }
    /// Confirmed, daemon-owned restoration of registry facts for one existing
    /// managed setup app. This never accepts a container/image selector.
    pub async fn setup_adopt(&self, request: SetupAdoptRequest) -> Result<SetupAdoptResult, Error> {
        let workspace = request.workspace.to_string_lossy().into_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_adopt_input(
            &workspace,
            &request.config,
            request.policy,
            deadline_ms,
            output_limit,
            request.confirm,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                setup_adopt_confirm: true,
                ..Request::operation(17)
            })
            .await?
        {
            Reply::SetupAdopt(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected setup adopt response")),
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
                workspace: workspace.into(),
                stack: stack.into(),
                digest: digest.into(),
                ..Request::operation(4)
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
    /// Submit one task declared by a setup document for execution inside its
    /// already ensured, ownership-verified application container.
    pub async fn submit_setup_app_task(
        &self,
        request: SetupAppTaskJobRequest,
    ) -> Result<u64, Error> {
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
                ..Request::operation(21)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected setup app task response")),
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
    /// Submit one explicit manifest stack runtime ensure. The selected stack
    /// is a manifest declaration, never a Docker target or command.
    pub async fn submit_manifest_ensure(
        &self,
        request: ManifestEnsureJobRequest,
    ) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("manifest workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("manifest deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("manifest output limit too large"))?;
        validate_manifest_ensure_wire(
            &workspace,
            &request.manifest,
            &request.stack,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                stack: request.stack,
                setup_config: request.manifest,
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(22)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected manifest ensure response")),
        }
    }
    /// Submit one named task for an already ensured supported manifest stack.
    /// The task name is the only executable selector; manifest command and
    /// managed container identity are re-derived by the daemon.
    pub async fn submit_manifest_app_task(
        &self,
        request: ManifestAppTaskJobRequest,
    ) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("manifest workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("manifest deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("manifest output limit too large"))?;
        validate_manifest_app_task_wire(
            &workspace,
            &request.manifest,
            &request.stack,
            &request.task_name,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                stack: request.stack,
                setup_config: request.manifest,
                setup_task_name: request.task_name,
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(23)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected manifest app task response")),
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
    setup_app_task_executor: Arc<dyn SetupAppTaskExecutor>,
    setup_ensure_executor: Arc<dyn SetupEnsureExecutor>,
    manifest_ensure_executor: Arc<dyn ManifestEnsureExecutor>,
    manifest_app_task_executor: Arc<dyn ManifestAppTaskExecutor>,
    setup_adopt_executor: Arc<dyn SetupAdoptExecutor>,
    doctor_executor: Arc<dyn DoctorExecutor>,
    setup_reconcile_executor: Arc<dyn SetupReconcileExecutor>,
}

#[cfg(test)]
struct SetupEnsureRecordGate {
    entered: async_engine::Sender<()>,
    release: async_engine::Receiver<()>,
}
#[derive(Clone)]
pub struct RegistryActor {
    sender: async_engine::Sender<DbCommand>,
}
enum DbCommand {
    Status(async_engine::OneshotSender<Result<Status, Error>>),
    DoctorIntegrity(async_engine::OneshotSender<&'static str>),
    Resources {
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<RegistryResourcePage, Error>>,
    },
    SetupEnsureEvents {
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<SetupEnsureEventPage, Error>>,
    },
    SetupGcPreview {
        workspace: String,
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<SetupGcPreviewPage, Error>>,
    },
    SetupReconcilePreview {
        workspace: String,
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<SetupReconcileCandidates, Error>>,
    },
    SetupMissingRepairCandidate {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    RepairMissingSetupContainer {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply:
            async_engine::OneshotSender<Result<Option<bosn_registry::SetupMissingRepair>, Error>>,
    },
    SetupGcCandidate {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply: async_engine::OneshotSender<Result<Option<bosn_registry::SetupGcCandidate>, Error>>,
    },
    FinalizeSetupGc {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        missing: bool,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    ConfirmSetupRetiredStopped {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    CompleteSetupWorkspace {
        workspace: String,
        reply: async_engine::OneshotSender<Result<SetupDoneResult, Error>>,
    },
    AppendSetupEnsureEvents {
        events: Vec<SetupEnsureEvent>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    RecordSetupEnsure {
        job_id: u64,
        execution: Box<SetupEnsureExecution>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    RecordManifestEnsure {
        job_id: u64,
        execution: Box<SetupEnsureExecution>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    PutManifestVolumeIntents {
        volumes: Vec<ManifestVolumeResource>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    RecordSetupAdoption {
        execution: Box<SetupEnsureExecution>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    BeginSetupAppTaskSession {
        job_id: u64,
        container_id: String,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    BeginManifestAppTaskSession {
        job_id: u64,
        container_id: String,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    FinishSetupAppTaskSession {
        job_id: u64,
        outcome: &'static str,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    FinishManifestAppTaskSession {
        job_id: u64,
        outcome: &'static str,
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
    SubmitSetupAppTask {
        request: SetupAppTaskJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitSetupEnsure {
        request: SetupEnsureJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitManifestEnsure {
        request: ManifestEnsureJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitManifestAppTask {
        request: ManifestAppTaskJobRequest,
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
    PersistManifestEnsure {
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum SetupJobKind {
    Prepare,
    Task,
    AppTask,
    Ensure,
    ManifestEnsure,
    ManifestAppTask,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupEnsureEventOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SetupEnsureEvent {
    kind: &'static str,
    detail: String,
}

impl SetupEnsureEvent {
    fn submitted(id: u64, request: &SetupEnsureJobRequest) -> Self {
        let policy = match request.policy {
            SetupPreparePolicy::Refresh => "refresh",
            SetupPreparePolicy::Offline => "offline",
        };
        // The event stream is diagnostic metadata, not a configuration
        // archive. In particular, locators can contain credentials, signed
        // query strings, or sensitive local path components.
        let source = config_locator_kind(&request.config);
        Self {
            kind: "setup.ensure.submitted",
            detail: format!("job_id={id} policy={policy} source={source}"),
        }
    }

    fn terminal(id: u64, outcome: SetupEnsureEventOutcome) -> Self {
        let (kind, outcome) = match outcome {
            SetupEnsureEventOutcome::Succeeded => ("setup.ensure.succeeded", "succeeded"),
            SetupEnsureEventOutcome::Failed => ("setup.ensure.failed", "failed"),
            SetupEnsureEventOutcome::Cancelled => ("setup.ensure.cancelled", "cancelled"),
            SetupEnsureEventOutcome::Superseded => ("setup.ensure.superseded", "superseded"),
        };
        Self {
            kind,
            // Never include executor receipts, engine output, workspace paths,
            // container IDs, or a config locator in terminal diagnostics.
            detail: format!("job_id={id} outcome={outcome}"),
        }
    }
}

fn config_locator_kind(config: &str) -> &'static str {
    if config.starts_with("https://") {
        "https"
    } else if config.starts_with("http://") {
        "http"
    } else if config.starts_with("file://") {
        "file"
    } else {
        "path"
    }
}

enum SetupJobRequest {
    Prepare(SetupPrepareRequest),
    Task(SetupTaskJobRequest),
    AppTask(SetupAppTaskJobRequest),
    Ensure(SetupEnsureJobRequest),
    ManifestEnsure(ManifestEnsureJobRequest),
    ManifestAppTask(ManifestAppTaskJobRequest),
}

#[derive(Clone)]
struct SetupExecutors {
    prepare: Arc<dyn SetupPrepareExecutor>,
    task: Arc<dyn SetupTaskExecutor>,
    app_task: Arc<dyn SetupAppTaskExecutor>,
    ensure: Arc<dyn SetupEnsureExecutor>,
    manifest_ensure: Arc<dyn ManifestEnsureExecutor>,
    manifest_app_task: Arc<dyn ManifestAppTaskExecutor>,
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
    async fn submit_setup_app_task(&self, request: SetupAppTaskJobRequest) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitSetupAppTask { request, reply })
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
    async fn submit_manifest_ensure(
        &self,
        request: ManifestEnsureJobRequest,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitManifestEnsure { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn submit_manifest_app_task(
        &self,
        request: ManifestAppTaskJobRequest,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitManifestAppTask { request, reply })
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
    // Job status is intentionally in-memory in this milestone. Keep just
    // enough typed identity to make its durable setup-ensure audit trail
    // complete without treating generic jobs as setup operations.
    let mut setup_kinds: BTreeMap<u64, SetupJobKind> = BTreeMap::new();
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
                        if setup_kinds.remove(&id) == Some(SetupJobKind::Ensure) {
                            let _ = registry
                                .append_setup_ensure_events(vec![SetupEnsureEvent::terminal(
                                    id,
                                    SetupEnsureEventOutcome::Cancelled,
                                )])
                                .await;
                        }
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
                    registry.clone(),
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
                    registry.clone(),
                );
            }
            JobCommand::SubmitSetupAppTask { request, reply } => {
                let digest = setup_app_task_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = jobs
                    .submit(&workspace, "setup-app-task", &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::AppTask(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::AppTask(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("setup app task job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitSetupEnsure { request, reply } => {
                let digest = setup_ensure_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = match jobs.submit(&workspace, "setup-ensure", &digest) {
                    Ok(Submission::Joined(id)) => Ok(id),
                    Ok(Submission::Started(id)) | Ok(Submission::Queued(id)) => {
                        match registry
                            .append_setup_ensure_events(vec![SetupEnsureEvent::submitted(
                                id, &request,
                            )])
                            .await
                        {
                            Ok(()) => {
                                setup_kinds.insert(id, SetupJobKind::Ensure);
                                requests.insert(id, SetupJobRequest::Ensure(request));
                                Ok(id)
                            }
                            Err(error) => {
                                // Do not launch an operation whose durable
                                // submission audit could not be written.
                                let _ = jobs.settle_with_error(
                                    id,
                                    false,
                                    Some("setup ensure submission audit unavailable".into()),
                                );
                                Err(error)
                            }
                        }
                    }
                    Ok(Submission::Superseded { job, replacement }) => {
                        let mut events = Vec::new();
                        if setup_kinds.get(&job) == Some(&SetupJobKind::Ensure) {
                            events.push(SetupEnsureEvent::terminal(
                                job,
                                SetupEnsureEventOutcome::Superseded,
                            ));
                        }
                        events.push(SetupEnsureEvent::submitted(replacement, &request));
                        match registry.append_setup_ensure_events(events).await {
                            Ok(()) => {
                                setup_kinds.remove(&job);
                                requests.remove(&job);
                                setup_kinds.insert(replacement, SetupJobKind::Ensure);
                                requests.insert(replacement, SetupJobRequest::Ensure(request));
                                Ok(replacement)
                            }
                            Err(error) => {
                                let _ = jobs.settle_with_error(
                                    replacement,
                                    false,
                                    Some("setup ensure submission audit unavailable".into()),
                                );
                                requests.remove(&job);
                                setup_kinds.remove(&job);
                                Err(error)
                            }
                        }
                    }
                    Err(_) => Err(Error::Protocol("setup ensure job admission")),
                };
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitManifestEnsure { request, reply } => {
                let digest = manifest_ensure_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let job_stack = format!("manifest-ensure:{}", request.stack);
                let result = jobs
                    .submit(&workspace, &job_stack, &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::ManifestEnsure(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::ManifestEnsure(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("manifest ensure job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitManifestAppTask { request, reply } => {
                let digest = manifest_app_task_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let job_stack = format!("manifest-app-task:{}", request.stack);
                let result = jobs
                    .submit(&workspace, &job_stack, &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::ManifestAppTask(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::ManifestAppTask(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("manifest app task job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
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
                        .record_setup_ensure(id, execution)
                        .await
                        .map_err(|error| format!("setup ensure registry recording failed: {error}"))
                        .map(|()| {
                            // Settle before accepting another command. A
                            // cancellation processed before this command has
                            // already changed the state to Cancelling and is
                            // rejected above; a later cancellation observes a
                            // terminal success and cannot be accepted.
                            cancellations.remove(&id);
                            setup_kinds.remove(&id);
                            let _ = jobs.log(id, bounded_log_line(&receipt));
                            let _ = jobs.settle_with_error(id, true, None);
                        })
                } else {
                    Err("setup ensure cancelled".into())
                };
                let _ = reply.send(result);
            }
            JobCommand::PersistManifestEnsure {
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
                        .record_manifest_ensure(id, execution)
                        .await
                        .map_err(|error| {
                            format!("manifest ensure registry recording failed: {error}")
                        })
                        .map(|()| {
                            cancellations.remove(&id);
                            let _ = jobs.log(id, bounded_log_line(&receipt));
                            let _ = jobs.settle_with_error(id, true, None);
                        })
                } else {
                    Err("manifest ensure cancelled".into())
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
                let terminal_outcome = if matches!(kind, SetupJobKind::Ensure) {
                    let cancelled = jobs
                        .job(id)
                        .is_ok_and(|job| job.state == jobs::JobState::Cancelling);
                    Some(if cancelled {
                        SetupEnsureEventOutcome::Cancelled
                    } else if result.is_ok() {
                        SetupEnsureEventOutcome::Succeeded
                    } else {
                        SetupEnsureEventOutcome::Failed
                    })
                } else {
                    None
                };
                if let Some(outcome) = terminal_outcome {
                    // A successful ensure settles in PersistSetupEnsure with
                    // its event and resources in one transaction. This branch
                    // therefore only records failed/cancelled terminal work.
                    if outcome != SetupEnsureEventOutcome::Succeeded
                        && setup_kinds.get(&id) == Some(&SetupJobKind::Ensure)
                    {
                        let _ = registry
                            .append_setup_ensure_events(vec![SetupEnsureEvent::terminal(
                                id, outcome,
                            )])
                            .await;
                    }
                }
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
                            SetupJobKind::AppTask => "setup app task",
                            SetupJobKind::Ensure => "setup ensure",
                            SetupJobKind::ManifestEnsure => "manifest ensure",
                            SetupJobKind::ManifestAppTask => "manifest app task",
                        };
                        let _ = jobs.log(id, format!("{operation} failed: {error}"));
                        let _ = jobs.settle_with_error(id, false, Some(error));
                    }
                }
                if jobs.job(id).is_ok_and(|job| job.state.terminal()) {
                    setup_kinds.remove(&id);
                }
            }
            JobCommand::Stop(reply) => {
                jobs.close();
                let cancelled: Vec<SetupEnsureEvent> = setup_kinds
                    .iter()
                    .filter_map(|(&id, kind)| {
                        (*kind == SetupJobKind::Ensure
                            && jobs
                                .job(id)
                                .is_ok_and(|job| job.state == jobs::JobState::Cancelled))
                        .then_some(SetupEnsureEvent::terminal(
                            id,
                            SetupEnsureEventOutcome::Cancelled,
                        ))
                    })
                    .collect();
                if !cancelled.is_empty() {
                    let _ = registry.append_setup_ensure_events(cancelled).await;
                }
                setup_kinds.retain(|&id, _| {
                    !jobs
                        .job(id)
                        .is_ok_and(|job| job.state == jobs::JobState::Cancelled)
                });
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
                registry.clone(),
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
    registry: RegistryActor,
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
        let session_recorder = ActorSetupAppTaskSessionRecorder {
            actor: registry.clone(),
            job_id: id,
        };
        let prepare_executor = Arc::clone(&executors.prepare);
        let task_executor = Arc::clone(&executors.task);
        let app_task_executor = Arc::clone(&executors.app_task);
        let ensure_executor = Arc::clone(&executors.ensure);
        let manifest_ensure_executor = Arc::clone(&executors.manifest_ensure);
        let manifest_app_task_executor = Arc::clone(&executors.manifest_app_task);
        let manifest_session_recorder = ActorManifestAppTaskSessionRecorder {
            actor: registry.clone(),
            job_id: id,
        };
        let manifest_registry = registry.clone();
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
                SetupJobRequest::AppTask(request) => Some((
                    SetupJobKind::AppTask,
                    app_task_executor
                        .execute(request, &token, &logs, &session_recorder)
                        .await,
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
                SetupJobRequest::ManifestEnsure(request) => {
                    let result = manifest_ensure_executor
                        .execute(request, &token, &logs, &manifest_registry)
                        .await;
                    match result {
                        Ok(execution) => {
                            let (reply, wait) = async_engine::oneshot_channel();
                            let persisted = if task_sender
                                .send(JobCommand::PersistManifestEnsure {
                                    id,
                                    execution,
                                    reply,
                                })
                                .await
                                .is_err()
                            {
                                Err("manifest ensure registry actor stopped".to_owned())
                            } else {
                                match wait.await {
                                    Ok(result) => result,
                                    Err(_) => {
                                        Err("manifest ensure registry actor stopped".to_owned())
                                    }
                                }
                            };
                            persisted
                                .err()
                                .map(|error| (SetupJobKind::ManifestEnsure, Err(error)))
                        }
                        Err(error) => Some((SetupJobKind::ManifestEnsure, Err(error))),
                    }
                }
                SetupJobRequest::ManifestAppTask(request) => Some((
                    SetupJobKind::ManifestAppTask,
                    manifest_app_task_executor
                        .execute(request, &token, &logs, &manifest_session_recorder)
                        .await,
                )),
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

#[derive(Clone)]
struct ActorSetupAppTaskSessionRecorder {
    actor: RegistryActor,
    job_id: u64,
}

impl SetupAppTaskSessionRecorder for ActorSetupAppTaskSessionRecorder {
    fn begin<'a>(
        &'a self,
        managed_container_identity: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .begin_setup_app_task_session(self.job_id, managed_container_identity)
                .await
                .map_err(|_| "registry session start failed".into())
        })
    }
    fn finish<'a>(
        &'a self,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .finish_setup_app_task_session(self.job_id, outcome)
                .await
                .map_err(|_| "registry session finish failed".into())
        })
    }
}

#[derive(Clone)]
struct ActorManifestAppTaskSessionRecorder {
    actor: RegistryActor,
    job_id: u64,
}
impl ManifestAppTaskSessionRecorder for ActorManifestAppTaskSessionRecorder {
    fn begin<'a>(
        &'a self,
        managed_container_identity: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .begin_manifest_app_task_session(self.job_id, managed_container_identity)
                .await
                .map_err(|_| "registry session start failed".into())
        })
    }
    fn finish<'a>(
        &'a self,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .finish_manifest_app_task_session(self.job_id, outcome)
                .await
                .map_err(|_| "registry session finish failed".into())
        })
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

fn setup_app_task_digest(request: &SetupAppTaskJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let policy = request.policy.wire().to_le_bytes();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.setup-app-task.v1".as_slice(),
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

fn manifest_ensure_digest(request: &ManifestEnsureJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.manifest-ensure.v1".as_slice(),
        workspace.as_bytes(),
        request.manifest.as_bytes(),
        request.stack.as_bytes(),
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "manifest:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}
fn manifest_app_task_digest(request: &ManifestAppTaskJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.manifest-app-task.v1".as_slice(),
        workspace.as_bytes(),
        request.manifest.as_bytes(),
        request.stack.as_bytes(),
        request.task_name.as_bytes(),
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "manifest:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}

/// Select the durable registry key from a receipt that has already passed
/// `adopt_setup_app`'s complete ownership validation. Docker's opaque ID is
/// useful in the operation receipt, but registry resource ownership and GC
/// use the exact content-addressed managed name.
fn setup_app_task_session_container_identity(observed: &SetupEnsureResult) -> String {
    observed.container_name.clone()
}
fn manifest_app_task_session_container_identity(observed: &SetupEnsureResult) -> String {
    observed.container_name.clone()
}

impl RegistryActor {
    async fn put_manifest_volume_intents(
        &self,
        volumes: Vec<ManifestVolumeResource>,
    ) -> Result<(), Error> {
        if volumes.is_empty() {
            return Ok(());
        }
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::PutManifestVolumeIntents { volumes, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn begin_setup_app_task_session(
        &self,
        job_id: u64,
        managed_container_identity: String,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::BeginSetupAppTaskSession {
                job_id,
                container_id: managed_container_identity,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn finish_setup_app_task_session(
        &self,
        job_id: u64,
        outcome: &'static str,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::FinishSetupAppTaskSession {
                job_id,
                outcome,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn begin_manifest_app_task_session(
        &self,
        job_id: u64,
        managed_container_identity: String,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::BeginManifestAppTaskSession {
                job_id,
                container_id: managed_container_identity,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn finish_manifest_app_task_session(
        &self,
        job_id: u64,
        outcome: &'static str,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::FinishManifestAppTaskSession {
                job_id,
                outcome,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn status(&self) -> Result<Status, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::Status(reply))
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn doctor_integrity(&self) -> &'static str {
        let (reply, wait) = async_engine::oneshot_channel();
        if self
            .sender
            .send(DbCommand::DoctorIntegrity(reply))
            .await
            .is_err()
        {
            return "unavailable";
        }
        match async_engine::timeout(DOCTOR_REGISTRY_DEADLINE, wait).await {
            Ok(Ok(state)) => state,
            Ok(Err(_)) => "unavailable",
            Err(_) => "deadline",
        }
    }
    async fn resources(&self, after: u64, limit: u32) -> Result<RegistryResourcePage, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::Resources {
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn setup_ensure_events(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<SetupEnsureEventPage, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupEnsureEvents {
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn setup_gc_preview(
        &self,
        workspace: String,
        after: u64,
        limit: u32,
    ) -> Result<SetupGcPreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupGcPreview {
                workspace,
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn setup_reconcile_preview(
        &self,
        workspace: String,
        after: u64,
        limit: u32,
    ) -> Result<SetupReconcileCandidates, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupReconcilePreview {
                workspace,
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn repair_missing_setup_container(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<Option<bosn_registry::SetupMissingRepair>, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RepairMissingSetupContainer {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn setup_missing_repair_candidate(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupMissingRepairCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn setup_gc_candidate(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<Option<bosn_registry::SetupGcCandidate>, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupGcCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn finalize_setup_gc(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
        missing: bool,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::FinalizeSetupGc {
                workspace,
                id,
                name,
                generation,
                missing,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn confirm_setup_retired_stopped(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ConfirmSetupRetiredStopped {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn complete_setup_workspace(&self, workspace: String) -> Result<SetupDoneResult, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::CompleteSetupWorkspace { workspace, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn append_setup_ensure_events(&self, events: Vec<SetupEnsureEvent>) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::AppendSetupEnsureEvents { events, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn record_setup_ensure(
        &self,
        job_id: u64,
        execution: SetupEnsureExecution,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RecordSetupEnsure {
                job_id,
                execution: Box::new(execution),
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn record_manifest_ensure(
        &self,
        job_id: u64,
        execution: SetupEnsureExecution,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RecordManifestEnsure {
                job_id,
                execution: Box::new(execution),
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn record_setup_adoption(&self, execution: SetupEnsureExecution) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RecordSetupAdoption {
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
            DbCommand::DoctorIntegrity(reply) => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.integrity_check();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(if result.is_ok() { "ready" } else { "failed" });
                    }
                    Err(_) => {
                        let _ = reply.send("unavailable");
                        return;
                    }
                }
            }
            DbCommand::Resources {
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = usize::try_from(after)
                        .map_err(|_| bosn_registry::Error::BadRow("page offset"))
                        .and_then(|after| registry.resources(after, limit as usize))
                        .map(|page| RegistryResourcePage {
                            next: page.next_offset.map(|value| value as u64),
                            records: page.items.into_iter().map(resource_diagnostic).collect(),
                        });
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
            DbCommand::SetupEnsureEvents {
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = usize::try_from(after)
                        .map_err(|_| bosn_registry::Error::BadRow("page offset"))
                        .and_then(|after| registry.setup_ensure_events(after, limit as usize))
                        .map(|page| SetupEnsureEventPage {
                            next: page.next_offset.map(|value| value as u64),
                            records: page.items.into_iter().map(event_diagnostic).collect(),
                        });
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
            DbCommand::SetupGcPreview {
                workspace,
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = usize::try_from(after)
                        .map_err(|_| bosn_registry::Error::BadRow("page offset"))
                        .and_then(|after| {
                            registry.setup_gc_preview(&workspace, after, limit as usize)
                        })
                        .map(setup_gc_preview_diagnostic);
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
            DbCommand::SetupReconcilePreview {
                workspace,
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let after = usize::try_from(after)
                            .map_err(|_| bosn_registry::Error::BadRow("page offset"))?;
                        let containers = registry.setup_reconcile_containers(
                            &workspace,
                            after,
                            limit as usize,
                        )?;
                        // Image IDs are durable facts recorded by successful ensure. A
                        // bounded preview refuses to infer identity from a name/tag.
                        let images = registry
                            .setup_reconcile_images(&workspace)?
                            .items
                            .into_iter()
                            .map(|r| r.generation)
                            .collect::<Vec<_>>();
                        Ok((
                            containers.next_offset.map(|v| v as u64),
                            containers
                                .items
                                .into_iter()
                                .map(|resource| {
                                    let missing_repairable = registry
                                        .setup_missing_repair_candidate(
                                            &workspace,
                                            &resource.id,
                                            &resource.name,
                                            &resource.generation,
                                        )?;
                                    Ok(SetupReconcileCandidate {
                                        resource,
                                        image_identities: images.clone(),
                                        missing_repairable,
                                    })
                                })
                                .collect::<Result<Vec<_>, bosn_registry::Error>>()?,
                        ))
                    })();
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
            DbCommand::RepairMissingSetupContainer {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let repaired = transaction.repair_missing_setup_container(
                            &workspace,
                            &id,
                            &name,
                            &generation,
                            now,
                        )?;
                        if repaired == Some(bosn_registry::SetupMissingRepair::Repaired) {
                            transaction.commit()?;
                        }
                        Ok(repaired)
                    })();
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
            DbCommand::SetupMissingRepairCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.setup_missing_repair_candidate(
                        &workspace,
                        &id,
                        &name,
                        &generation,
                    );
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
            DbCommand::SetupGcCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.setup_gc_candidate(&workspace, &id, &name, &generation);
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
            DbCommand::FinalizeSetupGc {
                workspace,
                id,
                name,
                generation,
                missing,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let kind = if missing {
                            "setup.gc.reconciled_missing"
                        } else {
                            "setup.gc.removed"
                        };
                        let removed = transaction.finalize_setup_gc_candidate(
                            &workspace,
                            &id,
                            &name,
                            &generation,
                            now,
                            kind,
                        )?;
                        // Dropping an uncommitted immediate transaction rolls it
                        // back; no stale-preview event is persisted.
                        if removed {
                            transaction.commit()?;
                        }
                        Ok(removed)
                    })();
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
            DbCommand::ConfirmSetupRetiredStopped {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let recorded = transaction.confirm_setup_retired_container_stopped(
                            &workspace,
                            &id,
                            &name,
                            &generation,
                            now,
                        )?;
                        if recorded {
                            transaction.commit()?;
                        }
                        Ok(recorded)
                    })();
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
            DbCommand::CompleteSetupWorkspace { workspace, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let completed = transaction.complete_setup_workspace(&workspace, now)?;
                        // An already-complete workspace is a genuine no-op:
                        // no event or timestamp write is committed.
                        if completed.uses_completed != 0 {
                            transaction.commit()?;
                        }
                        Ok(SetupDoneResult::from(completed))
                    })();
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
            DbCommand::AppendSetupEnsureEvents { events, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = append_setup_ensure_events(&mut registry, &events);
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
            DbCommand::RecordSetupEnsure {
                job_id,
                execution,
                reply,
            } => {
                #[cfg(test)]
                if let Some(gate) = &mut setup_ensure_record_gate
                    && (gate.entered.send(()).await.is_err() || gate.release.recv().await.is_none())
                {
                    let _ = reply.send(Err(Error::ActorClosed));
                    continue;
                }
                let worker = async_engine::launch_blocking(move || {
                    let result = record_setup_ensure(&mut registry, job_id, &execution);
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
            DbCommand::RecordManifestEnsure {
                job_id,
                execution,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = record_manifest_ensure(&mut registry, job_id, &execution);
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
            DbCommand::PutManifestVolumeIntents { volumes, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = put_manifest_volume_intents(&mut registry, &volumes);
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
            DbCommand::RecordSetupAdoption { execution, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = record_setup_adoption(&mut registry, &execution);
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
            DbCommand::BeginSetupAppTaskSession {
                job_id,
                container_id,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result =
                        record_setup_app_task_session(&mut registry, job_id, &container_id);
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
            DbCommand::FinishSetupAppTaskSession {
                job_id,
                outcome,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = finish_setup_app_task_session(&mut registry, job_id, outcome);
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
            DbCommand::BeginManifestAppTaskSession {
                job_id,
                container_id,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result =
                        record_manifest_app_task_session(&mut registry, job_id, &container_id);
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
            DbCommand::FinishManifestAppTaskSession {
                job_id,
                outcome,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = finish_manifest_app_task_session(&mut registry, job_id, outcome);
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

fn setup_app_task_session_id(job_id: u64) -> String {
    format!("setup-app-task:{job_id}")
}
fn manifest_app_task_session_id(job_id: u64) -> String {
    format!("manifest-app-task:{job_id}")
}

fn record_setup_app_task_session(
    registry: &mut Registry,
    job_id: u64,
    container_id: &str,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    transaction.put_execution_session(&ExecutionSession {
        id: setup_app_task_session_id(job_id),
        container_id: container_id.into(),
        engine_binary: "docker".into(),
        client_pid: std::process::id(),
        client_start: None,
        lease_ids: Vec::new(),
    })?;
    transaction.append_event(now, "setup.app-task.started", "owned_declared_task")?;
    transaction.commit()
}

fn finish_setup_app_task_session(
    registry: &mut Registry,
    job_id: u64,
    outcome: &str,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    if outcome == "uncertain" {
        // Do not remove the session: cancelling/timing out the local Docker
        // client does not prove the remote `exec` process ended.
        transaction.append_event(now, "setup.app-task.uncertain", "remote_completion_unknown")?;
    } else {
        transaction.delete_execution_session(&setup_app_task_session_id(job_id))?;
        transaction.append_event(now, "setup.app-task.finished", outcome)?;
    }
    transaction.commit()
}

fn record_manifest_app_task_session(
    registry: &mut Registry,
    job_id: u64,
    container_id: &str,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    transaction.put_execution_session(&ExecutionSession {
        id: manifest_app_task_session_id(job_id),
        container_id: container_id.into(),
        engine_binary: "docker".into(),
        client_pid: std::process::id(),
        client_start: None,
        lease_ids: Vec::new(),
    })?;
    transaction.append_event(now, "manifest.app-task.started", "owned_declared_task")?;
    transaction.commit()
}
fn finish_manifest_app_task_session(
    registry: &mut Registry,
    job_id: u64,
    outcome: &str,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    if outcome == "uncertain" {
        transaction.append_event(
            now,
            "manifest.app-task.uncertain",
            "remote_completion_unknown",
        )?;
    } else {
        transaction.delete_execution_session(&manifest_app_task_session_id(job_id))?;
        transaction.append_event(now, "manifest.app-task.finished", outcome)?;
    }
    transaction.commit()
}

fn record_setup_ensure(
    registry: &mut Registry,
    job_id: u64,
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
    // A new successful setup document generation supersedes only prior Bosn
    // setup *container* ownership in this exact canonical workspace/stack.
    // It does not stop, delete, or otherwise mutate Docker; it also leaves
    // image ownership active because inspected image identities can be shared
    // across documents and workspaces. Keeping this after both current
    // resource upserts means an image conflict rolls back without retiring a
    // previously active generation.
    transaction.retire_prior_setup_container_generations(
        &container.workspace,
        &container.stack,
        &container.generation,
    )?;
    // Success is never visible in the event log until both durable ownership
    // facts and any generation retirement have been accepted by this very
    // transaction.
    let event = SetupEnsureEvent::terminal(job_id, SetupEnsureEventOutcome::Succeeded);
    transaction.append_event(now, event.kind, &event.detail)?;
    transaction.commit()
}

/// Persist one successful manifest-runtime ensure atomically. A succeeding
/// generation is recorded before only the previous manifest container use for
/// this exact workspace/stack is retired. This is durable lifecycle accounting
/// only: it never stops/deletes a container or image, and a failed upsert rolls
/// the whole transition back without retiring the prior generation.
fn record_manifest_ensure(
    registry: &mut Registry,
    job_id: u64,
    execution: &SetupEnsureExecution,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    for (kind, id, name, stack, generation, workspace) in [
        (
            ResourceKind::Container,
            &execution.resource.id,
            &execution.resource.name,
            &execution.resource.stack,
            &execution.resource.generation,
            &execution.resource.workspace,
        ),
        (
            ResourceKind::Image,
            &execution.image.id,
            &execution.image.name,
            &execution.image.stack,
            &execution.image.generation,
            &execution.image.workspace,
        ),
    ] {
        transaction.put_resource(&Resource {
            id: id.clone(),
            kind,
            name: name.clone(),
            stack: stack.clone(),
            generation: generation.clone(),
            scope: Scope::Machine,
            workspace: workspace.clone(),
            created_at: now,
            last_used: now,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })?;
        transaction.put_resource_use(&ResourceUse {
            resource_id: id.clone(),
            workspace: workspace.clone(),
            stack: stack.clone(),
            generation: generation.clone(),
            last_used: now,
            state: ResourceState::Active,
        })?;
    }
    // The engine volume was created/reused only after its exact contract was
    // durably intended.  Record the resource and consume that intent in the
    // same transaction as container success; normal generation rollover never
    // deletes or retires volume data.
    for volume in &execution.volumes {
        transaction.put_resource(&Resource {
            id: volume.id.clone(),
            kind: ResourceKind::Volume,
            name: volume.name.clone(),
            stack: volume.stack.clone(),
            generation: volume.generation.clone(),
            scope: volume.scope,
            workspace: volume.workspace.clone(),
            created_at: now,
            last_used: now,
            state: ResourceState::Active,
            retention: volume.retention,
        })?;
        transaction.put_resource_use(&ResourceUse {
            resource_id: volume.id.clone(),
            workspace: volume.workspace.clone(),
            stack: volume.stack.clone(),
            generation: volume.generation.clone(),
            last_used: now,
            state: ResourceState::Active,
        })?;
        transaction.delete_volume_creation_intent(&volume.name)?;
    }
    // This follows both upserts so an image/container identity conflict drops
    // the transaction with the preceding generation still active. The
    // registry primitive is manifest-namespace-only and never changes setup
    // resources, images, other stacks, or other workspaces.
    transaction.retire_prior_manifest_container_generations(
        &execution.resource.workspace,
        &execution.resource.stack,
        &execution.resource.generation,
    )?;
    transaction.append_event(
        now,
        "manifest.ensure.succeeded",
        &format!("job_id={job_id}"),
    )?;
    transaction.commit()
}

fn put_manifest_volume_intents(
    registry: &mut Registry,
    volumes: &[ManifestVolumeResource],
) -> Result<(), bosn_registry::Error> {
    let mut transaction = registry.begin_immediate()?;
    for volume in volumes {
        transaction.put_volume_creation_intent(&VolumeCreationIntent {
            name: volume.name.clone(),
            labels: volume.labels.clone(),
            stack: volume.stack.clone(),
            generation: volume.generation.clone(),
            scope: volume.scope,
            workspace: volume.workspace.clone(),
        })?;
    }
    transaction.commit()
}

/// Restore only an absent registry view of an already proven Docker fact.
/// Existing records must exactly agree with the re-derived ownership facts;
/// adoption is never an overwrite or a way to cross workspace/stack state.
fn record_setup_adoption(
    registry: &mut Registry,
    execution: &SetupEnsureExecution,
) -> Result<(), bosn_registry::Error> {
    let container = &execution.resource;
    let image = &execution.image;
    for (kind, id, name, stack, generation, workspace) in [
        (
            ResourceKind::Container,
            &container.id,
            &container.name,
            &container.stack,
            &container.generation,
            &container.workspace,
        ),
        (
            ResourceKind::Image,
            &image.id,
            &image.name,
            &image.stack,
            &image.generation,
            &image.workspace,
        ),
    ] {
        if let Some(existing) = registry.resource_by_kind_name(kind, name)?
            && (existing.id != *id
                || existing.stack != *stack
                || existing.generation != *generation
                || existing.workspace != *workspace
                || existing.scope != Scope::Machine
                || existing.state != ResourceState::Active
                || existing.retention != Retention::Pinned)
        {
            return Err(bosn_registry::Error::ResourceIdentityConflict);
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut tx = registry.begin_immediate()?;
    for (kind, id, name, stack, generation, workspace) in [
        (
            ResourceKind::Container,
            &container.id,
            &container.name,
            &container.stack,
            &container.generation,
            &container.workspace,
        ),
        (
            ResourceKind::Image,
            &image.id,
            &image.name,
            &image.stack,
            &image.generation,
            &image.workspace,
        ),
    ] {
        tx.put_resource(&Resource {
            id: id.clone(),
            kind,
            name: name.clone(),
            stack: stack.clone(),
            generation: generation.clone(),
            scope: Scope::Machine,
            workspace: workspace.clone(),
            created_at: now,
            last_used: now,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })?;
        tx.put_resource_use(&ResourceUse {
            resource_id: id.clone(),
            workspace: workspace.clone(),
            stack: stack.clone(),
            generation: generation.clone(),
            last_used: now,
            state: ResourceState::Active,
        })?;
    }
    tx.append_event(now, "setup.ensure.adopted", "managed_setup_app_restored")?;
    tx.commit()
}

fn append_setup_ensure_events(
    registry: &mut Registry,
    events: &[SetupEnsureEvent],
) -> Result<(), bosn_registry::Error> {
    if events.is_empty() {
        return Ok(());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    for event in events {
        transaction.append_event(now, event.kind, &event.detail)?;
    }
    transaction.commit()
}
impl Service {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            setup_executor: Arc::new(DockerSetupPrepareExecutor::new(state_dir.clone())),
            setup_task_executor: Arc::new(DockerSetupTaskExecutor::new(state_dir.clone())),
            setup_app_task_executor: Arc::new(DockerSetupAppTaskExecutor::new(state_dir.clone())),
            setup_ensure_executor: Arc::new(DockerSetupEnsureExecutor::new(state_dir.clone())),
            manifest_ensure_executor: Arc::new(DockerManifestEnsureExecutor::new()),
            manifest_app_task_executor: Arc::new(DockerManifestAppTaskExecutor::new()),
            setup_adopt_executor: Arc::new(DockerSetupAdoptExecutor::new(state_dir.clone())),
            doctor_executor: Arc::new(DockerDoctorExecutor::new()),
            setup_reconcile_executor: Arc::new(DockerSetupReconcileExecutor::new()),
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
    /// Substitute only the complete semantic setup-app-task executor. This is
    /// a test seam; it cannot add Docker argv, a container target, or a task
    /// command to the caller-facing request.
    pub fn with_setup_app_task_executor(mut self, executor: Arc<dyn SetupAppTaskExecutor>) -> Self {
        self.setup_app_task_executor = executor;
        self
    }
    /// Substitute only the complete semantic setup-ensure executor. This is a
    /// test seam; it does not add a caller-controlled container operation.
    pub fn with_setup_ensure_executor(mut self, executor: Arc<dyn SetupEnsureExecutor>) -> Self {
        self.setup_ensure_executor = executor;
        self
    }
    /// Substitute the finite semantic manifest-runtime executor for tests.
    pub fn with_manifest_ensure_executor(
        mut self,
        executor: Arc<dyn ManifestEnsureExecutor>,
    ) -> Self {
        self.manifest_ensure_executor = executor;
        self
    }
    /// Substitute the complete semantic manifest app-task executor for tests.
    pub fn with_manifest_app_task_executor(
        mut self,
        executor: Arc<dyn ManifestAppTaskExecutor>,
    ) -> Self {
        self.manifest_app_task_executor = executor;
        self
    }
    pub fn with_setup_adopt_executor(mut self, executor: Arc<dyn SetupAdoptExecutor>) -> Self {
        self.setup_adopt_executor = executor;
        self
    }
    /// Substitute the fixed semantic doctor probe for deterministic tests.
    /// This is not an engine-command injection seam.
    pub fn with_doctor_executor(mut self, executor: Arc<dyn DoctorExecutor>) -> Self {
        self.doctor_executor = executor;
        self
    }
    pub fn with_setup_reconcile_executor(
        mut self,
        executor: Arc<dyn SetupReconcileExecutor>,
    ) -> Self {
        self.setup_reconcile_executor = executor;
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
                app_task: Arc::clone(&self.setup_app_task_executor),
                ensure: Arc::clone(&self.setup_ensure_executor),
                manifest_ensure: Arc::clone(&self.manifest_ensure_executor),
                manifest_app_task: Arc::clone(&self.manifest_app_task_executor),
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
            let doctor = Arc::clone(&self.doctor_executor);
            let reconcile = Arc::clone(&self.setup_reconcile_executor);
            let adopt = Arc::clone(&self.setup_adopt_executor);
            clients.spawn(async move {
                handle(stream, actor, jobs, stop, doctor, adopt, reconcile).await
            });
        }
        while clients.join_next().await.is_some() {}
        // Keep the sole registry writer alive while the job actor cancels and
        // drains typed work: a shutdown-cancelled setup ensure still needs its
        // durable terminal audit event before the writer can be released.
        jobs.stop().await;
        drop(jobs);
        let _ = job_worker.await;
        actor.stop().await;
        let _ = worker.await;
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

const SETUP_GC_ENGINE_DEADLINE: Duration = Duration::from_secs(5);
const SETUP_GC_ENGINE_OUTPUT: usize = 16 * 1024;

/// Inspect exactly one known candidate. The format is deliberately fixed and
/// returns only the three labels Bosn needs to prove its own ownership.
async fn inspect_setup_gc_container(
    engine: &DockerEngine,
    candidate: &bosn_registry::SetupGcCandidate,
) -> Result<Option<bool>, Error> {
    let format = "{{.Name}}\t{{.State.Running}}\t{{index .Config.Labels \"com.zackees.bosn.setup-managed\"}}\t{{index .Config.Labels \"com.zackees.bosn.setup-content-sha256\"}}\t{{index .Config.Labels \"com.zackees.bosn.setup-container\"}}";
    let result = engine
        .with_args(["container", "inspect", "--format", format, &candidate.name])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("setup gc container inspection failed"))?;
    if result.exit_code == 1 {
        return Ok(None);
    }
    if !result.ok() {
        return Err(Error::Protocol("setup gc container inspection failed"));
    }
    let output = std::str::from_utf8(&result.stdout)
        .map_err(|_| Error::Protocol("setup gc container inspection invalid"))?;
    let fields: Vec<_> = output.trim_end_matches(['\r', '\n']).split('\t').collect();
    let content = candidate
        .generation
        .strip_prefix("sha256:")
        .ok_or(Error::Protocol("setup gc candidate identity invalid"))?;
    if fields.len() != 5
        || fields[0] != format!("/{}", candidate.name)
        || fields[2] != "v1"
        || fields[3] != content
        || fields[4] != candidate.name
    {
        return Err(Error::Protocol("setup gc container ownership mismatch"));
    }
    let running = match fields[1] {
        "true" => true,
        "false" => false,
        _ => return Err(Error::Protocol("setup gc container inspection invalid")),
    };
    Ok(Some(running))
}

async fn apply_setup_gc_candidate(
    actor: &RegistryActor,
    workspace: String,
    token: String,
) -> Result<SetupGcApplyResult, Error> {
    let (id, name, generation) = parse_setup_gc_token(&token)?;
    let candidate = actor
        .setup_gc_candidate(workspace.clone(), id, name, generation)
        .await?
        .ok_or(Error::Protocol("setup gc preview is stale or protected"))?;
    let engine = DockerEngine::docker();
    let first = inspect_setup_gc_container(&engine, &candidate).await?;
    if first.is_none() {
        let reconciled = actor
            .finalize_setup_gc(
                workspace,
                candidate.id,
                candidate.name,
                candidate.generation,
                true,
            )
            .await?;
        return reconciled
            .then_some(SetupGcApplyResult {
                removed: false,
                reconciled_missing: true,
            })
            .ok_or(Error::Protocol("setup gc preview became stale"));
    }
    if first != Some(false) {
        return Err(Error::Protocol("setup gc candidate is still running"));
    }
    // A second ownership inspection closes the only practical inspect/remove
    // interval without ever using a name glob or Docker selector.
    let second = inspect_setup_gc_container(&engine, &candidate).await?;
    if second.is_none() {
        let reconciled = actor
            .finalize_setup_gc(
                workspace,
                candidate.id,
                candidate.name,
                candidate.generation,
                true,
            )
            .await?;
        return reconciled
            .then_some(SetupGcApplyResult {
                removed: false,
                reconciled_missing: true,
            })
            .ok_or(Error::Protocol("setup gc preview became stale"));
    }
    if second != Some(false) {
        return Err(Error::Protocol("setup gc candidate is still running"));
    }
    let removed = engine
        .with_args(["container", "rm", &candidate.name])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("setup gc container removal failed"))?;
    if !removed.ok() {
        return Err(Error::Protocol("setup gc container removal failed"));
    }
    let finalized = actor
        .finalize_setup_gc(
            workspace,
            candidate.id,
            candidate.name,
            candidate.generation,
            false,
        )
        .await?;
    finalized
        .then_some(SetupGcApplyResult {
            removed: true,
            reconciled_missing: false,
        })
        .ok_or(Error::Protocol(
            "setup gc registry finalization failed after container removal",
        ))
}

/// Repair only the durable lifecycle accounting for one previewed setup app
/// that fixed Docker inspection proves absent. No Docker mutation occurs: the
/// next semantic ensure is solely responsible for creating/re-recording an
/// app. A forged/stale token is rejected before inspection because the actor
/// first proves the exact active ownership shape.
async fn repair_missing_setup_reconcile_candidate(
    actor: &RegistryActor,
    reconcile: &dyn SetupReconcileExecutor,
    workspace: String,
    token: String,
) -> Result<SetupReconcileMissingRepairResult, Error> {
    let (id, name, generation) = parse_setup_reconcile_missing_token(&token)?;
    if !actor
        .setup_missing_repair_candidate(
            workspace.clone(),
            id.clone(),
            name.clone(),
            generation.clone(),
        )
        .await?
    {
        // A repeat of a successful exact token is safe and silent. All other
        // stale/protected shapes fail closed without Docker observation.
        return match actor
            .repair_missing_setup_container(workspace, id, name, generation)
            .await?
        {
            Some(bosn_registry::SetupMissingRepair::AlreadyRepaired) => {
                Ok(SetupReconcileMissingRepairResult {
                    repaired: false,
                    already_repaired: true,
                })
            }
            _ => Err(Error::Protocol(
                "setup reconcile repair preview is stale or protected",
            )),
        };
    }
    match reconcile.inspect(&name).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(Error::Protocol(
                "setup reconcile candidate is no longer missing",
            ));
        }
        Err(_) => {
            return Err(Error::Protocol(
                "setup reconcile container inspection failed",
            ));
        }
    }
    match actor
        .repair_missing_setup_container(workspace, id, name, generation)
        .await?
    {
        Some(bosn_registry::SetupMissingRepair::Repaired) => {
            Ok(SetupReconcileMissingRepairResult {
                repaired: true,
                already_repaired: false,
            })
        }
        Some(bosn_registry::SetupMissingRepair::AlreadyRepaired) => {
            Ok(SetupReconcileMissingRepairResult {
                repaired: false,
                already_repaired: true,
            })
        }
        None => Err(Error::Protocol(
            "setup reconcile repair preview became stale",
        )),
    }
}

/// Stop a live retired candidate with no caller-controlled Docker input. A
/// subsequent fixed inspection proves it transitioned to stopped before the
/// actor records the event. The registry record is intentionally retained.
async fn stop_setup_retired_candidate(
    actor: &RegistryActor,
    workspace: String,
    token: String,
) -> Result<SetupRetiredStopResult, Error> {
    let (id, name, generation) = parse_setup_gc_token(&token)?;
    let candidate = actor
        .setup_gc_candidate(workspace.clone(), id, name, generation)
        .await?
        .ok_or(Error::Protocol(
            "setup retired stop preview is stale or protected",
        ))?;
    let engine = DockerEngine::docker();
    match inspect_setup_gc_container(&engine, &candidate).await? {
        None => return Err(Error::Protocol("setup retired stop candidate is absent")),
        Some(false) => {
            // Already stopped is idempotent. Do not add duplicate events and
            // keep the exact retired candidate eligible for explicit GC.
            return Ok(SetupRetiredStopResult {
                stopped: false,
                already_stopped: true,
            });
        }
        Some(true) => {}
    }
    // Recheck the exact identity after registry selection, then make the one
    // permitted engine mutation. The short grace is product-fixed and stays
    // inside the absolute engine deadline; no caller-controlled argv or
    // timeout reaches this boundary.
    if inspect_setup_gc_container(&engine, &candidate).await? != Some(true) {
        return Err(Error::Protocol("setup retired stop candidate changed"));
    }
    let stopped = engine
        .with_args(["container", "stop", "--time", "1", &candidate.name])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("setup retired stop failed"))?;
    if !stopped.ok() {
        return Err(Error::Protocol("setup retired stop failed"));
    }
    if inspect_setup_gc_container(&engine, &candidate).await? != Some(false) {
        return Err(Error::Protocol("setup retired stop did not stop candidate"));
    }
    let recorded = actor
        .confirm_setup_retired_stopped(
            workspace,
            candidate.id,
            candidate.name,
            candidate.generation,
        )
        .await?;
    if !recorded {
        return Err(Error::Protocol("setup retired stop registry became stale"));
    }
    Ok(SetupRetiredStopResult {
        stopped: true,
        already_stopped: false,
    })
}

async fn handle(
    mut s: AsyncStream,
    actor: RegistryActor,
    jobs: JobActor,
    stop: CancellationSource,
    doctor: Arc<dyn DoctorExecutor>,
    adopt: Arc<dyn SetupAdoptExecutor>,
    reconcile: Arc<dyn SetupReconcileExecutor>,
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
            21 => {
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
                    .map(|()| SetupAppTaskJobRequest {
                        workspace: PathBuf::from(r.workspace),
                        config: r.setup_config,
                        policy,
                        task_name: r.setup_task_name,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                });
                match request {
                    Some(request) => match jobs.submit_setup_app_task(request).await {
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
            22 => match validate_manifest_ensure_request_wire(&r) {
                Ok(()) => match jobs
                    .submit_manifest_ensure(ManifestEnsureJobRequest {
                        workspace: PathBuf::from(r.workspace),
                        manifest: r.setup_config,
                        stack: r.stack,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                    .await
                {
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
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            23 => match validate_manifest_app_task_request_wire(&r) {
                Ok(()) => match jobs
                    .submit_manifest_app_task(ManifestAppTaskJobRequest {
                        workspace: PathBuf::from(r.workspace),
                        manifest: r.setup_config,
                        stack: r.stack,
                        task_name: r.setup_task_name,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                    .await
                {
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
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            11 => match validate_registry_diagnostics_request_wire(&r) {
                Ok(()) => match actor
                    .resources(r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok(page) => ReplyWire {
                        code: 80,
                        diagnostic_next: page.next.unwrap_or(0),
                        diagnostic_has_next: page.next.is_some(),
                        resources_diagnostic: page
                            .records
                            .into_iter()
                            .map(ResourceDiagnosticWire::from)
                            .collect(),
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            12 => match validate_registry_diagnostics_request_wire(&r) {
                Ok(()) => match actor
                    .setup_ensure_events(r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok(page) => ReplyWire {
                        code: 90,
                        diagnostic_next: page.next.unwrap_or(0),
                        diagnostic_has_next: page.next.is_some(),
                        setup_ensure_events: page
                            .records
                            .into_iter()
                            .map(SetupEnsureEventWire::from)
                            .collect(),
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            13 => {
                match validate_doctor_request_wire(&r) {
                    Ok(()) => {
                        let registry = actor.doctor_integrity().await;
                        let report =
                            match async_engine::timeout(DOCTOR_ENGINE_DEADLINE, doctor.doctor())
                                .await
                            {
                                Ok(report) => report,
                                Err(_) => DockerDoctorReport {
                                    state: DockerDoctorState::Deadline,
                                    client_version: None,
                                    server_version: None,
                                },
                            };
                        let report = DoctorReport::from_engine(registry, report);
                        ReplyWire {
                            code: 100,
                            doctor_daemon: report.daemon,
                            doctor_registry: report.registry,
                            doctor_engine: report.engine,
                            doctor_client_version: report.client_version.unwrap_or_default(),
                            doctor_server_version: report.server_version.unwrap_or_default(),
                            ..Default::default()
                        }
                    }
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            17 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_adopt_request_wire(&r, policy)
                        .ok()
                        .map(|()| SetupAdoptRequest {
                            workspace: PathBuf::from(r.workspace),
                            config: r.setup_config,
                            policy,
                            deadline: Duration::from_millis(r.setup_deadline_ms),
                            output_limit: r.setup_output_limit as usize,
                            confirm: true,
                        })
                });
                match request {
                    Some(request) => {
                        let (logs, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
                        let drain = async_engine::launch(async move {
                            while receiver.recv().await.is_some() {}
                        });
                        let cancellation = async_engine::CancellationSource::new();
                        let token = cancellation.token();
                        let result = adopt.execute(request, &token, &logs).await;
                        drop(logs);
                        let _ = drain.await;
                        match result {
                            Ok(execution) => match actor.record_setup_adoption(execution).await {
                                Ok(()) => ReplyWire {
                                    code: 140,
                                    setup_adopted: true,
                                    ..Default::default()
                                },
                                Err(_) => ReplyWire {
                                    code: 3,
                                    ..Default::default()
                                },
                            },
                            Err(_) => ReplyWire {
                                code: 3,
                                ..Default::default()
                            },
                        }
                    }
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            14 => match validate_setup_gc_preview_request_wire(&r) {
                Ok(()) => match actor
                    .setup_gc_preview(r.workspace, r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok(page) => ReplyWire {
                        code: 110,
                        diagnostic_next: page.next.unwrap_or(0),
                        diagnostic_has_next: page.next.is_some(),
                        setup_gc_candidates: page
                            .candidates
                            .into_iter()
                            .map(SetupGcCandidateWire::from)
                            .collect(),
                        gc_protected_not_retired: page.counts.protected_not_retired,
                        gc_protected_ambiguous_use: page.counts.protected_ambiguous_use,
                        gc_protected_lease: page.counts.protected_lease,
                        gc_protected_session: page.counts.protected_session,
                        gc_excluded_unmanaged: page.counts.excluded_unmanaged,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            15 => match validate_setup_gc_apply_request_wire(&r) {
                Ok(()) => match apply_setup_gc_candidate(&actor, r.workspace, r.gc_candidate_token)
                    .await
                {
                    Ok(result) => ReplyWire {
                        code: 120,
                        gc_removed: result.removed,
                        gc_reconciled_missing: result.reconciled_missing,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            16 => match validate_setup_done_request_wire(&r)
                .and_then(|()| canonical_setup_done_workspace(Path::new(&r.workspace)))
            {
                Ok(workspace) => match actor.complete_setup_workspace(workspace).await {
                    Ok(result) => ReplyWire {
                        code: 130,
                        setup_done_uses: result.uses_completed,
                        setup_done_resources: result.resources_completed,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            18 => match validate_setup_retired_stop_request_wire(&r) {
                Ok(()) => {
                    match stop_setup_retired_candidate(&actor, r.workspace, r.gc_candidate_token)
                        .await
                    {
                        Ok(result) => ReplyWire {
                            code: 150,
                            setup_retired_stopped: result.stopped,
                            setup_retired_already_stopped: result.already_stopped,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    }
                }
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            19 => match validate_setup_reconcile_preview_request_wire(&r) {
                Ok(()) => match actor
                    .setup_reconcile_preview(r.workspace, r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok((next, candidates)) => {
                        let mut records = Vec::with_capacity(candidates.len());
                        for candidate in candidates {
                            let inspected = reconcile.inspect(&candidate.resource.name).await;
                            let drift = classify_setup_reconcile(&candidate, inspected);
                            let repair_token = (drift == "missing" && candidate.missing_repairable)
                                .then(|| setup_reconcile_missing_token(&candidate));
                            records.push(SetupReconcileRecord {
                                id: candidate.resource.id,
                                name: candidate.resource.name,
                                generation: candidate.resource.generation,
                                repair_token,
                                drift: drift.into(),
                            });
                        }
                        ReplyWire {
                            code: 160,
                            diagnostic_next: next.unwrap_or(0),
                            diagnostic_has_next: next.is_some(),
                            setup_reconcile_records: records
                                .into_iter()
                                .map(SetupReconcileRecordWire::from)
                                .collect(),
                            ..Default::default()
                        }
                    }
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            20 => match validate_setup_reconcile_repair_missing_request_wire(&r) {
                Ok(()) => match repair_missing_setup_reconcile_candidate(
                    &actor,
                    reconcile.as_ref(),
                    r.workspace,
                    r.gc_candidate_token,
                )
                .await
                {
                    Ok(result) => ReplyWire {
                        code: 170,
                        setup_reconcile_repaired: result.repaired,
                        setup_reconcile_already_repaired: result.already_repaired,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
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
    #[prost(uint64, tag = "14")]
    diagnostic_after: u64,
    #[prost(uint32, tag = "15")]
    diagnostic_limit: u32,
    #[prost(string, tag = "16")]
    gc_candidate_token: String,
    #[prost(bool, tag = "17")]
    gc_confirm: bool,
    #[prost(bool, tag = "18")]
    setup_done_confirm: bool,
    #[prost(bool, tag = "19")]
    setup_adopt_confirm: bool,
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
            diagnostic_after: 0,
            diagnostic_limit: 0,
            gc_candidate_token: String::new(),
            gc_confirm: false,
            setup_done_confirm: false,
            setup_adopt_confirm: false,
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

fn validate_manifest_ensure_wire(
    workspace: &str,
    manifest: &str,
    stack: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_setup_prepare_wire(
        workspace,
        manifest,
        SetupPreparePolicy::Refresh,
        deadline_ms,
        output_limit,
    )?;
    if !safe_manifest_relative_path(manifest)
        || stack.is_empty()
        || stack.len() > 128
        || !stack
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(Error::Protocol("invalid manifest ensure selector"));
    }
    Ok(())
}

fn validate_manifest_ensure_request_wire(request: &Request) -> Result<(), Error> {
    validate_manifest_ensure_wire(
        &request.workspace,
        &request.setup_config,
        &request.stack,
        request.setup_deadline_ms,
        request.setup_output_limit,
    )?;
    if !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || request.setup_policy != 0
        || !request.setup_task_name.is_empty()
        || request.diagnostic_after != 0
        || request.diagnostic_limit != 0
        || !request.gc_candidate_token.is_empty()
        || request.gc_confirm
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic manifest ensure fields"));
    }
    Ok(())
}
fn validate_manifest_app_task_wire(
    workspace: &str,
    manifest: &str,
    stack: &str,
    task_name: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_manifest_ensure_wire(workspace, manifest, stack, deadline_ms, output_limit)?;
    validate_setup_task_wire(
        workspace,
        manifest,
        SetupPreparePolicy::Refresh,
        task_name,
        deadline_ms,
        output_limit,
    )
}
fn validate_manifest_app_task_request_wire(request: &Request) -> Result<(), Error> {
    validate_manifest_app_task_wire(
        &request.workspace,
        &request.setup_config,
        &request.stack,
        &request.setup_task_name,
        request.setup_deadline_ms,
        request.setup_output_limit,
    )?;
    if !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || request.setup_policy != 0
        || request.diagnostic_after != 0
        || request.diagnostic_limit != 0
        || !request.gc_candidate_token.is_empty()
        || request.gc_confirm
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic manifest app task fields"));
    }
    Ok(())
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
        || request.setup_done_confirm
        || request.setup_adopt_confirm
    {
        return Err(Error::Protocol("nonsemantic setup ensure fields"));
    }
    Ok(())
}
fn validate_setup_adopt_input(
    workspace: &str,
    config: &str,
    policy: SetupPreparePolicy,
    deadline_ms: u64,
    output_limit: u32,
    confirm: bool,
) -> Result<(), Error> {
    validate_setup_ensure_wire(workspace, config, policy, deadline_ms, output_limit)?;
    if !confirm {
        return Err(Error::Protocol("setup adoption requires confirmation"));
    }
    Ok(())
}
fn validate_setup_adopt_request_wire(
    request: &Request,
    policy: SetupPreparePolicy,
) -> Result<(), Error> {
    validate_setup_adopt_input(
        &request.workspace,
        &request.setup_config,
        policy,
        request.setup_deadline_ms,
        request.setup_output_limit,
        request.setup_adopt_confirm,
    )?;
    if !request.stack.is_empty()
        || !request.digest.is_empty()
        || request.job_id != 0
        || request.log_after != 0
        || request.log_limit != 0
        || !request.setup_task_name.is_empty()
        || request.diagnostic_after != 0
        || request.diagnostic_limit != 0
        || !request.gc_candidate_token.is_empty()
        || request.gc_confirm
        || request.setup_done_confirm
    {
        return Err(Error::Protocol("nonsemantic setup adopt fields"));
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
    #[prost(uint64, tag = "15")]
    diagnostic_next: u64,
    #[prost(bool, tag = "16")]
    diagnostic_has_next: bool,
    #[prost(message, repeated, tag = "17")]
    resources_diagnostic: Vec<ResourceDiagnosticWire>,
    #[prost(message, repeated, tag = "18")]
    setup_ensure_events: Vec<SetupEnsureEventWire>,
    #[prost(string, tag = "19")]
    doctor_daemon: String,
    #[prost(string, tag = "20")]
    doctor_registry: String,
    #[prost(string, tag = "21")]
    doctor_engine: String,
    #[prost(string, tag = "22")]
    doctor_client_version: String,
    #[prost(string, tag = "23")]
    doctor_server_version: String,
    #[prost(message, repeated, tag = "24")]
    setup_gc_candidates: Vec<SetupGcCandidateWire>,
    #[prost(uint64, tag = "25")]
    gc_protected_not_retired: u64,
    #[prost(uint64, tag = "26")]
    gc_protected_ambiguous_use: u64,
    #[prost(uint64, tag = "27")]
    gc_protected_lease: u64,
    #[prost(uint64, tag = "28")]
    gc_protected_session: u64,
    #[prost(uint64, tag = "29")]
    gc_excluded_unmanaged: u64,
    #[prost(bool, tag = "30")]
    gc_removed: bool,
    #[prost(bool, tag = "31")]
    gc_reconciled_missing: bool,
    #[prost(uint64, tag = "32")]
    setup_done_uses: u64,
    #[prost(uint64, tag = "33")]
    setup_done_resources: u64,
    #[prost(bool, tag = "34")]
    setup_adopted: bool,
    #[prost(bool, tag = "35")]
    setup_retired_stopped: bool,
    #[prost(bool, tag = "36")]
    setup_retired_already_stopped: bool,
    #[prost(message, repeated, tag = "37")]
    setup_reconcile_records: Vec<SetupReconcileRecordWire>,
    #[prost(bool, tag = "38")]
    setup_reconcile_repaired: bool,
    #[prost(bool, tag = "39")]
    setup_reconcile_already_repaired: bool,
}
#[derive(Message)]
struct LogRecordWire {
    #[prost(uint64, tag = "1")]
    cursor: u64,
    #[prost(string, tag = "2")]
    line: String,
}
#[derive(Message)]
struct ResourceDiagnosticWire {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    kind: String,
    #[prost(string, tag = "3")]
    name: String,
    #[prost(string, tag = "4")]
    stack: String,
    #[prost(string, tag = "5")]
    generation: String,
    #[prost(string, tag = "6")]
    state: String,
    #[prost(string, tag = "7")]
    retention: String,
    #[prost(double, tag = "8")]
    created_at: f64,
    #[prost(double, tag = "9")]
    last_used: f64,
}
impl From<RegistryResourceDiagnostic> for ResourceDiagnosticWire {
    fn from(value: RegistryResourceDiagnostic) -> Self {
        Self {
            id: value.id,
            kind: value.kind,
            name: value.name,
            stack: value.stack,
            generation: value.generation,
            state: value.state,
            retention: value.retention,
            created_at: value.created_at,
            last_used: value.last_used,
        }
    }
}
impl From<ResourceDiagnosticWire> for RegistryResourceDiagnostic {
    fn from(value: ResourceDiagnosticWire) -> Self {
        Self {
            id: value.id,
            kind: value.kind,
            name: value.name,
            stack: value.stack,
            generation: value.generation,
            state: value.state,
            retention: value.retention,
            created_at: value.created_at,
            last_used: value.last_used,
        }
    }
}
#[derive(Message)]
struct SetupGcCandidateWire {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, tag = "3")]
    generation: String,
    #[prost(string, tag = "4")]
    reason: String,
    #[prost(string, tag = "5")]
    token: String,
}
impl From<SetupGcCandidateDiagnostic> for SetupGcCandidateWire {
    fn from(value: SetupGcCandidateDiagnostic) -> Self {
        Self {
            id: value.id,
            name: value.name,
            generation: value.generation,
            reason: value.reason,
            token: value.token,
        }
    }
}
impl From<SetupGcCandidateWire> for SetupGcCandidateDiagnostic {
    fn from(value: SetupGcCandidateWire) -> Self {
        Self {
            id: value.id,
            name: value.name,
            generation: value.generation,
            reason: value.reason,
            token: value.token,
        }
    }
}
#[derive(Message)]
struct SetupEnsureEventWire {
    #[prost(uint64, tag = "1")]
    cursor: u64,
    #[prost(double, tag = "2")]
    at: f64,
    #[prost(string, tag = "3")]
    kind: String,
    #[prost(string, tag = "4")]
    detail: String,
}
impl From<SetupEnsureEventDiagnostic> for SetupEnsureEventWire {
    fn from(value: SetupEnsureEventDiagnostic) -> Self {
        Self {
            cursor: value.cursor,
            at: value.at,
            kind: value.kind,
            detail: value.detail,
        }
    }
}
impl From<SetupEnsureEventWire> for SetupEnsureEventDiagnostic {
    fn from(value: SetupEnsureEventWire) -> Self {
        Self {
            cursor: value.cursor,
            at: value.at,
            kind: value.kind,
            detail: value.detail,
        }
    }
}
#[derive(Message)]
struct SetupReconcileRecordWire {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, tag = "3")]
    generation: String,
    #[prost(string, tag = "4")]
    drift: String,
    #[prost(string, tag = "5")]
    repair_token: String,
}
impl From<SetupReconcileRecord> for SetupReconcileRecordWire {
    fn from(value: SetupReconcileRecord) -> Self {
        Self {
            id: value.id,
            name: value.name,
            generation: value.generation,
            drift: value.drift,
            repair_token: value.repair_token.unwrap_or_default(),
        }
    }
}
impl From<SetupReconcileRecordWire> for SetupReconcileRecord {
    fn from(value: SetupReconcileRecordWire) -> Self {
        Self {
            id: value.id,
            name: value.name,
            generation: value.generation,
            drift: value.drift,
            repair_token: (!value.repair_token.is_empty()).then_some(value.repair_token),
        }
    }
}
enum Reply {
    Pong,
    Status(Status),
    Shutdown,
    Job(u64),
    JobStatus(JobStatus),
    Cancelled,
    JobLogs(JobLogPage),
    RegistryResources(RegistryResourcePage),
    SetupEnsureEvents(SetupEnsureEventPage),
    SetupGcPreview(SetupGcPreviewPage),
    SetupGcApply(SetupGcApplyResult),
    SetupRetiredStop(SetupRetiredStopResult),
    SetupDone(SetupDoneResult),
    SetupAdopt(SetupAdoptResult),
    Doctor(DoctorReport),
    SetupReconcilePreview(SetupReconcilePreviewPage),
    SetupReconcileMissingRepair(SetupReconcileMissingRepairResult),
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
        80 => Ok(Reply::RegistryResources(RegistryResourcePage {
            next: v.diagnostic_has_next.then_some(v.diagnostic_next),
            records: v.resources_diagnostic.into_iter().map(Into::into).collect(),
        })),
        90 => Ok(Reply::SetupEnsureEvents(SetupEnsureEventPage {
            next: v.diagnostic_has_next.then_some(v.diagnostic_next),
            records: v.setup_ensure_events.into_iter().map(Into::into).collect(),
        })),
        100 => Ok(Reply::Doctor(DoctorReport {
            daemon: v.doctor_daemon,
            registry: v.doctor_registry,
            engine: v.doctor_engine,
            client_version: (!v.doctor_client_version.is_empty())
                .then_some(v.doctor_client_version),
            server_version: (!v.doctor_server_version.is_empty())
                .then_some(v.doctor_server_version),
        })),
        110 => Ok(Reply::SetupGcPreview(SetupGcPreviewPage {
            next: v.diagnostic_has_next.then_some(v.diagnostic_next),
            candidates: v.setup_gc_candidates.into_iter().map(Into::into).collect(),
            counts: SetupGcPreviewCounts {
                protected_not_retired: v.gc_protected_not_retired,
                protected_ambiguous_use: v.gc_protected_ambiguous_use,
                protected_lease: v.gc_protected_lease,
                protected_session: v.gc_protected_session,
                excluded_unmanaged: v.gc_excluded_unmanaged,
            },
        })),
        120 => Ok(Reply::SetupGcApply(SetupGcApplyResult {
            removed: v.gc_removed,
            reconciled_missing: v.gc_reconciled_missing,
        })),
        130 => Ok(Reply::SetupDone(SetupDoneResult {
            uses_completed: v.setup_done_uses,
            resources_completed: v.setup_done_resources,
        })),
        140 => Ok(Reply::SetupAdopt(SetupAdoptResult {
            adopted: v.setup_adopted,
        })),
        150 => Ok(Reply::SetupRetiredStop(SetupRetiredStopResult {
            stopped: v.setup_retired_stopped,
            already_stopped: v.setup_retired_already_stopped,
        })),
        160 => Ok(Reply::SetupReconcilePreview(SetupReconcilePreviewPage {
            next: v.diagnostic_has_next.then_some(v.diagnostic_next),
            records: v
                .setup_reconcile_records
                .into_iter()
                .map(Into::into)
                .collect(),
        })),
        170 => Ok(Reply::SetupReconcileMissingRepair(
            SetupReconcileMissingRepairResult {
                repaired: v.setup_reconcile_repaired,
                already_repaired: v.setup_reconcile_already_repaired,
            },
        )),
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
    fn manifest_ensure_wire_accepts_only_its_typed_selectors() {
        let valid = || Request {
            workspace: "/workspace".into(),
            setup_config: "bosn.toml".into(),
            stack: "app_one".into(),
            setup_deadline_ms: 1,
            setup_output_limit: 1,
            ..Request::operation(22)
        };
        assert!(validate_manifest_ensure_request_wire(&valid()).is_ok());
        for invalid in [
            Request {
                setup_config: "../bosn.toml".into(),
                ..valid()
            },
            Request {
                setup_policy: 1,
                ..valid()
            },
            Request {
                setup_task_name: "shell".into(),
                ..valid()
            },
        ] {
            assert!(validate_manifest_ensure_request_wire(&invalid).is_err());
        }
    }

    #[test]
    fn manifest_app_task_wire_accepts_only_declared_task_selectors() {
        let valid = || Request {
            workspace: "/workspace".into(),
            setup_config: "bosn.toml".into(),
            stack: "app_one".into(),
            setup_task_name: "check-1".into(),
            setup_deadline_ms: 1,
            setup_output_limit: 1,
            ..Request::operation(23)
        };
        assert!(validate_manifest_app_task_request_wire(&valid()).is_ok());
        for invalid in [
            Request {
                setup_config: "../bosn.toml".into(),
                ..valid()
            },
            Request {
                setup_policy: 1,
                ..valid()
            },
            Request {
                setup_task_name: "shell; id".into(),
                ..valid()
            },
        ] {
            assert!(validate_manifest_app_task_request_wire(&invalid).is_err());
        }
    }

    #[test]
    fn manifest_stack_plan_derives_generation_and_refuses_unsupported_fields() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
        std::fs::write(
            workspace.join("bosn.toml"),
            format!("[stack.app]\nimage = '{image}'\n[stack.app.env]\nMODE = 'test'\n"),
        )
        .unwrap();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ManifestRuntimePlan {
            plan, generation, ..
        } = runtime
            .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                workspace: workspace.clone(),
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .unwrap();
        assert!(generation.starts_with("sha256:"));
        assert_eq!(plan.app.environment["MODE"], "test");
        std::fs::write(
            workspace.join("bosn.toml"),
            format!("[stack.app]\nimage = '{image}'\nworkdir = '/'\n"),
        )
        .unwrap();
        assert!(
            runtime
                .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                    workspace,
                    manifest: "bosn.toml".into(),
                    stack: "app".into(),
                    deadline: Duration::from_secs(1),
                    output_limit: 64,
                }))
                .is_err()
        );
    }

    #[test]
    fn manifest_stack_plan_derives_typed_scoped_named_volumes() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
        std::fs::write(
            workspace.join("bosn.toml"),
            format!(
                "[stack.app]\nimage = '{image}'\nfamily = 'shared-cache'\n[stack.app.volumes.cache]\nscope = 'stack'\ndestination = '/var/cache/app'\nretention = 'pinned'\n[stack.app.volumes.scratch]\nscope = 'spec'\n"
            ),
        ).unwrap();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime
            .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                workspace: workspace.clone(),
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .unwrap();
        assert_eq!(result.plan.named_volumes.len(), 2);
        assert_eq!(result.volumes.len(), 2);
        assert!(result.plan.named_volumes.iter().all(|volume| {
            volume.name.starts_with("bosn-v-")
                && volume
                    .labels
                    .get("com.zackees.bosn.setup-content-sha256")
                    .is_some_and(|identity| identity.len() == 64)
        }));
        let cache = result
            .volumes
            .iter()
            .find(|volume| volume.retention == Retention::Pinned)
            .unwrap();
        assert_eq!(cache.scope, Scope::Stack);
        assert_eq!(cache.workspace, workspace.to_string_lossy());

        std::fs::write(
            workspace.join("bosn.toml"),
            format!(
                "[stack.app]\nimage = '{image}'\n[stack.app.env]\nMODE = 'changed'\n[stack.app.volumes.cache]\nscope = 'stack'\ndestination = '/var/cache/app'\nretention = 'pinned'\n[stack.app.volumes.scratch]\nscope = 'spec'\n"
            ),
        )
        .unwrap();
        let changed = runtime
            .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .unwrap();
        assert_ne!(changed.generation, result.generation);
        let original_stack = result
            .volumes
            .iter()
            .find(|volume| volume.scope == Scope::Stack)
            .unwrap();
        let changed_stack = changed
            .volumes
            .iter()
            .find(|volume| volume.scope == Scope::Stack)
            .unwrap();
        assert_eq!(changed_stack.name, original_stack.name);
        assert_eq!(changed_stack.generation, original_stack.generation);
    }

    #[test]
    fn manifest_stack_plan_translates_workspace_binds_and_workdir_into_setup_shape() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("project")).unwrap();
        let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
        let write = |workdir: &str, readonly: bool| {
            std::fs::write(
                workspace.join("bosn.toml"),
                format!(
                    "[stack.app]\nimage = '{image}'\nworkdir = '{workdir}'\n[stack.app.mounts.repo]\nsource = '.'\ndestination = '/repo'\nreadonly = {readonly}\n[stack.app.mounts.project]\nsource = 'project'\ndestination = '/repo/project'\n"
                ),
            )
            .unwrap();
        };
        let request = || ManifestEnsureJobRequest {
            workspace: workspace.clone(),
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            deadline: Duration::from_secs(1),
            output_limit: 64,
        };
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        write("/repo/project", true);
        let ManifestRuntimePlan {
            plan: first,
            generation: first_generation,
            ..
        } = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
        assert_eq!(first.app.workdir.as_deref(), Some("project"));
        assert_eq!(
            first.app.mounts,
            vec![
                bosn_core::WorkspaceMount {
                    source: "project".into(),
                    target: "/repo/project".into(),
                    readonly: false,
                },
                bosn_core::WorkspaceMount {
                    source: ".".into(),
                    target: "/repo".into(),
                    readonly: true,
                },
            ]
        );
        write("/repo/project", false);
        let ManifestRuntimePlan {
            plan: second,
            generation: second_generation,
            ..
        } = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
        assert_eq!(second.app.workdir.as_deref(), Some("project"));
        assert_ne!(first_generation, second_generation);
        assert_ne!(first.content_sha256, second.content_sha256);
        write("/repo", false);
        let ManifestRuntimePlan {
            plan: third,
            generation: third_generation,
            ..
        } = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
        assert_eq!(third.app.workdir.as_deref(), Some("."));
        assert_ne!(second_generation, third_generation);
        assert_ne!(second.content_sha256, third.content_sha256);
    }

    #[test]
    fn manifest_stack_plan_refuses_mount_sources_outside_the_selected_workspace() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let workspace = temporary.path().join("workspace");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
        std::fs::write(
            workspace.join("bosn.toml"),
            format!(
                "[stack.app]\nimage = '{image}'\n[stack.app.mounts.bad]\nsource = '{}'\ndestination = '/repo'\n",
                outside.display()
            ),
        )
        .unwrap();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime
                .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                    workspace,
                    manifest: "bosn.toml".into(),
                    stack: "app".into(),
                    deadline: Duration::from_secs(1),
                    output_limit: 64,
                }))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn manifest_stack_plan_refuses_a_symlink_mount_source_even_when_it_names_workspace() {
        use std::os::unix::fs::symlink;

        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("real")).unwrap();
        symlink(workspace.join("real"), workspace.join("link")).unwrap();
        let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
        std::fs::write(
            workspace.join("bosn.toml"),
            format!(
                "[stack.app]\nimage = '{image}'\n[stack.app.mounts.link]\nsource = 'link'\ndestination = '/repo'\n"
            ),
        )
        .unwrap();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime
                .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                    workspace,
                    manifest: "bosn.toml".into(),
                    stack: "app".into(),
                    deadline: Duration::from_secs(1),
                    output_limit: 64,
                }))
                .is_err()
        );
    }

    #[test]
    fn manifest_stack_task_plan_reuses_declared_binds_and_container_workdir() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("project")).unwrap();
        let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
        std::fs::write(
            workspace.join("bosn.toml"),
            format!(
                "[stack.app]\nimage = '{image}'\nworkdir = '/repo/project'\n[stack.app.mounts.repo]\nsource = '.'\ndestination = '/repo'\n[task.check]\nstack = 'app'\ncmd = 'pwd'\n"
            ),
        )
        .unwrap();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ManifestRuntimePlan { plan, .. } = runtime
            .run(manifest_stack_task_setup_plan(&ManifestAppTaskJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                task_name: "check".into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .unwrap();
        assert_eq!(plan.app.workdir.as_deref(), Some("project"));
        assert_eq!(plan.app.mounts[0].target, "/repo");
        assert_eq!(plan.tasks["check"].command, "pwd");
    }

    #[test]
    fn manifest_app_task_plan_retains_only_a_task_from_its_selected_stack() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
        std::fs::write(workspace.join("bosn.toml"), format!(
            "[stack.app]\nimage='{image}'\n[stack.other]\nimage='{image}'\n[task.check]\nstack='app'\ncmd='printf ok'\n[task.foreign]\nstack='other'\ncmd='printf no'\n"
        )).unwrap();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = ManifestAppTaskJobRequest {
            workspace: workspace.clone(),
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            task_name: "check".into(),
            deadline: Duration::from_secs(1),
            output_limit: 64,
        };
        let ManifestRuntimePlan { plan, .. } = runtime
            .run(manifest_stack_task_setup_plan(&request))
            .unwrap();
        assert_eq!(plan.task_names, ["check"]);
        assert_eq!(plan.tasks["check"].command, "printf ok");
        let foreign = ManifestAppTaskJobRequest {
            task_name: "foreign".into(),
            ..request
        };
        assert!(
            runtime
                .run(manifest_stack_task_setup_plan(&foreign))
                .is_err()
        );
    }

    #[test]
    fn manifest_ensure_is_daemon_owned_and_records_atomic_facts_with_fake_executor() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(FakeManifestEnsureExecutor::default());
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_manifest_ensure_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let request = ManifestEnsureJobRequest {
                    workspace: workspace.clone(),
                    manifest: "bosn.toml".into(),
                    stack: "app".into(),
                    deadline: Duration::from_secs(2),
                    output_limit: 4096,
                };
                let first = client.submit_manifest_ensure(request).await.unwrap();
                wait_for_job_state(&client, first, "Succeeded").await;
                assert_eq!(fake.calls.lock().unwrap().len(), 1);
                let resources = client.registry_resources(0, 16).await.unwrap();
                assert!(resources.records.iter().any(|record| record.stack == "app"));
                let events = Registry::open_read_only(state.join("registry.sqlite3"))
                    .unwrap()
                    .events(0, 16)
                    .unwrap();
                assert!(
                    events
                        .items
                        .iter()
                        .any(|record| record.kind == "manifest.ensure.succeeded")
                );
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn manifest_ensure_rollover_is_same_stack_only_and_same_generation_reuses_identity() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace_a = temporary.path().join("workspace-a");
        let workspace_b = temporary.path().join("workspace-b");
        std::fs::create_dir(&workspace_a).unwrap();
        std::fs::create_dir(&workspace_b).unwrap();
        let fake = Arc::new(FakeManifestEnsureExecutor::default());
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_manifest_ensure_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let request =
                    |workspace: PathBuf, manifest: &str, stack: &str| ManifestEnsureJobRequest {
                        workspace,
                        manifest: manifest.into(),
                        stack: stack.into(),
                        deadline: Duration::from_secs(2),
                        output_limit: 4096,
                    };
                let old = client
                    .submit_manifest_ensure(request(workspace_a.clone(), "old.toml", "app"))
                    .await
                    .unwrap();
                wait_for_job_state(&client, old, "Succeeded").await;
                // A completed same-generation request is a new durable job,
                // but reuses exactly its existing managed identity.
                let old_again = client
                    .submit_manifest_ensure(request(workspace_a.clone(), "old.toml", "app"))
                    .await
                    .unwrap();
                wait_for_job_state(&client, old_again, "Succeeded").await;
                let other_workspace = client
                    .submit_manifest_ensure(request(workspace_b.clone(), "workspace-b.toml", "app"))
                    .await
                    .unwrap();
                wait_for_job_state(&client, other_workspace, "Succeeded").await;
                let other_stack = client
                    .submit_manifest_ensure(request(
                        workspace_a.clone(),
                        "other-stack.toml",
                        "other",
                    ))
                    .await
                    .unwrap();
                wait_for_job_state(&client, other_stack, "Succeeded").await;
                let new = client
                    .submit_manifest_ensure(request(workspace_a.clone(), "new.toml", "app"))
                    .await
                    .unwrap();
                wait_for_job_state(&client, new, "Succeeded").await;

                let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
                let resources = registry.resources(0, 32).unwrap().items;
                let find = |id: &str| resources.iter().find(|resource| resource.id == id).unwrap();
                assert_eq!(
                    find("manifest-container:app:old").state,
                    ResourceState::Retired
                );
                assert_eq!(
                    find("manifest-container:app:new").state,
                    ResourceState::Active
                );
                assert_eq!(
                    find("manifest-container:app:workspace-b").state,
                    ResourceState::Active,
                    "other workspace must not be retired"
                );
                assert_eq!(
                    find("manifest-container:other:other-stack").state,
                    ResourceState::Active,
                    "other stack must not be retired"
                );
                assert_eq!(fake.calls.lock().unwrap().len(), 5);
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn manifest_rollover_is_atomic_when_current_image_identity_conflicts() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let workspace = "/canonical/manifest";
        record_manifest_ensure(
            &mut registry,
            1,
            &manifest_ensure_execution(workspace, "app", "old", "sha256:old-image"),
        )
        .unwrap();
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .put_resource(&Resource {
                id: "foreign-image".into(),
                kind: ResourceKind::Image,
                name: "manifest-image:sha256:new-image".into(),
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
        assert!(matches!(
            record_manifest_ensure(
                &mut registry,
                2,
                &manifest_ensure_execution(workspace, "app", "new", "sha256:new-image"),
            ),
            Err(bosn_registry::Error::ResourceIdentityConflict)
        ));
        let resources = registry.resources(0, 16).unwrap().items;
        assert_eq!(
            resources
                .iter()
                .find(|resource| resource.id == "manifest-container:app:old")
                .unwrap()
                .state,
            ResourceState::Active,
            "a failed new record cannot retire the previous generation"
        );
        assert!(
            resources
                .iter()
                .all(|resource| resource.id != "manifest-container:app:new")
        );
        assert_eq!(
            registry
                .events(0, 16)
                .unwrap()
                .items
                .iter()
                .filter(|event| event.kind == "manifest.ensure.succeeded")
                .count(),
            1,
            "the failed operation cannot leave a terminal success event"
        );
    }

    #[test]
    fn manifest_volume_intent_precedes_engine_work_and_is_consumed_with_success() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let workspace = "/canonical/manifest";
        let volume = manifest_volume_resource(workspace, "app");

        put_manifest_volume_intents(&mut registry, std::slice::from_ref(&volume)).unwrap();
        let intents = registry.volume_creation_intents(0, 8).unwrap().items;
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].name, volume.name);
        assert_eq!(intents[0].labels, volume.labels);

        let mut execution =
            manifest_ensure_execution(workspace, "app", "generation", "sha256:image");
        execution.volumes.push(volume.clone());
        record_manifest_ensure(&mut registry, 1, &execution).unwrap();

        assert!(
            registry
                .volume_creation_intents(0, 8)
                .unwrap()
                .items
                .is_empty()
        );
        let resources = registry.resources(0, 8).unwrap().items;
        let recorded = resources
            .iter()
            .find(|resource| resource.id == volume.id)
            .unwrap();
        assert_eq!(recorded.kind, ResourceKind::Volume);
        assert_eq!(recorded.name, volume.name);
        assert_eq!(recorded.state, ResourceState::Active);
        assert!(
            registry
                .resource_uses(0, 8)
                .unwrap()
                .items
                .iter()
                .any(|use_record| use_record.resource_id == volume.id)
        );
    }

    #[test]
    fn reconcile_missing_token_is_exact_and_rejects_gc_or_tampered_forms() {
        let candidate = SetupReconcileCandidate {
            resource: Resource {
                id: "setup-container:abc".into(),
                kind: ResourceKind::Container,
                name: "bosn-setup-abc".into(),
                stack: "setup".into(),
                generation: "sha256:abc".into(),
                scope: Scope::Machine,
                workspace: "/work".into(),
                created_at: 1.0,
                last_used: 1.0,
                state: ResourceState::Active,
                retention: Retention::Pinned,
            },
            image_identities: vec![],
            missing_repairable: true,
        };
        let token = setup_reconcile_missing_token(&candidate);
        assert_eq!(
            parse_setup_reconcile_missing_token(&token).unwrap(),
            (
                "setup-container:abc".into(),
                "bosn-setup-abc".into(),
                "sha256:abc".into()
            )
        );
        assert!(parse_setup_reconcile_missing_token(&(token + "00")).is_err());
        assert!(parse_setup_reconcile_missing_token("sgc1-7465737400").is_err());
    }

    #[test]
    fn reconcile_classifies_all_read_only_observations_conservatively() {
        let candidate = SetupReconcileCandidate {
            resource: Resource {
                id: "setup-container:abc".into(),
                kind: ResourceKind::Container,
                name: "bosn-setup-abc".into(),
                stack: "setup".into(),
                generation: "sha256:abc".into(),
                scope: Scope::Machine,
                workspace: "/private/work".into(),
                created_at: 1.0,
                last_used: 1.0,
                state: ResourceState::Active,
                retention: Retention::Pinned,
            },
            image_identities: vec!["sha256:image".into()],
            missing_repairable: true,
        };
        let observed = |running| SetupReconcileObserved {
            name: "/bosn-setup-abc".into(),
            running,
            image_identity: "sha256:image".into(),
            managed: "v1".into(),
            content: "abc".into(),
            container: "bosn-setup-abc".into(),
        };
        assert_eq!(
            classify_setup_reconcile(&candidate, Ok(Some(observed(true)))),
            "matching_running"
        );
        assert_eq!(
            classify_setup_reconcile(&candidate, Ok(Some(observed(false)))),
            "matching_stopped"
        );
        assert_eq!(classify_setup_reconcile(&candidate, Ok(None)), "missing");
        let mut value = observed(true);
        value.name = "/wrong".into();
        assert_eq!(
            classify_setup_reconcile(&candidate, Ok(Some(value))),
            "name_mismatch"
        );
        let mut value = observed(true);
        value.managed = "foreign".into();
        assert_eq!(
            classify_setup_reconcile(&candidate, Ok(Some(value))),
            "label_mismatch"
        );
        let mut value = observed(true);
        value.image_identity = "sha256:wrong".into();
        assert_eq!(
            classify_setup_reconcile(&candidate, Ok(Some(value))),
            "image_mismatch"
        );
        assert_eq!(
            classify_setup_reconcile(&candidate, Err("deadline".into())),
            "inspect_error"
        );
        let mut malformed = candidate.clone();
        malformed.resource.generation = "bad".into();
        assert_eq!(
            classify_setup_reconcile(&malformed, Ok(Some(observed(true)))),
            "unknown"
        );
    }

    #[test]
    fn reconcile_preview_wire_is_bounded_and_rejects_every_nonsemantic_control() {
        let valid = Request {
            workspace: "/private/work".into(),
            diagnostic_after: 0,
            diagnostic_limit: 1,
            ..Request::operation(19)
        };
        assert!(validate_setup_reconcile_preview_request_wire(&valid).is_ok());
        let mut malformed = valid;
        malformed.setup_config = "docker run attacker".into();
        assert!(validate_setup_reconcile_preview_request_wire(&malformed).is_err());
        malformed.setup_config.clear();
        malformed.diagnostic_limit = MAX_REGISTRY_DIAGNOSTIC_PAGE + 1;
        assert!(validate_setup_reconcile_preview_request_wire(&malformed).is_err());
    }

    #[test]
    fn reconcile_missing_repair_wire_requires_confirmation_and_only_a_token() {
        let valid = Request {
            workspace: "/work".into(),
            gc_candidate_token: "srm1-610062006300".into(),
            gc_confirm: true,
            ..Request::operation(20)
        };
        assert!(validate_setup_reconcile_repair_missing_request_wire(&valid).is_ok());
        let missing_confirmation = Request {
            workspace: "/work".into(),
            gc_candidate_token: "srm1-610062006300".into(),
            gc_confirm: false,
            ..Request::operation(20)
        };
        assert!(
            validate_setup_reconcile_repair_missing_request_wire(&missing_confirmation).is_err()
        );
        let nonsemantic = Request {
            workspace: "/work".into(),
            gc_candidate_token: "srm1-610062006300".into(),
            gc_confirm: true,
            setup_config: "https://attacker.invalid/setup.toml".into(),
            ..Request::operation(20)
        };
        assert!(validate_setup_reconcile_repair_missing_request_wire(&nonsemantic).is_err());
    }

    #[test]
    fn retired_stop_wire_requires_only_preview_identity_and_confirmation() {
        let request = || {
            Request {
            workspace: "/workspace".into(),
            gc_candidate_token: "sgc1-73657475702d636f6e7461696e65723a6100626f736e2d73657475702d61007368613235363a6100".into(),
            gc_confirm: true,
            ..Request::operation(18)
        }
        };
        assert!(validate_setup_retired_stop_request_wire(&request()).is_ok());
        for invalid in [
            Request {
                gc_confirm: false,
                ..request()
            },
            Request {
                setup_deadline_ms: 1,
                ..request()
            },
            Request {
                setup_config: "docker://bad".into(),
                ..request()
            },
            Request {
                diagnostic_limit: 1,
                ..request()
            },
        ] {
            assert!(validate_setup_retired_stop_request_wire(&invalid).is_err());
        }
    }

    #[test]
    fn setup_done_wire_requires_only_explicit_confirmation() {
        let request = || Request {
            workspace: "/workspace".into(),
            setup_done_confirm: true,
            ..Request::operation(16)
        };
        assert!(validate_setup_done_request_wire(&request()).is_ok());
        for invalid in [
            Request {
                setup_done_confirm: false,
                ..request()
            },
            Request {
                setup_config: "https://user:secret@example.invalid/setup.toml".into(),
                ..request()
            },
            Request {
                gc_confirm: true,
                ..request()
            },
            Request {
                setup_task_name: "injected".into(),
                ..request()
            },
        ] {
            assert!(validate_setup_done_request_wire(&invalid).is_err());
        }
    }

    #[test]
    fn setup_ensure_reactivates_records_after_explicit_done() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let workspace = "/canonical/workspace";
        let execution = setup_ensure_execution(workspace, "same", "sha256:image");
        record_setup_ensure(&mut registry, 1, &execution).unwrap();
        let mut transaction = registry.begin_immediate().unwrap();
        assert_eq!(
            transaction
                .complete_setup_workspace(workspace, 2.0)
                .unwrap()
                .uses_completed,
            2
        );
        transaction.commit().unwrap();
        assert!(
            registry
                .resource_uses(0, 10)
                .unwrap()
                .items
                .iter()
                .all(|use_row| use_row.state == ResourceState::Done)
        );
        record_setup_ensure(&mut registry, 2, &execution).unwrap();
        assert!(
            registry
                .resources(0, 10)
                .unwrap()
                .items
                .iter()
                .all(|resource| resource.state == ResourceState::Active)
        );
        assert!(
            registry
                .resource_uses(0, 10)
                .unwrap()
                .items
                .iter()
                .all(|use_row| use_row.state == ResourceState::Active)
        );
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

    struct FakeSetupAppTaskExecutor {
        started: AtomicUsize,
        observed: Mutex<Vec<String>>,
    }
    impl FakeSetupAppTaskExecutor {
        fn new() -> Self {
            Self {
                started: AtomicUsize::new(0),
                observed: Mutex::new(Vec::new()),
            }
        }
    }
    impl SetupAppTaskExecutor for FakeSetupAppTaskExecutor {
        fn execute<'a>(
            &'a self,
            request: SetupAppTaskJobRequest,
            _cancellation: &'a async_engine::CancellationToken,
            logs: &'a async_engine::Sender<String>,
            session: &'a dyn SetupAppTaskSessionRecorder,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                self.observed
                    .lock()
                    .unwrap()
                    .push(request.task_name.clone());
                session.begin("owned-container-id".into()).await?;
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
    }
    impl FakeManifestAppTaskExecutor {
        fn new() -> Self {
            Self {
                observed: Mutex::new(Vec::new()),
            }
        }
    }
    impl ManifestAppTaskExecutor for FakeManifestAppTaskExecutor {
        fn execute<'a>(
            &'a self,
            request: ManifestAppTaskJobRequest,
            _cancellation: &'a async_engine::CancellationToken,
            logs: &'a async_engine::Sender<String>,
            session: &'a dyn ManifestAppTaskSessionRecorder,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.observed.lock().unwrap().push(request.clone());
                session.begin("bosn-setup-manifest-identity".into()).await?;
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
                    volumes: Vec::new(),
                })
            })
        }
    }

    #[derive(Default)]
    struct FakeManifestEnsureExecutor {
        calls: Mutex<Vec<ManifestEnsureJobRequest>>,
    }
    impl ManifestEnsureExecutor for FakeManifestEnsureExecutor {
        fn execute<'a>(
            &'a self,
            request: ManifestEnsureJobRequest,
            _cancellation: &'a async_engine::CancellationToken,
            logs: &'a async_engine::Sender<String>,
            _registry: &'a RegistryActor,
        ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>>
        {
            Box::pin(async move {
                logs.send("[fake] manifest stack ensured".into())
                    .await
                    .map_err(|_| "fake manifest log consumer closed".to_owned())?;
                self.calls.lock().unwrap().push(request.clone());
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
                        name: format!("bosn-setup-{generation}"),
                        stack: request.stack.clone(),
                        generation: format!("sha256:{generation}"),
                        workspace: workspace.clone(),
                    },
                    image: SetupEnsureImageResource {
                        id: format!("manifest-image:sha256:{generation}"),
                        name: format!("manifest-image:sha256:{generation}"),
                        stack: request.stack,
                        generation: format!("sha256:{generation}"),
                        workspace,
                    },
                    volumes: Vec::new(),
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
            named_volumes: Vec::new(),
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
    fn setup_app_task_is_prompt_typed_and_clears_its_durable_session() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(FakeSetupAppTaskExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_app_task_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let request = SetupAppTaskJobRequest {
                    workspace,
                    config: "https://example.invalid/setup.toml".into(),
                    policy: SetupPreparePolicy::Refresh,
                    task_name: "check".into(),
                    deadline: Duration::from_secs(2),
                    output_limit: 4096,
                };
                let submitted = std::time::Instant::now();
                let first = client.submit_setup_app_task(request.clone()).await.unwrap();
                assert!(submitted.elapsed() < Duration::from_millis(250));
                assert_eq!(
                    first,
                    client.submit_setup_app_task(request).await.unwrap(),
                    "identical semantic app-task requests coalesce"
                );
                wait_for_job_state(&client, first, "Succeeded").await;
                assert_eq!(fake.started.load(Ordering::SeqCst), 1);
                assert_eq!(*fake.observed.lock().unwrap(), vec!["check"]);
                assert_eq!(client.status().await.unwrap().sessions, 0);
                let logs = client.job_logs(first, 0, 8).await.unwrap();
                assert!(
                    logs.records
                        .iter()
                        .any(|record| record.line.contains("declared app task"))
                );
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn manifest_app_task_is_prompt_typed_and_clears_its_durable_session() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(FakeManifestAppTaskExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_manifest_app_task_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let request = ManifestAppTaskJobRequest {
                    workspace,
                    manifest: "bosn.toml".into(),
                    stack: "app".into(),
                    task_name: "check".into(),
                    deadline: Duration::from_secs(2),
                    output_limit: 4096,
                };
                let first = client
                    .submit_manifest_app_task(request.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    first,
                    client.submit_manifest_app_task(request).await.unwrap()
                );
                wait_for_job_state(&client, first, "Succeeded").await;
                assert_eq!(fake.observed.lock().unwrap().len(), 1);
                assert_eq!(client.status().await.unwrap().sessions, 0);
                assert!(
                    client
                        .job_logs(first, 0, 8)
                        .await
                        .unwrap()
                        .records
                        .iter()
                        .any(|record| record.line.contains("manifest app task"))
                );
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn uncertain_app_task_uses_verified_managed_receipt_identity_to_protect_gc() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = state.join("registry.sqlite3");
        let mut registry =
            Registry::create_writer(&database, "11111111-2222-4333-8444-555555555555").unwrap();
        let mut transaction = registry.begin_immediate().unwrap();
        let generation = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let resource_id = format!("setup-container:{generation}");
        let container_name = format!("bosn-setup-{generation}");
        transaction
            .put_resource(&Resource {
                id: resource_id.clone(),
                kind: ResourceKind::Container,
                name: container_name.clone(),
                stack: "setup".into(),
                generation: format!("sha256:{generation}"),
                scope: Scope::Machine,
                workspace: "/workspace".into(),
                created_at: 1.0,
                last_used: 1.0,
                state: ResourceState::Retired,
                retention: Retention::Pinned,
            })
            .unwrap();
        transaction
            .put_resource_use(&ResourceUse {
                resource_id: resource_id.clone(),
                workspace: "/workspace".into(),
                stack: "setup".into(),
                generation: format!("sha256:{generation}"),
                last_used: 1.0,
                state: ResourceState::Retired,
            })
            .unwrap();
        transaction.commit().unwrap();

        // This has the actual receipt shape returned by the ownership-safe
        // Docker inspection: its opaque Docker ID must never be used as the
        // durable registry/GC key.
        let observed = SetupEnsureResult {
            container_name: container_name.clone(),
            container_id: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
            image_identity:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            created: false,
            started: false,
            running: true,
        };
        let managed_identity = setup_app_task_session_container_identity(&observed);
        assert_eq!(managed_identity, container_name);
        assert_ne!(managed_identity, observed.container_id);
        record_setup_app_task_session(&mut registry, 7, &managed_identity).unwrap();
        finish_setup_app_task_session(&mut registry, 7, "uncertain").unwrap();
        assert_eq!(registry.status().unwrap().sessions, 1);
        assert_eq!(
            registry.execution_sessions(0, 1).unwrap().items[0].container_id,
            container_name
        );
        let protected = registry.setup_gc_preview("/workspace", 0, 16).unwrap();
        assert!(protected.candidates.items.is_empty());
        assert_eq!(protected.counts.protected_session, 1);

        finish_setup_app_task_session(&mut registry, 7, "failed").unwrap();
        assert_eq!(registry.status().unwrap().sessions, 0);
        let eligible = registry.setup_gc_preview("/workspace", 0, 16).unwrap();
        assert_eq!(eligible.candidates.items.len(), 1);
        assert_eq!(eligible.candidates.items[0].id, resource_id);
    }

    #[test]
    fn uncertain_manifest_app_task_session_protects_matching_manifest_container() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let database = temporary.path().join("registry.sqlite3");
        let mut registry =
            Registry::create_writer(&database, "11111111-2222-4333-8444-555555555555").unwrap();
        let generation = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let name = "bosn-setup-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let id = "manifest-container:app:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .put_resource(&Resource {
                id: id.into(),
                kind: ResourceKind::Container,
                name: name.into(),
                stack: "app".into(),
                generation: generation.into(),
                scope: Scope::Machine,
                workspace: "/workspace".into(),
                created_at: 1.0,
                last_used: 1.0,
                state: ResourceState::Retired,
                retention: Retention::Pinned,
            })
            .unwrap();
        transaction
            .put_resource_use(&ResourceUse {
                resource_id: id.into(),
                workspace: "/workspace".into(),
                stack: "app".into(),
                generation: generation.into(),
                last_used: 1.0,
                state: ResourceState::Retired,
            })
            .unwrap();
        transaction.commit().unwrap();
        record_manifest_app_task_session(&mut registry, 8, name).unwrap();
        finish_manifest_app_task_session(&mut registry, 8, "uncertain").unwrap();
        assert_eq!(
            registry.execution_sessions(0, 1).unwrap().items[0].container_id,
            name
        );
        assert!(
            registry
                .setup_gc_preview("/workspace", 0, 16)
                .unwrap()
                .candidates
                .items
                .is_empty()
        );
        finish_manifest_app_task_session(&mut registry, 8, "failed").unwrap();
        assert_eq!(
            registry
                .setup_gc_preview("/workspace", 0, 16)
                .unwrap()
                .candidates
                .items
                .len(),
            1
        );
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
        let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        let events = registry.events(0, 10).unwrap().items;
        assert_eq!(
            events
                .iter()
                .map(|event| (event.kind.as_str(), event.detail.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "setup.ensure.submitted",
                    "job_id=1 policy=refresh source=https",
                ),
                (
                    "setup.ensure.submitted",
                    "job_id=2 policy=refresh source=https",
                ),
                ("setup.ensure.cancelled", "job_id=1 outcome=cancelled"),
                ("setup.ensure.succeeded", "job_id=2 outcome=succeeded"),
            ]
        );
    }

    #[test]
    fn setup_ensure_stops_after_prepare_failure_or_ownership_mismatch_without_mutation() {
        for config in [
            "https://user:secret@example.invalid/prepare-fail.toml?token=not-for-events",
            "https://user:secret@example.invalid/ensure-mismatch.toml?token=not-for-events",
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
            let events = registry.events(0, 10).unwrap().items;
            assert_eq!(
                events
                    .iter()
                    .map(|event| (event.kind.as_str(), event.detail.as_str()))
                    .collect::<Vec<_>>(),
                vec![
                    (
                        "setup.ensure.submitted",
                        "job_id=1 policy=offline source=https",
                    ),
                    ("setup.ensure.failed", "job_id=1 outcome=failed"),
                ]
            );
            let rendered = format!("{events:?}");
            for sensitive in [
                "user:secret",
                "token=",
                "not-for-events",
                "prepare-fail",
                "ensure-mismatch",
            ] {
                assert!(!rendered.contains(sensitive), "event leaked {sensitive}");
            }
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
        let events = registry.events(0, 10).unwrap().items;
        assert_eq!(
            events
                .iter()
                .map(|event| (event.kind.as_str(), event.detail.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "setup.ensure.submitted",
                    "job_id=1 policy=refresh source=https",
                ),
                ("setup.ensure.succeeded", "job_id=1 outcome=succeeded"),
                (
                    "setup.ensure.submitted",
                    "job_id=1 policy=refresh source=https",
                ),
                ("setup.ensure.succeeded", "job_id=1 outcome=succeeded"),
            ]
        );
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
            volumes: Vec::new(),
        };
        assert!(matches!(
            record_setup_ensure(&mut registry, 7, &execution),
            Err(bosn_registry::Error::ResourceIdentityConflict)
        ));
        // The failed image upsert rolls back the preceding container and both
        // use rows; only the deliberate pre-existing conflicting row remains.
        let resources = registry.resources(0, 10).unwrap().items;
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].id, "foreign-image-row");
        assert!(registry.resource_uses(0, 10).unwrap().items.is_empty());
        // The terminal success event is part of the same transaction as its
        // resources, so an identity conflict cannot leave a false success.
        assert!(registry.events(0, 10).unwrap().items.is_empty());
    }

    #[test]
    fn setup_adoption_restores_absent_records_but_refuses_incompatible_existing_state() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let execution = setup_ensure_execution("/verified/workspace", "document", "sha256:image");
        record_setup_adoption(&mut registry, &execution).unwrap();
        assert_eq!(registry.resources(0, 10).unwrap().items.len(), 2);
        assert_eq!(registry.resource_uses(0, 10).unwrap().items.len(), 2);
        assert_eq!(
            registry.setup_ensure_events(0, 10).unwrap().items[0].kind,
            "setup.ensure.adopted"
        );
        // Same exact durable state is idempotent.
        record_setup_adoption(&mut registry, &execution).unwrap();
        let mut conflicting = execution.clone();
        conflicting.resource.workspace = "/other/workspace".into();
        assert!(matches!(
            record_setup_adoption(&mut registry, &conflicting),
            Err(bosn_registry::Error::ResourceIdentityConflict)
        ));
        let resources = registry.resources(0, 10).unwrap().items;
        assert_eq!(resources.len(), 2);
        assert!(
            resources
                .iter()
                .all(|resource| resource.workspace == "/verified/workspace")
        );
    }

    #[test]
    fn setup_ensure_generation_rollover_retires_only_prior_setup_containers() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = temporary.path().join("registry.sqlite3");
        let workspace_a = "/canonical/workspace-a";
        let workspace_b = "/canonical/workspace-b";
        let shared_image = "sha256:shared-image";
        let mut registry =
            Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();

        record_setup_ensure(
            &mut registry,
            1,
            &setup_ensure_execution(workspace_a, "generation-a", shared_image),
        )
        .unwrap();

        // A container outside the `setup` stack is deliberately in the
        // product namespace too. Rollover must still leave it untouched.
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .put_resource(&Resource {
                id: "setup-container:other-stack".into(),
                kind: ResourceKind::Container,
                name: "other-stack-container".into(),
                stack: "other".into(),
                generation: "sha256:other".into(),
                scope: Scope::Machine,
                workspace: workspace_a.into(),
                created_at: 1.0,
                last_used: 1.0,
                state: ResourceState::Active,
                retention: Retention::Pinned,
            })
            .unwrap();
        transaction
            .put_resource_use(&ResourceUse {
                resource_id: "setup-container:other-stack".into(),
                workspace: workspace_a.into(),
                stack: "other".into(),
                generation: "sha256:other".into(),
                last_used: 1.0,
                state: ResourceState::Active,
            })
            .unwrap();
        transaction.commit().unwrap();
        // The registry primitive is deliberately hard-scoped to `setup`;
        // even an internal caller cannot reuse it to retire another stack.
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .retire_prior_setup_container_generations(workspace_a, "other", "sha256:new")
            .unwrap();
        transaction.commit().unwrap();

        // The exact same inspected image is shared across documents and
        // workspaces. It must never be retired during a container rollover.
        record_setup_ensure(
            &mut registry,
            2,
            &setup_ensure_execution(workspace_b, "generation-c", shared_image),
        )
        .unwrap();
        record_setup_ensure(
            &mut registry,
            3,
            &setup_ensure_execution(workspace_a, "generation-b", shared_image),
        )
        .unwrap();
        // Re-ensuring the current content is an active idempotent upsert, not
        // another retirement transition.
        record_setup_ensure(
            &mut registry,
            4,
            &setup_ensure_execution(workspace_a, "generation-b", shared_image),
        )
        .unwrap();
        drop(registry);

        // Reopen to prove terminal ownership accounting survives a daemon
        // restart rather than being an in-memory observation.
        let registry = Registry::open_read_only(&path).unwrap();
        let resources = registry.resources(0, 16).unwrap().items;
        let resource = |id: &str| resources.iter().find(|value| value.id == id).unwrap();
        assert_eq!(
            resource("setup-container:generation-a").state,
            ResourceState::Retired
        );
        assert_eq!(
            resource("setup-container:generation-b").state,
            ResourceState::Active
        );
        assert_eq!(
            resource("setup-container:generation-c").state,
            ResourceState::Active
        );
        assert_eq!(
            resource("setup-container:other-stack").state,
            ResourceState::Active
        );
        assert_eq!(
            resource(&format!("setup-image:{shared_image}")).state,
            ResourceState::Active
        );

        let uses = registry.resource_uses(0, 32).unwrap().items;
        let use_state = |id: &str, workspace: &str, stack: &str, generation: &str| {
            uses.iter()
                .find(|value| {
                    value.resource_id == id
                        && value.workspace == workspace
                        && value.stack == stack
                        && value.generation == generation
                })
                .unwrap()
                .state
        };
        assert_eq!(
            use_state(
                "setup-container:generation-a",
                workspace_a,
                "setup",
                "sha256:generation-a"
            ),
            ResourceState::Retired
        );
        assert_eq!(
            use_state(
                "setup-container:generation-b",
                workspace_a,
                "setup",
                "sha256:generation-b"
            ),
            ResourceState::Active
        );
        assert_eq!(
            use_state(
                "setup-container:generation-c",
                workspace_b,
                "setup",
                "sha256:generation-c"
            ),
            ResourceState::Active
        );
        assert_eq!(
            use_state(
                "setup-container:other-stack",
                workspace_a,
                "other",
                "sha256:other"
            ),
            ResourceState::Active
        );
        for image_use in uses
            .iter()
            .filter(|value| value.resource_id == format!("setup-image:{shared_image}"))
        {
            assert_eq!(image_use.state, ResourceState::Active);
        }
    }

    #[test]
    fn retired_stop_registry_confirmation_preserves_candidate_and_rejects_stale_state() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let workspace = "/canonical/retired-stop";
        record_setup_ensure(
            &mut registry,
            1,
            &setup_ensure_execution(workspace, "old", "sha256:image"),
        )
        .unwrap();
        record_setup_ensure(
            &mut registry,
            2,
            &setup_ensure_execution(workspace, "new", "sha256:image"),
        )
        .unwrap();
        let candidate = registry
            .setup_gc_candidate(
                workspace,
                "setup-container:old",
                "bosn-setup-old",
                "sha256:old",
            )
            .unwrap()
            .expect("retired candidate");
        let mut tx = registry.begin_immediate().unwrap();
        assert!(
            tx.confirm_setup_retired_container_stopped(
                workspace,
                &candidate.id,
                &candidate.name,
                &candidate.generation,
                3.0,
            )
            .unwrap()
        );
        tx.commit().unwrap();
        assert!(
            registry
                .setup_gc_candidate(
                    workspace,
                    &candidate.id,
                    &candidate.name,
                    &candidate.generation,
                )
                .unwrap()
                .is_some()
        );
        // A stale identity is a no-write failure, not permission to append an
        // event after a resource/use/lease/session protection race.
        let events_before = registry.events(0, 16).unwrap().items.len();
        let mut tx = registry.begin_immediate().unwrap();
        assert!(
            !tx.confirm_setup_retired_container_stopped(
                workspace,
                "setup-container:other",
                &candidate.name,
                &candidate.generation,
                4.0,
            )
            .unwrap()
        );
        drop(tx);
        assert_eq!(registry.events(0, 16).unwrap().items.len(), events_before);
    }

    #[test]
    fn setup_ensure_rollover_conflict_rolls_back_without_retiring_current_generation() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let workspace = "/canonical/workspace";
        record_setup_ensure(
            &mut registry,
            1,
            &setup_ensure_execution(workspace, "generation-a", "sha256:image-a"),
        )
        .unwrap();

        // Make the later image upsert fail after the next generation's
        // container would otherwise have been accepted. The immediate
        // transaction must preserve the active old generation and its use.
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .put_resource(&Resource {
                id: "foreign-image".into(),
                kind: ResourceKind::Image,
                name: "setup-image:sha256:image-b".into(),
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
        assert!(matches!(
            record_setup_ensure(
                &mut registry,
                2,
                &setup_ensure_execution(workspace, "generation-b", "sha256:image-b"),
            ),
            Err(bosn_registry::Error::ResourceIdentityConflict)
        ));
        let old = registry
            .resources(0, 16)
            .unwrap()
            .items
            .into_iter()
            .find(|value| value.id == "setup-container:generation-a")
            .unwrap();
        assert_eq!(old.state, ResourceState::Active);
        let old_use = registry
            .resource_uses(0, 16)
            .unwrap()
            .items
            .into_iter()
            .find(|value| value.resource_id == "setup-container:generation-a")
            .unwrap();
        assert_eq!(old_use.state, ResourceState::Active);
        assert!(
            registry
                .resources(0, 16)
                .unwrap()
                .items
                .iter()
                .all(|value| value.id != "setup-container:generation-b")
        );
    }

    #[test]
    fn setup_ensure_rollover_never_retires_a_container_shared_by_another_workspace() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let workspace_a = "/canonical/workspace-a";
        let workspace_b = "/canonical/workspace-b";
        record_setup_ensure(
            &mut registry,
            1,
            &setup_ensure_execution(workspace_a, "generation-a", "sha256:image-a"),
        )
        .unwrap();
        // This is not normal setup-app ownership (the content-addressed
        // container should not be shared across workspaces), but it proves
        // that accounting fails closed rather than retiring a global resource
        // observed by another workspace.
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .put_resource_use(&ResourceUse {
                resource_id: "setup-container:generation-a".into(),
                workspace: workspace_b.into(),
                stack: "setup".into(),
                generation: "sha256:generation-a".into(),
                last_used: 1.0,
                state: ResourceState::Active,
            })
            .unwrap();
        transaction.commit().unwrap();
        record_setup_ensure(
            &mut registry,
            2,
            &setup_ensure_execution(workspace_a, "generation-b", "sha256:image-b"),
        )
        .unwrap();

        let resources = registry.resources(0, 16).unwrap().items;
        assert_eq!(
            resources
                .iter()
                .find(|value| value.id == "setup-container:generation-a")
                .unwrap()
                .state,
            ResourceState::Active
        );
        let uses = registry.resource_uses(0, 16).unwrap().items;
        assert!(
            uses.iter()
                .filter(|value| value.resource_id == "setup-container:generation-a")
                .all(|value| value.state == ResourceState::Active)
        );
        assert_eq!(
            resources
                .iter()
                .find(|value| value.id == "setup-container:generation-b")
                .unwrap()
                .state,
            ResourceState::Active
        );
    }

    #[test]
    fn setup_ensure_rollover_is_visible_through_daemon_registry_diagnostics() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let workspace = "/canonical/workspace";
        let mut registry = Registry::create_writer(
            state.join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        record_setup_ensure(
            &mut registry,
            1,
            &setup_ensure_execution(workspace, "generation-a", "sha256:image"),
        )
        .unwrap();
        record_setup_ensure(
            &mut registry,
            2,
            &setup_ensure_execution(workspace, "generation-b", "sha256:image"),
        )
        .unwrap();
        drop(registry);

        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(Service::new(state.clone()).serve());
                let client = wait_for_client(&state).await;
                let page = client.registry_resources(0, 16).await.unwrap();
                assert_eq!(
                    page.records
                        .iter()
                        .find(|value| value.id == "setup-container:generation-a")
                        .unwrap()
                        .state,
                    "retired"
                );
                assert_eq!(
                    page.records
                        .iter()
                        .find(|value| value.id == "setup-container:generation-b")
                        .unwrap()
                        .state,
                    "active"
                );
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    #[test]
    fn cancelled_setup_ensure_does_not_persist_or_retire_existing_resources() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let canonical_workspace = workspace.to_string_lossy().into_owned();
        let mut initial_registry = Registry::create_writer(
            state.join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        record_setup_ensure(
            &mut initial_registry,
            1,
            &setup_ensure_execution(
                &canonical_workspace,
                "existing-generation",
                "sha256:existing-image",
            ),
        )
        .unwrap();
        drop(initial_registry);
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
        let resources = registry.resources(0, 10).unwrap().items;
        assert_eq!(resources.len(), 2);
        assert!(
            resources
                .iter()
                .all(|resource| resource.state == ResourceState::Active)
        );
        let uses = registry.resource_uses(0, 10).unwrap().items;
        assert_eq!(uses.len(), 2);
        assert!(
            uses.iter()
                .all(|resource_use| resource_use.state == ResourceState::Active)
        );
        assert_eq!(
            registry
                .events(0, 10)
                .unwrap()
                .items
                .iter()
                .map(|event| (event.kind.as_str(), event.detail.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("setup.ensure.succeeded", "job_id=1 outcome=succeeded"),
                (
                    "setup.ensure.submitted",
                    "job_id=1 policy=refresh source=https",
                ),
                ("setup.ensure.cancelled", "job_id=1 outcome=cancelled"),
            ]
        );
    }

    #[test]
    fn shutdown_cancelled_setup_ensure_keeps_a_durable_terminal_event() {
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
                client
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
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
        let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        assert!(registry.resources(0, 10).unwrap().items.is_empty());
        assert_eq!(
            registry
                .events(0, 10)
                .unwrap()
                .items
                .iter()
                .map(|event| (event.kind.as_str(), event.detail.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "setup.ensure.submitted",
                    "job_id=1 policy=refresh source=https",
                ),
                ("setup.ensure.cancelled", "job_id=1 outcome=cancelled"),
            ]
        );
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
                        app_task: Arc::new(FakeSetupAppTaskExecutor::new()),
                        ensure: fake,
                        manifest_ensure: Arc::new(DockerManifestEnsureExecutor::new()),
                        manifest_app_task: Arc::new(DockerManifestAppTaskExecutor::new()),
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
    fn registry_diagnostics_wire_rejects_nonsemantic_or_unbounded_fields() {
        let request = || Request {
            diagnostic_after: 0,
            diagnostic_limit: 1,
            ..Request::operation(11)
        };
        assert!(validate_registry_diagnostics_request_wire(&request()).is_ok());
        for invalid in [
            Request {
                workspace: "/attacker".into(),
                ..request()
            },
            Request {
                setup_config: "https://user:secret@example.invalid/setup.toml".into(),
                ..request()
            },
            Request {
                job_id: 1,
                ..request()
            },
            Request {
                diagnostic_limit: MAX_REGISTRY_DIAGNOSTIC_PAGE + 1,
                ..request()
            },
        ] {
            assert!(validate_registry_diagnostics_request_wire(&invalid).is_err());
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
    fn daemon_registry_diagnostics_are_bounded_safe_and_read_only() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let db = state.join("registry.sqlite3");
        let mut registry =
            Registry::create_writer(&db, "00000000-0000-4000-8000-000000000123").unwrap();
        let mut tx = registry.begin_immediate().unwrap();
        tx.put_resource(&Resource {
            id: "managed".into(),
            kind: ResourceKind::Container,
            name: "managed-app".into(),
            stack: "setup".into(),
            generation: "sha256:managed".into(),
            scope: Scope::Machine,
            workspace: "/private/workspace".into(),
            created_at: 1.0,
            last_used: 2.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })
        .unwrap();
        tx.append_event(1.0, "unrelated", "not exposed").unwrap();
        tx.append_event(2.0, "setup.ensure.succeeded", "job_id=1 outcome=succeeded")
            .unwrap();
        tx.commit().unwrap();
        drop(registry);
        let before = std::fs::metadata(&db).unwrap().len();
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let resources = client.registry_resources(0, 1).await.unwrap();
            assert_eq!(resources.records.len(), 1);
            assert_eq!(resources.records[0].id, "managed");
            assert_eq!(resources.records[0].name, "managed-app");
            assert!(!format!("{:?}", resources.records[0]).contains("/private/workspace"));
            let events = client.setup_ensure_events(0, 1).await.unwrap();
            assert_eq!(events.records[0].kind, "setup.ensure.succeeded");
            assert!(!events.records.iter().any(|event| event.kind == "unrelated"));
            assert!(matches!(
                client.registry_resources(0, 0).await,
                Err(Error::Protocol("invalid registry page limit"))
            ));
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
        assert_eq!(std::fs::metadata(&db).unwrap().len(), before);
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
                diagnostic_after: 0,
                diagnostic_limit: 0,
                gc_candidate_token: String::new(),
                gc_confirm: false,
                setup_done_confirm: false,
                setup_adopt_confirm: false,
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
    fn doctor_is_daemon_owned_read_only_and_uses_a_typed_fake_engine() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let fake = Arc::new(FakeDoctorExecutor::ready());
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_doctor_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let database = state.join("registry.sqlite3");
            let before = std::fs::read(&database).unwrap();
            let report = client.doctor().await.unwrap();
            assert_eq!(report.daemon, "ready");
            assert_eq!(report.registry, "ready");
            assert_eq!(report.engine, "ready");
            assert_eq!(report.client_version.as_deref(), Some("29.0.1"));
            assert_eq!(report.server_version.as_deref(), Some("29.0.1"));
            assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read(&database).unwrap(), before);
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn doctor_wire_rejects_every_caller_control() {
        let request = || Request::operation(13);
        assert!(validate_doctor_request_wire(&request()).is_ok());
        for invalid in [
            Request {
                workspace: "/path".into(),
                ..request()
            },
            Request {
                setup_config: "https://user:secret@example.invalid/a".into(),
                ..request()
            },
            Request {
                setup_deadline_ms: 1,
                ..request()
            },
            Request {
                setup_output_limit: 1,
                ..request()
            },
            Request {
                diagnostic_limit: 1,
                ..request()
            },
            Request {
                job_id: 1,
                ..request()
            },
        ] {
            assert!(validate_doctor_request_wire(&invalid).is_err());
        }
    }

    #[test]
    fn doctor_missing_daemon_is_typed_and_does_not_create_state() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("missing-state");
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report = runtime
            .run(Client::for_state(&state).unwrap().doctor())
            .unwrap();
        assert_eq!(report.daemon, "unavailable");
        assert_eq!(report.registry, "unavailable");
        assert_eq!(report.engine, "unavailable");
        assert!(!state.exists());
    }

    #[test]
    fn doctor_executor_deadline_is_typed_without_waiting_for_a_slow_engine() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_doctor_executor(Arc::new(SlowDoctorExecutor))
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let started = std::time::Instant::now();
            let report = client.doctor().await.unwrap();
            assert_eq!(report.daemon, "ready");
            assert_eq!(report.registry, "ready");
            assert_eq!(report.engine, "deadline");
            assert!(started.elapsed() < Duration::from_secs(3));
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
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
