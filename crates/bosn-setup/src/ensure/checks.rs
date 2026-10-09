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
    result.reports_missing()
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

/// Docker 29 stores `WorkingDir` through `path.Clean` (`/w/.` reads back as
/// `/w`), while older engines echo the requested spelling. A workdir at a
/// mount's root is derived as `<target>/.`, so the reuse proof compares the
/// lexical form: drop `.` segments and repeated or trailing slashes. `..` is
/// kept verbatim, so it can never make two different directories compare equal.
pub(crate) fn lexical_container_path(value: &str) -> String {
    if !value.starts_with('/') {
        return value.to_owned();
    }
    let parts: Vec<&str> = value
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    format!("/{}", parts.join("/"))
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

pub(crate) async fn verify_configuration<E: SetupEnsureEngine>(
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

pub(crate) fn container_volume_source<'a>(
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

pub(crate) fn verify_volume_observation(
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

#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "baseline, ci.yml#229"
)]
pub(crate) fn verify_actual_configuration(
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
            && expected.host_docker_socket.as_ref().is_none_or(|s| {
                s.target != target && s.proxy_dir.as_deref() != Some(target.as_str())
            })
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
        || lexical_container_path(&scalar(config.get("WorkingDir"))?)
            != lexical_container_path(
                &expected
                    .workdir
                    .clone()
                    .unwrap_or(scalar(base.get("WorkingDir"))?),
            )
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
        if let Some(dir) = &socket.proxy_dir {
            wanted.insert(dir.clone(), ("bind".into(), dir.clone(), true));
        }
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
