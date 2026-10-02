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
    ActArtifact, ActEngineBackend, ActInvocation, CacheVolume, EngineLine, EngineSpec, ExecEnd,
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
    pub cache: CacheVolume,
    /// The run's deadline, fixed when the run started: planning, prepare,
    /// the job listing and execution all count against it.
    pub deadline: async_engine::Deadline,
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
    /// Called every [`PROGRESS_TICK`] while no line arrives, so a silent
    /// step's progress still becomes visible.
    fn tick(&mut self) {}
}

/// How often a quiet execution gives the observer a chance to publish.
pub const PROGRESS_TICK: Duration = Duration::from_millis(250);

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
        if let Err(error) = backend.ensure_cache(&plan.cache).await {
            break 'run ExecutionEnd::EngineFailed(error);
        }
        observer.note("creating isolated engine");
        if let Err(error) = backend
            .create(&EngineSpec {
                name: name.clone(),
                labels,
                cache: plan.cache.clone(),
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
        let deadline = plan.deadline;
        observer.note("preparing engine: act, frozen source, runner image");
        let prepared = async_engine::timeout_at(
            deadline,
            async_engine::cancellable(
                cancellation,
                backend.prepare(&name, &plan.source, &plan.event, plan.act),
            ),
        )
        .await;
        match prepared {
            Err(_) => break 'run ExecutionEnd::TimedOut,
            Ok(Err(_)) => break 'run ExecutionEnd::Cancelled,
            Ok(Ok(Err(error))) => break 'run ExecutionEnd::EngineFailed(error),
            Ok(Ok(Ok(()))) => {}
        }
        match async_engine::timeout_at(deadline, backend.list(&name, &plan.invocation.workflow))
            .await
        {
            Err(_) => break 'run ExecutionEnd::TimedOut,
            Ok(Ok(listing)) => observer.declared(&listing),
            Ok(Err(error)) => break 'run ExecutionEnd::EngineFailed(error),
        }
        observer.note("running act on the isolated engine");
        let (lines, mut receiver) = async_engine::channel(512);
        let execute = async {
            let end = backend
                .execute(
                    &name,
                    &plan.invocation,
                    deadline.remaining(),
                    cancellation,
                    &lines,
                )
                .await;
            drop(lines);
            end
        };
        let drain = async {
            loop {
                match async_engine::timeout(PROGRESS_TICK, receiver.recv()).await {
                    Ok(Some(line)) => observer.line(line),
                    Ok(None) => break,
                    Err(_) => observer.tick(),
                }
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
pub(crate) mod tests;
