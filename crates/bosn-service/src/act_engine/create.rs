//! The frozen engine profile, its Docker create arguments, and owned creation and removal.

use super::*;

// Cgroup nesting follows the pinned image's DIND_COMMIT 8d9e3502.../hack/dind.
// Only the private cgroup namespace root is touched; startup is finite and
// deliberately omits the publisher entrypoint's TCP listeners and extra mounts.
pub(super) const ENGINE_INIT: &str = r#"set -eu
[ "$$" -eq 1 ] || { echo 'engine init requires PID 1' >&2; exit 1; }
IFS= read -r group < /proc/self/cgroup
[ "$group" = '0::/' ] || { echo 'engine init requires private cgroup v2 root' >&2; exit 1; }
[ -f /sys/fs/cgroup/cgroup.controllers ] || exit 1
mkdir -p /sys/fs/cgroup/init
IFS= read -r controllers < /sys/fs/cgroup/cgroup.controllers
[ -n "$controllers" ] || exit 1
controls=
for controller in $controllers; do
    case "$controller" in *[!a-z0-9_]*|'') exit 1 ;; esac
    controls="$controls +$controller"
done
attempt=0
while :; do
    while IFS= read -r pid; do
        case "$pid" in *[!0-9]*|'') exit 1 ;; esac
        printf '%s\n' "$pid" > /sys/fs/cgroup/init/cgroup.procs || :
    done < /sys/fs/cgroup/cgroup.procs
    if { printf '%s\n' "$controls" > /sys/fs/cgroup/cgroup.subtree_control; } 2>/dev/null; then break; fi
    attempt=$((attempt + 1))
    [ "$attempt" -lt 32 ] || { echo 'private cgroup controllers unavailable' >&2; exit 1; }
    sleep 0.01
done
exec docker-init -- "$@"
"#;
pub(crate) fn engine_command() -> Vec<String> {
    [
        "sh",
        "-ec",
        ENGINE_INIT,
        "bosn-act-engine-init",
        "dockerd",
        "--feature=containerd-snapshotter=true",
        // overlayfs on the private storage: a new container shares the image's
        // layers instead of copying them, as `native` did (~5 s and ~5 GiB
        // of RAM-backed storage per job container, plus a slower image load).
        "--storage-driver=overlayfs",
        "--data-root=/var/lib/docker",
        "--exec-root=/run/docker",
        "--host=unix:///var/run/docker.sock",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
/// Where the engine's Docker storage (`/var/lib/docker`) is mounted.
pub(crate) const STORAGE_TARGET: &str = "/var/lib/docker";

/// What backs an engine's Docker storage (#425).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineStorage {
    /// A private exec tmpfs: RAM, inside the engine's memory limit, capped at
    /// `storage_bytes`.
    Memory,
    /// A labelled per-run volume on the host engine's disk, independently
    /// removed after the engine. `storage_bytes` is the free-disk budget it was sized against,
    /// not a quota, and none of it counts against memory.
    Disk,
}

impl EngineStorage {
    pub(crate) fn policy(self) -> ActEngineTmpfsPolicy {
        match self {
            Self::Memory => ActEngineTmpfsPolicy::StorageExecRunTmpNoexecV1,
            Self::Disk => ActEngineTmpfsPolicy::NamedDiskStorageRunTmpNoexecV2,
        }
    }

    pub(crate) fn of(policy: ActEngineTmpfsPolicy) -> Self {
        match policy {
            ActEngineTmpfsPolicy::StorageExecRunTmpNoexecV1 => Self::Memory,
            ActEngineTmpfsPolicy::DiskStorageRunTmpNoexecV1
            | ActEngineTmpfsPolicy::NamedDiskStorageRunTmpNoexecV2 => Self::Disk,
        }
    }
}

/// Limits apply to the entire isolated engine and all its descendants.
/// Storage is a private tmpfs or an anonymous disk volume ([`EngineStorage`]);
/// the engine's writable root is disabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActEngineLimits {
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub storage: EngineStorage,
    pub nano_cpus: u64,
    pub pids: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActEngineError(pub String);
impl fmt::Display for ActEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ActEngineError {}

impl ActEngineLimits {
    pub(crate) fn validate(self) -> Result<(), ActEngineError> {
        // Only memory-backed storage is carved out of the memory limit.
        let in_memory = match self.storage {
            EngineStorage::Memory => self.storage_bytes,
            EngineStorage::Disk => (16 << 20) + (64 << 20),
        };
        if self.storage_bytes < 1 << 20
            || self.storage_bytes > i64::MAX as u64
            || self.memory_bytes.saturating_sub(in_memory) < 512 << 20
            || self.memory_bytes > i64::MAX as u64
            || self.nano_cpus < 1_000_000
            || self.nano_cpus > 256_000_000_000
            || self.pids == 0
            || self.pids > 65536
        {
            return Err(ActEngineError("invalid bounded Act engine limits".into()));
        }
        Ok(())
    }

    pub(super) fn tmpfs(self) -> BTreeMap<String, String> {
        let storage =
            (self.storage == EngineStorage::Memory).then_some((STORAGE_TARGET, self.storage_bytes));
        storage
            .into_iter()
            .chain([("/run", 16 << 20), ("/tmp", 64 << 20)])
            // Docker 29.7.2 daemon/oci_linux.go defaults user tmpfs to noexec.
            // Snapshots must execute container binaries; only their private
            // storage mount clears that default. /run and /tmp remain noexec.
            .map(|(path, bytes)| {
                let execution = if path == STORAGE_TARGET { ",exec" } else { "" };
                (
                    path.into(),
                    format!("rw{execution},nosuid,nodev,size={bytes}"),
                )
            })
            .collect()
    }
}

pub(crate) fn command_digest(command: &[String]) -> Result<String, ActEngineError> {
    if command.is_empty()
        || command.len() > 64
        || command.iter().any(|part| part.contains('\0'))
        || command.iter().map(String::len).sum::<usize>() > 65536
    {
        return Err(ActEngineError("unbounded engine command identity".into()));
    }
    let mut bytes = b"bosn-act-init-command/v1\0".to_vec();
    bytes.extend(serde_json::to_vec(command).map_err(|error| ActEngineError(error.to_string()))?);
    Ok(kernal_api::hash::Sha256Hasher::digest(&bytes).to_string())
}

/// Freeze creation inputs before the registry intent commits. Recovery reads
/// this profile rather than guessing the limits or command of the current build.
pub(crate) fn creation_profile(
    limits: ActEngineLimits,
) -> Result<ActEngineCreationProfile, ActEngineError> {
    limits.validate()?;
    let profile = ActEngineCreationProfile {
        memory_bytes: limits.memory_bytes,
        storage_bytes: limits.storage_bytes,
        nano_cpus: limits.nano_cpus,
        pids: limits.pids,
        run_tmpfs_bytes: 16 << 20,
        tmp_tmpfs_bytes: 64 << 20,
        tmpfs_policy: limits.storage.policy(),
        init_command_sha256: command_digest(&engine_command())?,
        cache_volume: None,
        cache_coordination: None,
        tool_generation: None,
    };
    profile
        .validate()
        .map_err(|error| ActEngineError(error.to_string()))?;
    Ok(profile)
}

pub(crate) fn frozen_limits(intent: &ActEngineIntent) -> Result<ActEngineLimits, ActEngineError> {
    let profile = intent.creation_profile.as_ref().ok_or_else(|| {
        ActEngineError("legacy engine has no frozen creation profile; execution is refused".into())
    })?;
    profile
        .validate()
        .map_err(|error| ActEngineError(error.to_string()))?;
    let limits = ActEngineLimits {
        memory_bytes: profile.memory_bytes,
        storage_bytes: profile.storage_bytes,
        storage: EngineStorage::of(profile.tmpfs_policy),
        nano_cpus: profile.nano_cpus,
        pids: profile.pids,
    };
    limits.validate()?;
    Ok(limits)
}

/// Only the trusted daemon may use these arguments, after its intent commits.
/// No source directory, host socket, credentials or anonymous volume is bound.
pub fn create_arguments(
    intent: &ActEngineIntent,
    owner: &str,
    limits: ActEngineLimits,
) -> Result<Vec<String>, ActEngineError> {
    limits.validate()?;
    let cache = intent
        .creation_profile
        .as_ref()
        .and_then(|profile| profile.cache_volume.clone());
    if cache.is_some() {
        let act = crate::ci::pins::act_artifact("amd64")
            .ok_or_else(|| ActEngineError("no pinned startup act artifact".into()))?;
        if intent.act_version != crate::ci::pins::ACT_VERSION
            || intent.act_image_digest != format!("sha256:{}", act.sha256)
        {
            return Err(ActEngineError(
                "startup act differs from frozen intent".into(),
            ));
        }
    }
    let generation = intent
        .creation_profile
        .as_ref()
        .and_then(|profile| profile.tool_generation.clone());
    let expected_profile = creation_profile_with_tools(limits, cache.clone(), generation.clone())?;
    if intent.creation_profile.as_ref() != Some(&expected_profile) {
        return Err(ActEngineError(
            "creation differs from frozen engine profile".into(),
        ));
    }
    let labels = intent
        .required_labels(owner)
        .map_err(|e| ActEngineError(e.to_string()))?;
    let mut args = vec![
        "create".into(),
        "--pull".into(),
        "never".into(),
        "--name".into(),
        intent.engine_name(),
        "--privileged".into(),
        "--read-only".into(),
        // The image's entrypoint otherwise adds a TCP dockerd listener.
        // Execute only our verified Unix-socket command directly.
        "--entrypoint".into(),
        "".into(),
        "--cgroupns".into(),
        "private".into(),
        "--ipc".into(),
        "private".into(),
        "--network".into(),
        "bridge".into(),
        "--memory".into(),
        limits.memory_bytes.to_string(),
        "--memory-swap".into(),
        limits.memory_bytes.to_string(),
        "--cpu-period".into(),
        "100000".into(),
        "--cpu-quota".into(),
        (limits.nano_cpus / 10_000).to_string(),
        "--pids-limit".into(),
        limits.pids.to_string(),
        "--env".into(),
        "DOCKER_TLS_CERTDIR=".into(),
        "--env".into(),
        "DOCKER_CONTAINERD_ROOT=/var/lib/docker/containerd/daemon".into(),
        "--no-healthcheck".into(),
        "--log-driver".into(),
        "local".into(),
        "--log-opt".into(),
        "max-size=1m".into(),
        "--log-opt".into(),
        "max-file=2".into(),
    ];
    for (path, options) in limits.tmpfs() {
        args.extend(["--tmpfs".into(), format!("{path}:{options}")]);
    }
    if limits.storage == EngineStorage::Disk {
        // New profiles bind exact intent-derived storage; legacy profiles are observed only.
        args.extend([
            "--mount".into(),
            intent.storage_volume_name().map_or_else(
                || format!("type=volume,target={STORAGE_TARGET}"),
                |name| format!("type=volume,source={name},target={STORAGE_TARGET}"),
            ),
        ]);
    }
    if let Some(cache) = &cache {
        args.extend(["--mount".into(), cache_mount_argument(cache)]);
    }
    for (key, value) in labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    args.push(format!(
        "docker.io/library/docker@{}",
        intent.engine_image_digest
    ));
    args.extend(engine_command_with_tools(
        cache.as_ref(),
        generation.as_ref(),
    )?);
    Ok(args)
}

pub(super) fn hexadecimal(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) fn empty(value: &Value) -> bool {
    value.is_null()
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
}

pub(super) async fn docker_control(
    engine: &DockerEngine,
    args: Vec<String>,
) -> Result<Vec<u8>, ActEngineError> {
    docker_control_budget(engine, args, super::budgets::CONTROL).await
}

pub(super) async fn docker_control_budget(
    engine: &DockerEngine,
    args: Vec<String>,
    budget: std::time::Duration,
) -> Result<Vec<u8>, ActEngineError> {
    let result = engine
        .with_args(args)
        .capture_async(RunOptions::bounded(budget, 2 << 20))
        .await
        .map_err(|e| ActEngineError(e.to_string()))?;
    if result.exit_code != 0 {
        return Err(ActEngineError(format!(
            "Docker control refused: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(result.stdout)
}

/// Commit the immutable intent before Docker can create anything. Register the
/// observed immutable ID before starting the private daemon. Errors preserve the
/// pending record for recovery; they never authorize an unverified deletion.
pub async fn create_owned_engine(
    registry: &RegistryActor,
    engine: &DockerEngine,
    intent: ActEngineIntent,
    owner: &str,
    expected_config_digest: &str,
    limits: ActEngineLimits,
    at: f64,
) -> Result<ActEngineObservation, ActEngineError> {
    create_owned_engine_inner(
        registry,
        engine,
        intent,
        owner,
        EngineImageProof::Legacy(expected_config_digest),
        limits,
        at,
    )
    .await
}
pub async fn create_owned_engine_from_manifest(
    registry: &RegistryActor,
    engine: &DockerEngine,
    intent: ActEngineIntent,
    owner: &str,
    proof: &VerifiedEngineManifest,
    limits: ActEngineLimits,
    at: f64,
) -> Result<ActEngineObservation, ActEngineError> {
    create_owned_engine_inner(
        registry,
        engine,
        intent,
        owner,
        EngineImageProof::Publisher(proof),
        limits,
        at,
    )
    .await
}
pub(super) enum EngineImageProof<'a> {
    Legacy(&'a str),
    Publisher(&'a VerifiedEngineManifest),
}
pub(super) async fn create_owned_engine_inner(
    registry: &RegistryActor,
    engine: &DockerEngine,
    intent: ActEngineIntent,
    owner: &str,
    proof: EngineImageProof<'_>,
    limits: ActEngineLimits,
    at: f64,
) -> Result<ActEngineObservation, ActEngineError> {
    let expected_config_digest = match &proof {
        EngineImageProof::Legacy(digest) => *digest,
        EngineImageProof::Publisher(proof) => &proof.config_digest,
    };
    let args = create_arguments(&intent, owner, limits)?;
    if !at.is_finite()
        || at < intent.created_at
        || !expected_config_digest
            .strip_prefix("sha256:")
            .is_some_and(|v| hexadecimal(v, 64))
    {
        return Err(ActEngineError(
            "invalid engine config identity or observation time".into(),
        ));
    }
    registry
        .act_registry(ActRegistryCommand::Begin(intent.clone()))
        .await
        .map_err(|e| ActEngineError(e.to_string()))?;
    let image_document = docker_control(
        engine,
        vec![
            "image".into(),
            "inspect".into(),
            format!("docker.io/library/docker@{}", intent.engine_image_digest),
        ],
    )
    .await?;
    let image_identity = match proof {
        EngineImageProof::Legacy(_) => {
            observe_engine_image(&image_document, &intent, expected_config_digest)?
        }
        EngineImageProof::Publisher(proof) => {
            observe_engine_image_from_manifest(&image_document, &intent, proof)?
        }
    };
    if let Some(cache) = intent
        .creation_profile
        .as_ref()
        .and_then(|profile| profile.cache_volume.as_ref())
    {
        let volume = docker_control(
            engine,
            vec!["volume".into(), "inspect".into(), cache.name.clone()],
        )
        .await?;
        verify_cache_volume(&volume, cache)?;
    }
    ensure_storage_volume(engine, &intent, owner).await?;
    let created = docker_control(engine, args).await?;
    let id = std::str::from_utf8(&created)
        .map_err(|_| ActEngineError("Docker create returned non-UTF8 ID".into()))?
        .trim();
    if !hexadecimal(id, 64) {
        return Err(ActEngineError(
            "Docker create did not return an immutable ID".into(),
        ));
    }
    let document = docker_control(
        engine,
        vec!["container".into(), "inspect".into(), id.into()],
    )
    .await?;
    let observed = observe_engine(&document, &intent, owner, &image_identity, limits)?;
    if observed.engine_id != id {
        return Err(ActEngineError("Docker inspect changed created ID".into()));
    }
    registry
        .act_registry(ActRegistryCommand::Register {
            run: intent.run_id.clone(),
            observed: observed.clone(),
            at,
        })
        .await
        .map_err(|e| ActEngineError(e.to_string()))?;
    if let Err(error) =
        docker_control(engine, vec!["container".into(), "start".into(), id.into()]).await
    {
        registry
            .act_registry(ActRegistryCommand::Cleanup {
                run: intent.run_id,
                outcome: bosn_registry::act::ActRunOutcome::Failed,
                at,
            })
            .await
            .map_err(|e| ActEngineError(e.to_string()))?;
        return Err(error);
    }
    Ok(observed)
}

/// Remove only an engine whose real observation the registry authorizes. Both
/// exact ID and canonical name must be absent in successful daemon list probes
/// before the durable removal receipt can retire that run.
pub async fn remove_owned_engine(
    registry: &RegistryActor,
    engine: &DockerEngine,
    run: &str,
    observed: ActEngineObservation,
    at: f64,
) -> Result<(), ActEngineError> {
    let reply = registry
        .act_registry(ActRegistryCommand::Authorize {
            run: run.into(),
            observed: observed.clone(),
        })
        .await
        .map_err(|e| ActEngineError(e.to_string()))?;
    let ActRegistryReply::Authorized(record) = reply else {
        return Err(ActEngineError(
            "registry did not authorize exact engine removal".into(),
        ));
    };
    if record.engine_id.as_deref() != Some(observed.engine_id.as_str())
        || record.intent.engine_name() != observed.name
    {
        return Err(ActEngineError("registry removal identity changed".into()));
    }
    docker_control_budget(
        engine,
        vec![
            "container".into(),
            "rm".into(),
            "--force".into(),
            // Legacy anonymous storage goes with the container. Named private
            // storage is reconciled below; the shared cache is preserved.
            "--volumes".into(),
            observed.engine_id.clone(),
        ],
        super::budgets::DELETE,
    )
    .await?;
    for filter in [
        format!("id={}", observed.engine_id),
        format!("name=^/{}$", observed.name),
    ] {
        let document = docker_control(
            engine,
            vec![
                "container".into(),
                "ls".into(),
                "--all".into(),
                "--no-trunc".into(),
                "--filter".into(),
                filter,
                "--format".into(),
                "{{.ID}}".into(),
            ],
        )
        .await?;
        if !document.iter().all(u8::is_ascii_whitespace) {
            return Err(ActEngineError(
                "engine removal absence is not established".into(),
            ));
        }
    }
    crate::act_engine::stop_source_writers(registry, engine, &record, &record.registry_id).await?;
    remove_storage_volume(engine, &record, &record.registry_id).await?;
    registry
        .act_registry(ActRegistryCommand::Finalize {
            run: run.into(),
            proof: bosn_registry::act::ActEngineRemovalProof {
                storage_volume: record.intent.storage_volume_name(),
                name: observed.name,
                engine_id: Some(observed.engine_id),
            },
            at,
        })
        .await
        .map_err(|e| ActEngineError(e.to_string()))?;
    Ok(())
}
