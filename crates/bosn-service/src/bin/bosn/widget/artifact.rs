//! Install the matching, source-bound desktop release through neutral facades.

use std::{io, path::PathBuf, time::Duration};

use kernal_api::{archive, hash::Sha256Hasher, http, platform::fs};
use serde::Deserialize;

const TARGET: &str = "x86_64-unknown-linux-gnu";
const MAX_BINARY: u64 = 256 << 20;
const API: &str = "https://api.github.com/repos/zackees/bosn";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Deserialize)]
struct Commit {
    sha: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    source_sha: String,
    target: String,
    binary_sha256: String,
}

pub fn installed_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local/share/bosn/widget")
            .join(env!("CARGO_PKG_VERSION"))
            .join("bosn-widget"),
    )
}

fn asset<'a>(release: &'a Release, name: &str) -> io::Result<&'a Asset> {
    let mut matches = release.assets.iter().filter(|asset| asset.name == name);
    let found = matches
        .next()
        .ok_or_else(|| invalid("matching desktop release artifact is absent"))?;
    if matches.next().is_some() || found.size == 0 || found.size > MAX_BINARY {
        return Err(invalid("ambiguous or oversized desktop release artifact"));
    }
    if !found
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .is_some_and(|digest| hex(digest, 64))
    {
        return Err(invalid(
            "desktop release asset has no authoritative SHA256 digest",
        ));
    }
    Ok(found)
}

async fn download(url: &str, ceiling: u64) -> io::Result<Vec<u8>> {
    let client = http::Client::new(http::Limits {
        max_redirects: 5,
        max_body_bytes: ceiling,
        connect_timeout: Duration::from_secs(10),
        total_timeout: Duration::from_secs(120),
        read_timeout: Duration::from_secs(30),
        ..http::Limits::default()
    })?;
    let request = http::Request {
        headers: &[
            ("User-Agent", "bosn-widget-installer"),
            ("Accept", "application/vnd.github+json"),
        ],
        ..http::Request::get(url)
    };
    let response = client.execute(request).await?;
    if response.status() != 200 {
        return Err(invalid(format!(
            "desktop release request returned HTTP {}",
            response.status()
        )));
    }
    response.into_bytes().await
}

fn validate_payload(
    manifest: &Manifest,
    binary: &[u8],
    version: &str,
    source: &str,
) -> io::Result<()> {
    if manifest.schema != 1
        || manifest.version != version
        || manifest.source_sha != source
        || manifest.target != TARGET
        || !hex(source, 40)
    {
        return Err(invalid("desktop release source/version/target mismatch"));
    }
    if binary.len() < 64
        || binary.len() as u64 > MAX_BINARY
        || binary[..6] != *b"\x7fELF\x02\x01"
        || binary[18..20] != [62, 0]
    {
        return Err(invalid(
            "desktop artifact is not the expected Linux x86_64 ELF",
        ));
    }
    if !hex(&manifest.binary_sha256, 64)
        || Sha256Hasher::digest(binary).to_hex() != manifest.binary_sha256
    {
        return Err(invalid("desktop executable checksum mismatch"));
    }
    Ok(())
}

fn unpack(
    bytes: &[u8],
    parent: &std::path::Path,
    version: &str,
    source: &str,
) -> io::Result<fs::TemporaryDirectory> {
    let stage = fs::TemporaryDirectory::in_directory(parent, "widget-install-")?;
    let archive_path = stage.path().join("download.tar.gz");
    std::fs::write(&archive_path, bytes)?;
    let destination = stage.path().join("payload");
    archive::extract(
        &archive_path,
        &destination,
        archive::ArchiveFormat::TarGzip,
        archive::ExtractionLimits {
            max_input_bytes: MAX_BINARY,
            max_output_bytes: MAX_BINARY + 4096,
            max_entry_bytes: MAX_BINARY,
            max_entries: 2,
            max_metadata_bytes: 4096,
            max_path_bytes: 64,
            max_link_steps: 4,
            ..archive::ExtractionLimits::default()
        },
    )?;
    let entries = std::fs::read_dir(&destination)?.collect::<io::Result<Vec<_>>>()?;
    if entries.len() != 2
        || entries.iter().any(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some("bosn-widget" | "manifest.json")
            ) || !entry.file_type().is_ok_and(|kind| kind.is_file())
        })
    {
        return Err(invalid(
            "desktop archive contains unexpected or nonregular entries",
        ));
    }
    let metadata = destination.join("manifest.json");
    if std::fs::metadata(&metadata)?.len() > 4096 {
        return Err(invalid("desktop manifest exceeds its limit"));
    }
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(metadata)?)
        .map_err(|error| invalid(error.to_string()))?;
    let binary = destination.join("bosn-widget");
    validate_payload(&manifest, &std::fs::read(&binary)?, version, source)?;
    fs::make_owner_executable(&binary)?;
    Ok(stage)
}

fn validate_download(bytes: &[u8], expected: &Asset) -> io::Result<()> {
    if bytes.len() as u64 != expected.size
        || expected.digest.as_deref()
            != Some(&format!("sha256:{}", Sha256Hasher::digest(bytes).to_hex()))
    {
        return Err(invalid("desktop release archive digest/size mismatch"));
    }
    Ok(())
}

async fn install_release(destination: PathBuf) -> io::Result<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    let tag = format!("v{version}");
    let release: Release =
        serde_json::from_slice(&download(&format!("{API}/releases/tags/{tag}"), 1 << 20).await?)
            .map_err(|error| invalid(error.to_string()))?;
    if release.tag_name != tag || release.draft || release.prerelease {
        return Err(invalid("matching stable desktop release is unavailable"));
    }
    let name = format!("bosn-widget-{tag}-{TARGET}.tar.gz");
    let expected = asset(&release, &name)?;
    let commit: Commit =
        serde_json::from_slice(&download(&format!("{API}/commits/{tag}"), 1 << 20).await?)
            .map_err(|error| invalid(error.to_string()))?;
    if !hex(&commit.sha, 40) {
        return Err(invalid("invalid desktop release commit identity"));
    }
    let bytes = download(
        &format!("https://github.com/zackees/bosn/releases/download/{tag}/{name}"),
        MAX_BINARY,
    )
    .await?;
    validate_download(&bytes, expected)?;
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("widget installation path has no parent"))?;
    fs::create_dir_all_private(parent)?;
    fs::ensure_dir_private(parent)?;
    let stage = unpack(&bytes, parent, version, &commit.sha)?;
    fs::replacement::atomic_replace(&stage.path().join("payload/bosn-widget"), &destination)?;
    Ok(destination)
}

pub fn install() -> io::Result<PathBuf> {
    let target = kernal_api::platform::host::process_target();
    if target.os != "linux" || target.architecture != "x86_64" {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "published desktop installer supports Linux x86_64",
        ));
    }
    let destination = installed_path().ok_or_else(|| invalid("HOME is not set"))?;
    kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()?
        .run(install_release(destination))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCHIVE: &[u8] = include_bytes!("fixtures/widget.tar.gz");

    #[test]
    fn installer_refuses_missing_digest_duplicate_and_oversized_assets() {
        let make = |digest: Option<String>, size| Asset {
            name: "widget.tar.gz".into(),
            size,
            digest,
        };
        let mut release = Release {
            tag_name: "v0.1.12".into(),
            draft: false,
            prerelease: false,
            assets: vec![make(None, 1)],
        };
        assert!(asset(&release, "widget.tar.gz").is_err());
        release.assets = vec![make(Some(format!("sha256:{}", "a".repeat(64))), 1)];
        assert!(asset(&release, "widget.tar.gz").is_ok());
        release
            .assets
            .push(make(Some(format!("sha256:{}", "a".repeat(64))), 1));
        assert!(asset(&release, "widget.tar.gz").is_err());
        release.assets = vec![make(
            Some(format!("sha256:{}", "a".repeat(64))),
            MAX_BINARY + 1,
        )];
        assert!(asset(&release, "widget.tar.gz").is_err());
        assert!(asset(&release, "other.tar.gz").is_err());
    }

    #[test]
    fn installer_rejects_download_tampering_and_truncation() {
        let expected = Asset {
            name: "widget.tar.gz".into(),
            size: ARCHIVE.len() as u64,
            digest: Some(format!("sha256:{}", Sha256Hasher::digest(ARCHIVE).to_hex())),
        };
        validate_download(ARCHIVE, &expected).unwrap();
        assert!(validate_download(&ARCHIVE[..ARCHIVE.len() - 1], &expected).is_err());
        let mut tampered = ARCHIVE.to_vec();
        tampered[0] ^= 1;
        assert!(validate_download(&tampered, &expected).is_err());
    }

    #[test]
    fn installer_verifies_version_source_architecture_and_binary_before_publication() {
        let root = fs::TemporaryDirectory::new().unwrap();
        let stage = unpack(ARCHIVE, root.path(), "0.1.12", &"a".repeat(40)).unwrap();
        assert!(stage.path().join("payload/bosn-widget").is_file());
        assert!(unpack(ARCHIVE, root.path(), "0.1.13", &"a".repeat(40)).is_err());
        assert!(unpack(ARCHIVE, root.path(), "0.1.12", &"b".repeat(40)).is_err());
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(stage.path().join("payload/manifest.json")).unwrap(),
        )
        .unwrap();
        let mut binary = std::fs::read(stage.path().join("payload/bosn-widget")).unwrap();
        binary[63] ^= 1;
        assert!(validate_payload(&manifest, &binary, "0.1.12", &"a".repeat(40)).is_err());
        binary[18] = 183;
        assert!(validate_payload(&manifest, &binary, "0.1.12", &"a".repeat(40)).is_err());
    }
}
