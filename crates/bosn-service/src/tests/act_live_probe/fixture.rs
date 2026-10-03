//! The probe's durable registry records, verified cleanup and Docker observations.

use super::*;

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
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
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
pub(super) async fn docker(engine: &DockerEngine, args: Vec<String>) -> std::io::Result<Vec<u8>> {
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
