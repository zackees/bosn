//! Executing Act inside a registered, claimed private engine.

use super::*;

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
pub(super) async fn verify_registry(
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
pub(super) async fn control(
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
pub(super) async fn transfer_input(
    registry: &RegistryActor,
    engine: &DockerEngine,
    ownership: (&ActRuntimeRequest<'_>, &str),
    source: &Path,
    destination: &str,
    started: Instant,
    cancellation: &async_engine::CancellationToken,
) -> std::io::Result<()> {
    let (request, token) = ownership;
    if !matches!(destination, "act.oci.tar" | "source.tar" | "event.json") {
        return Err(error("unknown private input destination"));
    }
    let remaining = request
        .execution_deadline
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| error("Act lifecycle deadline exceeded"))?;
    if cancellation.is_cancelled() {
        return Err(error("Act runtime cancelled"));
    }
    verify_registry(registry, request, token).await?;
    let source = std::fs::File::open(source)?;
    let output = engine
        .with_args([
            "exec".to_owned(),
            "-i".to_owned(),
            request.observed.engine_id.clone(),
            "sh".to_owned(),
            "-c".to_owned(),
            "umask 077; cat > \"$1\"".to_owned(),
            "bosn-input".to_owned(),
            format!("{INPUTS}/{destination}"),
        ])
        .capture_with_stdin_file_async(
            source,
            request.archive_ceiling,
            RunOptions::bounded(remaining.min(Duration::from_secs(30)), 2 << 20),
            Some(cancellation),
        )
        .await
        .map_err(|e| error(e.to_string()))?;
    if output.exit_code != 0 {
        return Err(error(format!(
            "owned engine input transfer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    verify_registry(registry, request, token).await?;
    Ok(())
}
pub(super) fn verify_ready_engine(bytes: &[u8]) -> std::io::Result<()> {
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
pub(super) async fn wait_ready_engine(
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
pub(super) fn driver_arguments(
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
pub(super) struct RuntimeCleanupGuard {
    pub(super) registry: RegistryActor,
    pub(super) engine: DockerEngine,
    pub(super) run: String,
    pub(super) intent: ActEngineIntent,
    pub(super) observed: ActEngineObservation,
    pub(super) token: String,
    pub(super) active: bool,
}
impl RuntimeCleanupGuard {
    pub(super) fn new(
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
            transfer_input(
                registry,
                engine,
                (&request, &token),
                &evidence.join(file),
                file,
                started,
                cancellation,
            )
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
        let load_output = control(vec![
            "exec".into(),
            id.clone(),
            "docker".into(),
            "load".into(),
            "--input".into(),
            format!("{INPUTS}/act.oci.tar"),
        ])
        .await?;
        private_file(&evidence.join("image-load.stdout"), &load_output)?;
        let loaded = control(vec![
            "exec".into(),
            id.clone(),
            "docker".into(),
            "image".into(),
            "inspect".into(),
            request.package.manifest_digest.clone(),
        ])
        .await?;
        private_file(&evidence.join("act-image-inspect.json"), &loaded)?;
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
            request.package.runner_manifest_digest.clone(),
        ])
        .await?;
        private_file(&evidence.join("runner-image-inspect.json"), &runner_loaded)?;
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
        if output.exit_code != 0 {
            #[cfg(all(test, target_os = "linux"))]
            if std::env::var_os("BOSN_ACT_PROBE_INPUT_DIR").is_some() {
                let remaining = request.execution_deadline.saturating_sub(started.elapsed());
                if let Err(diagnostic) = crate::tests::act_live_probe::diagnose_nested_failure(
                    registry,
                    engine,
                    request.intent,
                    request.observed,
                    &token,
                    &evidence,
                    remaining,
                )
                .await
                {
                    let _ = private_file(
                        &evidence.join("probe-diagnostic-error.txt"),
                        diagnostic.to_string().as_bytes(),
                    );
                }
            }
            // Retain complete structured stdout when Act reports failed jobs.
            // This evidence never overrides the driver's failing exit status.
            report.jobs = parse_job_results(&output.stdout).unwrap_or_default();
            let detail = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            return Err(error(format!(
                "Act driver exited {}: {}",
                output.exit_code,
                String::from_utf8_lossy(&detail[detail.len().saturating_sub(2048)..])
            )));
        }
        let mut bytes = output.stdout;
        bytes.extend_from_slice(&output.stderr);
        report.jobs = parse_job_results(&bytes)?;
        if report.jobs.is_empty()
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
