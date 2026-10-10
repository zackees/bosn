//! The named shared cache and exact engine volume-mount boundary.
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
    creation_profile_with_tools(limits, cache, None)
}

pub(crate) fn creation_profile_with_tools(
    limits: ActEngineLimits,
    cache: Option<ActEngineCacheVolume>,
    generation: Option<bosn_registry::act::ActToolGenerationBinding>,
) -> Result<ActEngineCreationProfile, ActEngineError> {
    if generation.as_ref().is_some_and(|generation| {
        generation.overlay_recipe_sha256 != crate::ci::engine::tool_overlay_recipe_sha256()
    }) {
        return Err(ActEngineError(
            "tool overlay recipe differs from frozen producer".into(),
        ));
    }
    let mut profile = creation_profile(limits)?;
    profile.cache_coordination = cache
        .as_ref()
        .map(|_| bosn_registry::act::ActCacheCoordination::SharedLegacyLeaseV1);
    profile.init_command_sha256 = command_digest(&engine_command_with_tools(
        cache.as_ref(),
        generation.as_ref(),
        None,
    )?)?;
    profile.cache_volume = cache;
    profile.tool_generation = generation;
    profile
        .validate()
        .map_err(|error| ActEngineError(error.to_string()))?;
    Ok(profile)
}

/// A cache-backed engine verifies and installs act while it is still the
/// dedicated startup process. The command digest freezes both artifact hashes
/// and the exact install/INIT chain before durable registration and creation.
/// Engines without a shared cache retain their historical command identity.
#[cfg(test)]
pub(super) fn engine_command_with_cache(
    cache: Option<&ActEngineCacheVolume>,
) -> Result<Vec<String>, ActEngineError> {
    engine_command_with_tools(cache, None, None)
}

pub(super) fn engine_command_with_tools(
    cache: Option<&ActEngineCacheVolume>,
    generation: Option<&bosn_registry::act::ActToolGenerationBinding>,
    socket: Option<&bosn_registry::act::ActEngineDockerSocket>,
) -> Result<Vec<String>, ActEngineError> {
    let mut dockerd = engine_command();
    dockerd.extend(socket::listener_args(socket));
    if generation.is_some() && cache.is_none() {
        return Err(ActEngineError(
            "tool generation requires shared cache".into(),
        ));
    }
    let Some(cache) = cache else {
        return Ok(dockerd);
    };
    cache
        .validate()
        .map_err(|error| ActEngineError(error.to_string()))?;
    use crate::ci::engine::{ENGINE_CACHE, ENGINE_WORK, act_artifact, install_act_script};
    if cache.target != ENGINE_CACHE {
        return Err(ActEngineError(
            "startup requires the canonical shared cache mount".into(),
        ));
    }
    let act = act_artifact("amd64")
        .ok_or_else(|| ActEngineError("no pinned startup act artifact".into()))?;
    let script = format!(
        "set -eu; mkdir -p {ENGINE_WORK}/bin; {}; exec 9>&-; exec \"$@\"",
        install_act_script(act),
    );
    let mut command = vec![
        "sh".into(),
        "-ec".into(),
        script,
        "bosn-act-bootstrap".into(),
    ];
    if let Some(generation) = generation {
        generation
            .validate()
            .map_err(|error| ActEngineError(error.to_string()))?;
        command.extend([
            format!("{ENGINE_WORK}/bin/act"),
            "--cache-server-path".into(),
            format!("{ENGINE_CACHE}/toolstore-v1"),
            "cache".into(),
            "tool-exec".into(),
            "--generation".into(),
            generation.id.clone(),
            "--max-bytes".into(),
            generation.max_payload_bytes.to_string(),
            "--apply".into(),
            "--".into(),
        ]);
    }
    command.extend(dockerd);
    Ok(command)
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
    let volumes: Vec<storage_volume::DockerVolume> =
        serde_json::from_slice(document).map_err(|e| ActEngineError(e.to_string()))?;
    let [volume] = volumes.as_slice() else {
        return Err(refused());
    };
    // Every identity label but the two that legitimately vary.
    let expected = cache_volume_labels(ANY_REGISTRY, 0.0)?;
    let labels = volume.labels.as_ref().ok_or_else(refused)?;
    if volume.name != cache.name
        || volume.driver != "local"
        || volume.scope != "local"
        || volume
            .options
            .as_ref()
            .is_some_and(|options| !options.is_empty())
        || expected
            .iter()
            .filter(|(key, _)| ![LABEL_CREATED, LABEL_REGISTRY].contains(&key.as_str()))
            .any(|(key, value)| labels.get(key) != Some(value))
        || labels
            .get(LABEL_REGISTRY)
            .is_none_or(|registry| cache_volume_labels(registry, 0.0).is_err())
        || labels
            .get(LABEL_CREATED)
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
/// (named) and the exact private storage identity (legacy anonymous or v2 named).
fn expected_volumes<'a>(
    cache: Option<&'a ActEngineCacheVolume>,
    storage: EngineStorage,
    storage_name: Option<&'a str>,
) -> Vec<(Option<&'a str>, &'a str)> {
    let cache = cache.map(|cache| (Some(cache.name.as_str()), cache.target.as_str()));
    let disk = (storage == EngineStorage::Disk).then_some((storage_name, STORAGE_TARGET));
    cache.into_iter().chain(disk).collect()
}

/// `HostConfig.Mounts`: exactly the expected volume mounts, read-write.
pub(super) fn host_mounts_match(
    host: &Value,
    cache: Option<&ActEngineCacheVolume>,
    storage: EngineStorage,
    storage_name: Option<&str>,
) -> bool {
    let expected = expected_volumes(cache, storage, storage_name);
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
    storage_name: Option<&str>,
) -> bool {
    let volumes: Vec<&Value> = mounts.iter().filter(|m| m["Type"] == "volume").collect();
    let expected = expected_volumes(cache, storage, storage_name);
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

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn selected_tool_generation_is_frozen_into_native_engine_startup() {
        let limits = super::super::tests::limits();
        let mut intent = super::super::tests::intent();
        let act = crate::ci::pins::act_artifact("amd64").unwrap();
        intent.act_version = crate::ci::pins::ACT_VERSION.into();
        intent.act_image_digest = format!("sha256:{}", act.sha256);
        let cache = ActEngineCacheVolume {
            name: "bosn-ci-cache-v1".into(),
            target: crate::ci::engine::ENGINE_CACHE.into(),
        };
        let mut document =
            serde_json::to_value(creation_profile_with_cache(limits, Some(cache)).unwrap())
                .unwrap();
        document["tool_generation"] = serde_json::json!({
            "id": "a".repeat(64), "max_payload_bytes": 33554432,
            "overlay_recipe_sha256": crate::ci::engine::tool_overlay_recipe_sha256()
        });
        let profile: ActEngineCreationProfile = serde_json::from_value(document).unwrap();
        profile.validate().unwrap();
        // The producer must recompute the frozen command for its typed binding.
        // An old command hash with a new binding must never authorize creation.
        intent.creation_profile = Some(profile);
        assert!(create_arguments(&intent, ANY_REGISTRY, limits).is_err());
        let profile = intent.creation_profile.as_ref().unwrap();
        intent.creation_profile = Some(
            creation_profile_with_tools(
                limits,
                profile.cache_volume.clone(),
                profile.tool_generation.clone(),
            )
            .unwrap(),
        );
        let args = create_arguments(&intent, ANY_REGISTRY, limits).unwrap();
        assert!(args.windows(2).any(|pair| pair == ["cache", "tool-exec"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--generation", &"a".repeat(64)])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--max-bytes", "33554432"])
        );
        assert!(args.contains(&format!("{}/toolstore-v1", crate::ci::engine::ENGINE_CACHE)));
        let mut altered = intent.creation_profile.clone().unwrap();
        altered
            .tool_generation
            .as_mut()
            .unwrap()
            .overlay_recipe_sha256 = "f".repeat(64);
        intent.creation_profile = Some(altered);
        assert!(create_arguments(&intent, ANY_REGISTRY, limits).is_err());
    }

    #[test]
    fn cache_backed_engine_freezes_verified_act_install_before_init() {
        let limits = super::super::tests::limits();
        let mut intent = super::super::tests::intent();
        let act = crate::ci::pins::act_artifact("amd64").unwrap();
        intent.act_version = crate::ci::pins::ACT_VERSION.into();
        intent.act_image_digest = format!("sha256:{}", act.sha256);
        let cache = ActEngineCacheVolume {
            name: "bosn-ci-cache-v1".into(),
            target: crate::ci::engine::ENGINE_CACHE.into(),
        };
        intent.creation_profile = Some(creation_profile_with_cache(limits, Some(cache)).unwrap());
        let args = create_arguments(&intent, ANY_REGISTRY, limits).unwrap();
        let image = format!("docker.io/library/docker@{}", intent.engine_image_digest);
        let index = args.iter().position(|argument| argument == &image).unwrap();
        let command = &args[index + 1..];
        assert!(
            command[2].contains("sha256sum -c -"),
            "act must be verified before engine INIT"
        );
        assert!(
            command[2].contains(
                crate::ci::pins::act_artifact("amd64")
                    .unwrap()
                    .binary_sha256
            )
        );
        assert!(
            command[2].contains("exec 9>&-"),
            "archive writer must close before engine lifetime"
        );
        // The isolated runtime fixture consumes the compiled command generator.
        println!(
            "BOOTSTRAP_COMMAND_JSON={}",
            serde_json::to_string(command).unwrap()
        );
        assert_eq!(
            command_digest(command).unwrap(),
            intent.creation_profile.unwrap().init_command_sha256
        );
    }

    #[test]
    fn startup_refuses_an_artifact_different_from_the_frozen_intent() {
        let limits = super::super::tests::limits();
        let mut intent = super::super::tests::intent();
        let act = crate::ci::pins::act_artifact("amd64").unwrap();
        intent.act_version = crate::ci::pins::ACT_VERSION.into();
        intent.act_image_digest = format!("sha256:{}", act.sha256);
        intent.creation_profile = Some(
            creation_profile_with_cache(
                limits,
                Some(ActEngineCacheVolume {
                    name: "bosn-ci-cache-v1".into(),
                    target: crate::ci::engine::ENGINE_CACHE.into(),
                }),
            )
            .unwrap(),
        );
        assert!(create_arguments(&intent, ANY_REGISTRY, limits).is_ok());
        let mut wrong = intent.clone();
        wrong.act_version = "old-release".into();
        assert!(create_arguments(&wrong, ANY_REGISTRY, limits).is_err());
        wrong = intent;
        wrong.act_image_digest = format!("sha256:{}", "0".repeat(64));
        assert!(create_arguments(&wrong, ANY_REGISTRY, limits).is_err());
    }

    #[cfg(unix)]
    fn rejected_install_never_enters_init(fail_checksum: bool) {
        use std::os::unix::fs::PermissionsExt;
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let root = temporary.path();
        let tools = root.join("commands");
        std::fs::create_dir(&tools).unwrap();
        let executable = |name: &str, script: &str| {
            let file = tools.join(name);
            std::fs::write(&file, script).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        executable("wget", "#!/bin/sh\nexit 55\n");
        if fail_checksum {
            executable(
                "tar",
                "#!/bin/sh\nprintf '#!/bin/sh\\nexit 0\\n' > \"$4/act\"; chmod 700 \"$4/act\"\n",
            );
            executable(
                "sha256sum",
                "#!/bin/sh\nIFS= read -r row; case \"$row\" in */bin/act) exit 53 ;; *) exit 0 ;; esac\n",
            );
        } else {
            executable("tar", "#!/bin/sh\nexit 37\n");
            executable("sha256sum", "#!/bin/sh\nexit 0\n");
        }
        let mut command = engine_command_with_cache(Some(&ActEngineCacheVolume {
            name: "bosn-ci-cache-v1".into(),
            target: crate::ci::engine::ENGINE_CACHE.into(),
        }))
        .unwrap();
        command.truncate(4);
        command[2] = command[2]
            .replace(
                crate::ci::engine::ENGINE_CACHE,
                root.join("cache").to_str().unwrap(),
            )
            .replace(
                crate::ci::engine::ENGINE_WORK,
                root.join("work").to_str().unwrap(),
            );
        let admitted = root.join("init-started");
        command.extend(
            [
                "sh",
                "-ec",
                "printf started > \"$1\"",
                "test-init",
                admitted.to_str().unwrap(),
            ]
            .map(str::to_owned),
        );
        let result = std::process::Command::new(&command[0])
            .args(&command[1..])
            .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
            .output()
            .unwrap();
        assert!(
            !admitted.exists(),
            "unverified install entered INIT: checksum_failure={fail_checksum}, exit={:?}",
            result.status.code()
        );
        assert!(!result.status.success());
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_refuses_failed_extraction() {
        rejected_install_never_enters_init(false);
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_refuses_failed_binary_checksum() {
        rejected_install_never_enters_init(true);
    }

    #[test]
    fn typed_machine_cache_inspection_protects_the_volume_boundary() {
        let cache = ActEngineCacheVolume {
            name: "bosn-ci-cache-v1".into(),
            target: "/bosn/cache".into(),
        };
        let valid = serde_json::json!([{
            "Name": cache.name,
            "Driver": "local",
            "Scope": "local",
            "Options": null,
            "Labels": cache_volume_labels(ANY_REGISTRY, 1.0).unwrap(),
        }]);
        assert!(verify_cache_volume(&serde_json::to_vec(&valid).unwrap(), &cache).is_ok());
        for (field, value) in [
            ("Options", serde_json::json!({"device": "/host"})),
            ("Labels", serde_json::json!({LABEL_REGISTRY: 123})),
            ("Driver", serde_json::json!("foreign")),
        ] {
            let mut document = valid.clone();
            document[0][field] = value;
            assert!(verify_cache_volume(&serde_json::to_vec(&document).unwrap(), &cache).is_err());
        }
    }
}
