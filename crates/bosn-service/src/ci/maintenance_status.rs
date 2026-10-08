//! Latest evidence from this daemon's registry; never machine-wide totals.
use bosn_registry::cache_maintenance::{
    ActionMaintenanceOutcome, MaintenanceOutcome, MaintenanceSnapshot, ToolMaintenanceOutcome,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MaintenanceStatus {
    NeverObserved,
    Unavailable,
    Recorded {
        observed_at: f64,
        recovery_failed: bool,
        cleanup_failed: bool,
        outcome: MaintenanceResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_maintenance: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action_maintenance: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MaintenanceResult {
    Unknown,
    Observed {
        exit_code: i32,
        partial: bool,
        budget_bytes: i64,
        remaining_completed_bytes: Option<u64>,
        protected_bytes: Option<u64>,
        budget_met: Option<bool>,
        reclaimed_archive_bytes: Option<u64>,
    },
}

impl From<MaintenanceSnapshot> for MaintenanceStatus {
    fn from(snapshot: MaintenanceSnapshot) -> Self {
        let outcome = match snapshot.outcome {
            MaintenanceOutcome::Unknown { .. } => MaintenanceResult::Unknown,
            MaintenanceOutcome::Observed {
                exit_code,
                partial,
                budget_bytes,
                remaining_completed_bytes,
                protected_bytes,
                budget_met,
                reclaimed_archive_bytes,
            } => MaintenanceResult::Observed {
                exit_code,
                partial,
                budget_bytes,
                remaining_completed_bytes,
                protected_bytes,
                budget_met,
                reclaimed_archive_bytes,
            },
        };
        Self::Recorded {
            observed_at: snapshot.observed_at,
            recovery_failed: snapshot.recovery_error.is_some(),
            cleanup_failed: snapshot
                .helper
                .is_some_and(|helper| helper.cleanup_error.is_some()),
            outcome,
            action_maintenance: snapshot.action_outcome.map(|outcome| match outcome {
                ActionMaintenanceOutcome::Held { diagnostic } => {
                    format!("actions: held: {diagnostic}")
                }
                ActionMaintenanceOutcome::Observed { stats } => format!(
                    "actions: allocated bytes {} -> {}; budget {}; retired classes {}",
                    stats.allocated_before,
                    stats.allocated_after,
                    stats.budget_bytes,
                    stats.retired_classes
                ),
            }),
            tool_maintenance: snapshot.tool_outcome.map(|outcome| match outcome {
                ToolMaintenanceOutcome::NotEnrolled => "tools: not enrolled".into(),
                ToolMaintenanceOutcome::Held { diagnostic } => format!("tools: held: {diagnostic}"),
                ToolMaintenanceOutcome::Observed { stats } => format!(
                    "tools: allocated bytes {} -> {}; retired generations {}, objects {}",
                    stats.allocated_before,
                    stats.allocated_after,
                    stats.retired_generations,
                    stats.retired_objects
                ),
            }),
        }
    }
}

impl MaintenanceStatus {
    pub fn summary(&self) -> String {
        match self {
            Self::NeverObserved => "maintenance: never observed by this registry".into(),
            Self::Unavailable => "maintenance: registry evidence unavailable".into(),
            Self::Recorded {
                observed_at,
                recovery_failed,
                cleanup_failed,
                outcome,
                tool_maintenance,
                action_maintenance,
            } => {
                let result = match outcome {
                    MaintenanceResult::Unknown => "unknown result".to_owned(),
                    MaintenanceResult::Observed {
                        exit_code,
                        partial,
                        budget_met,
                        reclaimed_archive_bytes,
                        ..
                    } => format!(
                        "exit {exit_code}; {}; archive budget {}; reclaimed archive bytes {}",
                        if *partial {
                            "partial result"
                        } else {
                            "complete result"
                        },
                        match budget_met {
                            Some(true) => "met",
                            Some(false) => "not met",
                            None => "unknown",
                        },
                        reclaimed_archive_bytes
                            .map_or_else(|| "unknown".into(), |bytes| bytes.to_string()),
                    ),
                };
                format!(
                    "maintenance: last recorded at {observed_at:.3} (Unix seconds, this registry); {result}{}{}{}{}",
                    if *recovery_failed {
                        "; helper recovery failed"
                    } else {
                        ""
                    },
                    if *cleanup_failed {
                        "; helper cleanup failed"
                    } else {
                        ""
                    },
                    tool_maintenance
                        .as_ref()
                        .map_or_else(String::new, |tools| format!("; {tools}")),
                    action_maintenance
                        .as_ref()
                        .map_or_else(String::new, |actions| format!("; {actions}"))
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_evidence_keeps_unknown_reclamation_and_failed_cleanup_visible() {
        let snapshot = MaintenanceSnapshot {
            schema_version: 1,
            observed_at: 123.0,
            helper: Some(bosn_registry::cache_maintenance::MaintenanceHelper {
                nonce: "11111111-2222-4333-8444-555555555555".into(),
                container_id: "a".repeat(64),
                cleanup_error: Some("private output".into()),
            }),
            outcome: MaintenanceOutcome::Observed {
                exit_code: 1,
                partial: true,
                budget_bytes: 100,
                remaining_completed_bytes: Some(200),
                protected_bytes: Some(200),
                budget_met: Some(false),
                reclaimed_archive_bytes: None,
            },
            recovery_error: None,
            action_outcome: Some(ActionMaintenanceOutcome::Held {
                diagnostic: "reader lease busy".into(),
            }),
            tool_outcome: None,
        };
        snapshot.validate().unwrap();
        let status = MaintenanceStatus::from(snapshot);
        let text = status.summary();
        assert!(text.contains("partial result"));
        assert!(text.contains("budget not met"));
        assert!(text.contains("reclaimed archive bytes unknown"));
        assert!(text.contains("helper cleanup failed"));
        assert!(text.contains("actions: held: reader lease busy"));
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("private output"));
        assert!(!json.contains("container_id"));
        assert_ne!(
            MaintenanceStatus::Unavailable,
            MaintenanceStatus::NeverObserved
        );
    }
}
