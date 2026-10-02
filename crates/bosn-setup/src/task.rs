//! Typed execution of one task declared by a validated setup document.
//!
//! This is deliberately a primitive, not setup apply or lifecycle management.
//! It turns one declaration retained in [`crate::SetupPlan`] into a finite
//! semantic command and streams it through the engine.  It cannot accept
//! caller-supplied Docker arguments, container names, volumes, labels,
//! networks, privileges, or state paths.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use bosn_core::{MAX_ENVIRONMENT_ENTRIES, SetupTask};
use bosn_engine::{CommandError, CommandResult, DockerEngine, EngineEvent, RunOptions};
use kernal_api::{
    async_engine::{CancellationToken, Deadline, Sender},
    platform::fs,
};

use crate::{PreparedImage, PreparedImageKind, SetupPlan, SetupPlanAppSource};

/// All typed inputs for executing one declared setup task.
///
/// `workspace_root` is an already-resolved host directory.  It is rechecked
/// against the plan's canonical root before mounts are derived, so a caller
/// cannot redirect a document-relative mount to a different workspace.
#[derive(Debug)]
pub struct SetupTaskRequest<'a> {
    pub plan: &'a SetupPlan,
    pub workspace_root: PathBuf,
    pub task_name: String,
    pub prepared_image: &'a PreparedImage,
    pub options: RunOptions,
    pub cancellation: &'a CancellationToken,
    pub events: &'a Sender<EngineEvent>,
}

/// One host workspace mount selected exclusively from the setup document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupTaskMount {
    /// Canonical existing path below the request's canonical workspace root.
    pub source: PathBuf,
    /// Normalized, absolute path inside the container.
    pub target: String,
    pub readonly: bool,
}

/// The only engine command a setup task executor may issue.
///
/// The shell text is the command already declared in the validated document;
/// it is not supplied by the operation caller.  The Docker adapter converts it
/// only to `docker run --rm ... IMAGE sh -lc COMMAND`, wrapped so the image's
/// `ENV PATH` survives the login profile (see `crate::shell`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupTaskCommand {
    Run {
        image_identity: String,
        mounts: Vec<SetupTaskMount>,
        environment: BTreeMap<String, String>,
        workdir: Option<String>,
        command: String,
    },
}

/// The only engine command used for a declared task inside the already
/// ensured setup application.  The target name is content-addressed from the
/// validated plan; callers cannot choose a container ID or Docker arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupAppTaskCommand {
    Exec {
        container_name: String,
        /// Names forwarded as `--env NAME` (no `=value`): Docker copies each
        /// value from the Docker client's own process environment, so a
        /// secret never appears in argv (#308). Values never enter this crate.
        passthrough_env: Vec<String>,
        command: String,
    },
}

impl SetupAppTaskCommand {
    fn docker_args(&self) -> Vec<String> {
        match self {
            Self::Exec {
                container_name,
                passthrough_env,
                command,
            } => {
                let mut args = vec!["container".into(), "exec".into()];
                for name in passthrough_env {
                    args.push("--env".into());
                    args.push(name.clone());
                }
                args.push(container_name.clone());
                args.extend(crate::shell::login_shell_args(command));
                args
            }
        }
    }
}

impl SetupTaskCommand {
    fn docker_args(&self) -> Vec<String> {
        match self {
            Self::Run {
                image_identity,
                mounts,
                environment,
                workdir,
                command,
            } => {
                let mut args = vec!["run".into(), "--rm".into()];
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
                for (key, value) in environment {
                    args.push("--env".into());
                    args.push(format!("{key}={value}"));
                }
                if let Some(workdir) = workdir {
                    args.push("--workdir".into());
                    args.push(workdir.clone());
                }
                args.push(image_identity.clone());
                args.extend(crate::shell::login_shell_args(command));
                args
            }
        }
    }
}

/// Testable engine boundary for one setup task.
///
/// Fakes receive only [`SetupTaskCommand`], never generic Docker argv.  The
/// production adapter is the sole conversion to a Docker invocation.
pub trait SetupTaskEngine {
    type StreamFuture<'a>: Future<Output = Result<CommandResult, CommandError>> + Send + 'a
    where
        Self: 'a;

    fn stream<'a>(
        &'a self,
        command: SetupTaskCommand,
        options: RunOptions,
        cancellation: &'a CancellationToken,
        events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a>;
}

/// Testable engine boundary for one declared task in the setup app.  This
/// operation intentionally has no inspection or lifecycle command: callers
/// must prove exact app ownership with `adopt_setup_app` before reaching it.
pub trait SetupAppTaskEngine {
    type StreamFuture<'a>: Future<Output = Result<CommandResult, CommandError>> + Send + 'a
    where
        Self: 'a;

    fn stream<'a>(
        &'a self,
        command: SetupAppTaskCommand,
        options: RunOptions,
        cancellation: &'a CancellationToken,
        events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a>;
}

impl SetupTaskEngine for DockerEngine {
    type StreamFuture<'a> =
        Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>>;

    fn stream<'a>(
        &'a self,
        command: SetupTaskCommand,
        options: RunOptions,
        cancellation: &'a CancellationToken,
        events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a> {
        let engine = self.with_args(command.docker_args());
        Box::pin(async move { engine.stream(options, Some(cancellation), events).await })
    }
}

impl SetupAppTaskEngine for DockerEngine {
    type StreamFuture<'a> =
        Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>>;

    fn stream<'a>(
        &'a self,
        command: SetupAppTaskCommand,
        options: RunOptions,
        cancellation: &'a CancellationToken,
        events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a> {
        let engine = self.with_args(command.docker_args());
        Box::pin(async move { engine.stream(options, Some(cancellation), events).await })
    }
}

/// The terminal receipt for a task command that exited successfully.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupTaskResult {
    pub task_name: String,
    pub image_identity: String,
    pub exit_code: i32,
}

/// All typed inputs for executing one declared task inside the ensured setup
/// application. Ownership is deliberately not inferred here: the daemon must
/// perform a fresh `adopt_setup_app` inspection immediately before this call.
#[derive(Debug)]
pub struct SetupAppTaskRequest<'a> {
    pub plan: &'a SetupPlan,
    pub workspace_root: PathBuf,
    pub task_name: String,
    /// Environment variable names to forward from the Docker client process
    /// environment. Only names, never values; see [`SetupAppTaskCommand`].
    pub passthrough_env: Vec<String>,
    pub prepared_image: &'a PreparedImage,
    pub options: RunOptions,
    pub cancellation: &'a CancellationToken,
    pub events: &'a Sender<EngineEvent>,
}

/// Why a single setup task was refused or did not complete successfully.
#[derive(Debug)]
pub enum SetupTaskError {
    InvalidRequest(&'static str),
    UnknownTask,
    Cancelled,
    Deadline,
    Transport(CommandError),
    TaskFailed { exit_code: i32, detail: String },
}

impl std::fmt::Display for SetupTaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(reason) => {
                write!(formatter, "invalid setup task request: {reason}")
            }
            Self::UnknownTask => formatter.write_str("setup task is not declared by this plan"),
            Self::Cancelled => formatter.write_str("setup task was cancelled"),
            Self::Deadline => formatter.write_str("setup task exceeded its deadline"),
            Self::Transport(error) => write!(formatter, "setup task engine transport: {error}"),
            Self::TaskFailed { exit_code, detail } => {
                write!(
                    formatter,
                    "declared setup task exited with {exit_code}: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for SetupTaskError {}

impl From<CommandError> for SetupTaskError {
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::Cancelled { .. } => Self::Cancelled,
            CommandError::Deadline { .. } => Self::Deadline,
            other => Self::Transport(other),
        }
    }
}

/// Execute exactly one named task retained in `request.plan`.
///
/// The supplied image receipt must be for this exact plan and must identify the
/// application image that preparation observed.  The full RunOptions budget is
/// applied to this one bounded engine call; cancellation is checked before the
/// engine is reached and forwarded to it unchanged.
pub async fn execute_setup_task<E: SetupTaskEngine>(
    engine: &E,
    request: SetupTaskRequest<'_>,
) -> Result<SetupTaskResult, SetupTaskError> {
    let command = derive_command(&request)?;
    if request.cancellation.is_cancelled() {
        return Err(SetupTaskError::Cancelled);
    }
    if request.options.deadline.is_zero() {
        return Err(SetupTaskError::Deadline);
    }
    if request.options.output_limit == 0 {
        return Err(SetupTaskError::InvalidRequest("output budget is zero"));
    }
    let deadline = Deadline::after(request.options.deadline);
    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err(SetupTaskError::Deadline);
    }
    let result = engine
        .stream(
            command,
            RunOptions::streaming(remaining, request.options.output_limit),
            request.cancellation,
            request.events,
        )
        .await?;
    let used = result.stdout.len().saturating_add(result.stderr.len());
    if used > request.options.output_limit {
        return Err(SetupTaskError::Transport(CommandError::OutputLimit {
            limit: request.options.output_limit,
            reaped_pid: None,
            cleanup: None,
        }));
    }
    if !result.ok() {
        return Err(SetupTaskError::TaskFailed {
            exit_code: result.exit_code,
            detail: failure_detail(&result),
        });
    }
    Ok(SetupTaskResult {
        task_name: request.task_name,
        image_identity: request.prepared_image.observed_identity.clone(),
        exit_code: result.exit_code,
    })
}

/// Execute exactly one named task in the deterministic setup application.
///
/// This function derives only `docker container exec NAME sh -lc COMMAND`
/// (wrapped so the image's `ENV PATH` survives the login profile; see
/// `crate::shell`).
/// It never accepts a container identity, raw argv, mounts, environment, or
/// working-directory override. A killed local `docker exec` client does not
/// prove the remote command stopped; callers must retain that uncertainty in
/// their lifecycle result.
pub async fn execute_setup_app_task<E: SetupAppTaskEngine>(
    engine: &E,
    request: SetupAppTaskRequest<'_>,
) -> Result<SetupTaskResult, SetupTaskError> {
    let command = derive_command(&SetupTaskRequest {
        plan: request.plan,
        workspace_root: request.workspace_root.clone(),
        task_name: request.task_name.clone(),
        prepared_image: request.prepared_image,
        options: request.options,
        cancellation: request.cancellation,
        events: request.events,
    })?;
    if request.passthrough_env.iter().any(|name| {
        name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    }) {
        return Err(SetupTaskError::InvalidRequest(
            "passthrough environment name is invalid",
        ));
    }
    let SetupTaskCommand::Run {
        image_identity,
        command,
        ..
    } = command;
    if request.cancellation.is_cancelled() {
        return Err(SetupTaskError::Cancelled);
    }
    if request.options.deadline.is_zero() {
        return Err(SetupTaskError::Deadline);
    }
    if request.options.output_limit == 0 {
        return Err(SetupTaskError::InvalidRequest("output budget is zero"));
    }
    let deadline = Deadline::after(request.options.deadline);
    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err(SetupTaskError::Deadline);
    }
    let result = engine
        .stream(
            SetupAppTaskCommand::Exec {
                container_name: format!("bosn-setup-{}", request.plan.content_sha256),
                passthrough_env: request.passthrough_env,
                command,
            },
            RunOptions::streaming(remaining, request.options.output_limit),
            request.cancellation,
            request.events,
        )
        .await?;
    let used = result.stdout.len().saturating_add(result.stderr.len());
    if used > request.options.output_limit {
        return Err(SetupTaskError::Transport(CommandError::OutputLimit {
            limit: request.options.output_limit,
            reaped_pid: None,
            cleanup: None,
        }));
    }
    if !result.ok() {
        return Err(SetupTaskError::TaskFailed {
            exit_code: result.exit_code,
            detail: failure_detail(&result),
        });
    }
    Ok(SetupTaskResult {
        task_name: request.task_name,
        image_identity,
        exit_code: result.exit_code,
    })
}

fn derive_command(request: &SetupTaskRequest<'_>) -> Result<SetupTaskCommand, SetupTaskError> {
    validate_plan_shape(request.plan)?;
    let workspace_root = canonical_workspace(&request.workspace_root)?;
    if workspace_root != request.plan.workspace_root {
        return Err(SetupTaskError::InvalidRequest(
            "workspace is not the plan's canonical workspace root",
        ));
    }
    validate_prepared_image(request.plan, request.prepared_image)?;
    validate_task_name(&request.task_name)?;
    let task = request
        .plan
        .tasks
        .get(&request.task_name)
        .ok_or(SetupTaskError::UnknownTask)?;
    validate_task(task)?;
    let mounts = derive_mounts(&workspace_root, request.plan)?;
    let environment = merged_environment(&request.plan.app.environment, &task.environment)?;
    let selected_workdir = task
        .workdir
        .as_deref()
        .or(request.plan.app.workdir.as_deref());
    let workdir = selected_workdir
        .map(|value| resolve_workdir(value, &request.plan.app.mounts, &workspace_root))
        .transpose()?;
    Ok(SetupTaskCommand::Run {
        image_identity: request.prepared_image.observed_identity.clone(),
        mounts,
        environment,
        workdir,
        command: task.command.clone(),
    })
}

fn validate_plan_shape(plan: &SetupPlan) -> Result<(), SetupTaskError> {
    if plan.macos_guest.is_some() {
        return Err(SetupTaskError::InvalidRequest(
            "macOS guests require a bounded SSH task transport",
        ));
    }
    if !valid_hash(&plan.content_sha256) {
        return Err(SetupTaskError::InvalidRequest(
            "plan content hash is not canonical",
        ));
    }
    let names: Vec<_> = plan.tasks.keys().cloned().collect();
    if plan.task_names != names {
        return Err(SetupTaskError::InvalidRequest(
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
            return Err(SetupTaskError::InvalidRequest(
                "plan application receipt was modified",
            ));
        }
    }
    validate_workspace_relative(plan.app.workdir.as_deref())?;
    for mount in &plan.app.mounts {
        validate_workspace_relative(Some(&mount.source))?;
        validate_container_path(&mount.target)?;
    }
    Ok(())
}

fn validate_prepared_image(plan: &SetupPlan, image: &PreparedImage) -> Result<(), SetupTaskError> {
    if image.setup_content_sha256 != plan.content_sha256
        || !valid_identity(&image.observed_identity)
    {
        return Err(SetupTaskError::InvalidRequest(
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
        _ => Err(SetupTaskError::InvalidRequest(
            "prepared image is not for the planned application",
        )),
    }
}

fn derive_mounts(
    workspace_root: &Path,
    plan: &SetupPlan,
) -> Result<Vec<SetupTaskMount>, SetupTaskError> {
    let mut targets = BTreeSet::new();
    plan.app
        .mounts
        .iter()
        .map(|mount| {
            if !targets.insert(mount.target.clone()) {
                return Err(SetupTaskError::InvalidRequest(
                    "duplicate container mount target",
                ));
            }
            let source = canonical_workspace_member(workspace_root, &mount.source)?;
            let source = source
                .to_str()
                .ok_or(SetupTaskError::InvalidRequest("mount source is not UTF-8"))?;
            if source.contains(',') || mount.target.contains(',') {
                return Err(SetupTaskError::InvalidRequest(
                    "mount path cannot be represented safely by Docker",
                ));
            }
            Ok(SetupTaskMount {
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
) -> Result<String, SetupTaskError> {
    validate_workspace_relative(Some(workdir))?;
    let selected = mounts
        .iter()
        .filter(|mount| workspace_prefix(workdir, &mount.source))
        .max_by_key(|mount| mount.source.len())
        .ok_or(SetupTaskError::InvalidRequest(
            "declared workdir is not covered by a declared workspace mount",
        ))?;
    let suffix =
        relative_suffix(workdir, &selected.source).expect("workspace_prefix selected this mount");
    // Recheck the source at application time. The semantic workdir contract is
    // a directory bind, not merely a path that existed when its plan was read.
    let source = canonical_workspace_member(workspace_root, &selected.source)?;
    let metadata = fs::context_path_metadata_no_follow(&source).map_err(|_| {
        SetupTaskError::InvalidRequest("declared workdir mount source does not exist")
    })?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err(SetupTaskError::InvalidRequest(
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

fn merged_environment(
    app: &BTreeMap<String, String>,
    task: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, SetupTaskError> {
    let mut merged = app.clone();
    merged.extend(task.iter().map(|(key, value)| (key.clone(), value.clone())));
    if merged.len() > MAX_ENVIRONMENT_ENTRIES {
        return Err(SetupTaskError::InvalidRequest(
            "merged task environment exceeds the entry limit",
        ));
    }
    if merged.iter().any(|(key, value)| {
        !valid_environment_name(key) || value.len() > 16 * 1024 || value.contains('\0')
    }) {
        return Err(SetupTaskError::InvalidRequest(
            "task environment is not safe document data",
        ));
    }
    Ok(merged)
}

fn validate_task(task: &SetupTask) -> Result<(), SetupTaskError> {
    if task.command.is_empty() || task.command.len() > 16 * 1024 || task.command.contains('\0') {
        return Err(SetupTaskError::InvalidRequest(
            "declared task command is invalid",
        ));
    }
    validate_workspace_relative(task.workdir.as_deref())?;
    Ok(())
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, SetupTaskError> {
    if !path.is_absolute() {
        return Err(SetupTaskError::InvalidRequest("workspace is not absolute"));
    }
    let metadata = fs::context_path_metadata_no_follow(path)
        .map_err(|_| SetupTaskError::InvalidRequest("workspace is not a directory"))?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err(SetupTaskError::InvalidRequest(
            "workspace is not a directory",
        ));
    }
    let canonical = fs::canonical_context_path(path)
        .map_err(|_| SetupTaskError::InvalidRequest("workspace cannot be canonicalized"))?;
    let canonical_metadata = fs::context_path_metadata_no_follow(&canonical)
        .map_err(|_| SetupTaskError::InvalidRequest("workspace is not a directory"))?;
    (canonical_metadata.kind == fs::ContextPathKind::Directory)
        .then_some(canonical)
        .ok_or(SetupTaskError::InvalidRequest(
            "workspace is not a directory",
        ))
}

fn canonical_workspace_member(root: &Path, relative: &str) -> Result<PathBuf, SetupTaskError> {
    validate_workspace_relative(Some(relative))?;
    let candidate = root.join(relative);
    let metadata = fs::context_path_metadata_no_follow(&candidate)
        .map_err(|_| SetupTaskError::InvalidRequest("declared mount source does not exist"))?;
    if metadata.kind == fs::ContextPathKind::Symlink {
        return Err(SetupTaskError::InvalidRequest(
            "declared mount source is a symlink",
        ));
    }
    let canonical = fs::canonical_context_path(&candidate)
        .map_err(|_| SetupTaskError::InvalidRequest("declared mount source cannot be resolved"))?;
    if !canonical.starts_with(root) {
        return Err(SetupTaskError::InvalidRequest(
            "declared mount source escapes workspace",
        ));
    }
    Ok(canonical)
}

fn validate_workspace_relative(value: Option<&str>) -> Result<(), SetupTaskError> {
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
        return Err(SetupTaskError::InvalidRequest(
            "workspace-relative path is invalid",
        ));
    }
    Ok(())
}

fn validate_container_path(value: &str) -> Result<(), SetupTaskError> {
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
        return Err(SetupTaskError::InvalidRequest(
            "container path is not normalized absolute",
        ));
    }
    Ok(())
}

fn validate_task_name(value: &str) -> Result<(), SetupTaskError> {
    if value.is_empty()
        || value.len() > 64
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        })
    {
        return Err(SetupTaskError::InvalidRequest("task name is invalid"));
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
mod tests;
