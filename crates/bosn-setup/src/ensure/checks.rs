//! Validators and engine-output parsers for setup ensure.

use super::*;

pub(crate) fn validate_observed(
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

pub(crate) fn canonical_workspace(path: &Path) -> Result<PathBuf, SetupEnsureError> {
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

pub(crate) fn canonical_workspace_member(
    root: &Path,
    relative: &str,
) -> Result<PathBuf, SetupEnsureError> {
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

pub(crate) fn parse_inspection(
    stdout: &[u8],
) -> Result<SetupEnsureObservedContainer, CommandError> {
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

pub(crate) fn parse_created_id(stdout: &[u8]) -> Result<String, SetupEnsureError> {
    let value = std::str::from_utf8(stdout)
        .map_err(|_| SetupEnsureError::EngineProtocol("container create output is not UTF-8"))?
        .trim();
    valid_container_id(value)
        .then_some(value.into())
        .ok_or(SetupEnsureError::EngineProtocol(
            "container create did not return an ID",
        ))
}

pub(crate) fn is_absent_container(result: &CommandResult) -> bool {
    result.exit_code == 1
        && String::from_utf8_lossy(&result.stderr)
            .to_ascii_lowercase()
            .contains("no such container")
}

pub(crate) fn protocol_error(detail: &'static str) -> CommandError {
    CommandError::OutputCompletion {
        detail: detail.into(),
        reaped_pid: None,
        cleanup: None,
    }
}

pub(crate) fn validate_workspace_relative(value: Option<&str>) -> Result<(), SetupEnsureError> {
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

pub(crate) fn validate_container_path(value: &str) -> Result<(), SetupEnsureError> {
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

pub(crate) fn workspace_prefix(path: &str, prefix: &str) -> bool {
    prefix == "."
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) fn relative_suffix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix == "." {
        Some(path.strip_prefix("./").unwrap_or(path))
    } else if path == prefix {
        Some("")
    } else {
        path.strip_prefix(prefix)?.strip_prefix('/')
    }
}

pub(crate) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn valid_identity(value: &str) -> bool {
    value.len() == 71 && value.starts_with("sha256:") && valid_hash(&value[7..])
}

pub(crate) fn valid_container_id(value: &str) -> bool {
    value.len() == 64 && valid_hash(value)
}

pub(crate) fn valid_pinned_image(image: &str) -> bool {
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

pub(crate) fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'_' | b'a'..=b'z' | b'A'..=b'Z'))
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

pub(crate) fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

pub(crate) fn failure_detail(result: &CommandResult) -> String {
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
