//! Verified, bounded OCI layout archive transport; no engine or network I/O.
use crate::act_image::ActImagePackage;
use kernal_api::hash::Sha256Hasher;
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write};

#[derive(Debug)]
pub enum ActArchiveError {
    Invalid(&'static str),
    Io(std::io::Error),
}
impl std::fmt::Display for ActArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => f.write_str(s),
            Self::Io(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for ActArchiveError {}
impl From<std::io::Error> for ActArchiveError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
/// Borrowed verified transport blob. Repeated identical blobs are deduplicated.
pub struct ActArchiveBlob<'a> {
    pub digest: &'a str,
    pub bytes: &'a [u8],
}

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
const DOCKER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const TAR: &str = "application/vnd.oci.image.layer.v1.tar";
const GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const MAX_JSON: usize = 1024 * 1024;
// USTAR's 11 octal size digits cannot represent 16 GiB layers.
const MAX_BLOB: u64 = 0o77777777777;
fn invalid(s: &'static str) -> ActArchiveError {
    ActArchiveError::Invalid(s)
}
fn hash(b: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(b))
}
fn valid_digest(d: &str) -> bool {
    d.strip_prefix("sha256:").is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn descriptor(v: &Value, media: &str, digest: &str, size: u64) -> Result<(), ActArchiveError> {
    if !v.is_object()
        || v["mediaType"] != media
        || v["digest"] != digest
        || v["size"].as_u64() != Some(size)
        || v.get("urls").is_some()
        || v.get("data").is_some()
    {
        return Err(invalid("image descriptor mismatch or external content"));
    }
    Ok(())
}
fn add_blob<'a>(
    blobs: &mut BTreeMap<String, &'a [u8]>,
    digest: &str,
    bytes: &'a [u8],
) -> Result<(), ActArchiveError> {
    if bytes.is_empty()
        || bytes.len() as u64 > MAX_BLOB
        || !valid_digest(digest)
        || hash(bytes) != digest
    {
        return Err(invalid("blob digest or size mismatch"));
    }
    if blobs.get(digest).is_some_and(|old| *old != bytes) {
        return Err(invalid("conflicting duplicate blob"));
    }
    blobs.insert(digest.to_owned(), bytes);
    Ok(())
}
/// Verify the complete OCI graph and archive size before the first write.
/// The ceiling includes USTAR headers, padding and terminator. Output errors
/// can leave a partial archive: callers must stage privately and only import
/// after success. Reference names are deliberately single safe ASCII labels.
/// Base blobs may repeat identically; undeclared blobs are refused. No OCI
/// publication or Docker import is performed here.
pub fn write_act_oci_archive(
    package: &ActImagePackage,
    base_blobs: &[ActArchiveBlob<'_>],
    reference_name: &str,
    max_archive_bytes: u64,
    writer: &mut impl Write,
) -> Result<u64, ActArchiveError> {
    if reference_name.is_empty()
        || reference_name.len() > 121
        || !reference_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        || !reference_name.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(invalid("unsafe OCI reference name"));
    }
    if package.runner_manifest.len() > MAX_JSON
        || package.runner_config.len() > MAX_JSON
        || package.manifest.len() > MAX_JSON
        || package.config.len() > MAX_JSON
        || package.layer.len() > 64 * 1024 * 1024 + 16384
        || package.base_layers.len() > 128
        || base_blobs.len() > 256
        || !valid_digest(&package.binary_digest)
    {
        return Err(invalid("package outside archive bounds"));
    }
    let manifest: Value =
        serde_json::from_slice(&package.manifest).map_err(|_| invalid("invalid manifest JSON"))?;
    let config: Value =
        serde_json::from_slice(&package.config).map_err(|_| invalid("invalid config JSON"))?;
    if manifest["schemaVersion"] != 2
        || manifest["mediaType"] != MANIFEST
        || config["os"] != "linux"
        || config["architecture"] != "amd64"
        || !config["config"].is_object()
        || config["config"].get("Volumes").is_some()
    {
        return Err(invalid("unsupported manifest or config"));
    }
    if manifest["annotations"]["com.zackees.bosn.act.binary-sha256"] != package.binary_digest
        || config["config"]["Labels"]["com.zackees.bosn.act.binary-sha256"] != package.binary_digest
    {
        return Err(invalid("binary identity metadata mismatch"));
    }
    descriptor(
        &manifest["config"],
        CONFIG,
        &package.config_digest,
        package.config.len() as u64,
    )?;
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| invalid("missing layers"))?;
    let diffs = config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or_else(|| invalid("missing DiffIDs"))?;
    if layers.len() != package.base_layers.len() + 1
        || diffs.len() != layers.len()
        || config["rootfs"]["type"] != "layers"
        || diffs.iter().any(|v| !v.as_str().is_some_and(valid_digest))
    {
        return Err(invalid("layer graph mismatch"));
    }
    let layer_digest = hash(&package.layer);
    descriptor(
        layers.last().unwrap(),
        TAR,
        &layer_digest,
        package.layer.len() as u64,
    )?;
    if diffs.last().unwrap() != &layer_digest {
        return Err(invalid("Act layer DiffID mismatch"));
    }
    // Preserve and validate the independently pinned runner graph. Sharing layer
    // bytes alone does not make the runner image available in a fresh engine.
    let runner_manifest: Value = serde_json::from_slice(&package.runner_manifest)
        .map_err(|_| invalid("invalid runner manifest JSON"))?;
    let runner_config: Value = serde_json::from_slice(&package.runner_config)
        .map_err(|_| invalid("invalid runner config JSON"))?;
    let runner_layers = runner_manifest["layers"]
        .as_array()
        .ok_or_else(|| invalid("missing runner layers"))?;
    let runner_diffs = runner_config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or_else(|| invalid("missing runner DiffIDs"))?;
    if runner_manifest["schemaVersion"] != 2
        || !matches!(
            runner_manifest["mediaType"].as_str(),
            Some(MANIFEST | DOCKER_MANIFEST)
        )
        || runner_config["os"] != "linux"
        || runner_config["architecture"] != "amd64"
        || !runner_config["config"].is_object()
        || runner_config["config"].get("Volumes").is_some()
        || runner_config["rootfs"]["type"] != "layers"
        || runner_layers.len() != package.base_layers.len()
        || runner_diffs.as_slice() != &diffs[..package.base_layers.len()]
        || manifest["annotations"]["com.zackees.bosn.act.base-manifest"]
            != package.runner_manifest_digest
    {
        return Err(invalid("runner graph mismatch"));
    }
    let runner_config_media = runner_manifest["config"]["mediaType"]
        .as_str()
        .filter(|media| matches!(*media, CONFIG | DOCKER_CONFIG))
        .ok_or_else(|| invalid("unsupported runner config media type"))?;
    descriptor(
        &runner_manifest["config"],
        runner_config_media,
        &package.runner_config_digest,
        package.runner_config.len() as u64,
    )?;
    let mut required = BTreeMap::new();
    for (i, base) in package.base_layers.iter().enumerate() {
        if !matches!(base.media_type.as_str(), TAR | GZIP)
            || !valid_digest(&base.digest)
            || base.size == 0
            || base.size > MAX_BLOB
        {
            return Err(invalid("unsupported base layer"));
        }
        descriptor(&layers[i], &base.media_type, &base.digest, base.size)?;
        let runner_media = runner_layers[i]["mediaType"]
            .as_str()
            .filter(|media| {
                *media == base.media_type || (base.media_type == GZIP && *media == DOCKER_GZIP)
            })
            .ok_or_else(|| invalid("unsupported runner layer media type"))?;
        descriptor(&runner_layers[i], runner_media, &base.digest, base.size)?;
        if base.media_type == TAR && diffs[i] != base.digest {
            return Err(invalid("plain base layer DiffID mismatch"));
        }
        if required
            .insert(base.digest.as_str(), base.size)
            .is_some_and(|size| size != base.size)
        {
            return Err(invalid("conflicting base descriptors"));
        }
    }
    let mut blobs = BTreeMap::new();
    for blob in base_blobs {
        if required.get(blob.digest).copied() != Some(blob.bytes.len() as u64) {
            return Err(invalid("extra or wrong-size base blob"));
        }
        add_blob(&mut blobs, blob.digest, blob.bytes)?;
    }
    if required.keys().any(|d| !blobs.contains_key(*d)) {
        return Err(invalid("missing base blob"));
    }
    add_blob(
        &mut blobs,
        &package.runner_manifest_digest,
        &package.runner_manifest,
    )?;
    add_blob(
        &mut blobs,
        &package.runner_config_digest,
        &package.runner_config,
    )?;
    add_blob(&mut blobs, &package.manifest_digest, &package.manifest)?;
    add_blob(&mut blobs, &package.config_digest, &package.config)?;
    add_blob(&mut blobs, &layer_digest, &package.layer)?;
    let layout = b"{\"imageLayoutVersion\":\"1.0.0\"}";
    let index = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {"mediaType": MANIFEST, "digest": package.manifest_digest,
             "size": package.manifest.len(), "platform": {"architecture":"amd64","os":"linux"},
             "annotations": {"org.opencontainers.image.ref.name": reference_name}},
            {"mediaType": runner_manifest["mediaType"], "digest": package.runner_manifest_digest,
             "size": package.runner_manifest.len(), "platform": {"architecture":"amd64","os":"linux"},
             "annotations": {"org.opencontainers.image.ref.name": format!("{reference_name}-runner")}}
        ]
    })).map_err(|_| invalid("index serialization failed"))?;
    let mut entries = vec![
        ("oci-layout".to_owned(), layout.as_slice()),
        ("index.json".to_owned(), index.as_slice()),
    ];
    entries.extend(
        blobs
            .iter()
            .map(|(digest, bytes)| (format!("blobs/sha256/{}", &digest[7..]), *bytes)),
    );
    let total = entries.iter().try_fold(1024u64, |total, (_, bytes)| {
        let padded = (bytes.len() as u64)
            .checked_add(511)
            .map(|n| n / 512 * 512)
            .ok_or_else(|| invalid("archive size overflow"))?;
        total
            .checked_add(512)
            .and_then(|n| n.checked_add(padded))
            .ok_or_else(|| invalid("archive size overflow"))
    })?;
    if total > max_archive_bytes {
        return Err(invalid("archive byte ceiling exceeded"));
    }
    for (path, bytes) in entries {
        write_entry(writer, &path, bytes)?;
    }
    writer.write_all(&[0; 1024])?;
    Ok(total)
}
fn octal(field: &mut [u8], value: u64) {
    field.fill(b'0');
    let text = format!("{:o}", value);
    let start = field.len() - 1 - text.len();
    field[start..start + text.len()].copy_from_slice(text.as_bytes());
    *field.last_mut().unwrap() = 0;
}
fn write_entry(writer: &mut impl Write, path: &str, bytes: &[u8]) -> Result<(), ActArchiveError> {
    let mut header = [0u8; 512];
    header[..path.len()].copy_from_slice(path.as_bytes());
    octal(&mut header[100..108], 0o644);
    octal(&mut header[108..116], 0);
    octal(&mut header[116..124], 0);
    octal(&mut header[124..136], bytes.len() as u64);
    octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let sum = header.iter().map(|b| u64::from(*b)).sum();
    octal(&mut header[148..155], sum);
    header[155] = b' ';
    writer.write_all(&header)?;
    writer.write_all(bytes)?;
    let padding = (512 - bytes.len() % 512) % 512;
    writer.write_all(&[0; 512][..padding])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::act_image::package_act_image;
    fn digest(b: &[u8]) -> String {
        format!("sha256:{}", Sha256Hasher::digest(b))
    }
    fn fixture() -> (ActImagePackage, Vec<u8>) {
        let base = b"base layer fixture".to_vec();
        let config = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":[digest(&base)]}})).unwrap();
        let manifest = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":MANIFEST,"config":{"mediaType":CONFIG,"digest":digest(&config),"size":config.len()},"layers":[{"mediaType":TAR,"digest":digest(&base),"size":base.len()}]})).unwrap();
        let binary = b"synthetic verified Act binary";
        (
            package_act_image(
                &manifest,
                &digest(&manifest),
                &config,
                &digest(&config),
                binary,
                &digest(binary),
                "0.2.88",
            )
            .unwrap(),
            base,
        )
    }

    #[test]
    fn deterministic_system_tar_and_oci_references() {
        let (p, base) = fixture();
        let blob = ActArchiveBlob {
            digest: &p.base_layers[0].digest,
            bytes: &base,
        };
        let mut out = Vec::new();
        let size = write_act_oci_archive(&p, &[blob], "bosn-act", 1024 * 1024, &mut out).unwrap();
        assert_eq!(size, out.len() as u64);
        let mut second = Vec::new();
        write_act_oci_archive(
            &p,
            &[ActArchiveBlob {
                digest: &p.base_layers[0].digest,
                bytes: &base,
            }],
            "bosn-act",
            size,
            &mut second,
        )
        .unwrap();
        assert_eq!(out, second);
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("image.tar");
        std::fs::write(&archive, &out).unwrap();
        assert!(
            std::process::Command::new("tar")
                .arg("-xf")
                .arg(&archive)
                .arg("-C")
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let index: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("index.json")).unwrap()).unwrap();
        assert_eq!(index["manifests"][0]["digest"], p.manifest_digest);
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            "bosn-act"
        );
        assert_eq!(index["manifests"].as_array().unwrap().len(), 2);
        assert_eq!(index["manifests"][1]["digest"], p.runner_manifest_digest);
        assert_eq!(
            index["manifests"][1]["annotations"]["org.opencontainers.image.ref.name"],
            "bosn-act-runner"
        );
        for (d, b) in [
            (&p.runner_manifest_digest, p.runner_manifest.as_slice()),
            (&p.runner_config_digest, p.runner_config.as_slice()),
            (&p.manifest_digest, p.manifest.as_slice()),
            (&p.config_digest, p.config.as_slice()),
            (&p.base_layers[0].digest, base.as_slice()),
        ] {
            assert_eq!(
                std::fs::read(dir.path().join(format!("blobs/sha256/{}", &d[7..]))).unwrap(),
                b
            );
        }
        let layout: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("oci-layout")).unwrap()).unwrap();
        assert_eq!(layout["imageLayoutVersion"], "1.0.0");
        let manifest: Value = serde_json::from_slice(&p.manifest).unwrap();
        for entry in manifest["layers"].as_array().unwrap() {
            let identity = entry["digest"].as_str().unwrap();
            let bytes =
                std::fs::read(dir.path().join(format!("blobs/sha256/{}", &identity[7..]))).unwrap();
            assert_eq!(hash(&bytes), identity);
            assert_eq!(bytes.len() as u64, entry["size"].as_u64().unwrap());
        }
        assert_ne!(p.manifest_digest, p.config_digest);
        assert_ne!(p.binary_digest, p.config_digest);
    }
    #[test]
    fn refusals_write_nothing() {
        let (p, base) = fixture();
        let check = |p: &ActImagePackage, blobs: &[ActArchiveBlob<'_>], name: &str, limit: u64| {
            let mut out = Vec::new();
            assert!(write_act_oci_archive(p, blobs, name, limit, &mut out).is_err());
            assert!(out.is_empty());
        };
        check(&p, &[], "act", 100000);
        check(
            &p,
            &[ActArchiveBlob {
                digest: &p.base_layers[0].digest,
                bytes: b"corrupt",
            }],
            "act",
            100000,
        );
        check(
            &p,
            &[
                ActArchiveBlob {
                    digest: &p.base_layers[0].digest,
                    bytes: &base,
                },
                ActArchiveBlob {
                    digest: &digest(b"extra"),
                    bytes: b"extra",
                },
            ],
            "act",
            100000,
        );
        check(
            &p,
            &[
                ActArchiveBlob {
                    digest: &p.base_layers[0].digest,
                    bytes: &base,
                },
                ActArchiveBlob {
                    digest: &p.base_layers[0].digest,
                    bytes: b"conflict",
                },
            ],
            "act",
            100000,
        );
        for name in ["", "../act", "act/name", "act\nname", "act:tag"] {
            check(
                &p,
                &[ActArchiveBlob {
                    digest: &p.base_layers[0].digest,
                    bytes: &base,
                }],
                name,
                100000,
            );
        }
        check(
            &p,
            &[ActArchiveBlob {
                digest: &p.base_layers[0].digest,
                bytes: &base,
            }],
            "act",
            1,
        );
        let mut bad = p.clone();
        bad.config.push(0);
        check(
            &bad,
            &[ActArchiveBlob {
                digest: &p.base_layers[0].digest,
                bytes: &base,
            }],
            "act",
            100000,
        );
        let mut bad = p.clone();
        bad.base_layers[0].size += 1;
        check(
            &bad,
            &[ActArchiveBlob {
                digest: &p.base_layers[0].digest,
                bytes: &base,
            }],
            "act",
            100000,
        );
    }

    #[test]
    fn independently_rehashed_descriptor_tampering_is_refused() {
        let (p, base) = fixture();
        for change in ["size", "digest", "urls", "mediaType", "oversized"] {
            let mut bad = p.clone();
            let mut m: Value = serde_json::from_slice(&bad.manifest).unwrap();
            let field = if change == "oversized" {
                "size"
            } else {
                change
            };
            m["layers"][0][field] = match change {
                "size" => json!(base.len() + 1),
                "oversized" => {
                    bad.base_layers[0].size = MAX_BLOB + 1;
                    json!(MAX_BLOB + 1)
                }
                "digest" => json!(digest(b"foreign")),
                "urls" => json!(["https://foreign.invalid/layer"]),
                _ => json!("application/octet-stream"),
            };
            bad.manifest = serde_json::to_vec(&m).unwrap();
            bad.manifest_digest = digest(&bad.manifest);
            let mut output = Vec::new();
            assert!(
                write_act_oci_archive(
                    &bad,
                    &[ActArchiveBlob {
                        digest: &p.base_layers[0].digest,
                        bytes: &base
                    }],
                    "act",
                    100000,
                    &mut output
                )
                .is_err()
            );
            assert!(output.is_empty());
        }
    }

    #[test]
    fn docker_runner_media_types_keep_original_pinned_graph() {
        let base = b"synthetic compressed layer";
        let config = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":[digest(b"uncompressed fixture")]}})).unwrap();
        let manifest = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":DOCKER_MANIFEST,"config":{"mediaType":DOCKER_CONFIG,"digest":digest(&config),"size":config.len()},"layers":[{"mediaType":DOCKER_GZIP,"digest":digest(base),"size":base.len()}]})).unwrap();
        let binary = b"Act fixture";
        let p = package_act_image(
            &manifest,
            &digest(&manifest),
            &config,
            &digest(&config),
            binary,
            &digest(binary),
            "0.2.88",
        )
        .unwrap();
        let mut output = Vec::new();
        write_act_oci_archive(
            &p,
            &[ActArchiveBlob {
                digest: &digest(base),
                bytes: base,
            }],
            "act",
            100000,
            &mut output,
        )
        .unwrap();
        assert_eq!(p.runner_manifest, manifest);
        assert_eq!(p.runner_config, config);
        assert!(!output.is_empty());
    }

    #[test]
    fn runner_graph_tampering_writes_nothing() {
        let (p, base) = fixture();
        for change in [
            "manifest_bytes",
            "config_bytes",
            "config_descriptor",
            "layers",
            "diff_ids",
            "pin",
        ] {
            let mut bad = p.clone();
            match change {
                "manifest_bytes" => bad.runner_manifest.push(0),
                "config_bytes" => bad.runner_config.push(0),
                "pin" => bad.runner_manifest_digest = digest(b"foreign runner"),
                "config_descriptor" | "layers" => {
                    let mut value: Value = serde_json::from_slice(&bad.runner_manifest).unwrap();
                    if change == "layers" {
                        value["layers"] = json!([]);
                    } else {
                        value["config"]["digest"] = json!(digest(b"foreign config"));
                    }
                    bad.runner_manifest = serde_json::to_vec(&value).unwrap();
                    bad.runner_manifest_digest = digest(&bad.runner_manifest);
                    let mut driver: Value = serde_json::from_slice(&bad.manifest).unwrap();
                    driver["annotations"]["com.zackees.bosn.act.base-manifest"] =
                        json!(bad.runner_manifest_digest);
                    bad.manifest = serde_json::to_vec(&driver).unwrap();
                    bad.manifest_digest = digest(&bad.manifest);
                }
                _ => {
                    let mut value: Value = serde_json::from_slice(&bad.runner_config).unwrap();
                    value["rootfs"]["diff_ids"][0] = json!(digest(b"foreign filesystem"));
                    bad.runner_config = serde_json::to_vec(&value).unwrap();
                    bad.runner_config_digest = digest(&bad.runner_config);
                }
            }
            let mut output = Vec::new();
            assert!(
                write_act_oci_archive(
                    &bad,
                    &[ActArchiveBlob {
                        digest: &p.base_layers[0].digest,
                        bytes: &base,
                    }],
                    "act",
                    100000,
                    &mut output
                )
                .is_err(),
                "{change}"
            );
            assert!(output.is_empty(), "{change}");
        }
    }

    #[test]
    fn identical_duplicates_deduplicate_and_writer_failure_propagates() {
        let (p, base) = fixture();
        let one = || ActArchiveBlob {
            digest: &p.base_layers[0].digest,
            bytes: &base,
        };
        let mut a = Vec::new();
        let size = write_act_oci_archive(&p, &[one()], "act", 100000, &mut a).unwrap();
        let mut b = Vec::new();
        write_act_oci_archive(&p, &[one(), one()], "act", size, &mut b).unwrap();
        assert_eq!(a, b);
        let mut none = Vec::new();
        assert!(write_act_oci_archive(&p, &[one()], "act", size - 1, &mut none).is_err());
        assert!(none.is_empty());
        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected disk failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(matches!(
            write_act_oci_archive(&p, &[one()], "act", size, &mut Failed),
            Err(ActArchiveError::Io(_))
        ));
    }
}
