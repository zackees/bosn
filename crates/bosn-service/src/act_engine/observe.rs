//! Verifying a real `docker inspect` of an owned engine against its committed intent.

use super::*;

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
    // The cache volume (if frozen) is checked on its own; every other
    // reported mount must be one of the declared tmpfs mounts.
    let tmpfs_mounts: Vec<&Value> = mounts.iter().filter(|m| m["Type"] != "volume").collect();
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
        || !host_mounts_match(&host["Mounts"], profile.cache_volume.as_ref())
        || !volume_mounts_match(mounts, profile.cache_volume.as_ref())
        || !empty(&host["PortBindings"])
        || tmpfs != expected_tmpfs
        // Docker may omit tmpfs entries from Mounts; declarations remain exact.
        || (!tmpfs_mounts.is_empty() && (tmpfs_mounts.len() != expected_tmpfs.len()
        || tmpfs_mounts.iter().any(|m| {
            m["Type"] != "tmpfs"
                || !m["Destination"]
                    .as_str()
                    .is_some_and(|d| expected_tmpfs.contains_key(d))
        })
        || expected_tmpfs.keys().any(|d| {
            tmpfs_mounts
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
