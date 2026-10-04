//! One bounded maintenance pass, independent of workflow servers.
use super::{DockerActBackend, RunOptions, owned};
use crate::ci::{cache_maintenance::CohortReport, cache_policy::CachePolicy};
use std::time::Duration;

#[derive(Debug)]
pub struct MaintenanceAttempt {
    pub exit_code: i32,
    pub report: CohortReport,
    pub diagnostic: String,
}
impl MaintenanceAttempt {
    pub fn require_complete(&self) -> Result<(), String> {
        if self.exit_code != 0 || self.report.partial {
            return Err("cohort maintenance remains incomplete".into());
        }
        Ok(())
    }
    pub fn require_budget_met(&self) -> Result<(), String> {
        self.require_complete()?;
        if self.report.budget_met != Some(true) {
            return Err("cohort archive budget remains unmet".into());
        }
        Ok(())
    }
}

impl DockerActBackend {
    /// Caller supplies a verified maintenance helper and the shared enrolled
    /// policy. This executes no workflow and authorizes no legacy-source deletion.
    pub async fn maintain_cache_cohort(
        &self,
        engine: &str,
        policy: CachePolicy,
    ) -> Result<MaintenanceAttempt, String> {
        let mut args = owned(&["exec", engine]);
        args.extend(super::maintenance_lease::command());
        args.extend(policy.maintenance_pass_args());
        let output = self
            .docker
            .with_args(args)
            .capture_async(RunOptions::bounded(Duration::from_secs(30), 64 * 1024))
            .await
            .map_err(|e| format!("cohort maintenance outcome is unknown: {e}"))?;
        let diagnostic = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(256)
            .collect::<String>();
        // Nonzero exits can report partial reclamation. Preserve that evidence.
        let report = CohortReport::parse(&output.stdout, policy).map_err(|e| {
            format!("cohort maintenance has no valid outcome evidence: {e}; {diagnostic}")
        })?;
        Ok(MaintenanceAttempt {
            exit_code: output.exit_code,
            report,
            diagnostic,
        })
    }
}
