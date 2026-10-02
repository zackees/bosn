//! Daemon-only Docker observations for the private Act engine boundary.
//! OCI manifest/config identities and Docker's store-dependent image ID are
//! observed separately before trusting container ownership labels.

use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};
use bosn_engine::{DockerEngine, RunOptions};
use bosn_registry::act::{
    ActEngineCreationProfile, ActEngineIntent, ActEngineObservation, ActEngineTmpfsPolicy,
};
use serde_json::Value;
use std::{collections::BTreeMap, fmt};

/// Publisher bytes shipped with this daemon, independently pinned from Docker
/// inspection and persistent intent data. Unknown historical images remain
/// cleanup-required until their publisher proof is available.
pub(crate) fn bundled_engine_manifests()
-> Result<BTreeMap<String, VerifiedEngineManifest>, ActEngineError> {
    const MANIFEST: &str =
        "sha256:6acc6aaf783ac1c1100822e542534c3dab3f1d38782760b0bdcb688280574d9e";
    const CONFIG: &str = "sha256:8cdb6d492106752d557cda50e628b88e7bb303a7eaea91a10bdf672b95ad4f52";
    let proof = VerifiedEngineManifest::verify(
        include_bytes!("act_engine_data/engine-manifest.json"),
        MANIFEST,
        include_bytes!("act_engine_data/engine-config.json"),
        CONFIG,
    )?;
    Ok(BTreeMap::from([(MANIFEST.into(), proof)]))
}

/// Produced only by a successful image inspection bound to the pinned manifest.
/// Docker classic uses the config digest as its ID; containerd may use manifest.
#[derive(Clone, Debug)]
pub struct VerifiedEngineImage {
    manifest_digest: String,
    docker_image_id: String,
}

/// Hash-verified publisher manifest/config bytes; never derived from Docker annotations.
#[derive(Clone, Debug)]
pub struct VerifiedEngineManifest {
    manifest_digest: String,
    config_digest: String,
    media_type: String,
    manifest_size: u64,
}
impl VerifiedEngineManifest {
    pub fn verify(
        manifest: &[u8],
        manifest_pin: &str,
        config: &[u8],
        config_pin: &str,
    ) -> Result<Self, ActEngineError> {
        let hash =
            |bytes: &[u8]| format!("sha256:{}", kernal_api::hash::Sha256Hasher::digest(bytes));
        if manifest.len() > 1 << 20
            || config.len() > 1 << 20
            || hash(manifest) != manifest_pin
            || hash(config) != config_pin
        {
            return Err(ActEngineError(
                "publisher manifest/config digest or bounds mismatch".into(),
            ));
        }
        let m: Value =
            serde_json::from_slice(manifest).map_err(|e| ActEngineError(e.to_string()))?;
        let c: Value = serde_json::from_slice(config).map_err(|e| ActEngineError(e.to_string()))?;
        let media = m["mediaType"].as_str().unwrap_or_default();
        if m["schemaVersion"] != 2
            || !matches!(
                media,
                "application/vnd.oci.image.manifest.v1+json"
                    | "application/vnd.docker.distribution.manifest.v2+json"
            )
            || !matches!(
                m["config"]["mediaType"].as_str(),
                Some(
                    "application/vnd.oci.image.config.v1+json"
                        | "application/vnd.docker.container.image.v1+json"
                )
            )
            || m["config"]["digest"] != config_pin
            || m["config"]["size"].as_u64() != Some(config.len() as u64)
            || c["os"] != "linux"
            || c["architecture"] != "amd64"
        {
            return Err(ActEngineError(
                "publisher manifest does not bind expected Linux engine config".into(),
            ));
        }
        Ok(Self {
            manifest_digest: manifest_pin.into(),
            config_digest: config_pin.into(),
            media_type: media.into(),
            manifest_size: manifest.len() as u64,
        })
    }
}
pub fn observe_engine_image_from_manifest(
    document: &[u8],
    intent: &ActEngineIntent,
    proof: &VerifiedEngineManifest,
) -> Result<VerifiedEngineImage, ActEngineError> {
    if proof.manifest_digest != intent.engine_image_digest {
        return Err(ActEngineError(
            "publisher manifest differs from immutable intent".into(),
        ));
    }
    let value: Value =
        serde_json::from_slice(document).map_err(|e| ActEngineError(e.to_string()))?;
    let image = value
        .as_array()
        .filter(|v| v.len() == 1)
        .and_then(|v| v.first())
        .ok_or_else(|| ActEngineError("image inspect must contain exactly one image".into()))?;
    let id = image["Id"]
        .as_str()
        .ok_or_else(|| ActEngineError("missing Docker image ID".into()))?;
    let pin = format!("docker.io/library/docker@{}", proof.manifest_digest);
    let short = format!("docker@{}", proof.manifest_digest);
    if !image["RepoDigests"].as_array().is_some_and(|v| {
        v.iter()
            .any(|v| v.as_str() == Some(&pin) || v.as_str() == Some(&short))
    }) {
        return Err(ActEngineError(
            "image lacks pinned repository digest".into(),
        ));
    }
    let descriptor = &image["Descriptor"];
    if descriptor.is_null() {
        if id != proof.config_digest {
            return Err(ActEngineError(
                "classic image ID differs from verified config".into(),
            ));
        }
    } else if descriptor["digest"] != proof.manifest_digest
        || descriptor["mediaType"] != proof.media_type
        || descriptor["size"].as_u64() != Some(proof.manifest_size)
        || (id != proof.manifest_digest && id != proof.config_digest)
    {
        return Err(ActEngineError(
            "Docker descriptor differs from verified publisher manifest".into(),
        ));
    }
    Ok(VerifiedEngineImage {
        manifest_digest: proof.manifest_digest.clone(),
        docker_image_id: id.into(),
    })
}
pub fn observe_engine_image(
    document: &[u8],
    intent: &ActEngineIntent,
    expected_config_digest: &str,
) -> Result<VerifiedEngineImage, ActEngineError> {
    if !expected_config_digest
        .strip_prefix("sha256:")
        .is_some_and(|v| hexadecimal(v, 64))
    {
        return Err(ActEngineError("invalid pinned engine config digest".into()));
    }
    let value: Value =
        serde_json::from_slice(document).map_err(|e| ActEngineError(e.to_string()))?;
    let image = value
        .as_array()
        .filter(|v| v.len() == 1)
        .and_then(|v| v.first())
        .ok_or_else(|| ActEngineError("image inspect must contain exactly one image".into()))?;
    let id = image["Id"]
        .as_str()
        .filter(|v| {
            v.strip_prefix("sha256:")
                .is_some_and(|v| hexadecimal(v, 64))
        })
        .ok_or_else(|| ActEngineError("missing Docker image ID".into()))?;
    let descriptor = &image["Descriptor"];
    if !descriptor.is_null() {
        if descriptor["digest"].as_str() != Some(intent.engine_image_digest.as_str())
            || descriptor["annotations"]["config.digest"].as_str() != Some(expected_config_digest)
            || (id != expected_config_digest && id != intent.engine_image_digest)
        {
            return Err(ActEngineError(
                "image descriptor does not bind pinned manifest and config".into(),
            ));
        }
    } else {
        let pin = format!("docker.io/library/docker@{}", intent.engine_image_digest);
        let short_pin = format!("docker@{}", intent.engine_image_digest);
        let bound = image["RepoDigests"].as_array().is_some_and(|v| {
            v.iter()
                .any(|v| v.as_str() == Some(pin.as_str()) || v.as_str() == Some(short_pin.as_str()))
        });
        if id != expected_config_digest || !bound {
            return Err(ActEngineError(
                "classic image store does not bind pinned manifest and config".into(),
            ));
        }
    }
    Ok(VerifiedEngineImage {
        manifest_digest: intent.engine_image_digest.clone(),
        docker_image_id: id.into(),
    })
}

// Cgroup nesting follows the pinned image's DIND_COMMIT 8d9e3502.../hack/dind.
// Only the private cgroup namespace root is touched; startup is finite and
// deliberately omits the publisher entrypoint's TCP listeners and extra mounts.
const ENGINE_INIT: &str = r#"set -eu
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
        "--storage-driver=native",
        "--data-root=/var/lib/docker",
        "--exec-root=/run/docker",
        "--host=unix:///var/run/docker.sock",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
/// Limits apply to the entire isolated engine and all its descendants.
/// Storage is private tmpfs; the engine's writable root is disabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActEngineLimits {
    pub memory_bytes: u64,
    pub storage_bytes: u64,
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
    fn validate(self) -> Result<(), ActEngineError> {
        if self.storage_bytes < 1 << 20
            || self.memory_bytes.saturating_sub(self.storage_bytes) < 512 << 20
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

    fn tmpfs(self) -> BTreeMap<String, String> {
        [
            ("/var/lib/docker", self.storage_bytes),
            ("/run", 16 << 20),
            ("/tmp", 64 << 20),
        ]
        .into_iter()
        // Docker 29.7.2 daemon/oci_linux.go defaults user tmpfs to noexec.
        // Native snapshots must execute container binaries; only their private
        // storage mount clears that default. /run and /tmp remain noexec.
        .map(|(path, bytes)| {
            let execution = if path == "/var/lib/docker" {
                ",exec"
            } else {
                ""
            };
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
        tmpfs_policy: ActEngineTmpfsPolicy::StorageExecRunTmpNoexecV1,
        init_command_sha256: command_digest(&engine_command())?,
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
    if intent.creation_profile.as_ref() != Some(&creation_profile(limits)?) {
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
    for (key, value) in labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    args.push(format!(
        "docker.io/library/docker@{}",
        intent.engine_image_digest
    ));
    args.extend(engine_command());
    Ok(args)
}

fn hexadecimal(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn empty(value: &Value) -> bool {
    value.is_null()
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
}

async fn docker_control(
    engine: &DockerEngine,
    args: Vec<String>,
) -> Result<Vec<u8>, ActEngineError> {
    let result = engine
        .with_args(args)
        .capture_async(RunOptions::bounded(
            std::time::Duration::from_secs(30),
            2 << 20,
        ))
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
enum EngineImageProof<'a> {
    Legacy(&'a str),
    Publisher(&'a VerifiedEngineManifest),
}
async fn create_owned_engine_inner(
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
    docker_control(
        engine,
        vec![
            "container".into(),
            "rm".into(),
            "--force".into(),
            observed.engine_id.clone(),
        ],
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
    registry
        .act_registry(ActRegistryCommand::Finalize {
            run: run.into(),
            proof: bosn_registry::act::ActEngineRemovalProof {
                name: observed.name,
                engine_id: Some(observed.engine_id),
            },
            at,
        })
        .await
        .map_err(|e| ActEngineError(e.to_string()))?;
    Ok(())
}

/// Parse a successful, bounded real `docker inspect` response. This function
/// never interprets a failed or unavailable probe as ownership or absence.
pub fn observe_engine(
    document: &[u8],
    intent: &ActEngineIntent,
    owner: &str,
    image_identity: &VerifiedEngineImage,
    limits: ActEngineLimits,
) -> Result<ActEngineObservation, ActEngineError> {
    limits.validate()?;
    if frozen_limits(intent)? != limits {
        return Err(ActEngineError(
            "observation differs from frozen engine limits".into(),
        ));
    }
    let profile = intent
        .creation_profile
        .as_ref()
        .ok_or_else(|| ActEngineError("engine creation profile is absent".into()))?;
    if image_identity.manifest_digest != intent.engine_image_digest {
        return Err(ActEngineError(
            "verified image belongs to another pinned manifest".into(),
        ));
    }
    let required = intent
        .required_labels(owner)
        .map_err(|e| ActEngineError(e.to_string()))?;
    let value: Value =
        serde_json::from_slice(document).map_err(|e| ActEngineError(e.to_string()))?;
    let objects = value
        .as_array()
        .filter(|v| v.len() == 1)
        .ok_or_else(|| ActEngineError("inspect must contain exactly one engine".into()))?;
    let engine = &objects[0];
    let id = engine["Id"]
        .as_str()
        .filter(|v| hexadecimal(v, 64))
        .ok_or_else(|| ActEngineError("missing immutable engine ID".into()))?;
    let labels: BTreeMap<String, String> =
        serde_json::from_value(engine["Config"]["Labels"].clone())
            .map_err(|e| ActEngineError(e.to_string()))?;
    let host = &engine["HostConfig"];
    let tmpfs: BTreeMap<String, String> =
        serde_json::from_value(host["Tmpfs"].clone()).map_err(|e| ActEngineError(e.to_string()))?;
    let mounts = engine["Mounts"]
        .as_array()
        .ok_or_else(|| ActEngineError("missing mount observation".into()))?;
    let env = engine["Config"]["Env"]
        .as_array()
        .ok_or_else(|| ActEngineError("missing environment observation".into()))?;
    let expected_tmpfs: BTreeMap<String, String> = [
        (
            "/var/lib/docker".into(),
            format!("rw,exec,nosuid,nodev,size={}", profile.storage_bytes),
        ),
        (
            "/run".into(),
            format!("rw,nosuid,nodev,size={}", profile.run_tmpfs_bytes),
        ),
        (
            "/tmp".into(),
            format!("rw,nosuid,nodev,size={}", profile.tmp_tmpfs_bytes),
        ),
    ]
    .into_iter()
    .collect();
    let command: Vec<String> = serde_json::from_value(engine["Config"]["Cmd"].clone())
        .map_err(|error| ActEngineError(error.to_string()))?;
    if command_digest(&command)? != profile.init_command_sha256 {
        return Err(ActEngineError(
            "observed command differs from frozen engine init".into(),
        ));
    }
    if engine["Name"].as_str() != Some(format!("/{}", intent.engine_name()).as_str())
        || engine["Image"].as_str() != Some(image_identity.docker_image_id.as_str())
        || engine["Config"]["Image"].as_str()
            != Some(format!("docker.io/library/docker@{}", intent.engine_image_digest).as_str())
        || required.iter().any(|(k, v)| labels.get(k) != Some(v))
        || host["Privileged"] != true
        || host["ReadonlyRootfs"] != true
        || host["Memory"].as_u64() != Some(limits.memory_bytes)
        || host["MemorySwap"].as_u64() != Some(limits.memory_bytes)
        || host["CpuPeriod"].as_u64() != Some(100000)
        || host["CpuQuota"].as_u64() != Some(limits.nano_cpus / 10_000)
        || host["PidsLimit"].as_u64() != Some(limits.pids)
        || host["PidMode"] != ""
        || host["IpcMode"] != "private"
        || host["CgroupnsMode"] != "private"
        || host["NetworkMode"] != "bridge"
        || host["LogConfig"]["Type"] != "local"
        || host["LogConfig"]["Config"]["max-size"] != "1m"
        || host["LogConfig"]["Config"]["max-file"] != "2"
        || engine["Config"]
            .get("Entrypoint")
            .is_none_or(|v| !v.is_null() && !v.as_array().is_some_and(|v| v.is_empty()))
        || !empty(&host["Binds"])
        || !empty(&host["VolumesFrom"])
        || !empty(&host["Mounts"])
        || !empty(&host["PortBindings"])
        || tmpfs != expected_tmpfs
        // Docker may omit tmpfs entries from Mounts; declarations remain exact.
        || (!mounts.is_empty() && (mounts.len() != expected_tmpfs.len()
        || mounts.iter().any(|m| {
            m["Type"] != "tmpfs"
                || !m["Destination"]
                    .as_str()
                    .is_some_and(|d| expected_tmpfs.contains_key(d))
        })
        || expected_tmpfs.keys().any(|d| {
            mounts
                .iter()
                .filter(|m| m["Destination"].as_str() == Some(d.as_str()))
                .count()
                != 1
        })))
        || env
            .iter()
            .filter(|v| {
                v.as_str()
                    .is_some_and(|s| s.starts_with("DOCKER_TLS_CERTDIR="))
            })
            .collect::<Vec<_>>()
            != vec![&Value::String("DOCKER_TLS_CERTDIR=".into())]
        || env
            .iter()
            .filter(|v| {
                v.as_str()
                    .is_some_and(|s| s.starts_with("DOCKER_CONTAINERD_ROOT="))
            })
            .collect::<Vec<_>>()
            != vec![&Value::String(
                "DOCKER_CONTAINERD_ROOT=/var/lib/docker/containerd/daemon".into(),
            )]
        || env
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.starts_with("DOCKER_HOST=")))
        || engine["Config"]["Healthcheck"]["Test"] != serde_json::json!(["NONE"])
        || engine["Config"]["Volumes"]
            .as_object()
            .is_none_or(|v| v.len() != 1 || !v.contains_key("/var/lib/docker"))
    {
        return Err(ActEngineError(
            "Act engine identity, ownership or isolation does not match committed intent".into(),
        ));
    }
    Ok(ActEngineObservation {
        name: intent.engine_name(),
        engine_id: id.into(),
        image_digest: intent.engine_image_digest.clone(),
        labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn intent() -> ActEngineIntent {
        ActEngineIntent {
            run_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            workspace: "/private/source".into(),
            candidate_sha: "a".repeat(40),
            payload_sha256: "b".repeat(64),
            snapshot_sha256: "c".repeat(64),
            act_version: "0.2.88".into(),
            act_image_digest: format!("sha256:{}", "d".repeat(64)),
            engine_image_digest: format!("sha256:{}", "e".repeat(64)),
            runner_image_digest: format!("sha256:{}", "f".repeat(64)),
            creation_profile: Some(creation_profile(limits()).unwrap()),
            created_at: 1.0,
        }
    }
    const OWNER: &str = "11111111-2222-4333-8444-555555555555";
    fn limits() -> ActEngineLimits {
        ActEngineLimits {
            memory_bytes: 8 << 30,
            storage_bytes: 4 << 30,
            nano_cpus: 4_000_000_000,
            pids: 1024,
        }
    }
    fn document() -> serde_json::Value {
        let i = intent();
        let l = limits();
        json!([{"Id":"1".repeat(64),"Name":format!("/{}",i.engine_name()),"Image":format!("sha256:{}","2".repeat(64)),"Config":{"Image":format!("docker.io/library/docker@{}",i.engine_image_digest),"Entrypoint":null,"Labels":i.required_labels(OWNER).unwrap(),"Env":["DOCKER_TLS_CERTDIR=","DOCKER_CONTAINERD_ROOT=/var/lib/docker/containerd/daemon"],"Volumes":{"/var/lib/docker":{}},"Healthcheck":{"Test":["NONE"]},"Cmd":engine_command()},"HostConfig":{"Privileged":true,"Memory":l.memory_bytes,"MemorySwap":l.memory_bytes,"CpuPeriod":100000,"CpuQuota":l.nano_cpus/10_000,"ReadonlyRootfs":true,"PidMode":"","IpcMode":"private","CgroupnsMode":"private","PidsLimit":l.pids,"Binds":null,"PortBindings":{},"NetworkMode":"bridge","LogConfig":{"Type":"local","Config":{"max-size":"1m","max-file":"2"}},"Tmpfs":l.tmpfs()},"Mounts":[{"Type":"tmpfs","Destination":"/var/lib/docker"},{"Type":"tmpfs","Destination":"/run"},{"Type":"tmpfs","Destination":"/tmp"}]}])
    }
    #[test]
    fn real_descriptor_without_annotations_requires_publisher_config_proof() {
        let config =
            serde_json::to_vec(&serde_json::json!({"os":"linux","architecture":"amd64"})).unwrap();
        let hash = |b: &[u8]| format!("sha256:{}", kernal_api::hash::Sha256Hasher::digest(b));
        let config_pin = hash(&config);
        let manifest = serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_pin,"size":config.len()}})).unwrap();
        let pin = hash(&manifest);
        let mut i = intent();
        i.engine_image_digest = pin.clone();
        let image = serde_json::json!([{"Id":pin,"RepoDigests":[format!("docker.io/library/docker@{pin}")],"Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":pin,"size":manifest.len()}}]);
        let document = serde_json::to_vec(&image).unwrap();
        assert!(
            observe_engine_image(&document, &i, &config_pin).is_err(),
            "real shape reproduces legacy refusal"
        );
        let proof = VerifiedEngineManifest::verify(&manifest, &pin, &config, &config_pin).unwrap();
        let verified = observe_engine_image_from_manifest(&document, &i, &proof).unwrap();
        assert_eq!(verified.docker_image_id, pin);
        let classic =
            serde_json::json!([{"Id":config_pin,"RepoDigests":[format!("docker@{pin}")]}]);
        assert!(
            observe_engine_image_from_manifest(&serde_json::to_vec(&classic).unwrap(), &i, &proof)
                .is_ok()
        );
        let mut foreign_intent = i.clone();
        foreign_intent.engine_image_digest = format!("sha256:{}", "0".repeat(64));
        assert!(observe_engine_image_from_manifest(&document, &foreign_intent, &proof).is_err());

        for change in ["digest", "size", "mediaType", "Id", "RepoDigests"] {
            let mut wrong = image.clone();
            match change {
                "size" => wrong[0]["Descriptor"][change] = serde_json::json!(0),
                "Id" => wrong[0][change] = serde_json::json!(config_pin.replace('a', "b") + "0"),
                "RepoDigests" => wrong[0][change] = serde_json::json!([]),
                _ => wrong[0]["Descriptor"][change] = serde_json::json!("foreign"),
            }
            assert!(
                observe_engine_image_from_manifest(
                    &serde_json::to_vec(&wrong).unwrap(),
                    &i,
                    &proof
                )
                .is_err(),
                "{change}"
            );
        }
        for change in ["digest", "size", "mediaType"] {
            let mut wrong = serde_json::from_slice::<Value>(&manifest).unwrap();
            wrong["config"][change] = serde_json::json!("foreign");
            let bytes = serde_json::to_vec(&wrong).unwrap();
            assert!(
                VerifiedEngineManifest::verify(&bytes, &hash(&bytes), &config, &config_pin)
                    .is_err()
            );
        }
        assert!(VerifiedEngineManifest::verify(&manifest, &pin, b"corrupt", &config_pin).is_err());
        assert!(VerifiedEngineManifest::verify(b"corrupt", &pin, &config, &config_pin).is_err());
    }
    fn classic_identity() -> VerifiedEngineImage {
        let i = intent();
        let config = format!("sha256:{}", "2".repeat(64));
        let image =
            json!([{"Id":config,"RepoDigests":[format!("docker@{}",i.engine_image_digest)]}]);
        observe_engine_image(&serde_json::to_vec(&image).unwrap(), &i, &config).unwrap()
    }
    #[test]
    fn containerd_manifest_image_id_is_bound_to_verified_config() {
        let i = intent();
        let config = format!("sha256:{}", "2".repeat(64));
        let image = json!([{"Id":i.engine_image_digest,"Descriptor":{"digest":i.engine_image_digest,"annotations":{"config.digest":config}}}]);
        let identity =
            observe_engine_image(&serde_json::to_vec(&image).unwrap(), &i, &config).unwrap();
        let mut container = document();
        container[0]["Image"] = json!(i.engine_image_digest);
        assert!(
            observe_engine(
                &serde_json::to_vec(&container).unwrap(),
                &i,
                OWNER,
                &identity,
                limits()
            )
            .is_ok()
        );
        let mut wrong = image.clone();
        wrong[0]["Descriptor"]["annotations"]["config.digest"] =
            json!(format!("sha256:{}", "9".repeat(64)));
        assert!(observe_engine_image(&serde_json::to_vec(&wrong).unwrap(), &i, &config).is_err());
    }
    #[test]
    fn verifies_real_config_identity_and_complete_private_boundary() {
        let i = intent();
        let d = document();
        let observation = observe_engine(
            &serde_json::to_vec(&d).unwrap(),
            &i,
            OWNER,
            &classic_identity(),
            limits(),
        )
        .unwrap();
        assert_eq!(observation.image_digest, i.engine_image_digest);
        assert_eq!(observation.engine_id, "1".repeat(64));
        assert!(
            observe_engine(
                &serde_json::to_vec(&d).unwrap(),
                &i,
                OWNER,
                &VerifiedEngineImage {
                    manifest_digest: i.engine_image_digest.clone(),
                    docker_image_id: i.engine_image_digest.clone()
                },
                limits()
            )
            .is_err()
        );
    }
    #[test]
    fn created_container_without_reported_tmpfs_mounts_retains_storage_boundary() {
        let mut d = document();
        d[0]["Mounts"] = json!([]);
        let observe = |d: &Value| {
            observe_engine(
                &serde_json::to_vec(d).unwrap(),
                &intent(),
                OWNER,
                &classic_identity(),
                limits(),
            )
        };
        assert!(
            observe(&d).is_ok(),
            "Docker created container omits tmpfs from Mounts"
        );
        for change in 0..7 {
            let mut wrong = d.clone();
            match change {
                0 => wrong[0]["HostConfig"]["Tmpfs"] = json!({}),
                1 => wrong[0]["HostConfig"]["Tmpfs"]["/var/lib/docker"] = json!("rw,size=1"),
                2 => wrong[0]["HostConfig"]["Binds"] = json!(["/foreign:/foreign"]),
                3 => wrong[0]["HostConfig"]["VolumesFrom"] = json!(["foreign"]),
                4 => {
                    wrong[0]["HostConfig"]["Mounts"] =
                        json!([{"Type":"volume","Target":"/foreign"}])
                }
                5 => {
                    wrong[0]["Mounts"] = json!([{"Type":"volume","Destination":"/var/lib/docker"}])
                }
                _ => wrong[0]["Mounts"] = json!([{"Type":"tmpfs","Destination":"/var/lib/docker"}]),
            }
            assert!(observe(&wrong).is_err(), "{change}");
        }
    }
    #[test]
    fn refuses_host_socket_anonymous_storage_labels_and_unbounded_resources() {
        for change in 0..13 {
            let mut d = document();
            match change {
                0 => {
                    d[0]["HostConfig"]["Binds"] =
                        json!(["/var/run/docker.sock:/var/run/docker.sock"])
                }
                1 => d[0]["Mounts"][0]["Type"] = json!("volume"),
                2 => d[0]["HostConfig"]["Memory"] = json!(0),
                3 => d[0]["HostConfig"]["MemorySwap"] = json!(-1),
                4 => d[0]["HostConfig"]["CpuQuota"] = json!(0),
                5 => d[0]["HostConfig"]["PidsLimit"] = json!(-1),
                6 => d[0]["Config"]["Labels"]["com.zackees.bosn.act.run-id"] = json!("foreign"),
                7 => d[0]["HostConfig"]["PortBindings"] = json!({"2375/tcp":[{"HostPort":"2375"}]}),
                8 => d[0]["Config"]["Env"] = json!(["DOCKER_TLS_CERTDIR=/certs"]),
                9 => d[0]["HostConfig"]["ReadonlyRootfs"] = json!(false),
                10 => d[0]["HostConfig"]["LogConfig"]["Config"]["max-size"] = json!("0"),
                11 => d[0]["HostConfig"]["PidMode"] = json!("host"),
                _ => d[0]["Config"]["Cmd"] = json!(["dockerd", "--host=tcp://0.0.0.0:2375"]),
            }
            assert!(
                observe_engine(
                    &serde_json::to_vec(&d).unwrap(),
                    &intent(),
                    OWNER,
                    &classic_identity(),
                    limits()
                )
                .is_err(),
                "mutation {change}"
            );
        }
    }
    #[test]
    fn image_entrypoint_cannot_add_an_unverified_tcp_docker_listener() {
        let args = create_arguments(&intent(), OWNER, limits()).unwrap();
        assert!(args.windows(2).any(|p| p == ["--entrypoint", ""]));
        let mut observed = document();
        observed[0]["Config"]["Entrypoint"] = json!(["dockerd-entrypoint.sh"]);
        assert!(
            observe_engine(
                &serde_json::to_vec(&observed).unwrap(),
                &intent(),
                OWNER,
                &classic_identity(),
                limits()
            )
            .is_err()
        );
    }
    #[test]
    fn private_cgroup_init_reparents_and_refuses_unavailable_or_foreign_roots() {
        use std::process::Command;
        let root =
            std::env::temp_dir().join(format!("bosn-cgroup-init-3345-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let group = root.join("self-cgroup");
        let cgroups = root.join("cgroups");
        std::fs::create_dir(&cgroups).unwrap();
        std::fs::write(&group, "0::/\n").unwrap();
        std::fs::write(cgroups.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        std::fs::write(cgroups.join("cgroup.procs"), "1\n42\n").unwrap();
        std::fs::write(cgroups.join("cgroup.subtree_control"), "").unwrap();
        // Run only against a synthetic file tree, never the host cgroupfs.
        let script = ENGINE_INIT
            .replace("[ \"$$\" -eq 1 ]", "[ 1 -eq 1 ]")
            .replace("/proc/self/cgroup", group.to_str().unwrap())
            .replace("/sys/fs/cgroup", cgroups.to_str().unwrap())
            .replace("exec docker-init --", "exec");
        let run = || {
            Command::new("sh")
                .args(["-ec", &script, "fixture", "sh", "-c", "printf initialized"])
                .output()
                .unwrap()
        };
        let pid_guard = script.replace("[ 1 -eq 1 ]", "[ \"$$\" -eq 1 ]");
        let refused = Command::new("sh")
            .args([
                "-ec",
                &pid_guard,
                "fixture",
                "sh",
                "-c",
                "printf initialized",
            ])
            .output()
            .unwrap();
        assert!(!refused.status.success());
        assert!(refused.stdout.is_empty());
        assert_eq!(run().stdout, b"initialized");
        assert_eq!(
            std::fs::read_to_string(cgroups.join("init/cgroup.procs")).unwrap(),
            "42\n"
        );
        assert_eq!(
            std::fs::read_to_string(cgroups.join("cgroup.subtree_control")).unwrap(),
            " +cpu +memory +pids\n"
        );
        for bad in ["0::/foreign\n", "1:memory:/\n"] {
            std::fs::write(&group, bad).unwrap();
            let result = run();
            assert!(!result.status.success());
            assert!(result.stdout.is_empty());
        }
        std::fs::write(&group, "0::/\n").unwrap();
        std::fs::write(cgroups.join("cgroup.controllers"), "cpu bad-token\n").unwrap();
        assert!(!run().status.success());
        std::fs::write(cgroups.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        // A controller write failure must finish after a finite retry budget.
        let blocked = root.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        let failing = script.replace(
            cgroups.join("cgroup.subtree_control").to_str().unwrap(),
            blocked.to_str().unwrap(),
        );
        let result = Command::new("sh")
            .args(["-ec", &failing, "fixture", "sh", "-c", "printf initialized"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("private cgroup controllers unavailable")
        );
        assert!(!ENGINE_INIT.contains("mount "));
        assert!(!ENGINE_INIT.contains("tcp://"));
        let mut wrong = document();
        wrong[0]["Config"]["Cmd"][2] = json!("exec dockerd --host=tcp://0.0.0.0:2375");
        assert!(
            observe_engine(
                &serde_json::to_vec(&wrong).unwrap(),
                &intent(),
                OWNER,
                &classic_identity(),
                limits()
            )
            .is_err()
        );
        eprintln!("retained cgroup fixture {}", root.display());
    }
    #[test]
    fn native_containerd_content_and_runtime_remain_under_bounded_roots() {
        let args = create_arguments(&intent(), OWNER, limits()).unwrap();
        let expected = [
            "dockerd",
            "--feature=containerd-snapshotter=true",
            "--storage-driver=native",
            "--data-root=/var/lib/docker",
            "--exec-root=/run/docker",
            "--host=unix:///var/run/docker.sock",
        ];
        assert_eq!(&args[args.len() - expected.len()..], expected);
        assert!(args.windows(2).any(|p| p
            == [
                "--env",
                "DOCKER_CONTAINERD_ROOT=/var/lib/docker/containerd/daemon"
            ]));
        let mut observed = document();
        observed[0]["Config"]["Env"] = json!([
            "DOCKER_TLS_CERTDIR=",
            "DOCKER_CONTAINERD_ROOT=/var/lib/containerd"
        ]);
        assert!(
            observe_engine(
                &serde_json::to_vec(&observed).unwrap(),
                &intent(),
                OWNER,
                &classic_identity(),
                limits()
            )
            .is_err()
        );
    }
    #[test]
    fn engine_arguments_override_image_volume_without_any_host_bind() {
        let args = create_arguments(&intent(), OWNER, limits()).unwrap();
        assert!(
            args.windows(2).any(|p| p[0] == "--tmpfs"
                && p[1] == "/var/lib/docker:rw,exec,nosuid,nodev,size=4294967296")
        );
        assert!(!args.iter().any(|v| matches!(
            v.as_str(),
            "--volume" | "-v" | "--mount" | "--publish" | "-p" | "--rm"
        )));
        assert_eq!(args.last().unwrap(), "--host=unix:///var/run/docker.sock");
        assert!(
            create_arguments(
                &intent(),
                OWNER,
                ActEngineLimits {
                    memory_bytes: 1,
                    ..limits()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn recovery_uses_frozen_command_while_creation_requires_current_producer() {
        let mut prior = intent();
        let mut prior_command = engine_command();
        prior_command.push("--log-level=warn".into());
        prior.creation_profile.as_mut().unwrap().init_command_sha256 =
            command_digest(&prior_command).unwrap();
        let mut observed = document();
        observed[0]["Config"]["Cmd"] = json!(prior_command);
        observed[0]["Config"]["Labels"] = json!(prior.required_labels(OWNER).unwrap());
        assert!(
            observe_engine(
                &serde_json::to_vec(&observed).unwrap(),
                &prior,
                OWNER,
                &classic_identity(),
                frozen_limits(&prior).unwrap()
            )
            .is_ok()
        );
        assert!(create_arguments(&prior, OWNER, limits()).is_err());
        observed[0]["Config"]["Cmd"] = json!(engine_command());
        assert!(
            observe_engine(
                &serde_json::to_vec(&observed).unwrap(),
                &prior,
                OWNER,
                &classic_identity(),
                frozen_limits(&prior).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_intent_or_changed_creation_limits_never_create_or_observe() {
        let mut legacy = intent();
        legacy.creation_profile = None;
        assert!(create_arguments(&legacy, OWNER, limits()).is_err());
        assert!(
            observe_engine(
                &serde_json::to_vec(&document()).unwrap(),
                &legacy,
                OWNER,
                &classic_identity(),
                limits()
            )
            .is_err()
        );
        let changed = ActEngineLimits {
            memory_bytes: 10 << 30,
            ..limits()
        };
        assert!(create_arguments(&intent(), OWNER, changed).is_err());
        assert!(
            observe_engine(
                &serde_json::to_vec(&document()).unwrap(),
                &intent(),
                OWNER,
                &classic_identity(),
                changed
            )
            .is_err()
        );
        let mut profile_drift = intent();
        profile_drift
            .creation_profile
            .as_mut()
            .unwrap()
            .run_tmpfs_bytes = 32 << 20;
        assert!(create_arguments(&profile_drift, OWNER, limits()).is_err());
    }

    #[test]
    fn snapshot_storage_exec_is_required_but_other_tmpfs_cannot_gain_exec() {
        let declared = limits().tmpfs();
        assert_eq!(
            declared["/var/lib/docker"],
            "rw,exec,nosuid,nodev,size=4294967296"
        );
        for path in ["/run", "/tmp"] {
            assert!(!declared[path].split(',').any(|option| option == "exec"));
        }
        for (path, value) in [
            ("/var/lib/docker", "rw,nosuid,nodev,size=4294967296"),
            ("/run", "rw,exec,nosuid,nodev,size=16777216"),
            ("/tmp", "rw,exec,nosuid,nodev,size=67108864"),
        ] {
            let mut observed = document();
            observed[0]["HostConfig"]["Tmpfs"][path] = json!(value);
            assert!(
                observe_engine(
                    &serde_json::to_vec(&observed).unwrap(),
                    &intent(),
                    OWNER,
                    &classic_identity(),
                    limits()
                )
                .is_err(),
                "unexpected execution policy at {path}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn actor_commits_before_create_and_start_and_refuses_uncertain_removal() {
        use bosn_registry::{
            Registry,
            act::{ActEngineState, ActRunOutcome},
        };
        use kernal_api::{async_engine, platform::fs::TemporaryDirectory};
        // A synthetic CLI reads the actor's real SQLite snapshots before each
        // mutation. This proves ordering, not real Docker or Act execution.
        const SCRIPT: &str = r#"
import json, pathlib, sqlite3, sys
db, observation, mode, log = sys.argv[1:5]
args = sys.argv[5:]
with pathlib.Path(log).open('a') as out: out.write(json.dumps(args)+'\n')
record=json.loads(sqlite3.connect('file:'+db+'?mode=ro',uri=True).execute("SELECT detail FROM events WHERE kind LIKE 'act.engine.v1:%' ORDER BY id DESC LIMIT 1").fetchone()[0])
if args[:2]==['image','inspect']:
 print(json.dumps([{'Id':'sha256:'+'2'*64,'RepoDigests':['docker@sha256:'+'e'*64]}]))
elif args[0]=='create':
 assert record['state']=='pending' and record['engine_id'] is None
 print('1'*64)
elif args[:2]==['container','inspect']:
 document=json.loads(pathlib.Path(observation).read_text())
 if mode=='foreign': document[0]['Image']='sha256:'+'9'*64
 print(json.dumps(document))
elif args[:2]==['container','start']:
 assert record['state']=='registered' and record['engine_id']=='1'*64
 if mode=='start-failed': sys.exit(7)
 print('1'*64)
elif args[:2]==['container','rm']:
 assert record['state']=='cleanup_required' and record['engine_id']=='1'*64
 assert args[-1]=='1'*64
 print('1'*64)
elif args[:2]==['container','ls']:
 if mode=='probe-failed': sys.exit(8)
 if mode=='still-present': print('1'*64)
else: sys.exit(9)
"#;
        for mode in [
            "success",
            "foreign",
            "start-failed",
            "probe-failed",
            "still-present",
        ] {
            let dir = TemporaryDirectory::new().unwrap();
            let db = dir.path().join("registry.sqlite3");
            let fixture = dir.path().join("docker.py");
            let observation = dir.path().join("inspect.json");
            let log = dir.path().join("commands.jsonl");
            std::fs::write(&fixture, SCRIPT).unwrap();
            std::fs::write(&observation, serde_json::to_vec(&document()).unwrap()).unwrap();
            let engine = DockerEngine::synthetic_for_test(
                "python3",
                [
                    fixture.to_string_lossy().into_owned(),
                    db.to_string_lossy().into_owned(),
                    observation.to_string_lossy().into_owned(),
                    mode.into(),
                    log.to_string_lossy().into_owned(),
                ],
            );
            let writer = Registry::create_writer(&db, OWNER).unwrap();
            let runtime = async_engine::RuntimeBuilder::multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.run(async {
                let (sender, receiver) = async_engine::channel(16);
                let actor = RegistryActor { sender };
                let task = async_engine::launch(crate::registry_actor(writer, receiver, None));
                let created = create_owned_engine(
                    &actor,
                    &engine,
                    intent(),
                    OWNER,
                    &format!("sha256:{}", "2".repeat(64)),
                    limits(),
                    1.0,
                )
                .await;
                if matches!(mode, "foreign" | "start-failed") {
                    assert!(created.is_err());
                } else {
                    let observed = created.unwrap();
                    let token = "12345678-1234-4234-8234-123456789abc";
                    actor
                        .act_registry(ActRegistryCommand::Claim {
                            intent: intent(),
                            observed: observed.clone(),
                            token: token.into(),
                            at: 2.0,
                        })
                        .await
                        .unwrap();
                    actor
                        .act_registry(ActRegistryCommand::Execution {
                            run: intent().run_id,
                            token: token.into(),
                            outcome: ActRunOutcome::Passed,
                            at: 2.0,
                        })
                        .await
                        .unwrap();
                    actor
                        .act_registry(ActRegistryCommand::CleanupClaimed {
                            run: intent().run_id,
                            token: token.into(),
                            outcome: ActRunOutcome::Passed,
                            at: 2.0,
                        })
                        .await
                        .unwrap();
                    assert_eq!(
                        remove_owned_engine(&actor, &engine, &intent().run_id, observed, 2.0)
                            .await
                            .is_ok(),
                        mode == "success"
                    );
                }
                actor.stop().await;
                task.await.unwrap();
            });
            let reader = Registry::open_writer(&db).unwrap();
            let record = reader.act_engine(&intent().run_id).unwrap().unwrap();
            assert_eq!(
                record.state,
                match mode {
                    "success" => ActEngineState::Terminal,
                    "foreign" => ActEngineState::Pending,
                    _ => ActEngineState::CleanupRequired,
                }
            );
            let commands = std::fs::read_to_string(log).unwrap();
            if mode == "foreign" {
                for line in commands.lines() {
                    let args: Vec<String> = serde_json::from_str(line).unwrap();
                    assert!(!args.starts_with(&["container".into(), "start".into()]));
                    assert!(!args.starts_with(&["container".into(), "rm".into()]));
                }
            }
            if mode == "success" {
                assert_eq!(commands.lines().count(), 7);
            }
        }
    }
}
