//! Planning a native manifest stack (and its macOS guest) into a setup plan.

use super::*;

/// Translate the strictly supported manifest runtime subset to the existing
/// typed setup receipt. This is intentionally a refusal boundary, not a
/// lossy migration: fields which would need more lifecycle semantics are
/// rejected before Docker is contacted.
#[cfg(test)]
pub(crate) async fn manifest_stack_setup_plan(
    request: &ManifestEnsureJobRequest,
) -> Result<ManifestRuntimePlan, String> {
    manifest_stack_setup_plan_at(request, None).await
}

pub(crate) async fn manifest_stack_setup_plan_at(
    request: &ManifestEnsureJobRequest,
    state_dir: Option<&Path>,
) -> Result<ManifestRuntimePlan, String> {
    manifest_stack_plan(
        &request.workspace,
        &request.manifest,
        &request.stack,
        None,
        state_dir,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn manifest_stack_task_setup_plan(
    request: &ManifestAppTaskJobRequest,
) -> Result<ManifestRuntimePlan, String> {
    manifest_stack_task_setup_plan_at(request, None).await
}

pub(crate) async fn manifest_stack_task_setup_plan_at(
    request: &ManifestAppTaskJobRequest,
    state_dir: Option<&Path>,
) -> Result<ManifestRuntimePlan, String> {
    manifest_stack_plan(
        &request.workspace,
        &request.manifest,
        &request.stack,
        Some(&request.task_name),
        state_dir,
    )
    .await
}

/// Re-read and validate one exact manifest snapshot before every engine
/// operation.  A selected task is injected only after it is proven to belong
/// to the selected stack; this remains a finite translation to the existing
/// typed setup primitives, not a generic manifest runner.
pub(crate) async fn manifest_stack_plan(
    request_workspace: &Path,
    request_manifest: &str,
    request_stack: &str,
    task_name: Option<&str>,
    state_dir: Option<&Path>,
) -> Result<ManifestRuntimePlan, String> {
    let (workspace, manifest) = load_native_manifest(request_workspace, request_manifest)?;
    let stack = manifest
        .stack(request_stack)
        .map_err(|_| "selected manifest stack does not exist".to_owned())?;
    // `default` is the only existing manifest declaration that selects an
    // application at document scope. Reuse that established selection rule
    // for daemon-start intent: exactly one explicit default wins, and a
    // one-stack manifest is implicitly selected. A non-default stack can
    // still be explicitly ensured and run; it is simply not a startup target.
    let autostart = manifest
        .default_stack()
        .is_ok_and(|default| default.name == stack.name);
    let macos_guest =
        derive_manifest_macos_guest(stack, &observe_manifest_guest_host_capability())?;
    if stack.env.len() > bosn_core::MAX_ENVIRONMENT_ENTRIES
        || stack.env.iter().any(|(key, value)| {
            key.is_empty()
                || key.contains('=')
                || key.contains('\0')
                || value.contains('\0')
                || value.len() > 16 * 1024
        })
    {
        return Err("selected manifest stack has unsafe environment data".into());
    }
    // The legacy manifest preserves bind-source spelling because it is also
    // used by the old Python executor.  The native runtime does not pass that
    // spelling to Docker.  It resolves it beneath this exact canonical
    // workspace and converts it into the typed setup representation first.
    //
    // The one exception is the host Docker engine socket at a fixed, closed
    // set of spellings: the manifest author's explicit choice to let the
    // container drive the host engine (for example `act`, whose job containers
    // are siblings). It is typed, never a generic host path.
    let mut mounts = Vec::new();
    let mut host_docker_socket = None;
    let proxy_dir = manifest_docker_proxy_dir(state_dir, stack.kind.is_some())?;
    for mount in &stack.mounts {
        if let Some(socket) = manifest_host_docker_socket(mount, proxy_dir.clone())? {
            if host_docker_socket.is_some() {
                return Err("manifest declares the host Docker socket more than once".into());
            }
            host_docker_socket = Some(socket);
            continue;
        }
        mounts.push(manifest_workspace_mount(&workspace, mount)?);
    }
    if host_docker_socket.is_some() && stack.kind.as_deref() == Some("macos-x64-guest") {
        return Err("the host Docker socket cannot be bound into a macOS guest stack".into());
    }
    // A dockurr bind exists outside the VM and is therefore intentionally not
    // a guest workdir. Keep the VM path separately for the typed SSH command;
    // the setup receipt itself must remain a pure guest container shape.
    let guest_workdir = macos_guest
        .as_ref()
        .and_then(|_| stack.workdir.clone())
        .map(|value| validate_manifest_guest_workdir(&value).map(|_| value))
        .transpose()?;
    let workdir = if macos_guest.is_some() {
        None
    } else {
        stack
            .workdir
            .as_deref()
            .map(|value| manifest_workdir_to_workspace_relative(&workspace, value, &mounts))
            .transpose()?
    };
    let tmpfs = manifest_tmpfs(stack)?;
    let (pinned_image, dockerfile_build, base_generation) = if stack.dockerfile.is_some() {
        if stack.image.is_some() {
            return Err("manifest Dockerfile build cannot also set image".into());
        }
        let build = manifest_dockerfile_build_plan(&manifest, stack, &workspace).await?;
        let generation = build.generation.clone();
        (None, Some(build), generation)
    } else {
        let image = stack
            .image
            .as_deref()
            .filter(|image| valid_manifest_pinned_image(image))
            .ok_or_else(|| {
                "selected manifest stack must use an immutable digest-pinned image".to_owned()
            })?
            .to_owned();
        let digest = image
            .rsplit_once("@sha256:")
            .map(|(_, value)| format!("sha256:{value}"))
            .expect("validated pinned image has digest");
        let base_generation = stack_generation_async(
            &manifest,
            stack,
            &workspace,
            &CollectorLimits::default(),
            &[ExternalImageIdentity {
                reference: image.clone(),
                platform: None,
                identity: Some(digest),
            }],
        )
        .await
        .map_err(|_| "manifest generation could not be derived".to_owned())?;
        (Some(image), None, base_generation)
    };
    // `bosn-generation` deliberately excludes workdir from the historical
    // content identity because legacy `docker exec` supplied it per task.
    // Native setup creates a persistent container with its workdir and binds,
    // so roll it whenever that effective lifecycle shape changes.
    let generation = manifest_runtime_generation(
        &base_generation,
        &mounts,
        guest_workdir.as_deref().or(workdir.as_deref()),
        &stack.volumes,
        &tmpfs,
        host_docker_socket.as_ref(),
        macos_guest.as_ref(),
    );
    let content_sha256 = generation
        .strip_prefix("sha256:")
        .ok_or_else(|| "manifest generation is invalid".to_owned())?
        .to_owned();
    let (source, app_source, asset_root) = if let Some(build) = dockerfile_build {
        let state_dir = state_dir
            .ok_or_else(|| {
                "manifest Dockerfile build requires daemon-owned state materialization".to_owned()
            })?
            .to_path_buf();
        let materialization_hash = content_sha256.clone();
        let materialization_dockerfile = build.dockerfile_path.clone();
        let materialization_entries = build.entries.clone();
        let asset_root = async_engine::launch_blocking(move || {
            SetupAssetStore::under_state_dir(state_dir)?.materialize_manifest_context(
                &materialization_hash,
                &materialization_dockerfile,
                &materialization_entries,
            )
        })
        .await
        .map_err(|_| "manifest Dockerfile materialization stopped".to_owned())?
        .map_err(|_| "manifest Dockerfile materialization was refused".to_owned())?;
        (
            SetupSource::InlineDockerfile(build.dockerfile_text),
            SetupPlanAppSource::InlineDockerfile {
                dockerfile_path: asset_root.join(build.dockerfile_path),
            },
            Some(asset_root),
        )
    } else {
        let image = pinned_image.expect("manifest source has image or Dockerfile");
        (
            SetupSource::PinnedImage(image.clone()),
            SetupPlanAppSource::PinnedImage { image },
            None,
        )
    };
    let mut tasks = BTreeMap::new();
    let mut guest_task = None;
    let mut secrets = Vec::new();
    let mut github_api_proxy = false;
    if let Some(task_name) = task_name {
        let task = manifest
            .task(task_name)
            .map_err(|_| "selected manifest task does not exist".to_owned())?;
        if task.stack != stack.name {
            return Err("selected manifest task does not belong to selected stack".into());
        }
        if task.cmd.len() > 16 * 1024 || task.cmd.contains('\0') {
            return Err("selected manifest task command is unsafe".into());
        }
        if !task.secrets.is_empty() && stack.guest.is_some() {
            return Err("manifest task secrets are not supported for macOS guest tasks".into());
        }
        if task.github_api_proxy && stack.guest.is_some() {
            return Err("github_api = \"proxy\" is not supported for macOS guest tasks".into());
        }
        secrets.clone_from(&task.secrets);
        github_api_proxy = task.github_api_proxy;
        tasks.insert(
            task.name.clone(),
            SetupTask {
                command: task.cmd.clone(),
                workdir: None,
                environment: BTreeMap::new(),
            },
        );
        if let Some(guest) = stack.guest.as_ref() {
            guest_task = Some(ManifestGuestTask {
                ssh_user: guest.ssh_user.clone(),
                ssh_port: u16::try_from(guest.ssh_port)
                    .map_err(|_| "macOS guest SSH port is invalid".to_owned())?,
                workdir: guest_workdir,
                command: task.cmd.clone(),
                payload: guest
                    .payload
                    .as_ref()
                    .map(|source| {
                        if !safe_manifest_relative_path(source) {
                            return Err(
                                "manifest guest payload must be a safe workspace-relative path"
                                    .to_owned(),
                            );
                        }
                        Ok(ManifestGuestPayload {
                            source: source.clone(),
                            destination: normalize_manifest_guest_payload_destination(
                                &guest.payload_destination,
                            )?,
                        })
                    })
                    .transpose()?,
            });
        }
    }
    let job_caches = manifest_job_caches(stack, host_docker_socket.is_some());
    let task_names = tasks.keys().cloned().collect();
    let workspace_string = workspace.to_string_lossy().into_owned();
    let volumes = manifest_named_volumes(stack, &workspace_string, &generation)?;
    let mut macos_guest = macos_guest;
    if let Some(guest) = &mut macos_guest {
        guest.storage_volume = volumes
            .iter()
            .find(|volume| volume.target == "/storage")
            .expect("validated macOS guest storage volume is materialized")
            .name
            .clone();
    }
    // A Linux manifest stack exists to host declared tasks, which run through
    // `docker exec`. Its PID 1 is therefore a fixed daemon-owned idle process
    // (the legacy runtime's semantics these manifests were written for), not
    // the image's default command: a base image such as debian defaults to an
    // interactive shell that exits at once. A guest keeps dockurr's entrypoint.
    let command = macos_guest
        .is_none()
        .then(|| MANIFEST_LINUX_IDLE_COMMAND.to_owned());
    let app = SetupApp {
        source,
        environment: stack.env.clone(),
        workdir,
        command,
        mounts,
    };
    Ok(ManifestRuntimePlan {
        plan: SetupPlan {
            source_kind: bosn_setup::SetupSourceKind::LocalFile,
            content_sha256,
            schema_version: bosn_core::SETUP_DOCUMENT_VERSION,
            workspace_root: workspace,
            asset_root,
            task_names,
            app,
            tasks,
            app_source,
            named_volumes: volumes
                .iter()
                .map(|volume| SetupNamedVolume {
                    name: volume.name.clone(),
                    target: volume.target.clone(),
                    labels: volume.labels.clone(),
                })
                .collect(),
            tmpfs,
            host_docker_socket,
            macos_guest,
        },
        generation,
        volumes,
        is_guest: stack.kind.as_deref() == Some("macos-x64-guest"),
        autostart,
        guest_task,
        secrets,
        github_api_proxy,
        job_caches,
    })
}

/// Whether this daemon routes tasks' Docker calls through its accounting
/// proxy; set once by [`Service::serve`] from the runner capacity. Plans
/// derived outside a serving daemon (tests, previews) never bind it.
pub(crate) static DOCKER_PROXY_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The proxy directory bound into a host-socket stack's container, when the
/// proxy is enabled. It is created here, because Docker refuses to bind a
/// missing source.
fn manifest_docker_proxy_dir(
    state_dir: Option<&Path>,
    guest: bool,
) -> Result<Option<String>, String> {
    let Some(state_dir) = state_dir else {
        return Ok(None);
    };
    if guest || !DOCKER_PROXY_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(None);
    }
    let dir = runners::proxy_dir(state_dir);
    runners::ensure_proxy_dir(&dir)
        .map_err(|error| format!("docker proxy directory unavailable: {error}"))?;
    dir.to_str()
        .map(|dir| Some(dir.to_owned()))
        .ok_or_else(|| "docker proxy directory is not UTF-8".to_owned())
}

fn manifest_job_caches(
    stack: &bosn_core::manifest::Stack,
    host_docker_socket: bool,
) -> Vec<runners::CacheRule> {
    if !host_docker_socket {
        return Vec::new();
    }
    let mut rules: Vec<runners::CacheRule> = stack
        .job_caches
        .iter()
        .map(|cache| runners::CacheRule {
            name: cache.name.clone(),
            volume: cache.volume.clone(),
            destination: cache.destination.clone(),
            scope: match cache.scope.as_str() {
                "machine" => runners::CacheScope::Machine,
                "workspace" => runners::CacheScope::Workspace,
                _ => runners::CacheScope::Repo,
            },
            mode: if cache.mode == "shared" {
                runners::CacheMode::Shared
            } else {
                runners::CacheMode::Exclusive
            },
            replicas: cache.replicas as usize,
        })
        .collect();
    if !rules
        .iter()
        .any(|rule| rule.volume.as_deref() == Some("act-toolcache"))
    {
        rules.push(runners::CacheRule::act_toolcache());
    }
    rules
}

/// Open a bounded local manifest exactly once for a native operation. The
/// parser rejects unknown stack fields, including dependency spellings; the
/// caller can therefore safely use its BTreeMap order as the only topology
/// currently represented by the legacy schema.
pub(crate) fn load_native_manifest(
    request_workspace: &Path,
    request_manifest: &str,
) -> Result<(PathBuf, bosn_core::Manifest), String> {
    let workspace = fs::canonical_context_path(request_workspace)
        .map_err(|_| "manifest workspace cannot be canonicalized".to_owned())?;
    let metadata = fs::context_path_metadata_no_follow(&workspace)
        .map_err(|_| "manifest workspace is not a directory".to_owned())?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err("manifest workspace is not a directory".into());
    }
    if !safe_manifest_relative_path(request_manifest) {
        return Err("manifest path must be a safe workspace-relative path".into());
    }
    let manifest_path = workspace.join(request_manifest);
    let manifest_path = fs::canonical_context_path(&manifest_path)
        .map_err(|_| "manifest file cannot be canonicalized".to_owned())?;
    if !manifest_path.starts_with(&workspace) {
        return Err("manifest path escapes selected workspace".into());
    }
    let bytes = fs::read_context_regular_file_bounded(&manifest_path, 1024 * 1024)
        .map_err(|_| "manifest file is not a bounded regular UTF-8 file".to_owned())?;
    let source =
        std::str::from_utf8(&bytes.bytes).map_err(|_| "manifest file is not UTF-8".to_owned())?;
    let manifest = parse_manifest_toml(
        source,
        ManifestRoots::new(
            "workspace manifest",
            workspace.to_string_lossy(),
            workspace.to_string_lossy(),
        ),
    )
    .map_err(|error| bounded_log_line(&format!("manifest is invalid: {error}")))?;
    Ok((workspace, manifest))
}

/// The current legacy TOML model has no dependency edge or root selector.
/// `BTreeMap` preserves its one unambiguous total order, so an all-stack
/// operation is deterministic across parsers and hosts. Unknown dependency
/// keys fail in `parse_manifest_toml` before this function is reached.
pub(crate) fn manifest_converge_stack_order(manifest: &bosn_core::Manifest) -> Vec<String> {
    manifest.stacks.keys().cloned().collect()
}

pub(crate) fn manifest_converge_stack_names(
    request: &ManifestConvergeJobRequest,
) -> Result<Vec<String>, String> {
    let (_, manifest) = load_native_manifest(&request.workspace, &request.manifest)?;
    let names = manifest_converge_stack_order(&manifest);
    if names.is_empty() {
        return Err("manifest declares no stacks".into());
    }
    Ok(names)
}
#[derive(Clone, Debug)]
pub(crate) struct ManifestRuntimePlan {
    pub(crate) plan: SetupPlan,
    pub(crate) generation: String,
    pub(crate) volumes: Vec<ManifestVolumeResource>,
    pub(crate) is_guest: bool,
    /// Derived only from the parsed document's existing default-stack
    /// selection semantics; never caller-provided.
    pub(crate) autostart: bool,
    /// Remote-only details retained outside `SetupPlan`: the setup container
    /// receipt must stay free of a Linux-container workdir for a VM guest.
    pub(crate) guest_task: Option<ManifestGuestTask>,
    /// Secret names the selected task declared (#308); names only.
    pub(crate) secrets: Vec<String>,
    /// The task declared `github_api = "proxy"`.
    pub(crate) github_api_proxy: bool,
    /// Cache mappings for the containers this stack's tasks start through
    /// the Docker proxy (#358): the manifest's `job_caches`, plus act's
    /// toolcache as an exclusive machine cache unless the manifest maps it.
    pub(crate) job_caches: Vec<runners::CacheRule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestGuestTask {
    pub(crate) ssh_user: String,
    pub(crate) ssh_port: u16,
    pub(crate) workdir: Option<String>,
    pub(crate) command: String,
    pub(crate) payload: Option<ManifestGuestPayload>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestGuestPayload {
    pub(crate) source: String,
    pub(crate) destination: String,
}

/// Read-only guest host facts composed from kernal-api's existing host and
/// filesystem facades. Bosn owns the product policy over those facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestGuestHostCapability {
    pub(crate) os: &'static str,
    pub(crate) kvm_available: bool,
    pub(crate) tun_available: bool,
}

pub(crate) fn observe_manifest_guest_host_capability() -> ManifestGuestHostCapability {
    let os = host::process_target().os;
    let device_available = |path: &str| {
        fs::context_path_metadata_no_follow(Path::new(path))
            .is_ok_and(|metadata| metadata.kind == fs::ContextPathKind::Other)
    };
    ManifestGuestHostCapability {
        os,
        kvm_available: os == "linux" && device_available("/dev/kvm"),
        tun_available: os == "linux" && device_available("/dev/net/tun"),
    }
}

/// Translate the one legacy guest kind into the finite setup representation.
/// Host observation remains separate from product policy so tests can prove
/// the refusal/default rules without a KVM host.
pub(crate) fn derive_manifest_macos_guest(
    stack: &bosn_core::manifest::Stack,
    capability: &ManifestGuestHostCapability,
) -> Result<Option<SetupMacosGuest>, String> {
    let (kind, guest) = (&stack.kind, &stack.guest);
    if kind.is_none() && guest.is_none() {
        return Ok(None);
    }
    let (Some(kind), Some(guest)) = (kind.as_deref(), guest) else {
        return Err("selected manifest stack uses an unsupported runtime field".into());
    };
    if kind != "macos-x64-guest" {
        return Err("selected manifest stack uses an unsupported runtime field".into());
    }
    if !stack
        .image
        .as_deref()
        .is_some_and(valid_manifest_macos_guest_image)
    {
        return Err(
            "macOS guest must use dockurr/macos (or a Docker Hub registry alias) pinned by sha256 digest"
                .into(),
        );
    }
    if stack.dockerfile.is_some() {
        return Err(
            "macOS guest stack must use an immutable guest image, not Dockerfile build".into(),
        );
    }
    if guest.ssh_host != "127.0.0.1" {
        return Err(
            "macOS guest SSH transport is fixed to 127.0.0.1; guest.ssh_host must be 127.0.0.1"
                .into(),
        );
    }
    if !valid_manifest_guest_ssh_user(&guest.ssh_user) {
        return Err("macOS guest ssh_user is unsafe for the typed SSH transport".into());
    }
    validate_manifest_macos_guest_storage(stack)?;
    if [
        guest.version.as_str(),
        guest.ram_size.as_str(),
        guest.disk_size.as_str(),
    ]
    .into_iter()
    .any(|value| value.is_empty() || value.len() > 128 || value.contains(['\0', '\n', '\r', '=']))
    {
        return Err("macOS guest sizing fields are unsafe".into());
    }
    if capability.os != "linux" || !capability.kvm_available || !capability.tun_available {
        return Err("macOS guest requires a Linux host with /dev/kvm and /dev/net/tun available to the Bosn daemon".into());
    }
    // The legacy runtime used a CPU-vendor probe only to choose a default and
    // took the one-core path on AMD. Without a dedicated kernel CPU-vendor
    // capability, one core is the safe portable default; explicit manifests
    // retain their declared finite CPU count.
    let cpu_cores = guest.cpu_cores.unwrap_or(1);
    let cpu_cores = u16::try_from(cpu_cores)
        .map_err(|_| "macOS guest cpu_cores exceeds native limit".to_owned())?;
    Ok(Some(SetupMacosGuest {
        ssh_port: u16::try_from(guest.ssh_port)
            .map_err(|_| "macOS guest ssh port is invalid".to_owned())?,
        web_port: u16::try_from(guest.web_port)
            .map_err(|_| "macOS guest web port is invalid".to_owned())?,
        version: guest.version.clone(),
        ram_size: guest.ram_size.clone(),
        disk_size: guest.disk_size.clone(),
        cpu_cores,
        storage_volume: String::new(),
        storage_scope: Scope::Machine,
        storage_retention: Retention::Pinned,
    }))
}

pub(crate) fn valid_manifest_guest_ssh_user(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

pub(crate) fn validate_manifest_guest_workdir(value: &str) -> Result<(), String> {
    if !normalized_container_path(value) {
        return Err("macOS guest workdir is not a normalized absolute path".into());
    }
    Ok(())
}

pub(crate) fn validate_manifest_macos_guest_storage(
    stack: &bosn_core::manifest::Stack,
) -> Result<(), String> {
    let storage = stack
        .volumes
        .iter()
        .filter(|volume| volume.mount_at() == "/storage")
        .collect::<Vec<_>>();
    let [storage] = storage.as_slice() else {
        return Err("macOS guest requires exactly one declared durable `storage` volume mounted at /storage".into());
    };
    if storage.name != "storage"
        || storage.scope != Scope::Machine
        || storage.retention != Retention::Pinned
    {
        return Err("macOS guest storage must be named `storage` with scope = `machine`, destination = `/storage`, and retention = `pinned`".into());
    }
    Ok(())
}
