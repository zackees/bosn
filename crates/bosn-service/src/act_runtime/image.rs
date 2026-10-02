//! Verifying loaded images and the frozen source snapshot.

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
pub(super) fn tar_number(field: &[u8]) -> std::io::Result<usize> {
    let field = std::str::from_utf8(field)
        .map_err(|_| error("non-octal snapshot size"))?
        .trim_matches(['\0', ' ']);
    usize::from_str_radix(if field.is_empty() { "0" } else { field }, 8)
        .map_err(|_| error("invalid snapshot integer"))
}
/// Only ordinary USTAR files/directories are admitted. Links, devices, PAX path
/// overrides and sparse extensions require a separately reviewed snapshot seam.
pub(super) fn verify_snapshot(tar: &[u8]) -> std::io::Result<()> {
    if tar.len() > 512 << 20 || tar.len() < 1024 || !tar.len().is_multiple_of(512) {
        return Err(error("snapshot outside byte bounds"));
    }
    let mut offset = 0;
    let mut workflows = 0;
    let mut paths = std::collections::BTreeSet::new();
    while offset + 512 <= tar.len() {
        let header = &tar[offset..offset + 512];
        if header.iter().all(|b| *b == 0) {
            if offset + 1024 > tar.len() || !tar[offset..].iter().all(|b| *b == 0) || workflows == 0
            {
                return Err(error("snapshot terminator or workflow inventory invalid"));
            }
            return Ok(());
        }
        let sum: usize = header
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    usize::from(*b)
                }
            })
            .sum();
        if sum != tar_number(&header[148..156])?
            || &header[257..263] != b"ustar\0"
            || !matches!(header[156], 0 | b'0' | b'5')
            || tar_number(&header[108..116])? != 0
            || tar_number(&header[116..124])? != 0
            || tar_number(&header[100..108])? & !0o777 != 0
        {
            return Err(error("unsafe snapshot header"));
        }
        let string = |bytes: &[u8]| {
            std::str::from_utf8(bytes.split(|b| *b == 0).next().unwrap())
                .map(str::to_owned)
                .map_err(|_| error("snapshot path not UTF-8"))
        };
        let name = string(&header[..100])?;
        let prefix = string(&header[345..500])?;
        let full = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = full.trim_end_matches('/');
        if path.is_empty()
            || !Path::new(path)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            || !paths.insert(path.to_owned())
        {
            return Err(error("snapshot path traversal or duplicate"));
        }
        let size = tar_number(&header[124..136])?;
        if header[156] == b'5' && size != 0 {
            return Err(error("snapshot directory contains bytes"));
        }
        if header[156] != b'5'
            && path.starts_with(".github/workflows/")
            && (path.ends_with(".yml") || path.ends_with(".yaml"))
        {
            workflows += 1;
        }
        offset = offset
            .checked_add(512)
            .and_then(|n| {
                size.checked_add(511)
                    .and_then(|s| n.checked_add(s / 512 * 512))
            })
            .filter(|n| *n <= tar.len())
            .ok_or_else(|| error("truncated snapshot content"))?;
    }
    Err(error("snapshot lacks terminator"))
}
