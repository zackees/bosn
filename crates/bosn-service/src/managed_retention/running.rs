//! Running Bosn containers and how long they have been up, for `bosn scan` (#536).
//!
//! Read-only. A running container is never a retention candidate, so without this report an
//! idle keepalive or engine that lives for days is invisible. Each row says whether it is a
//! setup keepalive (which maintenance stops once idle past the container gate) and whether its
//! process table is, right now, only the keepalive.

use bosn_engine::{DockerEngine, RunOptions};
use serde::Serialize;

use super::registered::LABEL_SETUP_MANAGED;
use super::{
    ContainerDetail, INSPECT_CHUNK, RETENTION_OUTPUT_LIMIT, labeled_union, now_seconds,
    parse_inspect,
};

/// One running Bosn-labelled container.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunningContainer {
    pub name: String,
    /// Seconds since Docker started it (creation time when the start time is unreadable).
    pub up_seconds: f64,
    /// It runs the fixed setup keepalive launcher rather than a declared command.
    pub keepalive: bool,
    /// A keepalive whose process table is exactly its shell and `sleep`: no task is running.
    pub idle: bool,
}

/// Every running Bosn container, longest-running first, plus any read that failed.
#[must_use]
///
/// `deadline` bounds each engine read, like the census's `--census-deadline-ms`.
pub fn running_containers(
    engine: &DockerEngine,
    deadline: std::time::Duration,
) -> (Vec<RunningContainer>, Vec<String>) {
    let options = RunOptions::bounded(deadline, RETENTION_OUTPUT_LIMIT);
    let mut unreadable = Vec::new();
    let mut rows = Vec::new();
    let Some(ids) = labeled_union(
        &[bosn_core::LABEL_KIND, LABEL_SETUP_MANAGED],
        |probe| engine.container_ids_with_label(probe, options),
        "docker ps -a",
        &mut unreadable,
    ) else {
        return (rows, unreadable);
    };
    let now = now_seconds();
    for chunk in ids.chunks(INSPECT_CHUNK) {
        let Some(entries) = parse_inspect::<ContainerDetail>(
            engine.inspect_containers(chunk, options),
            "docker inspect",
            &mut unreadable,
        ) else {
            continue;
        };
        for entry in entries.into_iter().filter(ContainerDetail::running) {
            let keepalive = super::idle::is_keepalive(&entry);
            let idle = keepalive && super::idle::keepalive_only(engine, &entry.id, options);
            rows.push(RunningContainer {
                name: entry.engine_name().to_owned(),
                up_seconds: entry
                    .started_age(now)
                    .or_else(|| entry.created_age(now))
                    .unwrap_or(0.0),
                keepalive,
                idle,
            });
        }
    }
    rows.sort_by(|left, right| {
        right
            .up_seconds
            .total_cmp(&left.up_seconds)
            .then_with(|| left.name.cmp(&right.name))
    });
    (rows, unreadable)
}
