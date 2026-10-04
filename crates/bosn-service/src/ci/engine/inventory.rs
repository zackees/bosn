//! Bounded current destination inventory through the verified act2 binary.
use super::{DockerActBackend, RunOptions, owned};
use crate::ci::{cache_cohort::Namespace, cache_maintenance::NamespaceAudit};
use std::time::Duration;

#[derive(Debug)]
pub struct InventoryAttempt {
    pub exit_code: i32,
    pub report: NamespaceAudit,
}

impl InventoryAttempt {
    pub fn require_current_inventory(&self) -> Result<(), String> {
        if self.exit_code != 0 {
            return Err("cache inventory command failed".into());
        }
        self.report.require_current_inventory()
    }
}

impl DockerActBackend {
    /// Caller supplies a verified engine. This reads one bounded catalog page
    /// and whole-store summaries; it does not enroll or delete cache data.
    pub async fn audit_cache_destination(
        &self,
        engine: &str,
        namespace: &Namespace,
    ) -> Result<InventoryAttempt, String> {
        let binary = format!("{}/bin/act", super::ENGINE_WORK);
        let output = self
            .run_bounded(
                owned(&[
                    "exec",
                    engine,
                    &binary,
                    "cache",
                    "audit",
                    "--cache-server-path",
                    &namespace.path(),
                ]),
                RunOptions::bounded(Duration::from_secs(30), 64 * 1024),
            )
            .await
            .map_err(|e| format!("current cache inventory is unknown: {e}"))?;
        // A nonzero command can still provide valid busy/partial evidence.
        Ok(InventoryAttempt {
            exit_code: output.exit_code,
            report: NamespaceAudit::parse_inventory(&output.stdout, namespace)?,
        })
    }
}
