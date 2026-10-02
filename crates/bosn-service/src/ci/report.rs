//! Verdicts: folding an engine outcome and a parsed tree into a run
//! conclusion, and the agent-facing failure report. Pure functions.

use super::{
    lifecycle::{CleanupEnd, EngineReport, ExecutionEnd},
    model::{ItemConclusion, RunTree},
    reply::{FailureReport, JobOutcomes, RunReport},
    wire::{Conclusion, RunRecord, SCHEMA_VERSION},
    workflow::DeclaredSteps,
};

/// Fold the engine report into the tree and return the run conclusion.
/// Partial coverage and failed cleanup are never a pass.
pub fn conclude(
    outcome: &Result<EngineReport, String>,
    tree: &mut RunTree,
    declared: &DeclaredSteps,
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
            tree.add_skipped_steps(declared);
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
/// `ui_origin` is the dashboard's origin while it serves; the report then
/// links straight to this run.
pub fn report(
    record: &RunRecord,
    ui_origin: Option<&str>,
    tail: impl FnOnce(&str, &str) -> Vec<String>,
) -> RunReport {
    let tree = &record.tree;
    let first_failure = tree.first_failure().map(|(job, section)| {
        let key = section.map(|s| format!("{}:{}", s.stage, s.id));
        FailureReport {
            job: job.key.clone(),
            job_name: job.name.clone(),
            tail: key
                .as_deref()
                .map(|k| tail(&job.key, k))
                .unwrap_or_default(),
            section: key,
            step: section.map(|s| s.name.clone()),
            exit_code: section.and_then(|s| s.exit_code),
        }
    });
    let count = |c: ItemConclusion| tree.jobs().filter(|j| j.conclusion == Some(c)).count();
    let unsupported = tree.unsupported_jobs();
    RunReport {
        schema_version: SCHEMA_VERSION,
        run: record.id.clone(),
        state: record.state,
        conclusion: record.conclusion,
        exit_code: record.conclusion.map(Conclusion::exit_code),
        reason: record.reason.clone(),
        sha: record.sha.clone(),
        dirty: record.dirty.clone(),
        workflow: record.workflow.clone(),
        trigger: record.trigger,
        mode: record.mode,
        actor: record.actor.clone(),
        cleanup: record.cleanup.clone(),
        first_failure,
        jobs: JobOutcomes {
            succeeded: count(ItemConclusion::Success),
            failed: count(ItemConclusion::Failure),
            cancelled: count(ItemConclusion::Cancelled),
            skipped: tree.skipped_jobs(),
            unsupported: unsupported.clone(),
        },
        coverage_complete: unsupported.is_empty(),
        ui_url: ui_origin.map(|origin| format!("{origin}/ci/runs/{}", record.id)),
        logs_command: format!("bosn ci logs {}", record.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::model::{ActParser, RunTree, parse_act_list};

    #[test]
    fn the_report_lists_skipped_and_unsupported_jobs_and_links_the_dashboard() {
        let list = "Stage  Job ID  Job name  Workflow name  Workflow file  Events\n\
                    0      lin     lin       w              ci.yml         push\n\
                    0      mac     mac       w              ci.yml         push\n\
                    1      late    late      w              ci.yml         push\n";
        let mut parser = ActParser::new(RunTree::declared(&parse_act_list(list)));
        for (seq, line) in [
            r#"{"job":"w/mac","jobID":"mac","level":"info","msg":"🚧  Skipping unsupported platform -- Try running with `-P macos-latest=...`"}"#,
            r#"{"job":"w/lin","jobID":"lin","msg":"⭐ Run Main a","stage":"Main","step":"a","stepID":["0"]}"#,
            r#"{"job":"w/lin","jobID":"lin","msg":"  ✅  Success - Main a","stage":"Main","stepID":["0"],"stepResult":"success"}"#,
            r#"{"job":"w/lin","jobID":"lin","msg":"🏁  Job succeeded","jobResult":"success"}"#,
        ]
        .into_iter()
        .enumerate()
        {
            parser.feed(seq as u64 + 1, line);
        }
        parser.tree.settle_finished();
        let mut record = crate::ci::tests::sample_record("run-1");
        record.tree = parser.tree;
        record.finish(Conclusion::Failure, None);

        let report = report(&record, Some("http://127.0.0.1:7341"), |_, _| Vec::new());
        assert_eq!(report.jobs.succeeded, 1);
        assert_eq!(report.jobs.unsupported, ["w/mac"]);
        // A job that never ran keeps its declared key, the job ID.
        assert_eq!(report.jobs.skipped, ["late"]);
        assert!(
            !report.coverage_complete,
            "an unsupported job is partial coverage"
        );
        assert_eq!(
            report.ui_url.as_deref(),
            Some("http://127.0.0.1:7341/ci/runs/run-1")
        );
        assert_eq!(report.logs_command, "bosn ci logs run-1");
        let without_ui = super::report(&record, None, |_, _| Vec::new());
        assert_eq!(
            without_ui.ui_url, None,
            "no link while the dashboard is off"
        );
    }
}
