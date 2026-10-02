//! Verdicts: folding an engine outcome and a parsed tree into a run
//! conclusion, and the agent-facing failure report. Pure functions.

use serde_json::{Value, json};

use super::{
    lifecycle::{CleanupEnd, EngineReport, ExecutionEnd},
    model::{ItemConclusion, RunTree},
    wire::{Conclusion, RunRecord, SCHEMA_VERSION},
};

/// Fold the engine report into the tree and return the run conclusion.
/// Partial coverage and failed cleanup are never a pass.
pub fn conclude(
    outcome: &Result<EngineReport, String>,
    tree: &mut RunTree,
) -> (Conclusion, Option<String>) {
    let report = match outcome {
        Ok(report) => report,
        Err(error) => {
            tree.cancel_unfinished();
            return (Conclusion::Error, Some(error.clone()));
        }
    };
    let (mut conclusion, mut reason) = match &report.execution {
        ExecutionEnd::Exited(code) => {
            tree.settle_finished();
            let failed = tree
                .jobs()
                .any(|j| j.conclusion == Some(ItemConclusion::Failure));
            if *code != 0 || failed {
                (Conclusion::Failure, None)
            } else if !tree.unsupported_jobs().is_empty() {
                (
                    Conclusion::Incomplete,
                    Some("some jobs need runners bosn cannot supervise".into()),
                )
            } else if !tree
                .jobs()
                .any(|j| j.conclusion == Some(ItemConclusion::Success))
            {
                (
                    Conclusion::Incomplete,
                    Some("no job ran for this trigger".into()),
                )
            } else {
                (Conclusion::Success, None)
            }
        }
        ExecutionEnd::TimedOut => {
            tree.cancel_unfinished();
            (
                Conclusion::TimedOut,
                Some("the run exceeded its timeout".into()),
            )
        }
        ExecutionEnd::Cancelled => {
            tree.cancel_unfinished();
            (Conclusion::Cancelled, None)
        }
        ExecutionEnd::EngineFailed(error) => {
            tree.cancel_unfinished();
            (Conclusion::Error, Some(error.clone()))
        }
    };
    if let CleanupEnd::Failed(error) = &report.cleanup {
        if matches!(conclusion, Conclusion::Success | Conclusion::Incomplete) {
            conclusion = Conclusion::Error;
        }
        let cleanup = format!("engine cleanup failed: {error}");
        reason = Some(match reason {
            Some(first) => format!("{first}; {cleanup}"),
            None => cleanup,
        });
    }
    (conclusion, reason)
}

/// The `ci report --json` agent contract. `tail(job, section)` returns the
/// last lines of that one section.
pub fn report(record: &RunRecord, tail: impl FnOnce(&str, &str) -> Vec<String>) -> Value {
    let tree = &record.tree;
    let failure = tree.first_failure().map(|(job, section)| {
        let key = section.map(|s| format!("{}:{}", s.stage, s.id));
        json!({
            "job": job.key,
            "job_name": job.name,
            "section": key,
            "step": section.map(|s| s.name.clone()),
            "exit_code": section.and_then(|s| s.exit_code),
            "tail": key.as_deref().map(|k| tail(&job.key, k)).unwrap_or_default(),
        })
    });
    let count = |c: ItemConclusion| tree.jobs().filter(|j| j.conclusion == Some(c)).count();
    json!({
        "schema_version": SCHEMA_VERSION,
        "run": record.id,
        "state": record.state,
        "conclusion": record.conclusion,
        "exit_code": record.conclusion.map(Conclusion::exit_code),
        "reason": record.reason,
        "sha": record.sha,
        "dirty": record.dirty,
        "workflow": record.workflow,
        "trigger": record.trigger,
        "mode": record.mode,
        "actor": record.actor,
        "cleanup": record.cleanup,
        "first_failure": failure,
        "jobs": {
            "succeeded": count(ItemConclusion::Success),
            "failed": count(ItemConclusion::Failure),
            "cancelled": count(ItemConclusion::Cancelled),
            "skipped": tree.skipped_jobs(),
            "unsupported": tree.unsupported_jobs(),
        },
        "coverage_complete": tree.unsupported_jobs().is_empty(),
        "ui_url": Value::Null,
        "logs_command": format!("bosn ci logs {}", record.id),
    })
}
