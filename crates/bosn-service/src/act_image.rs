//! Deterministic Bosn-owned Act OCI packaging; never an upstream image claim.
//!
//! Inputs are already-fetched bytes and independently pinned SHA-256 identities.
//! This module neither downloads nor extracts release archives, creates Docker
//! resources, nor publishes images. The runtime must fetch and hash every base
//! layer descriptor, assemble an OCI layout, and import it under its owned
//! engine. `manifest_digest` is the real generated OCI manifest identity;
//! `config_digest` is the Docker image ID, and `binary_digest` is neither.
use kernal_api::hash::Sha256Hasher;
use serde_json::{Value, json};

const MAX_JSON: usize = 1024 * 1024;
const MAX_BINARY: usize = 64 * 1024 * 1024;
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
const OCI_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const EPOCH: &str = "1970-01-01T00:00:00Z";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActImageError(pub &'static str);
impl std::fmt::Display for ActImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ActImageError {}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActBaseLayer {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActImagePackage {
    /// Original pinned runner graph, imported alongside the Act driver.
    pub runner_manifest: Vec<u8>,
    pub runner_config: Vec<u8>,
    pub runner_manifest_digest: String,
    pub runner_config_digest: String,
    pub manifest: Vec<u8>,
    pub config: Vec<u8>,
    /// Uncompressed POSIX USTAR; its blob digest equals its rootfs DiffID.
    pub layer: Vec<u8>,
    pub manifest_digest: String,
    pub config_digest: String,
    pub binary_digest: String,
    /// Required unchanged base blobs. This is not a self-contained OCI archive.
    pub base_layers: Vec<ActBaseLayer>,
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(bytes))
}
fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|v| {
        v.len() == 64
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn verify(bytes: &[u8], expected: &str, max: usize) -> Result<(), ActImageError> {
    if bytes.is_empty() || bytes.len() > max {
        return Err(ActImageError("input size outside package bounds"));
    }
    if !valid_digest(expected) || digest(bytes) != expected {
        return Err(ActImageError("pinned input digest mismatch"));
    }
    Ok(())
}
fn parse(bytes: &[u8]) -> Result<Value, ActImageError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| ActImageError("invalid image JSON"))?;
    if !value.is_object() {
        return Err(ActImageError("image JSON must be an object"));
    }
    Ok(value)
}
/// Package a verified static Linux/amd64 Act binary over one pinned runner
/// image manifest (not an image index). Digests include their sha256: prefix.
/// Configuration volumes are refused even when empty; no anonymous image
/// volume may be inherited. This emits only actual content-addressed OCI bytes.
pub fn package_act_image(
    base_manifest: &[u8],
    manifest_pin: &str,
    base_config: &[u8],
    config_pin: &str,
    act_binary: &[u8],
    binary_pin: &str,
    act_version: &str,
) -> Result<ActImagePackage, ActImageError> {
    verify(base_manifest, manifest_pin, MAX_JSON)?;
    verify(base_config, config_pin, MAX_JSON)?;
    verify(act_binary, binary_pin, MAX_BINARY)?;
    if act_version.is_empty()
        || act_version.len() > 32
        || !act_version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(ActImageError("invalid Act version"));
    }
    let manifest = parse(base_manifest)?;
    let mut config = parse(base_config)?;
    if manifest["schemaVersion"] != 2
        || !matches!(
            manifest["mediaType"].as_str(),
            Some(OCI_MANIFEST | "application/vnd.docker.distribution.manifest.v2+json")
        )
    {
        return Err(ActImageError("unsupported base manifest"));
    }
    if manifest["config"]["digest"] != config_pin
        || manifest["config"]["size"].as_u64() != Some(base_config.len() as u64)
        || !matches!(
            manifest["config"]["mediaType"].as_str(),
            Some(OCI_CONFIG | "application/vnd.docker.container.image.v1+json")
        )
    {
        return Err(ActImageError("base config descriptor mismatch"));
    }
    if config["os"] != "linux" || config["architecture"] != "amd64" {
        return Err(ActImageError("Act package requires Linux amd64 base"));
    }
    let image_config = config["config"]
        .as_object_mut()
        .ok_or(ActImageError("missing image configuration"))?;
    if image_config.contains_key("Volumes") {
        return Err(ActImageError("base image declares volumes"));
    }
    if image_config.get("Labels").is_some_and(|v| !v.is_object()) {
        return Err(ActImageError("invalid base image labels"));
    }
    let layers = manifest["layers"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .ok_or(ActImageError("unsupported base layers"))?;
    let diffs = config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or(ActImageError("missing base DiffIDs"))?;
    if config["rootfs"]["type"] != "layers"
        || diffs.len() != layers.len()
        || diffs.iter().any(|v| !v.as_str().is_some_and(valid_digest))
    {
        return Err(ActImageError("base layer and DiffID mismatch"));
    }
    let mut base_layers = Vec::with_capacity(layers.len());
    let mut descriptors = Vec::with_capacity(layers.len() + 1);
    for (index, entry) in layers.iter().enumerate() {
        let media = match entry["mediaType"].as_str() {
            Some(OCI_TAR) => OCI_TAR,
            Some(OCI_GZIP | "application/vnd.docker.image.rootfs.diff.tar.gzip") => OCI_GZIP,
            _ => return Err(ActImageError("unsupported base layer media type")),
        };
        let hash = entry["digest"]
            .as_str()
            .filter(|s| valid_digest(s))
            .ok_or(ActImageError("invalid base layer digest"))?;
        let size = entry["size"]
            .as_u64()
            .filter(|n| *n > 0 && *n <= 16 * 1024 * 1024 * 1024)
            .ok_or(ActImageError("invalid base layer size"))?;
        if entry.get("urls").is_some() || entry.get("data").is_some() {
            return Err(ActImageError("external or embedded base layer refused"));
        }
        if media == OCI_TAR && diffs[index] != hash {
            return Err(ActImageError("uncompressed base layer DiffID mismatch"));
        }
        base_layers.push(ActBaseLayer {
            media_type: media.into(),
            digest: hash.into(),
            size,
        });
        descriptors.push(json!({"mediaType":media,"digest":hash,"size":size}));
    }
    let layer = act_layer(act_binary);
    let layer_digest = digest(&layer);
    config["rootfs"]["diff_ids"]
        .as_array_mut()
        .unwrap()
        .push(json!(layer_digest));
    config["created"] = json!(EPOCH);
    let image_config = config["config"].as_object_mut().unwrap();
    image_config.insert("Entrypoint".into(), json!(["/usr/local/bin/act"]));
    image_config.insert("Cmd".into(), json!([]));
    image_config.insert("WorkingDir".into(), json!("/bosn-control"));
    let labels = image_config
        .entry("Labels")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .unwrap();
    labels.insert("com.zackees.bosn.act.packaging-schema".into(), json!("1"));
    labels.insert("com.zackees.bosn.act.version".into(), json!(act_version));
    labels.insert(
        "com.zackees.bosn.act.binary-sha256".into(),
        json!(binary_pin),
    );
    labels.insert(
        "com.zackees.bosn.act.base-manifest".into(),
        json!(manifest_pin),
    );
    let history = config
        .as_object_mut()
        .unwrap()
        .entry("history")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or(ActImageError("invalid base history"))?;
    history.push(json!({"created":EPOCH,"created_by":"bosn Act OCI packaging v1"}));
    let config = canonical_json(&config)?;
    let config_digest = digest(&config);
    descriptors.push(json!({"mediaType":OCI_TAR,"digest":layer_digest,"size":layer.len()}));
    let manifest = canonical_json(
        &json!({"schemaVersion":2,"mediaType":OCI_MANIFEST,"config":{"mediaType":OCI_CONFIG,"digest":config_digest,"size":config.len()},"layers":descriptors,"annotations":{"com.zackees.bosn.act.packaging-schema":"1","com.zackees.bosn.act.version":act_version,"com.zackees.bosn.act.binary-sha256":binary_pin,"com.zackees.bosn.act.base-manifest":manifest_pin}}),
    )?;
    Ok(ActImagePackage {
        runner_manifest: base_manifest.to_vec(),
        runner_config: base_config.to_vec(),
        runner_manifest_digest: manifest_pin.into(),
        runner_config_digest: config_pin.into(),
        manifest_digest: digest(&manifest),
        config_digest,
        binary_digest: binary_pin.into(),
        manifest,
        config,
        layer,
        base_layers,
    })
}
// Sort recursively even if another dependency enables serde_json/preserve_order.
fn canonical_json(value: &Value) -> Result<Vec<u8>, ActImageError> {
    fn write(value: &Value, out: &mut Vec<u8>) -> Result<(), ActImageError> {
        match value {
            Value::Object(map) => {
                out.push(b'{');
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort_unstable();
                for (n, key) in keys.into_iter().enumerate() {
                    if n > 0 {
                        out.push(b',');
                    }
                    out.extend(
                        serde_json::to_vec(key)
                            .map_err(|_| ActImageError("JSON serialization failed"))?,
                    );
                    out.push(b':');
                    write(&map[key], out)?;
                }
                out.push(b'}');
            }
            Value::Array(values) => {
                out.push(b'[');
                for (n, value) in values.iter().enumerate() {
                    if n > 0 {
                        out.push(b',');
                    }
                    write(value, out)?;
                }
                out.push(b']');
            }
            _ => out.extend(
                serde_json::to_vec(value)
                    .map_err(|_| ActImageError("JSON serialization failed"))?,
            ),
        }
        Ok(())
    }
    let mut out = Vec::new();
    write(value, &mut out)?;
    Ok(out)
}
// Only fixed paths/types are emitted; this is not a general tar writer/parser.
fn act_layer(binary: &[u8]) -> Vec<u8> {
    fn octal(field: &mut [u8], value: usize) {
        let s = format!("{:0width$o}", value, width = field.len() - 1);
        field[..s.len()].copy_from_slice(s.as_bytes());
    }
    fn entry(out: &mut Vec<u8>, name: &str, data: &[u8], kind: u8) {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        octal(&mut header[100..108], 0o755);
        octal(&mut header[108..116], 0);
        octal(&mut header[116..124], 0);
        octal(&mut header[124..136], data.len());
        octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        octal(&mut header[329..337], 0);
        octal(&mut header[337..345], 0);
        let sum: usize = header.iter().map(|b| usize::from(*b)).sum();
        let checksum = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(512), 0);
    }
    let mut out = Vec::with_capacity(binary.len() + 4096);
    for path in ["usr/", "usr/local/", "usr/local/bin/", "bosn-control/"] {
        entry(&mut out, path, &[], b'5');
    }
    // Opaque whiteout ensures no inherited .actrc survives in control cwd.
    entry(&mut out, "bosn-control/.wh..wh..opq", &[], b'0');
    entry(&mut out, "usr/local/bin/act", binary, b'0');
    out.extend_from_slice(&[0u8; 1024]);
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn fixture() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let config = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{"Env":["PATH=/usr/bin"],"Cmd":["/bin/bash"]},"rootfs":{"type":"layers","diff_ids":[format!("sha256:{}","1".repeat(64))]},"history":[{"created":"2026-01-01T00:00:00Z"}]})).unwrap();
        let manifest = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{"mediaType":"application/vnd.docker.container.image.v1+json","digest":digest(&config),"size":config.len()},"layers":[{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","digest":format!("sha256:{}","2".repeat(64)),"size":77}]})).unwrap();
        (
            manifest,
            config,
            b"verified synthetic static binary".to_vec(),
        )
    }
    fn package(m: &[u8], c: &[u8], b: &[u8]) -> Result<ActImagePackage, ActImageError> {
        package_act_image(m, &digest(m), c, &digest(c), b, &digest(b), "0.2.88")
    }
    #[test]
    fn deterministic_package_has_distinct_content_identities_and_oci_descriptors() {
        let (m, c, b) = fixture();
        let p = package(&m, &c, &b).unwrap();
        assert_eq!(p, package(&m, &c, &b).unwrap());
        assert_eq!(p.manifest_digest, digest(&p.manifest));
        assert_eq!(p.config_digest, digest(&p.config));
        assert_eq!(p.binary_digest, digest(&b));
        assert_ne!(p.manifest_digest, p.config_digest);
        assert_ne!(p.config_digest, p.binary_digest);
        let manifest: Value = serde_json::from_slice(&p.manifest).unwrap();
        assert_eq!(manifest["config"]["digest"], p.config_digest);
        assert_eq!(manifest["layers"][1]["digest"], digest(&p.layer));
        assert_eq!(
            manifest["layers"][1]["mediaType"],
            "application/vnd.oci.image.layer.v1.tar"
        );
        let config: Value = serde_json::from_slice(&p.config).unwrap();
        assert_eq!(
            config["config"]["Entrypoint"],
            json!(["/usr/local/bin/act"])
        );
        assert_eq!(config["rootfs"]["diff_ids"][1], digest(&p.layer));
        assert_eq!(p.base_layers.len(), 1);
    }
    #[test]
    fn every_input_digest_and_config_descriptor_size_is_enforced() {
        let (m, c, b) = fixture();
        let bad = format!("sha256:{}", "0".repeat(64));
        assert!(package_act_image(&m, &bad, &c, &digest(&c), &b, &digest(&b), "0.2.88").is_err());
        assert!(package_act_image(&m, &digest(&m), &c, &bad, &b, &digest(&b), "0.2.88").is_err());
        assert!(package_act_image(&m, &digest(&m), &c, &digest(&c), &b, &bad, "0.2.88").is_err());
        let mut changed: Value = serde_json::from_slice(&m).unwrap();
        changed["config"]["size"] = json!(1);
        assert!(package(&serde_json::to_vec(&changed).unwrap(), &c, &b).is_err());
    }
    #[test]
    fn unsupported_platform_and_external_layer_descriptors_fail_closed() {
        let (m, c, b) = fixture();
        for (field, value) in [("architecture", "arm64"), ("os", "windows")] {
            let mut config: Value = serde_json::from_slice(&c).unwrap();
            config[field] = json!(value);
            let config = serde_json::to_vec(&config).unwrap();
            let mut manifest: Value = serde_json::from_slice(&m).unwrap();
            manifest["config"]["digest"] = json!(digest(&config));
            manifest["config"]["size"] = json!(config.len());
            assert!(package(&serde_json::to_vec(&manifest).unwrap(), &config, &b).is_err());
        }
        for (field, value) in [
            ("mediaType", json!("unknown")),
            ("urls", json!(["https://untrusted.example/blob"])),
            ("digest", json!("sha256:bad")),
            ("size", json!(0)),
        ] {
            let mut manifest: Value = serde_json::from_slice(&m).unwrap();
            manifest["layers"][0][field] = value;
            assert!(package(&serde_json::to_vec(&manifest).unwrap(), &c, &b).is_err());
        }
        assert!(package_act_image(&m, &digest(&m), &c, &digest(&c), &b, &digest(&b), "").is_err());
        assert!(verify(b"fixture", &digest(b"fixture"), 1).is_err());
    }
    #[test]
    fn inherited_volumes_and_incompatible_rootfs_are_refused() {
        let (m, c, b) = fixture();
        for change in [json!({"/data":{}}), json!({})] {
            let mut config: Value = serde_json::from_slice(&c).unwrap();
            config["config"]["Volumes"] = change;
            let config = serde_json::to_vec(&config).unwrap();
            let mut manifest: Value = serde_json::from_slice(&m).unwrap();
            manifest["config"]["digest"] = json!(digest(&config));
            manifest["config"]["size"] = json!(config.len());
            assert!(package(&serde_json::to_vec(&manifest).unwrap(), &config, &b).is_err());
        }
        let mut config: Value = serde_json::from_slice(&c).unwrap();
        config["rootfs"]["diff_ids"] = json!([]);
        let config = serde_json::to_vec(&config).unwrap();
        let mut manifest: Value = serde_json::from_slice(&m).unwrap();
        manifest["config"]["digest"] = json!(digest(&config));
        manifest["config"]["size"] = json!(config.len());
        assert!(package(&serde_json::to_vec(&manifest).unwrap(), &config, &b).is_err());
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;
    #[test]
    fn canonical_json_sorts_every_object_level() {
        let value: Value =
            serde_json::from_str(r#"{"z":{"y":1,"a":2},"a":[{"x":4,"b":3}]}"#).unwrap();
        assert_eq!(
            canonical_json(&value).unwrap(),
            br#"{"a":[{"b":3,"x":4}],"z":{"a":2,"y":1}}"#
        );
    }
    #[cfg(unix)]
    #[test]
    fn standard_tar_reads_exact_binary_from_generated_ustar() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let binary = b"verified fixture bytes\0not an executable";
        let mut child = Command::new("tar")
            .args(["-xOf", "-", "usr/local/bin/act"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&act_layer(binary))
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, binary);
    }
    #[test]
    fn fixed_layer_contains_no_wall_clock_or_host_identity() {
        let layer = act_layer(b"fixture");
        assert_eq!(&layer[..4], b"usr/");
        assert_eq!(&layer[108..116], b"0000000\0");
        assert_eq!(&layer[116..124], b"0000000\0");
        assert_eq!(&layer[136..148], b"00000000000\0");
        assert_eq!(&layer[257..265], b"ustar\x0000");
        assert!(layer[layer.len() - 1024..].iter().all(|b| *b == 0));
    }
}
