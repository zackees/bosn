//! Translation of the Compose subset into a typed setup document (inert).

use super::*;

/// Translate the deliberately small, semantically identical Compose subset into a typed setup
/// document. This function is inert: it has no filesystem, daemon, engine, network, or process
/// API. Unsupported fields are refused rather than being forwarded as Docker arguments.
pub fn translate_compose_to_setup(
    document: &ComposeDocument,
) -> Result<ComposeSetupPlan, ComposeError> {
    if document.services.len() != 1 {
        return Err(ComposeError::unsupported(
            "services",
            "the setup adapter accepts exactly one service; split multi-service Compose applications before translation",
        ));
    }
    if !document.volumes.is_empty() {
        return Err(ComposeError::unsupported(
            "volumes",
            "top-level named volumes are not represented by the single-app setup model; use workspace bind mounts",
        ));
    }
    if !document.networks.is_empty() {
        return Err(ComposeError::unsupported(
            "networks",
            "top-level networks are not represented by the single-app setup model",
        ));
    }

    let (service_name, service) = document
        .services
        .iter()
        .next()
        .expect("a one-service Compose document has one service");
    let service_path = format!("services.{service_name}");
    reject_translation_features(service, &service_path)?;

    let image = service.image.as_deref().ok_or_else(|| {
        ComposeError::unsupported(
            format!("{service_path}.image"),
            "the setup adapter requires one image pinned as name@sha256:<64 lowercase hex digits>",
        )
    })?;
    validate_pinned_setup_image(image, &format!("{service_path}.image"))?;

    let mounts = translate_bind_mounts(&service.mounts, &format!("{service_path}.volumes"))?;
    let workdir = translate_workdir(
        service.working_dir.as_deref(),
        &service.mounts,
        &format!("{service_path}.working_dir"),
    )?;
    let command = translate_command(service.command.as_deref(), &service_path)?;
    validate_setup_environment(&service.environment, &format!("{service_path}.environment"))?;

    let setup = SetupDocument {
        version: SETUP_DOCUMENT_VERSION,
        app: SetupApp {
            source: SetupSource::PinnedImage(image.into()),
            environment: service.environment.clone(),
            workdir,
            command,
            mounts,
        },
        tasks: Default::default(),
        files: Vec::new(),
    };
    canonical_compose_setup_plan(service_name.clone(), setup)
}

/// Parse and translate one Compose YAML string without accessing paths or system state.
pub fn parse_and_translate_compose_yaml(source: &str) -> Result<ComposeSetupPlan, ComposeError> {
    translate_compose_to_setup(&parse_compose_yaml(source)?)
}

pub(crate) fn canonical_compose_setup_plan(
    service: String,
    setup: SetupDocument,
) -> Result<ComposeSetupPlan, ComposeError> {
    #[derive(Serialize)]
    struct CanonicalReceipt<'a> {
        version: u32,
        service: &'a str,
        setup: &'a SetupDocument,
    }

    let normalized_json = serde_json::to_string(&CanonicalReceipt {
        version: COMPOSE_SETUP_PLAN_VERSION,
        service: &service,
        setup: &setup,
    })
    .map_err(|error| {
        ComposeError::new(
            ComposeErrorCode::InvalidValue,
            "compose",
            error.to_string(),
            "use only serializable Bosn Compose values",
        )
    })?;
    let digest = format!("sha256:{:x}", Sha256::digest(normalized_json.as_bytes()));
    Ok(ComposeSetupPlan {
        version: COMPOSE_SETUP_PLAN_VERSION,
        service,
        setup,
        normalized_json,
        digest,
    })
}

pub(crate) fn reject_translation_features(
    service: &ServiceSpec,
    path: &str,
) -> Result<(), ComposeError> {
    let reject = |field: &str, remedy: &str| {
        Err(ComposeError::unsupported(format!("{path}.{field}"), remedy))
    };
    if service.build.is_some() {
        return reject(
            "build",
            "the setup adapter cannot inspect a Compose build context; use a pinned image",
        );
    }
    if !service.profiles.is_empty() {
        return reject(
            "profiles",
            "profile selection is not represented by the single-app setup model",
        );
    }
    if !service.networks.is_empty() {
        return reject(
            "networks",
            "network attachment is not represented by the single-app setup model",
        );
    }
    if !service.ports.is_empty() {
        return reject(
            "ports",
            "published ports are not represented by the single-app setup model",
        );
    }
    if !service.depends_on.is_empty() {
        return reject(
            "depends_on",
            "service dependencies require a multi-service daemon planner",
        );
    }
    if service.healthcheck.is_some() {
        return reject(
            "healthcheck",
            "healthcheck lifecycle semantics are not represented by the setup model",
        );
    }
    if !service.labels.is_empty() {
        return reject(
            "labels",
            "caller Compose labels are not represented; setup owns its managed labels",
        );
    }
    if service.entrypoint.is_some() {
        return reject(
            "entrypoint",
            "entrypoint overrides are not represented; retain the image entrypoint",
        );
    }
    if service.restart.is_some() {
        return reject(
            "restart",
            "restart policy is not represented by the setup model",
        );
    }
    if service.container_name.is_some() {
        return reject(
            "container_name",
            "setup derives its owned container name from the receipt digest",
        );
    }
    Ok(())
}

pub(crate) fn translate_bind_mounts(
    mounts: &[MountSpec],
    path: &str,
) -> Result<Vec<WorkspaceMount>, ComposeError> {
    if mounts.len() > MAX_MOUNTS {
        return Err(ComposeError::unsupported(
            path,
            format!("the setup model allows at most {MAX_MOUNTS} workspace bind mounts"),
        ));
    }
    mounts
        .iter()
        .enumerate()
        .map(|(index, mount)| match mount {
            MountSpec::Bind {
                source,
                target,
                read_only,
            } => {
                validate_setup_relative_path(source.as_str(), &format!("{path}[{index}].source"))?;
                validate_setup_container_path(target, &format!("{path}[{index}].target"))?;
                Ok(WorkspaceMount {
                    source: source.as_str().into(),
                    target: target.clone(),
                    readonly: *read_only,
                })
            }
            MountSpec::Volume { .. } => Err(ComposeError::unsupported(
                format!("{path}[{index}]"),
                "named volumes are not represented by the setup model; use a workspace bind mount",
            )),
            MountSpec::Tmpfs { .. } => Err(ComposeError::unsupported(
                format!("{path}[{index}]"),
                "tmpfs mounts are not represented by the setup model",
            )),
        })
        .collect()
}

pub(crate) fn translate_workdir(
    working_dir: Option<&str>,
    mounts: &[MountSpec],
    path: &str,
) -> Result<Option<String>, ComposeError> {
    let Some(working_dir) = working_dir else {
        return Ok(None);
    };
    validate_setup_container_path(working_dir, path)?;
    let selected = mounts
        .iter()
        .filter_map(|mount| match mount {
            MountSpec::Bind { source, target, .. } if container_prefix(working_dir, target) => {
                Some((source.as_str(), target.as_str()))
            }
            _ => None,
        })
        .max_by_key(|(_, target)| target.len())
        .ok_or_else(|| {
            ComposeError::unsupported(
                path,
                "working_dir must be covered by a declared workspace bind mount",
            )
        })?;
    let suffix = working_dir[selected.1.len()..].trim_start_matches('/');
    let workspace_path = match (selected.0, suffix) {
        (".", "") => ".".into(),
        (".", suffix) => suffix.into(),
        (source, "") => source.into(),
        (source, suffix) => format!("{source}/{suffix}"),
    };
    validate_setup_relative_path(&workspace_path, path)?;
    Ok(Some(workspace_path))
}

pub(crate) fn translate_command(
    command: Option<&[String]>,
    service_path: &str,
) -> Result<Option<String>, ComposeError> {
    let Some(command) = command else {
        return Ok(None);
    };
    let [shell, flag, script] = command else {
        return Err(ComposeError::unsupported(
            format!("{service_path}.command"),
            "only the exact list [sh, -lc, <nonempty script>] is represented by setup",
        ));
    };
    if shell != "sh"
        || flag != "-lc"
        || script.is_empty()
        || script.len() > 16 * 1024
        || script.contains('\0')
    {
        return Err(ComposeError::unsupported(
            format!("{service_path}.command"),
            "only the exact list [sh, -lc, <nonempty bounded script>] is represented by setup",
        ));
    }
    Ok(Some(script.clone()))
}

pub(crate) fn validate_pinned_setup_image(image: &str, path: &str) -> Result<(), ComposeError> {
    let valid = image.len() <= 512
        && !image
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        && image.rsplit_once("@sha256:").is_some_and(|(name, digest)| {
            !name.is_empty()
                && !name.contains('@')
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    valid.then_some(()).ok_or_else(|| {
        ComposeError::unsupported(
            path,
            "the setup adapter requires image: name@sha256:<64 lowercase hex digits>",
        )
    })
}

pub(crate) fn validate_setup_environment(
    environment: &BTreeMap<String, String>,
    path: &str,
) -> Result<(), ComposeError> {
    if environment.len() > MAX_ENVIRONMENT_ENTRIES {
        return Err(ComposeError::unsupported(
            path,
            format!("the setup model allows at most {MAX_ENVIRONMENT_ENTRIES} environment values"),
        ));
    }
    for (name, value) in environment {
        let mut bytes = name.bytes();
        let valid_name = matches!(bytes.next(), Some(b'_' | b'a'..=b'z' | b'A'..=b'Z'))
            && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
        if !valid_name || value.len() > 16 * 1024 || value.contains('\0') {
            return Err(ComposeError::unsupported(
                format!("{path}.{name}"),
                "setup environment names must be shell identifiers and values must be bounded without NUL",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_setup_relative_path(
    path: &str,
    error_path: &str,
) -> Result<(), ComposeError> {
    let valid = !path.is_empty()
        && path.len() <= 4096
        && !path.contains('\0')
        && !path.contains('\\')
        && !path.starts_with('/')
        && !is_windows_absolute(path)
        && !path
            .split('/')
            .any(|component| component.is_empty() || component == "..");
    valid.then_some(()).ok_or_else(|| {
        unsafe_path(
            error_path,
            "path cannot be represented as a safe setup workspace-relative path",
        )
    })
}

pub(crate) fn validate_setup_container_path(
    path: &str,
    error_path: &str,
) -> Result<(), ComposeError> {
    let suffix = path.strip_prefix('/');
    let valid = !path.is_empty()
        && path.len() <= 4096
        && !path.contains('\0')
        && !path.contains('\\')
        && suffix.is_some_and(|suffix| {
            suffix.is_empty()
                || !suffix
                    .split('/')
                    .any(|component| component.is_empty() || component == "..")
        });
    valid.then_some(()).ok_or_else(|| {
        unsafe_path(
            error_path,
            "path cannot be represented as a normalized setup container path",
        )
    })
}

pub(crate) fn container_prefix(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}
