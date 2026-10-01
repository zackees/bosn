//! Daemon-only execution inside an already registered private Act engine.
//! Public RPC must never accept these trusted observations or image bytes.
use crate::{
    RegistryActor,
    act_archive::{ActArchiveBlob, write_act_oci_archive},
    act_image::ActImagePackage,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};
use bosn_engine::{DockerEngine, RunOptions};
use bosn_registry::act::{ActEngineIntent, ActEngineObservation, ActRunOutcome};
use kernal_api::{async_engine, hash::Sha256Hasher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const INPUTS: &str = "/var/lib/docker/bosn-inputs";
fn hash(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(bytes))
}
fn file_hash(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256Hasher::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("sha256:{}", hasher.finalize()))
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |t| t.as_secs_f64())
}
fn error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}
/// These bytes and observations are trusted daemon inputs, never client proof.
pub struct ActRuntimeRequest<'a> {
    pub intent: &'a ActEngineIntent,
    pub observed: &'a ActEngineObservation,
    pub package: &'a ActImagePackage,
    pub base_blobs: &'a [ActArchiveBlob<'a>],
    pub source_archive: &'a [u8],
    pub event_payload: &'a [u8],
    pub event_name: &'a str,
    pub evidence_root: &'a Path,
    pub archive_ceiling: u64,
    pub execution_deadline: Duration,
    pub output_ceiling: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActJobResult {
    pub id: String,
    pub name: String,
    pub matrix: Value,
    pub outcome: String,
    pub reason: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct ActRuntimeReport {
    pub schema_version: u32,
    pub run_id: String,
    pub candidate_sha: String,
    pub engine_id: String,
    pub act_manifest_digest: String,
    pub act_config_digest: String,
    pub act_binary_digest: String,
    pub runner_manifest_digest: String,
    pub runner_config_digest: String,
    pub act_docker_image_id: Option<String>,
    pub runner_docker_image_id: Option<String>,
    pub event_sha256: String,
    pub snapshot_sha256: String,
    pub selection_scope: String,
    pub jobs: Vec<ActJobResult>,
    pub execution_success: bool,
    pub outcome: String,
    pub output_sha256: Option<String>,
    pub engine_removed: bool,
    pub failure: Option<String>,
}
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
/// Act0.2.88 emits jobID, job, matrix and terminal jobResult JSON fields.
/// Unknown, missing and unsupported results cannot become execution success.
pub fn parse_job_results(bytes: &[u8]) -> std::io::Result<Vec<ActJobResult>> {
    let mut jobs: BTreeMap<String, ActJobResult> = BTreeMap::new();
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.iter().all(u8::is_ascii_whitespace))
    {
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| error("Act output is not complete JSON-lines evidence"))?;
        let Some(id) = value["jobID"].as_str() else {
            continue;
        };
        let name = value["job"].as_str().unwrap_or(id);
        let matrix = value.get("matrix").cloned().unwrap_or_else(|| json!({}));
        let key = format!("{name}\0{id}\0{matrix}");
        let job = jobs.entry(key).or_insert_with(|| ActJobResult {
            id: id.into(),
            name: name.into(),
            matrix,
            outcome: "incomplete".into(),
            reason: None,
        });
        if value["msg"]
            .as_str()
            .is_some_and(|m| m.contains("Skipping unsupported platform"))
        {
            job.outcome = "unsupported".into();
            job.reason = value["msg"].as_str().map(str::to_owned);
        } else if let Some(result) = value["jobResult"].as_str() {
            if !matches!(result, "success" | "failure" | "skipped" | "cancelled")
                || (job.outcome != "incomplete" && job.outcome != result)
            {
                return Err(error("conflicting or unknown Act job result"));
            }
            job.outcome = result.into();
        }
    }
    if jobs.len() > 4096 {
        return Err(error("Act job inventory exceeds bounded report"));
    }
    Ok(jobs.into_values().collect())
}
fn private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
fn tar_number(field: &[u8]) -> std::io::Result<usize> {
    let field = std::str::from_utf8(field)
        .map_err(|_| error("non-octal snapshot size"))?
        .trim_matches(['\0', ' ']);
    usize::from_str_radix(if field.is_empty() { "0" } else { field }, 8)
        .map_err(|_| error("invalid snapshot integer"))
}
/// Only ordinary USTAR files/directories are admitted. Links, devices, PAX path
/// overrides and sparse extensions require a separately reviewed snapshot seam.
fn verify_snapshot(tar: &[u8]) -> std::io::Result<()> {
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
async fn verify_registry(
    registry: &RegistryActor,
    request: &ActRuntimeRequest<'_>,
    token: &str,
) -> std::io::Result<()> {
    let reply = registry
        .act_registry(ActRegistryCommand::VerifyClaimed {
            run: request.intent.run_id.clone(),
            observed: request.observed.clone(),
            token: token.into(),
        })
        .await
        .map_err(|e| error(e.to_string()))?;
    let ActRegistryReply::Verified(record) = reply else {
        return Err(error("registry did not verify execution ownership"));
    };
    if record.intent != *request.intent
        || record.state != bosn_registry::act::ActEngineState::Registered
        || record.engine_id.as_deref() != Some(request.observed.engine_id.as_str())
        || record.execution.is_some()
    {
        return Err(error(
            "registered execution identity changed or already executed",
        ));
    }
    Ok(())
}
async fn control(
    registry: &RegistryActor,
    engine: &DockerEngine,
    ownership: (&ActRuntimeRequest<'_>, &str),
    args: Vec<String>,
    started: Instant,
    cancellation: &async_engine::CancellationToken,
    command_timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    let (request, token) = ownership;
    if cancellation.is_cancelled() {
        return Err(error("Act runtime cancelled"));
    }
    let remaining = request
        .execution_deadline
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| error("Act lifecycle deadline exceeded"))?;
    verify_registry(registry, request, token).await?;
    let output = engine
        .with_args(args)
        .capture_async(RunOptions::bounded(remaining.min(command_timeout), 2 << 20))
        .await
        .map_err(|e| error(e.to_string()))?;
    if output.exit_code != 0 {
        return Err(error(format!(
            "owned engine control failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}
fn verify_ready_engine(bytes: &[u8]) -> std::io::Result<()> {
    let info: Value = serde_json::from_slice(bytes)?;
    let containerd = info["DriverStatus"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row == &json!(["driver-type", "io.containerd.snapshotter.v1"]))
    });
    if info["DockerRootDir"] != "/var/lib/docker"
        || info["Driver"] != "native"
        || info["ServerVersion"] != "29.7.2"
        || info["OSType"] != "linux"
        || !matches!(info["Architecture"].as_str(), Some("x86_64" | "amd64"))
        || !containerd
    {
        return Err(error(
            "private daemon native snapshotter/storage identity mismatch",
        ));
    }
    Ok(())
}
async fn wait_ready_engine(
    registry: &RegistryActor,
    engine: &DockerEngine,
    ownership: (&ActRuntimeRequest<'_>, &str),
    started: Instant,
    cancellation: &async_engine::CancellationToken,
) -> std::io::Result<()> {
    let (request, token) = ownership;
    let readiness = request.execution_deadline.min(Duration::from_secs(30));
    loop {
        if cancellation.is_cancelled() {
            return Err(error("Act runtime cancelled during readiness"));
        }
        if started.elapsed() >= readiness {
            return Err(error("private daemon readiness deadline exceeded"));
        }
        verify_registry(registry, request, token).await?;
        match control(
            registry,
            engine,
            ownership,
            vec![
                "exec".into(),
                request.observed.engine_id.clone(),
                "docker".into(),
                "info".into(),
                "--format={{json .}}".into(),
            ],
            started,
            cancellation,
            readiness.saturating_sub(started.elapsed()),
        )
        .await
        {
            Ok(bytes) => return verify_ready_engine(&bytes),
            Err(_) => async_engine::sleep(Duration::from_millis(100)).await,
        }
    }
}
fn driver_arguments(
    request: &ActRuntimeRequest<'_>,
    image_id: &str,
    runner_id: &str,
) -> Vec<String> {
    let source = format!("{INPUTS}/source");
    let mut args = vec![
        "exec".into(),
        request.observed.engine_id.clone(),
        "docker".into(),
        "run".into(),
        "--rm".into(),
        "--pull=never".into(),
        "--platform=linux/amd64".into(),
        "--name".into(),
        format!("bosn-act-driver-{}", request.intent.run_id),
        "--network=bridge".into(),
        "--memory=536870912".into(),
        "--memory-swap=536870912".into(),
        "--pids-limit=256".into(),
        "--read-only".into(),
        "--workdir=/tmp".into(),
        "--tmpfs".into(),
        "/tmp:rw,nosuid,nodev,size=268435456".into(),
        "--mount".into(),
        format!("type=bind,source={source},target={source},readonly"),
        "--mount".into(),
        format!("type=bind,source={INPUTS}/event.json,target={INPUTS}/event.json,readonly"),
        "--mount".into(),
        "type=bind,source=/var/run/docker.sock,target=/var/run/docker.sock".into(),
        "--env=DOCKER_HOST=unix:///var/run/docker.sock".into(),
        "--env=HOME=/tmp/home".into(),
        "--env=XDG_CONFIG_HOME=/tmp/config".into(),
        image_id.into(),
        request.event_name.into(),
        "--json".into(),
        "--verbose".into(),
        "--pull=false".into(),
        "--container-architecture=linux/amd64".into(),
        "--network=bridge".into(),
        "-C".into(),
        source.clone(),
        "-W".into(),
        format!("{source}/.github/workflows"),
        "--eventpath".into(),
        format!("{INPUTS}/event.json"),
        "--env".into(),
        format!("SHA_REF={}", request.intent.candidate_sha),
    ];
    for flag in ["--env-file", "--secret-file", "--var-file", "--input-file"] {
        args.extend([flag.into(), "/dev/null".into()]);
    }
    for platform in ["ubuntu-latest", "ubuntu-22.04"] {
        args.extend(["--platform".into(), format!("{platform}={runner_id}")]);
    }
    for platform in [
        "ubuntu-24.04",
        "ubuntu-20.04",
        "windows-latest",
        "windows-2025",
        "windows-2022",
        "windows-11-arm",
        "macos-latest",
        "macos-15",
        "macos-15-intel",
        "macos-14",
        "self-hosted",
    ] {
        args.extend(["--platform".into(), format!("{platform}=")]);
    }
    args
}
/// A dropped daemon job future schedules cleanup while its runtime is alive.
/// A process crash still requires startup reconciliation of durable records.
struct RuntimeCleanupGuard {
    registry: RegistryActor,
    engine: DockerEngine,
    run: String,
    intent: ActEngineIntent,
    observed: ActEngineObservation,
    token: String,
    active: bool,
}
impl RuntimeCleanupGuard {
    fn new(
        registry: &RegistryActor,
        engine: &DockerEngine,
        request: &ActRuntimeRequest<'_>,
        token: &str,
    ) -> Self {
        Self {
            registry: registry.clone(),
            engine: engine.clone(),
            run: request.intent.run_id.clone(),
            intent: request.intent.clone(),
            observed: request.observed.clone(),
            token: token.into(),
            active: true,
        }
    }
}
impl Drop for RuntimeCleanupGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let registry = self.registry.clone();
        let engine = self.engine.clone();
        let run = self.run.clone();
        let observed = self.observed.clone();
        let token = self.token.clone();
        let intent = self.intent.clone();
        async_engine::launch(async move {
            // Same-actor order covers a committed claim whose reply was lost.
            // A canceled loser or uncommitted claim has no cleanup authority.
            let Ok(ActRegistryReply::Verified(record)) = registry
                .act_registry(ActRegistryCommand::VerifyClaimed {
                    run: run.clone(),
                    observed: observed.clone(),
                    token: token.clone(),
                })
                .await
            else {
                return;
            };
            if record.intent != intent
                || record.engine_id.as_deref() != Some(observed.engine_id.as_str())
            {
                return;
            }
            let _ = registry
                .act_registry(ActRegistryCommand::CleanupClaimed {
                    run: run.clone(),
                    token,
                    outcome: ActRunOutcome::Interrupted,
                    at: now(),
                })
                .await;
            // Registry authorization remains mandatory even if the state was
            // already cleanup-required, or another cleanup task won the race.
            let _ =
                crate::act_engine::remove_owned_engine(&registry, &engine, &run, observed, now())
                    .await;
        })
        .detach();
    }
}
/// Execute every triggered workflow in the frozen workflow directory. The
/// caller must retain this future or cancel its token, and reconcile all durable
/// nonterminal records on daemon startup; dropping the future is not absence.
pub async fn run_registered_act(
    registry: &RegistryActor,
    engine: &DockerEngine,
    request: ActRuntimeRequest<'_>,
    cancellation: &async_engine::CancellationToken,
) -> std::io::Result<ActRuntimeReport> {
    let started = Instant::now();
    let random = kernal_api::random::SecureRandom::new(1, Duration::from_secs(5))
        .map_err(|e| error(e.to_string()))?;
    let token = crate::uuid(&random.bytes(16).await.map_err(|e| error(e.to_string()))?);
    // Inert until exact committed ownership can be observed. It does not grant
    // cleanup authority merely because a claim request was sent.
    let mut cleanup_guard = RuntimeCleanupGuard::new(registry, engine, &request, &token);
    let claim = registry
        .act_registry(ActRegistryCommand::Claim {
            intent: request.intent.clone(),
            observed: request.observed.clone(),
            token: token.clone(),
            at: now(),
        })
        .await
        .map_err(|e| error(e.to_string()))?;
    if !matches!(claim, ActRegistryReply::Claimed(_)) {
        return Err(error("registry did not commit exclusive execution claim"));
    }
    let evidence = request.evidence_root.join(&request.intent.run_id);
    let mut report = ActRuntimeReport {
        schema_version: 1,
        run_id: request.intent.run_id.clone(),
        candidate_sha: request.intent.candidate_sha.clone(),
        engine_id: request.observed.engine_id.clone(),
        act_manifest_digest: request.package.manifest_digest.clone(),
        act_config_digest: request.package.config_digest.clone(),
        act_binary_digest: request.package.binary_digest.clone(),
        runner_manifest_digest: request.intent.runner_image_digest.clone(),
        runner_config_digest: request.package.runner_config_digest.clone(),
        act_docker_image_id: None,
        runner_docker_image_id: None,
        event_sha256: request.intent.payload_sha256.clone(),
        snapshot_sha256: request.intent.snapshot_sha256.clone(),
        selection_scope: "all event-triggered workflows in frozen .github/workflows; only Linux amd64 ubuntu-latest/ubuntu-22.04 mapped; foreign cells cannot pass".into(),
        jobs: vec![],
        execution_success: false,
        outcome: "incomplete".into(),
        output_sha256: None,
        engine_removed: false,
        failure: None,
    };
    let execution: std::io::Result<()> = async {
        private_directory(&evidence)?;
        let control = |args| {
            control(
                registry,
                engine,
                (&request, token.as_str()),
                args,
                started,
                cancellation,
                Duration::from_secs(30),
            )
        };
        if request.archive_ceiling == 0
            || request.archive_ceiling > 2 << 30
            || request.output_ceiling == 0
            || request.output_ceiling > 64 << 20
            || request.execution_deadline.is_zero()
            || request.execution_deadline > Duration::from_secs(7200)
            || request.event_payload.len() > 1 << 20
            || hash(request.source_archive) != format!("sha256:{}", request.intent.snapshot_sha256)
            || hash(request.event_payload) != format!("sha256:{}", request.intent.payload_sha256)
            || request.package.manifest_digest != request.intent.act_image_digest
        {
            return Err(error("runtime input identity or limits invalid"));
        }
        verify_snapshot(request.source_archive)?;
        let payload: Value = serde_json::from_slice(request.event_payload)?;
        let selected = match request.event_name {
            "push" => &payload["after"],
            "pull_request" => &payload["pull_request"]["head"]["sha"],
            "workflow_dispatch" => payload["inputs"]
                .get("candidate_sha")
                .unwrap_or(&payload["inputs"]["commit_sha"]),
            _ => return Err(error("unsupported typed Act event")),
        };
        if selected != &request.intent.candidate_sha {
            return Err(error("event candidate identity mismatch"));
        }
        let manifest: Value = serde_json::from_slice(&request.package.manifest)?;
        if manifest["annotations"]["com.zackees.bosn.act.base-manifest"]
            != request.intent.runner_image_digest
        {
            return Err(error("Act package runner pin mismatch"));
        }
        private_file(
            &evidence.join("intent.json"),
            &serde_json::to_vec(request.intent)?,
        )?;
        let mut archive_options = std::fs::OpenOptions::new();
        archive_options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            archive_options.mode(0o600);
        }
        let mut archive = archive_options.open(evidence.join("act.oci.tar"))?;
        write_act_oci_archive(
            request.package,
            request.base_blobs,
            "bosn-act",
            request.archive_ceiling,
            &mut archive,
        )
        .map_err(|e| error(e.to_string()))?;
        archive.sync_all()?;
        private_file(&evidence.join("source.tar"), request.source_archive)?;
        private_file(&evidence.join("event.json"), request.event_payload)?;
        sync_directory(&evidence)?;
        wait_ready_engine(registry, engine, (&request, &token), started, cancellation).await?;
        let id = &request.observed.engine_id;
        control(vec![
            "exec".into(),
            id.clone(),
            "mkdir".into(),
            "-p".into(),
            format!("{INPUTS}/source"),
        ])
        .await?;
        for file in ["act.oci.tar", "source.tar", "event.json"] {
            control(vec![
                "cp".into(),
                evidence.join(file).to_string_lossy().into_owned(),
                format!("{id}:{INPUTS}/{file}"),
            ])
            .await?;
        }
        let copied = control(vec![
            "exec".into(),
            id.clone(),
            "sha256sum".into(),
            format!("{INPUTS}/act.oci.tar"),
            format!("{INPUTS}/source.tar"),
            format!("{INPUTS}/event.json"),
        ])
        .await?;
        let expected_copies = format!(
            "{}  {INPUTS}/act.oci.tar\n{}  {INPUTS}/source.tar\n{}  {INPUTS}/event.json\n",
            &file_hash(&evidence.join("act.oci.tar"))?[7..],
            request.intent.snapshot_sha256,
            request.intent.payload_sha256
        );
        if copied != expected_copies.as_bytes() {
            return Err(error("engine-private copied input identity mismatch"));
        }
        control(vec![
            "exec".into(),
            id.clone(),
            "tar".into(),
            "-xf".into(),
            format!("{INPUTS}/source.tar"),
            "-C".into(),
            format!("{INPUTS}/source"),
        ])
        .await?;
        control(vec![
            "exec".into(),
            id.clone(),
            "docker".into(),
            "load".into(),
            "--input".into(),
            format!("{INPUTS}/act.oci.tar"),
        ])
        .await?;
        let loaded = control(vec![
            "exec".into(),
            id.clone(),
            "docker".into(),
            "image".into(),
            "inspect".into(),
            "bosn-act".into(),
        ])
        .await?;
        let image_id = verify_loaded_image(
            &loaded,
            &request.package.manifest_digest,
            &request.package.config_digest,
            &request.package.manifest,
            &request.package.config,
        )?;
        let runner_loaded = control(vec![
            "exec".into(),
            id.clone(),
            "docker".into(),
            "image".into(),
            "inspect".into(),
            "bosn-act-runner".into(),
        ])
        .await?;
        if request.package.runner_manifest_digest != request.intent.runner_image_digest {
            return Err(error("runner immutable manifest mismatch"));
        }
        let runner_id = verify_loaded_image(
            &runner_loaded,
            &request.package.runner_manifest_digest,
            &request.package.runner_config_digest,
            &request.package.runner_manifest,
            &request.package.runner_config,
        )?;
        report.act_docker_image_id = Some(image_id.clone());
        report.runner_docker_image_id = Some(runner_id.clone());
        verify_registry(registry, &request, &token).await?;
        let (sender, mut receiver) = async_engine::channel(16);
        let log_path = evidence.join("output.frames");
        let mut opts = std::fs::OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(log_path)?;
        let logging = async_engine::launch(async move {
            let mut digest = Sha256Hasher::new();
            while let Some(event) = receiver.recv().await {
                let (tag, bytes) = match event {
                    bosn_engine::EngineEvent::Stdout(b) => (1u8, b),
                    bosn_engine::EngineEvent::Stderr(b) => (2u8, b),
                };
                digest.update([tag]);
                digest.update((bytes.len() as u32).to_le_bytes());
                digest.update(&bytes);
                file.write_all(&[tag])?;
                file.write_all(&(bytes.len() as u32).to_le_bytes())?;
                file.write_all(&bytes)?;
                file.sync_data()?;
            }
            file.sync_all()?;
            Ok::<_, std::io::Error>(format!("sha256:{}", digest.finalize()))
        });
        let output = engine
            .with_args(driver_arguments(&request, &image_id, &runner_id))
            .stream(
                RunOptions::streaming(
                    request
                        .execution_deadline
                        .checked_sub(started.elapsed())
                        .filter(|d| !d.is_zero())
                        .ok_or_else(|| error("Act lifecycle deadline exceeded"))?,
                    request.output_ceiling,
                ),
                Some(cancellation),
                &sender,
            )
            .await;
        drop(sender);
        report.output_sha256 = Some(logging.await.map_err(|e| error(e.to_string()))??);
        let output = output.map_err(|e| error(e.to_string()))?;
        let mut bytes = output.stdout;
        bytes.extend_from_slice(&output.stderr);
        report.jobs = parse_job_results(&bytes)?;
        if output.exit_code != 0
            || report.jobs.is_empty()
            || report
                .jobs
                .iter()
                .any(|j| !matches!(j.outcome.as_str(), "success" | "skipped"))
        {
            return Err(error("Act execution or supported job coverage failed"));
        }
        report.execution_success = true;
        Ok(())
    }
    .await;
    if let Err(e) = execution {
        report.failure = Some(e.to_string());
    }
    let outcome = if cancellation.is_cancelled() {
        ActRunOutcome::Cancelled
    } else if report.execution_success {
        ActRunOutcome::Passed
    } else {
        ActRunOutcome::Failed
    };
    report.outcome = match outcome {
        ActRunOutcome::Passed => "passed",
        ActRunOutcome::Failed => "failed",
        ActRunOutcome::Cancelled => "cancelled",
        ActRunOutcome::Interrupted => "interrupted",
    }
    .into();
    let execution_evidence = serde_json::to_vec(&report)?;
    if execution_evidence.len() > 1 << 20 {
        return Err(error("execution evidence too large"));
    }
    private_file(&evidence.join("execution.json"), &execution_evidence)?;
    sync_directory(&evidence)?;
    registry
        .act_registry(ActRegistryCommand::Execution {
            run: request.intent.run_id.clone(),
            token: token.clone(),
            outcome,
            at: now(),
        })
        .await
        .map_err(|e| error(e.to_string()))?;
    registry
        .act_registry(ActRegistryCommand::CleanupClaimed {
            run: request.intent.run_id.clone(),
            token: token.clone(),
            outcome,
            at: now(),
        })
        .await
        .map_err(|e| error(e.to_string()))?;
    let removal = crate::act_engine::remove_owned_engine(
        registry,
        engine,
        &request.intent.run_id,
        request.observed.clone(),
        now(),
    )
    .await;
    match removal {
        Ok(()) => report.engine_removed = true,
        Err(e) => {
            report.failure = Some(format!(
                "{}; cleanup: {e}",
                report.failure.as_deref().unwrap_or("execution completed")
            ))
        }
    };
    cleanup_guard.active = false;
    let raw = serde_json::to_vec(&report)?;
    if raw.len() > 1 << 20 {
        return Err(error("result evidence too large"));
    }
    private_file(&evidence.join("result.json.pending"), &raw)?;
    std::fs::rename(
        evidence.join("result.json.pending"),
        evidence.join("result.json"),
    )?;
    sync_directory(&evidence)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_actor_transport_failures_persist_results_and_never_retire_uncertain_absence() {
        use bosn_registry::{Registry, act::ActEngineState};
        const OWNER: &str = "11111111-2222-4333-8444-555555555555";
        const SCRIPT: &str = r#"import sys,json,sqlite3,time
db,image,mode,log=sys.argv[1:5];args=sys.argv[5:]
with open(log,'a') as f: f.write(json.dumps(args)+'\n')
con=sqlite3.connect(db)
record=json.loads(con.execute("select detail from events where kind like 'act.engine.v1:%' order by id desc limit 1").fetchone()[0])
if args[:2]==['container','rm']:
 assert record['state']=='cleanup_required' and args[-1]==record['engine_id']
 print(record['engine_id'])
elif args[:2]==['container','ls']:
 if mode=='probe-failed': sys.exit(8)
elif args[0]=='cp':
 assert record['state']=='registered' and args[-1].startswith(record['engine_id']+':/var/lib/docker/bosn-inputs/')
 if mode=='copy-failed': sys.exit(7)
elif args[:4]==['exec',record['engine_id'],'docker','info']:
 if mode=='readiness-timeout': sys.exit(8)
 value={'DockerRootDir':'/var/lib/docker','Driver':'native','ServerVersion':'29.7.2','OSType':'linux','Architecture':'x86_64','DriverStatus':[['driver-type','io.containerd.snapshotter.v1']]}
 if mode=='foreign-storage': value['DockerRootDir']='/var/lib/containerd'
 print(json.dumps(value))
elif args[:3]==['exec',record['engine_id'],'sha256sum']:
 import hashlib
 from pathlib import Path
 evidence=Path(db).parent/'evidence'/record['intent']['run_id']
 for path in args[3:]:
  digest=hashlib.sha256((evidence/Path(path).name).read_bytes()).hexdigest()
  if mode=='copy-corrupt': digest='f'*64
  print(digest+'  '+path)
elif args[:4]==['exec',record['engine_id'],'docker','load']:
 if mode=='load-failed': sys.exit(7)
elif args[:5]==['exec',record['engine_id'],'docker','image','inspect']:
 value=json.load(open(image))['runner' if args[-1]=='bosn-act-runner' else 'act']
 if mode=='missing-runner' and args[-1]=='bosn-act-runner': sys.exit(7)
 if mode=='foreign-image': value[0]['Descriptor']['digest']='sha256:'+'f'*64
 print(json.dumps(value))
elif args[:4]==['exec',record['engine_id'],'docker','run']:
 assert record['state']=='registered'
 assert '--network=bridge' in args and '--read-only' in args and '-W' in args and args[args.index('-W')+1].endswith('/source/.github/workflows')
 assert not any('GITHUB_TOKEN' in x for x in args)
 expected=json.load(open(image))['runner'][0]['Id']
 assert 'ubuntu-latest='+expected in args and 'ubuntu-22.04='+expected in args
 if mode in ['timeout','dropped','duplicate']: time.sleep(4)
 print(json.dumps({'jobID':'lint','job':'CI/lint','matrix':{},'jobResult':'success'}))
 if mode=='unsupported': print(json.dumps({'jobID':'mac','job':'CI/mac','matrix':{},'msg':'Skipping unsupported platform macos-latest'}))
elif args[:3]==['exec',record['engine_id'],'mkdir'] or args[:3]==['exec',record['engine_id'],'tar']: pass
else: raise Exception('unexpected args '+repr(args))
"#;
        for mode in [
            "success",
            "copy-failed",
            "copy-corrupt",
            "load-failed",
            "foreign-image",
            "missing-runner",
            "unsupported",
            "timeout",
            "probe-failed",
            "cancelled",
            "foreign-storage",
            "readiness-timeout",
            "dropped",
            "duplicate",
            "claim-race",
            "claim-drop",
            "loser-claim-drop",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            std::fs::create_dir_all(source.join(".github/workflows")).unwrap();
            std::fs::write(
                source.join(".github/workflows/ci.yml"),
                "name: CI\non: push\njobs: {}\n",
            )
            .unwrap();
            let snapshot_path = dir.path().join("snapshot.tar");
            assert!(
                std::process::Command::new("tar")
                    .args(["--format=ustar", "--owner=0", "--group=0", "-cf"])
                    .arg(&snapshot_path)
                    .arg("-C")
                    .arg(&source)
                    .arg(".github/workflows/ci.yml")
                    .status()
                    .unwrap()
                    .success()
            );
            let snapshot = std::fs::read(&snapshot_path).unwrap();
            let base_layer = b"synthetic verified base layer";
            let base_layer_digest = hash(base_layer);
            let config = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":[base_layer_digest]}})).unwrap();
            let base = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":hash(&config),"size":config.len()},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":base_layer_digest,"size":base_layer.len()}]})).unwrap();
            let package = crate::act_image::package_act_image(
                &base,
                &hash(&base),
                &config,
                &hash(&config),
                b"synthetic Act",
                &hash(b"synthetic Act"),
                "0.2.88",
            )
            .unwrap();
            let candidate = "a".repeat(40);
            let payload = serde_json::to_vec(&json!({"after":candidate})).unwrap();
            let intent = ActEngineIntent {
                run_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
                workspace: source.to_string_lossy().into_owned(),
                candidate_sha: candidate,
                payload_sha256: hash(&payload)[7..].into(),
                snapshot_sha256: hash(&snapshot)[7..].into(),
                act_version: "0.2.88".into(),
                act_image_digest: package.manifest_digest.clone(),
                engine_image_digest: format!("sha256:{}", "e".repeat(64)),
                runner_image_digest: hash(&base),
                created_at: now().floor(),
            };
            let observed = ActEngineObservation {
                name: intent.engine_name(),
                engine_id: "1".repeat(64),
                image_digest: intent.engine_image_digest.clone(),
                labels: intent.required_labels(OWNER).unwrap(),
            };
            let db = dir.path().join("registry.sqlite3");
            let mut registry = Registry::create_writer(&db, OWNER).unwrap();
            let mut transaction = registry.begin_immediate().unwrap();
            transaction.begin_act_engine(&intent).unwrap();
            transaction
                .register_act_engine(&intent.run_id, &observed, now())
                .unwrap_or_else(|e| panic!("{mode}: registration {e:?}"));
            if mode == "loser-claim-drop" {
                transaction
                    .claim_act_execution(
                        &intent,
                        &observed,
                        "12345678-1234-4234-8234-123456789abc",
                        now(),
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
            let image_config: Value = serde_json::from_slice(&package.config).unwrap();
            let runner_config: Value = serde_json::from_slice(&package.runner_config).unwrap();
            let image = json!({"act":[{"Id":package.manifest_digest,"Descriptor":{"digest":package.manifest_digest,"mediaType":serde_json::from_slice::<Value>(&package.manifest).unwrap()["mediaType"],"size":package.manifest.len()},"RootFS":{"Type":"layers","Layers":image_config["rootfs"]["diff_ids"]},"Config":{"Volumes":null}}],"runner":[{"Id":package.runner_manifest_digest,"Descriptor":{"digest":package.runner_manifest_digest,"mediaType":serde_json::from_slice::<Value>(&package.runner_manifest).unwrap()["mediaType"],"size":package.runner_manifest.len()},"RootFS":{"Type":"layers","Layers":runner_config["rootfs"]["diff_ids"]},"Config":{"Volumes":null}}]});
            let image_path = dir.path().join("image.json");
            std::fs::write(&image_path, serde_json::to_vec(&image).unwrap()).unwrap();
            let script = dir.path().join("docker.py");
            std::fs::write(&script, SCRIPT).unwrap();
            let commands = dir.path().join("commands.jsonl");
            let engine = DockerEngine::synthetic_for_test(
                "uv",
                [
                    "run".into(),
                    "--no-project".into(),
                    "python".into(),
                    script.to_string_lossy().into_owned(),
                    db.to_string_lossy().into_owned(),
                    image_path.to_string_lossy().into_owned(),
                    mode.into(),
                    commands.to_string_lossy().into_owned(),
                ],
            );
            let evidence = dir.path().join("evidence");
            std::fs::create_dir(&evidence).unwrap();
            let runtime = async_engine::RuntimeBuilder::multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.run(async {
                let (sender, receiver) = async_engine::channel(16);
                let actor = RegistryActor { sender };
                let task = async_engine::launch(async move {
                    if matches!(mode, "claim-drop" | "loser-claim-drop") {
                        async_engine::sleep(Duration::from_millis(250)).await;
                    }
                    crate::registry_actor(registry, receiver, None).await
                });
                if mode == "claim-race" {
                    let contenders = [
                        "12345678-1234-4234-8234-123456789abc",
                        "98765432-1234-4234-8234-123456789abc",
                    ];
                    let mut claims = Vec::new();
                    for token in contenders {
                        let actor = actor.clone();
                        let intent = intent.clone();
                        let observed = observed.clone();
                        claims.push(async_engine::launch(async move {
                            actor
                                .act_registry(ActRegistryCommand::Claim {
                                    intent,
                                    observed,
                                    token: token.into(),
                                    at: now(),
                                })
                                .await
                        }));
                    }
                    let mut owner = None;
                    for claim in claims {
                        if let Ok(ActRegistryReply::Claimed(record)) = claim.await.unwrap() {
                            assert!(owner.is_none());
                            owner = record.execution_claim;
                        }
                    }
                    let owner = owner.expect("one atomic winner");
                    assert!(
                        actor
                            .act_registry(ActRegistryCommand::Cleanup {
                                run: intent.run_id.clone(),
                                outcome: ActRunOutcome::Interrupted,
                                at: now()
                            })
                            .await
                            .is_err()
                    );
                    assert!(!commands.exists());
                    actor
                        .act_registry(ActRegistryCommand::CleanupClaimed {
                            run: intent.run_id.clone(),
                            token: owner,
                            outcome: ActRunOutcome::Interrupted,
                            at: now(),
                        })
                        .await
                        .unwrap();
                    crate::act_engine::remove_owned_engine(
                        &actor,
                        &engine,
                        &intent.run_id,
                        observed.clone(),
                        now(),
                    )
                    .await
                    .unwrap();
                    actor.stop().await;
                    task.await.unwrap();
                    return;
                }
                let cancellation_source = async_engine::CancellationSource::new();
                let cancellation = cancellation_source.token();
                if mode == "cancelled" {
                    cancellation_source.cancel();
                }
                let base_blobs = [ActArchiveBlob {
                    digest: &base_layer_digest,
                    bytes: base_layer,
                }];
                let execution = run_registered_act(
                    &actor,
                    &engine,
                    ActRuntimeRequest {
                        intent: &intent,
                        observed: &observed,
                        package: &package,
                        base_blobs: &base_blobs,
                        source_archive: &snapshot,
                        event_payload: &payload,
                        event_name: "push",
                        evidence_root: &evidence,
                        archive_ceiling: 1 << 20,
                        execution_deadline: Duration::from_secs(
                            if matches!(mode, "timeout" | "readiness-timeout") {
                                2
                            } else {
                                10
                            },
                        ),
                        output_ceiling: 1 << 20,
                    },
                    &cancellation,
                );
                if mode == "duplicate" {
                    let mut first = Box::pin(execution);
                    assert!(
                        async_engine::timeout(Duration::from_millis(500), &mut first)
                            .await
                            .is_err()
                    );
                    let ActRegistryReply::Recovery(before) = actor
                        .act_registry(ActRegistryCommand::Pending {
                            after_run_id: None,
                            limit: 16,
                        })
                        .await
                        .unwrap()
                    else {
                        panic!("recovery reply");
                    };
                    let commands_before = std::fs::read(&commands).unwrap();
                    let receipt_before =
                        std::fs::read(evidence.join(&intent.run_id).join("intent.json")).unwrap();
                    let duplicate = run_registered_act(
                        &actor,
                        &engine,
                        ActRuntimeRequest {
                            intent: &intent,
                            observed: &observed,
                            package: &package,
                            base_blobs: &base_blobs,
                            source_archive: &snapshot,
                            event_payload: &payload,
                            event_name: "push",
                            evidence_root: &evidence,
                            archive_ceiling: 1 << 20,
                            execution_deadline: Duration::from_secs(10),
                            output_ceiling: 1 << 20,
                        },
                        &cancellation,
                    )
                    .await;
                    assert!(
                        duplicate.is_err(),
                        "duplicate caller mutated execution: {duplicate:?}"
                    );
                    let ActRegistryReply::Recovery(page) = actor
                        .act_registry(ActRegistryCommand::Pending {
                            after_run_id: None,
                            limit: 16,
                        })
                        .await
                        .unwrap()
                    else {
                        panic!("recovery reply");
                    };
                    assert_eq!(page.items[0].state, ActEngineState::Registered);
                    assert_eq!(page.items, before.items);
                    assert_eq!(std::fs::read(&commands).unwrap(), commands_before);
                    assert_eq!(
                        std::fs::read(evidence.join(&intent.run_id).join("intent.json")).unwrap(),
                        receipt_before
                    );
                    assert!(
                        !std::fs::read_to_string(&commands)
                            .unwrap()
                            .contains("container\", \"rm")
                    );
                    drop(first);
                    for _ in 0..100 {
                        let ActRegistryReply::Recovery(page) = actor
                            .act_registry(ActRegistryCommand::Pending {
                                after_run_id: None,
                                limit: 16,
                            })
                            .await
                            .unwrap()
                        else {
                            panic!("recovery reply");
                        };
                        if page.items.is_empty() {
                            break;
                        }
                        async_engine::sleep(Duration::from_millis(20)).await;
                    }
                    actor.stop().await;
                    task.await.unwrap();
                    return;
                }
                if mode == "loser-claim-drop" {
                    assert!(
                        async_engine::timeout(Duration::from_millis(100), execution)
                            .await
                            .is_err()
                    );
                    async_engine::sleep(Duration::from_millis(300)).await;
                    let ActRegistryReply::Recovery(page) = actor
                        .act_registry(ActRegistryCommand::Pending {
                            after_run_id: None,
                            limit: 16,
                        })
                        .await
                        .unwrap()
                    else {
                        panic!("recovery reply");
                    };
                    assert_eq!(page.items[0].state, ActEngineState::Registered);
                    assert_eq!(
                        page.items[0].execution_claim.as_deref(),
                        Some("12345678-1234-4234-8234-123456789abc")
                    );
                    assert!(!commands.exists());
                    assert_eq!(std::fs::read_dir(&evidence).unwrap().count(), 0);
                    actor
                        .act_registry(ActRegistryCommand::CleanupClaimed {
                            run: intent.run_id.clone(),
                            token: "12345678-1234-4234-8234-123456789abc".into(),
                            outcome: ActRunOutcome::Interrupted,
                            at: now(),
                        })
                        .await
                        .unwrap();
                    crate::act_engine::remove_owned_engine(
                        &actor,
                        &engine,
                        &intent.run_id,
                        observed.clone(),
                        now(),
                    )
                    .await
                    .unwrap();
                    actor.stop().await;
                    task.await.unwrap();
                    return;
                }
                if matches!(mode, "dropped" | "claim-drop") {
                    assert!(
                        async_engine::timeout(
                            Duration::from_millis(if mode == "claim-drop" { 100 } else { 500 }),
                            execution
                        )
                        .await
                        .is_err()
                    );
                    for _ in 0..100 {
                        let ActRegistryReply::Recovery(page) = actor
                            .act_registry(ActRegistryCommand::Pending {
                                after_run_id: None,
                                limit: 16,
                            })
                            .await
                            .unwrap()
                        else {
                            panic!("recovery reply");
                        };
                        if page.items.is_empty() {
                            break;
                        }
                        async_engine::sleep(Duration::from_millis(20)).await;
                    }
                    actor.stop().await;
                    task.await.unwrap();
                    return;
                }
                let report = execution.await.unwrap();
                assert_eq!(
                    report.execution_success,
                    matches!(mode, "success" | "probe-failed"),
                    "{mode}: {:?}",
                    report.failure
                );
                assert_eq!(report.engine_removed, mode != "probe-failed", "{mode}");
                let persisted: Value = serde_json::from_slice(
                    &std::fs::read(evidence.join(&intent.run_id).join("result.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(persisted["engine_removed"], report.engine_removed);
                if mode == "unsupported" {
                    assert!(report.jobs.iter().any(|j| j.outcome == "unsupported"));
                }
                actor.stop().await;
                task.await.unwrap();
            });
            let registry = Registry::open_writer(&db).unwrap();
            let record = registry.act_engine(&intent.run_id).unwrap().unwrap();
            assert_eq!(
                record.state,
                if mode == "probe-failed" {
                    ActEngineState::CleanupRequired
                } else {
                    ActEngineState::Terminal
                }
            );
            assert!(
                std::fs::read_to_string(commands)
                    .unwrap()
                    .contains("container\", \"rm")
            );
        }
    }
    #[test]
    fn snapshot_admits_system_ustar_and_refuses_extensions_links_and_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        std::fs::write(dir.path().join(".github/workflows/ci.yml"), "on: push\n").unwrap();
        let archive = dir.path().join("source.tar");
        let build = |format: &str| {
            assert!(
                std::process::Command::new("tar")
                    .args(["--format", format, "--owner=0", "--group=0", "-cf"])
                    .arg(&archive)
                    .arg("-C")
                    .arg(dir.path())
                    .arg(".github/workflows/ci.yml")
                    .status()
                    .unwrap()
                    .success()
            );
            std::fs::read(&archive).unwrap()
        };
        let valid = build("ustar");
        verify_snapshot(&valid).unwrap();
        assert!(verify_snapshot(&build("pax")).is_err());
        for (name, kind) in [
            ("../escape.yml", b'0'),
            (".github/workflows/ci.yml", b'2'),
            ("/absolute.yml", b'0'),
        ] {
            let mut corrupted = valid.clone();
            corrupted[..100].fill(0);
            corrupted[..name.len()].copy_from_slice(name.as_bytes());
            corrupted[156] = kind;
            corrupted[148..156].fill(b' ');
            let sum: usize = corrupted[..512].iter().map(|b| usize::from(*b)).sum();
            corrupted[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            assert!(verify_snapshot(&corrupted).is_err(), "{name}/{kind}");
        }
        let mut checksum = valid.clone();
        checksum[0] ^= 1;
        assert!(verify_snapshot(&checksum).is_err());
        assert!(verify_snapshot(&valid[..512]).is_err());
    }
    #[test]
    fn immutable_timestamp_roundtrip_is_exact() {
        for offset in 1..100000_u64 {
            let timestamp = f64::from_bits(1790892332.0_f64.to_bits() + offset);
            let encoded = serde_json::to_string(&timestamp).unwrap();
            let decoded: f64 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(
                timestamp.to_bits(),
                decoded.to_bits(),
                "timestamp {encoded}"
            );
        }
    }
    #[test]
    fn imported_manifest_and_config_are_separate_from_docker_image_id() {
        let config_bytes = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{"Volumes":{}},"rootfs":{"type":"layers","diff_ids":[format!("sha256:{}","c".repeat(64))]}})).unwrap();
        let config = hash(&config_bytes);
        let manifest_bytes = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":config,"size":config_bytes.len(),"mediaType":"application/vnd.oci.image.config.v1+json"}})).unwrap();
        let manifest = hash(&manifest_bytes);
        let expected: Value = serde_json::from_slice(&config_bytes).unwrap();
        let observation = json!([{"Id":manifest,"Descriptor":{"digest":manifest,"mediaType":"application/vnd.oci.image.manifest.v1+json","size":manifest_bytes.len()},"RootFS":{"Type":"layers","Layers":expected["rootfs"]["diff_ids"]},"Config":{"Volumes":{}}}]);
        let verify = |v: &Value, m: &[u8], c: &[u8]| {
            verify_loaded_image(&serde_json::to_vec(v).unwrap(), &manifest, &config, m, c)
        };
        assert_eq!(
            verify(&observation, &manifest_bytes, &config_bytes).unwrap(),
            manifest
        );
        for field in ["manifest", "size", "mediaType", "layers", "volumes"] {
            let mut bad = observation.clone();
            match field {
                "manifest" => bad[0]["Descriptor"]["digest"] = json!(config),
                "size" => bad[0]["Descriptor"]["size"] = json!(0),
                "mediaType" => bad[0]["Descriptor"]["mediaType"] = json!("unknown"),
                "layers" => bad[0]["RootFS"]["Layers"] = json!([]),
                _ => bad[0]["Config"]["Volumes"] = json!({"/data":{}}),
            };
            assert!(verify(&bad, &manifest_bytes, &config_bytes).is_err());
        }
        assert!(verify(&observation, b"{}", &config_bytes).is_err());
        assert!(verify(&observation, &manifest_bytes, b"{}").is_err());
    }

    #[test]
    fn results_do_not_promote_unknown_or_foreign_jobs() {
        let data=b"{\"jobID\":\"lint\",\"job\":\"lint\",\"matrix\":{},\"jobResult\":\"success\"}\n{\"jobID\":\"mac\",\"job\":\"mac\",\"matrix\":{},\"msg\":\"Skipping unsupported platform macos-latest\"}\n";
        let jobs = parse_job_results(data).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|v| v.outcome == "unsupported"));
        assert!(parse_job_results(b"not JSON\n").is_err());
        let missing = parse_job_results(
            b"{\"jobID\":\"unknown\",\"job\":\"unknown\",\"matrix\":{},\"msg\":\"started\"}\n",
        )
        .unwrap();
        assert_eq!(missing[0].outcome, "incomplete");
    }
}
