//! The fake host engine's singleton side (#544): one claim, and a slot
//! table per engine, shared by every daemon (registry) driving the fake.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::ci::{
    engine::BoxFuture,
    machine::{EngineClaim, EngineIdentity, Foreign, Holder, Leased, MachineEngine, Survey},
};

#[derive(Default)]
pub struct Table {
    pub ready: bool,
    pub held: BTreeMap<u16, Holder>,
    pub requested: bool,
    pub retiring: bool,
    pub idle_secs: u64,
}

#[derive(Default)]
pub struct FakeMachine {
    pub claim: Mutex<Option<EngineClaim>>,
    pub tables: Mutex<BTreeMap<String, Table>>,
    /// Engine IDs running something (legacy drain).
    pub busy: Mutex<BTreeSet<String>>,
    /// Engines removed without their registry (takeover, legacy drain).
    pub removed: Mutex<Vec<String>>,
    next: Mutex<u64>,
}

impl FakeBackend {
    fn foreign(&self, observed: &ActEngineObservation) -> Foreign {
        let labels = &observed.labels;
        Foreign {
            id: observed.engine_id.clone(),
            name: observed.name.clone(),
            running: !self.stopped.lock().unwrap().contains(&observed.engine_id),
            identity: EngineIdentity::from_labels(labels),
            registry: labels
                .get("com.zackees.bosn.registry")
                .cloned()
                .unwrap_or_default(),
            socket_dir: Some(format!("/nonexistent/sock/{}", observed.name)),
            storage_volume: None,
            memory_bytes: 8 << 30,
            nano_cpus: 2_000_000_000,
            pids: 4096,
            created: labels
                .get("com.zackees.bosn.created")
                .and_then(|at| at.parse::<f64>().ok())
                .map_or(0, |at| at as u64),
        }
    }
}

impl MachineEngine for FakeBackend {
    fn claim(&self) -> BoxFuture<'_, Result<Option<EngineClaim>, String>> {
        let claim = self.machine.claim.lock().unwrap().clone();
        Box::pin(async move { Ok(claim) })
    }
    fn create_claim<'a>(&'a self, claim: &'a EngineClaim) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let mut slot = self.machine.claim.lock().unwrap();
            if slot.is_some() {
                return Ok(false);
            }
            let mut next = self.machine.next.lock().unwrap();
            *next += 1;
            *slot = Some(EngineClaim {
                id: format!("claim-{next}"),
                ..claim.clone()
            });
            Ok(true)
        })
    }
    fn remove_claim<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut slot = self.machine.claim.lock().unwrap();
            if slot.as_ref().is_some_and(|claim| claim.id == id) {
                *slot = None;
            }
            Ok(())
        })
    }
    fn inspect<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Foreign>, String>> {
        let found = self.engines.lock().unwrap().get(name).cloned();
        Box::pin(async move { Ok(found.map(|observed| self.foreign(&observed))) })
    }
    fn engines(&self) -> BoxFuture<'_, Result<Vec<Foreign>, String>> {
        let all: Vec<_> = self.engines.lock().unwrap().values().cloned().collect();
        Box::pin(async move {
            Ok(all
                .iter()
                .map(|observed| self.foreign(observed))
                .filter(|engine| engine.running)
                .collect())
        })
    }
    fn busy<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        let busy = self.machine.busy.lock().unwrap().contains(engine);
        Box::pin(async move { Ok(busy) })
    }
    fn remove_engine<'a>(&'a self, engine: &'a Foreign) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.engines.lock().unwrap().remove(&engine.name);
            self.machine
                .removed
                .lock()
                .unwrap()
                .push(engine.name.clone());
            Ok(())
        })
    }
    fn lease_slot<'a>(
        &'a self,
        engine: &'a str,
        holder: &'a Holder,
    ) -> BoxFuture<'a, Result<Leased, String>> {
        Box::pin(async move {
            let mut tables = self.machine.tables.lock().unwrap();
            let table = tables.entry(engine.into()).or_default();
            if table.retiring {
                return Ok(Leased::Retiring);
            }
            if !table.ready {
                return Ok(Leased::Preparing);
            }
            let Some(slot) =
                (0..crate::ci::engine::MAX_SLOTS).find(|s| !table.held.contains_key(s))
            else {
                return Ok(Leased::Full);
            };
            table.held.insert(slot, holder.clone());
            table.idle_secs = 0;
            Ok(Leased::Slot(slot))
        })
    }
    fn release_slot<'a>(&'a self, engine: &'a str, slot: u16) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut tables = self.machine.tables.lock().unwrap();
            let table = tables.entry(engine.into()).or_default();
            table.held.remove(&slot);
            table.idle_secs = 0;
            Ok(())
        })
    }
    fn survey<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<Survey, String>> {
        Box::pin(async move {
            let mut tables = self.machine.tables.lock().unwrap();
            let table = tables.entry(engine.into()).or_default();
            Ok(Survey {
                held: table.held.iter().map(|(s, h)| (*s, h.clone())).collect(),
                idle_secs: table.idle_secs,
                requested: table.requested,
                retiring: table.retiring,
            })
        })
    }
    fn mark_ready<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut tables = self.machine.tables.lock().unwrap();
            tables.entry(engine.into()).or_default().ready = true;
            Ok(())
        })
    }
    fn request_retire<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut tables = self.machine.tables.lock().unwrap();
            tables.entry(engine.into()).or_default().requested = true;
            Ok(())
        })
    }
    fn begin_retire<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let mut tables = self.machine.tables.lock().unwrap();
            let table = tables.entry(engine.into()).or_default();
            if table.held.is_empty() {
                table.retiring = true;
            }
            Ok(table.retiring && table.held.is_empty())
        })
    }
}
