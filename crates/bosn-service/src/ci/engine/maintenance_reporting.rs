//! Persist a bounded latest snapshot before delivering a supervisor tick.
use super::MaintenanceTick;
use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};
use bosn_registry::cache_maintenance::{
    ActionMaintenanceOutcome, MaintenanceHelper, MaintenanceOutcome, MaintenanceSnapshot,
    ToolMaintenanceOutcome,
};

pub(super) async fn persist(
    registry: &RegistryActor,
    tick: &MaintenanceTick,
) -> Result<(), String> {
    let helper = tick.attempt.as_ref().ok().map(|attempt| MaintenanceHelper {
        nonce: attempt.nonce.clone(),
        container_id: attempt.container_id.clone(),
        cleanup_error: attempt.cleanup.as_ref().err().map(|s| diagnostic(s)),
    });
    let outcome = match &tick.attempt {
        Err(error) => MaintenanceOutcome::Unknown {
            diagnostic: diagnostic(error),
        },
        Ok(attempt) => match &attempt.outcome {
            Err(error) => MaintenanceOutcome::Unknown {
                diagnostic: diagnostic(error),
            },
            Ok(attempt) => {
                let report = &attempt.report;
                // A partial root report cannot establish total reclamation.
                let reclaimed = (!report.partial)
                    .then(|| {
                        report.namespaces.as_ref().and_then(|namespaces| {
                            namespaces.iter().try_fold(0u64, |sum, ns| {
                                sum.checked_add(ns.retention.as_ref()?.reclaimed_archive_bytes)
                            })
                        })
                    })
                    .flatten();
                MaintenanceOutcome::Observed {
                    exit_code: attempt.exit_code,
                    partial: report.partial,
                    budget_bytes: report.budget_bytes,
                    remaining_completed_bytes: report.remaining_completed_bytes,
                    protected_bytes: report.protected_bytes,
                    budget_met: report.budget_met,
                    reclaimed_archive_bytes: reclaimed,
                }
            }
        },
    };
    let recovery_error = match &tick.recovery {
        Err(error) => Some(diagnostic(error)),
        Ok(report) => report.deferred.as_ref().map(|s| diagnostic(s)),
    };
    let snapshot = MaintenanceSnapshot {
        schema_version: 1,
        observed_at: crate::ci::lifecycle::now_seconds(),
        helper,
        outcome,
        recovery_error,
        action_outcome: tick
            .attempt
            .as_ref()
            .ok()
            .and_then(|attempt| attempt.actions.as_ref())
            .map(disposable_outcome),
        image_outcome: tick
            .attempt
            .as_ref()
            .ok()
            .and_then(|attempt| attempt.images.as_ref())
            .map(disposable_outcome),
        archive_outcome: tick
            .attempt
            .as_ref()
            .ok()
            .and_then(|attempt| attempt.archives.as_ref())
            .map(disposable_outcome),
        tool_outcome: tick
            .attempt
            .as_ref()
            .ok()
            .and_then(|attempt| attempt.tools.as_ref())
            .map(|tools| match tools {
                Ok(None) => ToolMaintenanceOutcome::NotEnrolled,
                Ok(Some(stats)) => ToolMaintenanceOutcome::Observed {
                    stats: stats.clone(),
                },
                Err(error) => ToolMaintenanceOutcome::Held {
                    diagnostic: diagnostic(error),
                },
            }),
    };
    match registry
        .act_registry(ActRegistryCommand::MaintenanceRecord(snapshot))
        .await
        .map_err(|e| e.to_string())?
    {
        ActRegistryReply::Committed => Ok(()),
        _ => Err("maintenance snapshot registry reply mismatch".into()),
    }
}
fn disposable_outcome(
    result: &Result<super::action_cache::ActionMaintenanceStats, String>,
) -> ActionMaintenanceOutcome {
    match result {
        Ok(stats) => ActionMaintenanceOutcome::Observed {
            stats: stats.clone(),
        },
        Err(error) => ActionMaintenanceOutcome::Held {
            diagnostic: diagnostic(error),
        },
    }
}
fn diagnostic(value: &str) -> String {
    value.chars().take(128).collect()
}
