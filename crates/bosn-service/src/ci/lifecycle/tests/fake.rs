//! The synthetic engine host the lifecycle, spare and runtime tests drive.

use super::*;

/// Fault points for the synthetic engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    /// Fail before `docker create` is sent (an image, cache-volume or storage-volume step):
    /// the engine provably never existed (#554).
    pub pre_create: bool,
    /// `docker create` was sent and reported failure; Docker may still finish it.
    pub create: bool,
    /// Create the container, then report failure (partial creation).
    pub create_after_side_effect: bool,
    pub prepare: bool,
    pub exit_code: i32,
    pub hang: bool,
    pub remove: bool,
    pub inspect: bool,
    /// Resolving the engine image never finishes (a stuck pull).
    pub slow_image: bool,
    /// Saving the tool cache fails.
    pub save: bool,
    /// The shared cache cannot be measured.
    pub cache_measure: bool,
}

/// In-memory engine host: name -> observation. Creation and retirement
/// drive the real registry actor through the same commands, in the same
/// order, as the owned-engine layer does on Docker.
#[derive(Default)]
pub struct FakeBackend {
    pub engines: Mutex<BTreeMap<String, ActEngineObservation>>,
    pub faults: Mutex<Faults>,
    pub executions: Mutex<u32>,
    /// Whether the cache volume exists.
    pub cache: Mutex<bool>,
    /// Tool-cache saves that found their engine still present.
    pub saved_while_live: Mutex<u32>,
    /// What sampling the engine's storage reports; `None` fails the sample.
    pub storage: Mutex<Option<crate::ci::storage::StorageUsage>>,
    /// How long each storage sample takes to answer (#538).
    pub storage_delay: Mutex<Duration>,
    /// Storage samples started, and how many were in flight at once at most.
    pub storage_probes: Mutex<(u32, u32, u32)>,
    /// Engines prepared (act and runner image), spares included.
    pub engine_preparations: Mutex<u32>,
    /// The host the fake engine reports; [`FAKE_HOST`] unless set.
    pub host: Mutex<Option<crate::ci::limits::HostResources>>,
    /// Engine IDs whose container has stopped behind the daemon's back (#556).
    pub stopped: Mutex<std::collections::BTreeSet<String>>,
    /// Run labels whose scope was closed (#547).
    pub closed_scopes: Mutex<Vec<String>>,
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
    /// A host with room for a spare engine ([`crate::ci::spare::ROOM`]).
    pub fn roomy() -> Self {
        let backend = Self::default();
        *backend.host.lock().unwrap() = Some(ROOMY_HOST);
        backend
    }
    pub fn live(&self) -> usize {
        self.engines.lock().unwrap().len()
    }
    pub fn insert(
        &self,
        name: &str,
        image: &str,
        labels: BTreeMap<String, String>,
    ) -> ActEngineObservation {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        let observed = ActEngineObservation {
            name: name.into(),
            engine_id: format!("{:064x}", *next),
            image_digest: image.into(),
            labels,
        };
        self.engines
            .lock()
            .unwrap()
            .insert(name.into(), observed.clone());
        observed
    }
    fn live_id(&self, engine_id: &str) -> bool {
        self.engines
            .lock()
            .unwrap()
            .values()
            .any(|engine| engine.engine_id == engine_id)
    }
}
pub const LISTING: &str = "Stage  Job ID  Job name  Workflow name  Workflow file  Events\n\
                           0      a       a         w              ci.yml         push\n";
/// The machine the fake host engine reports.
pub const FAKE_HOST: crate::ci::limits::HostResources = crate::ci::limits::HostResources {
    total_memory: 16 << 30,
    available_memory: 12 << 30,
    cpus: 2,
    available_disk: Some(200 << 30),
};
/// A host with room for a spare.
pub const ROOMY_HOST: crate::ci::limits::HostResources = crate::ci::limits::HostResources {
    total_memory: 64 << 30,
    available_memory: 60 << 30,
    cpus: 8,
    available_disk: Some(200 << 30),
};
pub(super) fn later(record: &ActEngineRecord) -> f64 {
    now_seconds().max(record.updated_at)
}
impl ActEngineBackend for FakeBackend {
    fn engine_running<'a>(&'a self, engine_id: &'a str) -> crate::ci::engine::BoxFuture<'a, bool> {
        Box::pin(async move {
            self.live_id(engine_id) && !self.stopped.lock().unwrap().contains(engine_id)
        })
    }
    fn ensure_engine_image(&self) -> crate::ci::engine::BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if self.faults().slow_image {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
    fn host_resources(
        &self,
    ) -> crate::ci::engine::BoxFuture<'_, Result<crate::ci::limits::HostResources, String>> {
        let host = self.host.lock().unwrap().unwrap_or(FAKE_HOST);
        Box::pin(async move { Ok(host) })
    }
    fn ensure_cache<'a>(
        &'a self,
        _cache: &'a CacheVolume,
    ) -> crate::ci::engine::BoxFuture<'a, Result<(), String>> {
        *self.cache.lock().unwrap() = true;
        Box::pin(async { Ok(()) })
    }
    fn cache_usage<'a>(
        &'a self,
        volume: &'a str,
    ) -> crate::ci::engine::BoxFuture<'a, Result<crate::ci::CacheUsage, String>> {
        let exists = *self.cache.lock().unwrap();
        let failed = self.faults().cache_measure;
        Box::pin(async move {
            if failed {
                return Err("cache size: Docker unavailable".into());
            }
            Ok(crate::ci::CacheUsage {
                volume: volume.into(),
                bytes: exists.then_some(4096),
                allocated_bytes: exists.then_some(4096),
                ..Default::default()
            })
        })
    }
    fn storage_usage<'a>(
        &'a self,
        _engine: &'a str,
    ) -> crate::ci::engine::BoxFuture<'a, Result<crate::ci::storage::StorageUsage, String>> {
        let usage = *self.storage.lock().unwrap();
        let delay = *self.storage_delay.lock().unwrap();
        Box::pin(async move {
            {
                let mut probes = self.storage_probes.lock().unwrap();
                probes.0 += 1;
                probes.1 += 1;
                probes.2 = probes.2.max(probes.1);
            }
            async_engine::sleep(delay).await;
            self.storage_probes.lock().unwrap().1 -= 1;
            usage.ok_or_else(|| "df: not sampled".to_string())
        })
    }
    fn save_toolcache<'a>(
        &'a self,
        engine: &'a str,
    ) -> crate::ci::engine::BoxFuture<'a, Result<(), String>> {
        if self.live_id(engine) {
            *self.saved_while_live.lock().unwrap() += 1;
        }
        let fails = self.faults().save;
        Box::pin(async move {
            if fails {
                Err("disk full".into())
            } else {
                Ok(())
            }
        })
    }
    fn remove_cache<'a>(
        &'a self,
        _volume: &'a str,
    ) -> crate::ci::engine::BoxFuture<'a, Result<(), String>> {
        *self.cache.lock().unwrap() = false;
        Box::pin(async { Ok(()) })
    }
    fn create<'a>(
        &'a self,
        registry: &'a RegistryActor,
        intent: &'a ActEngineIntent,
        owner: &'a str,
        at: f64,
    ) -> crate::ci::engine::BoxFuture<'a, Result<ActEngineObservation, String>> {
        Box::pin(async move {
            let f = self.faults();
            commit(registry, ActRegistryCommand::Begin(intent.clone())).await?;
            if f.pre_create {
                return Err("synthetic failure before docker create".into());
            }
            commit(
                registry,
                ActRegistryCommand::CreateRequested {
                    run: intent.run_id.clone(),
                    at,
                },
            )
            .await?;
            if f.create {
                return Err("synthetic create failure".into());
            }
            let labels = intent.required_labels(owner).map_err(|e| e.to_string())?;
            let observed = self.insert(&intent.engine_name(), &intent.engine_image_digest, labels);
            if f.create_after_side_effect {
                return Err("synthetic create failure after side effect".into());
            }
            commit(
                registry,
                ActRegistryCommand::Register {
                    run: intent.run_id.clone(),
                    observed: observed.clone(),
                    at,
                },
            )
            .await?;
            Ok(observed)
        })
    }
    fn retire<'a>(
        &'a self,
        registry: &'a RegistryActor,
        owner: &'a str,
        record: &'a ActEngineRecord,
        _budget: Duration,
    ) -> crate::ci::engine::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let f = self.faults();
            if f.inspect {
                return Err("synthetic inspect failure".into());
            }
            let run = record.intent.run_id.clone();
            let name = record.intent.engine_name();
            let found = self.engines.lock().unwrap().get(&name).cloned();
            let engine_id = match found {
                None => {
                    crate::act_runtime::confirm_absence(record)
                        .map_err(|error| error.to_string())?;
                    record.engine_id.clone()
                }
                Some(observed) => {
                    let required = record
                        .intent
                        .required_labels(owner)
                        .map_err(|e| e.to_string())?;
                    if observed.labels != required {
                        return Err(format!("engine {name} is not provably ours"));
                    }
                    if record.engine_id.is_none() {
                        commit(
                            registry,
                            ActRegistryCommand::Recover {
                                run: run.clone(),
                                observed: observed.clone(),
                                at: later(record),
                            },
                        )
                        .await?;
                    }
                    let reply = registry
                        .act_registry(ActRegistryCommand::Authorize {
                            run: run.clone(),
                            observed: observed.clone(),
                        })
                        .await
                        .map_err(|e| format!("cleanup not authorized: {e}"))?;
                    if !matches!(reply, ActRegistryReply::Authorized(_)) {
                        return Err("cleanup not authorized".into());
                    }
                    if f.remove {
                        return Err("synthetic removal failure".into());
                    }
                    self.engines.lock().unwrap().remove(&name);
                    Some(observed.engine_id)
                }
            };
            commit(
                registry,
                ActRegistryCommand::Finalize {
                    run,
                    proof: ActEngineRemovalProof {
                        storage_volume: record.intent.storage_volume_name(),
                        name,
                        engine_id,
                    },
                    at: later(record),
                },
            )
            .await
        })
    }
    fn prepare_engine<'a>(
        &'a self,
        engine: &'a str,
        _act: ActArtifact,
    ) -> crate::ci::engine::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            assert!(self.live_id(engine), "prepare addresses the engine by ID");
            *self.engine_preparations.lock().unwrap() += 1;
            if self.faults().prepare {
                return Err("synthetic prepare failure".into());
            }
            Ok(())
        })
    }
    fn prepare_run<'a>(
        &'a self,
        engine: &'a str,
        _invocation: &'a ActInvocation,
        _source: &'a std::path::Path,
        _event: &'a std::path::Path,
        _generation: Option<&'a bosn_registry::act::ActToolGenerationBinding>,
    ) -> crate::ci::engine::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            assert!(self.live_id(engine), "prepare addresses the engine by ID");
            if self.stopped.lock().unwrap().contains(engine) {
                return Err("Error response from daemon: container is not running".into());
            }
            if self.faults().prepare {
                return Err("synthetic prepare failure".into());
            }
            Ok(())
        })
    }
    fn list<'a>(
        &'a self,
        _engine: &'a str,
        _invocation: &'a ActInvocation,
    ) -> crate::ci::engine::BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Ok(LISTING.to_string()) })
    }
    fn close_scope<'a>(
        &'a self,
        _engine: &'a str,
        scope: &'a crate::ci::engine::RunScope,
    ) -> crate::ci::engine::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.closed_scopes
                .lock()
                .unwrap()
                .push(scope.label().into());
            Ok(())
        })
    }
    fn execute<'a>(
        &'a self,
        _engine: &'a str,
        _invocation: &'a ActInvocation,
        deadline: Duration,
        cancellation: &'a CancellationToken,
        lines: &'a async_engine::Sender<EngineLine>,
    ) -> crate::ci::engine::BoxFuture<'a, Result<ExecEnd, String>> {
        Box::pin(async move {
            *self.executions.lock().unwrap() += 1;
            let f = self.faults();
            let _ = lines
                .send(EngineLine::Stdout(
                    r#"{"job":"w/a","jobID":"a","msg":"⭐ Run Main x","stage":"Main","stepID":["0"]}"#.into(),
                ))
                .await;
            let _ = lines
                .send(EngineLine::Stderr("act: warning on stderr".into()))
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
}
