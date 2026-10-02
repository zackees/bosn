//! Fetching the pinned artifacts from their publishers, bounded and cached.

use super::*;

pub(super) fn proxy_environment(
    values: impl IntoIterator<Item = (OsString, OsString)>,
) -> io::Result<()> {
    if values.into_iter().any(|(key, value)| {
        !value.is_empty()
            && ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"]
                .iter()
                .any(|name| key.to_string_lossy().eq_ignore_ascii_case(name))
    }) {
        return Err(refused(
            "Act acquisition unavailable with ambient proxy configuration",
        ));
    }
    Ok(())
}
pub(super) struct Budget {
    pub(super) deadline: Instant,
    pub(super) cancellation: CancellationToken,
}
impl Budget {
    pub(super) fn remaining(&self) -> io::Result<Duration> {
        if self.cancellation.is_cancelled() {
            return Err(refused("Act acquisition cancelled"));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(refused("Act acquisition deadline exceeded"));
        }
        Ok(remaining)
    }
}
pub(super) type Pending<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;
pub(super) trait Body: Send {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> Pending<'a, usize>;
}
impl Body for http::Response {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> Pending<'a, usize> {
        Box::pin(self.read(buffer))
    }
}
pub(super) struct Response {
    pub(super) status: u16,
    pub(super) location: Option<Vec<u8>>,
    pub(super) length: Option<Vec<u8>>,
    pub(super) encoding: Option<Vec<u8>>,
    pub(super) body: Box<dyn Body>,
}
pub(super) trait Transport: Sync {
    fn request<'a>(
        &'a self,
        url: &'a str,
        auth: Option<&'a str>,
        ceiling: u64,
        remaining: Duration,
    ) -> Pending<'a, Response>;
}
pub(super) struct PublisherHttp;
impl Transport for PublisherHttp {
    fn request<'a>(
        &'a self,
        url: &'a str,
        auth: Option<&'a str>,
        ceiling: u64,
        remaining: Duration,
    ) -> Pending<'a, Response> {
        Box::pin(async move {
            // Recheck before every request; do not alter process environment.
            proxy_environment(std::env::vars_os())?;
            let client = http::Client::new(http::Limits {
                max_redirects: 0,
                max_request_bytes: 0,
                max_url_bytes: 8192,
                max_body_bytes: ceiling,
                max_header_bytes: 16 << 10,
                max_header_count: 64,
                connect_timeout: remaining.min(Duration::from_secs(10)),
                total_timeout: remaining.min(Duration::from_secs(120)),
                read_timeout: remaining.min(Duration::from_secs(15)),
            })?;
            let mut headers = vec![
                (
                    "Accept",
                    "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json",
                ),
                ("Accept-Encoding", "identity"),
            ];
            if let Some(auth) = auth {
                headers.push(("Authorization", auth));
            }
            let response = client
                .execute(http::Request {
                    method: http::Method::Get,
                    url,
                    headers: &headers,
                    body: &[],
                })
                .await?;
            Ok(Response {
                status: response.status(),
                location: response.header("location").map(<[u8]>::to_vec),
                length: response.header("content-length").map(<[u8]>::to_vec),
                encoding: response.header("content-encoding").map(<[u8]>::to_vec),
                body: Box::new(response),
            })
        })
    }
}
pub(super) fn publisher_host(url: &str) -> io::Result<&str> {
    if url.len() > 8192 || url.chars().any(|c| c.is_control() || c == '\\') || url.contains('#') {
        return Err(refused("artifact publisher URL is invalid"));
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| refused("artifact publisher requires HTTPS"))?;
    let host = rest.split('/').next().unwrap_or("");
    if !matches!(
        host,
        "ghcr.io"
            | "github.com"
            | "release-assets.githubusercontent.com"
            | "pkg-containers.githubusercontent.com"
    ) {
        return Err(refused("artifact redirect publisher is not allowed"));
    }
    Ok(host)
}
pub(super) async fn open_response(
    transport: &impl Transport,
    initial: &str,
    token: Option<&str>,
    ceiling: u64,
    budget: &Budget,
) -> io::Result<Response> {
    let mut url = initial.to_owned();
    let mut token_allowed = true;
    for hop in 0..=3 {
        let host = publisher_host(&url)?;
        let auth = (token_allowed && host == "ghcr.io")
            .then_some(token)
            .flatten();
        let remaining = budget.remaining()?;
        let response = async_engine::cancellable(
            &budget.cancellation,
            async_engine::timeout(remaining, transport.request(&url, auth, ceiling, remaining)),
        )
        .await
        .map_err(|_| refused("Act acquisition cancelled"))?
        .map_err(|_| refused("artifact HTTP deadline exceeded"))??;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            if hop == 3 {
                return Err(refused("artifact redirect ceiling exceeded"));
            }
            let next = std::str::from_utf8(
                response
                    .location
                    .as_deref()
                    .ok_or_else(|| refused("artifact redirect has no location"))?,
            )
            .map_err(|_| refused("artifact redirect location is invalid"))?;
            let next_host = publisher_host(next)?;
            // Even a later redirect back to ghcr cannot regain authorization.
            if next_host != host {
                token_allowed = false;
            }
            url = next.into();
            continue;
        }
        if response.status != 200 {
            return Err(refused("artifact publisher returned unsuccessful status"));
        }
        if response
            .encoding
            .as_deref()
            .is_some_and(|v| v != b"identity")
        {
            return Err(refused("artifact HTTP content encoding is not identity"));
        }
        if let Some(length) = response.length.as_ref() {
            let n = std::str::from_utf8(length)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| refused("artifact HTTP length is invalid"))?;
            if n > ceiling {
                return Err(refused("artifact HTTP advertised size exceeds ceiling"));
            }
        }
        return Ok(response);
    }
    Err(refused("artifact redirect ceiling exceeded"))
}
pub(super) async fn copy_body(
    mut response: Response,
    output: &mut impl Write,
    expected_size: Option<u64>,
    ceiling: u64,
    expected_digest: Option<&str>,
    budget: &Budget,
) -> io::Result<u64> {
    let advertised = response
        .length
        .as_deref()
        .map(|v| {
            std::str::from_utf8(v)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| refused("artifact HTTP length is invalid"))
        })
        .transpose()?;
    let mut bytes = 0u64;
    let mut hasher = Sha256Hasher::new();
    let mut buffer = vec![0u8; 65536];
    loop {
        let count = async_engine::cancellable(
            &budget.cancellation,
            async_engine::timeout(budget.remaining()?, response.body.read(&mut buffer)),
        )
        .await
        .map_err(|_| refused("Act acquisition cancelled"))?
        .map_err(|_| refused("artifact body deadline exceeded"))??;
        if count > buffer.len() {
            return Err(refused("artifact transport returned invalid chunk size"));
        }
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .filter(|n| *n <= ceiling)
            .ok_or_else(|| refused("artifact body exceeds ceiling"))?;
        if expected_size.is_some_and(|n| bytes > n) {
            return Err(refused("artifact body exceeds descriptor size"));
        }
        hasher.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    budget.remaining()?;
    if advertised.is_some_and(|n| bytes != n) {
        return Err(refused("artifact HTTP body length mismatch"));
    }
    if expected_size.is_some_and(|n| bytes != n) {
        return Err(refused("artifact body descriptor size mismatch"));
    }
    if expected_digest.is_some_and(|d| format!("sha256:{}", hasher.finalize()) != d) {
        return Err(refused("artifact body digest mismatch"));
    }
    Ok(bytes)
}
pub(super) struct ArtifactSpec<'a> {
    pub(super) pin: &'a str,
    pub(super) size: Option<u64>,
    pub(super) ceiling: u64,
}
pub(super) async fn fetch_cached(
    transport: &impl Transport,
    root: &Path,
    owner: u32,
    url: &str,
    token: Option<&str>,
    artifact: ArtifactSpec<'_>,
    budget: &Budget,
) -> io::Result<PathBuf> {
    let ArtifactSpec { pin, size, ceiling } = artifact;
    let path = root.join(digest_name(pin)?);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            verified_file(&path, owner, pin, size, ceiling)?;
            budget.remaining()?;
            return Ok(path);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if cache_usage(root, owner)?
        .checked_add(size.unwrap_or(ceiling))
        .is_none_or(|n| n > AGGREGATE)
    {
        return Err(refused("artifact cache has insufficient bounded space"));
    }
    let staging = fs::TemporaryDirectory::in_directory(root, "stage-")?;
    let pending = staging.path().join("download");
    let mut file = fs::create_private_file(&pending)?;
    let response = open_response(transport, url, token, ceiling, budget).await?;
    copy_body(response, &mut file, size, ceiling, Some(pin), budget).await?;
    file.sync_all()?;
    drop(file);
    budget.remaining()?;
    // Atomic create-only publication: hard_link never replaces an existing
    // name, even if a noncooperating process races the advisory lock.
    std::fs::hard_link(&pending, &path)?;
    std::fs::File::open(root)?.sync_all()?;
    staging.close()?;
    Ok(path)
}
pub(super) struct Pins<'a> {
    pub(super) manifest: &'a str,
    pub(super) config: &'a str,
    pub(super) archive: &'a str,
    pub(super) binary: &'a str,
    pub(super) binary_size: u64,
}
pub(super) const PINS: Pins<'static> = Pins {
    manifest: RUNNER,
    config: CONFIG,
    archive: ACT_ARCHIVE,
    binary: ACT_BINARY,
    binary_size: 21098680,
};

/// Acquire the fixed graph into a server-selected private cache. Do not expose
/// the cache path or private transport seam as client-supplied authority.
pub async fn acquire_pinned_act_artifacts(
    root: &Path,
    options: ActArtifactAcquisitionOptions,
    cancellation: &CancellationToken,
) -> io::Result<VerifiedActArtifacts> {
    if !cfg!(target_arch = "x86_64") {
        return Err(refused("Act acquisition supports native Linux x86_64 only"));
    }
    proxy_environment(std::env::vars_os())?;
    acquire(&PublisherHttp, root, options, cancellation, &PINS).await
}
pub(super) async fn acquire(
    transport: &impl Transport,
    root: &Path,
    options: ActArtifactAcquisitionOptions,
    cancellation: &CancellationToken,
    pins: &Pins<'_>,
) -> io::Result<VerifiedActArtifacts> {
    if options.deadline.is_zero() || options.deadline > Duration::from_secs(600) {
        return Err(refused("Act acquisition deadline is outside bounds"));
    }
    let budget = Budget {
        deadline: Instant::now()
            .checked_add(options.deadline)
            .ok_or_else(|| refused("Act acquisition deadline is invalid"))?,
        cancellation: cancellation.clone(),
    };
    budget.remaining()?;
    let owner = private_cache(root)?;
    let lock_path = root.join(".acquire.lock");
    match std::fs::symlink_metadata(&lock_path) {
        Ok(_) => {
            open_private(&lock_path, owner)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let lock = cache_lock(&lock_path, owner)?;
    cache_usage(root, owner)?;
    // Anonymous, public pull capability. Never consult Docker config, netrc,
    // GitHub tokens or caller-controlled challenge realms/scopes.
    let manifest_cached = root.join(digest_name(pins.manifest)?);
    let needs_registry = match std::fs::symlink_metadata(&manifest_cached) {
        Ok(_) => {
            let bytes = verified_file(&manifest_cached, owner, pins.manifest, None, JSON)?;
            let value: Value = serde_json::from_slice(&bytes)?;
            let layers = value["layers"]
                .as_array()
                .filter(|v| v.len() <= 32)
                .ok_or_else(|| refused("cached runner layer graph is invalid"))?;
            let mut missing = false;
            for pin in std::iter::once(pins.config)
                .chain(layers.iter().map(|v| v["digest"].as_str().unwrap_or("")))
            {
                let path = root.join(digest_name(pin)?);
                match std::fs::symlink_metadata(path) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => missing = true,
                    Err(e) => return Err(e),
                }
            }
            missing
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(e) => return Err(e),
    };
    let bearer = if needs_registry {
        let response = open_response(transport, TOKEN_URL, None, TOKEN, &budget).await?;
        let mut token = Vec::new();
        copy_body(response, &mut token, None, TOKEN, None, &budget).await?;
        let token: Value = serde_json::from_slice(&token)?;
        let token = token
            .get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && t.len() <= 8192 && t.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or_else(|| refused("anonymous registry token is invalid"))?;
        format!("Bearer {token}")
    } else {
        String::new()
    };
    let manifest_path = fetch_cached(
        transport,
        root,
        owner,
        &format!("{MANIFEST_URL}{}", pins.manifest),
        Some(&bearer),
        ArtifactSpec {
            pin: pins.manifest,
            size: None,
            ceiling: JSON,
        },
        &budget,
    )
    .await?;
    let manifest = verified_file(&manifest_path, owner, pins.manifest, None, JSON)?;
    let value: Value = serde_json::from_slice(&manifest)?;
    let config_size = value["config"]["size"]
        .as_u64()
        .filter(|s| *s > 0 && *s <= JSON)
        .ok_or_else(|| refused("runner config size is invalid"))?;
    if value["config"]["digest"] != pins.config {
        return Err(refused("runner manifest config pin mismatch"));
    }
    let config_path = fetch_cached(
        transport,
        root,
        owner,
        &format!("{BLOB_URL}{}", pins.config),
        Some(&bearer),
        ArtifactSpec {
            pin: pins.config,
            size: Some(config_size),
            ceiling: JSON,
        },
        &budget,
    )
    .await?;
    let config = verified_file(&config_path, owner, pins.config, Some(config_size), JSON)?;
    let binary_path = root.join(digest_name(pins.binary)?);
    match std::fs::symlink_metadata(&binary_path) {
        Ok(_) => {
            verified_file(
                &binary_path,
                owner,
                pins.binary,
                Some(pins.binary_size),
                BINARY,
            )?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let archive = fetch_cached(
                transport,
                root,
                owner,
                ACT_URL,
                None,
                ArtifactSpec {
                    pin: pins.archive,
                    size: None,
                    ceiling: RELEASE,
                },
                &budget,
            )
            .await?;
            if cache_usage(root, owner)?
                .checked_add(BINARY)
                .is_none_or(|n| n > AGGREGATE)
            {
                return Err(refused("artifact extraction exceeds cache disk ceiling"));
            }
            let staging = fs::TemporaryDirectory::in_directory(root, "stage-")?;
            let destination = staging.path().join("act");
            // The worker owns staging until it finishes. A dropped caller cannot
            // delete files under a running extractor or publish its output.
            let (staging, result) = async_engine::launch_blocking(move || {
                let result = kernal_api::archive::extract_member(
                    &archive,
                    "act",
                    &destination,
                    kernal_api::archive::ArchiveFormat::TarGzip,
                    kernal_api::archive::ExtractionLimits {
                        max_input_bytes: RELEASE,
                        max_output_bytes: BINARY,
                        max_entry_bytes: BINARY,
                        max_entries: 16,
                        max_metadata_bytes: 64 << 10,
                        max_path_bytes: 256,
                        max_link_steps: 16,
                        ..Default::default()
                    },
                );
                (staging, result)
            })
            .await
            .map_err(|_| refused("Act extraction worker failed"))?;
            result?;
            budget.remaining()?;
            // The archive backend preserves mode, so enforce private cache mode
            // before applying the regular-file identity/hash boundary.
            use std::os::unix::fs::PermissionsExt;
            let extracted = staging.path().join("act");
            std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o600))?;
            verified_file(
                &extracted,
                owner,
                pins.binary,
                Some(pins.binary_size),
                BINARY,
            )?;
            std::fs::hard_link(&extracted, &binary_path)?;
            std::fs::File::open(root)?.sync_all()?;
            staging.close()?;
        }
        Err(e) => return Err(e),
    }
    let binary = verified_file(
        &binary_path,
        owner,
        pins.binary,
        Some(pins.binary_size),
        BINARY,
    )?;
    let package = package_act_image(
        &manifest,
        pins.manifest,
        &config,
        pins.config,
        &binary,
        pins.binary,
        "0.2.88",
    )
    .map_err(|_| refused("verified Act image graph is invalid"))?;
    drop(binary);
    if package.base_layers.len() > 32 {
        return Err(refused("runner layer count exceeds ceiling"));
    }
    let mut descriptors = BTreeMap::new();
    let mut total = 0u64;
    for layer in &package.base_layers {
        digest_name(&layer.digest)?;
        if let Some(previous) = descriptors.insert(layer.digest.clone(), layer.size) {
            if previous != layer.size {
                return Err(refused("runner duplicate layer descriptors conflict"));
            }
        } else {
            total = total
                .checked_add(layer.size)
                .filter(|n| *n <= AGGREGATE)
                .ok_or_else(|| refused("runner aggregate layer bytes exceed ceiling"))?;
        }
    }
    for (pin, size) in &descriptors {
        fetch_cached(
            transport,
            root,
            owner,
            &format!("{BLOB_URL}{pin}"),
            Some(&bearer),
            ArtifactSpec {
                pin,
                size: Some(*size),
                ceiling: *size,
            },
            &budget,
        )
        .await?;
    }
    let mut blobs = BTreeMap::new();
    for (pin, size) in descriptors {
        budget.remaining()?;
        let bytes = verified_file(
            &root.join(digest_name(&pin)?),
            owner,
            &pin,
            Some(size),
            size,
        )?;
        blobs.insert(pin, bytes);
    }
    budget.remaining()?;
    drop(lock);
    let receipt = ActArtifactAcquisitionReceipt {
        schema_version: 1,
        act_version: "0.2.88".into(),
        runner_manifest_digest: pins.manifest.into(),
        runner_config_digest: pins.config.into(),
        act_archive_digest: pins.archive.into(),
        act_binary_digest: pins.binary.into(),
        unique_layer_bytes: total,
        unique_layer_count: blobs.len(),
    };
    Ok(VerifiedActArtifacts {
        package,
        blobs,
        receipt,
    })
}
