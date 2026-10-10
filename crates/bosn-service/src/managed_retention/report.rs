//! The unattended maintenance pass and its log lines (#456, #518, #545).

use std::path::Path;

use bosn_core::ResourceKind;
use bosn_core::retention::RetentionPolicy;
use bosn_engine::DockerEngine;

use super::{ManagedRetentionOutcome, SetupContainerReport, managed_retention_pass};
use crate::diagnostics::ManagedRetentionSummary;

/// The unattended pass the daemon runs on its maintenance interval.
///
/// Reclamation is on by default (#545): a machine must stay clean by itself. Only an explicit
/// `auto_retention = false` in the state directory's `retention.toml` turns it off, and then the
/// pass still reads the engine and reports what it would remove. Every removal keeps the
/// ownership, liveness, pin and age gates in [`super::managed_retention_pass`].
pub fn maintenance_pass(state_dir: &Path) {
    let policy = RetentionPolicy::default();
    let apply = auto_retention_enabled(state_dir);
    let engine = DockerEngine::docker();
    // #545: an idle keepalive is "running" and so held as in use forever. Stop the provably idle
    // ones first, so this same pass can reclaim them and the volumes they pinned.
    if apply {
        let idle = super::idle::retire_idle_keepalives(
            &engine,
            state_dir,
            policy.container_ttl,
            super::idle::MAX_STOPS_PER_PASS,
        );
        for line in idle.report_lines() {
            eprintln!("bosn retention: {line}");
        }
    }
    let outcome = managed_retention_pass(&engine, state_dir, policy, apply);
    report_pass(&outcome);
    // One timestamped line per pass, so `<state>/daemon.log` shows that passes run (#545).
    let summary = &outcome.summary;
    eprintln!(
        "bosn retention: pass finished at unix {:.0}: applied={} planned={} removed={} \
         removed_bytes={} failed={}",
        super::now_seconds(),
        summary.applied,
        summary.planned,
        summary.removed,
        summary.removed_bytes,
        summary.failed,
    );
}

/// Print what a pass did, or would do.
pub fn report_pass(outcome: &ManagedRetentionOutcome) {
    let summary = &outcome.summary;
    if let Some(refused) = &summary.refused {
        eprintln!("bosn retention: {refused}");
        return;
    }
    // #518: the stopped-container pile is reported whether or not anything is reclaimable, and
    // whether or not the operator opted in. It is the only signal a default install gets.
    report_setup_containers(&outcome.setup_containers, summary.applied);
    if let Some(line) = pass_report_line(summary) {
        eprintln!("bosn retention: {line}");
    }
    if let Some(line) = held_report_line(summary) {
        eprintln!("bosn retention: {line}");
    }
    for failure in &summary.failures {
        eprintln!("bosn retention: {failure}");
    }
}

/// The pass's one-line summary, or `None` when it planned nothing. An applied pass reports what
/// it actually removed, never what it planned (#551).
pub(super) fn pass_report_line(summary: &ManagedRetentionSummary) -> Option<String> {
    if summary.planned == 0 {
        return None;
    }
    Some(if summary.applied {
        format!(
            "removed {} of {} planned owned object(s), {} bytes, {} deferred, {} failed; \
             see them: bosn gc owned",
            summary.removed,
            summary.planned,
            summary.removed_bytes,
            summary.deferred,
            summary.failed,
        )
    } else {
        format!(
            "would remove {} owned object(s), {} deferred; see them: bosn gc owned",
            summary.planned, summary.deferred,
        )
    })
}

/// Why observed objects were kept, or `None` when nothing notable was held (#545).
///
/// Objects held only because they are young or alive are the normal state of a busy machine and
/// are not worth a line on every interval; the line appears when some object is held for a reason
/// an operator can act on (foreign or incomplete ownership, an unreadable age, a pin).
pub(super) fn held_report_line(summary: &ManagedRetentionSummary) -> Option<String> {
    use bosn_core::retention::HoldReason;
    let notable = summary
        .held
        .keys()
        .any(|reason| !matches!(reason, HoldReason::InUse | HoldReason::WithinTtl));
    if !notable {
        return None;
    }
    let parts: Vec<String> = summary
        .held
        .iter()
        .map(|(reason, count)| format!("{}={count}", reason.as_str()))
        .collect();
    Some(format!("kept owned object(s): {}", parts.join(", ")))
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
        "the daemon reclaims them once past the gate; see them: bosn gc owned"
    } else {
        "unattended reclamation is disabled by auto_retention = false in retention.toml"
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

/// The file that can opt a machine out of unattended reclamation.
const RETENTION_CONFIG: &str = "retention.toml";

/// Whether unattended reclamation runs (#545: on by default).
///
/// An absent file, or a file without the key, means "yes". An explicit `true`/`yes`/`1` means
/// "yes"; any other explicit value means "no", so a typo in an opt-out never deletes. A file that
/// exists but cannot be read means "no": the daemon cannot prove the operator did not opt out.
pub(super) fn auto_retention_enabled(state_dir: &Path) -> bool {
    let text = match std::fs::read_to_string(state_dir.join(RETENTION_CONFIG)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    let mut enabled = true;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "auto_retention" {
            enabled = matches!(value.trim(), "true" | "yes" | "1");
        }
    }
    enabled
}
