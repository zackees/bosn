//! One act run on one isolated engine, end to end, through the registry.
//!
//! Order of effects (each registry step is durable before the next engine
//! call): intent -> create -> observe + register -> prepare -> execute ->
//! execution outcome -> cleanup request -> authorize -> remove -> proven
//! absence -> terminal record. Cleanup is part of success: a run whose engine
//! could not be proven gone is never reported as passing, and its record
//! stays `cleanup_required` for the next recovery pass.

use std::{path::PathBuf, time::Duration};

use bosn_registry::act::{
    ActEngineIntent, ActEngineRecord, ActEngineRemovalProof, ActEngineState, ActRunOutcome,
};
use kernal_api::async_engine::{self, CancellationToken};

use super::engine::{
    ActArtifact, ActEngineBackend, ActInvocation, EngineLine, EngineSpec, ExecEnd,
};
use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};

/// Everything fixed before the engine exists.
#[derive(Clone, Debug)]
pub struct EnginePlan {
    pub intent: ActEngineIntent,
    /// The pinned act build recorded in the intent's `act_image_digest`.
    pub act: ActArtifact,
    pub source: PathBuf,
    pub event: PathBuf,
    pub invocation: ActInvocation,
    pub deadline: Duration,
}

/// How the workflow execution itself ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionEnd {
    /// act exited with this code.
    Exited(i32),
    TimedOut,
    Cancelled,
    /// The engine could not be created, observed, registered or prepared.
    EngineFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupEnd {
    /// The engine was removed (or never existed) and its absence proven.
    Removed,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineReport {
    pub execution: ExecutionEnd,
    pub cleanup: CleanupEnd,
    /// The engine's immutable ID once observed.
    pub engine_id: Option<String>,
}

/// Receives the declared-job listing and every output line, in order.
pub trait EngineObserver: Send {
    fn note(&mut self, text: &str);
    fn declared(&mut self, listing: &str);
    fn line(&mut self, line: EngineLine);
}

/// Registry transition times must never go backwards.
struct Clock(f64);
impl Clock {
    fn now(&mut self) -> f64 {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        self.0 = self.0.max(wall);
        self.0
    }
}

pub fn now_seconds() -> f64 {
    Clock(0.0).now()
}

async fn commit(registry: &RegistryActor, command: ActRegistryCommand) -> Result<(), String> {
    registry
        .act_registry(command)
        .await
        .map(|_| ())
        .map_err(|e| format!("registry: {e}"))
}

/// Run one workflow on a fresh isolated engine. Never panics on engine
/// faults; every path ends in a cleanup attempt whose result is reported.
pub async fn run_on_engine(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    plan: &EnginePlan,
    cancellation: &CancellationToken,
    observer: &mut dyn EngineObserver,
) -> EngineReport {
    let run = plan.intent.run_id.clone();
    let name = plan.intent.engine_name();
    let mut clock = Clock(plan.intent.created_at);
    // The intent is durable before anything exists on the host engine.
    if let Err(error) = commit(registry, ActRegistryCommand::Begin(plan.intent.clone())).await {
        return EngineReport {
            execution: ExecutionEnd::EngineFailed(error),
            cleanup: CleanupEnd::Removed,
            engine_id: None,
        };
    }
    let mut engine_id = None;
    let execution = 'run: {
        let labels = match registry_labels(registry, &plan.intent).await {
            Ok(labels) => labels,
            Err(error) => break 'run ExecutionEnd::EngineFailed(error),
        };
        observer.note("creating isolated engine");
        if let Err(error) = backend
            .create(&EngineSpec {
                name: name.clone(),
                labels,
            })
            .await
        {
            break 'run ExecutionEnd::EngineFailed(error);
        }
        let observed = match backend.inspect(&name).await {
            Ok(Some(observed)) => observed,
            Ok(None) => {
                break 'run ExecutionEnd::EngineFailed("engine vanished after creation".into());
            }
            Err(error) => break 'run ExecutionEnd::EngineFailed(error),
        };
        let id = observed.engine_id.clone();
        if let Err(error) = commit(
            registry,
            ActRegistryCommand::Register {
                run: run.clone(),
                observed,
                at: clock.now(),
            },
        )
        .await
        {
            break 'run ExecutionEnd::EngineFailed(error);
        }
        engine_id = Some(id);
        if cancellation.is_cancelled() {
            break 'run ExecutionEnd::Cancelled;
        }
        observer.note("preparing engine: act, frozen source, runner image");
        let prepared = async_engine::cancellable(
            cancellation,
            backend.prepare(&name, &plan.source, &plan.event, plan.act),
        )
        .await;
        match prepared {
            Err(_) => break 'run ExecutionEnd::Cancelled,
            Ok(Err(error)) => break 'run ExecutionEnd::EngineFailed(error),
            Ok(Ok(())) => {}
        }
        match backend.list(&name, &plan.invocation.workflow).await {
            Ok(listing) => observer.declared(&listing),
            Err(error) => break 'run ExecutionEnd::EngineFailed(error),
        }
        observer.note("running act on the isolated engine");
        let (lines, mut receiver) = async_engine::channel(512);
        let execute = async {
            let end = backend
                .execute(&name, &plan.invocation, plan.deadline, cancellation, &lines)
                .await;
            drop(lines);
            end
        };
        let drain = async {
            while let Some(line) = receiver.recv().await {
                observer.line(line);
            }
        };
        let (end, ()) = async_engine::join(execute, drain).await;
        let end = match end {
            Ok(ExecEnd::Exited(code)) => ExecutionEnd::Exited(code),
            Ok(ExecEnd::TimedOut) => ExecutionEnd::TimedOut,
            Ok(ExecEnd::Cancelled) => ExecutionEnd::Cancelled,
            Err(error) => ExecutionEnd::EngineFailed(error),
        };
        let outcome = registry_outcome(&end);
        if let Err(error) = commit(
            registry,
            ActRegistryCommand::Execution {
                run: run.clone(),
                outcome,
                at: clock.now(),
            },
        )
        .await
        {
            break 'run ExecutionEnd::EngineFailed(error);
        }
        end
    };
    let outcome = registry_outcome(&execution);
    let cleanup = match commit(
        registry,
        ActRegistryCommand::Cleanup {
            run: run.clone(),
            outcome,
            at: clock.now(),
        },
    )
    .await
    {
        Err(error) => CleanupEnd::Failed(error),
        Ok(()) => {
            observer.note("removing isolated engine");
            match retire(
                registry,
                backend,
                &run,
                &name,
                engine_id.clone(),
                &mut clock,
            )
            .await
            {
                Ok(()) => CleanupEnd::Removed,
                Err(error) => CleanupEnd::Failed(error),
            }
        }
    };
    EngineReport {
        execution,
        cleanup,
        engine_id,
    }
}

fn registry_outcome(end: &ExecutionEnd) -> ActRunOutcome {
    match end {
        ExecutionEnd::Exited(0) => ActRunOutcome::Passed,
        ExecutionEnd::Cancelled => ActRunOutcome::Cancelled,
        _ => ActRunOutcome::Failed,
    }
}

async fn registry_labels(
    registry: &RegistryActor,
    intent: &ActEngineIntent,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let status = registry
        .status()
        .await
        .map_err(|e| format!("registry: {e}"))?;
    intent
        .required_labels(&status.registry_id)
        .map_err(|e| format!("registry: {e}"))
}

/// Remove an engine whose record is `cleanup_required`, proving absence
/// before the terminal record. A container holding the name without the
/// exact ownership labels is never removed.
async fn retire(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    run: &str,
    name: &str,
    mut engine_id: Option<String>,
    clock: &mut Clock,
) -> Result<(), String> {
    match backend.inspect(name).await? {
        None => {}
        Some(observed) => {
            if engine_id.is_none() {
                // Created but never registered (crash or failed register):
                // capture its identity for cleanup only.
                commit(
                    registry,
                    ActRegistryCommand::Recover {
                        run: run.into(),
                        observed: observed.clone(),
                        at: clock.now(),
                    },
                )
                .await
                .map_err(|e| format!("engine {name} is not provably ours: {e}"))?;
                engine_id = Some(observed.engine_id.clone());
            }
            let observed_id = observed.engine_id.clone();
            let reply = registry
                .act_registry(ActRegistryCommand::Authorize {
                    run: run.into(),
                    observed,
                })
                .await
                .map_err(|e| format!("cleanup not authorized: {e}"))?;
            if !matches!(reply, ActRegistryReply::Authorized(_)) {
                return Err("cleanup not authorized".into());
            }
            backend.remove(&observed_id).await?;
            if backend.inspect(name).await?.is_some() {
                return Err(format!("engine {name} still exists after removal"));
            }
        }
    }
    commit(
        registry,
        ActRegistryCommand::Finalize {
            run: run.into(),
            proof: ActEngineRemovalProof {
                name: name.into(),
                engine_id,
            },
            at: clock.now(),
        },
    )
    .await
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub retired: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Every non-terminal engine record right now. Take this snapshot before the
/// daemon admits any run, then [`reconcile`] exactly it: a run started later
/// can never be mistaken for one a previous daemon left behind.
pub async fn pending_records(registry: &RegistryActor) -> Result<Vec<ActEngineRecord>, String> {
    let mut records = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = match registry
            .act_registry(ActRegistryCommand::Pending {
                after_run_id: after.clone(),
                limit: 64,
            })
            .await
            .map_err(|e| format!("recovery page: {e}"))?
        {
            ActRegistryReply::Recovery(page) => page,
            _ => return Err("recovery page: unexpected reply".into()),
        };
        records.extend(page.items);
        match page.next_run_id {
            Some(next) => after = Some(next),
            None => return Ok(records),
        }
    }
}

/// Interrupted runs are marked `interrupted`; their engines are removed
/// after the same ownership checks as a normal cleanup.
pub async fn reconcile(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    records: &[ActEngineRecord],
) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    for record in records {
        let run = record.intent.run_id.clone();
        match recover_one(registry, backend, record).await {
            Ok(()) => report.retired.push(run),
            Err(error) => report.failed.push((run, error)),
        }
    }
    report
}

/// Snapshot and reconcile in one step (only safe while no run can start).
pub async fn recover(registry: &RegistryActor, backend: &dyn ActEngineBackend) -> RecoveryReport {
    match pending_records(registry).await {
        Ok(records) => reconcile(registry, backend, &records).await,
        Err(error) => RecoveryReport {
            retired: Vec::new(),
            failed: vec![("*".into(), error)],
        },
    }
}

async fn recover_one(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    record: &ActEngineRecord,
) -> Result<(), String> {
    let run = &record.intent.run_id;
    let mut clock = Clock(record.updated_at);
    if matches!(
        record.state,
        ActEngineState::Pending | ActEngineState::Registered
    ) {
        commit(
            registry,
            ActRegistryCommand::Cleanup {
                run: run.clone(),
                outcome: ActRunOutcome::Interrupted,
                at: clock.now(),
            },
        )
        .await?;
    }
    retire(
        registry,
        backend,
        run,
        &record.intent.engine_name(),
        record.engine_id.clone(),
        &mut clock,
    )
    .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{Registry, registry_actor};
    use bosn_registry::act::ActEngineObservation;
    use kernal_api::async_engine::{CancellationSource, RuntimeBuilder};
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    /// Fault points for the synthetic engine.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Faults {
        pub create: bool,
        /// Create the container, then report failure (partial creation).
        pub create_after_side_effect: bool,
        pub prepare: bool,
        pub exit_code: i32,
        pub hang: bool,
        pub remove: bool,
        pub inspect: bool,
    }

    /// In-memory engine host: name -> (id, labels).
    #[derive(Default)]
    pub struct FakeBackend {
        pub engines: Mutex<BTreeMap<String, ActEngineObservation>>,
        pub faults: Mutex<Faults>,
        pub executions: Mutex<u32>,
        next: Mutex<u64>,
    }
    impl FakeBackend {
        pub fn with(faults: Faults) -> Self {
            let backend = Self::default();
            *backend.faults.lock().unwrap() = faults;
            backend
        }
        fn faults(&self) -> Faults {
            *self.faults.lock().unwrap()
        }
        pub fn live(&self) -> usize {
            self.engines.lock().unwrap().len()
        }
        fn insert(&self, spec: &EngineSpec) {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            self.engines.lock().unwrap().insert(
                spec.name.clone(),
                ActEngineObservation {
                    name: spec.name.clone(),
                    engine_id: format!("{:064x}", *next),
                    image_digest: format!("sha256:{}", "e".repeat(64)),
                    labels: spec.labels.clone(),
                },
            );
        }
    }
    pub const LISTING: &str = "Stage  Job ID  Job name  Workflow name  Workflow file  Events\n\
                               0      a       a         w              ci.yml         push\n";
    impl ActEngineBackend for FakeBackend {
        fn resolve_engine_image(
            &self,
        ) -> super::super::engine::BoxFuture<'_, Result<String, String>> {
            Box::pin(async { Ok(format!("sha256:{}", "e".repeat(64))) })
        }
        fn create<'a>(
            &'a self,
            spec: &'a EngineSpec,
        ) -> super::super::engine::BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                let f = self.faults();
                if f.create {
                    return Err("synthetic create failure".into());
                }
                self.insert(spec);
                if f.create_after_side_effect {
                    return Err("synthetic create failure after side effect".into());
                }
                Ok(())
            })
        }
        fn inspect<'a>(
            &'a self,
            name: &'a str,
        ) -> super::super::engine::BoxFuture<'a, Result<Option<ActEngineObservation>, String>>
        {
            Box::pin(async move {
                if self.faults().inspect {
                    return Err("synthetic inspect failure".into());
                }
                Ok(self.engines.lock().unwrap().get(name).cloned())
            })
        }
        fn prepare<'a>(
            &'a self,
            _name: &'a str,
            _source: &'a std::path::Path,
            _event: &'a std::path::Path,
            _act: ActArtifact,
        ) -> super::super::engine::BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if self.faults().prepare {
                    return Err("synthetic prepare failure".into());
                }
                Ok(())
            })
        }
        fn list<'a>(
            &'a self,
            _name: &'a str,
            _workflow: &'a str,
        ) -> super::super::engine::BoxFuture<'a, Result<String, String>> {
            Box::pin(async { Ok(LISTING.to_string()) })
        }
        fn execute<'a>(
            &'a self,
            _name: &'a str,
            _invocation: &'a ActInvocation,
            deadline: Duration,
            cancellation: &'a CancellationToken,
            lines: &'a async_engine::Sender<EngineLine>,
        ) -> super::super::engine::BoxFuture<'a, Result<ExecEnd, String>> {
            Box::pin(async move {
                *self.executions.lock().unwrap() += 1;
                let f = self.faults();
                let _ = lines
                    .send(EngineLine::Stdout(
                        r#"{"job":"w/a","jobID":"a","msg":"⭐ Run Main x","stage":"Main","stepID":["0"]}"#.into(),
                    ))
                    .await;
                if f.hang {
                    return match async_engine::timeout(
                        deadline,
                        async_engine::cancellable(
                            cancellation,
                            async_engine::sleep(Duration::from_secs(3600)),
                        ),
                    )
                    .await
                    {
                        Err(_) => Ok(ExecEnd::TimedOut),
                        Ok(_) => Ok(ExecEnd::Cancelled),
                    };
                }
                let result = if f.exit_code == 0 {
                    "success"
                } else {
                    "failure"
                };
                let _ = lines
                    .send(EngineLine::Stdout(format!(
                        r#"{{"job":"w/a","jobID":"a","msg":"done","stage":"Main","stepID":["0"],"stepResult":"{result}"}}"#
                    )))
                    .await;
                let _ = lines
                    .send(EngineLine::Stdout(format!(
                        r#"{{"job":"w/a","jobID":"a","msg":"🏁","jobResult":"{result}"}}"#
                    )))
                    .await;
                Ok(ExecEnd::Exited(f.exit_code))
            })
        }
        fn remove<'a>(
            &'a self,
            engine_id: &'a str,
        ) -> super::super::engine::BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if self.faults().remove {
                    return Err("synthetic removal failure".into());
                }
                self.engines
                    .lock()
                    .unwrap()
                    .retain(|_, engine| engine.engine_id != engine_id);
                Ok(())
            })
        }
    }

    #[derive(Default)]
    pub struct Collect {
        pub lines: Vec<EngineLine>,
        pub listing: Option<String>,
    }
    impl EngineObserver for Collect {
        fn note(&mut self, _text: &str) {}
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
            engine_image_digest: format!("sha256:{}", "e".repeat(64)),
            runner_image_digest: format!("sha256:{}", "f".repeat(64)),
            created_at: 1.0,
        }
    }
    fn plan(run: &str, deadline: Duration) -> EnginePlan {
        EnginePlan {
            intent: intent(run),
            act: super::super::engine::act_artifact("x86_64").unwrap(),
            source: "/nonexistent".into(),
            event: "/nonexistent".into(),
            invocation: ActInvocation {
                event: "push".into(),
                workflow: ".github/workflows/ci.yml".into(),
                job: None,
            },
            deadline,
        }
    }
    pub fn run_id(n: u32) -> String {
        format!("aaaaaaaa-bbbb-4ccc-8ddd-{n:012x}")
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
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let registry =
                    Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
                let (sender, receiver) = async_engine::channel(16);
                let actor = RegistryActor { sender };
                let task = async_engine::launch(registry_actor(registry, receiver, None));
                body(actor.clone(), dir.path().to_path_buf()).await;
                actor.stop().await;
                let _ = task.await;
            });
    }

    async fn record(
        registry: &RegistryActor,
        _dir: &std::path::Path,
        run: &str,
    ) -> ActEngineRecord {
        match registry
            .act_registry(ActRegistryCommand::Get { run: run.into() })
            .await
            .unwrap()
        {
            ActRegistryReply::Record(Some(record)) => *record,
            other => panic!("no record for {run}: {other:?}"),
        }
    }

    fn terminal(record: &ActEngineRecord, outcome: ActRunOutcome) {
        assert_eq!(record.state, ActEngineState::Terminal, "{record:?}");
        assert_eq!(record.outcome, Some(outcome));
        assert!(record.removal.is_some(), "removal receipt");
    }

    #[test]
    fn success_and_failure_end_terminal_with_removal_receipts() {
        with_registry(|registry, dir| async move {
            for (n, code, outcome) in [(1, 0, ActRunOutcome::Passed), (2, 1, ActRunOutcome::Failed)]
            {
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
                assert_eq!(seen.lines.len(), 3);
                assert!(seen.listing.is_some());
                terminal(&record(&registry, &dir, &run_id(n)).await, outcome);
            }
        });
    }

    #[test]
    fn every_fault_point_ends_terminal_or_cleanup_required() {
        with_registry(|registry, dir| async move {
            let cases = [
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
                assert_eq!(report.cleanup, CleanupEnd::Removed, "case {i}: {report:?}");
                assert_ne!(report.execution, ExecutionEnd::Exited(0));
                assert_eq!(backend.live(), 0, "case {i}");
                terminal(&record(&registry, &dir, &run).await, outcome);
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
                async_engine::sleep(Duration::from_millis(100)).await;
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
            assert_eq!(report.cleanup, CleanupEnd::Removed);
            terminal(
                &record(&registry, &dir, &run).await,
                ActRunOutcome::Cancelled,
            );
        });
    }

    #[test]
    fn failed_cleanup_is_never_success_and_recovery_finishes_it() {
        with_registry(|registry, dir| async move {
            let backend = FakeBackend::with(Faults {
                remove: true,
                ..Faults::default()
            });
            let run = run_id(30);
            let report = run_on_engine(
                &registry,
                &backend,
                &plan(&run, Duration::from_secs(5)),
                &CancellationSource::new().token(),
                &mut Collect::default(),
            )
            .await;
            assert_eq!(report.execution, ExecutionEnd::Exited(0));
            assert!(matches!(report.cleanup, CleanupEnd::Failed(_)));
            let pending = record(&registry, &dir, &run).await;
            assert_eq!(pending.state, ActEngineState::CleanupRequired);
            assert_eq!(backend.live(), 1);
            backend.faults.lock().unwrap().remove = false;
            let recovered = recover(&registry, &backend).await;
            assert_eq!(recovered.retired, std::slice::from_ref(&run));
            assert_eq!(backend.live(), 0);
            // The run passed but cleanup needed recovery: the record keeps
            // the passed outcome with a removal receipt.
            terminal(&record(&registry, &dir, &run).await, ActRunOutcome::Passed);
        });
    }

    #[test]
    fn daemon_restart_mid_run_is_reconciled_as_interrupted() {
        with_registry(|registry, dir| async move {
            let backend = Arc::new(FakeBackend::with(Faults {
                hang: true,
                ..Faults::default()
            }));
            // 50 runs interrupted mid-execution (the future is dropped, as a
            // daemon SIGKILL would), plus one stuck before registration.
            let tasks: Vec<_> = (0..50)
                .map(|n| {
                    let registry = registry.clone();
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
            // Every run is mid-execution: drop them all, as a daemon SIGKILL would.
            for task in &tasks {
                task.cancel();
            }
            for task in tasks {
                assert!(task.await.is_err(), "run must have been in flight");
            }
            commit(&registry, ActRegistryCommand::Begin(intent(&run_id(200))))
                .await
                .unwrap();
            assert_eq!(backend.live(), 50);
            backend.faults.lock().unwrap().hang = false;
            let report = recover(&registry, backend.as_ref()).await;
            assert!(report.failed.is_empty(), "{:?}", report.failed);
            assert_eq!(report.retired.len(), 51);
            assert_eq!(backend.live(), 0, "no orphaned engine");
            for n in (0..50).map(|n| run_id(100 + n)).chain([run_id(200)]) {
                terminal(
                    &record(&registry, &dir, &n).await,
                    ActRunOutcome::Interrupted,
                );
            }
            assert!(
                recover(&registry, backend.as_ref())
                    .await
                    .retired
                    .is_empty()
            );
        });
    }

    #[test]
    fn recovery_only_touches_records_from_before_the_daemon_started() {
        with_registry(|registry, _dir| async move {
            let backend = Arc::new(FakeBackend::with(Faults {
                hang: true,
                ..Faults::default()
            }));
            commit(&registry, ActRegistryCommand::Begin(intent(&run_id(300))))
                .await
                .unwrap();
            let stale = pending_records(&registry).await.unwrap();
            assert_eq!(stale.len(), 1);
            // A run that starts after the snapshot sorts after it by UUID too.
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
            let report = reconcile(&registry, backend.as_ref(), &stale).await;
            assert_eq!(report.retired, [run_id(300)]);
            assert_eq!(backend.live(), 1, "the live run's engine is untouched");
            live.cancel();
        });
    }

    #[test]
    fn foreign_container_with_the_engine_name_is_never_removed() {
        with_registry(|registry, dir| async move {
            let run = run_id(40);
            let backend = FakeBackend::default();
            commit(&registry, ActRegistryCommand::Begin(intent(&run)))
                .await
                .unwrap();
            // Something else took the deterministic name, without our labels.
            backend.insert(&EngineSpec {
                name: intent(&run).engine_name(),
                labels: BTreeMap::new(),
            });
            let report = recover(&registry, &backend).await;
            assert_eq!(report.failed.len(), 1);
            assert_eq!(backend.live(), 1, "foreign container left alone");
            let r = record(&registry, &dir, &run).await;
            assert_eq!(r.state, ActEngineState::CleanupRequired);
        });
    }

    #[test]
    fn inspect_outage_during_cleanup_is_reported_not_assumed_absent() {
        with_registry(|registry, dir| async move {
            let backend = FakeBackend::with(Faults {
                inspect: true,
                ..Faults::default()
            });
            let run = run_id(51);
            commit(&registry, ActRegistryCommand::Begin(intent(&run)))
                .await
                .unwrap();
            let recovered = recover(&registry, &backend).await;
            assert_eq!(recovered.failed.len(), 1);
            assert_eq!(
                record(&registry, &dir, &run).await.state,
                ActEngineState::CleanupRequired,
                "an unreadable engine is never assumed absent"
            );
        });
    }
}
