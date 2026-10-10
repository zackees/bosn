//! The daemon's side of the shared engine (#547): which lifecycle a planned
//! run takes, idle retirement, and retirement at shutdown.

use std::time::Duration;

use super::super::{lifecycle::EngineReport, shared_engine::DEFAULT_IDLE_RETIRE};
use super::*;

impl CiRuntime {
    /// Run a planned run in the shared engine when its plan names one, else
    /// on its own engine.
    pub(super) async fn run_planned(
        &self,
        mut plan: EnginePlan,
        cancellation: &async_engine::CancellationToken,
        observer: &mut RunObserver,
    ) -> EngineReport {
        match plan.shared.take() {
            Some(want) => {
                lifecycle::shared::run_on_shared(
                    self.backend.as_ref(),
                    &self.shared,
                    want,
                    &plan,
                    cancellation,
                    observer,
                )
                .await
            }
            None => {
                lifecycle::run_on_engine(
                    &self.registry,
                    self.backend.as_ref(),
                    &plan,
                    cancellation,
                    observer,
                )
                .await
            }
        }
    }

    /// Retire the shared engine once no run has used it for
    /// `[engine] idle_retire_secs` (read on every check).
    pub(super) fn start_shared_reaper(&self) {
        let state_dir = self.state_dir.clone();
        self.shared.spawn_reaper(move || idle_retire(&state_dir));
    }

    /// Retire an idle shared engine and lease no more (daemon shutdown).
    pub async fn close_shared(&self) {
        self.shared.close().await;
    }
}

/// `[engine] idle_retire_secs`, or [`DEFAULT_IDLE_RETIRE`].
fn idle_retire(state_dir: &std::path::Path) -> Duration {
    super::super::config::load_engine(state_dir)
        .ok()
        .and_then(|config| config.idle_retire_secs)
        .map_or(DEFAULT_IDLE_RETIRE, Duration::from_secs)
}
