//! Runner management: list, drain/resume, the live limit, pruning finished
//! runs, and the machine-wide cache volume.

use super::*;

impl CiRuntime {
    pub(super) async fn runners(&self, action: RunnerAction) -> Result<RunnersReply, CiError> {
        let (mut pruned, mut cache) = (None, None);
        match &action {
            RunnerAction::PruneCache {
                older_than_secs,
                max_bytes,
            } => pruned = Some(self.prune(*older_than_secs, *max_bytes).await?),
            RunnerAction::CacheUsage => cache = Some(self.cache_usage().await?),
            RunnerAction::ClearCache => cache = Some(self.clear_cache().await?),
            RunnerAction::List
            | RunnerAction::Drain
            | RunnerAction::Resume
            | RunnerAction::SetLimit { .. } => {}
        }
        let mut state = self.lock();
        match action {
            RunnerAction::List
            | RunnerAction::PruneCache { .. }
            | RunnerAction::CacheUsage
            | RunnerAction::ClearCache => {}
            RunnerAction::Drain => state.scheduler.set_drained(true),
            RunnerAction::Resume => state.scheduler.set_drained(false),
            RunnerAction::SetLimit { limit } => {
                if !(1..=256).contains(&limit) {
                    return Err(CiError::refused("limit must be between 1 and 256"));
                }
                state.scheduler.set_limit(limit);
            }
        }
        self.store.save_settings(&Settings {
            limit: state.scheduler.limit(),
            drained: state.scheduler.drained(),
        });
        let status = runner_status(
            &state.scheduler,
            self.widget_presence(),
            self.spares.status(),
        );
        drop(state);
        self.kick();
        Ok(RunnersReply {
            runners: status,
            pruned_runs: pruned,
            cache,
        })
    }

    async fn cache_usage(&self) -> Result<CacheUsage, CiError> {
        let owner = self
            .registry
            .status()
            .await
            .map_err(|error| CiError::new("internal", error.to_string()))?
            .registry_id;
        Ok(self
            .backend
            .tracked_cache_usage(CACHE_VOLUME, &self.registry, &owner)
            .await
            .unwrap_or_else(|error| CacheUsage {
                volume: CACHE_VOLUME.into(),
                partial: true,
                errors: vec![error.chars().take(1024).collect()],
                ..CacheUsage::default()
            }))
    }

    /// Remove the machine-wide cache volume. Refused while a run executes;
    /// dispatch holds queued runs until the volume is gone.
    async fn clear_cache(&self) -> Result<CacheUsage, CiError> {
        {
            let mut state = self.lock();
            if state.scheduler.running() > 0 {
                return Err(CiError::refused(
                    "runs are executing; let them finish or cancel them, then clear the cache",
                ));
            }
            state.clearing_cache = true;
        }
        // The spare engine mounts the volume: retire it first.
        self.spares.retire_all().await;
        let cleared = match self.backend.remove_cache(CACHE_VOLUME).await {
            Ok(()) => self.cache_usage().await,
            Err(error) => Err(CiError::new("internal", error)),
        };
        // Measure the removed volume before dispatch may prepare a new spare
        // and recreate it. Otherwise the reply races that preparation.
        self.lock().clearing_cache = false;
        self.kick();
        cleared
    }

    /// Remove finished runs by age, total size, or beyond the retention
    /// counts, and drop sources of all but the newest few. Live runs are
    /// kept. Sizes are measured and files deleted off the state lock.
    pub(super) async fn prune(
        &self,
        older_than: Option<u64>,
        max_bytes: Option<u64>,
    ) -> Result<Vec<String>, CiError> {
        let sizes = match max_bytes {
            None => BTreeMap::new(),
            Some(_) => {
                let finished = self.lock().finished_newest_first();
                let store = self.store.clone();
                blocking(move || {
                    finished
                        .into_iter()
                        .map(|id| {
                            let size = store.run_bytes(&id);
                            (id, size)
                        })
                        .collect()
                })
                .await?
            }
        };
        let plan = self.lock().plan_prune(older_than, max_bytes, &sizes);
        let (store, writer) = (self.store.clone(), self.writer.clone());
        let pruned = plan.runs.clone();
        blocking(move || plan.apply(&store, &writer)).await?;
        Ok(pruned)
    }
}

/// What one prune pass deletes: decided under the lock, applied outside it.
struct PrunePlan {
    runs: Vec<String>,
    sources: Vec<String>,
}
impl PrunePlan {
    fn apply(&self, store: &Store, writer: &RecordWriter) {
        for id in &self.runs {
            writer.remove_run(id);
        }
        for id in &self.sources {
            store.remove_source(id);
        }
    }
}

impl CiState {
    fn finished_newest_first(&self) -> Vec<String> {
        self.order
            .iter()
            .rev()
            .filter(|id| self.runs[*id].record.state == RunState::Done)
            .cloned()
            .collect()
    }

    /// Drop pruned runs from the state now; their files go in `apply`.
    fn plan_prune(
        &mut self,
        older_than: Option<u64>,
        max_bytes: Option<u64>,
        sizes: &BTreeMap<String, u64>,
    ) -> PrunePlan {
        let now = lifecycle::now_seconds();
        let mut plan = PrunePlan {
            runs: Vec::new(),
            sources: Vec::new(),
        };
        let mut kept_bytes = 0;
        for (newest_first, id) in self.finished_newest_first().into_iter().enumerate() {
            let age = now - self.runs[&id].record.created_at;
            let size = sizes.get(&id).copied().unwrap_or(0);
            if newest_first >= KEEP_RUNS
                || older_than.is_some_and(|s| age > s as f64)
                || max_bytes.is_some_and(|m| kept_bytes + size > m)
            {
                self.runs.remove(&id);
                plan.runs.push(id);
                continue;
            }
            if newest_first >= KEEP_SOURCES {
                plan.sources.push(id);
            }
            kept_bytes += size;
        }
        self.order.retain(|id| self.runs.contains_key(id));
        plan
    }
}
