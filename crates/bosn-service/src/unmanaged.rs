//! Daemon-side orchestration of the unmanaged-artifact census.
//!
//! The daemon owns the engine boundary, so the bounded read-only reads live here and the
//! classification lives in `bosn-core`. This module never mutates engine state and never
//! decides to delete; see [`docs/rust-unmanaged.md`](../../../docs/rust-unmanaged.md).

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bosn_core::{
    Census, CensusConfig, EngineObservation, InspectedVolume, SystemDfReport, census, observe,
};
use bosn_engine::{CensusRead, DockerEngine, RunOptions};

/// Deadline for each individual read. A census issues one accounting read plus one label
/// read per required label key.
pub const CENSUS_READ_DEADLINE: Duration = Duration::from_secs(30);
/// Output cap for each read. The accounting document is the large one, and a host with a
/// very large build cache can legitimately report several megabytes.
pub const CENSUS_READ_OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
/// Volumes named in one `docker volume inspect` invocation. Keeps argv bounded on a host
/// with thousands of volumes.
const CENSUS_INSPECT_CHUNK: usize = 256;
/// Volumes the census will inspect in total. Beyond this the census is partial rather than
/// silently leaving volumes unmeasured.
const CENSUS_INSPECT_MAX: usize = 2048;

/// One census pass.
#[derive(Clone, Debug, PartialEq)]
pub struct UnmanagedCensus {
    pub census: Census,
    /// Reads the engine refused or answered unreadably. A non-empty list means the census is
    /// partial, and a partial census must never authorise reclamation.
    pub unreadable: Vec<String>,
}

impl UnmanagedCensus {
    /// Whether the census can be trusted to authorise a plan.
    #[must_use]
    pub fn is_trustworthy(&self) -> bool {
        self.unreadable.is_empty() && !self.census.partial
    }
}

/// Run the bounded read-only census against one engine.
///
/// `our_registry` is this registry's UUID, used to separate *our* artifacts from everything
/// else. When it is unknown every complete label set resolves to `ForeignRegistry`, which
/// protects rather than exposes.
#[must_use]
pub fn unmanaged_census(
    engine: &DockerEngine,
    our_registry: Option<&str>,
    config: CensusConfig,
) -> UnmanagedCensus {
    let options = RunOptions::bounded(CENSUS_READ_DEADLINE, CENSUS_READ_OUTPUT_LIMIT);
    let mut unreadable = Vec::new();
    let report = match engine.system_df_verbose(options) {
        Ok(CensusRead::Document(text)) => match serde_json::from_str::<SystemDfReport>(&text) {
            Ok(report) => Some(report),
            Err(_) => {
                unreadable.push(
                    "docker system df -v returned a document this build cannot read".to_owned(),
                );
                None
            }
        },
        Ok(CensusRead::Unavailable { detail }) => {
            unreadable.push(detail);
            None
        }
        Err(error) => {
            unreadable.push(format!("docker system df -v failed: {error}"));
            None
        }
    };
    let observed = match &report {
        Some(report) => {
            let labeled = bosn_labeled_image_ids(engine, options, &mut unreadable);
            let dangling = ids_from(
                engine.image_ids_dangling(options),
                "docker image ls --filter dangling",
                &mut unreadable,
            );
            let inspected = inspect_volumes(engine, report, options, &mut unreadable);
            observe(EngineObservation {
                report,
                dangling_image_ids: &dangling,
                bosn_labeled_image_ids: &labeled,
                inspected_volumes: &inspected,
                now: now_seconds(),
            })
        }
        None => Vec::new(),
    };
    let mut result = census(&observed, our_registry, config);
    if !unreadable.is_empty() {
        result.partial = true;
    }
    UnmanagedCensus {
        census: result,
        unreadable,
    }
}

/// Image IDs carrying any required Bosn label.
///
/// `docker image ls` does not expose labels, so image ownership cannot be read from the
/// accounting document. A failure for any single key makes the whole set untrustworthy,
/// which the caller surfaces as `unreadable` rather than as "no labels".
fn bosn_labeled_image_ids(
    engine: &DockerEngine,
    options: RunOptions,
    unreadable: &mut Vec<String>,
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for key in bosn_core::REQUIRED_LABELS {
        ids.extend(ids_from(
            engine.image_ids_with_label(key, options),
            "docker image ls --filter label",
            unreadable,
        ));
    }
    ids.into_iter().collect()
}

/// Collect one read's newline-separated IDs, recording a refusal rather than discarding it.
fn ids_from(
    result: Result<CensusRead, bosn_engine::CommandError>,
    what: &str,
    unreadable: &mut Vec<String>,
) -> Vec<String> {
    match result {
        Ok(CensusRead::Document(text)) => text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        Ok(CensusRead::Unavailable { detail }) => {
            unreadable.push(detail);
            Vec::new()
        }
        Err(error) => {
            unreadable.push(format!("{what} failed: {error}"));
            Vec::new()
        }
    }
}

/// Volume detail for every volume in the accounting document.
///
/// Volume age is only available from `docker volume inspect`, so this is a required read
/// rather than an enrichment: without it every volume is protected as unmeasured.
fn inspect_volumes(
    engine: &DockerEngine,
    report: &SystemDfReport,
    options: RunOptions,
    unreadable: &mut Vec<String>,
) -> Vec<InspectedVolume> {
    let names: Vec<String> = report
        .volumes
        .iter()
        .map(|volume| volume.name.clone())
        .filter(|name| !name.is_empty())
        .collect();
    if names.len() > CENSUS_INSPECT_MAX {
        unreadable.push(format!(
            "{} volumes exceed the {} this census will inspect",
            names.len(),
            CENSUS_INSPECT_MAX
        ));
    }
    let mut inspected = Vec::new();
    for chunk in names.chunks(CENSUS_INSPECT_CHUNK).take(CENSUS_INSPECT_MAX / CENSUS_INSPECT_CHUNK)
    {
        let text = match engine.inspect_volumes(chunk, options) {
            Ok(CensusRead::Document(text)) => text,
            Ok(CensusRead::Unavailable { detail }) => {
                unreadable.push(detail);
                continue;
            }
            Err(error) => {
                unreadable.push(format!("docker volume inspect failed: {error}"));
                continue;
            }
        };
        match serde_json::from_str::<Vec<InspectedVolume>>(&text) {
            Ok(mut volumes) => inspected.append(&mut volumes),
            Err(_) => unreadable
                .push("docker volume inspect returned a document this build cannot read".to_owned()),
        }
    }
    inspected
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_census_is_never_trustworthy() {
        let census = UnmanagedCensus {
            census: Census::default(),
            unreadable: vec!["docker system df -v exited with 1".to_owned()],
        };
        assert!(!census.is_trustworthy());
    }

    #[test]
    fn a_partial_census_is_never_trustworthy() {
        let census = UnmanagedCensus {
            census: Census {
                partial: true,
                ..Census::default()
            },
            unreadable: Vec::new(),
        };
        assert!(!census.is_trustworthy());
    }

    #[test]
    fn a_complete_census_is_trustworthy() {
        let census = UnmanagedCensus {
            census: Census::default(),
            unreadable: Vec::new(),
        };
        assert!(census.is_trustworthy());
    }

    #[test]
    fn now_seconds_is_after_the_migration() {
        // A clock that cannot be read yields 0.0, which makes every age negative and
        // therefore ineligible: fail closed, never fail open.
        assert!(now_seconds() > 1_700_000_000.0);
    }
}
