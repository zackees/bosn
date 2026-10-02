//! Publisher manifest/config proofs and the engine image observations bound to them.

use super::*;

/// Produced only by a successful image inspection bound to the pinned manifest.
/// Docker classic uses the config digest as its ID; containerd may use manifest.
#[derive(Clone, Debug)]
pub struct VerifiedEngineImage {
    pub(super) manifest_digest: String,
    pub(super) docker_image_id: String,
}

/// Hash-verified publisher manifest/config bytes; never derived from Docker annotations.
#[derive(Clone, Debug)]
pub struct VerifiedEngineManifest {
    pub(super) manifest_digest: String,
    pub(super) config_digest: String,
    pub(super) media_type: String,
    pub(super) manifest_size: u64,
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
