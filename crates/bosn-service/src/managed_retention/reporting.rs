//! Automatic retention configuration and operator reports.

use super::*;

/// The unattended pass the daemon runs on its maintenance interval.
///
/// Reclamation runs by default. Set `auto_retention = false` in the state directory's
/// `retention.toml` to opt out; ownership, liveness and age gates always apply.
pub fn maintenance_pass(state_dir: &Path) {
    let policy = RetentionPolicy::default();
    let apply = automatic_retention_enabled(state_dir);
    let engine = DockerEngine::docker();
    let outcome = managed_retention_pass(&engine, state_dir, policy, apply);
    report_pass(&outcome);
}

/// Print what a pass did, or would do.
pub fn report_pass(outcome: &ManagedRetentionOutcome) {
    let summary = &outcome.summary;
    if let Some(refused) = &summary.refused {
        eprintln!("bosn retention: incomplete pass: {refused}");
    }
    // #518: the stopped-container pile is reported whether or not anything is reclaimable, and
    // whether or not the operator opted out.
    report_setup_containers(&outcome.setup_containers, summary.applied);
    for held in &summary.held {
        eprintln!("bosn retention: protected {held}");
    }
    let omitted = summary.held_total.saturating_sub(summary.held.len() as u64);
    if omitted > 0 {
        eprintln!("bosn retention: {omitted} additional protection decision detail(s) omitted");
    }
    let verb = if summary.applied {
        "removed"
    } else {
        "would remove"
    };
    let count = if summary.applied {
        summary.removed
    } else {
        summary.planned
    };
    let bytes = if summary.applied {
        summary.removed_bytes
    } else {
        outcome.plan.bytes
    };
    eprintln!(
        "bosn retention: {verb} {count} owned object(s), {bytes} bytes, {} deferred ({} failure(s))",
        summary.deferred, summary.failed,
    );
    let mut removals = Vec::new();
    for receipt in &outcome.deletion_receipts {
        details::push(&mut removals, receipt.describe());
    }
    for removal in &removals {
        eprintln!("bosn retention: {removal}");
    }
    let omitted = outcome
        .deletion_receipts
        .len()
        .saturating_sub(removals.len());
    if omitted > 0 {
        eprintln!("bosn retention: {omitted} additional removal receipt(s) omitted");
    }
    for failure in &summary.failures {
        eprintln!("bosn retention: {failure}");
    }
    let omitted = summary.failed.saturating_sub(summary.failures.len() as u64);
    if omitted > 0 {
        eprintln!("bosn retention: {omitted} additional failure detail(s) omitted");
    }
}

/// Print the stopped setup-container pile, if there is one.
///
/// The message leads with the volume count, because that is the actual cost: a stopped
/// `bosn-setup-v2-*` container is kilobytes of writable layer holding megabytes of volumes
/// unreclaimable. `applied` only changes the advice, never the facts.
fn report_setup_containers(report: &SetupContainerReport, applied: bool) {
    if let Some(line) = setup_container_report_line(report, applied) {
        eprintln!("bosn retention: {line}");
    }
}

/// The one-line report for a stopped-container pile, or `None` when there is nothing to report.
///
/// Split from the printing so the message is testable without capturing stderr.
pub(super) fn setup_container_report_line(
    report: &SetupContainerReport,
    applied: bool,
) -> Option<String> {
    if report.is_empty() {
        return None;
    }
    let past_gate = past_container_gate(report);
    let oldest = report.oldest_age_seconds().map_or_else(
        || "unknown age".to_owned(),
        |age| format!("{:.1}h old", age / 3600.0),
    );
    let action = if applied {
        "initial inventory for apply pass; protected objects are listed separately"
    } else {
        "preview only"
    };
    Some(format!(
        "{} stopped owned setup container(s), oldest {oldest}, pinning {} volume(s), {} past the \
         {} container gate; {action}",
        report.container_count(),
        report.pinned_volume_count(),
        past_gate,
        describe_container_gate(),
    ))
}

/// How many stopped containers are already past the container age gate.
///
/// The gate comes from `bosn-core`'s policy rather than a constant invented here, so the number
/// the report calls stale is the same one an apply pass would act on.
fn past_container_gate(report: &SetupContainerReport) -> usize {
    let gate = RetentionPolicy::default()
        .ttl_for(ResourceKind::Container)
        .map_or(0.0, |ttl| ttl.as_secs_f64());
    report
        .stopped
        .iter()
        .filter(|container| container.age_seconds >= gate)
        .count()
}

/// The container gate, phrased for a human reading a log line.
fn describe_container_gate() -> String {
    let gate = RetentionPolicy::default()
        .ttl_for(ResourceKind::Container)
        .map_or(0, |ttl| ttl.as_secs() / 3600);
    format!("{gate}h")
}

/// The file that lets a machine opt out of unattended reclamation.
const RETENTION_CONFIG: &str = "retention.toml";

/// Automatic retention is enabled unless the operator explicitly opts out.
pub fn automatic_retention_enabled(state_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(state_dir.join(RETENTION_CONFIG)) else {
        return true;
    };
    #[derive(Default, serde::Deserialize)]
    struct RetentionConfig {
        auto_retention: Option<bool>,
        retention: Option<RetentionSection>,
    }
    #[derive(serde::Deserialize)]
    struct RetentionSection {
        auto_retention: Option<bool>,
    }
    let config = toml::from_str::<RetentionConfig>(&text).unwrap_or_default();
    config
        .auto_retention
        .or_else(|| config.retention.and_then(|section| section.auto_retention))
        .unwrap_or(true)
}
