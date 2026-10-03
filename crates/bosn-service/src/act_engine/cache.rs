//! The one named volume an engine may mount: `bosn ci`'s machine-wide cache.
//!
//! The volume is frozen into the creation profile, so recovery verifies the
//! same attachment the run created. Like a setup volume, its ownership is
//! verified (`docker volume inspect`) before an engine is created with it, and
//! the engine's observed attachment must be exactly that volume, read-write,
//! at the frozen target.

use super::*;
use bosn_core::{LABEL_CREATED, LABEL_REGISTRY, ResourceKind, ResourceLabels, Retention, Scope};
use bosn_registry::act::ActEngineCacheVolume;

/// The labels that make a cache volume owned by `owner`'s registry.
/// `created` is recorded once, when the volume is created.
pub(crate) fn cache_volume_labels(
    owner: &str,
    created: f64,
) -> Result<BTreeMap<String, String>, ActEngineError> {
    Ok(ResourceLabels::new(
        owner,
        ResourceKind::Volume,
        "ci-cache",
        "v1",
        Scope::Machine,
        "machine",
        &created.to_string(),
        Some(Retention::Pinned),
    )
    .map_err(|_| ActEngineError("cache volume labels".into()))?
    .to_map()
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect())
}

/// Freeze the engine profile with the cache volume it mounts (if any).
pub(crate) fn creation_profile_with_cache(
    limits: ActEngineLimits,
    cache: Option<ActEngineCacheVolume>,
) -> Result<ActEngineCreationProfile, ActEngineError> {
    let mut profile = creation_profile(limits)?;
    profile.cache_volume = cache;
    profile
        .validate()
        .map_err(|error| ActEngineError(error.to_string()))?;
    Ok(profile)
}

/// `--mount` for the frozen cache volume.
pub(super) fn cache_mount_argument(cache: &ActEngineCacheVolume) -> String {
    format!("type=volume,source={},target={}", cache.name, cache.target)
}

/// A successful `docker volume inspect` of the cache volume: the local
/// driver, no driver options, and bosn's machine-scope `ci-cache` identity
/// labels. The cache is machine-wide by design, shared by every bosn daemon
/// on the host, so the registry that created it may be another daemon's;
/// its label must still name a registry, and its creation time is whatever
/// it was created with.
pub(crate) fn verify_cache_volume(
    document: &[u8],
    cache: &ActEngineCacheVolume,
) -> Result<(), ActEngineError> {
    let refused = || ActEngineError("cache volume ownership does not match".into());
    let value: Value =
        serde_json::from_slice(document).map_err(|e| ActEngineError(e.to_string()))?;
    let volume = value
        .as_array()
        .filter(|v| v.len() == 1)
        .and_then(|v| v.first())
        .ok_or_else(refused)?;
    // Every identity label but the two that legitimately vary.
    let expected = cache_volume_labels(ANY_REGISTRY, 0.0)?;
    let labels = &volume["Labels"];
    if volume["Name"].as_str() != Some(cache.name.as_str())
        || volume["Driver"] != "local"
        || volume["Scope"] != "local"
        || !empty(&volume["Options"])
        || expected
            .iter()
            .filter(|(key, _)| ![LABEL_CREATED, LABEL_REGISTRY].contains(&key.as_str()))
            .any(|(key, value)| labels[key.as_str()].as_str() != Some(value.as_str()))
        || labels[LABEL_REGISTRY]
            .as_str()
            .is_none_or(|registry| cache_volume_labels(registry, 0.0).is_err())
        || labels[LABEL_CREATED]
            .as_str()
            .is_none_or(|created| created.parse::<f64>().is_err())
    {
        return Err(refused());
    }
    Ok(())
}

/// Placeholder owner for building the expected identity labels; the
/// registry label itself is checked separately.
const ANY_REGISTRY: &str = "00000000-0000-4000-8000-000000000000";

/// The volume mounts an engine is created with: the frozen cache volume
/// (named) and, for disk-backed storage, the anonymous storage volume.
fn expected_volumes(
    cache: Option<&ActEngineCacheVolume>,
    storage: EngineStorage,
) -> Vec<(Option<&str>, &str)> {
    let cache = cache.map(|cache| (Some(cache.name.as_str()), cache.target.as_str()));
    let disk = (storage == EngineStorage::Disk).then_some((None, STORAGE_TARGET));
    cache.into_iter().chain(disk).collect()
}

/// `HostConfig.Mounts`: exactly the expected volume mounts, read-write.
pub(super) fn host_mounts_match(
    host: &Value,
    cache: Option<&ActEngineCacheVolume>,
    storage: EngineStorage,
) -> bool {
    let expected = expected_volumes(cache, storage);
    if expected.is_empty() {
        return empty(host);
    }
    let Some(mounts) = host.as_array() else {
        return false;
    };
    mounts.len() == expected.len()
        && expected.iter().all(|(source, target)| {
            mounts
                .iter()
                .filter(|mount| host_mount_is(mount, *source, target))
                .count()
                == 1
        })
}

fn host_mount_is(mount: &Value, source: Option<&str>, target: &str) -> bool {
    let Some(fields) = mount.as_object() else {
        return false;
    };
    mount["Type"] == "volume"
        && match source {
            Some(name) => mount["Source"].as_str() == Some(name),
            None => mount.get("Source").is_none_or(|source| source == ""),
        }
        && mount["Target"].as_str() == Some(target)
        && mount
            .get("ReadOnly")
            .is_none_or(|read_only| read_only == false)
        && fields.keys().all(|key| {
            matches!(key.as_str(), "Type" | "Source" | "Target" | "ReadOnly")
                || (key == "VolumeOptions" && volume_options_are_plain(&mount[key.as_str()]))
        })
}

fn volume_options_are_plain(options: &Value) -> bool {
    options.as_object().is_some_and(|options| {
        options.iter().all(|(key, value)| match key.as_str() {
            "NoCopy" => value.is_boolean(),
            _ => empty(value),
        })
    })
}

/// The runtime `Mounts` entries that are volumes: exactly the expected
/// ones, read-write, on the local driver. The anonymous storage volume has a
/// Docker-generated 64-hex name.
pub(super) fn volume_mounts_match(
    mounts: &[Value],
    cache: Option<&ActEngineCacheVolume>,
    storage: EngineStorage,
) -> bool {
    let volumes: Vec<&Value> = mounts.iter().filter(|m| m["Type"] == "volume").collect();
    let expected = expected_volumes(cache, storage);
    volumes.len() == expected.len()
        && expected.iter().all(|(name, target)| {
            volumes
                .iter()
                .filter(|volume| {
                    volume["Name"].as_str().is_some_and(|actual| match name {
                        Some(name) => actual == *name,
                        None => hexadecimal(actual, 64),
                    }) && volume["Destination"].as_str() == Some(*target)
                        && volume["Driver"] == "local"
                        && volume["RW"] == true
                })
                .count()
                == 1
        })
}
