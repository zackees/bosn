//! Shared argument parsing and text/JSON output helpers.

use super::*;

pub(crate) fn set_once<T>(slot: &mut Option<T>, value: Option<T>) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(value.ok_or(())?);
    Ok(())
}

pub(crate) fn set_once_parsed<T>(
    slot: &mut Option<T>,
    value: Option<std::ffi::OsString>,
    parse: impl FnOnce(std::ffi::OsString) -> Result<T, ()>,
) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(parse(value.ok_or(())?)?);
    Ok(())
}

pub(crate) fn parse_u64(value: std::ffi::OsString) -> Result<u64, ()> {
    value.into_string().map_err(|_| ())?.parse().map_err(|_| ())
}

pub(crate) fn parse_usize(value: std::ffi::OsString) -> Result<usize, ()> {
    value.into_string().map_err(|_| ())?.parse().map_err(|_| ())
}

pub(crate) fn parse_setup_request_text(value: std::ffi::OsString) -> Result<String, ()> {
    const MAX_TEXT: usize = 8 * 1024;
    let value = value.into_string().map_err(|_| ())?;
    if value.is_empty() || value.len() > MAX_TEXT || value.bytes().any(|byte| byte == 0) {
        return Err(());
    }
    Ok(value)
}

pub(crate) fn parse_setup_config(value: std::ffi::OsString) -> Result<String, ()> {
    let value = parse_setup_request_text(value)?;
    parse_setup_config_locator(&value).map_err(|_| ())?;
    Ok(value)
}

pub(crate) fn parse_manifest_path(value: std::ffi::OsString) -> Result<String, ()> {
    let value = parse_setup_request_text(value)?;
    (!value.starts_with('/')
        && !value.contains('\\')
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".."))
    .then_some(value)
    .ok_or(())
}

pub(crate) fn parse_setup_task_name(value: std::ffi::OsString) -> Result<String, ()> {
    let value = parse_setup_request_text(value)?;
    if value.len() > 64
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        })
    {
        return Err(());
    }
    Ok(value)
}

pub(crate) fn source_kind_name(source_kind: SetupSourceKind) -> &'static str {
    match source_kind {
        SetupSourceKind::LocalFile => "local_file",
        SetupSourceKind::Https => "https",
    }
}

pub(crate) fn print_text(plan: &SetupPlan) {
    println!("setup plan (not applied)");
    println!("source_kind: {}", source_kind_name(plan.source_kind));
    println!("content_sha256: {}", plan.content_sha256);
    println!("schema_version: {}", plan.schema_version);
    println!("workspace: {}", plan.workspace_root.display());
    println!(
        "asset_root: {}",
        plan.asset_root
            .as_deref()
            .map_or_else(|| "(none)".into(), |path| path.display().to_string())
    );
    println!("tasks: {}", plan.task_names.join(", "));
    match &plan.app_source {
        SetupPlanAppSource::PinnedImage { image } => println!("app_source: pinned_image {image}"),
        SetupPlanAppSource::InlineDockerfile { dockerfile_path } => {
            println!(
                "app_source: inline_dockerfile {}",
                dockerfile_path.display()
            )
        }
    }
}

pub(crate) fn print_json(plan: &SetupPlan) {
    let app_source = match &plan.app_source {
        SetupPlanAppSource::PinnedImage { image } => {
            json!({"kind": "pinned_image", "image": image})
        }
        SetupPlanAppSource::InlineDockerfile { dockerfile_path } => {
            json!({"kind": "inline_dockerfile", "dockerfile_path": dockerfile_path})
        }
    };
    println!(
        "{}",
        json!({
            "action": "plan",
            "applied": false,
            "source_kind": source_kind_name(plan.source_kind),
            "content_sha256": plan.content_sha256,
            "schema_version": plan.schema_version,
            "workspace": plan.workspace_root,
            "asset_root": plan.asset_root,
            "task_names": plan.task_names,
            "app_source": app_source,
        })
    );
}
