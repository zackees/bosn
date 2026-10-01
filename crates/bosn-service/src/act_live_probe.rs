//! Explicit, ignored real-Docker probe. Inputs and all evidence are retained.
//! This is implementation evidence, not a public API or fleet/native CI proof.
#![cfg(target_os = "linux")]

use super::*;
use crate::act_archive::ActArchiveBlob;
use crate::act_engine::{
    ActEngineLimits, VerifiedEngineManifest, create_owned_engine_from_manifest, observe_engine,
    observe_engine_image_from_manifest, remove_owned_engine,
};
use crate::act_image::{ActBaseLayer, ActImagePackage, package_act_image};
use crate::act_registry::{ActRegistryCommand, ActRegistryReply};
use crate::act_runtime::{ActRuntimeRequest, run_registered_act};
use bosn_registry::act::{
    ActEngineIntent, ActEngineObservation, ActEngineRemovalProof, ActEngineState, ActRunOutcome,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RUNNER: &str = "sha256:be3b065b90a7a029ea30aa8ce897a62bfc8bd4d6698951b2527e1f11ba70cc6c";
const RUNNER_CONFIG: &str =
    "sha256:0385872e2126185df5bef04f9b47c04d81b59d48b8d98ee95bac0928adc08c85";
const ACT_BINARY: &str = "sha256:a76aa7627c633f5e9e9b06407d6eb1069213b1ee984599381b84ad4e7bd894f0";
const ENGINE: &str = "sha256:6acc6aaf783ac1c1100822e542534c3dab3f1d38782760b0bdcb688280574d9e";
const ENGINE_CONFIG: &str =
    "sha256:8cdb6d492106752d557cda50e628b88e7bb303a7eaea91a10bdf672b95ad4f52";
const OUTPUT: usize = 8 << 20;
const MARKER: &[u8] = b"BOSN_REAL_CANCEL_STARTED";

fn fail(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}
fn at() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(bytes))
}
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}
fn retain(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn bounded_file(path: &Path, ceiling: usize) -> std::io::Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > ceiling as u64 {
        return Err(fail("probe input is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(ceiling as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > ceiling {
        return Err(fail("probe input grew beyond its bound"));
    }
    Ok(bytes)
}
const LAYER_BYTES: u64 = 1 << 30;
fn read_layers(input: &Path, layers: &[ActBaseLayer]) -> std::io::Result<Vec<Vec<u8>>> {
    let total = layers
        .iter()
        .try_fold(0u64, |total, layer| total.checked_add(layer.size))
        .ok_or_else(|| fail("aggregate layer byte ceiling overflow"))?;
    if total > LAYER_BYTES {
        return Err(fail("aggregate layer byte ceiling exceeded"));
    }
    layers
        .iter()
        .map(|layer| {
            let ceiling = usize::try_from(layer.size)
                .map_err(|_| fail("layer size exceeds address space"))?;
            let bytes = bounded_file(
                &input.join("blobs/sha256").join(&layer.digest[7..]),
                ceiling,
            )?;
            if bytes.len() as u64 != layer.size || digest(&bytes) != layer.digest {
                return Err(fail("layer digest or size mismatch"));
            }
            Ok(bytes)
        })
        .collect()
}
struct ProbeObserver {
    stop: CancellationSource,
    task: Option<async_engine::Task<std::io::Result<bool>>>,
}
impl ProbeObserver {
    fn new() -> Self {
        Self {
            stop: CancellationSource::new(),
            task: None,
        }
    }
    async fn stop_and_join(&mut self) -> std::io::Result<bool> {
        self.stop.cancel();
        match self.task.take() {
            Some(task) => task.await.map_err(|e| fail(e.to_string()))?,
            None => Ok(false),
        }
    }
}
impl Drop for ProbeObserver {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
async fn random_uuid() -> std::io::Result<String> {
    let entropy = kernal_api::random::SecureRandom::new(1, Duration::from_secs(5))
        .map_err(|e| fail(e.to_string()))?;
    Ok(uuid(
        &entropy.bytes(16).await.map_err(|e| fail(e.to_string()))?,
    ))
}
fn limits() -> ActEngineLimits {
    ActEngineLimits {
        memory_bytes: 16 << 30,
        storage_bytes: 12 << 30,
        nano_cpus: 2_000_000_000,
        pids: 1024,
    }
}
async fn docker(engine: &DockerEngine, args: Vec<String>) -> std::io::Result<Vec<u8>> {
    let result = engine
        .with_args(args)
        .capture_async(RunOptions::bounded(Duration::from_secs(10), 1 << 20))
        .await
        .map_err(|e| fail(e.to_string()))?;
    if result.exit_code != 0 {
        return Err(fail(format!(
            "probe Docker observation refused: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(result.stdout)
}
fn git(source: &Path, args: &[&str]) -> std::io::Result<Vec<u8>> {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .current_dir(source)
        .args([
            "-c",
            "user.name=Bosn probe",
            "-c",
            "user.email=bosn-probe@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(fail(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}
fn source_fixture(
    root: &Path,
    cancel: bool,
) -> std::io::Result<(PathBuf, Vec<u8>, Vec<u8>, String)> {
    let source = root.join(if cancel {
        "cancel-source"
    } else {
        "success-source"
    });
    private_dir(&source)?;
    private_dir(&source.join(".github"))?;
    private_dir(&source.join(".github/workflows"))?;
    let shell = if cancel {
        "printf 'BOSN_REAL_CANCEL_STARTED\\n'\nsleep 300"
    } else {
        "printf 'BOSN_REAL_SUCCESS\\n'\nsleep 2"
    };
    let workflow = format!(
        "name: Bosn real probe\non: push\njobs:\n  probe:\n    runs-on: ubuntu-latest\n    steps:\n      - name: Verify real event and shell\n        shell: bash\n        run: |\n          test \"$GITHUB_SHA\" = \"${{{{ github.event.after }}}}\"\n          test \"${{{{ github.event.repository.full_name }}}}\" = \"bosn/act-local-probe\"\n          {}\n",
        shell.replace('\n', "\n          ")
    );
    retain(
        &source.join(".github/workflows/probe.yml"),
        workflow.as_bytes(),
    )?;
    git(&source, &["init", "--initial-branch=main"])?;
    git(&source, &["add", ".github/workflows/probe.yml"])?;
    git(
        &source,
        &["commit", "-m", "immutable local Act probe fixture"],
    )?;
    let sha = String::from_utf8(git(&source, &["rev-parse", "HEAD"])?)
        .map_err(|e| fail(e.to_string()))?
        .trim()
        .to_owned();
    let archive_path = root.join(if cancel {
        "cancel-source.tar"
    } else {
        "success-source.tar"
    });
    let output = Command::new("tar")
        .args([
            "--format=ustar",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--mtime=@0",
            "-cf",
        ])
        .arg(&archive_path)
        .arg("-C")
        .arg(&source)
        .arg(".github/workflows/probe.yml")
        .output()?;
    if !output.status.success() {
        return Err(fail(String::from_utf8_lossy(&output.stderr)));
    }
    let snapshot = bounded_file(&archive_path, 1 << 20)?;
    let payload = serde_json::to_vec(
        &json!({"ref":"refs/heads/main","before":"0".repeat(40),"after":sha,"head_commit":{"id":sha},"repository":{"id":1,"name":"act-local-probe","full_name":"bosn/act-local-probe","default_branch":"main","html_url":"https://github.com/bosn/act-local-probe","clone_url":"https://github.com/bosn/act-local-probe.git","owner":{"login":"bosn","name":"bosn","type":"User"}},"sender":{"login":"bosn"}}),
    )?;
    Ok((source, snapshot, payload, sha))
}
async fn record(
    registry: &RegistryActor,
    run: &str,
) -> std::io::Result<Option<bosn_registry::act::ActEngineRecord>> {
    let ActRegistryReply::Recovery(page) = registry
        .act_registry(ActRegistryCommand::Pending {
            after_run_id: None,
            limit: 16,
        })
        .await
        .map_err(|e| fail(e.to_string()))?
    else {
        return Err(fail("probe recovery reply missing"));
    };
    if page.next_run_id.is_some() {
        return Err(fail("unexpected recovery page overflow in two-run probe"));
    }
    Ok(page.items.into_iter().find(|r| r.intent.run_id == run))
}
/// Called only after this probe's runtime future has terminated. A persisted
/// live claim alone is never grounds to interrupt another execution owner.
async fn cleanup(
    registry: &RegistryActor,
    engine: &DockerEngine,
    intent: &ActEngineIntent,
    owner: &str,
    proof: &VerifiedEngineManifest,
) -> std::io::Result<()> {
    let Some(current) = record(registry, &intent.run_id).await? else {
        return Ok(());
    };
    if current.intent != *intent {
        return Err(fail("probe cleanup immutable intent changed"));
    }
    let image = docker(
        engine,
        vec![
            "image".into(),
            "inspect".into(),
            format!("docker.io/library/docker@{ENGINE}"),
        ],
    )
    .await?;
    let identity = observe_engine_image_from_manifest(&image, intent, proof)
        .map_err(|e| fail(e.to_string()))?;
    let observed = if let Some(id) = &current.engine_id {
        let raw = docker(
            engine,
            vec!["container".into(), "inspect".into(), id.clone()],
        )
        .await?;
        observe_engine(&raw, intent, owner, &identity, limits()).map_err(|e| fail(e.to_string()))?
    } else {
        let ids = docker(
            engine,
            vec![
                "container".into(),
                "ls".into(),
                "--all".into(),
                "--no-trunc".into(),
                "--filter".into(),
                format!("name=^/{}$", intent.engine_name()),
                "--format".into(),
                "{{.ID}}".into(),
            ],
        )
        .await?;
        let ids = std::str::from_utf8(&ids)
            .map_err(|e| fail(e.to_string()))?
            .split_whitespace()
            .collect::<Vec<_>>();
        if ids.len() > 1 {
            return Err(fail("ambiguous intended engine name"));
        }
        if ids.is_empty() {
            if current.state == ActEngineState::Pending {
                registry
                    .act_registry(ActRegistryCommand::Cleanup {
                        run: intent.run_id.clone(),
                        outcome: ActRunOutcome::Interrupted,
                        at: at(),
                    })
                    .await
                    .map_err(|e| fail(e.to_string()))?;
            }
            registry
                .act_registry(ActRegistryCommand::Finalize {
                    run: intent.run_id.clone(),
                    proof: ActEngineRemovalProof {
                        name: intent.engine_name(),
                        engine_id: None,
                    },
                    at: at(),
                })
                .await
                .map_err(|e| fail(e.to_string()))?;
            return Ok(());
        }
        let raw = docker(
            engine,
            vec!["container".into(), "inspect".into(), ids[0].into()],
        )
        .await?;
        let observed = observe_engine(&raw, intent, owner, &identity, limits())
            .map_err(|e| fail(e.to_string()))?;
        if current.state == ActEngineState::Pending {
            registry
                .act_registry(ActRegistryCommand::Cleanup {
                    run: intent.run_id.clone(),
                    outcome: ActRunOutcome::Interrupted,
                    at: at(),
                })
                .await
                .map_err(|e| fail(e.to_string()))?;
        }
        registry
            .act_registry(ActRegistryCommand::Recover {
                run: intent.run_id.clone(),
                observed: observed.clone(),
                at: at(),
            })
            .await
            .map_err(|e| fail(e.to_string()))?;
        observed
    };
    if current.state == ActEngineState::Registered {
        let command = match current.execution_claim {
            Some(token) => ActRegistryCommand::CleanupClaimed {
                run: intent.run_id.clone(),
                token,
                outcome: ActRunOutcome::Interrupted,
                at: at(),
            },
            None => ActRegistryCommand::Cleanup {
                run: intent.run_id.clone(),
                outcome: ActRunOutcome::Interrupted,
                at: at(),
            },
        };
        // A detached owner guard may have won this same-owner transition.
        let _ = registry.act_registry(command).await;
    }
    remove_owned_engine(registry, engine, &intent.run_id, observed, at())
        .await
        .map_err(|e| fail(e.to_string()))
}
fn frame_marker(path: &Path) -> std::io::Result<bool> {
    let frames = bounded_file(path, OUTPUT + 65536)?;
    frame_marker_bytes(&frames)
}
fn frame_marker_bytes(frames: &[u8]) -> std::io::Result<bool> {
    let mut offset = 0;
    let mut bytes = Vec::new();
    while offset + 5 <= frames.len() {
        let size = u32::from_le_bytes(frames[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if !matches!(frames[offset], 1 | 2) || size > OUTPUT {
            return Err(fail("invalid retained output frame"));
        }
        if offset + 5 + size > frames.len() {
            break;
        }
        bytes.extend_from_slice(&frames[offset + 5..offset + 5 + size]);
        offset += 5 + size;
    }
    Ok(bytes.windows(MARKER.len()).any(|w| w == MARKER))
}
async fn watch_cancellation(
    registry: RegistryActor,
    engine: DockerEngine,
    intent: ActEngineIntent,
    observed: ActEngineObservation,
    artifacts: (PathBuf, PathBuf),
    cancel: CancellationSource,
    stop: CancellationSource,
) -> std::io::Result<bool> {
    let (evidence, samples) = artifacts;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(120) && !stop.token().is_cancelled() {
        if let Some(current) = record(&registry, &intent.run_id).await?
            && current.state == ActEngineState::Registered
            && current.execution.is_none()
            && let Some(token) = current.execution_claim
        {
            registry
                .act_registry(ActRegistryCommand::VerifyClaimed {
                    run: intent.run_id.clone(),
                    observed: observed.clone(),
                    token,
                })
                .await
                .map_err(|e| fail(e.to_string()))?;
            if let Ok(raw) = docker(
                &engine,
                vec![
                    "exec".into(),
                    observed.engine_id.clone(),
                    "docker".into(),
                    "container".into(),
                    "ls".into(),
                    "--no-trunc".into(),
                    "--format".into(),
                    "{{json .}}".into(),
                ],
            )
            .await
            {
                if stop.token().is_cancelled() {
                    return Ok(false);
                }
                let containers = raw
                    .split(|b| *b == b'\n')
                    .filter(|b| !b.is_empty())
                    .map(serde_json::from_slice::<Value>)
                    .collect::<Result<Vec<_>, _>>()?;
                let driver = containers.iter().any(|v| {
                    v["Names"]
                        .as_str()
                        .is_some_and(|s| s == format!("bosn-act-driver-{}", intent.run_id))
                });
                let log = evidence.join(&intent.run_id).join("output.frames");
                if driver && containers.len() >= 2 && log.exists() && frame_marker(&log)? {
                    retain(&samples.join("nested-running.jsonl"), &raw)?;
                    if let Ok(peak) = docker(
                        &engine,
                        vec![
                            "exec".into(),
                            observed.engine_id.clone(),
                            "cat".into(),
                            "/sys/fs/cgroup/memory.peak".into(),
                        ],
                    )
                    .await
                    {
                        retain(&samples.join("memory.peak"), &peak)?;
                    }
                    if let Ok(stats) = docker(
                        &engine,
                        vec![
                            "stats".into(),
                            "--no-stream".into(),
                            "--format".into(),
                            "{{json .}}".into(),
                            observed.engine_id.clone(),
                        ],
                    )
                    .await
                    {
                        retain(&samples.join("docker-stats.json"), &stats)?;
                    }
                    cancel.cancel();
                    return Ok(true);
                }
            }
        }
        async_engine::sleep(Duration::from_millis(250)).await;
    }
    Ok(false)
}
async fn probe_case(
    registry: &RegistryActor,
    engine: &DockerEngine,
    images: (&ActImagePackage, &VerifiedEngineManifest),
    blobs: &[ActArchiveBlob<'_>],
    root: &Path,
    owner: &str,
    cancel_case: bool,
) -> std::io::Result<Value> {
    let (package, engine_manifest) = images;
    let (source, snapshot, payload, sha) = source_fixture(root, cancel_case)?;
    let run = random_uuid().await?;
    let intent = ActEngineIntent {
        run_id: run.clone(),
        workspace: source.to_string_lossy().into_owned(),
        candidate_sha: sha,
        payload_sha256: digest(&payload)[7..].into(),
        snapshot_sha256: digest(&snapshot)[7..].into(),
        act_version: "0.2.88".into(),
        act_image_digest: package.manifest_digest.clone(),
        engine_image_digest: ENGINE.into(),
        runner_image_digest: RUNNER.into(),
        created_at: at(),
    };
    retain(
        &root.join(format!("{run}-intent.json")),
        &serde_json::to_vec_pretty(&intent)?,
    )?;
    let cancellation = CancellationSource::new();
    let mut observer = ProbeObserver::new();
    let result = match async_engine::timeout(Duration::from_secs(120), async {
        let observed = create_owned_engine_from_manifest(
            registry,
            engine,
            intent.clone(),
            owner,
            engine_manifest,
            limits(),
            at(),
        )
        .await
        .map_err(|e| fail(e.to_string()))?;
        let raw = docker(
            engine,
            vec![
                "container".into(),
                "inspect".into(),
                observed.engine_id.clone(),
            ],
        )
        .await?;
        let host_image = docker(
            engine,
            vec![
                "image".into(),
                "inspect".into(),
                format!("docker.io/library/docker@{ENGINE}"),
            ],
        )
        .await?;
        let image = observe_engine_image_from_manifest(&host_image, &intent, engine_manifest)
            .map_err(|e| fail(e.to_string()))?;
        let confirmed = observe_engine(&raw, &intent, owner, &image, limits())
            .map_err(|e| fail(e.to_string()))?;
        if confirmed != observed {
            return Err(fail("registered real engine observation changed"));
        }
        retain(&root.join(format!("{run}-engine-inspect.json")), &raw)?;
        let evidence = root.join("evidence");
        let samples = root.join(format!("{run}-samples"));
        private_dir(&samples)?;
        observer.task = if cancel_case {
            Some(async_engine::launch(watch_cancellation(
                registry.clone(),
                engine.clone(),
                intent.clone(),
                observed.clone(),
                (evidence.clone(), samples),
                cancellation.clone(),
                observer.stop.clone(),
            )))
        } else {
            None
        };
        run_registered_act(
            registry,
            engine,
            ActRuntimeRequest {
                intent: &intent,
                observed: &observed,
                package,
                base_blobs: blobs,
                source_archive: &snapshot,
                event_payload: &payload,
                event_name: "push",
                evidence_root: &evidence,
                archive_ceiling: 2 << 30,
                execution_deadline: Duration::from_secs(120),
                output_ceiling: OUTPUT,
            },
            &cancellation.token(),
        )
        .await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(fail(
            "whole real probe operation exceeded 120-second deadline",
        )),
    };
    cancellation.cancel();
    let joined = observer.stop_and_join().await;
    let result: std::io::Result<Value> = (|| {
        let nested_cancelled = joined?;
        let report = result?;
        if !report.engine_removed
            || report.execution_success == cancel_case
            || (cancel_case && (!nested_cancelled || report.outcome != "cancelled"))
        {
            return Err(fail(format!(
                "real probe expectations failed: {report:?}; nested cancellation={nested_cancelled}"
            )));
        }
        let evidence = root.join("evidence");
        let frames = bounded_file(&evidence.join(&run).join("output.frames"), OUTPUT + 65536)?;
        if report.output_sha256.as_deref() != Some(digest(&frames).as_str()) {
            return Err(fail("durable output frame identity mismatch"));
        }
        let receipt: Value = serde_json::from_slice(&bounded_file(
            &evidence.join(&run).join("result.json"),
            1 << 20,
        )?)?;
        if receipt["engine_removed"] != true {
            return Err(fail("durable result has no verified absence"));
        }
        Ok(
            json!({"cancel_case":cancel_case,"nested_cancellation_observed":nested_cancelled,"report":report}),
        )
    })();
    let cleanup_result = cleanup(registry, engine, &intent, owner, engine_manifest).await;
    let summary = match (&result, &cleanup_result) {
        (Ok(value), Ok(())) => value.clone(),
        _ => {
            json!({"run_id":run,"error":result.as_ref().err().map(ToString::to_string),"cleanup_error":cleanup_result.as_ref().err().map(ToString::to_string)})
        }
    };
    retain(
        &root.join(format!("{run}-probe-result.json")),
        &serde_json::to_vec_pretty(&summary)?,
    )?;
    cleanup_result?;
    result
}

#[test]
#[ignore = "explicit owned Linux Docker probe; requires reviewed pinned inputs and root authorization"]
fn live_pinned_act_success_and_cancellation_remove_private_nested_engines() {
    let runtime = async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.run(async {
        let input = PathBuf::from(std::env::var_os("BOSN_ACT_PROBE_INPUT_DIR").expect("set explicit owned BOSN_ACT_PROBE_INPUT_DIR")).canonicalize().unwrap();
        assert!(input.starts_with("/tmp") && input.is_dir(), "probe inputs must be retained in owned /tmp");
        assert_eq!(std::fs::metadata(&input).unwrap().permissions().mode() & 0o077, 0, "probe input directory must be private");
        let manifest = bounded_file(&input.join("runner-manifest.json"), 1<<20).unwrap();
        let config = bounded_file(&input.join("runner-config.json"), 1<<20).unwrap();
        let binary = bounded_file(&input.join("act"), 64<<20).unwrap();
        let package = package_act_image(&manifest, RUNNER, &config, RUNNER_CONFIG, &binary, ACT_BINARY, "0.2.88").unwrap();
        let engine_manifest = VerifiedEngineManifest::verify(&bounded_file(&input.join("engine-manifest.json"),1<<20).unwrap(), ENGINE, &bounded_file(&input.join("engine-config.json"),1<<20).unwrap(), ENGINE_CONFIG).unwrap();
        let owned_blobs = read_layers(&input, &package.base_layers).unwrap();
        let blobs = package.base_layers.iter().zip(&owned_blobs).map(|(layer,bytes)| ActArchiveBlob { digest: &layer.digest, bytes }).collect::<Vec<_>>();
        let root = input.join(format!("probe-{}", random_uuid().await.unwrap())); private_dir(&root).unwrap(); private_dir(&root.join("evidence")).unwrap();
        let owner = random_uuid().await.unwrap();
        let db = root.join("registry.sqlite3");
        let registry = Registry::create_writer(&db, &owner).unwrap();
        let (sender, receiver) = async_engine::channel(16);
        let actor = RegistryActor { sender };
        let task = async_engine::launch(registry_actor(registry, receiver, None));
        let engine = DockerEngine::docker();
        let mut results = Vec::new();
        let mut error = None;
        for cancel in [false,true] { match probe_case(&actor, &engine, (&package, &engine_manifest), &blobs, &root, &owner, cancel).await { Ok(result) => results.push(result), Err(failure) => { error = Some(failure.to_string()); break; } } }
        actor.stop().await; task.await.unwrap();
        retain(&root.join("probe-summary.json"), &serde_json::to_vec_pretty(&json!({"schema_version":1,"scope":"two real Linux shell probes; no public API, fleet graph or native parity proof","results":results,"error":error})).unwrap()).unwrap();
        eprintln!("retained real Act probe evidence: {}", root.display());
        assert!(error.is_none(), "real Act probe failed; retained evidence at {}: {error:?}", root.display());
        let registry = Registry::open_writer(&db).unwrap(); assert!(registry.pending_act_engines(None,16).unwrap().items.is_empty());
    });
}

#[test]
fn cancellation_marker_requires_valid_complete_frames() {
    let mut incomplete = vec![2, MARKER.len() as u8, 0, 0, 0];
    incomplete.extend_from_slice(&MARKER[..10]);
    assert!(!frame_marker_bytes(&incomplete).unwrap());
    let mut complete = vec![2, MARKER.len() as u8, 0, 0, 0];
    complete.extend_from_slice(MARKER);
    assert!(frame_marker_bytes(&complete).unwrap());
    complete[0] = 3;
    assert!(frame_marker_bytes(&complete).is_err());
    assert!(frame_marker_bytes(&[1, 1, 0, 128, 0]).is_err());
    assert!(!frame_marker_bytes(&[]).unwrap());
}

#[test]
fn aggregate_layer_limit_refuses_before_blob_io() {
    let layer = |size| ActBaseLayer {
        digest: format!("sha256:{}", "0".repeat(64)),
        size,
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
    };
    for layers in [
        vec![layer(LAYER_BYTES), layer(1)],
        vec![layer(u64::MAX), layer(1)],
    ] {
        let error = read_layers(Path::new("/nonexistent-probe-input"), &layers).unwrap_err();
        assert!(error.to_string().contains("aggregate layer byte ceiling"));
    }
}
#[test]
fn timed_operation_stops_and_joins_owned_observer() {
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observed_exit = exited.clone();
            let mut observer = ProbeObserver::new();
            let stop = observer.stop.clone();
            observer.task = Some(async_engine::launch(async move {
                while !stop.token().is_cancelled() {
                    async_engine::sleep(Duration::from_millis(1)).await;
                }
                async_engine::sleep(Duration::from_millis(10)).await;
                observed_exit.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(false)
            }));
            assert!(
                async_engine::timeout(
                    Duration::from_millis(5),
                    async_engine::sleep(Duration::from_secs(1))
                )
                .await
                .is_err()
            );
            assert!(!exited.load(std::sync::atomic::Ordering::SeqCst));
            assert!(!observer.stop_and_join().await.unwrap());
            assert!(exited.load(std::sync::atomic::Ordering::SeqCst));
        });
}
