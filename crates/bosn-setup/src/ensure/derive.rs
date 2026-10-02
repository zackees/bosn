//! Deriving the fixed engine command, mounts and volumes from a validated setup plan.

use super::*;

#[derive(Clone, Debug)]
pub(crate) struct DerivedEnsure {
    pub(crate) container_name: String,
    pub(crate) image_identity: String,
    pub(crate) mounts: Vec<SetupEnsureMount>,
    pub(crate) environment: BTreeMap<String, String>,
    pub(crate) workdir: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) volumes: Vec<SetupEnsureVolume>,
    pub(crate) tmpfs: Vec<SetupEnsureTmpfs>,
    pub(crate) host_docker_socket: Option<crate::SetupHostDockerSocket>,
    pub(crate) macos_guest: Option<SetupEnsureMacosGuest>,
}

impl DerivedEnsure {
    /// This app's runtime shape as Docker flags (see [`runtime_shape_args`]).
    pub(crate) fn runtime_shape_args(&self) -> Vec<String> {
        runtime_shape_args(
            &self.mounts,
            &self.volumes,
            &self.tmpfs,
            self.host_docker_socket.as_ref(),
            &self.environment,
            self.workdir.as_deref(),
        )
    }

    pub(crate) fn create_command(&self) -> SetupEnsureCommand {
        SetupEnsureCommand::Create {
            container_name: self.container_name.clone(),
            image_identity: self.image_identity.clone(),
            mounts: self.mounts.clone(),
            volumes: self.volumes.clone(),
            tmpfs: self.tmpfs.clone(),
            host_docker_socket: Box::new(self.host_docker_socket.clone()),
            environment: self.environment.clone(),
            workdir: self.workdir.clone(),
            command: self.command.clone(),
            labels: self.labels.clone(),
            macos_guest: Box::new(self.macos_guest.clone()),
        }
    }
}

pub(crate) fn derive_command(
    request: &SetupEnsureRequest<'_>,
) -> Result<DerivedEnsure, SetupEnsureError> {
    derive_creation(
        request.plan,
        &request.workspace_root,
        request.prepared_image,
    )
}

/// The sole container identity derivation shared by ensure, adoption and task execution.
pub fn setup_container_name(
    plan: &SetupPlan,
    workspace: &Path,
    image: &PreparedImage,
) -> Result<String, SetupEnsureError> {
    Ok(derive_creation(plan, workspace, image)?.container_name)
}

/// Pure verification for trusted engine adapters. These supplied observations
/// are data, not authentication; public clients cannot confer engine authority.
pub fn verify_setup_observation(
    plan: &SetupPlan,
    image: &PreparedImage,
    observed: &SetupEnsureObservedContainer,
    image_configuration: &serde_json::Value,
    volume_observations: &BTreeMap<String, serde_json::Value>,
) -> Result<(), SetupEnsureError> {
    let expected = derive_creation(plan, &plan.workspace_root, image)?;
    validate_observed(observed, &expected)?;
    verify_actual_configuration(observed, &expected, image_configuration)?;
    if volume_observations.len() != expected.volumes.len() {
        return Err(SetupEnsureError::OwnershipMismatch);
    }
    for volume in &expected.volumes {
        let receipt = volume_observations
            .get(&volume.name)
            .ok_or(SetupEnsureError::OwnershipMismatch)?;
        verify_volume_observation(
            volume,
            receipt,
            Some(container_volume_source(observed, volume)?),
        )?;
    }
    Ok(())
}

pub(crate) fn derive_creation(
    plan: &SetupPlan,
    workspace: &Path,
    image: &PreparedImage,
) -> Result<DerivedEnsure, SetupEnsureError> {
    validate_plan_shape(plan)?;
    let workspace_root = canonical_workspace(workspace)?;
    if workspace_root.to_str().is_none() {
        return Err(SetupEnsureError::InvalidRequest("workspace is not UTF-8"));
    }
    if workspace_root != plan.workspace_root {
        return Err(SetupEnsureError::InvalidRequest(
            "workspace is not the plan's canonical workspace root",
        ));
    }
    if plan.app.mounts.len() > 128 || plan.named_volumes.len() > 128 || plan.tmpfs.len() > 128 {
        return Err(SetupEnsureError::InvalidRequest(
            "creation mount inventory exceeds 128 entries per kind",
        ));
    }
    validate_prepared_image(plan, image)?;
    let mounts = derive_mounts(&workspace_root, plan)?;
    let workdir = plan
        .app
        .workdir
        .as_deref()
        .map(|value| resolve_workdir(value, &plan.app.mounts, &workspace_root))
        .transpose()?;
    let command = plan.app.command.clone();
    if command
        .as_deref()
        .is_some_and(|value| value.is_empty() || value.len() > 16 * 1024 || value.contains('\0'))
    {
        return Err(SetupEnsureError::InvalidRequest(
            "declared app command is invalid",
        ));
    }
    let volumes = derive_volumes(plan)?;
    let tmpfs = derive_tmpfs(plan)?;
    let macos_guest = plan
        .macos_guest
        .as_ref()
        .map(|guest| SetupEnsureMacosGuest {
            ssh_port: guest.ssh_port,
            web_port: guest.web_port,
            version: guest.version.clone(),
            ram_size: guest.ram_size.clone(),
            disk_size: guest.disk_size.clone(),
            cpu_cores: guest.cpu_cores,
        });
    let mut derived = DerivedEnsure {
        container_name: String::new(),
        image_identity: image.observed_identity.clone(),
        mounts,
        environment: validated_environment(&plan.app.environment)?,
        workdir,
        command,
        labels: BTreeMap::new(),
        volumes,
        tmpfs,
        host_docker_socket: plan.host_docker_socket.clone(),
        macos_guest,
    };
    derived
        .mounts
        .sort_by(|left, right| left.target.cmp(&right.target));
    derived
        .volumes
        .sort_by(|left, right| left.target.cmp(&right.target));
    derived
        .tmpfs
        .sort_by(|left, right| left.target.cmp(&right.target));
    let mut arguments = vec![plan.content_sha256.clone()];
    arguments.extend(derived.create_command().docker_args());
    if arguments
        .iter()
        .try_fold(0usize, |size, value| size.checked_add(value.len() + 8))
        .is_none_or(|size| size > 2 * 1024 * 1024)
    {
        return Err(SetupEnsureError::InvalidRequest(
            "creation profile exceeds 2 MiB",
        ));
    }
    let digest = crate::creation::creation_digest(&workspace_root, &arguments);
    derived.container_name = format!("bosn-setup-v2-{digest}");
    derived.labels = BTreeMap::from([
        (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
        (LABEL_CONTENT_SHA256.into(), plan.content_sha256.clone()),
        (LABEL_CONTAINER_NAME.into(), derived.container_name.clone()),
        (LABEL_CREATION_PROFILE.into(), format!("v2:{digest}")),
    ]);
    Ok(derived)
}

pub(crate) fn validate_plan_shape(plan: &SetupPlan) -> Result<(), SetupEnsureError> {
    if plan.schema_version != SETUP_DOCUMENT_VERSION || !valid_hash(&plan.content_sha256) {
        return Err(SetupEnsureError::InvalidRequest("plan receipt is invalid"));
    }
    let names: Vec<_> = plan.tasks.keys().cloned().collect();
    if plan.task_names != names {
        return Err(SetupEnsureError::InvalidRequest(
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
            return Err(SetupEnsureError::InvalidRequest(
                "plan application receipt was modified",
            ));
        }
    }
    validate_workspace_relative(plan.app.workdir.as_deref())?;
    for mount in &plan.app.mounts {
        validate_workspace_relative(Some(&mount.source))?;
        validate_container_path(&mount.target)?;
    }
    let mut targets: BTreeSet<String> = plan
        .app
        .mounts
        .iter()
        .map(|mount| mount.target.clone())
        .collect();
    if targets.len() != plan.app.mounts.len() {
        return Err(SetupEnsureError::InvalidRequest(
            "duplicate bind mount target",
        ));
    }
    for volume in &plan.named_volumes {
        if !valid_volume_name(&volume.name)
            || validate_container_path(&volume.target).is_err()
            || !targets.insert(volume.target.clone())
            || volume.labels.get(LABEL_MANAGED) != Some(&MANAGED_VALUE.into())
            || !valid_hash(
                volume
                    .labels
                    .get(LABEL_CONTENT_SHA256)
                    .map_or("", String::as_str),
            )
            || volume.labels.get(LABEL_CONTAINER_NAME) != Some(&volume.name)
        {
            return Err(SetupEnsureError::InvalidRequest(
                "named volume receipt was modified",
            ));
        }
    }
    for tmpfs in &plan.tmpfs {
        if validate_container_path(&tmpfs.target).is_err()
            || !targets.insert(tmpfs.target.clone())
            || tmpfs.size.as_ref().is_some_and(|size| size.value == 0)
            || tmpfs.mode.is_some_and(|mode| mode > 0o7777)
        {
            return Err(SetupEnsureError::InvalidRequest(
                "tmpfs receipt was modified",
            ));
        }
    }
    if let Some(socket) = &plan.host_docker_socket
        && (validate_container_path(&socket.target).is_err()
            || !targets.insert(socket.target.clone())
            || plan.macos_guest.is_some()
            || crate::SetupHostDockerSocketSource::from_host_path(socket.source.host_path())
                != Some(socket.source)
            || socket.proxy_dir.as_ref().is_some_and(|dir| {
                validate_container_path(dir).is_err()
                    || dir == "/"
                    || dir.contains(',')
                    || !targets.insert(dir.clone())
            }))
    {
        return Err(SetupEnsureError::InvalidRequest(
            "host Docker socket receipt was modified",
        ));
    }
    if let Some(guest) = &plan.macos_guest
        && (!matches!(plan.app.source, bosn_core::SetupSource::PinnedImage(_))
            || !matches!(&plan.app.source, bosn_core::SetupSource::PinnedImage(image) if valid_macos_guest_image(image))
            || !plan.app.mounts.is_empty()
            || plan.app.workdir.is_some()
            || plan.app.command.is_some()
            || guest.ssh_port == 0
            || guest.web_port == 0
            || guest.ssh_port == guest.web_port
            || guest.cpu_cores == 0
            || !valid_guest_env_value(&guest.version)
            || !valid_guest_env_value(&guest.ram_size)
            || !valid_guest_env_value(&guest.disk_size))
    {
        return Err(SetupEnsureError::InvalidRequest(
            "macOS guest receipt was modified",
        ));
    }
    if let Some(guest) = &plan.macos_guest {
        let matching_storage = plan
            .named_volumes
            .iter()
            .filter(|volume| volume.target == "/storage")
            .collect::<Vec<_>>();
        if guest.storage_volume.is_empty()
            || matching_storage.len() != 1
            || matching_storage[0].name != guest.storage_volume
            || !valid_volume_name(&guest.storage_volume)
            || guest.storage_scope != Scope::Machine
            || guest.storage_retention != Retention::Pinned
        {
            return Err(SetupEnsureError::InvalidRequest(
                "macOS guest storage volume receipt was modified",
            ));
        }
    }
    Ok(())
}

pub(crate) fn valid_guest_env_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.contains(['\0', '\n', '\r', '='])
}

/// Docker Hub's documented registry spellings all designate the one trusted
/// dockurr entrypoint image. A digest pins the bytes; accepting another
/// repository here would turn the fixed KVM/tun create shape into a generic
/// privileged-container escape hatch.
pub(crate) fn valid_macos_guest_image(value: &str) -> bool {
    let Some((name, digest)) = value.rsplit_once("@sha256:") else {
        return false;
    };
    let repository = if let Some((prefix, final_component)) = name.rsplit_once('/') {
        if let Some((repository, _tag)) = final_component.split_once(':') {
            format!("{prefix}/{repository}")
        } else {
            name.to_owned()
        }
    } else {
        name.to_owned()
    };
    matches!(
        repository.as_str(),
        "dockurr/macos"
            | "docker.io/dockurr/macos"
            | "index.docker.io/dockurr/macos"
            | "registry-1.docker.io/dockurr/macos"
    ) && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn derive_volumes(plan: &SetupPlan) -> Result<Vec<SetupEnsureVolume>, SetupEnsureError> {
    let bind_targets: BTreeSet<_> = plan.app.mounts.iter().map(|mount| &mount.target).collect();
    plan.named_volumes
        .iter()
        .map(|volume| {
            if bind_targets.contains(&volume.target) || !valid_volume_name(&volume.name) {
                return Err(SetupEnsureError::InvalidRequest(
                    "duplicate or unsafe named volume target",
                ));
            }
            Ok(SetupEnsureVolume {
                name: volume.name.clone(),
                target: volume.target.clone(),
                labels: volume.labels.clone(),
            })
        })
        .collect()
}

pub(crate) fn derive_tmpfs(plan: &SetupPlan) -> Result<Vec<SetupEnsureTmpfs>, SetupEnsureError> {
    plan.tmpfs
        .iter()
        .map(|mount| {
            Ok(SetupEnsureTmpfs {
                target: mount.target.clone(),
                readonly: mount.readonly,
                size: mount.size.clone(),
                exec: mount.exec,
                mode: mount.mode,
            })
        })
        .collect()
}

pub(crate) fn tmpfs_docker_value(mount: &SetupEnsureTmpfs) -> String {
    let mut options = Vec::new();
    if mount.readonly {
        options.push("ro".to_owned());
    }
    if let Some(size) = &mount.size {
        let unit = match size.unit {
            crate::SetupTmpfsSizeUnit::Bytes => "b",
            crate::SetupTmpfsSizeUnit::Kibibytes => "k",
            crate::SetupTmpfsSizeUnit::Mebibytes => "m",
            crate::SetupTmpfsSizeUnit::Gibibytes => "g",
        };
        options.push(format!("size={}{}", size.value, unit));
    }
    match mount.exec {
        Some(true) => options.push("exec".to_owned()),
        Some(false) => options.push("noexec".to_owned()),
        None => {}
    }
    if let Some(mode) = mount.mode {
        options.push(format!("mode={mode:o}"));
    }
    if options.is_empty() {
        mount.target.clone()
    } else {
        format!("{}:{}", mount.target, options.join(","))
    }
}

pub(crate) fn valid_volume_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

pub(crate) fn validate_prepared_image(
    plan: &SetupPlan,
    image: &PreparedImage,
) -> Result<(), SetupEnsureError> {
    if image.setup_content_sha256 != plan.content_sha256
        || !valid_identity(&image.observed_identity)
    {
        return Err(SetupEnsureError::InvalidRequest(
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
        _ => Err(SetupEnsureError::InvalidRequest(
            "prepared image is not for the planned application",
        )),
    }
}

pub(crate) fn derive_mounts(
    workspace_root: &Path,
    plan: &SetupPlan,
) -> Result<Vec<SetupEnsureMount>, SetupEnsureError> {
    let mut targets = BTreeSet::new();
    plan.app
        .mounts
        .iter()
        .map(|mount| {
            if !targets.insert(mount.target.clone()) {
                return Err(SetupEnsureError::InvalidRequest(
                    "duplicate container mount target",
                ));
            }
            let source = canonical_workspace_member(workspace_root, &mount.source)?;
            let source = source.to_str().ok_or(SetupEnsureError::InvalidRequest(
                "mount source is not UTF-8",
            ))?;
            if source.contains(',') || mount.target.contains(',') {
                return Err(SetupEnsureError::InvalidRequest(
                    "mount path cannot be represented safely by Docker",
                ));
            }
            Ok(SetupEnsureMount {
                source: PathBuf::from(source),
                target: mount.target.clone(),
                readonly: mount.readonly,
            })
        })
        .collect()
}

pub(crate) fn resolve_workdir(
    workdir: &str,
    mounts: &[bosn_core::WorkspaceMount],
    workspace_root: &Path,
) -> Result<String, SetupEnsureError> {
    validate_workspace_relative(Some(workdir))?;
    let selected = mounts
        .iter()
        .filter(|mount| workspace_prefix(workdir, &mount.source))
        .max_by_key(|mount| mount.source.len())
        .ok_or(SetupEnsureError::InvalidRequest(
            "declared workdir is not covered by a declared workspace mount",
        ))?;
    let suffix = relative_suffix(workdir, &selected.source).expect("selected mount covers workdir");
    // Recheck the selected source at apply time. A manifest/setup document can
    // be planned while a source is a directory and changed before Docker is
    // invoked; a file bind cannot meaningfully back a container workdir.
    let source = canonical_workspace_member(workspace_root, &selected.source)?;
    let metadata = fs::context_path_metadata_no_follow(&source).map_err(|_| {
        SetupEnsureError::InvalidRequest("declared workdir mount source does not exist")
    })?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err(SetupEnsureError::InvalidRequest(
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

pub(crate) fn validated_environment(
    input: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, SetupEnsureError> {
    if input.len() > MAX_ENVIRONMENT_ENTRIES
        || input.iter().any(|(key, value)| {
            !valid_environment_name(key) || value.len() > 16 * 1024 || value.contains('\0')
        })
    {
        return Err(SetupEnsureError::InvalidRequest(
            "app environment is not safe document data",
        ));
    }
    Ok(input.clone())
}
