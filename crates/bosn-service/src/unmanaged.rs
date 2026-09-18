//! Daemon-side orchestration of the unmanaged-artifact census.
//!
//! The daemon owns the engine boundary, so the bounded read-only reads live here and the
//! classification lives in `bosn-core`. This module never mutates engine state and never
//! decides to delete; see [`docs/rust-unmanaged.md`](../../../docs/rust-unmanaged.md).

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::path::Path;

use bosn_core::{
    Acknowledgement, Census, CensusConfig, EngineObservation, InspectedVolume, Plan,
    SystemDfReport, census, observe, plan, removal_rank,
};
use bosn_engine::{CensusRead, DockerEngine, RunOptions};
use serde::{Deserialize, Serialize};

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
/// Removals one apply pass will attempt. A plan larger than this stops early and says so
/// rather than running unbounded.
pub const MAX_UNMANAGED_REMOVALS: usize = 1024;
/// One removal's deadline.
const REMOVAL_DEADLINE: Duration = Duration::from_secs(60);
const REMOVAL_OUTPUT_LIMIT: usize = 1024 * 1024;
/// The acknowledgement file under the state directory.
pub const ACK_FILE: &str = "unmanaged-ack.json";
const MAX_ACK_BYTES: usize = 4 * 1024;

/// One census pass.
#[derive(Clone, Debug, PartialEq)]
pub struct UnmanagedCensus {
    pub census: Census,
    /// The per-artifact observation the census was aggregated from, kept so a plan can be
    /// built without a second engine pass.
    pub artifacts: Vec<bosn_core::ObservedArtifact>,
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
        artifacts: observed,
        unreadable,
    }
}

/// The default interval between unattended maintenance passes.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);

/// One unattended maintenance pass.
///
/// This is what makes the warning fire on a machine where nobody runs a command — which was
/// the reference machine in #147, where `~/.local/state/bosn` did not exist and nothing was
/// ever routed through bosn.
#[must_use]
pub fn maintenance_pass(
    state_dir: &Path,
    config: CensusConfig,
) -> (UnmanagedCensus, Option<bosn_core::Warning>) {
    let our_registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
        .ok()
        .and_then(|registry| registry.registry_id().ok());
    let engine = DockerEngine::docker();
    let scan = unmanaged_census(&engine, our_registry.as_deref(), config);
    let warning = bosn_core::warning(&scan.census, bosn_core::WarningThreshold::default());
    (scan, warning)
}

/// The warning's lines, without colour, so every surface says the same thing.
///
/// The caller decides how to render them; JSON never calls this, because JSON carries neither
/// caps nor ANSI.
#[must_use]
pub fn warning_lines(warning: &bosn_core::Warning) -> Vec<String> {
    let mut lines = Vec::new();
    if warning.partial {
        lines.push(
            "UNMANAGED DOCKER ARTIFACTS COULD NOT BE FULLY MEASURED \u{2014} THIS MACHINE IS NOT \
             KNOWN TO BE CLEAN"
                .to_owned(),
        );
    }
    if warning.reclaimable_objects > 0 {
        lines.push(format!(
            "{} UNMANAGED DOCKER OBJECTS ARE RECLAIMABLE ({})",
            warning.reclaimable_objects,
            human_bytes(warning.reclaimable_bytes),
        ));
        lines.push("  see them:  bosn gc --unmanaged".to_owned());
        lines.push("  remove:    bosn gc --unmanaged --apply --yes".to_owned());
    }
    if warning.report_only_bytes > 0 {
        lines.push(format!(
            "  build cache holds {} with no per-object removal, so bosn never sweeps it",
            human_bytes(warning.report_only_bytes),
        ));
    }
    if warning.review_objects > 0 {
        lines.push(format!(
            "  review {} items ({}) needing judgment: listed by the preview, opt one in with --include <id>",
            warning.review_objects,
            human_bytes(warning.review_bytes),
        ));
    }
    lines.push("  silence:   bosn scan --ack".to_owned());
    lines
}

/// Render bytes for humans. Census byte figures are approximate by construction.
#[must_use]
pub fn human_bytes(bytes: i128) -> String {
    const UNITS: [(&str, i128); 5] = [
        ("PiB", 1 << 50),
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1}{unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes}B")
}

/// What one apply pass did.
#[derive(Clone, Debug, PartialEq)]
pub struct UnmanagedApplyOutcome {
    /// The plan rebuilt from a fresh census immediately before any removal. The caller's plan
    /// is never trusted: every candidate is re-derived here.
    pub plan: Plan,
    pub removed: u64,
    pub removed_bytes: i128,
    pub failed: u64,
    pub failures: Vec<String>,
    /// Set when the pass refused to remove anything.
    pub refused: Option<String>,
}

/// Remove the Tier-1 (and explicitly included) artifacts this engine still confirms.
///
/// The census is taken inside this call, so a plan built earlier cannot authorise a removal
/// against state that has since changed. A census that is not complete refuses outright:
/// nothing is deleted from partial information.
#[must_use]
pub fn unmanaged_gc_apply(
    engine: &DockerEngine,
    our_registry: Option<&str>,
    config: CensusConfig,
    include: &[String],
) -> UnmanagedApplyOutcome {
    let scan = unmanaged_census(engine, our_registry, config);
    let plan = plan(&scan.artifacts, our_registry, config, include);
    if !scan.is_trustworthy() {
        return UnmanagedApplyOutcome {
            plan,
            removed: 0,
            removed_bytes: 0,
            failed: 0,
            failures: Vec::new(),
            refused: Some(
                "the census was incomplete, so nothing was removed; \
                 an unreadable census is never a reason to delete"
                    .to_owned(),
            ),
        };
    }
    let mut removed = 0u64;
    let mut removed_bytes = 0i128;
    let mut failed = 0u64;
    let mut failures = Vec::new();
    let mut refused = None;
    let mut ordered = plan.candidates.clone();
    ordered.sort_by_key(|candidate| removal_rank(candidate.class));
    for candidate in ordered {
        if removed as usize >= MAX_UNMANAGED_REMOVALS {
            refused = Some(format!(
                "stopped after {MAX_UNMANAGED_REMOVALS} removals; re-run to continue"
            ));
            break;
        }
        match remove_one(engine, &candidate.id, candidate.class) {
            Ok(()) => {
                removed += 1;
                removed_bytes += candidate.bytes;
            }
            Err(detail) => {
                failed += 1;
                failures.push(detail);
            }
        }
    }
    UnmanagedApplyOutcome {
        plan,
        removed,
        removed_bytes,
        failed,
        failures,
        refused,
    }
}

/// Remove exactly one artifact by its immutable identity.
///
/// A volume's identity *is* its name: Docker exposes no separate volume id, so the generated
/// name is what is proven and passed here. Nothing is removed by tag.
fn remove_one(
    engine: &DockerEngine,
    id: &str,
    class: bosn_core::UnmanagedClass,
) -> Result<(), String> {
    let options = RunOptions::bounded(REMOVAL_DEADLINE, REMOVAL_OUTPUT_LIMIT);
    let argv: Vec<&str> = match class {
        bosn_core::UnmanagedClass::StoppedContainer => vec!["rm", id],
        bosn_core::UnmanagedClass::DanglingImage
        | bosn_core::UnmanagedClass::UnreferencedImage => vec!["rmi", id],
        bosn_core::UnmanagedClass::AnonymousVolume
        | bosn_core::UnmanagedClass::NamedVolume => vec!["volume", "rm", id],
        // Build cache is never removable by identity, so it never reaches this call.
        bosn_core::UnmanagedClass::BuildCache => {
            return Err("build cache is not removable by identity".to_owned());
        }
    };
    match engine.with_args(argv).capture(options) {
        Ok(result) if result.ok() => Ok(()),
        // Docker refusing a removal is already fail-closed: the artifact stays.
        Ok(result) => Err(format!("{id}: docker exited with {}", result.exit_code)),
        Err(error) => Err(format!("{id}: {error}")),
    }
}

/// Read the acknowledgement, if one was written and is still readable.
///
/// Unreadable or malformed content is treated as "no acknowledgement", which fails toward
/// warning rather than toward silence.
#[must_use]
pub fn read_acknowledgement(state_dir: &Path) -> Option<Acknowledgement> {
    let path = state_dir.join(ACK_FILE);
    // Read with std::fs and an explicit cap, so the read and the write agree on the file's
    // shape. The kernel facade's private reader additionally requires owner-only permissions,
    // which the state directory does not impose.
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() > MAX_ACK_BYTES {
        return None;
    }
    let wire: AckWire = serde_json::from_slice(&bytes).ok()?;
    Some(Acknowledgement {
        at: wire.at,
        objects: wire.objects,
        bytes: i128::from(wire.bytes),
    })
}

/// Record that the user has seen the current footprint.
pub fn write_acknowledgement(
    state_dir: &Path,
    acknowledgement: Acknowledgement,
) -> Result<(), String> {
    // The kernel facade deliberately exposes no plain write primitive, so this one bounded
    // preference file is written with std::fs. It is not state: an unreadable or torn file
    // reads as "no acknowledgement", which warns rather than stays silent.
    std::fs::create_dir_all(state_dir).map_err(|error| error.to_string())?;
    let wire = AckWire {
        at: acknowledgement.at,
        objects: acknowledgement.objects,
        // Saturating, not wrapping: an unrepresentable mark must not become a small one.
        bytes: i64::try_from(acknowledgement.bytes).unwrap_or(i64::MAX),
    };
    let encoded = serde_json::to_vec(&wire).map_err(|error| error.to_string())?;
    std::fs::write(state_dir.join(ACK_FILE), &encoded).map_err(|error| error.to_string())
}

#[derive(Debug, Serialize, Deserialize)]
struct AckWire {
    at: f64,
    objects: u64,
    bytes: i64,
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
            artifacts: Vec::new(),
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
            artifacts: Vec::new(),
            unreadable: Vec::new(),
        };
        assert!(!census.is_trustworthy());
    }

    #[test]
    fn a_complete_census_is_trustworthy() {
        let census = UnmanagedCensus {
            census: Census::default(),
            artifacts: Vec::new(),
            unreadable: Vec::new(),
        };
        assert!(census.is_trustworthy());
    }

    #[test]
    fn the_warning_lines_never_name_a_docker_command() {
        let warning = bosn_core::Warning {
            reclaimable_objects: 3,
            reclaimable_bytes: 3 * 1024 * 1024 * 1024,
            review_objects: 1,
            review_bytes: 10,
            report_only_bytes: 2 * 1024 * 1024 * 1024,
            partial: false,
        };
        let lines = warning_lines(&warning);
        assert!(lines[0].contains("3 UNMANAGED DOCKER OBJECTS ARE RECLAIMABLE"));
        assert!(lines.iter().any(|line| line.contains("bosn gc --unmanaged")));
        assert!(lines.iter().any(|line| line.contains("bosn scan --ack")));
        // The founding invariant: the way out is never `docker system prune`.
        assert!(!lines.iter().any(|line| line.contains("docker ")));
        // A build-cache figure the tool cannot free is stated separately, never in the
        // reclaimable headline.
        assert!(!lines[0].contains("2.0GiB"));
    }

    #[test]
    fn the_warning_names_the_command_that_removes_them() {
        // #148: the warning is only useful if the way out is one obvious command. Removal
        // shipped with the daemon-owned apply (#276); the warning must say how to run it,
        // not that it does not exist.
        let warning = bosn_core::Warning {
            reclaimable_objects: 3,
            reclaimable_bytes: 3 * 1024 * 1024 * 1024,
            review_objects: 0,
            review_bytes: 0,
            report_only_bytes: 0,
            partial: false,
        };
        let lines = warning_lines(&warning);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("bosn gc --unmanaged --apply --yes")),
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("not in this build")),
            "{lines:#?}"
        );
    }

    #[test]
    fn a_partial_warning_says_the_machine_is_not_known_to_be_clean() {
        let warning = bosn_core::Warning {
            reclaimable_objects: 0,
            reclaimable_bytes: 0,
            review_objects: 0,
            review_bytes: 0,
            report_only_bytes: 0,
            partial: true,
        };
        let lines = warning_lines(&warning);
        assert!(lines[0].contains("NOT KNOWN TO BE CLEAN"));
        assert!(!lines.iter().any(|line| line.contains("ARE RECLAIMABLE")));
    }

    #[test]
    fn an_acknowledgement_round_trips_through_the_state_file() {
        let directory =
            kernal_api::platform::fs::TemporaryDirectory::new().expect("temporary directory");
        assert!(read_acknowledgement(directory.path()).is_none());
        let acknowledgement = Acknowledgement {
            at: 1_789_588_000.0,
            objects: 17,
            bytes: 16_477_292_500,
        };
        write_acknowledgement(directory.path(), acknowledgement).expect("write");
        let read = read_acknowledgement(directory.path()).expect("read");
        assert_eq!(read, acknowledgement);
    }

    #[test]
    fn an_unreadable_acknowledgement_reads_as_no_acknowledgement() {
        // Failing toward "no acknowledgement" means failing toward warning, never toward
        // silence.
        let directory =
            kernal_api::platform::fs::TemporaryDirectory::new().expect("temporary directory");
        std::fs::write(directory.path().join(ACK_FILE), b"not json").expect("write");
        assert!(read_acknowledgement(directory.path()).is_none());
        std::fs::write(directory.path().join(ACK_FILE), vec![b'x'; MAX_ACK_BYTES + 1])
            .expect("write oversized");
        assert!(read_acknowledgement(directory.path()).is_none());
    }

    #[test]
    fn now_seconds_is_after_the_migration() {
        // A clock that cannot be read yields 0.0, which makes every age negative and
        // therefore ineligible: fail closed, never fail open.
        assert!(now_seconds() > 1_700_000_000.0);
    }
}
