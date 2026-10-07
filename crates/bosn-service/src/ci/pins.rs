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
pub const RUNNER_IMAGE: &str = "docker.io/catthehacker/ubuntu@sha256:4f2d5083a9d10d018c1c511eb8665cd480553c11975e78fd903a46daa830768b";
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

/// Prove that a `docker image inspect` of [`runner_tag`] in an engine is the
/// pinned runner, whichever way it got there. Returns the image ID.
pub fn verify_runner(inspect: &[u8]) -> Result<String, String> {
    crate::act_runtime::verify_loaded_image(
        inspect,
        runner_manifest(),
        RUNNER_CONFIG,
        RUNNER_MANIFEST_BYTES,
        RUNNER_CONFIG_BYTES,
    )
    .map_err(|error| format!("runner image is not the pinned one: {error}"))
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
            url: "https://github.com/zackees/act2/releases/download/v0.2.89-act2.11/act_Linux_x86_64.tar.gz",
            sha256: "753d2f1e9db1d31088ef0de372b5b124e932481d3b4987ad420de8ce278c9e64",
            binary_sha256: "bd9c112f9fb54eb159e6b45a1223f9ed6e84e8e5ee6f9a1cb27c1d6f913a1c4d",
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::hash::Sha256Hasher;
    use serde_json::{Value, json};

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

    /// What the containerd image store reports for the pinned runner.
    fn inspect() -> Value {
        let config: Value = serde_json::from_slice(RUNNER_CONFIG_BYTES).unwrap();
        let manifest: Value = serde_json::from_slice(RUNNER_MANIFEST_BYTES).unwrap();
        json!([{
            "Id": runner_manifest(),
            "Descriptor": {
                "digest": runner_manifest(),
                "mediaType": manifest["mediaType"],
                "size": RUNNER_MANIFEST_BYTES.len(),
            },
            "Config": {
                "Env": config["config"]["Env"],
                "Entrypoint": config["config"]["Entrypoint"],
                "Cmd": config["config"]["Cmd"],
                "User": config["config"]["User"],
                "WorkingDir": config["config"]["WorkingDir"],
                "Volumes": null,
            },
            "RootFS": {"Type": "layers", "Layers": config["rootfs"]["diff_ids"]},
        }])
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
    fn only_the_pinned_runner_identity_is_accepted() {
        let good = inspect();
        assert_eq!(
            verify_runner(&serde_json::to_vec(&good).unwrap()).unwrap(),
            runner_manifest()
        );
        let tampered: [fn(&mut Value); 4] = [
            |v| v[0]["Descriptor"]["digest"] = json!(format!("sha256:{}", "0".repeat(64))),
            |v| v[0]["RootFS"]["Layers"][0] = json!(format!("sha256:{}", "1".repeat(64))),
            |v| v[0]["Config"]["Entrypoint"] = json!(["/bin/evil"]),
            |v| v[0]["Id"] = json!(format!("sha256:{}", "2".repeat(64))),
        ];
        for tamper in tampered {
            let mut bad = good.clone();
            tamper(&mut bad);
            assert!(verify_runner(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
        assert!(verify_runner(b"[]").is_err());
    }

    #[test]
    fn act_is_pinned_for_the_engine_architecture_only() {
        let act = act_artifact("x86_64").unwrap();
        assert!(act.url.contains(ACT_VERSION));
        assert_ne!(act.sha256, act.binary_sha256);
        assert!(act_artifact("aarch64").is_none());
    }
}
