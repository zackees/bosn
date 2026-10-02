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

use bosn_core::{MAX_ENVIRONMENT_ENTRIES, Retention, SETUP_DOCUMENT_VERSION, Scope};
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
const LABEL_CREATION_PROFILE: &str = "com.zackees.bosn.setup-creation-profile";

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

/// A typed tmpfs mount. It has no arbitrary option field: all permitted
/// options were parsed into finite values before reaching this engine seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureTmpfs {
    pub target: String,
    pub readonly: bool,
    pub size: Option<crate::SetupTmpfsSize>,
    pub exec: Option<bool>,
    pub mode: Option<u32>,
}

/// The only privileged container shape accepted by setup ensure.  Every value
/// originates in a parsed `macos-x64-guest` manifest declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupEnsureMacosGuest {
    pub ssh_port: u16,
    pub web_port: u16,
    pub version: String,
    pub ram_size: String,
    pub disk_size: String,
    pub cpu_cores: u16,
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
    ImageInspect {
        image_identity: String,
    },
    Create {
        container_name: String,
        image_identity: String,
        mounts: Vec<SetupEnsureMount>,
        volumes: Vec<SetupEnsureVolume>,
        tmpfs: Vec<SetupEnsureTmpfs>,
        /// Explicit manifest opt-in; resources created through it are not
        /// Bosn-managed.
        host_docker_socket: Option<crate::SetupHostDockerSocket>,
        environment: BTreeMap<String, String>,
        workdir: Option<String>,
        command: Option<String>,
        labels: BTreeMap<String, String>,
        macos_guest: Box<Option<SetupEnsureMacosGuest>>,
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
                "{{json .}}".into(),
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
                "{{json .}}".into(),
                container_name.clone(),
            ],
            Self::ImageInspect { image_identity } => vec![
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{json .}}".into(),
                image_identity.clone(),
            ],
            Self::Create {
                container_name,
                image_identity,
                mounts,
                volumes,
                tmpfs,
                host_docker_socket,
                environment,
                workdir,
                command,
                labels,
                macos_guest,
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
                for mount in tmpfs {
                    args.push("--tmpfs".into());
                    args.push(tmpfs_docker_value(mount));
                }
                if let Some(socket) = host_docker_socket {
                    let mut value = format!(
                        "type=bind,src={},dst={}",
                        socket.source.host_path(),
                        socket.target
                    );
                    if socket.readonly {
                        value.push_str(",readonly");
                    }
                    args.push("--mount".into());
                    args.push(value);
                }
                for (key, value) in environment {
                    args.push("--env".into());
                    args.push(format!("{key}={value}"));
                }
                if let Some(workdir) = workdir {
                    args.push("--workdir".into());
                    args.push(workdir.clone());
                }
                if let Some(guest) = macos_guest.as_ref() {
                    args.extend([
                        "--device".into(),
                        "/dev/kvm".into(),
                        "--device".into(),
                        "/dev/net/tun".into(),
                        "--cap-add".into(),
                        "NET_ADMIN".into(),
                        "--publish".into(),
                        format!("127.0.0.1:{}:22", guest.ssh_port),
                        "--publish".into(),
                        format!("127.0.0.1:{}:8006", guest.web_port),
                        "--stop-timeout".into(),
                        "120".into(),
                    ]);
                    for (key, value) in [
                        ("VERSION", &guest.version),
                        ("RAM_SIZE", &guest.ram_size),
                        ("DISK_SIZE", &guest.disk_size),
                        ("CPU_CORES", &guest.cpu_cores.to_string()),
                    ] {
                        args.push("--env".into());
                        args.push(format!("{key}={value}"));
                    }
                }
                args.push(image_identity.clone());
                if macos_guest.is_none()
                    && let Some(command) = command
                {
                    args.extend(crate::shell::login_shell_args(command));
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
    /// Bounded actual Docker Config, HostConfig and Mounts evidence. Labels
    /// alone are insufficient to authorize reuse.
    pub configuration: serde_json::Value,
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
            let result = if matches!(
                command,
                SetupEnsureCommand::Inspect { .. }
                    | SetupEnsureCommand::ImageInspect { .. }
                    | SetupEnsureCommand::VolumeInspect { .. }
            ) {
                private_observation(&engine, options, cancellation).await?
            } else {
                engine.stream(options, Some(cancellation), events).await?
            };
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

/// Raw Config/Env is verifier input, never job-log output. Keep the same
/// cancellation/deadline/reap path while draining an owned private channel.
async fn private_observation(
    engine: &DockerEngine,
    options: RunOptions,
    cancellation: &CancellationToken,
) -> Result<CommandResult, CommandError> {
    let (events, mut receiver) = kernal_api::async_engine::channel(32);
    let drain =
        kernal_api::async_engine::launch(async move { while receiver.recv().await.is_some() {} });
    let result = engine.stream(options, Some(cancellation), &events).await;
    drop(events);
    drain
        .await
        .map_err(|e| CommandError::Io(std::io::Error::other(e.to_string())))?;
    result
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
                return Err(SetupEnsureError::ActionFailed {
                    action: "container inspect",
                    detail: "private container observation failed".into(),
                });
            }
            observed
        }
        SetupEnsureResponse::Command(result) => {
            consume_output(&result, &mut remaining_output, request.options.output_limit)?;
            return Err(SetupEnsureError::ActionFailed {
                action: "container inspect",
                detail: "private container observation failed".into(),
            });
        }
    };

    if let Some(observed) = observed {
        validate_observed(&observed, &derived)?;
        verify_configuration(
            engine,
            &observed,
            &derived,
            &deadline,
            &mut remaining_output,
            &request,
        )
        .await?;
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
    // invoked. A matching existing container has now passed actual mount and
    // configuration and volume-metadata verification. For a new container, create/reuse every exact
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
            let observed = crate::creation::bounded_json(&result.stdout)
                .map_err(|_| SetupEnsureError::OwnershipMismatch)?;
            verify_volume_observation(volume, &observed, None)?;
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
            return Err(SetupEnsureError::EngineProtocol(
                "private volume inspect failed",
            ));
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
        SetupEnsureCommand::Inspect {
            container_name: derived.container_name.clone(),
        },
        &deadline,
        &mut remaining_output,
        &request,
    )
    .await?;
    let SetupEnsureResponse::Inspection(Some(observed), result) = response else {
        return Err(SetupEnsureError::OwnershipMismatch);
    };
    consume_output(&result, &mut remaining_output, request.options.output_limit)?;
    if !result.ok() || observed.container_id != container_id {
        return Err(SetupEnsureError::OwnershipMismatch);
    }
    validate_observed(&observed, &derived)?;
    verify_configuration(
        engine,
        &observed,
        &derived,
        &deadline,
        &mut remaining_output,
        &request,
    )
    .await?;
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
        None => {
            return Err(SetupEnsureError::ActionFailed {
                action: "container inspect",
                detail: "private container observation failed".into(),
            });
        }
    };
    validate_observed(&observed, &derived)?;
    verify_configuration(
        engine,
        &observed,
        &derived,
        &deadline,
        &mut remaining_output,
        &request,
    )
    .await?;
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
    let output_limit = if matches!(
        command,
        SetupEnsureCommand::Inspect { .. }
            | SetupEnsureCommand::ImageInspect { .. }
            | SetupEnsureCommand::VolumeInspect { .. }
    ) {
        (*remaining_output).min(crate::creation::MAX_OBSERVATION_BYTES)
    } else {
        *remaining_output
    };
    Ok(engine
        .stream(
            command,
            RunOptions::streaming(remaining, output_limit),
            request.cancellation,
            request.events,
        )
        .await?)
}

async fn verify_configuration<E: SetupEnsureEngine>(
    engine: &E,
    observed: &SetupEnsureObservedContainer,
    expected: &DerivedEnsure,
    deadline: &Deadline,
    remaining_output: &mut usize,
    request: &SetupEnsureRequest<'_>,
) -> Result<(), SetupEnsureError> {
    let response = invoke(
        engine,
        SetupEnsureCommand::ImageInspect {
            image_identity: expected.image_identity.clone(),
        },
        deadline,
        remaining_output,
        request,
    )
    .await?;
    let SetupEnsureResponse::Command(result) = response else {
        return Err(SetupEnsureError::EngineProtocol(
            "image configuration observation protocol mismatch",
        ));
    };
    consume_output(&result, remaining_output, request.options.output_limit)?;
    if !result.ok() {
        return Err(SetupEnsureError::EngineProtocol(
            "private image configuration inspect failed",
        ));
    }
    let image = crate::creation::bounded_json(&result.stdout)
        .map_err(|_| SetupEnsureError::OwnershipMismatch)?;
    verify_actual_configuration(observed, expected, &image)?;
    for volume in &expected.volumes {
        let response = invoke(
            engine,
            SetupEnsureCommand::VolumeInspect {
                volume_name: volume.name.clone(),
            },
            deadline,
            remaining_output,
            request,
        )
        .await?;
        let SetupEnsureResponse::Command(result) = response else {
            return Err(SetupEnsureError::EngineProtocol(
                "private volume observation protocol mismatch",
            ));
        };
        consume_output(&result, remaining_output, request.options.output_limit)?;
        if !result.ok() {
            return Err(SetupEnsureError::OwnershipMismatch);
        }
        let receipt = crate::creation::bounded_json(&result.stdout)
            .map_err(|_| SetupEnsureError::OwnershipMismatch)?;
        verify_volume_observation(
            volume,
            &receipt,
            Some(container_volume_source(observed, volume)?),
        )?;
    }
    Ok(())
}

fn container_volume_source<'a>(
    observed: &'a SetupEnsureObservedContainer,
    expected: &SetupEnsureVolume,
) -> Result<&'a str, SetupEnsureError> {
    observed
        .configuration
        .get("Mounts")
        .and_then(serde_json::Value::as_array)
        .and_then(|mounts| {
            mounts.iter().find(|m| {
                m.get("Destination").and_then(serde_json::Value::as_str)
                    == Some(expected.target.as_str())
                    && m.get("Name").and_then(serde_json::Value::as_str)
                        == Some(expected.name.as_str())
            })
        })
        .and_then(|m| m.get("Source").and_then(serde_json::Value::as_str))
        .ok_or(SetupEnsureError::OwnershipMismatch)
}

fn verify_volume_observation(
    expected: &SetupEnsureVolume,
    observed: &serde_json::Value,
    source: Option<&str>,
) -> Result<(), SetupEnsureError> {
    use serde_json::Value;
    let mismatch = || SetupEnsureError::OwnershipMismatch;
    if observed.get("Name").and_then(Value::as_str) != Some(expected.name.as_str())
        || observed.get("Driver").and_then(Value::as_str) != Some("local")
        || observed.get("Scope").and_then(Value::as_str) != Some("local")
    {
        return Err(mismatch());
    }
    match observed.get("Options") {
        None | Some(Value::Null) => {}
        Some(Value::Object(options)) if options.is_empty() => {}
        _ => return Err(mismatch()),
    }
    for (key, value) in &expected.labels {
        if observed
            .get("Labels")
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            != Some(value.as_str())
        {
            return Err(mismatch());
        }
    }
    let mountpoint = observed
        .get("Mountpoint")
        .and_then(Value::as_str)
        .ok_or_else(mismatch)?;
    // Docker's daemon-side path is not a host-client filesystem path (notably
    // on Desktop). Require bounded normalized absolute spelling and exact
    // agreement with the container attachment, never guess a daemon root.
    if mountpoint == "/"
        || validate_container_path(mountpoint).is_err()
        || source.is_some_and(|source| source != mountpoint)
    {
        return Err(mismatch());
    }
    Ok(())
}

fn verify_actual_configuration(
    observed: &SetupEnsureObservedContainer,
    expected: &DerivedEnsure,
    image: &serde_json::Value,
) -> Result<(), SetupEnsureError> {
    use serde_json::Value;
    let fail = || SetupEnsureError::OwnershipMismatch;
    if image.get("Id").and_then(Value::as_str) != Some(expected.image_identity.as_str()) {
        return Err(fail());
    }
    let actual = &observed.configuration;
    if actual.get("Id").and_then(Value::as_str) != Some(observed.container_id.as_str())
        || actual.get("Image").and_then(Value::as_str) != Some(expected.image_identity.as_str())
        || actual.get("Name").and_then(Value::as_str)
            != Some(format!("/{}", expected.container_name).as_str())
        || actual.pointer("/State/Running").and_then(Value::as_bool) != Some(observed.running)
    {
        return Err(fail());
    }
    let base = image
        .get("Config")
        .and_then(Value::as_object)
        .ok_or_else(fail)?;
    let config = actual
        .get("Config")
        .and_then(Value::as_object)
        .ok_or_else(fail)?;
    let host = actual
        .get("HostConfig")
        .and_then(Value::as_object)
        .ok_or_else(fail)?;
    for (key, value) in &expected.labels {
        if config
            .get("Labels")
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            != Some(value.as_str())
        {
            return Err(fail());
        }
    }
    for key in ["Healthcheck", "StopSignal"] {
        if config.get(key).filter(|v| !v.is_null()) != base.get(key).filter(|v| !v.is_null()) {
            return Err(fail());
        }
    }
    for key in ["Tty", "OpenStdin", "StdinOnce", "AttachStdin"] {
        if config.get(key).and_then(Value::as_bool) != Some(false) {
            return Err(fail());
        }
    }
    let strings = |value: Option<&Value>| -> Result<Vec<String>, SetupEnsureError> {
        match value {
            None | Some(Value::Null) => Ok(vec![]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .filter(|s| s.len() <= 16 * 1024 && !s.contains('\0'))
                        .map(str::to_owned)
                        .ok_or_else(fail)
                })
                .collect(),
            _ => Err(fail()),
        }
    };
    let scalar = |value: Option<&Value>| -> Result<String, SetupEnsureError> {
        match value {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) if s.len() <= 16 * 1024 && !s.contains('\0') => Ok(s.clone()),
            _ => Err(fail()),
        }
    };
    // Every inherited VOLUME must be overridden by an explicit verified
    // attachment (notably dockurr's /storage). Never accept anonymous storage.
    let volume_keys = |value: Option<&Value>| -> Result<BTreeSet<String>, SetupEnsureError> {
        match value {
            None | Some(Value::Null) => Ok(BTreeSet::new()),
            Some(Value::Object(entries))
                if entries
                    .values()
                    .all(|v| v.as_object().is_some_and(|v| v.is_empty())) =>
            {
                Ok(entries.keys().cloned().collect())
            }
            _ => Err(fail()),
        }
    };
    let inherited_volumes = volume_keys(base.get("Volumes"))?;
    if volume_keys(config.get("Volumes"))? != inherited_volumes {
        return Err(fail());
    }
    for target in inherited_volumes {
        if !expected.mounts.iter().any(|m| m.target == target)
            && !expected.volumes.iter().any(|v| v.target == target)
            && !expected.tmpfs.iter().any(|m| m.target == target)
            && expected
                .host_docker_socket
                .as_ref()
                .is_none_or(|s| s.target != target)
        {
            return Err(fail());
        }
    }
    let mut environment = BTreeMap::new();
    for item in strings(base.get("Env"))? {
        let (key, value) = item.split_once('=').ok_or_else(fail)?;
        if environment
            .insert(key.to_owned(), value.to_owned())
            .is_some()
        {
            return Err(fail());
        }
    }
    environment.extend(expected.environment.clone());
    if let Some(guest) = &expected.macos_guest {
        environment.extend([
            ("VERSION".into(), guest.version.clone()),
            ("RAM_SIZE".into(), guest.ram_size.clone()),
            ("DISK_SIZE".into(), guest.disk_size.clone()),
            ("CPU_CORES".into(), guest.cpu_cores.to_string()),
        ]);
    }
    let mut actual_env = BTreeMap::new();
    for item in strings(config.get("Env"))? {
        let (key, value) = item.split_once('=').ok_or_else(fail)?;
        if actual_env
            .insert(key.to_owned(), value.to_owned())
            .is_some()
        {
            return Err(fail());
        }
    }
    let expected_cmd = match (&expected.command, &expected.macos_guest) {
        (Some(command), None) => crate::shell::login_shell_args(command).to_vec(),
        _ => strings(base.get("Cmd"))?,
    };
    if environment != actual_env
        || strings(config.get("Cmd"))? != expected_cmd
        || strings(config.get("Entrypoint"))? != strings(base.get("Entrypoint"))?
        || scalar(config.get("User"))? != scalar(base.get("User"))?
        || scalar(config.get("WorkingDir"))?
            != expected
                .workdir
                .clone()
                .unwrap_or(scalar(base.get("WorkingDir"))?)
    {
        return Err(fail());
    }
    if host.get("Privileged").and_then(Value::as_bool) != Some(false)
        || host
            .get("NetworkMode")
            .and_then(Value::as_str)
            .is_none_or(|v| !["default", "bridge"].contains(&v))
    {
        return Err(fail());
    }
    for field in [
        "Binds",
        "VolumesFrom",
        "DeviceRequests",
        "SecurityOpt",
        "GroupAdd",
        "DeviceCgroupRules",
    ] {
        if !matches!(host.get(field), Some(Value::Null))
            && !host
                .get(field)
                .is_some_and(|v| v.as_array().is_some_and(Vec::is_empty))
        {
            return Err(fail());
        }
    }
    if host.get("ReadonlyRootfs").and_then(Value::as_bool) != Some(false) {
        return Err(fail());
    }
    if host.get("PublishAllPorts").and_then(Value::as_bool) != Some(false) {
        return Err(fail());
    }
    if host.get("AutoRemove").and_then(Value::as_bool) != Some(false)
        || host.get("CgroupnsMode").and_then(Value::as_str) != Some("private")
        || host.get("RestartPolicy")
            != Some(&serde_json::json!({"Name":"no","MaximumRetryCount":0}))
    {
        return Err(fail());
    }
    for field in ["PidMode", "UTSMode", "UsernsMode"] {
        if host.get(field).and_then(Value::as_str) != Some("") {
            return Err(fail());
        }
    }
    if host
        .get("IpcMode")
        .and_then(Value::as_str)
        .is_none_or(|v| !["private", ""].contains(&v))
    {
        return Err(fail());
    }
    let mut wanted = BTreeMap::new();
    for mount in &expected.mounts {
        wanted.insert(
            mount.target.clone(),
            (
                "bind".to_owned(),
                mount.source.to_string_lossy().into_owned(),
                !mount.readonly,
            ),
        );
    }
    for volume in &expected.volumes {
        wanted.insert(
            volume.target.clone(),
            ("volume".to_owned(), volume.name.clone(), true),
        );
    }
    if let Some(socket) = &expected.host_docker_socket {
        wanted.insert(
            socket.target.clone(),
            (
                "bind".into(),
                socket.source.host_path().into(),
                !socket.readonly,
            ),
        );
    }
    let mut seen = BTreeMap::new();
    let mut seen_tmpfs = BTreeSet::new();
    for mount in actual
        .get("Mounts")
        .and_then(Value::as_array)
        .ok_or_else(fail)?
    {
        let kind = mount.get("Type").and_then(Value::as_str).ok_or_else(fail)?;
        let target = mount
            .get("Destination")
            .and_then(Value::as_str)
            .ok_or_else(fail)?;
        if kind == "tmpfs" {
            let wanted = expected
                .tmpfs
                .iter()
                .find(|m| m.target == target)
                .ok_or_else(fail)?;
            if !seen_tmpfs.insert(target)
                || mount.get("RW").and_then(Value::as_bool) != Some(!wanted.readonly)
            {
                return Err(fail());
            }
            continue;
        }
        let source = mount
            .get(if kind == "volume" { "Name" } else { "Source" })
            .and_then(Value::as_str)
            .ok_or_else(fail)?;
        if (kind == "bind" && mount.get("Propagation").and_then(Value::as_str) != Some("rprivate"))
            || (kind == "volume" && mount.get("Driver").and_then(Value::as_str) != Some("local"))
        {
            return Err(fail());
        }
        let rw = mount.get("RW").and_then(Value::as_bool).ok_or_else(fail)?;
        if seen
            .insert(target.to_owned(), (kind.to_owned(), source.to_owned(), rw))
            .is_some()
        {
            return Err(fail());
        }
    }
    if seen != wanted {
        return Err(fail());
    }
    // Verify declared mount options too: actual attachments alone do not
    // exclude a latent subpath, propagation or volume-driver substitution.
    let mut declared = BTreeMap::new();
    let declared_mounts = match host.get("Mounts") {
        Some(Value::Array(items)) => items.as_slice(),
        None | Some(Value::Null) if wanted.is_empty() => &[],
        _ => return Err(fail()),
    };
    for mount in declared_mounts {
        let object = mount.as_object().ok_or_else(fail)?;
        if object
            .keys()
            .any(|k| !["Type", "Source", "Target", "ReadOnly"].contains(&k.as_str()))
        {
            return Err(fail());
        }
        let text = |key| object.get(key).and_then(Value::as_str).ok_or_else(fail);
        let readonly = match object.get("ReadOnly") {
            None => false,
            Some(Value::Bool(v)) => *v,
            _ => return Err(fail()),
        };
        if declared
            .insert(
                text("Target")?.to_owned(),
                (
                    text("Type")?.to_owned(),
                    text("Source")?.to_owned(),
                    !readonly,
                ),
            )
            .is_some()
        {
            return Err(fail());
        }
    }
    if declared != wanted || host.get("VolumeDriver").and_then(Value::as_str) != Some("") {
        return Err(fail());
    }
    let tmpfs = match host.get("Tmpfs") {
        None | Some(Value::Null) => None,
        Some(Value::Object(entries)) => Some(entries),
        _ => return Err(fail()),
    };
    let wanted_tmpfs: BTreeMap<_, _> = expected
        .tmpfs
        .iter()
        .map(|m| {
            let value = tmpfs_docker_value(m);
            let (_, options) = value.split_once(':').expect("tmpfs options");
            (m.target.clone(), Value::String(options.into()))
        })
        .collect();
    if tmpfs
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default()
        != wanted_tmpfs
    {
        return Err(fail());
    }
    // Host-only guest grants must never leak into ordinary Linux profiles.
    if expected.macos_guest.is_none() {
        for field in ["Devices", "CapAdd", "CapDrop"] {
            if !matches!(host.get(field), Some(Value::Null))
                && !host
                    .get(field)
                    .is_some_and(|v| v.as_array().is_some_and(Vec::is_empty))
            {
                return Err(fail());
            }
        }
        if !host
            .get("PortBindings")
            .is_some_and(|v| v.is_null() || v.as_object().is_some_and(|m| m.is_empty()))
        {
            return Err(fail());
        }
    } else if let Some(guest) = &expected.macos_guest {
        if strings(host.get("CapAdd"))? != ["NET_ADMIN"]
            || !strings(host.get("CapDrop"))?.is_empty()
        {
            return Err(fail());
        }
        let devices = host
            .get("Devices")
            .and_then(Value::as_array)
            .ok_or_else(fail)?;
        let mut paths = BTreeSet::new();
        for device in devices {
            let source = device
                .get("PathOnHost")
                .and_then(Value::as_str)
                .ok_or_else(fail)?;
            if device.get("PathInContainer").and_then(Value::as_str) != Some(source)
                || device.get("CgroupPermissions").and_then(Value::as_str) != Some("rwm")
                || !paths.insert(source)
            {
                return Err(fail());
            }
        }
        if paths != BTreeSet::from(["/dev/kvm", "/dev/net/tun"]) {
            return Err(fail());
        }
        let ports = serde_json::json!({
            "22/tcp": [{"HostIp":"127.0.0.1", "HostPort":guest.ssh_port.to_string()}],
            "8006/tcp": [{"HostIp":"127.0.0.1", "HostPort":guest.web_port.to_string()}]
        });
        if host.get("PortBindings") != Some(&ports)
            || config.get("StopTimeout").and_then(Value::as_u64) != Some(120)
        {
            return Err(fail());
        }
    }
    Ok(())
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
    tmpfs: Vec<SetupEnsureTmpfs>,
    host_docker_socket: Option<crate::SetupHostDockerSocket>,
    macos_guest: Option<SetupEnsureMacosGuest>,
}

impl DerivedEnsure {
    fn create_command(&self) -> SetupEnsureCommand {
        SetupEnsureCommand::Create {
            container_name: self.container_name.clone(),
            image_identity: self.image_identity.clone(),
            mounts: self.mounts.clone(),
            volumes: self.volumes.clone(),
            tmpfs: self.tmpfs.clone(),
            host_docker_socket: self.host_docker_socket.clone(),
            environment: self.environment.clone(),
            workdir: self.workdir.clone(),
            command: self.command.clone(),
            labels: self.labels.clone(),
            macos_guest: Box::new(self.macos_guest.clone()),
        }
    }
}

fn derive_command(request: &SetupEnsureRequest<'_>) -> Result<DerivedEnsure, SetupEnsureError> {
    derive_creation(
        request.plan,
        &request.workspace_root,
        request.prepared_image,
    )
}

/// The sole container identity derivation shared by ensure, adoption and task execution.
pub fn setup_container_name(
    plan: &SetupPlan,
    workspace: &Path,
    image: &PreparedImage,
) -> Result<String, SetupEnsureError> {
    Ok(derive_creation(plan, workspace, image)?.container_name)
}

/// Pure verification for trusted engine adapters. These supplied observations
/// are data, not authentication; public clients cannot confer engine authority.
pub fn verify_setup_observation(
    plan: &SetupPlan,
    image: &PreparedImage,
    observed: &SetupEnsureObservedContainer,
    image_configuration: &serde_json::Value,
    volume_observations: &BTreeMap<String, serde_json::Value>,
) -> Result<(), SetupEnsureError> {
    let expected = derive_creation(plan, &plan.workspace_root, image)?;
    validate_observed(observed, &expected)?;
    verify_actual_configuration(observed, &expected, image_configuration)?;
    if volume_observations.len() != expected.volumes.len() {
        return Err(SetupEnsureError::OwnershipMismatch);
    }
    for volume in &expected.volumes {
        let receipt = volume_observations
            .get(&volume.name)
            .ok_or(SetupEnsureError::OwnershipMismatch)?;
        verify_volume_observation(
            volume,
            receipt,
            Some(container_volume_source(observed, volume)?),
        )?;
    }
    Ok(())
}

fn derive_creation(
    plan: &SetupPlan,
    workspace: &Path,
    image: &PreparedImage,
) -> Result<DerivedEnsure, SetupEnsureError> {
    validate_plan_shape(plan)?;
    let workspace_root = canonical_workspace(workspace)?;
    if workspace_root.to_str().is_none() {
        return Err(SetupEnsureError::InvalidRequest("workspace is not UTF-8"));
    }
    if workspace_root != plan.workspace_root {
        return Err(SetupEnsureError::InvalidRequest(
            "workspace is not the plan's canonical workspace root",
        ));
    }
    if plan.app.mounts.len() > 128 || plan.named_volumes.len() > 128 || plan.tmpfs.len() > 128 {
        return Err(SetupEnsureError::InvalidRequest(
            "creation mount inventory exceeds 128 entries per kind",
        ));
    }
    validate_prepared_image(plan, image)?;
    let mounts = derive_mounts(&workspace_root, plan)?;
    let workdir = plan
        .app
        .workdir
        .as_deref()
        .map(|value| resolve_workdir(value, &plan.app.mounts, &workspace_root))
        .transpose()?;
    let command = plan.app.command.clone();
    if command
        .as_deref()
        .is_some_and(|value| value.is_empty() || value.len() > 16 * 1024 || value.contains('\0'))
    {
        return Err(SetupEnsureError::InvalidRequest(
            "declared app command is invalid",
        ));
    }
    let volumes = derive_volumes(plan)?;
    let tmpfs = derive_tmpfs(plan)?;
    let macos_guest = plan
        .macos_guest
        .as_ref()
        .map(|guest| SetupEnsureMacosGuest {
            ssh_port: guest.ssh_port,
            web_port: guest.web_port,
            version: guest.version.clone(),
            ram_size: guest.ram_size.clone(),
            disk_size: guest.disk_size.clone(),
            cpu_cores: guest.cpu_cores,
        });
    let mut derived = DerivedEnsure {
        container_name: String::new(),
        image_identity: image.observed_identity.clone(),
        mounts,
        environment: validated_environment(&plan.app.environment)?,
        workdir,
        command,
        labels: BTreeMap::new(),
        volumes,
        tmpfs,
        host_docker_socket: plan.host_docker_socket.clone(),
        macos_guest,
    };
    derived
        .mounts
        .sort_by(|left, right| left.target.cmp(&right.target));
    derived
        .volumes
        .sort_by(|left, right| left.target.cmp(&right.target));
    derived
        .tmpfs
        .sort_by(|left, right| left.target.cmp(&right.target));
    let mut arguments = vec![plan.content_sha256.clone()];
    arguments.extend(derived.create_command().docker_args());
    if arguments
        .iter()
        .try_fold(0usize, |size, value| size.checked_add(value.len() + 8))
        .is_none_or(|size| size > 2 * 1024 * 1024)
    {
        return Err(SetupEnsureError::InvalidRequest(
            "creation profile exceeds 2 MiB",
        ));
    }
    let digest = crate::creation::creation_digest(&workspace_root, &arguments);
    derived.container_name = format!("bosn-setup-v2-{digest}");
    derived.labels = BTreeMap::from([
        (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
        (LABEL_CONTENT_SHA256.into(), plan.content_sha256.clone()),
        (LABEL_CONTAINER_NAME.into(), derived.container_name.clone()),
        (LABEL_CREATION_PROFILE.into(), format!("v2:{digest}")),
    ]);
    Ok(derived)
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
        ) if plan.asset_root.as_ref().is_some_and(|root| {
            crate::materialize::materialized_dockerfile_relative(root, dockerfile_path).is_some()
        }) => {}
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
    let mut targets: BTreeSet<String> = plan
        .app
        .mounts
        .iter()
        .map(|mount| mount.target.clone())
        .collect();
    if targets.len() != plan.app.mounts.len() {
        return Err(SetupEnsureError::InvalidRequest(
            "duplicate bind mount target",
        ));
    }
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
    for tmpfs in &plan.tmpfs {
        if validate_container_path(&tmpfs.target).is_err()
            || !targets.insert(tmpfs.target.clone())
            || tmpfs.size.as_ref().is_some_and(|size| size.value == 0)
            || tmpfs.mode.is_some_and(|mode| mode > 0o7777)
        {
            return Err(SetupEnsureError::InvalidRequest(
                "tmpfs receipt was modified",
            ));
        }
    }
    if let Some(socket) = &plan.host_docker_socket
        && (validate_container_path(&socket.target).is_err()
            || !targets.insert(socket.target.clone())
            || plan.macos_guest.is_some()
            || crate::SetupHostDockerSocketSource::from_host_path(socket.source.host_path())
                != Some(socket.source))
    {
        return Err(SetupEnsureError::InvalidRequest(
            "host Docker socket receipt was modified",
        ));
    }
    if let Some(guest) = &plan.macos_guest
        && (!matches!(plan.app.source, bosn_core::SetupSource::PinnedImage(_))
            || !matches!(&plan.app.source, bosn_core::SetupSource::PinnedImage(image) if valid_macos_guest_image(image))
            || !plan.app.mounts.is_empty()
            || plan.app.workdir.is_some()
            || plan.app.command.is_some()
            || guest.ssh_port == 0
            || guest.web_port == 0
            || guest.ssh_port == guest.web_port
            || guest.cpu_cores == 0
            || !valid_guest_env_value(&guest.version)
            || !valid_guest_env_value(&guest.ram_size)
            || !valid_guest_env_value(&guest.disk_size))
    {
        return Err(SetupEnsureError::InvalidRequest(
            "macOS guest receipt was modified",
        ));
    }
    if let Some(guest) = &plan.macos_guest {
        let matching_storage = plan
            .named_volumes
            .iter()
            .filter(|volume| volume.target == "/storage")
            .collect::<Vec<_>>();
        if guest.storage_volume.is_empty()
            || matching_storage.len() != 1
            || matching_storage[0].name != guest.storage_volume
            || !valid_volume_name(&guest.storage_volume)
            || guest.storage_scope != Scope::Machine
            || guest.storage_retention != Retention::Pinned
        {
            return Err(SetupEnsureError::InvalidRequest(
                "macOS guest storage volume receipt was modified",
            ));
        }
    }
    Ok(())
}

fn valid_guest_env_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.contains(['\0', '\n', '\r', '='])
}

/// Docker Hub's documented registry spellings all designate the one trusted
/// dockurr entrypoint image. A digest pins the bytes; accepting another
/// repository here would turn the fixed KVM/tun create shape into a generic
/// privileged-container escape hatch.
fn valid_macos_guest_image(value: &str) -> bool {
    let Some((name, digest)) = value.rsplit_once("@sha256:") else {
        return false;
    };
    let repository = if let Some((prefix, final_component)) = name.rsplit_once('/') {
        if let Some((repository, _tag)) = final_component.split_once(':') {
            format!("{prefix}/{repository}")
        } else {
            name.to_owned()
        }
    } else {
        name.to_owned()
    };
    matches!(
        repository.as_str(),
        "dockurr/macos"
            | "docker.io/dockurr/macos"
            | "index.docker.io/dockurr/macos"
            | "registry-1.docker.io/dockurr/macos"
    ) && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

fn derive_tmpfs(plan: &SetupPlan) -> Result<Vec<SetupEnsureTmpfs>, SetupEnsureError> {
    plan.tmpfs
        .iter()
        .map(|mount| {
            Ok(SetupEnsureTmpfs {
                target: mount.target.clone(),
                readonly: mount.readonly,
                size: mount.size.clone(),
                exec: mount.exec,
                mode: mount.mode,
            })
        })
        .collect()
}

fn tmpfs_docker_value(mount: &SetupEnsureTmpfs) -> String {
    let mut options = Vec::new();
    if mount.readonly {
        options.push("ro".to_owned());
    }
    if let Some(size) = &mount.size {
        let unit = match size.unit {
            crate::SetupTmpfsSizeUnit::Bytes => "b",
            crate::SetupTmpfsSizeUnit::Kibibytes => "k",
            crate::SetupTmpfsSizeUnit::Mebibytes => "m",
            crate::SetupTmpfsSizeUnit::Gibibytes => "g",
        };
        options.push(format!("size={}{}", size.value, unit));
    }
    match mount.exec {
        Some(true) => options.push("exec".to_owned()),
        Some(false) => options.push("noexec".to_owned()),
        None => {}
    }
    if let Some(mode) = mount.mode {
        options.push(format!("mode={mode:o}"));
    }
    if options.is_empty() {
        mount.target.clone()
    } else {
        format!("{}:{}", mount.target, options.join(","))
    }
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
    let value = crate::creation::bounded_json(stdout)
        .map_err(|_| protocol_error("invalid bounded container observation"))?;
    let text = |field: &str| value.get(field).and_then(serde_json::Value::as_str);
    let container_id = text("Id").ok_or_else(|| protocol_error("missing container ID"))?;
    let image_identity = text("Image").ok_or_else(|| protocol_error("missing image ID"))?;
    let running = value
        .pointer("/State/Running")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| protocol_error("missing running state"))?;
    if !valid_container_id(container_id) || !valid_identity(image_identity) {
        return Err(protocol_error("invalid observed identities"));
    }
    let mut labels = BTreeMap::new();
    for key in [
        LABEL_MANAGED,
        LABEL_CONTENT_SHA256,
        LABEL_CONTAINER_NAME,
        LABEL_CREATION_PROFILE,
    ] {
        let label = value
            .get("Config")
            .and_then(|v| v.get("Labels"))
            .and_then(|v| v.get(key))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| protocol_error("missing creation ownership label"))?;
        labels.insert(key.into(), label.into());
    }
    Ok(SetupEnsureObservedContainer {
        container_id: container_id.into(),
        running,
        image_identity: image_identity.into(),
        labels,
        configuration: value,
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

    #[cfg(all(unix, feature = "native-test-helper"))]
    #[test]
    fn structured_inspection_environment_is_captured_privately_not_forwarded() {
        let engine = DockerEngine::synthetic_for_test(
            "/bin/sh",
            [
                "-c",
                "printf '%s' '{\"Config\":{\"Env\":[\"SECRET=private-canary\"]}}'",
                "sh",
            ],
        );
        let cancellation = CancellationSource::new();
        let (events, mut receiver) = channel(8);
        let result = runtime()
            .run(SetupEnsureEngine::stream(
                &engine,
                SetupEnsureCommand::ImageInspect {
                    image_identity: IDENTITY.into(),
                },
                RunOptions::streaming(Duration::from_secs(2), 4096),
                &cancellation.token(),
                &events,
            ))
            .unwrap();
        let SetupEnsureResponse::Command(result) = result else {
            panic!("expected private command receipt");
        };
        assert!(
            String::from_utf8(result.stdout)
                .unwrap()
                .contains("private-canary")
        );
        drop(events);
        assert!(runtime().run(receiver.recv()).is_none());
    }

    #[test]
    fn creation_identity_binds_workspace_even_when_image_and_content_match() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::create_dir(first.path().join("src")).unwrap();
        std::fs::create_dir(second.path().join("src")).unwrap();
        let cancel = CancellationSource::new();
        let (events, _) = channel(8);
        let name = |workspace: &Path| {
            let plan = plan(workspace);
            let prepared = prepared(&plan);
            derive_command(&SetupEnsureRequest {
                plan: &plan,
                workspace_root: plan.workspace_root.clone(),
                prepared_image: &prepared,
                options: RunOptions::streaming(Duration::from_secs(2), 4096),
                cancellation: &cancel.token(),
                events: &events,
            })
            .unwrap()
            .container_name
        };
        assert_ne!(name(first.path()), name(second.path()));
        assert_eq!(name(first.path()), name(first.path()));
    }

    #[derive(Default)]
    struct FakeEngine {
        calls: Mutex<Vec<SetupEnsureCommand>>,
        results: Mutex<VecDeque<Result<SetupEnsureResponse, CommandError>>>,
        created: Mutex<Option<serde_json::Value>>,
    }

    impl FakeEngine {
        fn with_results(
            results: impl IntoIterator<Item = Result<SetupEnsureResponse, CommandError>>,
        ) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                results: Mutex::new(results.into_iter().collect()),
                created: Mutex::new(None),
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
            let command = self.calls.lock().unwrap().last().unwrap().clone();
            if matches!(command, SetupEnsureCommand::ImageInspect { .. }) {
                return ready(Ok(SetupEnsureResponse::Command(result(
                    0,
                    serde_json::to_vec(&fixture_image()).unwrap(),
                    [],
                ))));
            }
            if matches!(command, SetupEnsureCommand::Inspect { .. })
                && let Some(value) = self.created.lock().unwrap().as_ref()
            {
                return ready(Ok(SetupEnsureResponse::Inspection(
                    Some(parse_inspection(&serde_json::to_vec(value).unwrap()).unwrap()),
                    result(0, [], []),
                )));
            }
            if matches!(command, SetupEnsureCommand::Create { .. }) {
                *self.created.lock().unwrap() = Some(fixture_configuration(&command, false));
            }
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
        let image = prepared(plan);
        let derived = derive_creation(plan, &plan.workspace_root, &image).unwrap();
        SetupEnsureObservedContainer {
            container_id: CONTAINER_ID.into(),
            running,
            image_identity: IDENTITY.into(),
            labels: derived.labels.clone(),
            configuration: fixture_configuration(&derived.create_command(), running),
        }
    }

    fn fixture_image() -> serde_json::Value {
        serde_json::json!({"Id":IDENTITY,"Config":{
            "Env":["PATH=/usr/bin:/bin"], "Cmd":["sh"],"Entrypoint":null,
            "User":"","WorkingDir":"","Volumes":null
        }})
    }

    fn fixture_configuration(command: &SetupEnsureCommand, running: bool) -> serde_json::Value {
        let SetupEnsureCommand::Create {
            image_identity,
            mounts,
            volumes,
            tmpfs,
            host_docker_socket,
            environment,
            workdir,
            command,
            labels,
            macos_guest,
            ..
        } = command
        else {
            panic!("fixture requires create");
        };
        let mut config = fixture_image()["Config"].clone();
        for key in ["Tty", "OpenStdin", "StdinOnce", "AttachStdin"] {
            config[key] = false.into();
        }
        let mut env = environment.clone();
        env.entry("PATH".into())
            .or_insert_with(|| "/usr/bin:/bin".into());
        if let Some(guest) = macos_guest.as_ref() {
            env.extend([
                ("VERSION".into(), guest.version.clone()),
                ("RAM_SIZE".into(), guest.ram_size.clone()),
                ("DISK_SIZE".into(), guest.disk_size.clone()),
                ("CPU_CORES".into(), guest.cpu_cores.to_string()),
            ]);
            config["StopTimeout"] = 120.into();
        } else if let Some(command) = command {
            config["Cmd"] = serde_json::json!(crate::shell::login_shell_args(command));
        }
        config["Env"] = serde_json::json!(
            env.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
        );
        config["WorkingDir"] = serde_json::json!(workdir.as_deref().unwrap_or(""));
        config["Labels"] = serde_json::json!(labels);
        let mut actual_mounts: Vec<_> = mounts.iter().map(|m| serde_json::json!({"Type":"bind","Propagation":"rprivate","Source":m.source,"Destination":m.target,"RW":!m.readonly})).collect();
        actual_mounts.extend(volumes.iter().map(
            |v| serde_json::json!({"Type":"volume","Driver":"local","Name":v.name,"Source":format!("/var/lib/docker/volumes/{}/_data",v.name),"Destination":v.target,"RW":true}),
        ));
        if let Some(socket) = host_docker_socket {
            actual_mounts.push(serde_json::json!({"Type":"bind","Propagation":"rprivate","Source":socket.source.host_path(),"Destination":socket.target,"RW":!socket.readonly}));
        }
        let tmpfs: BTreeMap<_, _> = tmpfs
            .iter()
            .map(|m| {
                (
                    m.target.clone(),
                    tmpfs_docker_value(m).split_once(':').unwrap().1.to_owned(),
                )
            })
            .collect();
        let declared_mounts: Vec<_> = actual_mounts.iter().map(|m| serde_json::json!({"Type":m["Type"],
            "Source":if m["Type"] == "volume" { &m["Name"] } else { &m["Source"] }, "Target":m["Destination"],"ReadOnly":!m["RW"].as_bool().unwrap()})).collect();
        let mut host = serde_json::json!({"Mounts":declared_mounts,"VolumeDriver":"","Privileged":false,"NetworkMode":"default","Binds":null,"VolumesFrom":null,"DeviceRequests":null,
            "SecurityOpt":null,"GroupAdd":null,"DeviceCgroupRules":null,"PublishAllPorts":false,"AutoRemove":false,"CgroupnsMode":"private","RestartPolicy":{"Name":"no","MaximumRetryCount":0},"ReadonlyRootfs":false,"PidMode":"","UTSMode":"","UsernsMode":"","IpcMode":"private",
            "Devices":[],"CapAdd":null,"CapDrop":null,"PortBindings":{},"Tmpfs":tmpfs});
        if let Some(guest) = macos_guest.as_ref() {
            host["Devices"] = serde_json::json!([{"PathOnHost":"/dev/kvm","PathInContainer":"/dev/kvm","CgroupPermissions":"rwm"},
                {"PathOnHost":"/dev/net/tun","PathInContainer":"/dev/net/tun","CgroupPermissions":"rwm"}]);
            host["CapAdd"] = serde_json::json!(["NET_ADMIN"]);
            host["PortBindings"] = serde_json::json!({"22/tcp":[{"HostIp":"127.0.0.1","HostPort":guest.ssh_port.to_string()}],
                "8006/tcp":[{"HostIp":"127.0.0.1","HostPort":guest.web_port.to_string()}]});
        }
        serde_json::json!({"Id":CONTAINER_ID,"Name":format!("/{}",labels[LABEL_CONTAINER_NAME]),"Image":image_identity,"State":{"Running":running},"Config":config,"HostConfig":host,"Mounts":actual_mounts})
    }

    #[test]
    fn correctly_labelled_wrong_actual_bind_or_execution_config_refuses() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        let plan = plan(workspace.path());
        let derived = derive_creation(&plan, workspace.path(), &prepared(&plan)).unwrap();
        let observed = observed(&plan, true);
        assert!(verify_actual_configuration(&observed, &derived, &fixture_image()).is_ok());
        for (pointer, replacement) in [
            ("/Mounts/0/Source", serde_json::json!("/foreign/workspace")),
            ("/Mounts/0/RW", serde_json::json!(false)),
            ("/Config/WorkingDir", serde_json::json!("/foreign")),
            ("/Config/Cmd", serde_json::json!(["wrong"])),
            ("/Config/Env", serde_json::json!(["PATH=/foreign"])),
            ("/HostConfig/Privileged", serde_json::json!(true)),
            ("/HostConfig/Tmpfs", serde_json::json!("malformed grant")),
            ("/HostConfig/AutoRemove", serde_json::json!(true)),
            ("/HostConfig/CgroupnsMode", serde_json::json!("host")),
        ] {
            let mut bad = observed.clone();
            *bad.configuration.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                matches!(
                    verify_actual_configuration(&bad, &derived, &fixture_image()),
                    Err(SetupEnsureError::OwnershipMismatch)
                ),
                "{pointer}"
            );
            let engine = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
                Some(bad),
                result(0, [], []),
            ))]);
            let cancellation = CancellationSource::new();
            assert!(
                run(
                    &engine,
                    &plan,
                    workspace.path(),
                    &prepared(&plan),
                    &cancellation.token(),
                    RunOptions::streaming(Duration::from_secs(2), 4096)
                )
                .is_err()
            );
            assert!(engine.calls.lock().unwrap().iter().all(|c| matches!(
                c,
                SetupEnsureCommand::Inspect { .. } | SetupEnsureCommand::ImageInspect { .. }
            )));
        }
        let mut legacy = observed.clone();
        legacy.labels.remove(LABEL_CREATION_PROFILE);
        assert!(matches!(
            validate_observed(&legacy, &derived),
            Err(SetupEnsureError::OwnershipMismatch)
        ));
    }

    #[test]
    fn creation_identity_and_actual_verification_bind_named_volume_set() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        let mut plan = plan(workspace.path());
        plan.named_volumes.push(crate::SetupNamedVolume {
            name: "bosn-v-stack-a".into(),
            target: "/target".into(),
            labels: BTreeMap::from([
                (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
                (LABEL_CONTENT_SHA256.into(), HASH.into()),
                (LABEL_CONTAINER_NAME.into(), "bosn-v-stack-a".into()),
            ]),
        });
        let derived = derive_creation(&plan, workspace.path(), &prepared(&plan)).unwrap();
        let observed = observed(&plan, true);
        assert!(verify_actual_configuration(&observed, &derived, &fixture_image()).is_ok());
        let mut bad = observed.clone();
        bad.configuration["Mounts"][1]["Name"] = "bosn-v-stack-foreign".into();
        assert!(verify_actual_configuration(&bad, &derived, &fixture_image()).is_err());
        plan.named_volumes[0].name = "bosn-v-stack-b".into();
        plan.named_volumes[0]
            .labels
            .insert(LABEL_CONTAINER_NAME.into(), "bosn-v-stack-b".into());
        assert_ne!(
            derived.container_name,
            setup_container_name(&plan, workspace.path(), &prepared(&plan)).unwrap()
        );
    }

    #[test]
    fn guest_image_storage_volume_requires_exact_explicit_attachment() {
        let workspace = tempfile::tempdir().unwrap();
        let plan = macos_guest_plan(workspace.path());
        let image = prepared(&plan);
        let expected = derive_creation(&plan, workspace.path(), &image).unwrap();
        let mut base = fixture_image();
        base["Config"]["Volumes"] = serde_json::json!({"/storage":{}});
        let mut observation = observed(&plan, false);
        observation.configuration["Config"]["Volumes"] = base["Config"]["Volumes"].clone();
        assert!(verify_actual_configuration(&observation, &expected, &base).is_ok());
        base["Config"]["Volumes"]["/anonymous"] = serde_json::json!({});
        observation.configuration["Config"]["Volumes"] = base["Config"]["Volumes"].clone();
        assert!(verify_actual_configuration(&observation, &expected, &base).is_err());
    }

    #[test]
    fn reuse_and_adoption_refuse_local_volume_bind_options_or_source_mismatch() {
        let workspace = tempfile::tempdir().unwrap();
        let plan = macos_guest_plan(workspace.path());
        let image = prepared(&plan);
        let volume = &plan.named_volumes[0];
        for bind_backed in [true, false] {
            let mut observation = observed(&plan, true);
            observation.configuration["Mounts"][0]["Source"] =
                serde_json::json!("/var/lib/docker/volumes/owned/_data");
            let receipt = serde_json::json!({"Name":volume.name,"Driver":"local","Labels":volume.labels,
                "Scope":"local","Mountpoint":if bind_backed { "/var/lib/docker/volumes/owned/_data" } else { "/foreign/path" },
                "Options":if bind_backed { serde_json::json!({"type":"none","o":"bind","device":"/foreign/path"}) } else { serde_json::Value::Null }});
            for adopt in [false, true] {
                let engine = FakeEngine::with_results([
                    Ok(SetupEnsureResponse::Inspection(
                        Some(observation.clone()),
                        result(0, [], []),
                    )),
                    command(serde_json::to_vec(&receipt).unwrap()),
                ]);
                let cancellation = CancellationSource::new();
                let options = RunOptions::streaming(Duration::from_secs(2), 8192);
                let result = if adopt {
                    run_adopt(
                        &engine,
                        &plan,
                        workspace.path(),
                        &image,
                        &cancellation.token(),
                        options,
                    )
                } else {
                    run(
                        &engine,
                        &plan,
                        workspace.path(),
                        &image,
                        &cancellation.token(),
                        options,
                    )
                };
                assert!(
                    matches!(result, Err(SetupEnsureError::OwnershipMismatch)),
                    "bind_backed={bind_backed} adopt={adopt}"
                );
                assert!(engine.calls.lock().unwrap().iter().all(|c| matches!(
                    c,
                    SetupEnsureCommand::Inspect { .. }
                        | SetupEnsureCommand::ImageInspect { .. }
                        | SetupEnsureCommand::VolumeInspect { .. }
                )));
            }
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
            tmpfs: Vec::new(),
            host_docker_socket: None,
            macos_guest: None,
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

    fn macos_guest_plan(workspace: &Path) -> SetupPlan {
        let mut plan = plan(workspace);
        let image = format!("dockurr/macos@sha256:{HASH}");
        plan.app.source = bosn_core::SetupSource::PinnedImage(image.clone());
        plan.app_source = SetupPlanAppSource::PinnedImage { image };
        plan.app.mounts.clear();
        plan.app.workdir = None;
        plan.app.command = None;
        let storage_volume = "bosn-v-machine-macos-storage".to_owned();
        plan.named_volumes = vec![crate::SetupNamedVolume {
            name: storage_volume.clone(),
            target: "/storage".into(),
            labels: BTreeMap::from([
                (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
                (LABEL_CONTENT_SHA256.into(), HASH.into()),
                (LABEL_CONTAINER_NAME.into(), storage_volume.clone()),
            ]),
        }];
        plan.macos_guest = Some(crate::SetupMacosGuest {
            ssh_port: 2222,
            web_port: 8006,
            version: "ventura".into(),
            ram_size: "8G".into(),
            disk_size: "128G".into(),
            cpu_cores: 1,
            storage_volume,
            storage_scope: Scope::Machine,
            storage_retention: Retention::Pinned,
        });
        plan
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
                [
                    SetupEnsureCommand::Inspect { .. },
                    SetupEnsureCommand::ImageInspect { .. }
                ]
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
    fn inspection_is_complete_bounded_json_and_legacy_or_duplicate_keys_refuse() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        let plan = plan(workspace.path());
        let observed = observed(&plan, true);
        assert_eq!(
            SetupEnsureCommand::Inspect {
                container_name: observed.labels[LABEL_CONTAINER_NAME].clone()
            }
            .docker_args()[3],
            "{{json .}}"
        );
        assert_eq!(
            parse_inspection(&serde_json::to_vec(&observed.configuration).unwrap()).unwrap(),
            observed
        );
        assert!(parse_inspection(b"legacy\tlabels\tonly").is_err());
        assert!(
            crate::creation::bounded_json(br#"{"Config":{"Env":[],"Env":["foreign"]}}"#).is_err()
        );
        assert!(
            crate::creation::bounded_json(&vec![b' '; crate::creation::MAX_OBSERVATION_BYTES + 1])
                .is_err()
        );
    }

    #[test]
    fn volume_inspect_requests_complete_private_metadata() {
        let volume_name = format!("bosn-v-stack-{HASH}");
        let command = SetupEnsureCommand::VolumeInspect {
            volume_name: volume_name.clone(),
        };
        let args = command.docker_args();
        assert_eq!(args[0], "volume");
        assert_eq!(args[1], "inspect");
        assert_eq!(args[2], "--format");
        assert_eq!(args[3], "{{json .}}");
        assert_eq!(args[4], volume_name);
    }

    #[test]
    fn typed_tmpfs_is_emitted_without_a_raw_option_channel() {
        let command = SetupEnsureCommand::Create {
            container_name: "bosn-setup-test".into(),
            image_identity: IDENTITY.into(),
            mounts: Vec::new(),
            volumes: Vec::new(),
            tmpfs: vec![SetupEnsureTmpfs {
                target: "/run/cache".into(),
                readonly: true,
                size: Some(crate::SetupTmpfsSize {
                    value: 64,
                    unit: crate::SetupTmpfsSizeUnit::Mebibytes,
                }),
                exec: None,
                mode: None,
            }],
            host_docker_socket: None,
            environment: BTreeMap::new(),
            workdir: None,
            command: None,
            labels: BTreeMap::new(),
            macos_guest: Box::new(None),
        };
        let args = command.docker_args();
        assert_eq!(
            args.windows(2)
                .find(|pair| pair[0] == "--tmpfs")
                .map(|pair| pair[1].as_str()),
            Some("/run/cache:ro,size=64m")
        );
    }

    #[test]
    fn typed_tmpfs_exec_mode_and_host_docker_socket_are_emitted_from_typed_fields() {
        let command = SetupEnsureCommand::Create {
            container_name: "bosn-setup-test".into(),
            image_identity: IDENTITY.into(),
            mounts: Vec::new(),
            volumes: Vec::new(),
            tmpfs: vec![SetupEnsureTmpfs {
                target: "/mount-probe".into(),
                readonly: false,
                size: None,
                exec: Some(true),
                mode: Some(0o1777),
            }],
            host_docker_socket: Some(crate::SetupHostDockerSocket {
                source: crate::SetupHostDockerSocketSource::VarRun,
                target: "/var/run/docker.sock".into(),
                readonly: false,
            }),
            environment: BTreeMap::new(),
            workdir: None,
            command: None,
            labels: BTreeMap::new(),
            macos_guest: Box::new(None),
        };
        let args = command.docker_args();
        assert_eq!(
            args.windows(2)
                .find(|pair| pair[0] == "--tmpfs")
                .map(|pair| pair[1].as_str()),
            Some("/mount-probe:exec,mode=1777")
        );
        assert!(args.windows(2).any(|pair| pair[0] == "--mount"
            && pair[1] == "type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock"));
        let noexec = SetupEnsureTmpfs {
            target: "/t".into(),
            readonly: true,
            size: None,
            exec: Some(false),
            mode: Some(0o700),
        };
        assert_eq!(tmpfs_docker_value(&noexec), "/t:ro,noexec,mode=700");
    }

    #[test]
    fn host_docker_socket_target_cannot_collide_or_enter_a_guest() {
        let temporary = tempfile::tempdir().unwrap();
        let mut plan = plan(temporary.path());
        plan.host_docker_socket = Some(crate::SetupHostDockerSocket {
            source: crate::SetupHostDockerSocketSource::Run,
            target: plan.app.mounts[0].target.clone(),
            readonly: false,
        });
        assert!(matches!(
            validate_plan_shape(&plan),
            Err(SetupEnsureError::InvalidRequest(
                "host Docker socket receipt was modified"
            ))
        ));
        plan.host_docker_socket = Some(crate::SetupHostDockerSocket {
            source: crate::SetupHostDockerSocketSource::Run,
            target: "/var/run/docker.sock".into(),
            readonly: false,
        });
        assert!(validate_plan_shape(&plan).is_ok());

        let mut guest = macos_guest_plan(temporary.path());
        assert!(validate_plan_shape(&guest).is_ok());
        guest.host_docker_socket = plan.host_docker_socket.clone();
        assert!(matches!(
            validate_plan_shape(&guest),
            Err(SetupEnsureError::InvalidRequest(
                "host Docker socket receipt was modified"
            ))
        ));
    }

    #[test]
    fn typed_macos_guest_emits_only_its_fixed_privileged_runtime_shape() {
        let command = SetupEnsureCommand::Create {
            container_name: "bosn-setup-test".into(),
            image_identity: IDENTITY.into(),
            mounts: Vec::new(),
            volumes: Vec::new(),
            tmpfs: Vec::new(),
            host_docker_socket: None,
            environment: BTreeMap::new(),
            workdir: None,
            command: Some("must-not-be-emitted".into()),
            labels: BTreeMap::new(),
            macos_guest: Box::new(Some(SetupEnsureMacosGuest {
                ssh_port: 2222,
                web_port: 8006,
                version: "ventura".into(),
                ram_size: "8G".into(),
                disk_size: "128G".into(),
                cpu_cores: 1,
            })),
        };
        let args = command.docker_args();
        for expected in [
            "/dev/kvm",
            "/dev/net/tun",
            "NET_ADMIN",
            "127.0.0.1:2222:22",
            "127.0.0.1:8006:8006",
            "VERSION=ventura",
            "RAM_SIZE=8G",
            "DISK_SIZE=128G",
            "CPU_CORES=1",
        ] {
            assert!(
                args.iter().any(|value| value == expected),
                "missing {expected}"
            );
        }
        assert!(!args.iter().any(|value| value == "must-not-be-emitted"));
    }

    #[test]
    fn macos_guest_plan_requires_trusted_image_and_exact_durable_storage_receipt() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let plan = macos_guest_plan(&workspace);
        assert!(validate_plan_shape(&plan).is_ok());

        let mut untrusted_image = plan.clone();
        let image = format!("registry.example/dockurr/macos@sha256:{HASH}");
        untrusted_image.app.source = bosn_core::SetupSource::PinnedImage(image.clone());
        untrusted_image.app_source = SetupPlanAppSource::PinnedImage { image };
        assert!(matches!(
            validate_plan_shape(&untrusted_image),
            Err(SetupEnsureError::InvalidRequest(
                "macOS guest receipt was modified"
            ))
        ));

        let mut missing_storage = plan.clone();
        missing_storage.named_volumes.clear();
        assert!(matches!(
            validate_plan_shape(&missing_storage),
            Err(SetupEnsureError::InvalidRequest(
                "macOS guest storage volume receipt was modified"
            ))
        ));

        let mut unsafe_storage = plan;
        unsafe_storage.macos_guest.as_mut().unwrap().storage_scope = Scope::Stack;
        assert!(matches!(
            validate_plan_shape(&unsafe_storage),
            Err(SetupEnsureError::InvalidRequest(
                "macOS guest storage volume receipt was modified"
            ))
        ));
    }

    #[test]
    fn tmpfs_target_cannot_collide_with_a_bind_or_be_modified() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        let mut plan = plan(&workspace);
        plan.tmpfs.push(crate::SetupTmpfs {
            target: "/workspace".into(),
            readonly: false,
            size: None,
            exec: None,
            mode: None,
        });
        let image = prepared(&plan);
        let engine = FakeEngine::with_results([]);
        let cancellation = CancellationSource::new();
        assert!(matches!(
            run(
                &engine,
                &plan,
                &workspace,
                &image,
                &cancellation.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096),
            ),
            Err(SetupEnsureError::InvalidRequest(
                "tmpfs receipt was modified"
            ))
        ));
        assert!(engine.calls.lock().unwrap().is_empty());
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
                container_name: setup_container_name(&plan, &workspace, &image).unwrap(),
                container_id: CONTAINER_ID.into(),
                image_identity: IDENTITY.into(),
                created: true,
                started: true,
                running: true,
            }
        );
        let calls = engine.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 5);
        assert!(
            matches!(&calls[0], SetupEnsureCommand::Inspect { container_name } if container_name == &setup_container_name(&plan, &workspace, &image).unwrap())
        );
        assert_eq!(
            calls[4],
            SetupEnsureCommand::Start {
                container_name: setup_container_name(&plan, &workspace, &image).unwrap()
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
                SetupEnsureCommand::ImageInspect { .. },
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
            [
                SetupEnsureCommand::Inspect { .. },
                SetupEnsureCommand::ImageInspect { .. }
            ]
        ));
    }

    #[test]
    fn matching_container_reuse_reinspects_volume_metadata_without_mutation() {
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
        let volume = &plan.named_volumes[0];
        let receipt = serde_json::json!({"Name":volume.name,"Driver":"local","Scope":"local","Options":null,"Labels":volume.labels,
            "Mountpoint":format!("/var/lib/docker/volumes/{}/_data",volume.name)});
        let engine = FakeEngine::with_results([
            inspection(&plan, true),
            command(serde_json::to_vec(&receipt).unwrap()),
        ]);

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
            [
                SetupEnsureCommand::Inspect { .. },
                SetupEnsureCommand::ImageInspect { .. },
                SetupEnsureCommand::VolumeInspect { .. }
            ]
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
                SetupEnsureCommand::ImageInspect { .. },
                SetupEnsureCommand::Start { .. }
            ]
        ));
    }

    /// Extracts the first ```toml fenced block from docs/macos-guest.md. Read at
    /// run time (not include_str!) so builds from a package without docs still compile.
    fn macos_guest_doc_example() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/macos-guest.md");
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let fence = "```toml\n";
        let (_, after) = doc.split_once(fence).expect("toml example");
        let (example, _) = after.split_once("```").expect("closed fence");
        example.to_owned()
    }

    /// Replaces a documentation placeholder such as `<64 lowercase hex>` in the
    /// digest position with a real-shaped digest, leaving a literal digest alone.
    fn substitute_doc_digest_placeholder(example: &str) -> String {
        let mut out = String::new();
        let mut rest = example;
        while let Some((before, after)) = rest.split_once("@sha256:<") {
            let (_, tail) = after.split_once('>').expect("placeholder is closed");
            out.push_str(before);
            out.push_str("@sha256:");
            out.push_str(HASH);
            rest = tail;
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn macos_guest_doc_example_image_is_accepted_by_runtime() {
        let raw = macos_guest_doc_example();
        let example = substitute_doc_digest_placeholder(&raw);
        let manifest = bosn_core::parse_manifest_toml(
            &example,
            bosn_core::ManifestRoots::new("docs/macos-guest.md", "assets", "workspace"),
        )
        .unwrap_or_else(|error| panic!("docs/macos-guest.md example must parse: {error:?}"));
        let stack = manifest.stack("macos-x64").expect("stack macos-x64");
        assert_eq!(stack.kind.as_deref(), Some("macos-x64-guest"));
        assert!(stack.acknowledge_macos_license);
        let image = stack.image.as_deref().expect("example declares an image");
        assert!(
            valid_macos_guest_image(image),
            "docs/macos-guest.md example image {image:?} is refused by valid_macos_guest_image"
        );
        let storage = stack
            .volumes
            .iter()
            .find(|volume| volume.name == "storage")
            .expect("example declares the storage volume");
        assert_eq!(storage.scope, Scope::Machine);
        assert_eq!(storage.destination.as_deref(), Some("/storage"));
        assert_eq!(storage.retention, Retention::Pinned);
    }

    #[test]
    fn macos_guest_doc_digest_substitution_only_fills_placeholders() {
        let placeholder = "image = \"dockurr/macos@sha256:<64 lowercase hex>\"";
        assert_eq!(
            substitute_doc_digest_placeholder(placeholder),
            format!("image = \"dockurr/macos@sha256:{HASH}\"")
        );
        let literal = format!("image = \"dockurr/macos@sha256:{HASH}\"");
        assert_eq!(substitute_doc_digest_placeholder(&literal), literal);
        // The substitution must not make a foreign registry acceptable.
        assert!(!valid_macos_guest_image(
            "ghcr.io/o/r/macos-x64-guest:ventura"
        ));
        assert!(!valid_macos_guest_image(&format!(
            "ghcr.io/o/r/macos-x64-guest@sha256:{HASH}"
        )));
    }
}
