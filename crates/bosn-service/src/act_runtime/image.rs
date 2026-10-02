//! Verifying an image loaded into an engine against its pinned publisher bytes.

use super::*;

/// Verify Docker29 containerd image-store metadata. Classic stores without a
/// manifest descriptor fail closed; config IDs alone are not manifest proof.
pub fn verify_loaded_image(
    document: &[u8],
    manifest: &str,
    config: &str,
    manifest_bytes: &[u8],
    config_bytes: &[u8],
) -> std::io::Result<String> {
    if manifest_bytes.len() > 1 << 20
        || config_bytes.len() > 1 << 20
        || hash(manifest_bytes) != manifest
        || hash(config_bytes) != config
    {
        return Err(error("imported image proof bytes do not match pins"));
    }
    let manifest_proof: Value = serde_json::from_slice(manifest_bytes)?;
    let expected: Value = serde_json::from_slice(config_bytes)?;
    if manifest_proof["schemaVersion"] != 2
        || !matches!(
            manifest_proof["mediaType"].as_str(),
            Some(
                "application/vnd.oci.image.manifest.v1+json"
                    | "application/vnd.docker.distribution.manifest.v2+json"
            )
        )
        || manifest_proof["config"]["digest"] != config
        || manifest_proof["config"]["size"].as_u64() != Some(config_bytes.len() as u64)
        || !matches!(
            manifest_proof["config"]["mediaType"].as_str(),
            Some(
                "application/vnd.oci.image.config.v1+json"
                    | "application/vnd.docker.container.image.v1+json"
            )
        )
        || expected["os"] != "linux"
        || expected["architecture"] != "amd64"
        || expected["rootfs"]["type"] != "layers"
    {
        return Err(error(
            "imported image manifest does not bind pinned Linux config",
        ));
    }
    let v: Value = serde_json::from_slice(document)?;
    let records = v
        .as_array()
        .filter(|a| a.len() == 1)
        .ok_or_else(|| error("expected exactly one imported image"))?;
    let image = &records[0];
    for field in ["Env", "Entrypoint", "Cmd"] {
        let empty = json!([]);
        let normalize = |value: &Value| {
            if value.is_null() {
                empty.clone()
            } else {
                value.clone()
            }
        };
        let actual = normalize(&image["Config"][field]);
        let pinned = normalize(&expected["config"][field]);
        if !actual.is_array() || actual != pinned {
            return Err(error(
                "imported image execution config differs from pinned config",
            ));
        }
    }
    for field in ["User", "WorkingDir"] {
        let normalize = |value: &Value| {
            if value.is_null() {
                Some(String::new())
            } else {
                value.as_str().map(str::to_owned)
            }
        };
        if normalize(&image["Config"][field]).is_none()
            || normalize(&image["Config"][field]) != normalize(&expected["config"][field])
        {
            return Err(error(
                "imported image execution config differs from pinned config",
            ));
        }
    }
    let id = image["Id"]
        .as_str()
        .ok_or_else(|| error("missing Docker image ID"))?;
    if !id.strip_prefix("sha256:").is_some_and(|d| {
        d.len() == 64
            && d.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) || (id != manifest && id != config)
        || image["Descriptor"]["digest"] != manifest
        || image["Descriptor"]["mediaType"] != manifest_proof["mediaType"]
        || image["Descriptor"]["size"].as_u64() != Some(manifest_bytes.len() as u64)
        || image["RootFS"]["Type"] != "layers"
        || image["RootFS"]["Layers"] != expected["rootfs"]["diff_ids"]
        || (!image["Config"]["Volumes"].is_null()
            && !image["Config"]["Volumes"]
                .as_object()
                .is_some_and(|v| v.is_empty()))
    {
        return Err(error(
            "imported manifest, config or rootfs identity not established",
        ));
    }
    Ok(id.into())
}
