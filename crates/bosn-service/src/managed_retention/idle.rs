//! Idle retirement of setup keepalive containers (#545, #536).
//!
//! A manifest or setup app with no declared command runs the fixed keepalive
//! ([`crate::MANIFEST_LINUX_IDLE_COMMAND`]) so later tasks can `exec` into it. Docker reports it
//! running forever, so managed retention holds it as in use, and it pins every volume it mounts.
//! This stops (never removes) exactly those containers once they are provably idle; the ordinary
//! pass then reclaims them under its own gates, and a later ensure simply starts or recreates one.
//!
//! A container is stopped only when every one of these holds, re-checked immediately before the
//! stop:
//!
//! - it is running, setup-labelled, and proven owned by a record in this registry;
//! - its command is the fixed keepalive launcher, not a declared app command;
//! - no lease, execution session or creation intent names it, and it carries no pin label;
//! - both its age and the registry's idle time are past the container gate;
//! - its process table is exactly the keepalive shell and its `sleep`.
//!
//! Salvaged from draft #546 (`retire_idle_manifests`), widened from manifest containers to every
//! registry-proven keepalive, and without its admission hold or cache protocol (#547).

use std::path::Path;
use std::time::Duration;

use bosn_core::ResourceKind;
use bosn_engine::{DockerEngine, RunOptions};

use super::registered::{LABEL_SETUP_MANAGED, RegisteredOwnership};
use super::{
    ContainerDetail, INSPECT_CHUNK, RETENTION_OUTPUT_LIMIT, RETENTION_READ_DEADLINE, labeled_ids,
    now_seconds, parse_inspect, parse_read,
};

/// At most this many keepalives are stopped per maintenance pass.
pub(super) const MAX_STOPS_PER_PASS: usize = 16;
/// Grace for the keepalive's `trap 'exit 0' TERM` before Docker kills it.
const STOP_GRACE_SECS: &str = "10";
const STOP_DEADLINE: Duration = Duration::from_secs(60);

/// What one idle-retirement step did.
#[derive(Debug, Default, PartialEq)]
pub(super) struct IdleOutcome {
    pub stopped: Vec<String>,
    pub failures: Vec<String>,
}

impl IdleOutcome {
    pub(super) fn report_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.stopped.is_empty() {
            lines.push(format!(
                "stopped {} idle keepalive container(s): {}",
                self.stopped.len(),
                self.stopped.join(", ")
            ));
        }
        lines.extend(self.failures.iter().cloned());
        lines
    }
}

/// Stop up to `limit` provably idle keepalive containers.
pub(super) fn retire_idle_keepalives(
    engine: &DockerEngine,
    state_dir: &Path,
    idle_ttl: Duration,
    limit: usize,
) -> IdleOutcome {
    let mut outcome = IdleOutcome::default();
    if limit == 0 {
        return outcome;
    }
    // No registry, no proof: nothing is stopped.
    let Ok(ownership) = RegisteredOwnership::load(state_dir) else {
        return outcome;
    };
    let options = RunOptions::bounded(RETENTION_READ_DEADLINE, RETENTION_OUTPUT_LIMIT);
    let mut unreadable = Vec::new();
    let Some(ids) = labeled_ids(
        engine.container_ids_with_label(LABEL_SETUP_MANAGED, options),
        "docker ps -a",
        &mut unreadable,
    ) else {
        return outcome;
    };
    let gate = idle_ttl.as_secs_f64();
    let mut candidates = Vec::new();
    for chunk in ids.chunks(INSPECT_CHUNK) {
        let Some(entries) = parse_inspect::<ContainerDetail>(
            engine.inspect_containers(chunk, options),
            "docker inspect",
            &mut unreadable,
        ) else {
            continue;
        };
        candidates.extend(
            entries
                .into_iter()
                .filter(|entry| idle_keepalive(&ownership, entry, gate)),
        );
    }
    for entry in candidates {
        if outcome.stopped.len() >= limit {
            break;
        }
        match stop_if_still_idle(engine, state_dir, &entry, gate, options) {
            Ok(true) => outcome.stopped.push(entry.engine_name().to_owned()),
            Ok(false) => {}
            Err(detail) => outcome.failures.push(detail),
        }
    }
    outcome
}

/// Every gate that can be decided from one inspect and one registry read.
fn idle_keepalive(ownership: &RegisteredOwnership, entry: &ContainerDetail, gate: f64) -> bool {
    if !entry.running() || !is_keepalive(entry) {
        return false;
    }
    let Some(age) = entry.created_age(now_seconds()) else {
        return false;
    };
    let labels = entry.labels();
    let Some(proof) = ownership.normalize(
        ResourceKind::Container,
        entry.engine_name(),
        &labels,
        now_seconds(),
    ) else {
        return false;
    };
    !proof.protected
        && proof
            .labels
            .get(bosn_core::LABEL_RETENTION)
            .map(String::as_str)
            != Some("pinned")
        && age >= gate
        && proof.idle_seconds >= gate
}

/// Re-read the container, its processes and the registry, then stop it.
///
/// `Ok(false)` when anything changed and the container is kept. A failed stop is an error that
/// leaves the container exactly as it was.
fn stop_if_still_idle(
    engine: &DockerEngine,
    state_dir: &Path,
    planned: &ContainerDetail,
    gate: f64,
    options: RunOptions,
) -> Result<bool, String> {
    let name = planned.engine_name();
    let Some(entries) = parse_read::<ContainerDetail>(
        engine.inspect_containers(std::slice::from_ref(&planned.id), options),
    ) else {
        return Err(format!("idle container {name} could not be re-read"));
    };
    let Some(entry) = entries.into_iter().next() else {
        return Ok(false);
    };
    let top = engine
        .with_args(["top", &entry.id, "-eo", "pid,comm"])
        .capture(options)
        .map_err(|error| format!("idle container {name}: process table unreadable: {error}"))?;
    if !top.ok() || !only_keepalive_processes(&top.stdout) {
        return Ok(false);
    }
    // The registry last: a task that registered a session after the first read protects here.
    let fresh = RegisteredOwnership::load(state_dir)
        .map_err(|error| format!("idle container {name}: registry unreadable: {error}"))?;
    if entry.id != planned.id || !idle_keepalive(&fresh, &entry, gate) {
        return Ok(false);
    }
    let stopped = engine
        .with_args(["stop", "--time", STOP_GRACE_SECS, &entry.id])
        .capture(RunOptions::bounded(STOP_DEADLINE, 64 * 1024))
        .map_err(|error| format!("stopping idle container {name}: {error}"))?;
    if stopped.ok() {
        Ok(true)
    } else if stopped.reports_missing() {
        Ok(false)
    } else {
        Err(format!(
            "stopping idle container {name}: {}",
            String::from_utf8_lossy(&stopped.stderr).trim()
        ))
    }
}

/// The exact keepalive launcher argv, never a declared command that merely ends in one.
fn is_keepalive(entry: &ContainerDetail) -> bool {
    entry
        .config
        .as_ref()
        .and_then(|config| config.cmd.as_deref())
        .is_some_and(|cmd| {
            cmd == bosn_setup::login_shell_args(crate::MANIFEST_LINUX_IDLE_COMMAND).as_slice()
        })
}

/// `docker top -eo pid,comm` shows exactly one shell and one `sleep`.
fn only_keepalive_processes(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut lines = text.lines();
    if lines
        .next()
        .is_none_or(|header| header.split_whitespace().collect::<Vec<_>>() != ["PID", "COMMAND"])
    {
        return false;
    }
    let (mut shells, mut sleeps) = (0, 0);
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 || fields[0].parse::<u32>().is_err() {
            return false;
        }
        match fields[1] {
            "sh" => shells += 1,
            "sleep" => sleeps += 1,
            _ => return false,
        }
    }
    shells == 1 && sleeps == 1
}

#[cfg(test)]
mod tests;
