//! The shared engine's machine-wide side (#544): taking or adopting the
//! claim, idle retirement across daemons, taking over what a dead daemon
//! left, and draining legacy engines.

use std::{sync::Arc, time::Duration};

use bosn_registry::act::ActEngineDockerSocket;
use kernal_api::async_engine::{self, CancellationToken};

use super::{
    super::{
        engine::{ActEngineBackend, RunLimits, RunScope},
        machine::{
            CLAIM_PREPARE_LIMIT, Daemon, EngineClaim, EngineIdentity, Foreign, LEGACY_GRACE,
            MachineEngine, Survey,
        },
        spare::{self, SparePlan},
    },
    Held, Kind, LEGACY_DRAIN_WAIT, SharedEngine, Step,
};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Removes a claim this daemon made unless disarmed: a lease cancelled
/// while its engine is prepared must not leave other daemons waiting.
struct ClaimGuard {
    backend: Arc<dyn ActEngineBackend>,
    id: Option<String>,
}

impl ClaimGuard {
    fn disarm(mut self) -> String {
        self.id.take().unwrap_or_default()
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let backend = Arc::clone(&self.backend);
        async_engine::launch(async move {
            if let Some(machine) = backend.machine()
                && let Err(error) = machine.remove_claim(&id).await
            {
                eprintln!("bosn ci: engine claim not removed: {error}");
            }
        })
        .detach();
    }
}

/// Holders whose daemon is still alive.
fn live(survey: &Survey) -> usize {
    survey
        .held
        .iter()
        .filter(|(_, holder)| holder.daemon.alive())
        .count()
}

impl SharedEngine {
    /// The machine's engine for this run: the claimed one when it fits, a
    /// new one when there is no claim, or why the run must wait.
    pub(super) async fn find_or_make(
        &self,
        want: &SparePlan,
        identity: &EngineIdentity,
        cancellation: &CancellationToken,
    ) -> Result<Result<Held, Step>, String> {
        let machine = self.machine()?;
        let owner = super::super::lifecycle::registry_owner(&self.registry).await?;
        match machine.claim().await? {
            None => self.make(machine, want, &owner, cancellation).await,
            Some(claim) if claim.ours(&owner) => {
                // A claim this daemon kept after its engine went.
                machine.remove_claim(&claim.id).await?;
                Ok(Err(Step::Again))
            }
            Some(claim) => self.adopt(machine, claim, identity).await,
        }
    }

    async fn make(
        &self,
        machine: &dyn MachineEngine,
        want: &SparePlan,
        owner: &str,
        cancellation: &CancellationToken,
    ) -> Result<Result<Held, Step>, String> {
        let busy = self.drain_legacy(machine, None).await?;
        if !busy.is_empty() {
            let since = *self
                .state()
                .legacy_since
                .get_or_insert_with(std::time::Instant::now);
            if since.elapsed() < LEGACY_DRAIN_WAIT {
                return Ok(Err(Step::Wait(format!(
                    "waiting for {} legacy engine(s) to finish their runs: {}",
                    busy.len(),
                    busy.join(", ")
                ))));
            }
            eprintln!(
                "bosn ci: legacy engines still busy after {}s; making the shared engine \
                 beside them (each is retired once idle): {}",
                LEGACY_DRAIN_WAIT.as_secs(),
                busy.join(", ")
            );
        }
        self.state().legacy_since = None;
        let claim = EngineClaim {
            id: String::new(),
            engine: want.intent.engine_name(),
            registry: owner.into(),
            daemon: Daemon::current(),
            created: now(),
        };
        if !machine.create_claim(&claim).await? {
            return Ok(Err(Step::Again));
        }
        let id = machine
            .claim()
            .await?
            .filter(|found| found.engine == claim.engine)
            .map(|found| found.id)
            .ok_or("the engine claim vanished after it was made")?;
        let guard = ClaimGuard {
            backend: Arc::clone(&self.backend),
            id: Some(id),
        };
        let engine = spare::prepare(
            &self.registry,
            self.backend.as_ref(),
            want.clone(),
            cancellation,
        )
        .await?;
        // Runs of other daemons wait until now: the engine is prepared.
        if let Err(error) = machine.mark_ready(&engine.observed.engine_id).await {
            let _ = spare::retire(&self.registry, self.backend.as_ref(), &engine).await;
            return Err(format!("shared engine not opened to runs: {error}"));
        }
        let profile = engine
            .intent
            .creation_profile
            .clone()
            .ok_or("shared engine has no creation profile")?;
        let socket = profile
            .docker_socket
            .clone()
            .ok_or("shared engine has no socket directory")?;
        Ok(Ok(Held {
            id: engine.observed.engine_id.clone(),
            name: engine.intent.engine_name(),
            identity: EngineIdentity::of(&engine.intent).ok_or("shared engine identity")?,
            socket,
            memory_bytes: profile.memory_bytes,
            nano_cpus: profile.nano_cpus,
            pids: profile.pids,
            slots: Default::default(),
            kind: Kind::Owned {
                engine: Box::new(engine),
                claim: guard.disarm(),
            },
        }))
    }

    async fn adopt(
        &self,
        machine: &dyn MachineEngine,
        claim: EngineClaim,
        identity: &EngineIdentity,
    ) -> Result<Result<Held, Step>, String> {
        let engine = machine.inspect(&claim.engine).await?;
        let running = engine.as_ref().filter(|engine| engine.running);
        if let Some(engine) = running
            && engine.identity.as_ref() == Some(identity)
            && engine.registry == claim.registry
        {
            return adopted(engine).map(Ok);
        }
        let stalled = running.is_none()
            && now().saturating_sub(claim.created) > CLAIM_PREPARE_LIMIT.as_secs();
        if claim.daemon.alive() && !stalled {
            if let Some(engine) = running {
                let _ = machine.request_retire(&engine.id).await;
                return Ok(Err(Step::Wait(format!(
                    "the machine's shared engine {} belongs to another daemon and is not the \
                     engine this run needs; waiting for it to drain and retire",
                    engine.name
                ))));
            }
            return Ok(Err(Step::Wait(format!(
                "another daemon is preparing the machine's shared engine {}",
                claim.engine
            ))));
        }
        // Its daemon is gone: take the claim over once no live run is in it.
        if let Some(engine) = running {
            let survey = machine.survey(&engine.id).await?;
            if live(&survey) > 0 {
                let _ = machine.request_retire(&engine.id).await;
                return Ok(Err(Step::Wait(format!(
                    "the shared engine {} of a stopped daemon still runs other daemons' runs; \
                     waiting for them",
                    engine.name
                ))));
            }
        }
        self.take_over(machine, &claim, engine.as_ref()).await?;
        Ok(Err(Step::Again))
    }

    /// Remove an abandoned claim and its engine (proven gone).
    async fn take_over(
        &self,
        machine: &dyn MachineEngine,
        claim: &EngineClaim,
        engine: Option<&Foreign>,
    ) -> Result<(), String> {
        if let Some(engine) = engine {
            machine.remove_engine(engine).await?;
        }
        machine.remove_claim(&claim.id).await?;
        eprintln!(
            "bosn ci: took over the engine claim of stopped daemon pid {} ({})",
            claim.daemon.pid, claim.engine
        );
        Ok(())
    }

    /// Retire this daemon's engine when no run of any daemon holds it and it
    /// has been idle for `ttl` (or a daemon asked for it); whether it did.
    /// Slots of runs whose daemon died are reclaimed first.
    pub async fn retire_idle(&self, ttl: Duration) -> bool {
        let Ok(machine) = self.machine() else {
            return false;
        };
        let mut held = self.held.lock().await;
        let Some(current) = held.as_ref() else {
            return false;
        };
        if !self.backend.engine_running(&current.id).await {
            if let Some(current) = held.take()
                && matches!(current.kind, Kind::Owned { .. })
            {
                self.retire_owned(current).await;
            }
            return false;
        }
        if matches!(current.kind, Kind::Adopted) {
            return false;
        }
        let Ok(survey) = machine.survey(&current.id).await else {
            return false;
        };
        let limits =
            RunLimits::within_engine(current.memory_bytes, current.nano_cpus, current.pids);
        let reclaimed = self.reclaim(machine, &current.id, limits, &survey).await;
        if survey.held.len() > reclaimed
            || !(survey.requested || survey.idle_secs >= ttl.as_secs())
            || machine.begin_retire(&current.id).await != Ok(true)
        {
            return false;
        }
        if let Some(current) = held.take() {
            self.retire_owned(current).await;
        }
        true
    }

    /// Close the scopes of runs whose daemon died, and free their slots;
    /// how many were freed.
    async fn reclaim(
        &self,
        machine: &dyn MachineEngine,
        engine: &str,
        limits: RunLimits,
        survey: &Survey,
    ) -> usize {
        let mut freed = 0;
        for (slot, holder) in survey.held.iter().filter(|(_, h)| !h.daemon.alive()) {
            let Ok(scope) = RunScope::new(&holder.run, *slot, limits) else {
                continue;
            };
            if self.backend.close_scope(engine, &scope).await.is_ok()
                && machine.release_slot(engine, *slot).await.is_ok()
            {
                eprintln!(
                    "bosn ci: reclaimed slot {slot} of run {} (its daemon stopped)",
                    holder.run
                );
                freed += 1;
            }
        }
        freed
    }

    pub(super) async fn retire_owned(&self, current: Held) {
        let Kind::Owned { engine, claim } = current.kind else {
            return;
        };
        if let Err(error) = spare::retire(&self.registry, self.backend.as_ref(), &engine).await {
            eprintln!(
                "bosn ci: shared engine {} not removed: {error}",
                current.name
            );
            return;
        }
        if let Ok(machine) = self.machine()
            && let Err(error) = machine.remove_claim(&claim).await
        {
            eprintln!("bosn ci: engine claim not removed: {error}");
        }
    }

    /// The reaper's machine-wide pass: retire an idle engine whose daemon
    /// died, and idle legacy engines.
    pub async fn tend_machine(&self, ttl: Duration) {
        let Ok(machine) = self.machine() else {
            return;
        };
        let claim = match machine.claim().await {
            Ok(claim) => claim,
            Err(error) => {
                eprintln!("bosn ci: engine claim: {error}");
                return;
            }
        };
        if let Some(claim) = &claim
            && !claim.daemon.alive()
            && let Err(error) = self.retire_abandoned(machine, claim, ttl).await
        {
            eprintln!("bosn ci: abandoned shared engine {}: {error}", claim.engine);
        }
        let keep = claim.as_ref().map(|claim| claim.engine.as_str());
        if let Err(error) = self.drain_legacy(machine, keep).await {
            eprintln!("bosn ci: legacy engines: {error}");
        }
    }

    async fn retire_abandoned(
        &self,
        machine: &dyn MachineEngine,
        claim: &EngineClaim,
        ttl: Duration,
    ) -> Result<(), String> {
        let engine = machine.inspect(&claim.engine).await?;
        if let Some(engine) = engine.as_ref().filter(|engine| engine.running) {
            let survey = machine.survey(&engine.id).await?;
            if live(&survey) > 0 || !(survey.requested || survey.idle_secs >= ttl.as_secs()) {
                return Ok(());
            }
            // Every holder's daemon is gone: close their scopes first.
            let limits =
                RunLimits::within_engine(engine.memory_bytes, engine.nano_cpus, engine.pids);
            if self.reclaim(machine, &engine.id, limits, &survey).await < survey.held.len()
                || !machine.begin_retire(&engine.id).await?
            {
                return Ok(());
            }
        }
        let mut held = self.held.lock().await;
        if held
            .as_ref()
            .is_some_and(|current| current.name == claim.engine)
        {
            *held = None;
        }
        self.take_over(machine, claim, engine.as_ref()).await
    }

    /// Retire every idle legacy engine (any act engine but `keep` that runs
    /// nothing and is older than [`LEGACY_GRACE`]), each removal proven;
    /// the names of those still busy.
    pub(super) async fn drain_legacy(
        &self,
        machine: &dyn MachineEngine,
        keep: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let mut busy = Vec::new();
        let mut idle = std::collections::BTreeSet::new();
        for engine in machine.engines().await? {
            if Some(engine.name.as_str()) == keep {
                continue;
            }
            let young = now().saturating_sub(engine.created) < LEGACY_GRACE.as_secs();
            if young || machine.busy(&engine.id).await.unwrap_or(true) {
                busy.push(engine.name);
                continue;
            }
            idle.insert(engine.id.clone());
            if !self.state().idle_legacy.contains(&engine.id) {
                busy.push(engine.name);
                continue;
            }
            match machine.remove_engine(&engine).await {
                Ok(()) => eprintln!(
                    "bosn ci: retired idle legacy engine {} (registry {})",
                    engine.name, engine.registry
                ),
                Err(error) => {
                    eprintln!(
                        "bosn ci: legacy engine {} not retired: {error}",
                        engine.name
                    );
                    busy.push(engine.name);
                }
            }
        }
        self.state().idle_legacy = idle;
        Ok(busy)
    }
}

/// Another daemon's engine, as a run here uses it.
fn adopted(engine: &Foreign) -> Result<Held, String> {
    let host_dir = engine
        .socket_dir
        .clone()
        .ok_or("the shared engine binds no socket directory")?;
    #[cfg(unix)]
    let group = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&host_dir).map_or(0, |meta| meta.gid())
    };
    #[cfg(not(unix))]
    let group = 0;
    Ok(Held {
        kind: Kind::Adopted,
        id: engine.id.clone(),
        name: engine.name.clone(),
        identity: engine
            .identity
            .clone()
            .ok_or("the shared engine has no identity labels")?,
        socket: ActEngineDockerSocket { host_dir, group },
        memory_bytes: engine.memory_bytes,
        nano_cpus: engine.nano_cpus,
        pids: engine.pids,
        slots: Default::default(),
    })
}
