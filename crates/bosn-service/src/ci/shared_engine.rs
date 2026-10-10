//! One long-lived engine shared by concurrent runs (#547, plan step 2).
//!
//! The engine is an ordinary #349 owned engine, made exactly like a spare
//! ([`super::spare::prepare`]): its intent is durable before it exists, the
//! daemon prepares it (act, runner image) under its own execution claim, and
//! startup recovery retires one a dead daemon left. Unlike a spare it is
//! never handed to a run. Each run leases a slot in it instead, and runs in
//! its own scope ([`super::engine::RunScope`]): a cgroup with the run's
//! limits, a network, a work tree, its own artifact and cache server ports
//! (from the slot) and its own Docker proxy. When a run ends, everything
//! carrying its label is removed from the engine, and the engine stays up.
//!
//! **Idle retirement.** An engine with no leased slot for
//! `[engine] idle_retire_secs` (default [`DEFAULT_IDLE_RETIRE`], 10 minutes)
//! is retired by the daemon: removed with proof, its storage volume with it,
//! exactly like a finished per-run engine. The next run makes a new one. A
//! daemon that stops retires an idle engine; one it leaves behind (busy, or
//! the daemon died) is retired by the next daemon's startup recovery.
//!
//! An engine whose intent no longer matches what a run would create (the
//! config, the host sizing or a pin changed) is retired once idle and
//! replaced; until then runs that want the new engine get their own per-run
//! engine, so nothing waits on a busy engine to drain.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use bosn_registry::act::ActEngineDockerSocket;
use kernal_api::async_engine::{self, CancellationToken};

use super::{
    engine::{ActEngineBackend, MAX_SLOTS},
    lifecycle::Claim,
    spare::{self, Spare, SparePlan},
};
use crate::RegistryActor;

/// How long an engine with no run is kept before it is retired.
pub const DEFAULT_IDLE_RETIRE: Duration = Duration::from_secs(600);
/// How often the daemon checks for an idle engine.
const REAP_EVERY: Duration = Duration::from_secs(15);

/// A run's slot in the shared engine; give it back with [`SharedEngine::release`].
#[derive(Debug)]
pub struct Lease {
    pub engine_id: String,
    pub engine_name: String,
    pub slot: u16,
    pub socket: ActEngineDockerSocket,
    /// The engine's own memory, CPU and process limits, to size the run from.
    pub memory_bytes: u64,
    pub nano_cpus: u64,
    pub pids: u64,
}

struct Held {
    engine: Spare,
    slots: BTreeSet<u16>,
    idle_since: Instant,
}

#[derive(Default)]
struct State {
    closed: bool,
}

/// Why no slot was leased.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The engine is busy and is not the engine this run would create; the
    /// run should use its own engine.
    Mismatched,
    Failed(String),
}

pub struct SharedEngine {
    registry: RegistryActor,
    backend: Arc<dyn ActEngineBackend>,
    held: async_engine::Mutex<Option<Held>>,
    state: Mutex<State>,
}

impl SharedEngine {
    pub fn new(registry: RegistryActor, backend: Arc<dyn ActEngineBackend>) -> Self {
        Self {
            registry,
            backend,
            held: async_engine::Mutex::new(None),
            state: Mutex::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A slot in the shared engine, making (and preparing) the engine when
    /// there is none. `want` describes the engine this run would create.
    pub async fn lease(
        &self,
        want: SparePlan,
        cancellation: &CancellationToken,
    ) -> Result<Lease, Refused> {
        if self.state().closed {
            return Err(Refused::Failed("the daemon is stopping".into()));
        }
        let mut held = self.held.lock().await;
        if let Some(current) = held.as_ref() {
            let usable = current.engine.intent.same_engine(&want.intent)
                && self
                    .backend
                    .engine_running(&current.engine.observed.engine_id)
                    .await
                && Claim::held(&self.registry, &current.engine)
                    .verify()
                    .await
                    .is_ok();
            if !usable {
                if !current.slots.is_empty() {
                    return Err(Refused::Mismatched);
                }
                if let Some(stale) = held.take() {
                    self.retire(&stale.engine).await;
                }
            }
        }
        if held.is_none() {
            let engine = spare::prepare(&self.registry, self.backend.as_ref(), want, cancellation)
                .await
                .map_err(Refused::Failed)?;
            *held = Some(Held {
                engine,
                slots: BTreeSet::new(),
                idle_since: Instant::now(),
            });
        }
        let current = held
            .as_mut()
            .ok_or_else(|| Refused::Failed("no engine".into()))?;
        let slot = (0..MAX_SLOTS)
            .find(|slot| !current.slots.contains(slot))
            .ok_or_else(|| Refused::Failed("the shared engine has no free slot".into()))?;
        let profile = current
            .engine
            .intent
            .creation_profile
            .as_ref()
            .ok_or_else(|| Refused::Failed("shared engine has no creation profile".into()))?;
        let socket = profile
            .docker_socket
            .clone()
            .ok_or_else(|| Refused::Failed("shared engine has no socket directory".into()))?;
        let lease = Lease {
            engine_id: current.engine.observed.engine_id.clone(),
            engine_name: current.engine.intent.engine_name(),
            slot,
            socket,
            memory_bytes: profile.memory_bytes,
            nano_cpus: profile.nano_cpus,
            pids: profile.pids,
        };
        current.slots.insert(slot);
        Ok(lease)
    }

    /// The engine's claim still holds (checked before each in-engine step).
    pub async fn verify(&self, lease: &Lease) -> Result<(), String> {
        let held = self.held.lock().await;
        match held.as_ref() {
            Some(current) if current.engine.observed.engine_id == lease.engine_id => {
                Claim::held(&self.registry, &current.engine).verify().await
            }
            _ => Err("the shared engine was retired".into()),
        }
    }

    /// Give a slot back; the engine is idle from now when it was the last.
    pub async fn release(&self, lease: Lease) {
        let mut held = self.held.lock().await;
        if let Some(current) = held.as_mut()
            && current.engine.observed.engine_id == lease.engine_id
        {
            current.slots.remove(&lease.slot);
            if current.slots.is_empty() {
                current.idle_since = Instant::now();
            }
        }
    }

    /// Retire the engine if no run has held it for `ttl`; whether it did.
    pub async fn retire_idle(&self, ttl: Duration) -> bool {
        let mut held = self.held.lock().await;
        let idle = held
            .as_ref()
            .is_some_and(|current| current.slots.is_empty() && current.idle_since.elapsed() >= ttl);
        if !idle {
            return false;
        }
        if let Some(current) = held.take() {
            self.retire(&current.engine).await;
        }
        true
    }

    /// The engine and how many runs it holds, for status.
    pub async fn status(&self) -> Option<(String, usize)> {
        let held = self.held.lock().await;
        held.as_ref()
            .map(|current| (current.engine.intent.engine_name(), current.slots.len()))
    }

    /// Stop leasing and retire an idle engine (daemon shutdown). A busy one
    /// is left to the next daemon's startup recovery.
    pub async fn close(&self) {
        self.state().closed = true;
        self.retire_idle(Duration::ZERO).await;
    }

    /// Check for an idle engine every [`REAP_EVERY`] until closed. `ttl`
    /// is read on each check, so a config change applies live.
    pub fn spawn_reaper(self: &Arc<Self>, ttl: impl Fn() -> Duration + Send + 'static) {
        let keeper = Arc::downgrade(self);
        async_engine::launch(async move {
            loop {
                async_engine::sleep(REAP_EVERY).await;
                let Some(keeper) = keeper.upgrade() else {
                    return;
                };
                if keeper.state().closed {
                    return;
                }
                if keeper.retire_idle(ttl()).await {
                    eprintln!("bosn ci: idle shared engine retired");
                }
            }
        })
        .detach();
    }

    async fn retire(&self, engine: &Spare) {
        if let Err(error) = spare::retire(&self.registry, self.backend.as_ref(), engine).await {
            eprintln!(
                "bosn ci: shared engine {} not removed: {error}",
                engine.intent.engine_name()
            );
        }
    }
}
