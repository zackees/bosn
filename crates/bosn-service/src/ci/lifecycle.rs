//! One act run on one isolated engine, end to end, through the registry.
//!
//! Order of effects (each registry step is durable before the next engine
//! call): intent (with its frozen creation profile) -> create -> observe +
//! register -> start (the owned-engine layer, [`crate::act_engine`]) ->
//! exclusive execution claim -> prepare the engine -> prepare the run ->
//! execute (each verified against the claim; the engine's storage sampled
//! throughout) -> execution outcome ->
//! owner cleanup request -> retire (authorize, remove, proven absence,
//! terminal record). Cleanup is part of success: a
//! run whose engine could not be proven gone is never reported as passing,
//! and its record stays `cleanup_required` for the next daemon's startup
//! recovery ([`crate::act_runtime::recover_startup_act_engines`]), the only
//! recovery there is.
//!
//! A run whose plan holds a prepared spare ([`super::spare`], #410) skips
//! everything up to and including preparing the engine: its claim takes the
//! spare over from the daemon's claim, atomically and once. A spare that
//! cannot be claimed is retired and the run creates its own engine.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use bosn_registry::act::{
    ActEngineBinding, ActEngineDockerSocket, ActEngineIntent, ActEngineObservation,
    ActEngineRecord, ActEngineState, ActRunOutcome,
};
use kernal_api::async_engine::{self, CancellationToken};

use super::{
    engine::{ActArtifact, ActEngineBackend, ActInvocation, CacheVolume, EngineLine, ExecEnd},
    spare::Spare,
    storage::{self, StoragePeak, StorageUsage},
};
use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};

/// Everything fixed before the engine exists.
#[derive(Clone, Debug)]
pub struct EnginePlan {
    /// The immutable intent, with the frozen creation profile (limits and
    /// the cache volume mount) the engine is created from and verified by.
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
    /// A prepared spare engine to take over instead of creating one; only
    /// ever one whose intent is [`ActEngineIntent::same_engine`] as `intent`.
    pub spare: Option<Spare>,
    /// The shared engine to make when there is none (#547); `None` runs on
    /// the run's own engine.
    pub shared: Option<super::spare::SparePlan>,
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
    /// The fullest its private storage got while act ran, if sampled.
    pub storage: Option<StorageUsage>,
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

/// Elapsed time per lifecycle phase, for the run's log (where a run's time
/// goes outside act's own steps: engine start, prepare, save, removal).
struct Laps {
    start: Instant,
    last: Instant,
}
impl Laps {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last: now,
        }
    }
    /// Time since the previous lap, e.g. `"14.2s"`.
    fn lap(&mut self) -> String {
        let now = Instant::now();
        let took = now - self.last;
        self.last = now;
        format!("{:.1}s", took.as_secs_f64())
    }
    fn total(&self) -> String {
        format!("{:.1}s", self.start.elapsed().as_secs_f64())
    }
}

/// Keep the run's tool-cache installs for the next run. Best-effort: a failed
/// or slow save is noted and never changes the run's outcome.
async fn save_toolcache(
    backend: &dyn ActEngineBackend,
    name: &str,
    observer: &mut dyn EngineObserver,
    laps: &mut Laps,
) {
    let saved = async_engine::timeout(TOOLCACHE_SAVE_DEADLINE, backend.save_toolcache(name)).await;
    match saved {
        Ok(Ok(())) => observer.note(&format!("tool cache saved in {}", laps.lap())),
        Ok(Err(error)) => observer.note(&format!("tool cache not saved: {error}")),
        Err(_) => observer.note("tool cache not saved: timed out"),
    }
}

/// Sample the engine's storage into `peak`, warning in the log the first
/// time it is low. A failed sample keeps its first error and never changes
/// the run.
async fn sample_storage(
    backend: &dyn ActEngineBackend,
    engine: &str,
    peak: &mut StoragePeak,
    unsampled: &mut Option<String>,
    observer: &mut dyn EngineObserver,
) {
    match backend.storage_usage(engine).await {
        Ok(usage) => {
            if peak.record(usage) {
                observer.note(&storage::low_warning(usage));
            }
        }
        Err(error) => {
            unsampled.get_or_insert(error);
        }
    }
}

/// How long saving the tool cache may delay the engine's removal.
const TOOLCACHE_SAVE_DEADLINE: Duration = Duration::from_secs(120);

/// Registry transition times must never go backwards.
pub(super) struct Clock(pub(super) f64);
impl Clock {
    pub(super) fn now(&mut self) -> f64 {
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

/// The run's latest durable record, if its intent ever committed.
async fn record(registry: &RegistryActor, run: &str) -> Result<Option<ActEngineRecord>, String> {
    match registry
        .act_registry(ActRegistryCommand::Get { run: run.into() })
        .await
        .map_err(|e| format!("registry: {e}"))?
    {
        ActRegistryReply::Record(record) => Ok(record.map(|record| *record)),
        _ => Err("registry: unexpected record reply".into()),
    }
}

/// The registry this daemon owns engines for.
pub(super) async fn registry_owner(registry: &RegistryActor) -> Result<String, String> {
    registry
        .status()
        .await
        .map(|status| status.registry_id)
        .map_err(|e| format!("registry: {e}"))
}

/// A fresh, unguessable execution-claim token.
async fn claim_token() -> Result<String, String> {
    let random = kernal_api::random::SecureRandom::new(1, Duration::from_secs(5))
        .map_err(|e| format!("claim token: {e}"))?;
    let bytes = random
        .bytes(16)
        .await
        .map_err(|e| format!("claim token: {e}"))?;
    Ok(crate::uuid(&bytes))
}

/// The run's exclusive execution claim on its registered engine. Every
/// in-engine step re-verifies it, so an engine whose claim was withdrawn
/// (or whose observation no longer matches) is never driven further.
pub(super) struct Claim<'a> {
    registry: &'a RegistryActor,
    /// The engine's registry key: the run's own UUID, or a spare's.
    run: String,
    pub(super) observed: ActEngineObservation,
    pub(super) token: String,
}
impl<'a> Claim<'a> {
    pub(super) async fn commit(
        registry: &'a RegistryActor,
        intent: &ActEngineIntent,
        observed: ActEngineObservation,
        at: f64,
    ) -> Result<Self, String> {
        let token = claim_token().await?;
        match registry
            .act_registry(ActRegistryCommand::Claim {
                intent: intent.clone(),
                observed: observed.clone(),
                token: token.clone(),
                at,
            })
            .await
            .map_err(|e| format!("registry: {e}"))?
        {
            ActRegistryReply::Claimed(_) => Ok(Self {
                registry,
                run: intent.run_id.clone(),
                observed,
                token,
            }),
            _ => Err("registry did not commit the execution claim".into()),
        }
    }

    /// Take a prepared spare over from the daemon's claim, binding it to
    /// `binding`'s run. Exactly one claim can do this.
    async fn take_spare(
        registry: &'a RegistryActor,
        spare: &Spare,
        binding: ActEngineBinding,
        at: f64,
    ) -> Result<Self, String> {
        let token = claim_token().await?;
        match registry
            .act_registry(ActRegistryCommand::ClaimSpare {
                spare: spare.intent.run_id.clone(),
                observed: spare.observed.clone(),
                from: spare.token.clone(),
                token: token.clone(),
                binding,
                at,
            })
            .await
            .map_err(|e| format!("registry: {e}"))?
        {
            ActRegistryReply::Claimed(_) => Ok(Self {
                registry,
                run: spare.intent.run_id.clone(),
                observed: spare.observed.clone(),
                token,
            }),
            _ => Err("registry did not hand the spare over".into()),
        }
    }

    /// The claim still holds for this exact engine and nothing has executed.
    pub(super) async fn verify(&self) -> Result<(), String> {
        match self
            .registry
            .act_registry(ActRegistryCommand::VerifyClaimed {
                run: self.run.clone(),
                observed: self.observed.clone(),
                token: self.token.clone(),
            })
            .await
            .map_err(|e| format!("execution claim: {e}"))?
        {
            ActRegistryReply::Verified(record)
                if record.state == ActEngineState::Registered && record.execution.is_none() =>
            {
                Ok(())
            }
            _ => Err("execution claim no longer holds".into()),
        }
    }

    pub(super) fn engine(&self) -> &str {
        &self.observed.engine_id
    }

    /// The claim the daemon holds on a spare or the shared engine (#547).
    pub(super) fn held(registry: &'a RegistryActor, spare: &Spare) -> Self {
        Self {
            registry,
            run: spare.intent.run_id.clone(),
            observed: spare.observed.clone(),
            token: spare.token.clone(),
        }
    }
}

/// The run's engine under its claim, whether it is already prepared, and
/// the socket directory it binds (#547).
struct Acquired<'a> {
    claim: Claim<'a>,
    prepared: bool,
    socket: Option<ActEngineDockerSocket>,
}

fn docker_socket(intent: &ActEngineIntent) -> Option<ActEngineDockerSocket> {
    intent
        .creation_profile
        .as_ref()
        .and_then(|profile| profile.docker_socket.clone())
}

/// Claim the plan's spare (retiring it and falling back to a new engine when
/// it cannot be claimed), or create and claim a new engine. `engine_id` is
/// set as soon as an engine is known to exist.
async fn acquire<'a>(
    registry: &'a RegistryActor,
    backend: &dyn ActEngineBackend,
    owner: &str,
    plan: &EnginePlan,
    clock: &mut Clock,
    engine_id: &mut Option<String>,
    observer: &mut dyn EngineObserver,
) -> Result<Acquired<'a>, String> {
    if let Some(spare) = &plan.spare {
        observer.note(&format!(
            "claiming prepared spare engine {}",
            spare.intent.engine_name()
        ));
        // The registry still holds a spare whose container stopped (#556); claiming it would
        // fail the run at its first `docker exec`.
        let claimed = if backend.engine_running(&spare.observed.engine_id).await {
            Claim::take_spare(registry, spare, plan.intent.binding(), clock.now()).await
        } else {
            Err("its container is not running".to_owned())
        };
        match claimed {
            Ok(claim) => {
                *engine_id = Some(claim.observed.engine_id.clone());
                return Ok(Acquired {
                    claim,
                    prepared: true,
                    socket: docker_socket(&spare.intent),
                });
            }
            Err(error) => {
                let retired = super::spare::retire(registry, backend, spare).await;
                observer.note(&format!(
                    "spare engine not claimed ({error}); {}",
                    retired
                        .err()
                        .map_or("retired".into(), |e| format!("its removal failed: {e}"))
                ));
            }
        }
    }
    // A spare already mounts the cache volume; a new engine needs it to exist.
    backend.ensure_cache(&plan.cache).await?;
    observer.note("creating isolated engine");
    // The intent is durable before anything exists on the host engine.
    let observed = backend
        .create(registry, &plan.intent, owner, clock.now())
        .await?;
    *engine_id = Some(observed.engine_id.clone());
    let claim = Claim::commit(registry, &plan.intent, observed, clock.now()).await?;
    Ok(Acquired {
        claim,
        prepared: false,
        socket: docker_socket(&plan.intent),
    })
}

/// How long the in-run cleanup may take to prove an engine gone.
const CLEANUP_BUDGET: Duration = crate::act_engine::CLEANUP_BUDGET;

/// Run one workflow on a fresh isolated engine. Never panics on engine
/// faults; every path ends in a cleanup attempt whose result is reported.
#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "baseline, ci.yml#229"
)]
pub async fn run_on_engine(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    plan: &EnginePlan,
    cancellation: &CancellationToken,
    observer: &mut dyn EngineObserver,
) -> EngineReport {
    let run = plan.intent.run_id.clone();
    let mut clock = Clock(plan.intent.created_at);
    let owner = match registry_owner(registry).await {
        Ok(owner) => owner,
        Err(error) => {
            return EngineReport {
                execution: ExecutionEnd::EngineFailed(error),
                cleanup: CleanupEnd::Removed,
                engine_id: None,
                storage: None,
            };
        }
    };
    let mut engine_id = None;
    let mut claim = None;
    let mut laps = Laps::new();
    let mut peak = StoragePeak::default();
    // The run's scope inside the engine (#547): its proxy, and the engine it
    // must be closed in once anything of it may exist.
    let mut proxy = None;
    let mut scoped = None;
    let execution = 'run: {
        let acquired = match acquire(
            registry,
            backend,
            &owner,
            plan,
            &mut clock,
            &mut engine_id,
            observer,
        )
        .await
        {
            Ok(acquired) => acquired,
            Err(error) => break 'run ExecutionEnd::EngineFailed(error),
        };
        let held = claim.insert(acquired.claim);
        if let (Some(scope), Some(socket)) = (&plan.invocation.scope, &acquired.socket) {
            match super::run_proxy::RunProxy::start(socket, scope) {
                Ok(started) => proxy = Some(started),
                Err(error) => break 'run ExecutionEnd::EngineFailed(error),
            }
            scoped = Some((held.engine().to_owned(), scope));
        }
        let how = if acquired.prepared {
            "spare engine claimed"
        } else {
            "engine created"
        };
        observer.note(&format!("{how} in {}", laps.lap()));
        if cancellation.is_cancelled() {
            break 'run ExecutionEnd::Cancelled;
        }
        let deadline = plan.deadline;
        if let Err(error) = held.verify().await {
            break 'run ExecutionEnd::EngineFailed(error);
        }
        let prepare = async {
            if !acquired.prepared {
                observer.note("preparing engine: act, runner image");
                backend.prepare_engine(held.engine(), plan.act).await?;
            }
            observer.note("preparing run: tool cache, frozen source");
            backend
                .prepare_run(
                    held.engine(),
                    &plan.invocation,
                    &plan.source,
                    &plan.event,
                    plan.intent
                        .creation_profile
                        .as_ref()
                        .and_then(|profile| profile.tool_generation.as_ref()),
                )
                .await
        };
        let prepared =
            async_engine::timeout_at(deadline, async_engine::cancellable(cancellation, prepare))
                .await;
        match prepared {
            Err(_) => break 'run ExecutionEnd::TimedOut,
            Ok(Err(_)) => break 'run ExecutionEnd::Cancelled,
            Ok(Ok(Err(error))) => break 'run ExecutionEnd::EngineFailed(error),
            Ok(Ok(Ok(()))) => observer.note(&format!("engine prepared in {}", laps.lap())),
        }
        if let Err(error) = held.verify().await {
            break 'run ExecutionEnd::EngineFailed(error);
        }
        match async_engine::timeout_at(deadline, backend.list(held.engine(), &plan.invocation))
            .await
        {
            Err(_) => break 'run ExecutionEnd::TimedOut,
            Ok(Ok(listing)) => observer.declared(&listing),
            Ok(Err(error)) => break 'run ExecutionEnd::EngineFailed(error),
        }
        if let Err(error) = held.verify().await {
            break 'run ExecutionEnd::EngineFailed(error);
        }
        observer.note("running act on the isolated engine");
        let (lines, mut receiver) = async_engine::channel(512);
        let execute = async {
            let end = backend
                .execute(
                    held.engine(),
                    &plan.invocation,
                    deadline.remaining(),
                    cancellation,
                    &lines,
                )
                .await;
            drop(lines);
            end
        };
        let mut unsampled = None;
        let drain = async {
            let mut next_sample = Instant::now();
            loop {
                if Instant::now() >= next_sample {
                    next_sample = Instant::now() + storage::SAMPLE_INTERVAL;
                    sample_storage(backend, held.engine(), &mut peak, &mut unsampled, observer)
                        .await;
                }
                match async_engine::timeout(PROGRESS_TICK, receiver.recv()).await {
                    Ok(Some(line)) => observer.line(line),
                    Ok(None) => break,
                    Err(_) => observer.tick(),
                }
            }
        };
        let (end, ()) = async_engine::join(execute, drain).await;
        observer.note(&format!("act finished in {}", laps.lap()));
        match (peak.peak(), unsampled) {
            (Some(usage), _) => observer.note(&storage::peak_note(usage)),
            (None, Some(error)) => observer.note(&format!("engine storage not sampled: {error}")),
            (None, None) => {}
        }
        // The run's processes and containers go before its tool cache is
        // saved, so no job is still writing it.
        if let Some((engine, scope)) = scoped.take() {
            let _ = close_scope(backend, &engine, scope, observer).await;
        }
        let end = match end {
            Ok(ExecEnd::Exited(code)) => ExecutionEnd::Exited(code),
            Ok(ExecEnd::TimedOut) => ExecutionEnd::TimedOut,
            Ok(ExecEnd::Cancelled) => ExecutionEnd::Cancelled,
            Err(error) => ExecutionEnd::EngineFailed(error),
        };
        // A stopped launching client on cancellation/timeout does not prove
        // remote workflow writers have stopped. Preserve the previous warm
        // machine cache instead of copying from an uncertain live source.
        if matches!(&end, ExecutionEnd::Exited(_)) {
            if held.verify().await.is_ok() {
                save_toolcache(backend, held.engine(), observer, &mut laps).await;
            }
        } else {
            observer.note("tool cache not saved: workflow writers are not proven stopped");
        }
        if let Err(error) = commit(
            registry,
            ActRegistryCommand::Execution {
                run: held.run.clone(),
                token: held.token.clone(),
                outcome: registry_outcome(&end),
                at: clock.now(),
            },
        )
        .await
        {
            break 'run ExecutionEnd::EngineFailed(error);
        }
        end
    };
    if let Some((engine, scope)) = scoped.take() {
        let _ = close_scope(backend, &engine, scope, observer).await;
    }
    drop(proxy);
    let outcome = registry_outcome(&execution);
    let token = claim.as_ref().map(|held| held.token.clone());
    // The claimed engine's key: a spare's own UUID once one was claimed.
    let run = claim.as_ref().map_or(run, |held| held.run.clone());
    observer.note("removing isolated engine");
    let cleanup = match cleanup(
        registry,
        backend,
        &owner,
        &run,
        token.as_deref(),
        outcome,
        &mut clock,
    )
    .await
    {
        Ok(()) => {
            observer.note(&format!(
                "engine removed in {}; total {}",
                laps.lap(),
                laps.total()
            ));
            CleanupEnd::Removed
        }
        Err(error) => CleanupEnd::Failed(error),
    };
    EngineReport {
        execution,
        cleanup,
        engine_id,
        storage: peak.peak(),
    }
}

/// Close the run's scope in its engine, noting (never failing on) an error:
/// the engine's removal, or its next use, finds what is left.
async fn close_scope(
    backend: &dyn ActEngineBackend,
    engine: &str,
    scope: &super::engine::RunScope,
    observer: &mut dyn EngineObserver,
) -> Result<(), String> {
    let closed = async_engine::timeout(CLEANUP_BUDGET, backend.close_scope(engine, scope))
        .await
        .unwrap_or_else(|_| Err("timed out".into()));
    match &closed {
        Ok(()) => observer.note("run scope cleaned up: no container, network or volume left"),
        Err(error) => observer.note(&format!("run scope not cleaned up: {error}")),
    }
    closed
}

fn registry_outcome(end: &ExecutionEnd) -> ActRunOutcome {
    match end {
        ExecutionEnd::Exited(0) => ActRunOutcome::Passed,
        ExecutionEnd::Cancelled => ActRunOutcome::Cancelled,
        _ => ActRunOutcome::Failed,
    }
}

/// Request cleanup (as the claim's owner when the run held one) unless the
/// creation path already did, then retire the engine. A run whose intent
/// never committed has nothing to clean up.
pub(super) async fn cleanup(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    owner: &str,
    run: &str,
    token: Option<&str>,
    outcome: ActRunOutcome,
    clock: &mut Clock,
) -> Result<(), String> {
    let Some(current) = record(registry, run).await? else {
        return Ok(());
    };
    match current.state {
        ActEngineState::Terminal => return Ok(()),
        ActEngineState::CleanupRequired => {}
        ActEngineState::Pending | ActEngineState::Registered => {
            let at = clock.now().max(current.updated_at);
            let command = match (token, current.execution_claim.as_deref()) {
                (Some(token), Some(held)) if token == held => ActRegistryCommand::CleanupClaimed {
                    run: run.into(),
                    token: token.into(),
                    outcome,
                    at,
                },
                _ => ActRegistryCommand::Cleanup {
                    run: run.into(),
                    outcome,
                    at,
                },
            };
            commit(registry, command).await?;
        }
    }
    let current = record(registry, run)
        .await?
        .ok_or("registry: the run's record vanished")?;
    backend
        .retire(registry, owner, &current, CLEANUP_BUDGET)
        .await
}

pub mod shared;

#[cfg(test)]
pub(crate) mod tests;
