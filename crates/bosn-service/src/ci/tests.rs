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
use serde_json::Value;
use std::{sync::Arc, time::Duration};

fn staged(runtime: &CiRuntime, id: &str) {
    let source = runtime.staging_dir(id).join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("README"), "x").unwrap();
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
        dirty: false,
        origin: Some("https://github.com/o/r.git".into()),
        pr_number: None,
        timeout_secs: Some(5),
    }
}

async fn submit(runtime: &CiRuntime, sha_byte: char) -> Value {
    let staging = new_uuid().await.unwrap();
    staged(runtime, &staging);
    runtime
        .handle(CiRequest::Submit {
            request: request(&staging, sha_byte),
        })
        .await
        .unwrap()
}

async fn wait_done(runtime: &CiRuntime, run: &str) -> RunRecord {
    for _ in 0..500 {
        let record = runtime.record(run).unwrap();
        if record.state == RunState::Done {
            return record;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run} did not finish");
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
        assert_eq!(first["run"], again["run"], "same key, same run ID");
        assert_eq!(again["coalesced"], true);
        let other = submit(&runtime, 'b').await;
        assert_eq!(other["coalesced"], false);
        async_engine::sleep(Duration::from_millis(100)).await;
        let b = other["run"].as_str().unwrap().to_string();
        let a = first["run"].as_str().unwrap().to_string();
        assert_eq!(
            runtime.record(&b).unwrap().state,
            RunState::Queued,
            "limit 1; a = {:?}",
            runtime
                .record(&a)
                .map(|r| (r.state, r.conclusion, r.reason))
        );
        // Raising the limit starts the waiting run immediately.
        runtime
            .handle(CiRequest::Runners {
                action: RunnerAction::SetLimit { limit: 2 },
            })
            .await
            .unwrap();
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
        for run in [&a, &b] {
            runtime
                .handle(CiRequest::Cancel { run: run.clone() })
                .await
                .unwrap();
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
        let run = submit(&runtime, 'c').await["run"]
            .as_str()
            .unwrap()
            .to_string();
        let record = wait_done(&runtime, &run).await;
        assert_eq!(record.conclusion, Some(Conclusion::Failure));
        assert_eq!(Conclusion::Failure.exit_code(), 1);
        let report = runtime
            .handle(CiRequest::Report {
                run: run.clone(),
                tail: Some(10),
            })
            .await
            .unwrap();
        assert_eq!(report["first_failure"]["job"], "w/a");
        assert_eq!(report["first_failure"]["section"], "Main:0");
        let tail = report["first_failure"]["tail"].as_array().unwrap();
        assert_eq!(tail.len(), 2, "only the failing step's lines: {tail:?}");
        assert_eq!(report["exit_code"], 1);
        // Paged logs return every record exactly once.
        let mut seen = Vec::new();
        let mut since = 0;
        loop {
            let page = runtime
                .handle(CiRequest::Logs {
                    run: run.clone(),
                    job: None,
                    section: None,
                    since_seq: Some(since),
                    limit: Some(2),
                    max_bytes: None,
                })
                .await
                .unwrap();
            for r in page["records"].as_array().unwrap() {
                seen.push(r["seq"].as_u64().unwrap());
            }
            since = page["next_seq"].as_u64().unwrap();
            if page["more"] == false {
                break;
            }
        }
        let expected: Vec<u64> = (1..=record.log_records).collect();
        assert_eq!(seen, expected);
    });
}

#[test]
fn queued_runs_cancel_without_an_engine_and_drain_holds_the_queue() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 1);
        runtime
            .handle(CiRequest::Runners {
                action: RunnerAction::Drain,
            })
            .await
            .unwrap();
        let run = submit(&runtime, 'e').await["run"]
            .as_str()
            .unwrap()
            .to_string();
        async_engine::sleep(Duration::from_millis(50)).await;
        assert_eq!(runtime.record(&run).unwrap().state, RunState::Queued);
        let cancelled = runtime
            .handle(CiRequest::Cancel { run: run.clone() })
            .await
            .unwrap();
        assert_eq!(cancelled["cancelled"], true);
        assert_eq!(
            runtime.record(&run).unwrap().conclusion,
            Some(Conclusion::Cancelled)
        );
        assert_eq!(*backend.executions.lock().unwrap(), 0);
        let resumed = runtime
            .handle(CiRequest::Runners {
                action: RunnerAction::Resume,
            })
            .await
            .unwrap();
        assert_eq!(resumed["runners"]["drained"], false);
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
                request: dirty_release,
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
                request: request("../../etc", 'f'),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "refused");
    });
}

#[test]
fn restart_marks_unfinished_runs_interrupted_and_keeps_finished_ones() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let runtime = CiRuntime::start(&dir, registry.clone(), backend.clone(), 1);
        let done = submit(&runtime, '1').await["run"]
            .as_str()
            .unwrap()
            .to_string();
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
        assert_eq!(listed["runs"].as_array().unwrap().len(), 2);
    });
}

#[test]
fn conclusion_never_passes_partial_coverage_or_failed_cleanup() {
    let report = |execution, cleanup| {
        Ok(lifecycle::EngineReport {
            execution,
            cleanup,
            engine_id: None,
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
            &mut partial
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
            &mut passed.clone()
        )
        .0,
        Conclusion::Success
    );
    assert_eq!(
        report::conclude(
            &report(ExecutionEnd::Exited(0), CleanupEnd::Failed("x".into())),
            &mut passed
        )
        .0,
        Conclusion::Error
    );
    assert_eq!(
        report::conclude(
            &report(ExecutionEnd::TimedOut, CleanupEnd::Removed),
            &mut tree
        )
        .0,
        Conclusion::TimedOut
    );
    assert_eq!(Conclusion::TimedOut.exit_code(), 2);
}

#[test]
fn a_run_where_every_declared_job_was_skipped_is_not_success() {
    let mut tree = RunTree::declared(&parse_act_list(super::lifecycle::tests::LISTING));
    let outcome = Ok(lifecycle::EngineReport {
        execution: ExecutionEnd::Exited(0),
        cleanup: CleanupEnd::Removed,
        engine_id: None,
    });
    let (conclusion, reason) = report::conclude(&outcome, &mut tree);
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
