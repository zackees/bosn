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
mod checks;
mod derive;
use checks::*;
use derive::*;
pub use derive::{setup_container_name, verify_setup_observation};

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
        host_docker_socket: Box<Option<crate::SetupHostDockerSocket>>,
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
    #[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
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
                // Request the namespaces verification requires (#561): a daemon's own
                // defaults are `host` cgroupns on cgroup v1 and may be `shareable` IPC.
                let mut args = vec![
                    "container".into(),
                    "create".into(),
                    "--name".into(),
                    container_name.clone(),
                    "--cgroupns".into(),
                    "private".into(),
                    "--ipc".into(),
                    "private".into(),
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
                if let Some(socket) = host_docker_socket.as_ref() {
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
                    if let Some(dir) = &socket.proxy_dir {
                        args.push("--mount".into());
                        args.push(format!("type=bind,src={dir},dst={dir}"));
                    }
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
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
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
        } else if result.reports_missing() {
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

#[cfg(test)]
mod tests;
