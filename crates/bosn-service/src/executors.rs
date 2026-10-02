//! Job requests and the executor traits the daemon calls (test seams, not client authority).

use super::*;

/// Explicit policy for one daemon-owned setup image preparation request.
/// State is selected by [`Client::for_state`] and then owned by the daemon;
/// callers cannot substitute a state root in the RPC itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupPreparePolicy {
    Refresh,
    Offline,
}
impl SetupPreparePolicy {
    pub(crate) fn wire(self) -> u32 {
        match self {
            Self::Refresh => 1,
            Self::Offline => 2,
        }
    }
    pub(crate) fn from_wire(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Refresh),
            2 => Some(Self::Offline),
            _ => None,
        }
    }
    pub(crate) fn acquire_policy(self) -> SetupAcquirePolicy {
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
/// One bounded daemon-owned convergence of every stack declared by one
/// legacy Bosn manifest. The request deliberately has no root/dependency or
/// Docker selectors: the current TOML schema has no dependency relation, so
/// the daemon reads one validated snapshot and applies all stack names in
/// their canonical lexical order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestConvergeJobRequest {
    pub workspace: PathBuf,
    pub manifest: String,
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

/// Narrow test seam for the two fixed guest transports. The daemon supplies
/// fully-derived commands; this trait deliberately has no raw host, port,
/// argv, credential, or shell parameter.
pub(crate) trait GuestSshTaskTransport: Send + Sync {
    fn stream<'a>(
        &'a self,
        command: GuestSshCommand,
        options: RunOptions,
        cancellation: &'a async_engine::CancellationToken,
        events: &'a async_engine::Sender<EngineEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>>;
    fn stream_scp<'a>(
        &'a self,
        command: GuestScpCommand,
        options: RunOptions,
        cancellation: &'a async_engine::CancellationToken,
        events: &'a async_engine::Sender<EngineEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>>;
}

#[derive(Clone, Default)]
pub(crate) struct NativeGuestSshTaskTransport;
impl GuestSshTaskTransport for NativeGuestSshTaskTransport {
    fn stream<'a>(
        &'a self,
        command: GuestSshCommand,
        options: RunOptions,
        cancellation: &'a async_engine::CancellationToken,
        events: &'a async_engine::Sender<EngineEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            GuestSshEngine::system()
                .stream(&command, options, Some(cancellation), events)
                .await
        })
    }
    fn stream_scp<'a>(
        &'a self,
        command: GuestScpCommand,
        options: RunOptions,
        cancellation: &'a async_engine::CancellationToken,
        events: &'a async_engine::Sender<EngineEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            GuestScpEngine::system()
                .stream(&command, options, Some(cancellation), events)
                .await
        })
    }
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
    pub(crate) engine: DockerEngine,
}
impl DockerDoctorExecutor {
    pub(crate) fn new() -> Self {
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

/// Immutable restart authorization persisted only after a native manifest
/// ensure has both proved the engine object and committed its registry facts.
/// It is audit-log encoded to preserve compatibility with existing v5 state;
/// malformed/old entries are never recovery authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestRecoveryContract {
    pub(crate) resource_id: String,
    pub(crate) name: String,
    pub(crate) workspace: String,
    pub(crate) stack: String,
    pub(crate) generation: String,
    pub(crate) manifest: String,
    pub(crate) image_identity: String,
    pub(crate) guest: bool,
    /// Immutable per-success identity. A later successful ensure gets a new
    /// intent even if its generation is unchanged, so a prior policy-off or
    /// drift disable cannot accidentally suppress the renewed declaration.
    pub(crate) intent_id: String,
    /// Derived from the manifest's existing default-stack semantics by the
    /// executor that performed this successful ensure.
    pub(crate) autostart: bool,
}

/// Fixed engine seam for native manifest restart recovery. It exposes only
/// inspect and start of a deterministic registry-owned name; no caller can
/// inject Docker arguments, labels, images, or source paths.
pub trait ManifestRecoveryExecutor: Send + Sync {
    fn inspect<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<SetupReconcileObserved>, String>> + Send + 'a>>;
    fn start<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct DockerManifestRecoveryExecutor {
    pub(crate) engine: DockerEngine,
}
impl DockerManifestRecoveryExecutor {
    pub(crate) fn new() -> Self {
        Self {
            engine: DockerEngine::docker(),
        }
    }
}
impl ManifestRecoveryExecutor for DockerManifestRecoveryExecutor {
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
                .capture_async(RunOptions::bounded(
                    MANIFEST_RECOVERY_ENGINE_DEADLINE,
                    4 * 1024,
                ))
                .await
                .map_err(|_| "manifest recovery inspect failed".to_owned())?;
            if result.exit_code == 1 {
                return Ok(None);
            }
            if !result.ok() {
                return Err("manifest recovery inspect failed".into());
            }
            let text = std::str::from_utf8(&result.stdout)
                .map_err(|_| "manifest recovery inspect failed".to_owned())?;
            let values: Vec<_> = text.trim_end_matches(['\r', '\n']).split('\t').collect();
            if values.len() != 6
                || !matches!(values[1], "true" | "false")
                || values.iter().any(|value| value.len() > 1024)
            {
                return Err("manifest recovery inspect failed".into());
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
    fn start<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let result = self
                .engine
                .with_args(["container", "start", name])
                .capture_async(RunOptions::bounded(
                    MANIFEST_RECOVERY_ENGINE_DEADLINE,
                    4 * 1024,
                ))
                .await
                .map_err(|_| "manifest recovery start failed".to_owned())?;
            result
                .ok()
                .then_some(())
                .ok_or_else(|| "manifest recovery start failed".into())
        })
    }
}
#[derive(Clone)]
pub struct DockerSetupReconcileExecutor {
    pub(crate) engine: DockerEngine,
}
impl DockerSetupReconcileExecutor {
    pub(crate) fn new() -> Self {
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
    /// Native-manifest execution only: whether the exact successfully
    /// materialized stack was the manifest's selected default stack. This is
    /// an executor fact rather than an RPC option, so a client cannot turn a
    /// managed object into a daemon-start candidate.
    pub manifest_autostart: bool,
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
