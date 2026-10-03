//! Deriving a manifest stack's runtime: Dockerfile builds, volumes, tmpfs, mounts, generation.

use super::*;

/// One immutable, already-selected Docker build context. It carries typed
/// entries rather than workspace paths so the later setup materializer cannot
/// be redirected by a changed workspace entry.
#[derive(Clone, Debug)]
pub(crate) struct ManifestDockerfileBuild {
    pub(crate) generation: String,
    pub(crate) dockerfile_path: String,
    pub(crate) dockerfile_text: String,
    pub(crate) entries: Vec<ManifestBuildEntry>,
}

/// Collect and authorize the exact build bytes for the deliberately narrow
/// manifest-Dockerfile form.  This happens on the kernel blocking lane once;
/// the returned bytes are then copied into owner-private setup state, rather
/// than handing Docker the selected workspace path.
pub(crate) async fn manifest_dockerfile_build_plan(
    manifest: &bosn_core::Manifest,
    stack: &bosn_core::manifest::Stack,
    workspace: &Path,
) -> Result<ManifestDockerfileBuild, String> {
    let manifest = manifest.clone();
    let stack = stack.clone();
    let workspace = workspace.to_path_buf();
    async_engine::launch_blocking(move || {
        let dockerfile = stack
            .dockerfile
            .as_deref()
            .ok_or_else(|| "manifest Dockerfile build is missing its Dockerfile".to_owned())?;
        let context = collect_context(&workspace, Some(dockerfile), &CollectorLimits::default())
            .map_err(|_| "manifest Dockerfile context was refused".to_owned())?;
        let mut entries = Vec::new();
        for entry in &context.entries {
            match entry {
                ContextEntry::File {
                    path,
                    bytes,
                    executable,
                } => entries.push(ManifestBuildEntry::File {
                    path: path.clone(),
                    content: bytes.clone(),
                    executable: *executable,
                }),
                ContextEntry::Directory { path } => {
                    entries.push(ManifestBuildEntry::Directory { path: path.clone() })
                }
                ContextEntry::Symlink { path, target } => {
                    entries.push(ManifestBuildEntry::Symlink {
                        path: path.clone(),
                        target: target.clone(),
                    })
                }
            }
        }
        let dockerfile_bytes = context
            .entries
            .iter()
            .find_map(|entry| match entry {
                ContextEntry::File { path, bytes, .. } if path == dockerfile => {
                    Some(bytes.as_slice())
                }
                _ => None,
            })
            .ok_or_else(|| "manifest Dockerfile context has no root Dockerfile".to_owned())?;
        let dockerfile_text = std::str::from_utf8(dockerfile_bytes)
            .map_err(|_| "manifest Dockerfile is not UTF-8".to_owned())?
            .to_owned();
        let required = external_images(&dockerfile_text)
            .map_err(|_| "manifest Dockerfile uses an unsupported build form".to_owned())?;
        let mut observed = Vec::new();
        let mut identities = std::collections::BTreeSet::new();
        for image in required {
            if !valid_manifest_pinned_image(&image.reference) {
                return Err(unpinned_dockerfile_image_message(
                    dockerfile,
                    &image.reference,
                ));
            }
            if !identities.insert((image.reference.clone(), image.platform.clone())) {
                return Err("manifest Dockerfile repeats an external image declaration".into());
            }
            let digest = image
                .reference
                .rsplit_once("@sha256:")
                .map(|(_, digest)| format!("sha256:{digest}"))
                .expect("validated image has a digest");
            observed.push(ExternalImageIdentity {
                reference: image.reference,
                platform: image.platform,
                identity: Some(digest),
            });
        }
        let generation = stack_generation_from_context(&manifest, &stack, &context, &observed)
            .map_err(|_| "manifest Dockerfile generation could not be derived".to_owned())?;
        Ok(ManifestDockerfileBuild {
            generation,
            dockerfile_path: dockerfile.into(),
            dockerfile_text,
            entries,
        })
    })
    .await
    .map_err(|_| "manifest Dockerfile planning stopped".to_owned())?
}

/// Name the offending `FROM` reference and the exact remedy. A tag is mutable,
/// so Bosn could not prove which bytes a generation was built from.
pub(crate) fn unpinned_dockerfile_image_message(dockerfile: &str, reference: &str) -> String {
    let reference = bounded_log_line(reference);
    format!(
        "manifest Dockerfile external images must be immutable digest-pinned: {dockerfile} uses {reference}. Pin it as `FROM {reference}@sha256:<digest>` (find the digest with `docker buildx imagetools inspect {reference}`)"
    )
}

pub(crate) fn manifest_named_volumes(
    stack: &bosn_core::manifest::Stack,
    workspace: &str,
    generation: &str,
) -> Result<Vec<ManifestVolumeResource>, String> {
    let mut targets = std::collections::BTreeSet::new();
    stack
        .volumes
        .iter()
        .map(|volume| {
            let target = volume.mount_at();
            if !normalized_container_path(&target) || !targets.insert(target.clone()) {
                return Err("manifest volume target is unsafe or duplicated".into());
            }
            let scope_key = match volume.scope {
                Scope::Spec => format!("{workspace}\0{generation}"),
                Scope::Stack => workspace.into(),
                Scope::Machine => stack.family.clone().unwrap_or_else(|| stack.name.clone()),
            };
            let mut hasher = Sha256Hasher::new();
            manifest_generation_field(&mut hasher, b"bosn-manifest-volume-v1");
            manifest_generation_field(&mut hasher, stack.name.as_bytes());
            manifest_generation_field(&mut hasher, volume.name.as_bytes());
            manifest_generation_field(&mut hasher, scope_key.as_bytes());
            let identity = hasher.finalize().to_string();
            let name = format!("bosn-v-{}-{}", volume.scope.as_str(), &identity[..24]);
            let labels = BTreeMap::from([
                ("com.zackees.bosn.setup-managed".into(), "v1".into()),
                (
                    "com.zackees.bosn.setup-content-sha256".into(),
                    identity.clone(),
                ),
                ("com.zackees.bosn.setup-container".into(), name.clone()),
            ]);
            Ok(ManifestVolumeResource {
                id: format!("manifest-volume:{name}"),
                name,
                stack: stack.name.clone(),
                // Stack/machine volumes retain this stable identity across a
                // container generation rollover; spec identity includes the
                // parent generation in `scope_key` above.
                generation: format!("sha256:{identity}"),
                scope: volume.scope,
                workspace: workspace.into(),
                retention: volume.retention,
                target,
                labels,
            })
        })
        .collect()
}

/// Translate the limited legacy
/// `tmpfs = ["/target[:ro|rw][,size=N{b|k|m|g}][,exec|noexec][,mode=OCTAL]"]`
/// shape to typed setup data.  Every other option (`uid`, `nosuid`, ...), a
/// repeated option, or an unknown size unit fails closed instead of becoming a
/// Docker option string.
pub(crate) fn manifest_tmpfs(
    stack: &bosn_core::manifest::Stack,
) -> Result<Vec<SetupTmpfs>, String> {
    let mut targets = std::collections::BTreeSet::new();
    stack
        .tmpfs
        .iter()
        .map(|tmpfs| {
            if !normalized_container_path(&tmpfs.destination)
                || !targets.insert(tmpfs.destination.clone())
            {
                return Err("manifest tmpfs target is unsafe or duplicated".into());
            }
            let (_, raw_options) = tmpfs
                .value
                .split_once(':')
                .map(|(destination, options)| (destination, Some(options)))
                .unwrap_or((tmpfs.value.as_str(), None));
            let mut readonly = false;
            let mut mode_seen = false;
            let mut size = None;
            let mut exec = None;
            let mut mode = None;
            if let Some(raw_options) = raw_options {
                for option in raw_options.split(',') {
                    match option {
                        "ro" => {
                            if mode_seen {
                                return Err("manifest tmpfs mode is repeated".into());
                            }
                            readonly = true;
                            mode_seen = true;
                        }
                        "rw" => {
                            if mode_seen {
                                return Err("manifest tmpfs mode is repeated".into());
                            }
                            readonly = false;
                            mode_seen = true;
                        }
                        value if value.starts_with("size=") => {
                            if size.is_some() {
                                return Err("manifest tmpfs size is repeated".into());
                            }
                            size = Some(parse_manifest_tmpfs_size(&value[5..])?);
                        }
                        "exec" | "noexec" => {
                            if exec.is_some() {
                                return Err("manifest tmpfs exec option is repeated".into());
                            }
                            exec = Some(option == "exec");
                        }
                        value if value.starts_with("mode=") => {
                            if mode.is_some() {
                                return Err("manifest tmpfs mode= option is repeated".into());
                            }
                            mode = Some(parse_manifest_tmpfs_mode(&value[5..])?);
                        }
                        _ => return Err(
                            "manifest tmpfs uses an unsupported option; supported options are ro, rw, size=N{b,k,m,g}, exec, noexec, and mode=OCTAL"
                                .into(),
                        ),
                    }
                }
            }
            Ok(SetupTmpfs {
                target: tmpfs.destination.clone(),
                readonly,
                size,
                exec,
                mode,
            })
        })
        .collect()
}

/// A tmpfs `mode=` is octal permission bits only: 1-4 octal digits, at most
/// `0o7777`, so it cannot smuggle a second option or a sign.
pub(crate) fn parse_manifest_tmpfs_mode(value: &str) -> Result<u32, String> {
    if value.is_empty() || value.len() > 4 || !value.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err("manifest tmpfs mode must be 1-4 octal digits".into());
    }
    u32::from_str_radix(value, 8).map_err(|_| "manifest tmpfs mode is invalid".to_owned())
}

pub(crate) fn parse_manifest_tmpfs_size(value: &str) -> Result<SetupTmpfsSize, String> {
    let (digits, unit) = value.strip_suffix('b').map_or_else(
        || value.split_at(value.len().saturating_sub(1)),
        |digits| (digits, "b"),
    );
    let (digits, unit) = if matches!(unit, "k" | "m" | "g") {
        (digits, unit)
    } else if value.ends_with('b') {
        (digits, "b")
    } else {
        return Err("manifest tmpfs size has an unsupported unit".into());
    };
    let value = digits
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "manifest tmpfs size is invalid".to_owned())?;
    let unit = match unit {
        "b" => SetupTmpfsSizeUnit::Bytes,
        "k" => SetupTmpfsSizeUnit::Kibibytes,
        "m" => SetupTmpfsSizeUnit::Mebibytes,
        "g" => SetupTmpfsSizeUnit::Gibibytes,
        _ => unreachable!("unit was validated above"),
    };
    Ok(SetupTmpfsSize { value, unit })
}

/// Recognize an explicit bind of the host Docker engine socket. Only the fixed
/// spellings in [`SetupHostDockerSocketSource`] qualify, and the host path
/// must currently be a Unix socket. Everything created through the socket is
/// outside Bosn supervision; that is the manifest author's declared choice.
pub(crate) fn manifest_host_docker_socket(
    mount: &bosn_core::manifest::Mount,
    proxy_dir: Option<String>,
) -> Result<Option<SetupHostDockerSocket>, String> {
    let Some(source) = SetupHostDockerSocketSource::from_host_path(&mount.source) else {
        return Ok(None);
    };
    if !normalized_container_path(&mount.destination) {
        return Err("manifest mount target is not a normalized absolute path".into());
    }
    require_host_docker_socket(source.host_path())?;
    Ok(Some(SetupHostDockerSocket {
        source,
        target: mount.destination.clone(),
        readonly: mount.readonly,
        proxy_dir,
    }))
}

#[cfg(unix)]
pub(crate) fn require_host_docker_socket(path: &str) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    let metadata = std::fs::metadata(path).map_err(|_| {
        format!("manifest binds the host Docker socket, but {path} does not exist on this host")
    })?;
    if !metadata.file_type().is_socket() {
        return Err(format!(
            "manifest binds the host Docker socket, but {path} is not a Unix socket"
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn require_host_docker_socket(_path: &str) -> Result<(), String> {
    Err("the host Docker socket bind is supported only on Unix hosts".into())
}

/// Convert one legacy manifest bind into the narrower workspace-relative setup
/// bind.  Absolute legacy sources are accepted only when their canonical path
/// is inside the selected workspace; callers never get to name a host path at
/// the typed engine boundary.
pub(crate) fn manifest_workspace_mount(
    workspace: &Path,
    mount: &bosn_core::manifest::Mount,
) -> Result<bosn_core::WorkspaceMount, String> {
    let source = manifest_workspace_member(workspace, &mount.source)?;
    if !normalized_container_path(&mount.destination) {
        return Err("manifest mount target is not a normalized absolute path".into());
    }
    Ok(bosn_core::WorkspaceMount {
        source,
        target: mount.destination.clone(),
        readonly: mount.readonly,
    })
}

/// Resolve a legacy source without allowing an absolute path, `..`, or a
/// symlink to redirect the bind outside the selected canonical workspace.
/// The output is canonical workspace-relative spelling suitable for
/// `WorkspaceMount`, not the original caller/manifest spelling.
pub(crate) fn manifest_workspace_member(workspace: &Path, source: &str) -> Result<String, String> {
    if source.is_empty()
        || source.len() > 4096
        || source.contains(['\0', '\\'])
        || is_windows_absolute_path(source)
    {
        return Err("manifest mount source is unsafe".into());
    }
    let raw = Path::new(source);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        if source
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
            && source != "."
        {
            return Err("manifest mount source is not a normalized workspace path".into());
        }
        workspace.join(raw)
    };
    let metadata = fs::context_path_metadata_no_follow(&candidate)
        .map_err(|_| "declared manifest mount source does not exist".to_owned())?;
    if metadata.kind == fs::ContextPathKind::Symlink {
        return Err("declared manifest mount source is a symlink".into());
    }
    let canonical = fs::canonical_context_path(&candidate)
        .map_err(|_| "declared manifest mount source cannot be canonicalized".to_owned())?;
    let relative = canonical
        .strip_prefix(workspace)
        .map_err(|_| {
            "declared manifest mount source escapes workspace; a bind source must be inside the workspace (the only host path accepted is the Docker socket, /var/run/docker.sock or /run/docker.sock). Move the data under the workspace or declare a Bosn-managed [stack.NAME.volumes] entry instead".to_owned()
        })?;
    if relative.as_os_str().is_empty() {
        return Ok(".".into());
    }
    let relative = relative
        .to_str()
        .ok_or_else(|| "declared manifest mount source is not UTF-8".to_owned())?;
    if relative.is_empty()
        || relative.contains(['\0', '\\', ','])
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("declared manifest mount source is unsafe".into());
    }
    Ok(relative.into())
}

/// A manifest workdir is an absolute in-container path.  The setup primitive
/// deliberately stores only workspace-relative workdirs, so translate it
/// through the most-specific declared bind and reject image-only workdirs.
pub(crate) fn manifest_workdir_to_workspace_relative(
    workspace: &Path,
    workdir: &str,
    mounts: &[bosn_core::WorkspaceMount],
) -> Result<String, String> {
    if !normalized_container_path(workdir) {
        return Err("manifest workdir is not a normalized absolute path".into());
    }
    let selected = mounts
        .iter()
        .filter(|mount| container_prefix(workdir, &mount.target))
        .max_by_key(|mount| mount.target.len())
        .ok_or_else(|| {
            "manifest workdir is not covered by a declared workspace mount".to_owned()
        })?;
    let suffix = container_relative_suffix(workdir, &selected.target)
        .expect("container_prefix selected the manifest workdir mount");
    let relative = if selected.source == "." {
        if suffix.is_empty() {
            ".".into()
        } else {
            suffix.into()
        }
    } else if suffix.is_empty() {
        selected.source.clone()
    } else {
        format!("{}/{suffix}", selected.source)
    };
    // A bind of a regular file cannot meaningfully be an application working
    // directory. Check it here and the typed setup primitive will canonicalize
    // the same source again immediately before ensure/task application.
    let source = workspace.join(&selected.source);
    let metadata = fs::context_path_metadata_no_follow(&source)
        .map_err(|_| "manifest workdir mount source no longer exists".to_owned())?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err("manifest workdir must map through a directory bind mount".into());
    }
    Ok(relative)
}

pub(crate) fn normalized_container_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.contains(['\0', '\\', ','])
        && value.starts_with('/')
        && (value == "/"
            || !value[1..]
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".."))
}

pub(crate) fn container_prefix(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) fn container_relative_suffix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix == "/" {
        return Some(path.strip_prefix('/').unwrap_or(path));
    }
    if path == prefix {
        Some("")
    } else {
        path.strip_prefix(prefix)?.strip_prefix('/')
    }
}

pub(crate) fn is_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

pub(crate) fn manifest_runtime_generation(
    base_generation: &str,
    mounts: &[bosn_core::WorkspaceMount],
    workdir: Option<&str>,
    volumes: &[bosn_core::manifest::Volume],
    tmpfs: &[SetupTmpfs],
    host_docker_socket: Option<&SetupHostDockerSocket>,
    macos_guest: Option<&SetupMacosGuest>,
) -> String {
    let mut hasher = Sha256Hasher::new();
    manifest_generation_field(&mut hasher, b"bosn-manifest-runtime-v1");
    manifest_generation_field(&mut hasher, base_generation.as_bytes());
    manifest_generation_field(&mut hasher, &(mounts.len() as u64).to_be_bytes());
    for mount in mounts {
        manifest_generation_field(&mut hasher, mount.source.as_bytes());
        manifest_generation_field(&mut hasher, mount.target.as_bytes());
        manifest_generation_field(&mut hasher, if mount.readonly { b"1" } else { b"0" });
    }
    manifest_generation_field(&mut hasher, workdir.unwrap_or("").as_bytes());
    manifest_generation_field(&mut hasher, &(volumes.len() as u64).to_be_bytes());
    for volume in volumes {
        manifest_generation_field(&mut hasher, volume.name.as_bytes());
        manifest_generation_field(&mut hasher, volume.scope.as_str().as_bytes());
        manifest_generation_field(&mut hasher, volume.mount_at().as_bytes());
        manifest_generation_field(
            &mut hasher,
            match volume.retention {
                Retention::Warm => b"warm",
                Retention::Pinned => b"pinned",
            },
        );
    }
    manifest_generation_field(&mut hasher, &(tmpfs.len() as u64).to_be_bytes());
    let mut tmpfs = tmpfs.to_vec();
    tmpfs.sort_by(|left, right| left.target.cmp(&right.target));
    for tmpfs in tmpfs {
        manifest_generation_field(&mut hasher, tmpfs.target.as_bytes());
        manifest_generation_field(&mut hasher, if tmpfs.readonly { b"ro" } else { b"rw" });
        if let Some(size) = tmpfs.size {
            manifest_generation_field(&mut hasher, &size.value.to_be_bytes());
            manifest_generation_field(
                &mut hasher,
                match size.unit {
                    SetupTmpfsSizeUnit::Bytes => b"b",
                    SetupTmpfsSizeUnit::Kibibytes => b"k",
                    SetupTmpfsSizeUnit::Mebibytes => b"m",
                    SetupTmpfsSizeUnit::Gibibytes => b"g",
                },
            );
        } else {
            manifest_generation_field(&mut hasher, b"no-size");
        }
        // Hashed only when declared, so a manifest without these options
        // keeps its pre-existing runtime generation.
        if let Some(exec) = tmpfs.exec {
            manifest_generation_field(&mut hasher, if exec { b"exec" } else { b"noexec" });
        }
        if let Some(mode) = tmpfs.mode {
            manifest_generation_field(&mut hasher, b"mode");
            manifest_generation_field(&mut hasher, &mode.to_be_bytes());
        }
    }
    // Hashed only when declared, so manifests without it keep their
    // pre-existing runtime generation.
    if let Some(socket) = host_docker_socket {
        manifest_generation_field(&mut hasher, b"host-docker-socket:v1");
        manifest_generation_field(&mut hasher, socket.source.host_path().as_bytes());
        manifest_generation_field(&mut hasher, socket.target.as_bytes());
        manifest_generation_field(&mut hasher, if socket.readonly { b"1" } else { b"0" });
        // Hashed only when present, so a daemon without the proxy keeps the
        // generation (and container) it had before #358.
        if let Some(dir) = &socket.proxy_dir {
            manifest_generation_field(&mut hasher, b"docker-proxy-dir:v1");
            manifest_generation_field(&mut hasher, dir.as_bytes());
        }
    }
    match macos_guest {
        None => {
            manifest_generation_field(&mut hasher, b"macos-guest:none");
            manifest_generation_field(&mut hasher, MANIFEST_LINUX_IDLE_COMMAND.as_bytes());
        }
        Some(guest) => {
            manifest_generation_field(&mut hasher, b"macos-guest:v1");
            manifest_generation_field(&mut hasher, &guest.ssh_port.to_be_bytes());
            manifest_generation_field(&mut hasher, &guest.web_port.to_be_bytes());
            manifest_generation_field(&mut hasher, guest.version.as_bytes());
            manifest_generation_field(&mut hasher, guest.ram_size.as_bytes());
            manifest_generation_field(&mut hasher, guest.disk_size.as_bytes());
            manifest_generation_field(&mut hasher, &guest.cpu_cores.to_be_bytes());
        }
    }
    format!("sha256:{}", hasher.finalize())
}

pub(crate) fn manifest_generation_field(hasher: &mut Sha256Hasher, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

pub(crate) fn safe_manifest_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.contains('\0')
        && !value.contains('\\')
        && !value.starts_with('/')
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

pub(crate) fn valid_manifest_pinned_image(value: &str) -> bool {
    let Some((name, digest)) = value.rsplit_once("@sha256:") else {
        return false;
    };
    !name.is_empty()
        && value.len() <= 512
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Permit only Docker Hub spellings for dockurr's macOS entrypoint image. An
/// explicit final-component tag remains acceptable because the sha256 digest
/// is the identity; a registry port is not mistaken for a tag.
pub(crate) fn valid_manifest_macos_guest_image(value: &str) -> bool {
    if !valid_manifest_pinned_image(value) {
        return false;
    }
    let (name, _) = value
        .rsplit_once("@sha256:")
        .expect("validated pinned guest image has digest");
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
    )
}
