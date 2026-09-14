//! Ownership-safe creation and start of one setup application's container.
//!
//! This is a narrow core primitive, not setup apply or registry reconciliation.
//! It can inspect one deterministic Bosn-managed name, create it only when it
//! is absent, and start it only when stopped.  It never removes, replaces,
//! stops, adopts, garbage-collects, or otherwise takes over a container.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use bosn_core::{MAX_ENVIRONMENT_ENTRIES, SETUP_DOCUMENT_VERSION};
use bosn_engine::{CommandError, CommandResult, DockerEngine, EngineEvent, RunOptions};
use kernal_api::{
    async_engine::{CancellationToken, Deadline, Sender},
    platform::fs,
};

use crate::{PreparedImage, PreparedImageKind, SetupPlan, SetupPlanAppSource};

const LABEL_MANAGED: &str = "com.zackees.bosn.setup-managed";
const LABEL_CONTENT_SHA256: &str = "com.zackees.bosn.setup-content-sha256";
const LABEL_CONTAINER_NAME: &str = "com.zackees.bosn.setup-container";
const MANAGED_VALUE: &str = "v1";

/// A document-derived workspace bind mount for a persistent setup app.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureMount {
    pub source: PathBuf,
    pub target: String,
    pub readonly: bool,
}

/// A validated named Docker volume attachment.  This is a finite typed value,
/// never a caller-provided `--mount` string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureVolume {
    pub name: String,
    pub target: String,
    pub labels: BTreeMap<String, String>,
}

/// A finite semantic engine command used by [`ensure_setup_app`].
///
/// The operation deliberately has no generic Docker argument, network,
/// privilege, container-name, label, or raw-command input.  All mutable fields
/// are derived from the validated plan and matching image receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupEnsureCommand {
    VolumeInspect {
        volume_name: String,
    },
    VolumeCreate {
        volume: SetupEnsureVolume,
    },
    Inspect {
        container_name: String,
    },
    Create {
        container_name: String,
        image_identity: String,
        mounts: Vec<SetupEnsureMount>,
        volumes: Vec<SetupEnsureVolume>,
        environment: BTreeMap<String, String>,
        workdir: Option<String>,
        command: Option<String>,
        labels: BTreeMap<String, String>,
    },
    Start {
        container_name: String,
    },
}

impl SetupEnsureCommand {
    fn docker_args(&self) -> Vec<String> {
        match self {
            Self::VolumeInspect { volume_name } => vec![
                "volume".into(),
                "inspect".into(),
                "--format".into(),
                format!(
                    "{{{{index .Labels \"{LABEL_MANAGED}\"}}}}\t{{{{index .Labels \"{LABEL_CONTENT_SHA256}\"}}}}\t{{{{index .Labels \"{LABEL_CONTAINER_NAME}\"}}}}"
                ),
                volume_name.clone(),
            ],
            Self::VolumeCreate { volume } => {
                let mut args = vec!["volume".into(), "create".into()];
                for (key, value) in &volume.labels {
                    args.push("--label".into());
                    args.push(format!("{key}={value}"));
                }
                args.push(volume.name.clone());
                args
            }
            Self::Inspect { container_name } => vec![
                "container".into(),
                "inspect".into(),
                "--format".into(),
                format!(
                    "{{{{.Id}}}}\t{{{{.State.Running}}}}\t{{{{.Image}}}}\t{{{{index .Config.Labels \"{LABEL_MANAGED}\"}}}}\t{{{{index .Config.Labels \"{LABEL_CONTENT_SHA256}\"}}}}\t{{{{index .Config.Labels \"{LABEL_CONTAINER_NAME}\"}}}}"
                ),
                container_name.clone(),
            ],
            Self::Create {
                container_name,
                image_identity,
                mounts,
                volumes,
                environment,
                workdir,
                command,
                labels,
            } => {
                let mut args = vec![
                    "container".into(),
                    "create".into(),
                    "--name".into(),
                    container_name.clone(),
                ];
                for (key, value) in labels {
                    args.push("--label".into());
                    args.push(format!("{key}={value}"));
                }
                for mount in mounts {
                    let source = mount
                        .source
                        .to_str()
                        .expect("validated mount source is UTF-8");
                    let mut value = format!("type=bind,src={source},dst={}", mount.target);
                    if mount.readonly {
                        value.push_str(",readonly");
                    }
                    args.push("--mount".into());
                    args.push(value);
                }
                for volume in volumes {
                    args.push("--mount".into());
                    args.push(format!(
                        "type=volume,src={},dst={}",
                        volume.name, volume.target
                    ));
                }
                for (key, value) in environment {
                    args.push("--env".into());
                    args.push(format!("{key}={value}"));
                }
                if let Some(workdir) = workdir {
                    args.push("--workdir".into());
                    args.push(workdir.clone());
                }
                args.push(image_identity.clone());
                if let Some(command) = command {
                    args.extend(["sh".into(), "-lc".into(), command.clone()]);
                }
                args
            }
            Self::Start { container_name } => {
                vec!["container".into(), "start".into(), container_name.clone()]
            }
        }
    }
}

/// A successful inspection of a managed candidate container.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureObservedContainer {
    pub container_id: String,
    pub running: bool,
    pub image_identity: String,
    pub labels: BTreeMap<String, String>,
}

/// Typed response corresponding to one [`SetupEnsureCommand`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupEnsureResponse {
    Inspection(Option<SetupEnsureObservedContainer>, CommandResult),
    Command(CommandResult),
}

/// Testable semantic engine boundary for setup-container ensure.
///
/// Test doubles receive finite operations and never construct Docker argv.
pub trait SetupEnsureEngine {
    type StreamFuture<'a>: Future<Output = Result<SetupEnsureResponse, CommandError>> + Send + 'a
    where
        Self: 'a;

    fn stream<'a>(
        &'a self,
        command: SetupEnsureCommand,
        options: RunOptions,
        cancellation: &'a CancellationToken,
        events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a>;
}

impl SetupEnsureEngine for DockerEngine {
    type StreamFuture<'a> =
        Pin<Box<dyn Future<Output = Result<SetupEnsureResponse, CommandError>> + Send + 'a>>;

    fn stream<'a>(
        &'a self,
        command: SetupEnsureCommand,
        options: RunOptions,
        cancellation: &'a CancellationToken,
        events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a> {
        let engine = self.with_args(command.docker_args());
        Box::pin(async move {
            let result = engine.stream(options, Some(cancellation), events).await?;
            if !matches!(command, SetupEnsureCommand::Inspect { .. }) {
                return Ok(SetupEnsureResponse::Command(result));
            }
            if !result.ok() {
                if is_absent_container(&result) {
                    return Ok(SetupEnsureResponse::Inspection(None, result));
                }
                return Ok(SetupEnsureResponse::Command(result));
            }
            let observed = parse_inspection(&result.stdout)?;
            Ok(SetupEnsureResponse::Inspection(Some(observed), result))
        })
    }
}

/// All validated inputs for one ownership-safe setup app ensure.
#[derive(Debug)]
pub struct SetupEnsureRequest<'a> {
    pub plan: &'a SetupPlan,
    pub workspace_root: PathBuf,
    pub prepared_image: &'a PreparedImage,
    pub options: RunOptions,
    pub cancellation: &'a CancellationToken,
    pub events: &'a Sender<EngineEvent>,
}

/// Receipt for a successful no-delete/no-replace ensure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureResult {
    pub container_name: String,
    pub container_id: String,
    pub image_identity: String,
    pub created: bool,
    pub started: bool,
    /// State observed after the ownership-safe operation. `adopt_setup_app`
    /// preserves Docker state, while `ensure_setup_app` always returns a
    /// running app on success.
    pub running: bool,
}

/// Why a setup app ensure was refused or failed.
#[derive(Debug)]
pub enum SetupEnsureError {
    InvalidRequest(&'static str),
    OwnershipMismatch,
    Cancelled,
    Deadline,
    Transport(CommandError),
    ActionFailed {
        action: &'static str,
        detail: String,
    },
    EngineProtocol(&'static str),
}

impl std::fmt::Display for SetupEnsureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(reason) => {
                write!(formatter, "invalid setup ensure request: {reason}")
            }
            Self::OwnershipMismatch => {
                formatter.write_str("existing container is not the expected Bosn-managed setup app")
            }
            Self::Cancelled => formatter.write_str("setup app ensure was cancelled"),
            Self::Deadline => formatter.write_str("setup app ensure exceeded its deadline"),
            Self::Transport(error) => {
                write!(formatter, "setup app ensure engine transport: {error}")
            }
            Self::ActionFailed { action, detail } => {
                write!(formatter, "Docker {action} failed: {detail}")
            }
            Self::EngineProtocol(reason) => {
                write!(formatter, "setup app ensure engine protocol: {reason}")
            }
        }
    }
}

impl std::error::Error for SetupEnsureError {}

impl From<CommandError> for SetupEnsureError {
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::Cancelled { .. } => Self::Cancelled,
            CommandError::Deadline { .. } => Self::Deadline,
            other => Self::Transport(other),
        }
    }
}

/// Ensure the one deterministic, content-addressed container for `plan`.
///
/// An existing candidate is reused only if its observed image and every
/// ownership label exactly match this plan.  This primitive intentionally
/// refuses uncertainty; it does not adopt, delete, replace, stop, or GC any
/// existing Docker container.  Registry persistence and reconciliation are
/// separate future layers.
pub async fn ensure_setup_app<E: SetupEnsureEngine>(
    engine: &E,
    request: SetupEnsureRequest<'_>,
) -> Result<SetupEnsureResult, SetupEnsureError> {
    let derived = derive_command(&request)?;
    if request.cancellation.is_cancelled() {
        return Err(SetupEnsureError::Cancelled);
    }
    if request.options.deadline.is_zero() {
        return Err(SetupEnsureError::Deadline);
    }
    if request.options.output_limit == 0 {
        return Err(SetupEnsureError::InvalidRequest("output budget is zero"));
    }

    let deadline = Deadline::after(request.options.deadline);
    let mut remaining_output = request.options.output_limit;
    let inspection = invoke(
        engine,
        SetupEnsureCommand::Inspect {
            container_name: derived.container_name.clone(),
        },
        &deadline,
        &mut remaining_output,
        &request,
    )
    .await?;
    let observed = match inspection {
        SetupEnsureResponse::Inspection(observed, result) => {
            consume_output(&result, &mut remaining_output, request.options.output_limit)?;
            if observed.is_some() && !result.ok() {
                return Err(SetupEnsureError::EngineProtocol(
                    "present inspection has a nonzero result",
                ));
            }
            if observed.is_none() && result.ok() {
                return Err(SetupEnsureError::EngineProtocol(
                    "absent inspection has a successful result",
                ));
            }
            if result.exit_code != 0 && result.exit_code != 1 {
                return Err(action_failed("container inspect", &result));
            }
            observed
        }
        SetupEnsureResponse::Command(result) => {
            consume_output(&result, &mut remaining_output, request.options.output_limit)?;
            return Err(action_failed("container inspect", &result));
        }
    };

    if let Some(observed) = observed {
        validate_observed(&observed, &derived)?;
        if observed.running {
            return Ok(SetupEnsureResult {
                container_name: derived.container_name,
                container_id: observed.container_id,
                image_identity: derived.image_identity,
                created: false,
                started: false,
                running: true,
            });
        }
        let response = invoke(
            engine,
            SetupEnsureCommand::Start {
                container_name: derived.container_name.clone(),
            },
            &deadline,
            &mut remaining_output,
            &request,
        )
        .await?;
        require_action_success(
            "container start",
            response,
            &mut remaining_output,
            request.options.output_limit,
        )?;
        return Ok(SetupEnsureResult {
            container_name: derived.container_name,
            container_id: observed.container_id,
            image_identity: derived.image_identity,
            created: false,
            started: true,
            running: true,
        });
    }

    // The daemon wrote durable volume intents before this primitive was
    // invoked. A matching existing container already proves that its mounts
    // were created from this immutable plan, so do not mutate or re-inspect
    // volumes on its reuse path. For a new container, create/reuse every exact
    // labelled volume before the container itself, leaving only recoverable
    // intent-backed volume state if an attempt is interrupted.
    for volume in &derived.volumes {
        let response = invoke(
            engine,
            SetupEnsureCommand::VolumeInspect {
                volume_name: volume.name.clone(),
            },
            &deadline,
            &mut remaining_output,
            &request,
        )
        .await?;
        let SetupEnsureResponse::Command(result) = response else {
            return Err(SetupEnsureError::EngineProtocol(
                "volume inspection returned container response",
            ));
        };
        consume_output(&result, &mut remaining_output, request.options.output_limit)?;
        if result.ok() {
            let observed = std::str::from_utf8(&result.stdout).unwrap_or("").trim();
            let expected = [
                volume.labels.get(LABEL_MANAGED).map_or("", String::as_str),
                volume
                    .labels
                    .get(LABEL_CONTENT_SHA256)
                    .map_or("", String::as_str),
                volume
                    .labels
                    .get(LABEL_CONTAINER_NAME)
                    .map_or("", String::as_str),
            ]
            .join("\t");
            if observed != expected {
                return Err(SetupEnsureError::OwnershipMismatch);
            }
        } else if result.exit_code == 1 {
            let response = invoke(
                engine,
                SetupEnsureCommand::VolumeCreate {
                    volume: volume.clone(),
                },
                &deadline,
                &mut remaining_output,
                &request,
            )
            .await?;
            require_action_success(
                "volume create",
                response,
                &mut remaining_output,
                request.options.output_limit,
            )?;
        } else {
            return Err(action_failed("volume inspect", &result));
        }
    }

    let response = invoke(
        engine,
        derived.create_command(),
        &deadline,
        &mut remaining_output,
        &request,
    )
    .await?;
    let created = require_action_success(
        "container create",
        response,
        &mut remaining_output,
        request.options.output_limit,
    )?;
    let container_id = parse_created_id(&created.stdout)?;
    let response = invoke(
        engine,
        SetupEnsureCommand::Start {
            container_name: derived.container_name.clone(),
        },
        &deadline,
        &mut remaining_output,
        &request,
    )
    .await?;
    require_action_success(
        "container start",
        response,
        &mut remaining_output,
        request.options.output_limit,
    )?;
    Ok(SetupEnsureResult {
        container_name: derived.container_name,
        container_id,
        image_identity: derived.image_identity,
        created: true,
        started: true,
        running: true,
    })
}

/// Inspect and prove ownership of the deterministic setup app without changing
/// Docker. This is the sole primitive used to restore a lost local registry:
/// it derives the candidate name, labels, and image identity from the validated
/// plan and prepared image, then refuses any uncertainty. In particular it
/// never creates, starts, stops, removes, pulls, or replaces a container.
pub async fn adopt_setup_app<E: SetupEnsureEngine>(
    engine: &E,
    request: SetupEnsureRequest<'_>,
) -> Result<SetupEnsureResult, SetupEnsureError> {
    let derived = derive_command(&request)?;
    if request.cancellation.is_cancelled() {
        return Err(SetupEnsureError::Cancelled);
    }
    if request.options.deadline.is_zero() {
        return Err(SetupEnsureError::Deadline);
    }
    if request.options.output_limit == 0 {
        return Err(SetupEnsureError::InvalidRequest("output budget is zero"));
    }
    let deadline = Deadline::after(request.options.deadline);
    let mut remaining_output = request.options.output_limit;
    let inspection = invoke(
        engine,
        SetupEnsureCommand::Inspect {
            container_name: derived.container_name.clone(),
        },
        &deadline,
        &mut remaining_output,
        &request,
    )
    .await?;
    let SetupEnsureResponse::Inspection(observed, result) = inspection else {
        return Err(SetupEnsureError::EngineProtocol(
            "inspection returned a mutation response",
        ));
    };
    consume_output(&result, &mut remaining_output, request.options.output_limit)?;
    let observed = match observed {
        Some(observed) if result.ok() => observed,
        Some(_) => {
            return Err(SetupEnsureError::EngineProtocol(
                "present inspection has a nonzero result",
            ));
        }
        None if result.exit_code == 1 => return Err(SetupEnsureError::OwnershipMismatch),
        None if result.ok() => {
            return Err(SetupEnsureError::EngineProtocol(
                "absent inspection has a successful result",
            ));
        }
        None => return Err(action_failed("container inspect", &result)),
    };
    validate_observed(&observed, &derived)?;
    Ok(SetupEnsureResult {
        container_name: derived.container_name,
        container_id: observed.container_id,
        image_identity: derived.image_identity,
        created: false,
        started: false,
        running: observed.running,
    })
}

async fn invoke<E: SetupEnsureEngine>(
    engine: &E,
    command: SetupEnsureCommand,
    deadline: &Deadline,
    remaining_output: &mut usize,
    request: &SetupEnsureRequest<'_>,
) -> Result<SetupEnsureResponse, SetupEnsureError> {
    if request.cancellation.is_cancelled() {
        return Err(SetupEnsureError::Cancelled);
    }
    if *remaining_output == 0 {
        return Err(SetupEnsureError::Transport(CommandError::OutputLimit {
            limit: request.options.output_limit,
            reaped_pid: None,
            cleanup: None,
        }));
    }
    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err(SetupEnsureError::Deadline);
    }
    Ok(engine
        .stream(
            command,
            RunOptions::streaming(remaining, *remaining_output),
            request.cancellation,
            request.events,
        )
        .await?)
}

fn require_action_success(
    action: &'static str,
    response: SetupEnsureResponse,
    remaining_output: &mut usize,
    limit: usize,
) -> Result<CommandResult, SetupEnsureError> {
    let SetupEnsureResponse::Command(result) = response else {
        return Err(SetupEnsureError::EngineProtocol(
            "mutation returned an inspection response",
        ));
    };
    consume_output(&result, remaining_output, limit)?;
    if result.ok() {
        Ok(result)
    } else {
        Err(action_failed(action, &result))
    }
}

fn consume_output(
    result: &CommandResult,
    remaining: &mut usize,
    limit: usize,
) -> Result<(), SetupEnsureError> {
    let used = result.stdout.len().saturating_add(result.stderr.len());
    if used > *remaining {
        return Err(SetupEnsureError::Transport(CommandError::OutputLimit {
            limit,
            reaped_pid: None,
            cleanup: None,
        }));
    }
    *remaining -= used;
    Ok(())
}

fn action_failed(action: &'static str, result: &CommandResult) -> SetupEnsureError {
    SetupEnsureError::ActionFailed {
        action,
        detail: failure_detail(result),
    }
}

#[derive(Clone, Debug)]
struct DerivedEnsure {
    container_name: String,
    image_identity: String,
    mounts: Vec<SetupEnsureMount>,
    environment: BTreeMap<String, String>,
    workdir: Option<String>,
    command: Option<String>,
    labels: BTreeMap<String, String>,
    volumes: Vec<SetupEnsureVolume>,
}

impl DerivedEnsure {
    fn create_command(&self) -> SetupEnsureCommand {
        SetupEnsureCommand::Create {
            container_name: self.container_name.clone(),
            image_identity: self.image_identity.clone(),
            mounts: self.mounts.clone(),
            volumes: self.volumes.clone(),
            environment: self.environment.clone(),
            workdir: self.workdir.clone(),
            command: self.command.clone(),
            labels: self.labels.clone(),
        }
    }
}

fn derive_command(request: &SetupEnsureRequest<'_>) -> Result<DerivedEnsure, SetupEnsureError> {
    validate_plan_shape(request.plan)?;
    let workspace_root = canonical_workspace(&request.workspace_root)?;
    if workspace_root != request.plan.workspace_root {
        return Err(SetupEnsureError::InvalidRequest(
            "workspace is not the plan's canonical workspace root",
        ));
    }
    validate_prepared_image(request.plan, request.prepared_image)?;
    let mounts = derive_mounts(&workspace_root, request.plan)?;
    let workdir = request
        .plan
        .app
        .workdir
        .as_deref()
        .map(|value| resolve_workdir(value, &request.plan.app.mounts, &workspace_root))
        .transpose()?;
    let command = request.plan.app.command.clone();
    if command
        .as_deref()
        .is_some_and(|value| value.is_empty() || value.len() > 16 * 1024 || value.contains('\0'))
    {
        return Err(SetupEnsureError::InvalidRequest(
            "declared app command is invalid",
        ));
    }
    let container_name = format!("bosn-setup-{}", request.plan.content_sha256);
    let labels = BTreeMap::from([
        (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
        (
            LABEL_CONTENT_SHA256.into(),
            request.plan.content_sha256.clone(),
        ),
        (LABEL_CONTAINER_NAME.into(), container_name.clone()),
    ]);
    let volumes = derive_volumes(request.plan)?;
    Ok(DerivedEnsure {
        container_name,
        image_identity: request.prepared_image.observed_identity.clone(),
        mounts,
        environment: validated_environment(&request.plan.app.environment)?,
        workdir,
        command,
        labels,
        volumes,
    })
}

fn validate_plan_shape(plan: &SetupPlan) -> Result<(), SetupEnsureError> {
    if plan.schema_version != SETUP_DOCUMENT_VERSION || !valid_hash(&plan.content_sha256) {
        return Err(SetupEnsureError::InvalidRequest("plan receipt is invalid"));
    }
    let names: Vec<_> = plan.tasks.keys().cloned().collect();
    if plan.task_names != names {
        return Err(SetupEnsureError::InvalidRequest(
            "plan task receipt was modified",
        ));
    }
    match (&plan.app.source, &plan.app_source) {
        (
            bosn_core::SetupSource::PinnedImage(document_image),
            SetupPlanAppSource::PinnedImage { image },
        ) if document_image == image && valid_pinned_image(image) && plan.asset_root.is_none() => {}
        (
            bosn_core::SetupSource::InlineDockerfile(_),
            SetupPlanAppSource::InlineDockerfile { dockerfile_path },
        ) if plan
            .asset_root
            .as_ref()
            .is_some_and(|root| dockerfile_path == &root.join("Dockerfile")) => {}
        _ => {
            return Err(SetupEnsureError::InvalidRequest(
                "plan application receipt was modified",
            ));
        }
    }
    validate_workspace_relative(plan.app.workdir.as_deref())?;
    for mount in &plan.app.mounts {
        validate_workspace_relative(Some(&mount.source))?;
        validate_container_path(&mount.target)?;
    }
    let mut targets = BTreeSet::new();
    for volume in &plan.named_volumes {
        if !valid_volume_name(&volume.name)
            || validate_container_path(&volume.target).is_err()
            || !targets.insert(volume.target.clone())
            || volume.labels.get(LABEL_MANAGED) != Some(&MANAGED_VALUE.into())
            || !valid_hash(
                volume
                    .labels
                    .get(LABEL_CONTENT_SHA256)
                    .map_or("", String::as_str),
            )
            || volume.labels.get(LABEL_CONTAINER_NAME) != Some(&volume.name)
        {
            return Err(SetupEnsureError::InvalidRequest(
                "named volume receipt was modified",
            ));
        }
    }
    Ok(())
}

fn derive_volumes(plan: &SetupPlan) -> Result<Vec<SetupEnsureVolume>, SetupEnsureError> {
    let bind_targets: BTreeSet<_> = plan.app.mounts.iter().map(|mount| &mount.target).collect();
    plan.named_volumes
        .iter()
        .map(|volume| {
            if bind_targets.contains(&volume.target) || !valid_volume_name(&volume.name) {
                return Err(SetupEnsureError::InvalidRequest(
                    "duplicate or unsafe named volume target",
                ));
            }
            Ok(SetupEnsureVolume {
                name: volume.name.clone(),
                target: volume.target.clone(),
                labels: volume.labels.clone(),
            })
        })
        .collect()
}

fn valid_volume_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn validate_prepared_image(
    plan: &SetupPlan,
    image: &PreparedImage,
) -> Result<(), SetupEnsureError> {
    if image.setup_content_sha256 != plan.content_sha256
        || !valid_identity(&image.observed_identity)
    {
        return Err(SetupEnsureError::InvalidRequest(
            "prepared image receipt does not match plan",
        ));
    }
    match (&plan.app_source, &image.kind) {
        (
            SetupPlanAppSource::PinnedImage { image: expected },
            PreparedImageKind::PinnedImage { image: prepared },
        ) if expected == prepared && image.reference == *expected => Ok(()),
        (
            SetupPlanAppSource::InlineDockerfile { .. },
            PreparedImageKind::InlineDockerfile { tag },
        ) if tag == &format!("bosn-setup:{}", plan.content_sha256) && image.reference == *tag => {
            Ok(())
        }
        _ => Err(SetupEnsureError::InvalidRequest(
            "prepared image is not for the planned application",
        )),
    }
}

fn derive_mounts(
    workspace_root: &Path,
    plan: &SetupPlan,
) -> Result<Vec<SetupEnsureMount>, SetupEnsureError> {
    let mut targets = BTreeSet::new();
    plan.app
        .mounts
        .iter()
        .map(|mount| {
            if !targets.insert(mount.target.clone()) {
                return Err(SetupEnsureError::InvalidRequest(
                    "duplicate container mount target",
                ));
            }
            let source = canonical_workspace_member(workspace_root, &mount.source)?;
            let source = source.to_str().ok_or(SetupEnsureError::InvalidRequest(
                "mount source is not UTF-8",
            ))?;
            if source.contains(',') || mount.target.contains(',') {
                return Err(SetupEnsureError::InvalidRequest(
                    "mount path cannot be represented safely by Docker",
                ));
            }
            Ok(SetupEnsureMount {
                source: PathBuf::from(source),
                target: mount.target.clone(),
                readonly: mount.readonly,
            })
        })
        .collect()
}

fn resolve_workdir(
    workdir: &str,
    mounts: &[bosn_core::WorkspaceMount],
    workspace_root: &Path,
) -> Result<String, SetupEnsureError> {
    validate_workspace_relative(Some(workdir))?;
    let selected = mounts
        .iter()
        .filter(|mount| workspace_prefix(workdir, &mount.source))
        .max_by_key(|mount| mount.source.len())
        .ok_or(SetupEnsureError::InvalidRequest(
            "declared workdir is not covered by a declared workspace mount",
        ))?;
    let suffix = relative_suffix(workdir, &selected.source).expect("selected mount covers workdir");
    // Recheck the selected source at apply time. A manifest/setup document can
    // be planned while a source is a directory and changed before Docker is
    // invoked; a file bind cannot meaningfully back a container workdir.
    let source = canonical_workspace_member(workspace_root, &selected.source)?;
    let metadata = fs::context_path_metadata_no_follow(&source).map_err(|_| {
        SetupEnsureError::InvalidRequest("declared workdir mount source does not exist")
    })?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err(SetupEnsureError::InvalidRequest(
            "declared workdir must be backed by a directory mount",
        ));
    }
    let resolved = if suffix.is_empty() {
        selected.target.clone()
    } else if selected.target == "/" {
        format!("/{suffix}")
    } else {
        format!("{}/{suffix}", selected.target)
    };
    validate_container_path(&resolved)?;
    Ok(resolved)
}

fn validated_environment(
    input: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, SetupEnsureError> {
    if input.len() > MAX_ENVIRONMENT_ENTRIES
        || input.iter().any(|(key, value)| {
            !valid_environment_name(key) || value.len() > 16 * 1024 || value.contains('\0')
        })
    {
        return Err(SetupEnsureError::InvalidRequest(
            "app environment is not safe document data",
        ));
    }
    Ok(input.clone())
}

fn validate_observed(
    observed: &SetupEnsureObservedContainer,
    expected: &DerivedEnsure,
) -> Result<(), SetupEnsureError> {
    if !valid_container_id(&observed.container_id)
        || observed.image_identity != expected.image_identity
        || observed.labels != expected.labels
    {
        return Err(SetupEnsureError::OwnershipMismatch);
    }
    Ok(())
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, SetupEnsureError> {
    if !path.is_absolute() {
        return Err(SetupEnsureError::InvalidRequest(
            "workspace is not absolute",
        ));
    }
    let metadata = fs::context_path_metadata_no_follow(path)
        .map_err(|_| SetupEnsureError::InvalidRequest("workspace is not a directory"))?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err(SetupEnsureError::InvalidRequest(
            "workspace is not a directory",
        ));
    }
    let canonical = fs::canonical_context_path(path)
        .map_err(|_| SetupEnsureError::InvalidRequest("workspace cannot be canonicalized"))?;
    let metadata = fs::context_path_metadata_no_follow(&canonical)
        .map_err(|_| SetupEnsureError::InvalidRequest("workspace is not a directory"))?;
    (metadata.kind == fs::ContextPathKind::Directory)
        .then_some(canonical)
        .ok_or(SetupEnsureError::InvalidRequest(
            "workspace is not a directory",
        ))
}

fn canonical_workspace_member(root: &Path, relative: &str) -> Result<PathBuf, SetupEnsureError> {
    validate_workspace_relative(Some(relative))?;
    let candidate = root.join(relative);
    let metadata = fs::context_path_metadata_no_follow(&candidate)
        .map_err(|_| SetupEnsureError::InvalidRequest("declared mount source does not exist"))?;
    if metadata.kind == fs::ContextPathKind::Symlink {
        return Err(SetupEnsureError::InvalidRequest(
            "declared mount source is a symlink",
        ));
    }
    let canonical = fs::canonical_context_path(&candidate).map_err(|_| {
        SetupEnsureError::InvalidRequest("declared mount source cannot be resolved")
    })?;
    if !canonical.starts_with(root) {
        return Err(SetupEnsureError::InvalidRequest(
            "declared mount source escapes workspace",
        ));
    }
    Ok(canonical)
}

fn parse_inspection(stdout: &[u8]) -> Result<SetupEnsureObservedContainer, CommandError> {
    let line = std::str::from_utf8(stdout)
        .map_err(|_| protocol_error("container inspect output is not UTF-8"))?
        .trim_end_matches(['\r', '\n']);
    let mut fields = line.split('\t');
    let container_id = fields.next().unwrap_or_default();
    let running = match fields.next() {
        Some("true") => true,
        Some("false") => false,
        _ => return Err(protocol_error("container inspect running state is invalid")),
    };
    let image_identity = fields.next().unwrap_or_default();
    let managed = fields.next().unwrap_or_default();
    let content_sha256 = fields.next().unwrap_or_default();
    let container_name = fields.next().unwrap_or_default();
    if fields.next().is_some()
        || !valid_container_id(container_id)
        || !valid_identity(image_identity)
        || managed.contains(['\t', '\n', '\r'])
        || content_sha256.contains(['\t', '\n', '\r'])
        || container_name.contains(['\t', '\n', '\r'])
    {
        return Err(protocol_error("container inspect output is invalid"));
    }
    Ok(SetupEnsureObservedContainer {
        container_id: container_id.into(),
        running,
        image_identity: image_identity.into(),
        labels: BTreeMap::from([
            (LABEL_MANAGED.into(), managed.into()),
            (LABEL_CONTENT_SHA256.into(), content_sha256.into()),
            (LABEL_CONTAINER_NAME.into(), container_name.into()),
        ]),
    })
}

fn parse_created_id(stdout: &[u8]) -> Result<String, SetupEnsureError> {
    let value = std::str::from_utf8(stdout)
        .map_err(|_| SetupEnsureError::EngineProtocol("container create output is not UTF-8"))?
        .trim();
    valid_container_id(value)
        .then_some(value.into())
        .ok_or(SetupEnsureError::EngineProtocol(
            "container create did not return an ID",
        ))
}

fn is_absent_container(result: &CommandResult) -> bool {
    result.exit_code == 1
        && String::from_utf8_lossy(&result.stderr)
            .to_ascii_lowercase()
            .contains("no such container")
}

fn protocol_error(detail: &'static str) -> CommandError {
    CommandError::OutputCompletion {
        detail: detail.into(),
        reaped_pid: None,
        cleanup: None,
    }
}

fn validate_workspace_relative(value: Option<&str>) -> Result<(), SetupEnsureError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty()
        || value.len() > 4096
        || value.contains('\0')
        || value.contains('\\')
        || value.starts_with('/')
        || is_windows_absolute(value)
        || value.split('/').any(|part| part.is_empty() || part == "..")
    {
        return Err(SetupEnsureError::InvalidRequest(
            "workspace-relative path is invalid",
        ));
    }
    Ok(())
}

fn validate_container_path(value: &str) -> Result<(), SetupEnsureError> {
    if value.is_empty()
        || value.len() > 4096
        || value.contains('\0')
        || value.contains('\\')
        || !value.starts_with('/')
        || (value != "/"
            && value[1..]
                .split('/')
                .any(|part| part.is_empty() || part == ".."))
    {
        return Err(SetupEnsureError::InvalidRequest(
            "container path is not normalized absolute",
        ));
    }
    Ok(())
}

fn workspace_prefix(path: &str, prefix: &str) -> bool {
    prefix == "."
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn relative_suffix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix == "." {
        Some(path.strip_prefix("./").unwrap_or(path))
    } else if path == prefix {
        Some("")
    } else {
        path.strip_prefix(prefix)?.strip_prefix('/')
    }
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_identity(value: &str) -> bool {
    value.len() == 71 && value.starts_with("sha256:") && valid_hash(&value[7..])
}

fn valid_container_id(value: &str) -> bool {
    value.len() == 64 && valid_hash(value)
}

fn valid_pinned_image(image: &str) -> bool {
    let Some((name, digest)) = image.rsplit_once("@sha256:") else {
        return false;
    };
    image.len() <= 512
        && !name.is_empty()
        && !name.starts_with('-')
        && !name.contains('@')
        && !image
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        && valid_hash(digest)
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'_' | b'a'..=b'z' | b'A'..=b'Z'))
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn failure_detail(result: &CommandResult) -> String {
    let detail = if result.stderr.is_empty() {
        &result.stdout
    } else {
        &result.stderr
    };
    String::from_utf8_lossy(detail)
        .trim()
        .chars()
        .take(4096)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        future::{Ready, ready},
        sync::Mutex,
        time::Duration,
    };

    use super::*;
    use kernal_api::async_engine::{CancellationSource, RuntimeBuilder, channel};

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const IDENTITY: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CONTAINER_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    #[derive(Default)]
    struct FakeEngine {
        calls: Mutex<Vec<SetupEnsureCommand>>,
        results: Mutex<VecDeque<Result<SetupEnsureResponse, CommandError>>>,
    }

    impl FakeEngine {
        fn with_results(
            results: impl IntoIterator<Item = Result<SetupEnsureResponse, CommandError>>,
        ) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                results: Mutex::new(results.into_iter().collect()),
            }
        }
    }

    impl SetupEnsureEngine for FakeEngine {
        type StreamFuture<'a> = Ready<Result<SetupEnsureResponse, CommandError>>;

        fn stream<'a>(
            &'a self,
            command: SetupEnsureCommand,
            _options: RunOptions,
            _cancellation: &'a CancellationToken,
            _events: &'a Sender<EngineEvent>,
        ) -> Self::StreamFuture<'a> {
            self.calls.lock().unwrap().push(command);
            ready(
                self.results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("configured ensure result"),
            )
        }
    }

    fn runtime() -> kernal_api::async_engine::Runtime {
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn result(
        exit_code: i32,
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
    ) -> CommandResult {
        CommandResult {
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn absent() -> Result<SetupEnsureResponse, CommandError> {
        Ok(SetupEnsureResponse::Inspection(
            None,
            result(1, [], b"Error response from daemon: No such container"),
        ))
    }

    fn observed(plan: &SetupPlan, running: bool) -> SetupEnsureObservedContainer {
        let name = format!("bosn-setup-{}", plan.content_sha256);
        SetupEnsureObservedContainer {
            container_id: CONTAINER_ID.into(),
            running,
            image_identity: IDENTITY.into(),
            labels: BTreeMap::from([
                (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
                (LABEL_CONTENT_SHA256.into(), plan.content_sha256.clone()),
                (LABEL_CONTAINER_NAME.into(), name),
            ]),
        }
    }

    fn inspection(plan: &SetupPlan, running: bool) -> Result<SetupEnsureResponse, CommandError> {
        Ok(SetupEnsureResponse::Inspection(
            Some(observed(plan, running)),
            result(0, [], []),
        ))
    }

    fn command(stdout: impl Into<Vec<u8>>) -> Result<SetupEnsureResponse, CommandError> {
        Ok(SetupEnsureResponse::Command(result(0, stdout, [])))
    }

    fn plan(workspace: &Path) -> SetupPlan {
        let image = format!("registry.example/team/app@sha256:{HASH}");
        SetupPlan {
            source_kind: crate::SetupSourceKind::LocalFile,
            content_sha256: HASH.into(),
            schema_version: 1,
            workspace_root: fs::canonical_context_path(workspace).unwrap(),
            asset_root: None,
            task_names: Vec::new(),
            app: bosn_core::SetupApp {
                source: bosn_core::SetupSource::PinnedImage(image.clone()),
                environment: BTreeMap::from([("APP_MODE".into(), "production".into())]),
                workdir: Some("src".into()),
                command: Some("./serve --port 8080".into()),
                mounts: vec![bosn_core::WorkspaceMount {
                    source: ".".into(),
                    target: "/workspace".into(),
                    readonly: false,
                }],
            },
            tasks: BTreeMap::new(),
            app_source: SetupPlanAppSource::PinnedImage { image },
            named_volumes: Vec::new(),
        }
    }

    fn prepared(plan: &SetupPlan) -> PreparedImage {
        let SetupPlanAppSource::PinnedImage { image } = &plan.app_source else {
            unreachable!()
        };
        PreparedImage {
            setup_content_sha256: plan.content_sha256.clone(),
            kind: PreparedImageKind::PinnedImage {
                image: image.clone(),
            },
            reference: image.clone(),
            observed_identity: IDENTITY.into(),
        }
    }

    fn run(
        engine: &FakeEngine,
        plan: &SetupPlan,
        workspace: &Path,
        image: &PreparedImage,
        cancellation: &CancellationToken,
        options: RunOptions,
    ) -> Result<SetupEnsureResult, SetupEnsureError> {
        let (events, _receiver) = channel(8);
        runtime().run(ensure_setup_app(
            engine,
            SetupEnsureRequest {
                plan,
                workspace_root: workspace.into(),
                prepared_image: image,
                options,
                cancellation,
                events: &events,
            },
        ))
    }
    fn run_adopt(
        engine: &FakeEngine,
        plan: &SetupPlan,
        workspace: &Path,
        image: &PreparedImage,
        cancellation: &CancellationToken,
        options: RunOptions,
    ) -> Result<SetupEnsureResult, SetupEnsureError> {
        let (events, _receiver) = channel(8);
        runtime().run(adopt_setup_app(
            engine,
            SetupEnsureRequest {
                plan,
                workspace_root: workspace.into(),
                prepared_image: image,
                options,
                cancellation,
                events: &events,
            },
        ))
    }

    #[test]
    fn adoption_proves_exact_running_or_stopped_candidate_without_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        let plan = plan(&workspace);
        let image = prepared(&plan);
        for running in [true, false] {
            let engine = FakeEngine::with_results([inspection(&plan, running)]);
            let source = CancellationSource::new();
            let result = run_adopt(
                &engine,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096),
            )
            .unwrap();
            assert!(!result.created && !result.started);
            assert!(matches!(
                engine.calls.lock().unwrap().as_slice(),
                [SetupEnsureCommand::Inspect { .. }]
            ));
        }
        let mut bad = observed(&plan, true);
        bad.labels.insert(LABEL_MANAGED.into(), "foreign".into());
        let engine = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
            Some(bad),
            result(0, [], []),
        ))]);
        let source = CancellationSource::new();
        assert!(matches!(
            run_adopt(
                &engine,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::OwnershipMismatch)
        ));
        assert!(matches!(
            engine.calls.lock().unwrap().as_slice(),
            [SetupEnsureCommand::Inspect { .. }]
        ));
        let cancelled = FakeEngine::default();
        let source = CancellationSource::new();
        source.cancel();
        assert!(matches!(
            run_adopt(
                &cancelled,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::Cancelled)
        ));
        assert!(cancelled.calls.lock().unwrap().is_empty());
        let deadline = FakeEngine::default();
        let source = CancellationSource::new();
        assert!(matches!(
            run_adopt(
                &deadline,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::ZERO, 4096)
            ),
            Err(SetupEnsureError::Deadline)
        ));
        assert!(deadline.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn inspect_format_uses_actual_tabs_that_the_response_parser_accepts() {
        let container_name = format!("bosn-setup-{HASH}");
        let command = SetupEnsureCommand::Inspect {
            container_name: container_name.clone(),
        };
        let args = command.docker_args();
        assert_eq!(args[0], "container");
        assert_eq!(args[1], "inspect");
        assert_eq!(args[2], "--format");
        assert!(args[3].contains('\t'));
        assert!(!args[3].contains("\\\\t"));
        assert_eq!(args[4], container_name);

        let observed = parse_inspection(
            format!(
                "{CONTAINER_ID}\ttrue\t{IDENTITY}\t{MANAGED_VALUE}\t{HASH}\tbosn-setup-{HASH}\n"
            )
            .as_bytes(),
        )
        .expect("inspect output using the generated delimiter contract parses");
        assert_eq!(observed.container_id, CONTAINER_ID);
        assert!(observed.running);
        assert_eq!(observed.image_identity, IDENTITY);
        assert_eq!(
            observed.labels.get(LABEL_MANAGED).map(String::as_str),
            Some(MANAGED_VALUE)
        );
        assert_eq!(
            observed
                .labels
                .get(LABEL_CONTENT_SHA256)
                .map(String::as_str),
            Some(HASH)
        );
        assert_eq!(
            observed
                .labels
                .get(LABEL_CONTAINER_NAME)
                .map(String::as_str),
            Some(container_name.as_str())
        );
    }

    #[test]
    fn volume_inspect_format_uses_actual_tabs_that_match_the_label_receipt() {
        let volume_name = format!("bosn-v-stack-{HASH}");
        let command = SetupEnsureCommand::VolumeInspect {
            volume_name: volume_name.clone(),
        };
        let args = command.docker_args();
        assert_eq!(args[0], "volume");
        assert_eq!(args[1], "inspect");
        assert_eq!(args[2], "--format");
        assert!(args[3].contains('\t'));
        assert!(!args[3].contains("\\\\t"));
        assert_eq!(args[4], volume_name);
    }

    #[test]
    fn absent_container_is_created_then_started_with_only_plan_data() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        let plan = plan(&workspace);
        let image = prepared(&plan);
        let engine =
            FakeEngine::with_results([absent(), command(format!("{CONTAINER_ID}\n")), command([])]);
        let cancellation = CancellationSource::new();
        let receipt = run(
            &engine,
            &plan,
            &workspace,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096),
        )
        .unwrap();

        assert_eq!(
            receipt,
            SetupEnsureResult {
                container_name: format!("bosn-setup-{HASH}"),
                container_id: CONTAINER_ID.into(),
                image_identity: IDENTITY.into(),
                created: true,
                started: true,
                running: true,
            }
        );
        let calls = engine.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3);
        assert!(
            matches!(&calls[0], SetupEnsureCommand::Inspect { container_name } if container_name == &format!("bosn-setup-{HASH}"))
        );
        assert_eq!(
            calls[2],
            SetupEnsureCommand::Start {
                container_name: format!("bosn-setup-{HASH}")
            }
        );
        let SetupEnsureCommand::Create {
            image_identity,
            mounts,
            environment,
            workdir,
            command,
            labels,
            ..
        } = &calls[1]
        else {
            panic!("expected create")
        };
        assert_eq!(image_identity, IDENTITY);
        assert_eq!(
            mounts,
            &vec![SetupEnsureMount {
                source: fs::canonical_context_path(&workspace).unwrap(),
                target: "/workspace".into(),
                readonly: false
            }]
        );
        assert_eq!(
            environment,
            &BTreeMap::from([("APP_MODE".into(), "production".into())])
        );
        assert_eq!(workdir.as_deref(), Some("/workspace/src"));
        assert_eq!(command.as_deref(), Some("./serve --port 8080"));
        assert_eq!(
            labels.get(LABEL_CONTENT_SHA256).map(String::as_str),
            Some(HASH)
        );
    }

    #[test]
    fn workdir_backed_by_a_file_is_refused_before_any_engine_command() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("not-a-directory"), "proof").unwrap();
        let mut plan = plan(&workspace);
        plan.app.workdir = Some("not-a-directory".into());
        plan.app.mounts[0].source = "not-a-directory".into();
        let image = prepared(&plan);
        let engine = FakeEngine::with_results([]);
        let cancellation = CancellationSource::new();
        assert!(
            run(
                &engine,
                &plan,
                &workspace,
                &image,
                &cancellation.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096),
            )
            .is_err()
        );
        assert!(engine.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn matching_stopped_container_is_only_started_and_running_one_is_reused() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        let plan = plan(&workspace);
        let image = prepared(&plan);
        let cancellation = CancellationSource::new();
        let stopped = FakeEngine::with_results([inspection(&plan, false), command([])]);
        assert!(
            run(
                &stopped,
                &plan,
                &workspace,
                &image,
                &cancellation.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            )
            .unwrap()
            .started
        );
        assert!(matches!(
            stopped.calls.lock().unwrap().as_slice(),
            [
                SetupEnsureCommand::Inspect { .. },
                SetupEnsureCommand::Start { .. }
            ]
        ));
        let running = FakeEngine::with_results([inspection(&plan, true)]);
        let receipt = run(
            &running,
            &plan,
            &workspace,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096),
        )
        .unwrap();
        assert!(!receipt.created && !receipt.started);
        assert!(matches!(
            running.calls.lock().unwrap().as_slice(),
            [SetupEnsureCommand::Inspect { .. }]
        ));
    }

    #[test]
    fn matching_container_reuse_does_not_reinspect_or_mutate_declared_volumes() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        let mut plan = plan(&workspace);
        let volume_name = format!("bosn-v-stack-{HASH}");
        plan.named_volumes.push(crate::SetupNamedVolume {
            name: volume_name.clone(),
            target: "/var/lib/app".into(),
            labels: BTreeMap::from([
                (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
                (LABEL_CONTENT_SHA256.into(), HASH.into()),
                (LABEL_CONTAINER_NAME.into(), volume_name),
            ]),
        });
        let image = prepared(&plan);
        let cancellation = CancellationSource::new();
        let engine = FakeEngine::with_results([inspection(&plan, true)]);

        let receipt = run(
            &engine,
            &plan,
            &workspace,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096),
        )
        .unwrap();

        assert!(!receipt.created && !receipt.started);
        assert!(matches!(
            engine.calls.lock().unwrap().as_slice(),
            [SetupEnsureCommand::Inspect { .. }]
        ));
    }

    #[test]
    fn foreign_or_mismatched_existing_container_is_refused_before_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        let plan = plan(&workspace);
        let image = prepared(&plan);
        let cancellation = CancellationSource::new();
        let mut foreign = observed(&plan, false);
        foreign
            .labels
            .insert(LABEL_MANAGED.into(), "foreign".into());
        let engine = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
            Some(foreign),
            result(0, [], []),
        ))]);
        assert!(matches!(
            run(
                &engine,
                &plan,
                &workspace,
                &image,
                &cancellation.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::OwnershipMismatch)
        ));
        assert!(matches!(
            engine.calls.lock().unwrap().as_slice(),
            [SetupEnsureCommand::Inspect { .. }]
        ));
    }

    #[test]
    fn tampered_inputs_refuse_before_engine_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let other = temporary.path().join("other");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        std::fs::create_dir(&other).unwrap();
        let plan = plan(&workspace);
        let image = prepared(&plan);
        let cancellation = CancellationSource::new();
        for mutated in [
            {
                let mut p = plan.clone();
                p.content_sha256 = "bad".into();
                p
            },
            {
                let mut p = plan.clone();
                p.app.mounts[0].target = "/bad//path".into();
                p
            },
            {
                let mut p = plan.clone();
                p.app.environment.insert("BAD-NAME".into(), "x".into());
                p
            },
            {
                let mut p = plan.clone();
                p.app.workdir = Some("../escape".into());
                p
            },
            {
                let mut p = plan.clone();
                p.app.command = Some("\0".into());
                p
            },
        ] {
            let engine = FakeEngine::default();
            assert!(matches!(
                run(
                    &engine,
                    &mutated,
                    &workspace,
                    &image,
                    &cancellation.token(),
                    RunOptions::streaming(Duration::from_secs(2), 4096)
                ),
                Err(SetupEnsureError::InvalidRequest(_))
            ));
            assert!(engine.calls.lock().unwrap().is_empty());
        }
        let engine = FakeEngine::default();
        assert!(matches!(
            run(
                &engine,
                &plan,
                &other,
                &image,
                &cancellation.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::InvalidRequest(_))
        ));
        assert!(engine.calls.lock().unwrap().is_empty());
        let mut wrong_image = image.clone();
        wrong_image.reference = "registry.example/other@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        let engine = FakeEngine::default();
        assert!(matches!(
            run(
                &engine,
                &plan,
                &workspace,
                &wrong_image,
                &cancellation.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::InvalidRequest(_))
        ));
        assert!(engine.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_deadline_output_and_nonzero_never_continue_to_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        let plan = plan(&workspace);
        let image = prepared(&plan);
        let source = CancellationSource::new();
        source.cancel();
        let cancelled = FakeEngine::default();
        assert!(matches!(
            run(
                &cancelled,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::Cancelled)
        ));
        assert!(cancelled.calls.lock().unwrap().is_empty());
        let deadline = FakeEngine::default();
        let source = CancellationSource::new();
        assert!(matches!(
            run(
                &deadline,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::ZERO, 4096)
            ),
            Err(SetupEnsureError::Deadline)
        ));
        assert!(deadline.calls.lock().unwrap().is_empty());
        let output = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
            Some(observed(&plan, true)),
            result(0, b"12345", []),
        ))]);
        let source = CancellationSource::new();
        assert!(matches!(
            run(
                &output,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::from_secs(2), 4)
            ),
            Err(SetupEnsureError::Transport(
                CommandError::OutputLimit { .. }
            ))
        ));
        assert!(matches!(
            output.calls.lock().unwrap().as_slice(),
            [SetupEnsureCommand::Inspect { .. }]
        ));
        let failed = FakeEngine::with_results([
            absent(),
            Ok(SetupEnsureResponse::Command(result(
                19,
                [],
                b"create failed",
            ))),
        ]);
        let source = CancellationSource::new();
        assert!(matches!(
            run(
                &failed,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::ActionFailed {
                action: "container create",
                ..
            })
        ));
        assert!(matches!(
            failed.calls.lock().unwrap().as_slice(),
            [
                SetupEnsureCommand::Inspect { .. },
                SetupEnsureCommand::Create { .. }
            ]
        ));

        let inspect_failed = FakeEngine::with_results([Ok(SetupEnsureResponse::Command(result(
            7,
            [],
            b"inspect failed",
        )))]);
        let source = CancellationSource::new();
        assert!(matches!(
            run(
                &inspect_failed,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::ActionFailed {
                action: "container inspect",
                ..
            })
        ));
        assert!(matches!(
            inspect_failed.calls.lock().unwrap().as_slice(),
            [SetupEnsureCommand::Inspect { .. }]
        ));

        let start_failed = FakeEngine::with_results([
            inspection(&plan, false),
            Ok(SetupEnsureResponse::Command(result(
                11,
                [],
                b"start failed",
            ))),
        ]);
        let source = CancellationSource::new();
        assert!(matches!(
            run(
                &start_failed,
                &plan,
                &workspace,
                &image,
                &source.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            ),
            Err(SetupEnsureError::ActionFailed {
                action: "container start",
                ..
            })
        ));
        assert!(matches!(
            start_failed.calls.lock().unwrap().as_slice(),
            [
                SetupEnsureCommand::Inspect { .. },
                SetupEnsureCommand::Start { .. }
            ]
        ));
    }
}
