//! The probe's source fixture, durable records, cleanup and cancellation watch.

use super::*;

pub(super) fn git(source: &Path, args: &[&str]) -> std::io::Result<Vec<u8>> {
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
pub(super) fn source_fixture(
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
pub(super) async fn record(
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
pub(super) async fn cleanup(
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
    let profile = crate::act_engine::frozen_limits(&current.intent)
        .map_err(|error| fail(error.to_string()))?;
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
        observe_engine(&raw, intent, owner, &identity, profile).map_err(|e| fail(e.to_string()))?
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
        let observed = observe_engine(&raw, intent, owner, &identity, profile)
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
pub(super) fn frame_marker(path: &Path) -> std::io::Result<bool> {
    let frames = bounded_file(path, OUTPUT + 65536)?;
    frame_marker_bytes(&frames)
}
pub(super) fn frame_marker_bytes(frames: &[u8]) -> std::io::Result<bool> {
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
pub(super) async fn watch_cancellation(
    registry: RegistryActor,
    engine: DockerEngine,
    intent: ActEngineIntent,
    observed: ActEngineObservation,
    artifacts: (PathBuf, PathBuf, bool),
    cancel: CancellationSource,
    stop: CancellationSource,
) -> std::io::Result<bool> {
    let (evidence, samples, cancel_case) = artifacts;
    let mut sampled = 0usize;
    let mut nested_seen = std::collections::BTreeSet::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(120) && !stop.token().is_cancelled() {
        if let Some(current) = record(&registry, &intent.run_id).await?
            && current.state == ActEngineState::Registered
            && current.execution.is_none()
            && let Some(token) = current.execution_claim
        {
            if let Err(error) = registry
                .act_registry(ActRegistryCommand::VerifyClaimed {
                    run: intent.run_id.clone(),
                    observed: observed.clone(),
                    token,
                })
                .await
            {
                retain(
                    &samples.join("observer-authorization-error.json"),
                    &serde_json::to_vec_pretty(
                        &json!({"source":"probe observer authorization; not runtime evidence","unix_seconds":at(),"observer_error":error.to_string()}),
                    )?,
                )?;
                return Ok(false);
            }
            if sampled < 240 && !stop.token().is_cancelled() {
                let sample = sample_resources(&engine, &observed.engine_id, start.elapsed()).await;
                retain(
                    &samples.join(format!("resources-{sampled:03}.json")),
                    &serde_json::to_vec_pretty(&sample)?,
                )?;
                sampled += 1;
            }
            if !stop.token().is_cancelled() {
                sample_nested(
                    &engine,
                    &observed.engine_id,
                    &samples,
                    &mut nested_seen,
                    start.elapsed(),
                    Duration::from_secs(120)
                        .saturating_sub(start.elapsed())
                        .min(Duration::from_secs(2)),
                )
                .await?;
            }
            if !cancel_case {
                async_engine::sleep(Duration::from_millis(500)).await;
                continue;
            }
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
