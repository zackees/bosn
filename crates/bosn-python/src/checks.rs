//! Input validation, redaction and error mapping at the Python boundary.

use super::*;

pub(crate) fn parse_setup_policy(policy: &str) -> PyResult<SetupAcquirePolicy> {
    match policy {
        "online_refresh" => Ok(SetupAcquirePolicy::OnlineRefresh),
        "offline_cache_only" => Ok(SetupAcquirePolicy::OfflineCacheOnly),
        _ => Err(PyValueError::new_err(
            "policy must be 'online_refresh' or 'offline_cache_only'",
        )),
    }
}

pub(crate) fn parse_prepare_policy(policy: &str) -> PyResult<SetupPreparePolicy> {
    match policy {
        "online_refresh" => Ok(SetupPreparePolicy::Refresh),
        "offline_cache_only" => Ok(SetupPreparePolicy::Offline),
        _ => Err(PyValueError::new_err(
            "policy must be 'online_refresh' or 'offline_cache_only'",
        )),
    }
}

pub(crate) fn validate_setup_prepare_input(
    workspace: &Path,
    config_locator: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> PyResult<()> {
    let workspace = workspace
        .to_str()
        .ok_or_else(|| PyValueError::new_err("workspace must be valid UTF-8"))?;
    if workspace.is_empty() || workspace.len() > 8 * 1024 || workspace.bytes().any(|byte| byte == 0)
    {
        return Err(PyValueError::new_err("workspace is empty or invalid"));
    }
    // This is pure core parsing only: it rejects non-HTTPS remote locators,
    // userinfo, fragments, whitespace, and oversized values before IPC.
    // Do not echo the caller's locator, which may contain credentials.
    parse_setup_config_locator(config_locator)
        .map_err(|_| PyValueError::new_err("setup config locator is invalid"))?;
    if deadline_ms == 0 || deadline_ms > MAX_SETUP_PREPARE_DEADLINE_MS {
        return Err(PyValueError::new_err(
            "deadline_ms must be between 1 and 300000",
        ));
    }
    if output_limit == 0 || output_limit > MAX_SETUP_PREPARE_OUTPUT_BYTES {
        return Err(PyValueError::new_err(
            "output_limit must be between 1 and 8388608",
        ));
    }
    Ok(())
}

/// Keep this syntactic check identical to the daemon wire validator and the
/// setup-document schema. Existence in the document remains a daemon-owned
/// execution concern, so the Python boundary never parses or runs task data.
pub(crate) fn validate_setup_task_input(
    workspace: &Path,
    config_locator: &str,
    task_name: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> PyResult<()> {
    validate_setup_prepare_input(workspace, config_locator, deadline_ms, output_limit)?;
    if task_name.is_empty()
        || task_name.len() > 64
        || !task_name.as_bytes()[0].is_ascii_alphanumeric()
        || !task_name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        })
    {
        return Err(PyValueError::new_err("task_name is invalid"));
    }
    Ok(())
}

pub(crate) fn validate_manifest_ensure_input(
    workspace: &Path,
    manifest: &str,
    stack: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> PyResult<()> {
    let workspace = workspace
        .to_str()
        .ok_or_else(|| PyValueError::new_err("workspace must be valid UTF-8"))?;
    if workspace.is_empty() || workspace.len() > 8 * 1024 || workspace.bytes().any(|byte| byte == 0)
    {
        return Err(PyValueError::new_err("workspace is empty or invalid"));
    }
    if manifest.is_empty()
        || manifest.len() > 4096
        || manifest.contains('\0')
        || manifest.starts_with('/')
        || manifest.contains('\\')
        || manifest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(PyValueError::new_err(
            "manifest must be a safe workspace-relative path",
        ));
    }
    if stack.is_empty()
        || stack.len() > 128
        || !stack
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(PyValueError::new_err("stack is invalid"));
    }
    if deadline_ms == 0 || deadline_ms > MAX_SETUP_PREPARE_DEADLINE_MS {
        return Err(PyValueError::new_err(
            "deadline_ms must be between 1 and 300000",
        ));
    }
    if output_limit == 0 || output_limit > MAX_SETUP_PREPARE_OUTPUT_BYTES {
        return Err(PyValueError::new_err(
            "output_limit must be between 1 and 8388608",
        ));
    }
    Ok(())
}

pub(crate) fn validate_manifest_converge_input(
    workspace: &Path,
    manifest: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> PyResult<()> {
    // Keep the shared workspace/path/budget grammar precisely aligned with
    // named ensure. `all` is only an internal syntactic stand-in; no stack
    // selector crosses this Python API.
    validate_manifest_ensure_input(workspace, manifest, "all", deadline_ms, output_limit)
}

pub(crate) fn validate_manifest_task_name(task_name: &str) -> PyResult<()> {
    if task_name.is_empty()
        || task_name.len() > 64
        || !task_name.as_bytes()[0].is_ascii_alphanumeric()
        || !task_name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        })
    {
        return Err(PyValueError::new_err("task_name is invalid"));
    }
    Ok(())
}

pub(crate) fn validate_job_id(job_id: u64) -> PyResult<()> {
    if job_id == 0 {
        return Err(PyValueError::new_err("job_id must be positive"));
    }
    Ok(())
}

pub(crate) fn validate_registry_page(_after: u64, limit: u32) -> PyResult<()> {
    if limit == 0 || limit > MAX_REGISTRY_RECORDS {
        return Err(PyValueError::new_err("limit must be between 1 and 64"));
    }
    Ok(())
}

/// Remove common URL credentials and credential-bearing query values before a
/// daemon diagnostic crosses the Python boundary. Service protocol failures do
/// not include request text, but job logs originate with external tools and
/// are therefore handled defensively here as well.
pub(crate) fn redact_diagnostic(value: &str) -> String {
    let mut redacted = value.to_owned();
    for scheme in ["https://", "http://"] {
        let mut search_from = 0;
        while let Some(relative) = redacted[search_from..].find(scheme) {
            let start = search_from + relative + scheme.len();
            let end = redacted[start..]
                .find(|character: char| {
                    character.is_whitespace() || character == '/' || character == '?'
                })
                .map(|offset| start + offset)
                .unwrap_or(redacted.len());
            if let Some(at) = redacted[start..end].find('@') {
                let at = start + at;
                redacted.replace_range(start..=at, "[redacted]@");
                search_from = start + "[redacted]@".len();
            } else {
                search_from = end;
            }
        }
    }
    for key in [
        "token",
        "access_token",
        "password",
        "secret",
        "api_key",
        "apikey",
        "authorization",
    ] {
        let needle = format!("{key}=");
        let mut search_from = 0;
        while let Some(relative) = redacted[search_from..].to_ascii_lowercase().find(&needle) {
            let start = search_from + relative + needle.len();
            let end = redacted[start..]
                .find(|character: char| {
                    character == '&'
                        || character.is_whitespace()
                        || character == '"'
                        || character == '\''
                })
                .map(|offset| start + offset)
                .unwrap_or(redacted.len());
            redacted.replace_range(start..end, "[redacted]");
            search_from = start + "[redacted]".len();
        }
    }
    redacted
}

pub(crate) fn ci_call(state_dir: &Path, name: &str, arguments: &str) -> Result<String, String> {
    let arguments: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))?;
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let client = bosn_service::Client::for_state(state_dir).map_err(|e| e.to_string())?;
    let mut backend = bosn_service::ci::mcp::ClientCi::new(&runtime, &client);
    bosn_service::ci::mcp::call(name, &arguments, &mut backend)
        .ok_or_else(|| format!("unknown CI tool {name}"))?
        .map(|value| value.to_string())
}

pub(crate) fn service_error(error: bosn_service::Error) -> PyErr {
    if let bosn_service::Error::Ci { code, message } = &error {
        // CI codes are a stable contract (`refused`, `not_found`, ...).
        return PyRuntimeError::new_err(format!("Bosn CI {code}: {message}"));
    }
    if let bosn_service::Error::ProtocolUnsupported { .. } = &error {
        return PyRuntimeError::new_err(error.to_string());
    }
    let message = match error {
        bosn_service::Error::Io(_) | bosn_service::Error::Deadline => {
            "Bosn daemon is unavailable or did not respond in time"
        }
        bosn_service::Error::Unauthorized => "Bosn daemon authentication failed",
        bosn_service::Error::EndpointOccupied(_) => "Bosn daemon endpoint is unavailable",
        bosn_service::Error::Protocol(_) => "Bosn daemon rejected the request",
        bosn_service::Error::Ci { .. }
        | bosn_service::Error::ProtocolUnsupported { .. }
        | bosn_service::Error::Registry(_)
        | bosn_service::Error::Random
        | bosn_service::Error::ActorClosed => "Bosn daemon request failed",
    };
    PyRuntimeError::new_err(message)
}

pub(crate) fn source_kind_name(source_kind: SetupSourceKind) -> &'static str {
    match source_kind {
        SetupSourceKind::LocalFile => "local_file",
        SetupSourceKind::Https => "https",
    }
}
