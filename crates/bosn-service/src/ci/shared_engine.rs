//! One long-lived engine shared by every run on the machine (#547, #544).
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
//! **One per machine (#544).** Which daemon makes the engine is decided by
//! the host Docker engine's claim ([`super::machine`]). Every other daemon,
//! from any state directory, runs in the claimed engine when it is the
//! engine it would make, and otherwise waits for it to drain and retire.
//! Slots are leased in the engine itself, so the slot table spans daemons.
//! A run never falls back to an engine of its own.
//!
//! **Idle retirement.** The daemon that made the engine retires it once no
//! slot has been held for `[engine] idle_retire_secs` (default
//! [`DEFAULT_IDLE_RETIRE`], 10 minutes), or as soon as it is empty when a
//! daemon needs a different engine: removed with proof, its storage volume
//! with it, and the claim released. One whose daemon died is retired by any
//! daemon's reaper once idle, as are idle legacy engines.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use bosn_registry::act::ActEngineDockerSocket;
use kernal_api::async_engine::{self, CancellationToken};

use super::{
    engine::ActEngineBackend,
    lifecycle::Claim,
    machine::{Daemon, EngineIdentity, Holder, Leased, MachineEngine},
    spare::{Spare, SparePlan},
};
use crate::RegistryActor;

mod machine_ops;

/// How long an engine with no run is kept before it is retired.
pub const DEFAULT_IDLE_RETIRE: Duration = Duration::from_secs(600);
/// How often the daemon checks for an idle engine.
const REAP_EVERY: Duration = Duration::from_secs(15);
/// How often a waiting run looks again.
const WAIT_TICK: Duration = Duration::from_secs(2);
/// How long a new engine waits for busy legacy engines to finish before
/// it is made beside them (they are still retired once idle).
const LEGACY_DRAIN_WAIT: Duration = Duration::from_secs(10 * 60);

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

/// Whose engine it is.
enum Kind {
    /// This daemon made it (its registry holds the record) and holds the
    /// machine claim `claim` (the claim container's ID).
    Owned { engine: Box<Spare>, claim: String },
    /// Another daemon's.
    Adopted,
}

struct Held {
    kind: Kind,
    id: String,
    name: String,
    identity: EngineIdentity,
    socket: ActEngineDockerSocket,
    memory_bytes: u64,
    nano_cpus: u64,
    pids: u64,
    /// This daemon's runs in it.
    slots: BTreeSet<u16>,
}

impl Held {
    fn lease(&self, slot: u16) -> Lease {
        Lease {
            engine_id: self.id.clone(),
            engine_name: self.name.clone(),
            slot,
            socket: self.socket.clone(),
            memory_bytes: self.memory_bytes,
            nano_cpus: self.nano_cpus,
            pids: self.pids,
        }
    }
}

#[derive(Default)]
struct State {
    closed: bool,
    /// This daemon runs CI: only then does it tend the machine's engines.
    wanted: bool,
    /// Since when a new engine has waited for busy legacy engines.
    legacy_since: Option<Instant>,
    /// Legacy engines found idle on the last look: one is retired only when
    /// found idle twice, so a gap between two steps of a run is not idle.
    idle_legacy: BTreeSet<String>,
}

/// One pass of a lease.
enum Step {
    Leased(Lease),
    /// Look again now (the claim changed hands).
    Again,
    /// Look again after [`WAIT_TICK`]; why, for the run's log.
    Wait(String),
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

    fn machine(&self) -> Result<&dyn MachineEngine, String> {
        self.backend
            .machine()
            .ok_or_else(|| "this Docker backend cannot share an engine".into())
    }

    /// A slot for run `run` in the machine's shared engine, making (and
    /// preparing) the engine when there is none, and waiting while the
    /// engine there is not one this run can use. `want` describes the engine
    /// this daemon would make; `note` hears each reason the run waits.
    pub async fn lease(
        &self,
        run: &str,
        want: SparePlan,
        cancellation: &CancellationToken,
        note: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Lease, String> {
        self.state().wanted = true;
        let identity =
            EngineIdentity::of(&want.intent).ok_or("the shared engine has no creation profile")?;
        let holder = Holder {
            run: run.into(),
            registry: super::lifecycle::registry_owner(&self.registry).await?,
            daemon: Daemon::current(),
        };
        let mut said = BTreeSet::new();
        loop {
            if self.state().closed {
                return Err("the daemon is stopping".into());
            }
            match self.step(&want, &identity, &holder, cancellation).await? {
                Step::Leased(lease) => return Ok(lease),
                Step::Again => async_engine::sleep(Duration::from_millis(100)).await,
                Step::Wait(why) => {
                    if said.insert(why.clone()) {
                        note(&why);
                    }
                    async_engine::sleep(WAIT_TICK).await;
                }
            }
        }
    }

    async fn step(
        &self,
        want: &SparePlan,
        identity: &EngineIdentity,
        holder: &Holder,
        cancellation: &CancellationToken,
    ) -> Result<Step, String> {
        let machine = self.machine()?;
        let mut held = self.held.lock().await;
        if let Some(current) = held.take() {
            if self.usable(&current, identity).await {
                *held = Some(current);
            } else if let Some(wait) = self.replace(current, &mut held).await {
                return Ok(Step::Wait(wait));
            }
        }
        if held.is_none() {
            match self.find_or_make(want, identity, cancellation).await? {
                Ok(found) => *held = Some(found),
                Err(step) => return Ok(step),
            }
        }
        let Some(current) = held.as_mut() else {
            return Ok(Step::Again);
        };
        match machine.lease_slot(&current.id, holder).await? {
            Leased::Slot(slot) => {
                current.slots.insert(slot);
                Ok(Step::Leased(current.lease(slot)))
            }
            Leased::Preparing => Ok(Step::Wait(format!(
                "the machine's shared engine {} is still being prepared",
                current.name
            ))),
            Leased::Retiring => {
                if matches!(current.kind, Kind::Adopted) {
                    *held = None;
                }
                Ok(Step::Wait(
                    "the shared engine is retiring; waiting for the next one".into(),
                ))
            }
            Leased::Full => Ok(Step::Wait(
                "every slot in the shared engine is taken; waiting for one".into(),
            )),
        }
    }

    /// Whether runs can still use the engine this daemon holds.
    async fn usable(&self, current: &Held, identity: &EngineIdentity) -> bool {
        if current.identity != *identity || !self.backend.engine_running(&current.id).await {
            return false;
        }
        match &current.kind {
            Kind::Owned { engine, .. } => {
                Claim::held(&self.registry, engine).verify().await.is_ok()
            }
            Kind::Adopted => true,
        }
    }

    /// Let go of an engine runs can no longer use: an adopted one is
    /// forgotten; an owned one is retired once empty. Until then, why the
    /// run waits (and the engine stays held).
    async fn replace(&self, current: Held, held: &mut Option<Held>) -> Option<String> {
        if matches!(current.kind, Kind::Adopted) {
            return None;
        }
        let machine = self.machine().ok()?;
        let running = self.backend.engine_running(&current.id).await;
        if running && machine.begin_retire(&current.id).await != Ok(true) {
            let _ = machine.request_retire(&current.id).await;
            let name = current.name.clone();
            *held = Some(current);
            return Some(format!(
                "the shared engine {name} is busy as a different engine (config or pins \
                 changed); waiting for its runs to finish"
            ));
        }
        self.retire_owned(current).await;
        None
    }

    /// The engine's claim still holds (checked before each in-engine step).
    pub async fn verify(&self, lease: &Lease) -> Result<(), String> {
        let held = self.held.lock().await;
        match held.as_ref() {
            Some(current) if current.id == lease.engine_id => match &current.kind {
                Kind::Owned { engine, .. } => Claim::held(&self.registry, engine).verify().await,
                Kind::Adopted if self.backend.engine_running(&current.id).await => Ok(()),
                Kind::Adopted => Err("the shared engine stopped".into()),
            },
            _ => Err("the shared engine was retired".into()),
        }
    }

    /// Give a slot back.
    pub async fn release(&self, lease: Lease) {
        if let Ok(machine) = self.machine()
            && let Err(error) = machine.release_slot(&lease.engine_id, lease.slot).await
        {
            // An engine that is gone took its slot table with it.
            eprintln!(
                "bosn ci: slot {} in {} not released: {error}",
                lease.slot, lease.engine_name
            );
        }
        let mut held = self.held.lock().await;
        if let Some(current) = held.as_mut()
            && current.id == lease.engine_id
        {
            current.slots.remove(&lease.slot);
        }
    }

    /// The engine and how many of this daemon's runs it holds, for status.
    pub async fn status(&self) -> Option<(String, usize)> {
        let held = self.held.lock().await;
        held.as_ref()
            .map(|current| (current.name.clone(), current.slots.len()))
    }

    /// Stop leasing and retire this daemon's engine if no run (of any
    /// daemon) holds it (daemon shutdown). A busy one is left: its other
    /// daemons' reapers retire it once idle, or the next startup recovery.
    pub async fn close(&self) {
        self.state().closed = true;
        self.retire_idle(Duration::ZERO).await;
    }

    /// Tend the machine every [`REAP_EVERY`] until closed. `ttl` is read on
    /// each check, so a config change applies live.
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
                let ttl = ttl();
                if keeper.retire_idle(ttl).await {
                    eprintln!("bosn ci: idle shared engine retired");
                }
                if keeper.state().wanted {
                    keeper.tend_machine(ttl).await;
                }
            }
        })
        .detach();
    }
}
