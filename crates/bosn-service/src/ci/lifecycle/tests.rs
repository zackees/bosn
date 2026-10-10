use super::*;
use crate::{Registry, registry_actor};
use bosn_registry::act::{ActEngineObservation, ActEngineRemovalProof};
use kernal_api::async_engine::{CancellationSource, RuntimeBuilder};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

mod fake;
#[path = "live_cohort.rs"]
mod live_cohort;
pub use fake::*;

#[derive(Default)]
pub struct Collect {
    pub lines: Vec<EngineLine>,
    pub listing: Option<String>,
    pub notes: Vec<String>,
}
impl EngineObserver for Collect {
    fn note(&mut self, text: &str) {
        self.notes.push(text.into());
    }
    fn declared(&mut self, listing: &str) {
        self.listing = Some(listing.into());
    }
    fn line(&mut self, line: EngineLine) {
        self.lines.push(line);
    }
}

pub fn intent(run: &str) -> ActEngineIntent {
    ActEngineIntent {
        run_id: run.into(),
        workspace: "/private/source".into(),
        candidate_sha: "a".repeat(40),
        payload_sha256: "b".repeat(64),
        snapshot_sha256: "c".repeat(64),
        act_version: "0.2.88".into(),
        act_image_digest: format!("sha256:{}", "d".repeat(64)),
        engine_image_digest: crate::act_engine::ENGINE_MANIFEST.into(),
        runner_image_digest: format!("sha256:{}", "f".repeat(64)),
        created_at: 1.0,
        spare: false,
        creation_profile: Some(
            crate::act_engine::creation_profile_with_cache(
                super::super::limits::size_engine(FAKE_HOST, Default::default()).unwrap(),
                Some(test_cache().mount()),
            )
            .unwrap(),
        ),
    }
}
pub fn plan(run: &str, deadline: Duration) -> EnginePlan {
    EnginePlan {
        intent: intent(run),
        act: super::super::engine::act_artifact("x86_64").unwrap(),
        source: "/nonexistent".into(),
        event: "/nonexistent".into(),
        invocation: ActInvocation {
            event: "push".into(),
            workflow: ".github/workflows/ci.yml".into(),
            workflow_overlaid: false,
            job: None,
            cache_route: crate::ci::cache_cohort::CacheRoute::Legacy(
                crate::ci::cache_cohort::Namespace::parse(&"0".repeat(16)).unwrap(),
            ),
            secrets: Default::default(),
            params: Default::default(),
            scope: None,
        },
        cache: test_cache(),
        deadline: async_engine::Deadline::after(deadline),
        spare: None,
    }
}
pub fn test_cache() -> CacheVolume {
    CacheVolume {
        name: "bosn-ci-cache-test".into(),
        labels: BTreeMap::new(),
    }
}
pub fn run_id(n: u32) -> String {
    format!("aaaaaaaa-bbbb-4ccc-8ddd-{n:012x}")
}

pub const OWNER: &str = "11111111-2222-4333-8444-555555555555";

/// One daemon's sole registry writer over a registry file.
pub struct Daemon {
    pub registry: RegistryActor,
    task: async_engine::Task<()>,
}
impl Daemon {
    pub fn open(path: &std::path::Path) -> Self {
        let registry = if path.exists() {
            Registry::open_writer(path).unwrap()
        } else {
            Registry::create_writer(path, OWNER).unwrap()
        };
        let (sender, receiver) = async_engine::channel(16);
        let task = async_engine::launch(registry_actor(registry, receiver, None));
        Self {
            registry: RegistryActor { sender },
            task,
        }
    }
    pub async fn stop(self) {
        self.registry.stop().await;
        let _ = self.task.await;
    }
}

/// Run `body` with a live registry actor over a temporary registry.
pub fn with_registry<F, Fut>(body: F)
where
    F: FnOnce(RegistryActor, std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    RuntimeBuilder::multi_thread()
        // Each parallel fixture owns its pool; retain concurrent execution
        // without multiplying the container's CPU-sized pool per test.
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let daemon = Daemon::open(&path);
            body(daemon.registry.clone(), dir.path().to_path_buf()).await;
            daemon.stop().await;
        });
}

/// What the next daemon's startup does before it admits any run
/// ([`crate::act_runtime::recover_startup_act_engines`]), over the fake
/// engine host: interrupt every non-terminal record, retire its engine,
/// then seal the startup window.
pub async fn startup_recovery(
    registry: &RegistryActor,
    backend: &FakeBackend,
) -> (Vec<String>, Vec<(String, String)>) {
    let (mut retired, mut failed) = (Vec::new(), Vec::new());
    let mut after = None;
    loop {
        let ActRegistryReply::Recovery(page) = registry
            .act_registry(ActRegistryCommand::Pending {
                after_run_id: after.clone(),
                limit: 16,
            })
            .await
            .unwrap()
        else {
            panic!("recovery page");
        };
        for stale in page.items {
            let run = stale.intent.run_id.clone();
            commit(
                registry,
                ActRegistryCommand::StartupInterrupt {
                    run: run.clone(),
                    at: later(&stale),
                },
            )
            .await
            .unwrap();
            let current = record_of(registry, &run).await;
            match backend
                .retire(registry, OWNER, &current, Duration::from_secs(5))
                .await
            {
                Ok(()) => retired.push(run),
                Err(error) => failed.push((run, error)),
            }
        }
        match page.next_run_id {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    commit(registry, ActRegistryCommand::SealStartup)
        .await
        .unwrap();
    (retired, failed)
}

pub async fn record_of(registry: &RegistryActor, run: &str) -> ActEngineRecord {
    match registry
        .act_registry(ActRegistryCommand::Get { run: run.into() })
        .await
        .unwrap()
    {
        ActRegistryReply::Record(Some(record)) => *record,
        other => panic!("no record for {run}: {other:?}"),
    }
}

async fn record(registry: &RegistryActor, _dir: &std::path::Path, run: &str) -> ActEngineRecord {
    record_of(registry, run).await
}

pub fn terminal(record: &ActEngineRecord, outcome: ActRunOutcome) {
    assert_eq!(record.state, ActEngineState::Terminal, "{record:?}");
    assert_eq!(record.outcome, Some(outcome));
    assert!(record.removal.is_some(), "removal receipt");
}

#[test]
fn success_and_failure_end_terminal_with_removal_receipts() {
    with_registry(|registry, dir| async move {
        for (n, code, outcome) in [(1, 0, ActRunOutcome::Passed), (2, 1, ActRunOutcome::Failed)] {
            let backend = FakeBackend::with(Faults {
                exit_code: code,
                ..Faults::default()
            });
            let mut seen = Collect::default();
            let report = run_on_engine(
                &registry,
                &backend,
                &plan(&run_id(n), Duration::from_secs(5)),
                &CancellationSource::new().token(),
                &mut seen,
            )
            .await;
            assert_eq!(report.execution, ExecutionEnd::Exited(code));
            assert_eq!(report.cleanup, CleanupEnd::Removed);
            assert_eq!(backend.live(), 0, "no engine left");
            assert_eq!(
                seen.lines.len(),
                4,
                "three stdout lines and one stderr line"
            );
            assert!(seen.listing.is_some());
            terminal(&record(&registry, &dir, &run_id(n)).await, outcome);
        }
    });
}

#[test]
fn the_tool_cache_is_saved_before_removal_and_a_failed_save_changes_nothing() {
    with_registry(|registry, dir| async move {
        for (n, code, save_fails) in [(1, 1, false), (2, 0, true)] {
            let backend = FakeBackend::with(Faults {
                exit_code: code,
                save: save_fails,
                ..Faults::default()
            });
            let mut seen = Collect::default();
            let report = run_on_engine(
                &registry,
                &backend,
                &plan(&run_id(n), Duration::from_secs(5)),
                &CancellationSource::new().token(),
                &mut seen,
            )
            .await;
            assert_eq!(
                *backend.saved_while_live.lock().unwrap(),
                1,
                "saved once, before the engine is removed, whatever the outcome"
            );
            assert_eq!(report.execution, ExecutionEnd::Exited(code));
            assert_eq!(report.cleanup, CleanupEnd::Removed);
            assert_eq!(backend.live(), 0);
            if save_fails {
                assert!(
                    seen.notes.iter().any(|note| note.contains("tool cache")),
                    "{:?}",
                    seen.notes
                );
            }
        }
        let _ = dir;
    });
}

#[test]
fn every_phase_reports_how_long_it_took() {
    with_registry(|registry, _dir| async move {
        let backend = FakeBackend::default();
        let mut seen = Collect::default();
        run_on_engine(
            &registry,
            &backend,
            &plan(&run_id(1), Duration::from_secs(5)),
            &CancellationSource::new().token(),
            &mut seen,
        )
        .await;
        for phase in [
            "engine created in ",
            "engine prepared in ",
            "act finished in ",
            "tool cache saved in ",
            "engine removed in ",
        ] {
            assert!(
                seen.notes
                    .iter()
                    .any(|n| n.starts_with(phase) && n.contains('s')),
                "{phase}: {:?}",
                seen.notes
            );
        }
        assert!(
            seen.notes.iter().any(|n| n.contains("total ")),
            "{:?}",
            seen.notes
        );
    });
}

#[test]
fn every_fault_point_ends_terminal_or_cleanup_required() {
    with_registry(|registry, dir| async move {
        let cases = [
            // #554: a failure before `docker create` was sent retires to terminal.
            (
                Faults {
                    pre_create: true,
                    ..Faults::default()
                },
                ActRunOutcome::Failed,
            ),
            (
                Faults {
                    create: true,
                    ..Faults::default()
                },
                ActRunOutcome::Failed,
            ),
            (
                Faults {
                    create_after_side_effect: true,
                    ..Faults::default()
                },
                ActRunOutcome::Failed,
            ),
            (
                Faults {
                    prepare: true,
                    ..Faults::default()
                },
                ActRunOutcome::Failed,
            ),
            (
                Faults {
                    hang: true,
                    ..Faults::default()
                },
                ActRunOutcome::Failed,
            ), // timeout
        ];
        for (i, (faults, outcome)) in cases.into_iter().enumerate() {
            let run = run_id(10 + i as u32);
            let backend = FakeBackend::with(faults);
            let report = run_on_engine(
                &registry,
                &backend,
                &plan(&run, Duration::from_millis(200)),
                &CancellationSource::new().token(),
                &mut Collect::default(),
            )
            .await;
            if faults.create {
                assert!(
                    matches!(report.cleanup, CleanupEnd::Failed(_)),
                    "{report:?}"
                );
                assert_eq!(
                    record(&registry, &dir, &run).await.state,
                    ActEngineState::CleanupRequired
                );
            } else {
                assert_eq!(report.cleanup, CleanupEnd::Removed, "case {i}: {report:?}");
                terminal(&record(&registry, &dir, &run).await, outcome);
            }
            assert_ne!(report.execution, ExecutionEnd::Exited(0));
            assert_eq!(
                *backend.saved_while_live.lock().unwrap(),
                0,
                "uncertain execution must not publish shared tools: case {i}"
            );
            assert_eq!(backend.live(), 0, "case {i}");
        }
    });
}

#[test]
fn cancellation_mid_run_is_cancelled_and_cleaned() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            hang: true,
            ..Faults::default()
        }));
        let source = CancellationSource::new();
        let token = source.token();
        let run = run_id(20);
        let canceller = async {
            let started = async_engine::Deadline::after(Duration::from_secs(5));
            while *backend.executions.lock().unwrap() == 0 {
                assert!(!started.is_elapsed(), "workflow never reached execution");
                async_engine::sleep(Duration::from_millis(10)).await;
            }
            source.cancel();
        };
        let (report, ()) = async_engine::join(
            run_on_engine(
                &registry,
                backend.as_ref(),
                &plan(&run, Duration::from_secs(30)),
                &token,
                &mut Collect::default(),
            ),
            canceller,
        )
        .await;
        assert_eq!(report.execution, ExecutionEnd::Cancelled);
        assert_eq!(
            *backend.saved_while_live.lock().unwrap(),
            0,
            "cancelled launching client does not prove tool writers quiescent"
        );
        assert_eq!(report.cleanup, CleanupEnd::Removed);
        terminal(
            &record(&registry, &dir, &run).await,
            ActRunOutcome::Cancelled,
        );
    });
}

/// Restart the daemon over the same registry and run its startup recovery.
pub async fn restarted(
    path: &std::path::Path,
    backend: &FakeBackend,
) -> (Daemon, Vec<String>, Vec<(String, String)>) {
    let daemon = Daemon::open(path);
    let (retired, failed) = startup_recovery(&daemon.registry, backend).await;
    (daemon, retired, failed)
}

pub fn with_daemon<F, Fut>(body: F)
where
    F: FnOnce(Daemon, std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async { body(Daemon::open(&path), path.clone()).await });
}

#[test]
fn failed_cleanup_is_never_success_and_startup_recovery_finishes_it() {
    with_daemon(|daemon, path| async move {
        let backend = FakeBackend::with(Faults {
            remove: true,
            ..Faults::default()
        });
        let run = run_id(30);
        let report = run_on_engine(
            &daemon.registry,
            &backend,
            &plan(&run, Duration::from_secs(5)),
            &CancellationSource::new().token(),
            &mut Collect::default(),
        )
        .await;
        assert_eq!(report.execution, ExecutionEnd::Exited(0));
        assert!(matches!(report.cleanup, CleanupEnd::Failed(_)));
        let pending = record_of(&daemon.registry, &run).await;
        assert_eq!(pending.state, ActEngineState::CleanupRequired);
        assert_eq!(backend.live(), 1);
        daemon.stop().await;
        backend.faults.lock().unwrap().remove = false;
        let (daemon, retired, failed) = restarted(&path, &backend).await;
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(retired, std::slice::from_ref(&run));
        assert_eq!(backend.live(), 0);
        // The run passed but cleanup needed recovery: the record keeps
        // the passed outcome with a removal receipt.
        terminal(
            &record_of(&daemon.registry, &run).await,
            ActRunOutcome::Passed,
        );
        daemon.stop().await;
    });
}

#[test]
fn daemon_restart_mid_run_is_recovered_as_interrupted() {
    with_daemon(|daemon, path| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            hang: true,
            ..Faults::default()
        }));
        // 50 runs interrupted mid-execution (the future is dropped, as a
        // daemon SIGKILL would), plus one stuck before its engine exists.
        let tasks: Vec<_> = (0..50)
            .map(|n| {
                let registry = daemon.registry.clone();
                let backend = Arc::clone(&backend);
                async_engine::launch(async move {
                    run_on_engine(
                        &registry,
                        backend.as_ref(),
                        &plan(&run_id(100 + n), Duration::from_secs(3600)),
                        &CancellationSource::new().token(),
                        &mut Collect::default(),
                    )
                    .await
                })
            })
            .collect();
        while *backend.executions.lock().unwrap() < 50 {
            async_engine::sleep(Duration::from_millis(5)).await;
        }
        for task in &tasks {
            task.cancel();
        }
        for task in tasks {
            assert!(task.await.is_err(), "run must have been in flight");
        }
        // Run 200's `docker create` was sent and never resolved; run 201 died before sending it.
        for command in [
            ActRegistryCommand::Begin(intent(&run_id(200))),
            ActRegistryCommand::CreateRequested {
                run: run_id(200),
                at: 1.0,
            },
            ActRegistryCommand::Begin(intent(&run_id(201))),
        ] {
            commit(&daemon.registry, command).await.unwrap();
        }
        assert_eq!(backend.live(), 50);
        daemon.stop().await;
        backend.faults.lock().unwrap().hang = false;
        let (daemon, retired, failed) = restarted(&path, &backend).await;
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, run_id(200));
        assert_eq!(retired.len(), 51, "#554: an unsent creation retires");
        assert_eq!(
            record_of(&daemon.registry, &run_id(200)).await.state,
            ActEngineState::CleanupRequired
        );
        assert_eq!(
            record_of(&daemon.registry, &run_id(201)).await.state,
            ActEngineState::Terminal
        );
        assert_eq!(backend.live(), 0, "no orphaned engine");
        for n in (0..50).map(|n| run_id(100 + n)) {
            terminal(
                &record_of(&daemon.registry, &n).await,
                ActRunOutcome::Interrupted,
            );
        }
        daemon.stop().await;
        let (daemon, retired, failed) = restarted(&path, &backend).await;
        assert!(retired.is_empty(), "observed engines stay terminal");
        assert_eq!(failed.len(), 1, "uncertain creation remains quarantined");
        daemon.stop().await;
    });
}

#[test]
fn a_live_run_is_never_interrupted_once_runs_are_admitted() {
    with_registry(|registry, _dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            hang: true,
            ..Faults::default()
        }));
        let live = {
            let registry = registry.clone();
            let backend = Arc::clone(&backend);
            async_engine::launch(async move {
                run_on_engine(
                    &registry,
                    backend.as_ref(),
                    &plan(&run_id(301), Duration::from_secs(3600)),
                    &CancellationSource::new().token(),
                    &mut Collect::default(),
                )
                .await
            })
        };
        while *backend.executions.lock().unwrap() < 1 {
            async_engine::sleep(Duration::from_millis(5)).await;
        }
        // Admitting the run sealed the startup window: recovery authority
        // over its record is gone for this daemon's lifetime.
        assert!(
            registry
                .act_registry(ActRegistryCommand::StartupInterrupt {
                    run: run_id(301),
                    at: now_seconds(),
                })
                .await
                .is_err()
        );
        assert_eq!(backend.live(), 1, "the live run's engine is untouched");
        live.cancel();
    });
}

#[test]
fn foreign_container_with_the_engine_name_is_never_removed() {
    with_daemon(|daemon, path| async move {
        let run = run_id(40);
        let backend = FakeBackend::default();
        commit(&daemon.registry, ActRegistryCommand::Begin(intent(&run)))
            .await
            .unwrap();
        // Something else took the deterministic name, without our labels.
        backend.insert(
            &intent(&run).engine_name(),
            crate::act_engine::ENGINE_MANIFEST,
            BTreeMap::new(),
        );
        daemon.stop().await;
        let (daemon, _, failed) = restarted(&path, &backend).await;
        assert_eq!(failed.len(), 1);
        assert_eq!(backend.live(), 1, "foreign container left alone");
        let r = record_of(&daemon.registry, &run).await;
        assert_eq!(r.state, ActEngineState::CleanupRequired);
        daemon.stop().await;
    });
}

#[test]
fn inspect_outage_during_cleanup_is_reported_not_assumed_absent() {
    with_daemon(|daemon, path| async move {
        let backend = FakeBackend::with(Faults {
            inspect: true,
            ..Faults::default()
        });
        let run = run_id(51);
        commit(&daemon.registry, ActRegistryCommand::Begin(intent(&run)))
            .await
            .unwrap();
        daemon.stop().await;
        let (daemon, _, failed) = restarted(&path, &backend).await;
        assert_eq!(failed.len(), 1);
        assert_eq!(
            record_of(&daemon.registry, &run).await.state,
            ActEngineState::CleanupRequired,
            "an unreadable engine is never assumed absent"
        );
        daemon.stop().await;
    });
}

#[test]
fn every_engine_step_holds_the_execution_claim() {
    with_registry(|registry, _dir| async move {
        let backend = FakeBackend::default();
        let run = run_id(60);
        let report = run_on_engine(
            &registry,
            &backend,
            &plan(&run, Duration::from_secs(5)),
            &CancellationSource::new().token(),
            &mut Collect::default(),
        )
        .await;
        assert_eq!(report.execution, ExecutionEnd::Exited(0));
        let done = record_of(&registry, &run).await;
        assert_eq!(done.schema_version, 3);
        assert!(
            done.execution_claim.is_some(),
            "the run executed under a claim"
        );
        assert_eq!(done.execution, Some(ActRunOutcome::Passed));
        assert_eq!(
            done.intent
                .creation_profile
                .as_ref()
                .and_then(|profile| profile.cache_volume.as_ref())
                .map(|cache| cache.target.as_str()),
            Some(super::super::engine::ENGINE_CACHE),
            "the cache volume mount is frozen into the creation profile"
        );
    });
}

mod storage;

#[test]
fn delayed_create_after_empty_lookup_stays_recoverable() {
    with_registry(|registry, dir| async move {
        let backend = FakeBackend::with(Faults {
            create: true,
            ..Faults::default()
        });
        let run = run_id(95);
        let engine_plan = plan(&run, Duration::from_secs(30));
        let report = run_on_engine(
            &registry,
            &backend,
            &engine_plan,
            &CancellationSource::new().token(),
            &mut Collect::default(),
        )
        .await;
        assert!(
            matches!(report.cleanup, CleanupEnd::Failed(_)),
            "{report:?}"
        );
        let pending = record(&registry, &dir, &run).await;
        assert_eq!(pending.state, ActEngineState::CleanupRequired);
        assert!(pending.removal.is_none());
        // Docker's server finishes the request after the client timed out and
        // the first cleanup lookup returned no container. No sleeps or races.
        let owner = registry_owner(&registry).await.unwrap();
        backend.insert(
            &engine_plan.intent.engine_name(),
            &engine_plan.intent.engine_image_digest,
            engine_plan.intent.required_labels(&owner).unwrap(),
        );
        assert_eq!(backend.live(), 1);
        cleanup(
            &registry,
            &backend,
            &owner,
            &run,
            None,
            ActRunOutcome::Failed,
            &mut Clock(engine_plan.intent.created_at),
        )
        .await
        .unwrap();
        assert_eq!(backend.live(), 0);
        terminal(&record(&registry, &dir, &run).await, ActRunOutcome::Failed);
    });
}
