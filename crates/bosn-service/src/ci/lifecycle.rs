//! One act run in the machine's shared engine, end to end (#547).
//!
//! The engine itself is owned through the registry ([`super::shared_engine`],
//! [`super::spare::prepare`]): its intent is durable before it exists, its
//! creation is observed and registered, and the daemon holds its execution
//! claim. A run leases a slot in it, act2's serve mode prepares the engine and
//! opens the run's scope, act runs there, and closing the scope removes and
//! proves gone everything the run made. There is no per-run engine.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use bosn_registry::act::{
    ActEngineIntent, ActEngineObservation, ActEngineRecord, ActEngineState, ActRunOutcome,
};
use kernal_api::async_engine;

use super::{
    engine::{ActArtifact, ActEngineBackend, ActInvocation, CacheVolume, EngineLine},
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
    /// The shared engine to make when there is none (#547).
    pub shared: super::spare::SparePlan,
    /// `--github-proxy` (#650): the run's proxy, attached once the run has
    /// a scope in the engine.
    pub github: Option<std::sync::Arc<super::github::RunGithub>>,
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

/// Record one storage sample into `peak`, warning in the log the first
/// time it is low. A failed sample keeps its first error and never changes
/// the run.
fn record_storage(
    sample: Result<StorageUsage, String>,
    peak: &mut StoragePeak,
    unsampled: &mut Option<String>,
    observer: &mut dyn EngineObserver,
) {
    match sample {
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

/// What the drain loop woke up for.
enum Drained {
    Sampled(Result<StorageUsage, String>),
    Line(Option<EngineLine>),
    Tick,
}

/// Forward act's output to `observer` until the sender closes, sampling the
/// engine's storage alongside (#538). A slow storage probe runs concurrently
/// with the drain and never delays a line; at most one probe is in flight,
/// and the next starts [`storage::SAMPLE_INTERVAL`] after the last finished.
/// A probe still running at EOF gets at most [`FINAL_SAMPLE_GRACE`].
pub(super) async fn drain_output(
    backend: &dyn ActEngineBackend,
    engine: &str,
    receiver: &mut async_engine::Receiver<EngineLine>,
    peak: &mut StoragePeak,
    unsampled: &mut Option<String>,
    observer: &mut dyn EngineObserver,
) {
    let mut sampling = None;
    let mut next_sample = Instant::now();
    loop {
        if sampling.is_none() && Instant::now() >= next_sample {
            sampling = Some(backend.storage_usage(engine));
        }
        let event = {
            let mut recv = std::pin::pin!(async_engine::timeout(PROGRESS_TICK, receiver.recv()));
            std::future::poll_fn(|cx| {
                if let Some(probe) = sampling.as_mut()
                    && let std::task::Poll::Ready(sample) = probe.as_mut().poll(cx)
                {
                    return std::task::Poll::Ready(Drained::Sampled(sample));
                }
                recv.as_mut().poll(cx).map(|polled| match polled {
                    Ok(line) => Drained::Line(line),
                    Err(_) => Drained::Tick,
                })
            })
            .await
        };
        match event {
            Drained::Sampled(sample) => {
                sampling = None;
                next_sample = Instant::now() + storage::SAMPLE_INTERVAL;
                record_storage(sample, peak, unsampled, observer);
            }
            Drained::Line(Some(line)) => observer.line(line),
            Drained::Line(None) => {
                // Keep a probe already running when act ends, but never let it
                // hold up the run for longer than a short grace (#538).
                if let Some(probe) = sampling
                    && let Ok(sample) = async_engine::timeout(FINAL_SAMPLE_GRACE, probe).await
                {
                    record_storage(sample, peak, unsampled, observer);
                }
                break;
            }
            Drained::Tick => observer.tick(),
        }
    }
}

/// How long a storage probe still running when act ends may delay the run.
const FINAL_SAMPLE_GRACE: Duration = Duration::from_secs(2);

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

/// How long closing a run's scope may take to prove it gone.
const CLEANUP_BUDGET: Duration = crate::act_engine::CLEANUP_BUDGET;

/// Close the run's scope in its engine, noting (never failing on) an error:
/// the engine's next use, or its retirement, finds what is left. Serve saves
/// the tool cache once the run's writers are gone; that is noted too.
async fn close_scope(
    backend: &dyn ActEngineBackend,
    engine: &str,
    scope: &super::engine::RunScope,
    observer: &mut dyn EngineObserver,
    laps: &mut Laps,
) -> Result<(), String> {
    let closed = async_engine::timeout(CLEANUP_BUDGET, backend.close_scope(engine, scope))
        .await
        .unwrap_or_else(|_| Err("timed out".into()));
    match &closed {
        Ok(closed) => {
            observer.note("run scope cleaned up: no container, network or volume left");
            match &closed.tool_cache {
                Some(save) if save.saved => {
                    observer.note(&format!("tool cache saved in {}", laps.lap()))
                }
                Some(save) => observer.note(&format!("tool cache not saved: {}", save.note)),
                None => {}
            }
        }
        Err(error) => observer.note(&format!("run scope not cleaned up: {error}")),
    }
    closed.map(|_| ())
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
