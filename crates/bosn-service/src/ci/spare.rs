//! One prepared spare engine (#410), so a run skips engine creation and the
//! slow part of preparing it (act install, runner image load).
//!
//! A spare is an ordinary #349 owned Act engine: its intent (marked spare,
//! naming no run) and frozen creation profile are durable before it exists,
//! it carries the ownership labels, and the daemon prepares it under its own
//! execution claim, running only bosn's fixed scripts. A run takes it over
//! with one registry transaction that replaces that claim with the run's
//! ([`super::lifecycle`]); from then on it is that run's engine, removed
//! with proof when the run ends. A spare nobody claimed is retired the same
//! way, and startup recovery treats a spare a dead daemon left exactly like
//! any other non-terminal engine.
//!
//! [`SpareKeeper`] holds at most one. A run waits for a spare that is being
//! prepared rather than creating a second engine, and claims a spare only
//! when its intent would create exactly the same engine
//! ([`ActEngineIntent::same_engine`]); a spare made for other limits or pins
//! is retired instead.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use bosn_registry::act::{ActEngineIntent, ActEngineObservation, ActRunOutcome};
use kernal_api::async_engine::{self, CancellationSource, CancellationToken, OwnedMutexGuard};
use serde::Deserialize;

use super::{
    engine::{ActArtifact, ActEngineBackend, CacheVolume},
    lifecycle::{self, Claim, Clock},
    reply::{SpareState, SpareStatus},
};
use crate::RegistryActor;

/// `[engine] spares` in `<state>/config.toml`: how many prepared spare
/// engines the daemon keeps. `0` opts out; the default and the most is 1.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(try_from = "u8")]
pub enum Spares {
    None,
    #[default]
    One,
}

impl TryFrom<u8> for Spares {
    type Error = String;
    fn try_from(value: u8) -> Result<Self, String> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::One),
            _ => Err(format!("[engine] spares must be 0 or 1, not {value}")),
        }
    }
}

/// The host memory that must be available for a spare to be kept: an idle
/// spare holds its runner image (about 2 GiB) in its RAM-backed storage.
pub const ROOM: u64 = 16 << 30;
/// How long preparing a spare may take (a first runner pull included).
const PREPARE_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// After a spare could not be prepared, how long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// A prepared spare engine, held under the daemon's claim `token`.
#[derive(Clone, Debug)]
pub struct Spare {
    pub intent: ActEngineIntent,
    pub observed: ActEngineObservation,
    pub token: String,
}

/// What a new spare (or the shared engine, #547) is made from.
#[derive(Clone, Debug)]
pub struct SparePlan {
    pub intent: ActEngineIntent,
    pub act: ActArtifact,
    pub cache: CacheVolume,
}

/// The right to prepare the one spare: holds the slot until it is filled.
pub struct Fill {
    slot: OwnedMutexGuard<Option<Spare>>,
    cancel: CancellationToken,
}

impl Fill {
    /// Cancelled when the spare is no longer wanted (shutdown, cache clear).
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }
}

#[derive(Default)]
struct KeeperState {
    /// Set by the daemon's first `bosn ci` submission: a daemon that never
    /// runs CI never holds a spare.
    wanted: bool,
    closed: bool,
    filling: Option<CancellationSource>,
    status: Option<SpareStatus>,
    failed_at: Option<Instant>,
    retiring: Vec<async_engine::Task<()>>,
}

pub struct SpareKeeper {
    registry: RegistryActor,
    backend: Arc<dyn ActEngineBackend>,
    /// The spare; locked while one is prepared or taken.
    slot: Arc<async_engine::Mutex<Option<Spare>>>,
    state: Mutex<KeeperState>,
}

impl SpareKeeper {
    pub fn new(registry: RegistryActor, backend: Arc<dyn ActEngineBackend>) -> Self {
        Self {
            registry,
            backend,
            slot: Arc::new(async_engine::Mutex::new(None)),
            state: Mutex::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, KeeperState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The spare, if one is kept or being prepared.
    pub fn status(&self) -> Option<SpareStatus> {
        self.state().status.clone()
    }

    /// Keep a spare from now on (this daemon runs CI).
    pub fn want(&self) {
        self.state().wanted = true;
    }

    /// The right to prepare a spare, unless none is wanted yet, one exists
    /// or is being prepared, the keeper is closed, or the last attempt
    /// failed under a minute ago.
    pub fn begin(&self) -> Option<Fill> {
        let mut state = self.state();
        if !state.wanted
            || state.closed
            || state.failed_at.is_some_and(|at| at.elapsed() < RETRY_AFTER)
        {
            return None;
        }
        let slot = Arc::clone(&self.slot).try_lock_owned().ok()?;
        if slot.is_some() {
            return None;
        }
        let cancel = CancellationSource::new();
        let token = cancel.token();
        state.filling = Some(cancel);
        Some(Fill {
            slot,
            cancel: token,
        })
    }

    /// Prepare the spare `plan` describes (`None`: not now), then release
    /// the slot. A failure is logged and leaves no engine behind.
    pub async fn fill(&self, mut fill: Fill, plan: Result<Option<SparePlan>, String>) {
        let prepared = match plan {
            Ok(None) => Ok(None),
            Err(error) => Err(error),
            Ok(Some(plan)) => {
                self.state().status = Some(SpareStatus {
                    engine: plan.intent.engine_name(),
                    state: SpareState::Preparing,
                    storage_used: None,
                });
                prepare(&self.registry, self.backend.as_ref(), plan, &fill.cancel)
                    .await
                    .map(Some)
            }
        };
        let storage_used = match &prepared {
            Ok(Some(spare)) => self
                .backend
                .storage_usage(&spare.observed.engine_id)
                .await
                .ok()
                .map(|usage| usage.used),
            _ => None,
        };
        let mut state = self.state();
        state.filling = None;
        state.status = None;
        match prepared {
            Ok(Some(spare)) => {
                state.status = Some(SpareStatus {
                    engine: spare.intent.engine_name(),
                    state: SpareState::Ready,
                    storage_used,
                });
                *fill.slot = Some(spare);
            }
            Ok(None) => {}
            Err(error) => {
                state.failed_at = Some(Instant::now());
                eprintln!("bosn ci: spare engine not prepared: {error}");
            }
        }
    }

    /// The spare for a run whose intent is `want`, waiting (until `deadline`
    /// or cancellation) for one being prepared. A spare that would not be
    /// the same engine is retired in the background, never claimed.
    pub async fn take(
        self: &Arc<Self>,
        want: &ActEngineIntent,
        deadline: async_engine::Deadline,
        cancellation: &CancellationToken,
    ) -> Option<Spare> {
        if self.state().closed {
            return None;
        }
        let mut slot = async_engine::timeout_at(
            deadline,
            async_engine::cancellable(cancellation, self.slot.lock()),
        )
        .await
        .ok()?
        .ok()?;
        let spare = slot.take()?;
        self.state().status = None;
        drop(slot);
        if spare.intent.same_engine(want) {
            return Some(spare);
        }
        self.retire_in_background(spare);
        None
    }

    /// Retire a spare nobody will claim, tracked so [`Self::close`] waits.
    fn retire_in_background(self: &Arc<Self>, spare: Spare) {
        let keeper = Arc::clone(self);
        let task = async_engine::launch(async move {
            if let Err(error) = retire(&keeper.registry, keeper.backend.as_ref(), &spare).await {
                eprintln!(
                    "bosn ci: spare engine {} not removed: {error}",
                    spare.intent.engine_name()
                );
            }
        });
        let mut state = self.state();
        state.retiring.retain(|task| !task.is_finished());
        state.retiring.push(task);
    }

    /// Retire the spare (stopping one being prepared, which then removes
    /// its own engine) and wait for every retirement to finish. The keeper
    /// prepares new spares afterwards unless it is closed.
    pub async fn retire_all(&self) {
        if let Some(filling) = self.state().filling.take() {
            filling.cancel();
        }
        let spare = self.slot.lock().await.take();
        self.state().status = None;
        if let Some(spare) = spare
            && let Err(error) = retire(&self.registry, self.backend.as_ref(), &spare).await
        {
            eprintln!(
                "bosn ci: spare engine {} not removed: {error}",
                spare.intent.engine_name()
            );
        }
        let retiring = std::mem::take(&mut self.state().retiring);
        for task in retiring {
            let _ = task.await;
        }
    }

    /// Stop keeping spares and retire the one there is (daemon shutdown).
    pub async fn close(&self) {
        self.state().closed = true;
        self.retire_all().await;
    }

    /// Retire the spare in the background (opted out by config).
    pub fn discard(self: &Arc<Self>) {
        if self.state().status.is_none() {
            return;
        }
        let keeper = Arc::clone(self);
        let task = async_engine::launch(async move { keeper.retire_all().await });
        self.state().retiring.push(task);
    }
}

/// Create, claim and prepare a spare engine. Any failure removes what was
/// created (or leaves its record `cleanup_required` for startup recovery).
/// The shared engine (#547) is made the same way.
pub(super) async fn prepare(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    plan: SparePlan,
    cancellation: &CancellationToken,
) -> Result<Spare, String> {
    let owner = lifecycle::registry_owner(registry).await?;
    let mut clock = Clock(plan.intent.created_at);
    let mut token = None;
    let held = prepare_held(
        registry,
        backend,
        &owner,
        &plan,
        cancellation,
        &mut clock,
        &mut token,
    )
    .await;
    match held {
        Ok(observed) => Ok(Spare {
            intent: plan.intent,
            observed,
            token: token.unwrap_or_default(),
        }),
        Err(error) => {
            let removed = lifecycle::cleanup(
                registry,
                backend,
                &owner,
                &plan.intent.run_id,
                token.as_deref(),
                ActRunOutcome::Cancelled,
                &mut clock,
            )
            .await;
            Err(match removed {
                Ok(()) => error,
                Err(removal) => format!("{error}; its removal failed: {removal}"),
            })
        }
    }
}

async fn prepare_held(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    owner: &str,
    plan: &SparePlan,
    cancellation: &CancellationToken,
    clock: &mut Clock,
    token: &mut Option<String>,
) -> Result<ActEngineObservation, String> {
    backend.ensure_cache(&plan.cache).await?;
    let observed = backend
        .create(registry, &plan.intent, owner, clock.now())
        .await?;
    let held = Claim::commit(registry, &plan.intent, observed, clock.now()).await?;
    *token = Some(held.token.clone());
    held.verify().await?;
    let prepared = async_engine::timeout(
        PREPARE_DEADLINE,
        async_engine::cancellable(
            cancellation,
            backend.prepare_engine(held.engine(), plan.act),
        ),
    )
    .await;
    match prepared {
        Err(_) => return Err("preparing the spare timed out".into()),
        Ok(Err(_)) => return Err("preparing the spare was cancelled".into()),
        Ok(Ok(result)) => result?,
    }
    held.verify().await?;
    Ok(held.observed.clone())
}

/// Retire a spare as its holder. Refused once a run has claimed it: that
/// run owns its removal.
pub(super) async fn retire(
    registry: &RegistryActor,
    backend: &dyn ActEngineBackend,
    spare: &Spare,
) -> Result<(), String> {
    let owner = lifecycle::registry_owner(registry).await?;
    lifecycle::cleanup(
        registry,
        backend,
        &owner,
        &spare.intent.run_id,
        Some(&spare.token),
        ActRunOutcome::Cancelled,
        &mut Clock(spare.intent.created_at),
    )
    .await
}

#[cfg(test)]
mod tests;
