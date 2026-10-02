//! When the daemon prepares its spare engine (#410): while nothing is
//! queued and a run slot is free, so the spare counts against the
//! concurrency limit; only when `[engine] spares` keeps one and the host has
//! [`ROOM`] to spare; never while the cache volume is being cleared.

use bosn_registry::act::ActEngineBinding;

use super::super::spare::{ROOM, SparePlan, Spares};
use super::*;

impl CiRuntime {
    /// Start preparing the spare if the daemon is idle enough to keep one.
    pub(super) fn maybe_prepare_spare(&self) {
        let idle = {
            let state = self.lock();
            !state.clearing_cache
                && !state.scheduler.drained()
                && state.scheduler.queued() == 0
                && state.scheduler.running() < state.scheduler.limit()
        };
        if !idle {
            return;
        }
        let enabled = super::super::config::load(&self.state_dir)
            .is_ok_and(|config| config.engine.spares == Spares::One);
        if !enabled {
            self.spares.discard();
            return;
        }
        let Some(fill) = self.spares.begin() else {
            return;
        };
        let runtime = self.clone();
        async_engine::launch(async move {
            // Stopping the daemon must not wait for planning (an image pull).
            let plan = async_engine::cancellable(fill.cancellation(), runtime.spare_plan())
                .await
                .unwrap_or(Ok(None));
            runtime.spares.fill(fill, plan).await;
        })
        .detach();
    }

    /// The spare to prepare now; `None` when the host has no room for one.
    async fn spare_plan(&self) -> Result<Option<SparePlan>, String> {
        // Room first: a host without it never pulls the engine image for a spare.
        if self.backend.host_resources().await?.available_memory < ROOM {
            return Ok(None);
        }
        let config = super::super::config::load(&self.state_dir)?.engine;
        let spec = self.engine_spec(config).await?;
        let id = new_uuid().await.map_err(|e| e.message)?;
        let workspace = self.state_dir.to_string_lossy().into_owned();
        Ok(Some(SparePlan {
            intent: spec.intent(ActEngineBinding::spare(&id, &workspace), true),
            act: spec.act,
            cache: spec.cache,
        }))
    }

    /// Retire the spare and stop preparing new ones (daemon shutdown).
    pub async fn close_spares(&self) {
        self.spares.close().await;
    }
}
