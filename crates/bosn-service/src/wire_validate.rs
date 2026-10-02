//! Daemon-side validation of decoded wire requests (semantic fields only).

use super::*;

/// The unmanaged apply carries only what it needs: whether the caller confirmed, which
/// Tier-2 identities it opted in, and the age gate it used for its preview.
pub(crate) fn validate_unmanaged_apply_request_wire(r: &Request) -> Result<(), Error> {
    if !r.gc_confirm
        || r.workspace.len() > 8 * 1024
        || !r.gc_candidate_token.is_empty()
        || !r.digest.is_empty()
        || !r.stack.is_empty()
        || r.job_id != 0
        || !r.setup_config.is_empty()
        || r.setup_task_name.is_empty() && r.setup_deadline_ms != 0
    {
        return Err(Error::Protocol("nonsemantic unmanaged apply fields"));
    }
    if r.unmanaged_include.len() > 1024
        || r.unmanaged_include
            .iter()
            .any(|id| id.is_empty() || id.len() > 8 * 1024 || id.bytes().any(|b| b == 0))
    {
        return Err(Error::Protocol("invalid unmanaged apply include list"));
    }
    Ok(())
}

pub(crate) fn validate_setup_prepare_wire(
    workspace: &str,
    config: &str,
    _policy: SetupPreparePolicy,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_request_text_and_budget(
        workspace,
        config,
        deadline_ms,
        output_limit,
        SETUP_PREPARE_MAX_DEADLINE,
        SETUP_PREPARE_MAX_OUTPUT,
    )
}

pub(crate) fn validate_request_text_and_budget(
    workspace: &str,
    config: &str,
    deadline_ms: u64,
    output_limit: u32,
    max_deadline: Duration,
    max_output: usize,
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
    if deadline.is_zero() || deadline > max_deadline {
        return Err(Error::Protocol("invalid setup deadline"));
    }
    let output_limit = output_limit as usize;
    if output_limit == 0 || output_limit > max_output {
        return Err(Error::Protocol("invalid setup output limit"));
    }
    Ok(())
}

/// Manifest operations use the larger declared-workload budget.
pub(crate) fn validate_manifest_text_and_budget(
    workspace: &str,
    manifest: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_request_text_and_budget(
        workspace,
        manifest,
        deadline_ms,
        output_limit,
        MANIFEST_MAX_DEADLINE,
        MANIFEST_MAX_OUTPUT,
    )
}

pub(crate) fn validate_setup_task_wire(
    workspace: &str,
    config: &str,
    policy: SetupPreparePolicy,
    task_name: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_setup_prepare_wire(workspace, config, policy, deadline_ms, output_limit)?;
    validate_setup_task_name(task_name)
}

pub(crate) fn validate_setup_task_name(task_name: &str) -> Result<(), Error> {
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

pub(crate) fn validate_setup_ensure_wire(
    workspace: &str,
    config: &str,
    policy: SetupPreparePolicy,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_setup_prepare_wire(workspace, config, policy, deadline_ms, output_limit)
}

pub(crate) fn validate_manifest_ensure_wire(
    workspace: &str,
    manifest: &str,
    stack: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_manifest_text_and_budget(workspace, manifest, deadline_ms, output_limit)?;
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

pub(crate) fn validate_manifest_converge_wire(
    workspace: &str,
    manifest: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_manifest_text_and_budget(workspace, manifest, deadline_ms, output_limit)?;
    if !safe_manifest_relative_path(manifest) {
        return Err(Error::Protocol("invalid manifest converge selector"));
    }
    Ok(())
}

pub(crate) fn validate_manifest_ensure_request_wire(request: &Request) -> Result<(), Error> {
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

pub(crate) fn validate_manifest_converge_request_wire(request: &Request) -> Result<(), Error> {
    validate_manifest_converge_wire(
        &request.workspace,
        &request.setup_config,
        request.setup_deadline_ms,
        request.setup_output_limit,
    )?;
    if !request.stack.is_empty()
        || !request.digest.is_empty()
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
        return Err(Error::Protocol("nonsemantic manifest converge fields"));
    }
    Ok(())
}
pub(crate) fn validate_manifest_app_task_wire(
    workspace: &str,
    manifest: &str,
    stack: &str,
    task_name: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> Result<(), Error> {
    validate_manifest_ensure_wire(workspace, manifest, stack, deadline_ms, output_limit)?;
    validate_setup_task_name(task_name)
}
pub(crate) fn validate_manifest_app_task_request_wire(request: &Request) -> Result<(), Error> {
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
    if request.follow_lease_ms != 0
        && !(FOLLOW_LEASE_MIN..=FOLLOW_LEASE_MAX)
            .contains(&Duration::from_millis(request.follow_lease_ms))
    {
        return Err(Error::Protocol("invalid manifest app task follow lease"));
    }
    Ok(())
}

/// The operation reuses the compact private protobuf envelope, but accepts no
/// legacy job or task fields. Rejecting rather than ignoring these values
/// makes the semantic surface exactly the five documented immutable inputs.
pub(crate) fn validate_setup_ensure_request_wire(
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
pub(crate) fn validate_setup_adopt_input(
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
pub(crate) fn validate_setup_adopt_request_wire(
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
