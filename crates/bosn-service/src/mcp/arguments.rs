//! Typed parsing and validation of MCP tool arguments.

use super::*;

pub(crate) fn compose_plan_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<String, ToolFailure> {
    only_arguments(arguments, &["document"])?;
    let document = arguments
        .get("document")
        .and_then(Value::as_str)
        .ok_or(ToolFailure::Invalid("document must be a string"))?;
    if document.is_empty() || document.len() > MAX_MCP_COMPOSE_DOCUMENT_BYTES {
        return Err(ToolFailure::Invalid(
            "document must be within 1..=32768 bytes",
        ));
    }
    Ok(document.to_owned())
}

pub(crate) enum ToolFailure {
    Invalid(&'static str),
    Daemon,
    Setup,
}
impl ToolFailure {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Invalid(message) => message,
            Self::Daemon => "native Bosn daemon request failed",
            Self::Setup => "Bosn setup plan failed",
        }
    }
}

pub(crate) struct SetupPlanInput {
    pub(crate) workspace: PathBuf,
    pub(crate) locator: String,
    pub(crate) policy: SetupAcquirePolicy,
}

pub(crate) struct SetupPrepareInput {
    pub(crate) workspace: PathBuf,
    pub(crate) config: String,
    pub(crate) policy: SetupPreparePolicy,
    pub(crate) deadline_ms: u64,
    pub(crate) output_limit: usize,
}

pub(crate) struct SetupTaskInput {
    pub(crate) workspace: PathBuf,
    pub(crate) config: String,
    pub(crate) policy: SetupPreparePolicy,
    pub(crate) task_name: String,
    pub(crate) deadline_ms: u64,
    pub(crate) output_limit: usize,
}

pub(crate) fn setup_plan_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupPlanInput, ToolFailure> {
    only_arguments(arguments, &["workspace", "config", "policy"])?;
    let workspace = required_setup_string(arguments, "workspace")?;
    let locator = required_setup_string(arguments, "config")?;
    let policy = match required_setup_string(arguments, "policy")?.as_str() {
        "refresh" => SetupAcquirePolicy::OnlineRefresh,
        "offline" => SetupAcquirePolicy::OfflineCacheOnly,
        _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
    };
    Ok(SetupPlanInput {
        workspace: PathBuf::from(workspace),
        locator,
        policy,
    })
}

pub(crate) fn setup_prepare_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupPrepareRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "config",
            "policy",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let input = SetupPrepareInput {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        config: required_setup_string(arguments, "config")?,
        policy: match required_setup_string(arguments, "policy")?.as_str() {
            "refresh" => SetupPreparePolicy::Refresh,
            "offline" => SetupPreparePolicy::Offline,
            _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
        },
        deadline_ms: required_bounded_u64(arguments, "deadline_ms", 300_000)?,
        output_limit: required_bounded_u64(arguments, "output_limit", 8 * 1024 * 1024)? as usize,
    };
    Ok(SetupPrepareRequest {
        workspace: input.workspace,
        config: input.config,
        policy: input.policy,
        deadline: std::time::Duration::from_millis(input.deadline_ms),
        output_limit: input.output_limit,
    })
}

pub(crate) fn setup_ensure_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupEnsureJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "config",
            "policy",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let input = SetupPrepareInput {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        config: required_setup_string(arguments, "config")?,
        policy: match required_setup_string(arguments, "policy")?.as_str() {
            "refresh" => SetupPreparePolicy::Refresh,
            "offline" => SetupPreparePolicy::Offline,
            _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
        },
        deadline_ms: required_bounded_u64(arguments, "deadline_ms", 300_000)?,
        output_limit: required_bounded_u64(arguments, "output_limit", 8 * 1024 * 1024)? as usize,
    };
    Ok(SetupEnsureJobRequest {
        workspace: input.workspace,
        config: input.config,
        policy: input.policy,
        deadline: std::time::Duration::from_millis(input.deadline_ms),
        output_limit: input.output_limit,
    })
}

pub(crate) fn manifest_ensure_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<ManifestEnsureJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "manifest",
            "stack",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let manifest = required_setup_string(arguments, "manifest")?;
    if manifest.starts_with('/')
        || manifest.contains('\\')
        || manifest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ToolFailure::Invalid(
            "manifest must be a safe workspace-relative path",
        ));
    }
    let stack = required_setup_string(arguments, "stack")?;
    if stack.len() > 128
        || !stack
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ToolFailure::Invalid("manifest stack is invalid"));
    }
    Ok(ManifestEnsureJobRequest {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        manifest,
        stack,
        deadline: std::time::Duration::from_millis(required_bounded_u64(
            arguments,
            "deadline_ms",
            MANIFEST_MAX_DEADLINE.as_millis() as u64,
        )?),
        output_limit: required_bounded_u64(arguments, "output_limit", MANIFEST_MAX_OUTPUT as u64)?
            as usize,
    })
}
pub(crate) fn manifest_converge_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<ManifestConvergeJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &["workspace", "manifest", "deadline_ms", "output_limit"],
    )?;
    let manifest = required_setup_string(arguments, "manifest")?;
    if manifest.starts_with('/')
        || manifest.contains('\\')
        || manifest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ToolFailure::Invalid(
            "manifest must be a safe workspace-relative path",
        ));
    }
    Ok(ManifestConvergeJobRequest {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        manifest,
        deadline: std::time::Duration::from_millis(required_bounded_u64(
            arguments,
            "deadline_ms",
            MANIFEST_MAX_DEADLINE.as_millis() as u64,
        )?),
        output_limit: required_bounded_u64(arguments, "output_limit", MANIFEST_MAX_OUTPUT as u64)?
            as usize,
    })
}
pub(crate) fn manifest_app_task_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<ManifestAppTaskJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "manifest",
            "stack",
            "task_name",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let base = manifest_ensure_request(&{
        let mut copied = arguments.clone();
        copied.remove("task_name");
        copied
    })?;
    let task_name = required_setup_task_name(arguments)?;
    Ok(ManifestAppTaskJobRequest {
        workspace: base.workspace,
        manifest: base.manifest,
        stack: base.stack,
        task_name,
        deadline: base.deadline,
        output_limit: base.output_limit,
    })
}

pub(crate) fn setup_task_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupTaskJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "config",
            "policy",
            "task_name",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let input = SetupTaskInput {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        config: required_setup_string(arguments, "config")?,
        policy: match required_setup_string(arguments, "policy")?.as_str() {
            "refresh" => SetupPreparePolicy::Refresh,
            "offline" => SetupPreparePolicy::Offline,
            _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
        },
        task_name: required_setup_task_name(arguments)?,
        deadline_ms: required_bounded_u64(arguments, "deadline_ms", 300_000)?,
        output_limit: required_bounded_u64(arguments, "output_limit", 8 * 1024 * 1024)? as usize,
    };
    Ok(SetupTaskJobRequest {
        workspace: input.workspace,
        config: input.config,
        policy: input.policy,
        task_name: input.task_name,
        deadline: std::time::Duration::from_millis(input.deadline_ms),
        output_limit: input.output_limit,
    })
}

pub(crate) fn setup_app_task_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupAppTaskJobRequest, ToolFailure> {
    let task = setup_task_request(arguments)?;
    Ok(SetupAppTaskJobRequest {
        workspace: task.workspace,
        config: task.config,
        policy: task.policy,
        task_name: task.task_name,
        deadline: task.deadline,
        output_limit: task.output_limit,
    })
}

pub(crate) fn required_setup_string(
    arguments: &serde_json::Map<String, Value>,
    key: &'static str,
) -> Result<String, ToolFailure> {
    let value = arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or(ToolFailure::Invalid("setup arguments must be strings"))?;
    (!value.is_empty()
        && value.len() <= MAX_MCP_SETUP_STRING_BYTES
        && !value.bytes().any(|byte| byte == 0))
    .then_some(value.to_owned())
    .ok_or(ToolFailure::Invalid(
        "setup argument is empty or exceeds 8 KiB",
    ))
}

/// Keep task selection semantic at the MCP boundary.  This exact grammar is
/// also enforced by the authenticated daemon wire boundary: an alphanumeric
/// first byte followed by alphanumerics, underscores, or non-leading hyphens.
pub(crate) fn required_setup_task_name(
    arguments: &serde_json::Map<String, Value>,
) -> Result<String, ToolFailure> {
    let task_name = required_setup_string(arguments, "task_name")?;
    (task_name.len() <= 64
        && task_name.as_bytes()[0].is_ascii_alphanumeric()
        && task_name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        }))
    .then_some(task_name)
    .ok_or(ToolFailure::Invalid(
        "task_name is not a declared-task name",
    ))
}

pub(crate) fn required_bounded_u64(
    arguments: &serde_json::Map<String, Value>,
    key: &'static str,
    maximum: u64,
) -> Result<u64, ToolFailure> {
    let value = arguments
        .get(key)
        .and_then(Value::as_u64)
        .ok_or(ToolFailure::Invalid(
            "setup bounds must be positive integers",
        ))?;
    (value > 0 && value <= maximum)
        .then_some(value)
        .ok_or(ToolFailure::Invalid(
            "setup bound is outside its allowed range",
        ))
}

pub(crate) fn only_arguments(
    arguments: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), ToolFailure> {
    arguments
        .keys()
        .all(|key| allowed.contains(&key.as_str()))
        .then_some(())
        .ok_or(ToolFailure::Invalid("unsupported tool argument"))
}

pub(crate) fn job_id(arguments: &serde_json::Map<String, Value>) -> Result<u64, ToolFailure> {
    let id = optional_u64(arguments, "job_id", 0)?;
    (id > 0)
        .then_some(id)
        .ok_or(ToolFailure::Invalid("job_id must be a positive integer"))
}

pub(crate) fn optional_u64(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
    default: u64,
) -> Result<u64, ToolFailure> {
    match arguments.get(key) {
        None => Ok(default),
        Some(value) => value.as_u64().ok_or(ToolFailure::Invalid(
            "numeric arguments must be unsigned integers",
        )),
    }
}

pub(crate) fn registry_page_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<(u64, u32), ToolFailure> {
    only_arguments(arguments, &["after", "limit"])?;
    let after = optional_u64(arguments, "after", 0)?;
    let limit = optional_u64(arguments, "limit", u64::from(MAX_MCP_REGISTRY_RECORDS))?;
    (limit > 0 && limit <= u64::from(MAX_MCP_REGISTRY_RECORDS))
        .then_some((after, limit as u32))
        .ok_or(ToolFailure::Invalid("limit must be within 1..=64"))
}
pub(crate) fn setup_gc_preview_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<(PathBuf, u64, u32), ToolFailure> {
    only_arguments(arguments, &["workspace", "after", "limit"])?;
    let workspace = arguments
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_MCP_SETUP_STRING_BYTES
                && !value.bytes().any(|byte| byte == 0)
        })
        .map(PathBuf::from)
        .ok_or(ToolFailure::Invalid(
            "workspace must be a non-empty bounded string",
        ))?;
    let after = optional_u64(arguments, "after", 0)?;
    let limit = optional_u64(arguments, "limit", u64::from(MAX_MCP_REGISTRY_RECORDS))?;
    if limit == 0 || limit > u64::from(MAX_MCP_REGISTRY_RECORDS) {
        return Err(ToolFailure::Invalid("limit must be within 1..=64"));
    }
    let limit = limit as u32;
    Ok((workspace, after, limit))
}
pub(crate) fn setup_gc_apply_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<(PathBuf, String), ToolFailure> {
    only_arguments(arguments, &["workspace", "candidate_token", "confirm"])?;
    let workspace = arguments
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_MCP_SETUP_STRING_BYTES
                && !value.bytes().any(|byte| byte == 0)
        })
        .map(PathBuf::from)
        .ok_or(ToolFailure::Invalid(
            "workspace must be a non-empty bounded string",
        ))?;
    let token = arguments
        .get("candidate_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 24 * 1024)
        .map(str::to_owned)
        .ok_or(ToolFailure::Invalid(
            "candidate_token must be bounded string",
        ))?;
    if arguments.get("confirm") != Some(&Value::Bool(true)) {
        return Err(ToolFailure::Invalid("confirm must be true"));
    }
    Ok((workspace, token))
}
pub(crate) fn setup_done_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<PathBuf, ToolFailure> {
    only_arguments(arguments, &["workspace", "confirm"])?;
    if arguments.get("confirm") != Some(&Value::Bool(true)) {
        return Err(ToolFailure::Invalid("confirm must be true"));
    }
    arguments
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_MCP_SETUP_STRING_BYTES
                && !value.bytes().any(|byte| byte == 0)
        })
        .map(PathBuf::from)
        .ok_or(ToolFailure::Invalid(
            "workspace must be a non-empty bounded string",
        ))
}
pub(crate) fn setup_adopt_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupAdoptRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "config",
            "policy",
            "deadline_ms",
            "output_limit",
            "confirm",
        ],
    )?;
    if arguments.get("confirm") != Some(&Value::Bool(true)) {
        return Err(ToolFailure::Invalid("confirm must be true"));
    }
    let workspace = arguments
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|v| {
            !v.is_empty() && v.len() <= MAX_MCP_SETUP_STRING_BYTES && !v.bytes().any(|b| b == 0)
        })
        .map(PathBuf::from)
        .ok_or(ToolFailure::Invalid(
            "workspace must be a non-empty bounded string",
        ))?;
    let config = arguments
        .get("config")
        .and_then(Value::as_str)
        .filter(|v| {
            !v.is_empty() && v.len() <= MAX_MCP_SETUP_STRING_BYTES && !v.bytes().any(|b| b == 0)
        })
        .map(str::to_owned)
        .ok_or(ToolFailure::Invalid(
            "config must be a non-empty bounded string",
        ))?;
    let policy = match arguments.get("policy").and_then(Value::as_str) {
        Some("refresh") => SetupPreparePolicy::Refresh,
        Some("offline") => SetupPreparePolicy::Offline,
        _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
    };
    let deadline = required_bounded_u64(arguments, "deadline_ms", 300000)?;
    let output = required_bounded_u64(arguments, "output_limit", 8388608)?;
    Ok(SetupAdoptRequest {
        workspace,
        config,
        policy,
        deadline: std::time::Duration::from_millis(deadline),
        output_limit: output as usize,
        confirm: true,
    })
}
