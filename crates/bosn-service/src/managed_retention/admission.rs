//! Job admission must remain held throughout idle retirement and reclamation.

use std::path::PathBuf;

use kernal_api::async_engine;

use super::*;

pub(crate) async fn run_for_jobs(
    jobs: &crate::jobs::Jobs,
    executors: &crate::SetupExecutors,
    registry: &crate::RegistryActor,
    policy: RetentionPolicy,
    apply: bool,
) -> Result<ManagedRetentionOutcome, String> {
    let busy = jobs
        .load()
        .values()
        .any(|(queued, running)| *queued > 0 || *running > 0);
    let (outcome, admission) = run_when_idle(
        busy,
        executors.state_dir.clone(),
        policy,
        apply,
        Some(registry),
    )
    .await?;
    if !outcome.deletion_receipts.is_empty() {
        let receipts = outcome.deletion_receipts.clone();
        registry
            .prune_deleted_ownership(receipts, admission)
            .await
            .map_err(|error| {
                format!("physical cleanup succeeded but registry cleanup failed: {error}")
            })?;
    }
    Ok(outcome)
}

/// Invoked inside the job actor, which cannot admit another job until we return.
pub(crate) async fn run_when_idle(
    busy: bool,
    state_dir: Option<PathBuf>,
    policy: RetentionPolicy,
    apply: bool,
    registry: Option<&crate::RegistryActor>,
) -> Result<(ManagedRetentionOutcome, gate::Guard), String> {
    // Blocking a busy actor would also block log draining and job completion.
    if busy {
        return Err("retention deferred: jobs are queued or running".into());
    }
    let state_dir = state_dir.ok_or("retention state directory unavailable")?;
    let deadline = std::time::Instant::now() + budget::PASS_LIMIT;
    let admission = async_engine::launch_blocking(gate::retention)
        .await
        .map_err(|_| "retention admission worker stopped".to_owned())??;
    let (admission, recovery) = if apply && let Some(registry) = registry {
        registry.recover_image_intents(admission, deadline).await?
    } else {
        (admission, image_recovery::Report::default())
    };
    async_engine::launch_blocking(move || {
        let _budget =
            budget::Guard::start(deadline.saturating_duration_since(std::time::Instant::now()));
        let engine = DockerEngine::docker();
        let mut outcome = managed_retention_with_idle(&engine, &state_dir, policy, apply);
        if outcome.summary.refused.is_none() {
            peers::sweep(&engine, &state_dir, policy, apply, &mut outcome);
        }
        outcome.summary.held_total += recovery.held_count;
        for reason in recovery.held {
            details::push(&mut outcome.summary.held, reason);
        }
        Ok((outcome, admission))
    })
    .await
    .map_err(|_| "retention worker stopped".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_admission_refuses_before_any_engine_or_registry_access() {
        let runtime = async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .run(run_when_idle(
                true,
                None,
                RetentionPolicy::default(),
                true,
                None,
            ))
            .err()
            .unwrap();
        assert_eq!(error, "retention deferred: jobs are queued or running");
    }
}
