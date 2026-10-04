//! CiRuntime integration tests over the synthetic engine (no Docker).

use super::lifecycle::tests::{FakeBackend, Faults, with_registry};
use super::*;
use super::{
    lifecycle::{CleanupEnd, ExecutionEnd},
    model::{ActParser, LogRecord, RunTree, parse_act_list},
    provider::{Mode, Provider, Trigger},
    store,
};
use kernal_api::async_engine;
use std::{sync::Arc, time::Duration};

fn staged(runtime: &CiRuntime, id: &str) {
    let source = runtime.staging_dir(id).join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("README"), "x").unwrap();
    let workflows = source.join(".github/workflows");
    std::fs::create_dir_all(&workflows).unwrap();
    std::fs::write(workflows.join("ci.yml"), "jobs:\n  a:\n    steps: []\n").unwrap();
}

fn request(staging: &str, sha_byte: char) -> SubmitRequest {
    SubmitRequest {
        staging: staging.into(),
        workspace: "/work/repo".into(),
        provider: Provider::Github,
        engine: "act".into(),
        workflow: ".github/workflows/ci.yml".into(),
        job: None,
        trigger: Trigger::Push,
        mode: Mode::Minimal,
        actor: "human".into(),
        sha: sha_byte.to_string().repeat(40),
        branch: Some("main".into()),
        tree_digest: "d".repeat(64),
        git_tree: None,
        dirty: false,
        commit: None,
        base: None,
        origin: Some("https://github.com/o/r.git".into()),
        pr_number: None,
        // Leave room for heavily loaded CI hosts; timeout tests override this.
        timeout_secs: Some(30),
        secrets: Vec::new(),
        params: Default::default(),
    }
}

/// A queued record for `id`, for tests that only need its shape.
pub(crate) fn sample_record(id: &str) -> RunRecord {
    RunRecord::queued(id.into(), &request("s", 'a'), "push", b"{}")
}

/// Dispatch like the daemon does and parse the reply as a client would, so
/// every test also proves the wire reply decodes into its typed reply.
async fn call<T: serde::de::DeserializeOwned>(runtime: &CiRuntime, request: CiRequest) -> T {
    let value = runtime.handle(request).await.unwrap();
    serde_json::from_value(value).expect("reply decodes into its typed reply")
}

async fn submit(runtime: &CiRuntime, sha_byte: char) -> SubmitReply {
    let staging = new_uuid().await.unwrap();
    staged(runtime, &staging);
    call(
        runtime,
        CiRequest::Submit {
            request: Box::new(request(&staging, sha_byte)),
        },
    )
    .await
}

async fn wait_done(runtime: &CiRuntime, run: &str) -> RunRecord {
    for _ in 0..500 {
        let record = runtime.record(run).unwrap();
        if record.state == RunState::Done {
            return record;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    let record = runtime.record(run).unwrap();
    panic!(
        "run {run} did not finish: state {:?}, conclusion {:?}, reason {:?}, cleanup {:?}, engine {:?}",
        record.state, record.conclusion, record.reason, record.cleanup, record.engine_id
    );
}

#[test]
fn invalid_original_workflow_never_establishes_a_successful_run() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry, backend, 1);
        let staging = new_uuid().await.unwrap();
        staged(&runtime, &staging);
        std::fs::write(
            runtime
                .staging_dir(&staging)
                .join("source/.github/workflows/ci.yml"),
            "jobs: [",
        )
        .unwrap();
        let submitted: SubmitReply = call(
            &runtime,
            CiRequest::Submit {
                request: Box::new(request(&staging, 'a')),
            },
        )
        .await;
        let done = wait_done(&runtime, &submitted.run).await;
        assert_eq!(done.conclusion, Some(Conclusion::Incomplete));
        assert!(done.reason.unwrap().contains("workflow cannot be parsed"));
    });
}

#[test]
fn identical_submissions_coalesce_and_distinct_ones_queue_behind_the_limit() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            hang: true,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 1);
        let first = submit(&runtime, 'a').await;
        let again = submit(&runtime, 'a').await;
        assert_eq!(first.run, again.run, "same key, same run ID");
        assert!(again.coalesced);
        let other = submit(&runtime, 'b').await;
        assert!(!other.coalesced);
        async_engine::sleep(Duration::from_millis(100)).await;
        let (a, b) = (first.run, other.run);
        assert_eq!(
            runtime.record(&b).unwrap().state,
            RunState::Queued,
            "limit 1; a = {:?}",
            runtime
                .record(&a)
                .map(|r| (r.state, r.conclusion, r.reason))
        );
        // Raising the limit starts the waiting run immediately.
        let _: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::SetLimit { limit: 2 },
            },
        )
        .await;
        async_engine::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runtime.record(&b).unwrap().state,
            RunState::Running,
            "{:?}",
            runtime
                .record(&b)
                .map(|r| (r.conclusion, r.reason, r.cleanup))
        );
        assert_eq!(runtime.record(&a).unwrap().submitters, 2);
        while *backend.executions.lock().unwrap() < 2 {
            async_engine::sleep(Duration::from_millis(5)).await;
        }
        for run in [&a, &b] {
            let cancelled: CancelReply =
                call(&runtime, CiRequest::Cancel { run: run.clone() }).await;
            assert!(cancelled.cancelled);
            let done = wait_done(&runtime, run).await;
            assert_eq!(done.conclusion, Some(Conclusion::Cancelled));
            assert_eq!(done.cleanup.as_deref(), Some("removed"));
        }
        assert_eq!(
            *backend.executions.lock().unwrap(),
            2,
            "one execution per key"
        );
        assert_eq!(backend.live(), 0);
    });
}

#[test]
fn failing_run_reports_the_failing_step_and_only_its_tail() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            exit_code: 1,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry, backend, 2);
        let run = submit(&runtime, 'c').await.run;
        let record = wait_done(&runtime, &run).await;
        assert_eq!(record.conclusion, Some(Conclusion::Failure));
        assert_eq!(Conclusion::Failure.exit_code(), 1);
        let report: RunReport = call(
            &runtime,
            CiRequest::Report {
                run: run.clone(),
                tail: Some(10),
            },
        )
        .await;
        let failure = report.first_failure.expect("a failing step");
        assert_eq!(failure.job, "w/a");
        assert_eq!(failure.section.as_deref(), Some("Main:0"));
        assert_eq!(failure.tail.len(), 2, "only the failing step's lines");
        assert_eq!(report.exit_code, Some(1));
        // Paged logs return every record exactly once.
        let mut seen = Vec::new();
        let mut since = 0;
        loop {
            let page: LogsReply = call(
                &runtime,
                CiRequest::Logs {
                    run: run.clone(),
                    job: None,
                    section: None,
                    since_seq: Some(since),
                    limit: Some(2),
                    max_bytes: None,
                },
            )
            .await;
            seen.extend(page.records.iter().map(|r| r.seq));
            since = page.next_seq;
            if !page.more {
                break;
            }
        }
        let expected: Vec<u64> = (1..=record.log_records).collect();
        assert_eq!(seen, expected);
    });
}

#[test]
fn the_run_timeout_covers_planning_not_only_execution() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            slow_image: true,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 1);
        let staging = new_uuid().await.unwrap();
        staged(&runtime, &staging);
        let mut submission = request(&staging, 'f');
        submission.timeout_secs = Some(1);
        let reply: SubmitReply = call(
            &runtime,
            CiRequest::Submit {
                request: Box::new(submission),
            },
        )
        .await;
        let record = wait_done(&runtime, &reply.run).await;
        assert_eq!(record.conclusion, Some(Conclusion::TimedOut));
        assert_eq!(*backend.executions.lock().unwrap(), 0);
        assert!(
            backend.engines.lock().unwrap().is_empty(),
            "no engine was created"
        );
    });
}

#[test]
fn queued_runs_cancel_without_an_engine_and_drain_holds_the_queue() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 1);
        let _: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::Drain,
            },
        )
        .await;
        let run = submit(&runtime, 'e').await.run;
        async_engine::sleep(Duration::from_millis(50)).await;
        assert_eq!(runtime.record(&run).unwrap().state, RunState::Queued);
        let cancelled: CancelReply = call(&runtime, CiRequest::Cancel { run: run.clone() }).await;
        assert!(cancelled.cancelled);
        assert_eq!(
            runtime.record(&run).unwrap().conclusion,
            Some(Conclusion::Cancelled)
        );
        assert_eq!(*backend.executions.lock().unwrap(), 0);
        let resumed: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::Resume,
            },
        )
        .await;
        assert!(!resumed.runners.drained);
    });
}

#[test]
fn refusals_are_typed_and_release_needs_a_clean_tree() {
    with_registry(|registry, dir| async move {
        let runtime = CiRuntime::start(&dir, registry, Arc::new(FakeBackend::default()), 1);
        let staging = new_uuid().await.unwrap();
        staged(&runtime, &staging);
        let mut dirty_release = request(&staging, 'f');
        dirty_release.trigger = Trigger::Release;
        dirty_release.mode = Mode::Full;
        dirty_release.dirty = true;
        let error = runtime
            .handle(CiRequest::Submit {
                request: Box::new(dirty_release),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "refused");
        assert_eq!(Conclusion::Refused.exit_code(), 3);
        assert!(
            !runtime.staging_dir(&staging).exists(),
            "refused submissions do not leak their snapshot"
        );
        let error = runtime
            .handle(CiRequest::Submit {
                request: Box::new(request("../../etc", 'f')),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "refused");
    });
}

/// #403: only a `pr` run carries a base, and it must be a safe ref and a SHA.
#[test]
fn a_base_is_only_for_a_pr_run_and_must_be_a_branch_and_sha() {
    let based = |trigger, branch: &str, sha: String| {
        let mut submission = request(&"0".repeat(8), 'a');
        submission.staging = "00000000-0000-4000-8000-000000000002".into();
        submission.trigger = trigger;
        submission.base = Some(crate::ci::snapshot::BaseRef {
            branch: branch.into(),
            sha,
        });
        submission.validate()
    };
    assert!(based(Trigger::Pr, "main", "b".repeat(40)).is_ok());
    assert!(based(Trigger::Push, "main", "b".repeat(40)).is_err());
    assert!(based(Trigger::Pr, "../x", "b".repeat(40)).is_err());
    assert!(based(Trigger::Pr, "main", "HEAD".into()).is_err());
}

#[test]
fn restart_marks_unfinished_runs_interrupted_and_keeps_finished_ones() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry.clone(), backend.clone(), 1);
        let done = submit(&runtime, '1').await.run;
        wait_done(&runtime, &done).await;
        // Fake a run a dead daemon left running.
        let mut stuck = runtime.record(&done).unwrap();
        stuck.id = "00000000-0000-4000-8000-000000000001".into();
        stuck.state = RunState::Running;
        stuck.conclusion = None;
        runtime.save(&stuck);
        let restarted = CiRuntime::start(&dir, registry, backend, 1);
        let record = restarted.record(&stuck.id).unwrap();
        assert_eq!(record.state, RunState::Done);
        assert_eq!(record.conclusion, Some(Conclusion::Error));
        assert!(record.reason.unwrap().contains("interrupted"));
        assert_eq!(
            restarted.record(&done).unwrap().conclusion,
            Some(Conclusion::Success)
        );
        let listed = restarted.listing();
        assert_eq!(listed.runs.len(), 2);
    });
}

#[test]
fn conclusion_never_passes_partial_coverage_or_failed_cleanup() {
    let report = |execution, cleanup| {
        Ok(lifecycle::EngineReport {
            execution,
            cleanup,
            engine_id: None,
            storage: None,
        })
    };
    let mut tree = RunTree::declared(&parse_act_list(super::lifecycle::tests::LISTING));
    let mut parser = ActParser::new(tree.clone());
    parser.feed(
        1,
        r#"{"job":"w/a","jobID":"a","msg":"🚧  Skipping unsupported platform"}"#,
    );
    let mut partial = parser.tree.clone();
    assert_eq!(
        report::conclude(
            &report(ExecutionEnd::Exited(0), CleanupEnd::Removed),
            &mut partial,
            &Default::default(),
        )
        .0,
        Conclusion::Incomplete
    );
    parser.tree = tree.clone();
    parser.feed(
        1,
        r#"{"job":"w/a","jobID":"a","msg":"x","jobResult":"success"}"#,
    );
    let mut passed = parser.tree.clone();
    assert_eq!(
        report::conclude(
            &report(ExecutionEnd::Exited(0), CleanupEnd::Removed),
            &mut passed.clone(),
            &Default::default(),
        )
        .0,
        Conclusion::Success
    );
    assert_eq!(
        report::conclude(
            &report(ExecutionEnd::Exited(0), CleanupEnd::Failed("x".into())),
            &mut passed,
            &Default::default(),
        )
        .0,
        Conclusion::Error
    );
    assert_eq!(
        report::conclude(
            &report(ExecutionEnd::TimedOut, CleanupEnd::Removed),
            &mut tree,
            &Default::default(),
        )
        .0,
        Conclusion::TimedOut
    );
    assert_eq!(Conclusion::TimedOut.exit_code(), 2);
}

#[test]
fn a_failed_run_on_nearly_full_storage_says_why_in_its_reason() {
    const GIB: u64 = 1 << 30;
    let low = super::storage::StorageUsage {
        size: 20 * GIB,
        used: 16 * GIB,
        available: 4 * GIB,
    };
    let outcome = |execution, storage| {
        Ok(lifecycle::EngineReport {
            execution,
            cleanup: CleanupEnd::Removed,
            engine_id: None,
            storage,
        })
    };
    let declared = RunTree::declared(&parse_act_list(super::lifecycle::tests::LISTING));
    let (conclusion, reason) = report::conclude(
        &outcome(ExecutionEnd::Exited(1), Some(low)),
        &mut declared.clone(),
        &Default::default(),
    );
    assert_eq!(conclusion, Conclusion::Failure);
    let reason = reason.expect("a storage-starved failure explains itself");
    assert!(reason.contains("storage ran low"), "{reason}");
    assert!(reason.contains("storage_gib"), "{reason}");
    // An engine failure keeps its own error first.
    let (_, reason) = report::conclude(
        &outcome(ExecutionEnd::EngineFailed("runner load".into()), Some(low)),
        &mut declared.clone(),
        &Default::default(),
    );
    let reason = reason.unwrap();
    assert!(reason.starts_with("runner load; "), "{reason}");
    assert!(reason.contains("storage ran low"), "{reason}");
    // Roomy storage, or a run that did not fail, adds nothing.
    let roomy = super::storage::StorageUsage {
        size: 36 * GIB,
        used: 16 * GIB,
        available: 20 * GIB,
    };
    let (_, reason) = report::conclude(
        &outcome(ExecutionEnd::Exited(1), Some(roomy)),
        &mut declared.clone(),
        &Default::default(),
    );
    assert_eq!(reason, None);
    let (_, reason) = report::conclude(
        &outcome(ExecutionEnd::Cancelled, Some(low)),
        &mut declared.clone(),
        &Default::default(),
    );
    assert_eq!(reason, None);
}

#[test]
fn a_run_where_every_declared_job_was_skipped_is_not_success() {
    let mut tree = RunTree::declared(&parse_act_list(super::lifecycle::tests::LISTING));
    let outcome = Ok(lifecycle::EngineReport {
        execution: ExecutionEnd::Exited(0),
        cleanup: CleanupEnd::Removed,
        engine_id: None,
        storage: None,
    });
    let (conclusion, reason) = report::conclude(&outcome, &mut tree, &Default::default());
    assert_eq!(conclusion, Conclusion::Incomplete, "{reason:?}");
}

#[test]
fn log_pages_always_advance_and_tails_cut_on_char_boundaries() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let store = store::Store::open(dir.path()).unwrap();
    std::fs::create_dir_all(store.run_dir("r")).unwrap();
    let mut log = store.log_writer("r").unwrap();
    // One record far larger than any page, made of 4-byte characters.
    let huge = "⭐".repeat(30_000);
    for seq in 1..=2 {
        log.append(&LogRecord {
            seq,
            stream: "stdout".into(),
            job: Some("j".into()),
            section: Some("Main:0".into()),
            text: huge.clone(),
        });
    }
    log.flush();
    let query = store::LogQuery {
        since: 0,
        visible: 2,
        filter: store::LogFilter::default(),
        limit: 10,
        max_bytes: 1024,
    };
    let page = store.read_log("r", 0, &query);
    assert_eq!(
        page.records.len(),
        1,
        "an oversized record still moves the cursor"
    );
    assert_eq!(page.next_seq, 1);
    assert!(page.records[0].text.len() <= 1024);
    let tail = store.tail(
        "r",
        store::LogFilter {
            job: Some("j"),
            section: Some("Main:0"),
        },
        5,
    );
    assert_eq!(tail.len(), 2);
    assert!(tail.iter().all(|t| t.len() <= 2048 && t.ends_with('⭐')));
}

#[test]
fn steps_act_never_mentioned_are_listed_as_skipped_in_jobs_that_ran() {
    use super::workflow::{Declared, DeclaredStep};
    let mut parser = ActParser::new(RunTree::declared(&parse_act_list(
        super::lifecycle::tests::LISTING,
    )));
    for line in [
        r#"{"job":"w/a","jobID":"a","msg":"⭐ Run Main one","stage":"Main","step":"one","stepID":["0"]}"#,
        r#"{"job":"w/a","jobID":"a","msg":"ok","stage":"Main","stepID":["0"],"stepResult":"success"}"#,
        r#"{"job":"w/a","jobID":"a","msg":"⭐ Run Main three","stage":"Main","step":"three","stepID":["2"]}"#,
        r#"{"job":"w/a","jobID":"a","msg":"ok","stage":"Main","stepID":["2"],"stepResult":"success"}"#,
        r#"{"job":"w/a","jobID":"a","msg":"⭐ Run Complete job","stepid":["--complete-job"]}"#,
        r#"{"job":"w/a","jobID":"a","msg":"ok","stepid":["--complete-job"],"stepResult":"success"}"#,
        r#"{"job":"w/a","jobID":"a","msg":"🏁","jobResult":"success"}"#,
    ]
    .iter()
    .enumerate()
    {
        parser.feed(line.0 as u64 + 1, line.1);
    }
    let step = |id: &str, name: &str| DeclaredStep {
        id: id.into(),
        name: name.into(),
    };
    let declared = Declared {
        steps: [(
            "a".to_string(),
            vec![step("0", "one"), step("1", "never"), step("2", "three")],
        )]
        .into(),
        remote_only: Default::default(),
        ..Declared::default()
    };
    let mut tree = parser.tree;
    let (conclusion, _) = report::conclude(
        &Ok(lifecycle::EngineReport {
            execution: ExecutionEnd::Exited(0),
            cleanup: CleanupEnd::Removed,
            engine_id: None,
            storage: None,
        }),
        &mut tree,
        &declared,
    );
    assert_eq!(
        conclusion,
        Conclusion::Success,
        "a skipped step is not a failure"
    );
    let job = tree.jobs().next().unwrap();
    let order: Vec<_> = job
        .sections
        .iter()
        .map(|s| (s.id.as_str(), s.conclusion))
        .collect();
    use super::model::ItemConclusion::{Skipped, Success};
    assert_eq!(
        order,
        [
            ("0", Some(Success)),
            ("1", Some(Skipped)),
            ("2", Some(Success)),
            ("--complete-job", Some(Success)),
        ],
        "the skipped step sits in declaration order"
    );
}

#[test]
fn a_failed_widget_launch_is_reported_once_per_daemon() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry, backend, 1);
        assert!(runtime.note_launch_failure(), "the first failure is logged");
        assert!(!runtime.note_launch_failure(), "later ones stay quiet");
        assert!(
            !runtime.clone().note_launch_failure(),
            "clones share the daemon's state"
        );
    });
}

#[test]
fn stdout_and_stderr_stay_separate_and_seq_has_no_gaps() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry, backend, 1);
        let run = submit(&runtime, 'c').await.run;
        wait_done(&runtime, &run).await;
        let page: LogsReply = call(
            &runtime,
            CiRequest::Logs {
                run,
                job: None,
                section: None,
                since_seq: Some(0),
                limit: None,
                max_bytes: None,
            },
        )
        .await;
        let seqs: Vec<u64> = page.records.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>(), "no gaps");
        let stderr: Vec<_> = page
            .records
            .iter()
            .filter(|r| r.stream == "stderr")
            .collect();
        assert_eq!(stderr.len(), 1, "{:?}", page.records);
        assert_eq!(stderr[0].text, "act: warning on stderr");
        assert_eq!(stderr[0].job, None, "stderr is never attributed to a job");
        assert!(
            page.records
                .iter()
                .filter(|r| r.text.contains("Run Main x"))
                .all(|r| r.stream != "stderr"),
            "act's JSON on stdout is never relabelled"
        );
    });
}

#[test]
fn fifty_concurrent_submissions_with_ten_keys_make_ten_executions() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 4);
        // Drained, so no run finishes mid-burst and every duplicate coalesces.
        let _: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::Drain,
            },
        )
        .await;
        let tasks: Vec<_> = (0..50)
            .map(|i| {
                let runtime = runtime.clone();
                let key = char::from_digit(i % 10, 10).unwrap();
                async_engine::launch(async move { (key, submit(&runtime, key).await.run) })
            })
            .collect();
        let mut by_key =
            std::collections::BTreeMap::<char, std::collections::BTreeSet<String>>::new();
        for task in tasks {
            let (key, run) = task.await.unwrap();
            by_key.entry(key).or_default().insert(run);
        }
        assert_eq!(by_key.len(), 10);
        assert!(
            by_key.values().all(|runs| runs.len() == 1),
            "every submitter of a key gets the same run: {by_key:?}"
        );
        let _: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::Resume,
            },
        )
        .await;
        for runs in by_key.values() {
            let run = runs.iter().next().unwrap();
            let record = wait_done(&runtime, run).await;
            assert_eq!(record.submitters, 5, "five submitters joined {run}");
        }
        assert_eq!(*backend.executions.lock().unwrap(), 10);
        assert_eq!(backend.live(), 0, "no engine left");
    });
}

#[test]
fn a_run_whose_client_went_away_still_ends_and_is_cleaned() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            hang: true,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 1);
        let staging = new_uuid().await.unwrap();
        staged(&runtime, &staging);
        let mut submission = request(&staging, 'd');
        submission.timeout_secs = Some(1);
        let run = call::<SubmitReply>(
            &runtime,
            CiRequest::Submit {
                request: Box::new(submission),
            },
        )
        .await
        .run;
        // A client waiting on the run, killed mid-run: nothing else waits.
        let waiter = {
            let (runtime, run) = (runtime.clone(), run.clone());
            async_engine::launch(async move { wait_done(&runtime, &run).await })
        };
        async_engine::sleep(Duration::from_millis(100)).await;
        drop(waiter);
        let record = wait_done(&runtime, &run).await;
        assert_eq!(record.conclusion, Some(Conclusion::TimedOut));
        assert_eq!(backend.live(), 0, "the engine is removed with no client");
    });
}

#[test]
fn a_silent_step_still_shows_its_progress_while_it_runs() {
    with_registry(|registry, dir| async move {
        // Two lines, then silence until the timeout.
        let backend = Arc::new(FakeBackend::with(Faults {
            hang: true,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry, backend, 1);
        let staging = new_uuid().await.unwrap();
        staged(&runtime, &staging);
        let mut submission = request(&staging, 'e');
        submission.timeout_secs = Some(10);
        let run = call::<SubmitReply>(
            &runtime,
            CiRequest::Submit {
                request: Box::new(submission),
            },
        )
        .await
        .run;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let record = runtime.record(&run).unwrap();
            // Queued at first on a slow machine; never done before the step shows.
            assert_ne!(
                record.state,
                RunState::Done,
                "the step hangs until cancelled"
            );
            let shown = record.tree.jobs().any(|j| !j.sections.is_empty());
            if shown {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the running step never became visible: {record:?}"
            );
            async_engine::sleep(Duration::from_millis(50)).await;
        }
        let _: CancelReply = call(&runtime, CiRequest::Cancel { run: run.clone() }).await;
        wait_done(&runtime, &run).await;
    });
}

#[test]
fn the_cache_volume_is_measured_and_cleared_only_while_nothing_runs() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 1);
        let usage = |runtime: CiRuntime| async move {
            call::<RunnersReply>(
                &runtime,
                CiRequest::Runners {
                    action: RunnerAction::CacheUsage,
                },
            )
            .await
            .cache
            .expect("cache usage is reported")
        };
        assert_eq!(
            usage(runtime.clone()).await.bytes,
            None,
            "no run yet: no volume"
        );
        let run = submit(&runtime, '1').await.run;
        wait_done(&runtime, &run).await;
        let used = usage(runtime.clone()).await;
        assert_eq!(used.volume, "bosn-ci-cache-v1");
        assert!(used.bytes.is_some(), "a run creates the cache volume");

        *backend.faults.lock().unwrap() = Faults {
            hang: true,
            ..Faults::default()
        };
        let busy = submit(&runtime, '2').await.run;
        for _ in 0..200 {
            if *backend.executions.lock().unwrap() == 2 {
                break;
            }
            async_engine::sleep(Duration::from_millis(10)).await;
        }
        let refused = runtime
            .handle(CiRequest::Runners {
                action: RunnerAction::ClearCache,
            })
            .await
            .unwrap_err();
        assert_eq!(refused.code, "refused", "never under a running job");
        assert!(usage(runtime.clone()).await.bytes.is_some());

        let _: CancelReply = call(&runtime, CiRequest::Cancel { run: busy.clone() }).await;
        wait_done(&runtime, &busy).await;
        let cleared: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::ClearCache,
            },
        )
        .await;
        assert_eq!(cleared.cache.unwrap().bytes, None, "the volume is gone");
    });
}

mod params;
mod spare;

mod source_identity;
