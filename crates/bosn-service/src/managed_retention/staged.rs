//! Re-observe dependency liveness after each resource kind has been reclaimed.

use super::*;

/// Containers release volume/image references. A single pre-removal census would
/// otherwise hold those objects until another maintenance interval.
#[must_use]
pub fn managed_retention_pass(
    engine: &DockerEngine,
    state_dir: &Path,
    policy: RetentionPolicy,
    apply: bool,
) -> ManagedRetentionOutcome {
    let _budget = budget::Guard::start(budget::PASS_LIMIT);
    let _admission = match gate::retention() {
        Ok(guard) => guard,
        Err(error) => {
            let mut outcome = pass_limited(
                engine,
                state_dir,
                policy,
                false,
                bosn_core::retention::MAX_MANAGED_REMOVALS,
            );
            outcome.summary.refused = Some(error);
            return outcome;
        }
    };
    pass_limited(
        engine,
        state_dir,
        policy,
        apply,
        bosn_core::retention::MAX_MANAGED_REMOVALS,
    )
}

pub(super) fn pass_limited(
    engine: &DockerEngine,
    state_dir: &Path,
    policy: RetentionPolicy,
    apply: bool,
    limit: usize,
) -> ManagedRetentionOutcome {
    if !apply {
        return retention_stage(engine, state_dir, policy, false, None, limit);
    }
    let mut outcome = retention_stage(
        engine,
        state_dir,
        policy,
        true,
        Some(ResourceKind::Container),
        limit,
    );
    if outcome.summary.refused.is_some() {
        return outcome;
    }
    let mut reserved_bytes = outcome.plan.bytes.max(outcome.summary.removed_bytes);
    for kind in [ResourceKind::Volume, ResourceKind::Image] {
        let remaining = limit.saturating_sub(outcome.plan.candidates.len());
        let mut stage_policy = policy;
        stage_policy.max_bytes = policy
            .max_bytes
            .map(|ceiling| ceiling.saturating_sub(reserved_bytes).max(0));
        let stage = retention_stage(engine, state_dir, stage_policy, true, Some(kind), remaining);
        reserved_bytes =
            reserved_bytes.saturating_add(stage.plan.bytes.max(stage.summary.removed_bytes));
        let refused = stage.summary.refused.is_some();
        merge(&mut outcome, stage);
        if refused {
            break;
        }
    }
    outcome
}

pub(super) fn merge(outcome: &mut ManagedRetentionOutcome, stage: ManagedRetentionOutcome) {
    outcome.deletion_receipts.extend(stage.deletion_receipts);
    outcome.summary.planned += stage.summary.planned;
    outcome.summary.removed += stage.summary.removed;
    outcome.summary.removed_bytes += stage.summary.removed_bytes;
    outcome.summary.deferred += stage.summary.deferred;
    outcome.summary.failed += stage.summary.failed;
    for detail in stage.summary.failures {
        details::push(&mut outcome.summary.failures, detail);
    }
    outcome.summary.held_total = outcome
        .summary
        .held_total
        .saturating_add(stage.summary.held_total);
    for detail in stage.summary.held {
        details::push(&mut outcome.summary.held, detail);
    }
    outcome.plan.bytes += stage.plan.bytes;
    outcome.plan.deferred += stage.plan.deferred;
    outcome.plan.candidates.extend(stage.plan.candidates);
    outcome.plan.held.extend(stage.plan.held);
    if stage.summary.refused.is_some() {
        outcome.summary.refused = stage.summary.refused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected(total: u64) -> ManagedRetentionOutcome {
        ManagedRetentionOutcome {
            deletion_receipts: Vec::new(),
            summary: ManagedRetentionSummary {
                held_total: total,
                held: (0..total.min(64))
                    .map(|n| format!("protected {n}"))
                    .collect(),
                applied: true,
                planned: 0,
                removed: 0,
                removed_bytes: 0,
                deferred: 0,
                failed: 0,
                failures: Vec::new(),
                refused: None,
            },
            plan: bosn_core::retention::RetentionPlan::default(),
            setup_containers: SetupContainerReport::default(),
        }
    }

    #[test]
    fn stage_merge_preserves_protection_totals_when_details_are_full() {
        let mut outcome = protected(100);
        merge(&mut outcome, protected(200));
        assert_eq!(outcome.summary.held_total, 300);
        assert_eq!(outcome.summary.held.len(), 64);
        assert_eq!(
            outcome.summary.held_total - outcome.summary.held.len() as u64,
            236
        );
    }
}
