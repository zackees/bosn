//! Validation of diagnostic, GC, repair and setup-done requests.

use super::*;

pub(crate) fn validate_registry_page(after: u64, limit: u32) -> Result<(), Error> {
    let _ = usize::try_from(after).map_err(|_| Error::Protocol("invalid registry cursor"))?;
    if limit == 0 || limit > MAX_REGISTRY_DIAGNOSTIC_PAGE {
        return Err(Error::Protocol("invalid registry page limit"));
    }
    Ok(())
}

/// Diagnostic operations have no caller-selected files, jobs, setup values, or
/// engine controls. Rejecting stray fields makes the private wire contract as
/// narrow as every public front end rather than silently accepting ambiguity.
pub(crate) fn validate_registry_diagnostics_request_wire(request: &Request) -> Result<(), Error> {
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

pub(crate) fn validate_setup_gc_preview_request_wire(request: &Request) -> Result<(), Error> {
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
pub(crate) fn validate_manifest_volume_gc_preview_request_wire(
    request: &Request,
) -> Result<(), Error> {
    validate_setup_gc_preview_request_wire(request)
        .map_err(|_| Error::Protocol("nonsemantic manifest volume gc preview fields"))
}

pub(crate) fn validate_setup_reconcile_preview_request_wire(
    request: &Request,
) -> Result<(), Error> {
    validate_setup_gc_preview_request_wire(request)
        .map_err(|_| Error::Protocol("nonsemantic setup reconcile preview fields"))
}

pub(crate) fn validate_setup_reconcile_repair_missing_input(
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

pub(crate) fn validate_setup_reconcile_repair_missing_request_wire(
    request: &Request,
) -> Result<(), Error> {
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

pub(crate) fn validate_setup_gc_apply_input(
    workspace: &str,
    token: &str,
    confirm: bool,
) -> Result<(), Error> {
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

pub(crate) fn validate_setup_retired_stop_input(
    workspace: &str,
    token: &str,
    confirm: bool,
) -> Result<(), Error> {
    validate_setup_gc_apply_input(workspace, token, confirm)
        .map_err(|_| Error::Protocol("invalid setup retired stop request"))
}

pub(crate) fn validate_setup_gc_apply_request_wire(request: &Request) -> Result<(), Error> {
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
pub(crate) fn validate_manifest_volume_gc_apply_input(
    workspace: &str,
    token: &str,
    confirm: bool,
) -> Result<(), Error> {
    if workspace.is_empty()
        || workspace.len() > 8 * 1024
        || workspace.bytes().any(|b| b == 0)
        || !confirm
    {
        return Err(Error::Protocol("invalid manifest volume gc apply request"));
    }
    let _ = parse_manifest_volume_gc_token(token)?;
    Ok(())
}
pub(crate) fn validate_manifest_volume_gc_apply_request_wire(
    request: &Request,
) -> Result<(), Error> {
    validate_manifest_volume_gc_apply_input(
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
        return Err(Error::Protocol(
            "nonsemantic manifest volume gc apply fields",
        ));
    }
    Ok(())
}
pub(crate) fn validate_manifest_volume_release_apply_input(
    workspace: &str,
    token: &str,
    confirm: bool,
) -> Result<(), Error> {
    if workspace.is_empty()
        || workspace.len() > 8 * 1024
        || workspace.bytes().any(|b| b == 0)
        || !confirm
    {
        return Err(Error::Protocol(
            "invalid manifest volume release apply request",
        ));
    }
    let _ = parse_manifest_volume_release_token(token)?;
    Ok(())
}
pub(crate) fn validate_manifest_volume_release_apply_request_wire(
    request: &Request,
) -> Result<(), Error> {
    validate_manifest_volume_release_apply_input(
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
        return Err(Error::Protocol(
            "nonsemantic manifest volume release apply fields",
        ));
    }
    Ok(())
}

pub(crate) fn validate_setup_retired_stop_request_wire(request: &Request) -> Result<(), Error> {
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

pub(crate) fn validate_setup_done_input(workspace: &str, confirm: bool) -> Result<(), Error> {
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
pub(crate) fn canonical_setup_done_workspace(path: impl AsRef<Path>) -> Result<String, Error> {
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
pub(crate) fn validate_setup_done_request_wire(request: &Request) -> Result<(), Error> {
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
pub(crate) fn validate_doctor_request_wire(request: &Request) -> Result<(), Error> {
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
