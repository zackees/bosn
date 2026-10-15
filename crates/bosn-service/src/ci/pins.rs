//! The one place `bosn ci`'s act artifacts are pinned, and how each is
//! verified once it is inside an engine.
//!
//! - act: the release tarball by its sha256 and the extracted binary by its
//!   own, both checked inside the engine before act is run.
//! - The runner image: the linux/amd64 manifest of
//!   `catthehacker/ubuntu:act-24.04`, pinned by digest. Its publisher
//!   manifest and config ship with the daemon (`pins_data/`); after the
//!   engine loads or pulls it, `docker image inspect` must show exactly that
//!   manifest, config, execution config and rootfs
//!   ([`crate::act_runtime::verify_loaded_image`]), so a cached image tar is
//!   trusted only once its identity is proven.

pub use bosn_core::act::ACT_VERSION;

/// The runner image every `ubuntu-*` label maps to: the linux/amd64 manifest
/// of `catthehacker/ubuntu:act-24.04`.
/// It is the default entry of [`bosn_core::runner_images::RunnerTable`].
pub const RUNNER_IMAGE: &str = bosn_core::runner_images::DEFAULT_RUNNER_IMAGE;
/// Original PATH of the pinned runner, preserved when adding hosted tools.
pub const RUNNER_PATH: &str = "/opt/acttoolcache/node/24.19.0/x64/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games:/snap/bin";
/// The config [`RUNNER_IMAGE`]'s manifest names.
pub const RUNNER_CONFIG: &str =
    "sha256:cb041d0df9a749a73358ded823dd44f7c24111ef3efd152a3e6f3f4e3846f153";

/// The pinned publisher bytes, reached through one base directory.
macro_rules! pins_data {
    ($name:literal) => {
        include_bytes!(concat!("pins_data/", $name))
    };
}
const RUNNER_MANIFEST_BYTES: &[u8] = pins_data!("runner-manifest.json");
const RUNNER_CONFIG_BYTES: &[u8] = pins_data!("runner-config.json");

/// The runner manifest digest (`sha256:…`) recorded in each engine intent.
pub fn runner_manifest() -> &'static str {
    RUNNER_IMAGE
        .rsplit_once('@')
        .map_or("", |(_, digest)| digest)
}

/// The runner image's engine-local name. The pinned image is loaded from the
/// cache volume under this tag (a digest reference cannot be saved and
/// loaded portably), and act's platform mappings name it.
pub fn runner_tag() -> String {
    let digest = runner_manifest().trim_start_matches("sha256:");
    format!("bosn/act-runner:{}", &digest[..12.min(digest.len())])
}

/// The runner image as act2's serve mode loads and proves it (`act serve
/// prepare`, zackees/act2#67): the pull reference, the engine-local tag, the
/// image tar kept in the cache volume, and the publisher manifest and config
/// bytes whose digests are the pins. Serve refuses an image that is not them.
#[derive(Debug, serde::Serialize)]
pub struct RunnerPin {
    reference: &'static str,
    tag: String,
    archive: String,
    platform: &'static str,
    #[serde(serialize_with = "base64_bytes")]
    manifest: &'static [u8],
    #[serde(serialize_with = "base64_bytes")]
    config: &'static [u8],
}

fn base64_bytes<S: serde::Serializer>(bytes: &&[u8], serializer: S) -> Result<S::Ok, S::Error> {
    use base64::Engine as _;
    serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// The pinned runner, its tar kept under `images_dir`.
pub fn runner_pin(images_dir: &str) -> RunnerPin {
    RunnerPin {
        reference: RUNNER_IMAGE,
        tag: runner_tag(),
        archive: format!("{images_dir}/{}.tar", runner_tag().replace([':', '/'], "-")),
        platform: "linux/amd64",
        manifest: RUNNER_MANIFEST_BYTES,
        config: RUNNER_CONFIG_BYTES,
    }
}

/// One pinned act release for an engine architecture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActArtifact {
    pub url: &'static str,
    /// The release tarball.
    pub sha256: &'static str,
    /// The `act` binary inside it.
    pub binary_sha256: &'static str,
}

/// The pinned act release for the engine's architecture. The engine image
/// is linux/amd64, so only the x86_64 build can run in it.
pub fn act_artifact(architecture: &str) -> Option<ActArtifact> {
    match architecture {
        "x86_64" | "amd64" => Some(ActArtifact {
            // act2, zackees' fork of nektos/act (zackees/ci.yml ACT-002):
            // GitHub-parity RUNNER_ENVIRONMENT, an init in hosted-runner job
            // containers, --workflow-overlay (#424), and ordinary hosted
            // Linux shell/Node identity with preserved Docker socket access,
            // concurrent v4 artifact block assembly, raw stream tagging,
            // immutable generation admission with bounded physical retention,
            // and typed durable tool-recovery references (reserve/release)
            // with a finite lifetime that fence the v2 recovery store.
            // act2.15 retains exact-key cleanup and protected action checkout
            // reads, and escapes artifact signatures for stock Azure clients.
            // act2.17 adds `act serve` (act2#62), which owns each run's scope
            // inside the shared engine (#547), and `act doctor` (act2#61).
            // act2.18: serve owns the run proxy, runner-image load, tool
            // cache and cache budget (act2#67).
            url: "https://github.com/zackees/act2/releases/download/v0.2.89-act2.18/act_Linux_x86_64.tar.gz",
            sha256: "5ebd6231663bc3b2c54d9777cd26d9333575325ebc28a3bd8f9f0ef7ada13ed7",
            binary_sha256: "a64a5e95cd570d165cecb83097c7b57f963e9f00b2e4a61c2bbc3574606cdc41",
        }),
        _ => None,
    }
}

/// The daemon's effective execution pins, using the existing shared contract.
/// Unsupported hosts advertise no profile rather than promising an engine.
pub(crate) fn execution_pins() -> Option<bosn_core::act::ActPins> {
    let act = act_artifact(std::env::consts::ARCH)?;
    Some(bosn_core::act::ActPins {
        interface_schema: bosn_core::act::ACT_ADAPTER_SCHEMA,
        act_version: ACT_VERSION.into(),
        act_binary_digest: format!("sha256:{}", act.binary_sha256),
        engine_manifest_digest: crate::act_engine::ENGINE_MANIFEST.into(),
        engine_config_digest: crate::act_engine::ENGINE_CONFIG.into(),
        runner_manifest_digest: runner_manifest().into(),
        runner_config_digest: RUNNER_CONFIG.into(),
    })
}

/// Refuse an enrolled queued run before creating an engine when an upgrade
/// changes its provider. Legacy records remain readable but carry no profile.
pub(crate) fn require_execution_pins(pins: Option<&bosn_core::act::ActPins>) -> Result<(), String> {
    if pins.is_some() && pins != execution_pins().as_ref() {
        return Err("execution provider pins changed since submission; resubmit the run".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::hash::Sha256Hasher;
    use serde_json::Value;

    #[test]
    fn execution_pins_bind_receipts_and_refuse_changed_queued_providers() {
        let pins = execution_pins().expect("test host has a pinned act build");
        let record = crate::ci::tests::sample_record("profile-test");
        assert_eq!(record.execution_pins.as_ref(), Some(&pins));
        let mut encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(
            encoded["execution_pins"],
            serde_json::to_value(&pins).unwrap()
        );
        encoded.as_object_mut().unwrap().remove("execution_pins");
        let legacy: crate::ci::wire::RunRecord = serde_json::from_value(encoded).unwrap();
        assert!(legacy.execution_pins.is_none());
        assert!(require_execution_pins(None).is_ok());
        assert!(require_execution_pins(Some(&pins)).is_ok());
        for changed in 0..7 {
            let mut stale = pins.clone();
            match changed {
                0 => stale.interface_schema += 1,
                1 => stale.act_version.push_str("-changed"),
                2 => stale.act_binary_digest.push('0'),
                3 => stale.engine_manifest_digest.push('0'),
                4 => stale.engine_config_digest.push('0'),
                5 => stale.runner_manifest_digest.push('0'),
                _ => stale.runner_config_digest.push('0'),
            }
            assert!(require_execution_pins(Some(&stale)).is_err());
        }
    }

    #[test]
    fn retries_bind_the_current_provider_without_changing_the_source() {
        let current = crate::ci::tests::sample_record("current-provider");
        for legacy in [false, true] {
            let mut historical = current.clone();
            historical.id = "historical-provider".into();
            historical.act_version = "old-act".into();
            historical.runner_image = "old-runner".into();
            if legacy {
                historical.execution_pins = None;
            } else {
                historical.execution_pins.as_mut().unwrap().act_version = "old-act".into();
            }
            let retry = historical.retry("fresh-retry".into(), None);
            assert_eq!(retry.execution_pins, current.execution_pins);
            assert_eq!(retry.act_version, current.act_version);
            assert_eq!(retry.runner_image, current.runner_image);
            assert_eq!(retry.sha, historical.sha);
            assert_eq!(retry.git_tree, historical.git_tree);
            assert_eq!(retry.tree_digest, historical.tree_digest);
            assert_eq!(retry.payload_sha256, historical.payload_sha256);
            assert_eq!(retry.params, historical.params);
            assert_eq!(retry.retry_of.as_deref(), Some("historical-provider"));
            assert_eq!(historical.act_version, "old-act");
            assert_eq!(historical.runner_image, "old-runner");
        }
    }

    #[test]
    fn runner_path_matches_pinned_image_config() {
        let config: Value = serde_json::from_slice(RUNNER_CONFIG_BYTES).unwrap();
        let expected = format!("PATH={RUNNER_PATH}");
        assert!(
            config["config"]["Env"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry.as_str() == Some(expected.as_str()) })
        );
    }

    fn digest(bytes: &[u8]) -> String {
        format!("sha256:{}", Sha256Hasher::digest(bytes))
    }

    #[test]
    fn the_shipped_runner_proof_is_the_pinned_one() {
        assert_eq!(digest(RUNNER_MANIFEST_BYTES), runner_manifest());
        assert_eq!(digest(RUNNER_CONFIG_BYTES), RUNNER_CONFIG);
        let manifest: Value = serde_json::from_slice(RUNNER_MANIFEST_BYTES).unwrap();
        assert_eq!(manifest["config"]["digest"], RUNNER_CONFIG);
        assert_eq!(runner_tag(), "bosn/act-runner:4f2d5083a9d1");
    }

    #[test]
    fn serve_is_handed_the_pinned_publisher_bytes() {
        use base64::Engine as _;
        let pin = serde_json::to_value(runner_pin("/bosn/cache/images")).unwrap();
        let decode = |field: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(pin[field].as_str().unwrap())
                .unwrap()
        };
        assert_eq!(decode("manifest"), RUNNER_MANIFEST_BYTES);
        assert_eq!(decode("config"), RUNNER_CONFIG_BYTES);
        assert_eq!(pin["reference"], RUNNER_IMAGE);
        assert_eq!(pin["tag"], "bosn/act-runner:4f2d5083a9d1");
        assert_eq!(
            pin["archive"],
            "/bosn/cache/images/bosn-act-runner-4f2d5083a9d1.tar"
        );
        assert_eq!(pin["platform"], "linux/amd64");
    }

    #[test]
    fn act_is_pinned_for_the_engine_architecture_only() {
        let act = act_artifact("x86_64").unwrap();
        assert!(act.url.contains(ACT_VERSION));
        assert_ne!(act.sha256, act.binary_sha256);
        assert!(act_artifact("aarch64").is_none());
    }
}
