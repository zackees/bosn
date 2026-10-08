//! Daemon-side reclamation of the resources this registry owns (#456).
//!
//! [`crate::unmanaged`] deliberately protects everything this registry owns and can therefore
//! never reclaim it. That left three kinds of resource with no reclamation path at all:
//!
//! - `bosn-setup-v2-*` containers, created without `--rm` and with no idle reaper. A stopped one
//!   keeps every volume it mounted alive, so this is upstream of the volume leak.
//! - `bosn-v-stack-*` / `bosn-v-machine-*` volumes, which the token-bound release path can
//!   remove one at a time but which nothing retires by age.
//! - `bosn-setup:*` images, which have no expiry and are excluded from the unmanaged census
//!   precisely because they are ours.
//!
//! This module runs the age gates in [`bosn_core::retention`] against a fresh engine read and,
//! on an apply pass, re-checks each object immediately before removing it. The policy is pure and
//! unit-tested in `bosn-core`; everything here is I/O.
//!
//! #518: stopped setup containers are reported and reclaimed by default
//!
//! `bosn-setup-v2-*` containers cannot be created with `--rm`: `validate_observed` actively
//! enforces `AutoRemove == false`, so the container is designed to persist and reclamation must
//! come from here. A persisted container pins every volume it ever mounted, which is why this is
//! a disk problem and not a container-count problem — and why the report counts *pinned volumes*,
//! not containers.
//!
//! Reclamation runs automatically unless explicitly disabled in `retention.toml`.
//! Reporting runs on every maintenance interval, including when reclamation is disabled.
//!
//!
//! Three invariants hold for every removal this module performs:
//!
//! 1. **The object is re-verified after the plan is built.** A pass that takes minutes can see a
//!    volume mounted by a run that started after the read, so every candidate is re-inspected
//!    and re-classified immediately before its own removal.
//! 2. **An incomplete read removes nothing.** If the engine would not answer, the pass reports a
//!    refusal rather than treating "unknown" as "safe".
//! 3. **Only label-proven objects are touched.** The candidate set comes from Docker's own label
//!    filter over the Bosn namespace, and every candidate is re-proven by
//!    [`bosn_core::retention::classify_managed`] before removal. A name is never evidence.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use bosn_core::ResourceKind;
use bosn_core::retention::{RetentionPolicy, RetentionVerdict, classify_managed, plan_managed};
use bosn_engine::{CensusRead, DockerEngine, RunOptions};

use crate::diagnostics::ManagedRetentionSummary;

mod admission;
mod budget;
pub(crate) mod deletion_intents;
mod deletion_recovery;
mod details;
pub(crate) mod gate;
mod idle;
pub(crate) mod image_recovery;
mod images;
mod observations;
mod receipts;
use observations::observe_owned;
pub(crate) use receipts::DeletionReceipt;
pub(crate) mod peers;
pub(crate) mod registered;
mod reporting;
mod staged;
pub(crate) use admission::run_for_jobs;
pub use reporting::{automatic_retention_enabled, maintenance_pass, report_pass};
#[cfg(test)]
use reporting::{
    automatic_retention_enabled as auto_retention_enabled, setup_container_report_line,
};
pub use staged::managed_retention_pass;

/// Deadline for each individual engine read.
pub const RETENTION_READ_DEADLINE: Duration = Duration::from_secs(30);
/// GC performs a complete census and sequential, bounded removals. It cannot
/// share the three-second deadline used for status RPCs.
pub(crate) const RETENTION_REPLY_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// Output cap for a read.
const RETENTION_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
/// Deadline for one removal.
const RETENTION_REMOVAL_DEADLINE: Duration = Duration::from_secs(60);
const RETENTION_REMOVAL_OUTPUT_LIMIT: usize = 1024 * 1024;
/// Ids or names resolved per `inspect` batch. Keeps argv bounded on a host with thousands.
const INSPECT_CHUNK: usize = 128;

/// What one pass did, or would do.
#[derive(Clone, Debug)]
pub struct ManagedRetentionOutcome {
    pub(crate) deletion_receipts: Vec<receipts::DeletionReceipt>,
    pub summary: ManagedRetentionSummary,
    /// The plan the pass ran, kept for the daemon log and for tests.
    pub plan: bosn_core::retention::RetentionPlan,
    /// Stopped Bosn-owned setup containers and the volumes each one pins (#518).
    pub setup_containers: SetupContainerReport,
}

/// One stopped, Bosn-owned container and the volumes it is holding alive.
///
/// A persisted container's cost is not its own writable layer but the mounts it keeps referenced:
/// a stopped container keeps every volume it ever mounted eligible for neither GC nor release.
#[derive(Clone, Debug, PartialEq)]
pub struct StoppedSetupContainer {
    /// The container id, as Docker reported it.
    pub id: String,
    /// Effective idle age, clamped by latest recorded use. Zero when unknown.
    pub age_seconds: f64,
    /// The named volumes this container still mounts.
    pub pinned_volumes: Vec<String>,
}

/// Every stopped Bosn-owned container the pass observed, oldest first.
///
/// Reporting is unconditional; deletion is not. Nothing here is ever removed by this struct — it
/// exists so a default install, which has no `retention.toml`, still learns that it is leaking.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SetupContainerReport {
    pub stopped: Vec<StoppedSetupContainer>,
}

impl SetupContainerReport {
    /// Number of stopped Bosn-owned containers.
    pub fn container_count(&self) -> usize {
        self.stopped.len()
    }

    /// Distinct volumes pinned across every stopped container.
    ///
    /// Counted once per volume, not once per container, because a volume shared by two stopped
    /// containers is one blob on one filesystem.
    pub fn pinned_volume_count(&self) -> usize {
        let mut names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for container in &self.stopped {
            names.extend(container.pinned_volumes.iter().map(String::as_str));
        }
        names.len()
    }

    /// The age of the oldest stopped container, when there is one.
    pub fn oldest_age_seconds(&self) -> Option<f64> {
        self.stopped
            .iter()
            .map(|container| container.age_seconds)
            .fold(None, |oldest, age| {
                Some(oldest.map_or(age, |prior: f64| prior.max(age)))
            })
    }

    /// Whether there is anything to say.
    pub fn is_empty(&self) -> bool {
        self.stopped.is_empty()
    }
}

/// Called only while the job actor holds admission and has no pending work.
pub(crate) fn managed_retention_with_idle(
    engine: &DockerEngine,
    state_dir: &Path,
    policy: RetentionPolicy,
    apply: bool,
) -> ManagedRetentionOutcome {
    idle::run(
        engine,
        state_dir,
        policy,
        apply,
        bosn_core::retention::MAX_MANAGED_REMOVALS,
    )
}

/// Run one managed-retention pass.
///
/// `apply` is the only thing that mutates the engine. A preview still pays for a full read,
/// because a preview that cannot prove ownership is not a preview.
#[must_use]
fn retention_stage(
    engine: &DockerEngine,
    state_dir: &Path,
    policy: RetentionPolicy,
    apply: bool,
    kind: Option<ResourceKind>,
    remaining_objects: usize,
) -> ManagedRetentionOutcome {
    let our_registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
        .ok()
        .and_then(|registry| registry.registry_id().ok());

    let (mut artifacts, stopped_containers, refusal) = observe_owned(engine, state_dir);
    // Report stopped containers independently of whether this pass applies removals.
    let setup_containers = SetupContainerReport {
        stopped: stopped_containers,
    };

    // An incomplete read must not authorize a removal. This is the rule the unmanaged census
    // follows too, and for the same reason: "we could not see it" is not "it is safe".
    if let Some(detail) = refusal {
        return refused_outcome(detail, setup_containers);
    }

    if let Some(kind) = kind {
        artifacts.retain(|artifact| artifact.kind == kind);
    }
    let mut plan = plan_managed(&artifacts, our_registry.as_deref(), policy);
    if plan.candidates.len() > remaining_objects {
        plan.deferred += plan.candidates.len() - remaining_objects;
        plan.candidates.truncate(remaining_objects);
        plan.bytes = plan
            .candidates
            .iter()
            .filter_map(|candidate| candidate.bytes)
            .sum();
    }
    let mut deletion_receipts = Vec::new();
    let mut removed = 0_u64;
    let mut removed_bytes = 0_i128;
    let mut failed = 0_u64;
    let mut failures = Vec::new();
    let mut budget_deferred = 0;
    let mut timed_out = None;

    if apply {
        for candidate in &plan.candidates {
            if let Err(reason) = budget::check() {
                budget_deferred += (plan.candidates.len() as u64)
                    .saturating_sub(removed + failed + budget_deferred);
                timed_out = Some(reason);
                break;
            }
            // Re-verify this exact object immediately before removing it. A container that
            // started while the pass was running must never be removed by a stale plan.
            let verified = match revalidate(
                engine,
                candidate,
                our_registry.as_deref(),
                policy,
                state_dir,
            ) {
                Ok(measured) => measured,
                Err(detail) => {
                    failed += 1;
                    details::push(&mut failures, detail);
                    continue;
                }
            };
            let measured = verified.bytes;
            if policy.max_bytes.is_some_and(|ceiling| {
                measured
                    .is_none_or(|bytes| bytes < 0 || removed_bytes.saturating_add(bytes) > ceiling)
            }) {
                budget_deferred += 1;
                continue;
            }
            match remove_owned(engine, candidate, verified.receipt.as_ref()) {
                Ok(()) => {
                    if let Some(receipt) = verified.receipt {
                        deletion_receipts.push(receipt);
                    }
                    removed += 1;
                    removed_bytes = removed_bytes.saturating_add(measured.unwrap_or(0));
                }
                Err(detail) => {
                    failed += 1;
                    details::push(&mut failures, detail);
                }
            }
        }
    }

    ManagedRetentionOutcome {
        deletion_receipts,
        summary: ManagedRetentionSummary {
            applied: apply,
            planned: plan.candidates.len() as u64,
            removed,
            removed_bytes,
            deferred: plan.deferred as u64 + budget_deferred,
            failed,
            failures,
            held_total: plan.held.len() as u64,
            held: held_details(&plan.held),
            refused: timed_out,
        },
        plan,
        setup_containers,
    }
}

fn refused_outcome(
    detail: String,
    setup_containers: SetupContainerReport,
) -> ManagedRetentionOutcome {
    ManagedRetentionOutcome {
        deletion_receipts: Vec::new(),
        summary: ManagedRetentionSummary {
            applied: false,
            planned: 0,
            removed: 0,
            removed_bytes: 0,
            deferred: 0,
            failed: 0,
            failures: Vec::new(),
            held: Vec::new(),
            held_total: 0,
            refused: Some(detail),
        },
        plan: bosn_core::retention::RetentionPlan::default(),
        setup_containers,
    }
}

fn held_details(verdicts: &[RetentionVerdict]) -> Vec<String> {
    let mut held = Vec::new();
    for verdict in verdicts.iter().take(64) {
        details::push(
            &mut held,
            format!(
                "{} {}: {}",
                verdict.kind.as_str(),
                verdict.id,
                verdict.hold.map_or("unknown", |reason| reason.as_str())
            ),
        );
    }
    held
}

/// An observation's liveness signal. Only `in_use` is ever set here: every kind's dangling and
/// anonymous flags belong to the unmanaged census, which reaches different conclusions.
fn signals(in_use: bool) -> bosn_core::Signals {
    bosn_core::Signals {
        in_use,
        dangling: false,
        anonymous: false,
    }
}

struct FreshRemovalObservation {
    engine_name: String,
    labels: BTreeMap<String, String>,
    age: Option<f64>,
    in_use: bool,
    bytes: Option<i128>,
}

fn inspect_removal_candidate(
    engine: &DockerEngine,
    candidate: &RetentionVerdict,
) -> Result<Option<FreshRemovalObservation>, String> {
    let options = budget::options(RunOptions::bounded(
        RETENTION_READ_DEADLINE,
        RETENTION_OUTPUT_LIMIT,
    ));
    let mut engine_name = candidate.id.clone();
    let (labels, age, in_use, bytes) = match candidate.kind {
        ResourceKind::Container => {
            let probe = vec![candidate.id.clone()];
            let Some(entries) = parse_read::<ContainerDetail>(
                engine.inspect_containers(&probe, budget::options(options)),
            ) else {
                return Err(format!("container {} could not be re-read", candidate.id));
            };
            let Some(entry) = entries.into_iter().next() else {
                // Already gone: the desired state is reached, not a failure. Nothing was
                // reclaimed, so nothing is accounted for either.
                return Ok(None);
            };
            engine_name = entry.name.trim_start_matches('/').to_owned();
            (
                verified_container_labels(&entry),
                entry.created_age(now_seconds()),
                entry.running(),
                entry.size_bytes(),
            )
        }
        ResourceKind::Volume => {
            let Some(entries) = parse_read::<VolumeDetail>(engine.inspect_volumes(
                std::slice::from_ref(&candidate.id),
                budget::options(options),
            )) else {
                return Err(format!("volume {} could not be re-read", candidate.id));
            };
            let Some(entry) = entries.into_iter().next() else {
                return Ok(None);
            };
            let in_use = !volume_is_unused(engine, &entry.name, budget::options(options));
            (
                entry.labels.clone(),
                entry.created_age(now_seconds()),
                in_use,
                entry.size_bytes(),
            )
        }
        ResourceKind::Image => {
            let Some(entries) = parse_read::<ImageDetail>(engine.inspect_images(
                std::slice::from_ref(&candidate.id),
                budget::options(options),
            )) else {
                return Err(format!("image {} could not be re-read", candidate.id));
            };
            let Some(entry) = entries.into_iter().next() else {
                return Ok(None);
            };
            let in_use = !image_is_unused(engine, &entry.id, budget::options(options));
            (
                entry.labels(),
                entry.created_age(now_seconds()),
                in_use,
                entry.size_bytes(),
            )
        }
        _ => return Err("unsupported kind for removal".to_owned()),
    };
    Ok(Some(FreshRemovalObservation {
        engine_name,
        labels,
        age,
        in_use,
        bytes,
    }))
}

/// Re-check one candidate immediately before its removal.
///
/// Returns the size measured by that same fresh inspection, or `None` when the engine did not
/// report one. Every field of the decision comes from this read: the plan's `bytes` is a
/// snapshot from the start of the pass and must never stand in for a value the engine did not
/// give us now, because the summary's `removed_bytes` is an account of what actually went away.
fn revalidate(
    engine: &DockerEngine,
    candidate: &RetentionVerdict,
    our_registry: Option<&str>,
    policy: RetentionPolicy,
    state_dir: &Path,
) -> Result<receipts::Revalidated, String> {
    let Some(FreshRemovalObservation {
        engine_name,
        mut labels,
        mut age,
        mut in_use,
        bytes,
    }) = inspect_removal_candidate(engine, candidate)?
    else {
        return Ok(receipts::Revalidated {
            bytes: None,
            receipt: None,
        });
    };
    let registered = registered::RegisteredOwnership::load(state_dir)?;
    registered.apply_usage(
        candidate.kind,
        &engine_name,
        &mut labels,
        &mut age,
        &mut in_use,
    );
    if candidate.kind == ResourceKind::Image
        && let Some((normalized, last_used, protected)) =
            registered.normalize_image(&candidate.id, &labels)
    {
        labels = normalized;
        in_use |= protected;
        age = age.map(|value| value.min((now_seconds() - last_used).max(0.0)));
    }
    let name = if candidate.kind == ResourceKind::Container {
        labels
            .get("com.zackees.bosn.setup-container")
            .map_or("", String::as_str)
    } else {
        &candidate.id
    };
    if let Some((normalized, last_used)) = registered.normalize(candidate.kind, name, &labels) {
        in_use |= registered.protected_name(name);
        labels = normalized;
        age = age.map(|value| value.min((now_seconds() - last_used).max(0.0)));
    }
    let Some(age) = age else {
        return Err(format!(
            "{} {} has no usable age",
            candidate.kind.as_str(),
            candidate.id
        ));
    };
    let fresh = bosn_core::ObservedArtifact {
        id: candidate.id.clone(),
        kind: candidate.kind,
        labels,
        signals: bosn_core::Signals {
            in_use,
            dangling: false,
            anonymous: false,
        },
        bytes,
        age_seconds: Some(age),
    };
    match classify_managed(&fresh, our_registry, policy).hold {
        None => Ok(receipts::Revalidated {
            bytes,
            receipt: bosn_core::ResourceLabels::parse(&fresh.labels)
                .ok()
                .map(|labels| receipts::DeletionReceipt {
                    state_dir: state_dir.to_path_buf(),
                    physical_id: candidate.id.clone(),
                    physical_name: engine_name.clone(),
                    labels: registered.receipt_labels(&engine_name, labels),
                    observed_at: now_seconds(),
                }),
        }),
        Some(reason) => Err(format!(
            "{} {} is no longer reclaimable: {}",
            candidate.kind.as_str(),
            candidate.id,
            reason.as_str()
        )),
    }
}

/// Remove exactly one owned object by its immutable identity.
///
/// A volume's identity *is* its name, since Docker exposes no separate volume id; the name is
/// what was proven by the label read and what is passed here. Nothing is removed by tag.
fn remove_owned(
    engine: &DockerEngine,
    candidate: &RetentionVerdict,
    receipt: Option<&DeletionReceipt>,
) -> Result<(), String> {
    budget::check()?;
    if let Some(receipt) = receipt {
        deletion_intents::record(receipt)
            .map_err(|error| format!("deletion intent could not commit: {error}"))?;
    }
    let options = budget::options(RunOptions::bounded(
        RETENTION_REMOVAL_DEADLINE,
        RETENTION_REMOVAL_OUTPUT_LIMIT,
    ));
    let argv: Vec<&str> = match candidate.kind {
        // No `-f`: a container can start between the re-check and this call, and forcing it
        // would kill live work this pass never looked at. A plain `rm` already removes an
        // already-stopped container, and refuses a running one, which the caller reports as a
        // failure and moves on from.
        ResourceKind::Container => vec!["rm", &candidate.id],
        ResourceKind::Volume => vec!["volume", "rm", &candidate.id],
        ResourceKind::Image => vec!["rmi", &candidate.id],
        _ => return Err("unsupported kind for removal".to_owned()),
    };
    let result = engine
        .with_args(argv)
        .capture(budget::options(options))
        .map_err(|error| {
            format!(
                "removing {} {}: {error}",
                candidate.kind.as_str(),
                candidate.id
            )
        })?;
    if result.ok() {
        Ok(())
    } else {
        Err(format!(
            "removing {} {}: {}",
            candidate.kind.as_str(),
            candidate.id,
            String::from_utf8_lossy(&result.stderr).trim()
        ))
    }
}

/// Whether no container references this volume.
///
/// A failed or empty-but-unanswered read returns `false`, so the volume is held rather than
/// removed: an unreadable answer is not an all-clear.
fn volume_is_unused(engine: &DockerEngine, name: &str, options: RunOptions) -> bool {
    if budget::check().is_err() {
        return false;
    }
    matches!(engine.container_ids_using_volume(name, budget::options(options)), Ok(ids) if ids.is_empty())
}

/// Whether no container was created from this image. Same fail-safe as the volume check.
fn image_is_unused(engine: &DockerEngine, id: &str, options: RunOptions) -> bool {
    if budget::check().is_err() {
        return false;
    }
    matches!(engine.container_ids_using_image(id, budget::options(options)), Ok(ids) if ids.is_empty())
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Split a newline-separated id/name read, recording a refusal rather than discarding it.
fn labeled_ids(
    result: Result<CensusRead, bosn_engine::CommandError>,
    what: &str,
    unreadable: &mut details::ReadFailures,
) -> Option<Vec<String>> {
    match result {
        Ok(CensusRead::Document(text)) => Some(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
        Ok(CensusRead::Unavailable { detail }) => {
            unreadable.push(detail);
            None
        }
        Err(error) => {
            unreadable.push(format!("{what} failed: {error}"));
            None
        }
    }
}

/// Parse an inspect document, recording a refusal rather than discarding it.
fn parse_inspect<T: serde::de::DeserializeOwned>(
    result: Result<CensusRead, bosn_engine::CommandError>,
    what: &str,
    unreadable: &mut details::ReadFailures,
) -> Option<Vec<T>> {
    match result {
        Ok(CensusRead::Document(text)) => match serde_json::from_str::<Vec<T>>(&text) {
            Ok(entries) => Some(entries),
            Err(_) => {
                unreadable.push(format!("{what} returned a document this build cannot read"));
                None
            }
        },
        Ok(CensusRead::Unavailable { detail }) => {
            unreadable.push(detail);
            None
        }
        Err(error) => {
            unreadable.push(format!("{what} failed: {error}"));
            None
        }
    }
}

/// Parse an inspect document for a single re-check, where there is no pass-wide list to poison.
fn parse_read<T: serde::de::DeserializeOwned>(
    result: Result<CensusRead, bosn_engine::CommandError>,
) -> Option<Vec<T>> {
    let text = match result {
        Ok(CensusRead::Document(text)) => text,
        _ => return None,
    };
    serde_json::from_str::<Vec<T>>(&text).ok()
}

/// Legacy ownership labels must name the exact Docker object being observed.
fn verified_container_labels(entry: &ContainerDetail) -> BTreeMap<String, String> {
    let mut labels = entry.labels();
    if !labels.contains_key(bosn_core::LABEL_KIND)
        && labels
            .get("com.zackees.bosn.setup-container")
            .is_some_and(|name| entry.name.strip_prefix('/') != Some(name.as_str()))
    {
        labels.remove("com.zackees.bosn.setup-managed");
    }
    labels
}

/// Docker reports `null` rather than `{}` for an unlabelled object, so this cannot be a map.
fn null_as_empty_map<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<BTreeMap<String, String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Parse Docker's RFC3339 timestamps to epoch seconds, reusing the census's parser.
fn parse_docker_time(raw: &str) -> Option<f64> {
    bosn_core::parse_rfc3339_timestamp(raw)
}

// ---------------------------------------------------------------------------
// Engine read shapes. Parsed eagerly into typed structs at the boundary;
// never probed as untyped JSON.
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct ContainerDetail {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Created")]
    created: String,
    #[serde(rename = "State", default)]
    state: Option<ContainerState>,
    #[serde(rename = "Mounts", default)]
    mounts: Vec<ContainerMount>,
    #[serde(rename = "Config", default)]
    config: Option<LabelledConfig>,
    /// `docker inspect` reports `SizeRw` only when asked; absent means unmeasured, which the
    /// policy then treats as unknown rather than zero.
    #[serde(rename = "SizeRw", default)]
    size_rw: Option<i128>,
}

#[derive(Debug, serde::Deserialize, Default)]
struct ContainerState {
    #[serde(rename = "Running", default)]
    running: bool,
}

/// One entry of a container's mount table.
///
/// Only named volume mounts count toward "pinned volumes": a bind mount is a path on a filesystem
/// Bosn does not own, and an anonymous volume's name is an opaque id that would only inflate the
/// count with something the operator cannot act on.
#[derive(Debug, serde::Deserialize)]
struct ContainerMount {
    #[serde(rename = "Type", default)]
    mount_type: String,
    #[serde(rename = "Name", default)]
    name: Option<String>,
}

const VOLUME_MOUNT_TYPE: &str = "volume";

#[derive(Debug, serde::Deserialize)]
struct LabelledConfig {
    #[serde(rename = "Cmd", default)]
    command: Vec<String>,
    #[serde(rename = "Labels", default, deserialize_with = "null_as_empty_map")]
    labels: std::collections::BTreeMap<String, String>,
}

impl ContainerDetail {
    fn id(&self) -> &str {
        &self.id
    }
    fn running(&self) -> bool {
        self.state.as_ref().is_some_and(|state| state.running)
    }
    fn labels(&self) -> std::collections::BTreeMap<String, String> {
        self.config
            .as_ref()
            .map(|config| config.labels.clone())
            .unwrap_or_default()
    }
    fn created_age(&self, now: f64) -> Option<f64> {
        parse_docker_time(&self.created).map(|created| (now - created).max(0.0))
    }
    fn size_bytes(&self) -> Option<i128> {
        self.size_rw
    }
    /// The named volumes this container mounts, sorted and deduplicated.
    ///
    /// The report counts what is *pinned*, so a container that mounts the same volume twice must
    /// not report it twice. A nameless volume mount is skipped rather than reported as an empty
    /// name: an unnameable volume is not something the report could describe.
    fn pinned_volume_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for mount in &self.mounts {
            if mount.mount_type != VOLUME_MOUNT_TYPE {
                continue;
            }
            if let Some(name) = mount.name.as_deref().filter(|name| !name.is_empty()) {
                names.insert(name.to_owned());
            }
        }
        names.into_iter().collect()
    }
}

#[derive(Debug, serde::Deserialize)]
struct VolumeDetail {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CreatedAt")]
    created_at: String,
    #[serde(rename = "Labels", default, deserialize_with = "null_as_empty_map")]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(rename = "UsageData", default)]
    usage: Option<VolumeUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct VolumeUsage {
    #[serde(rename = "Size", default)]
    size: Option<String>,
}

impl VolumeDetail {
    fn created_age(&self, now: f64) -> Option<f64> {
        parse_docker_time(&self.created_at).map(|created| (now - created).max(0.0))
    }
    fn size_bytes(&self) -> Option<i128> {
        self.usage
            .as_ref()
            .and_then(|usage| usage.size.as_deref())
            .and_then(bosn_core::parse_docker_size)
    }
}

#[derive(Debug, serde::Deserialize)]
struct ImageDetail {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "RepoDigests", default)]
    repo_digests: Option<Vec<String>>,
    #[serde(rename = "Created")]
    created: String,
    #[serde(rename = "Size", default)]
    size: Option<i128>,
    #[serde(rename = "Config", default)]
    config: Option<LabelledConfig>,
}

impl ImageDetail {
    fn labels(&self) -> std::collections::BTreeMap<String, String> {
        self.config
            .as_ref()
            .map(|config| config.labels.clone())
            .unwrap_or_default()
    }
    fn created_age(&self, now: f64) -> Option<f64> {
        parse_docker_time(&self.created).map(|created| (now - created).max(0.0))
    }
    fn size_bytes(&self) -> Option<i128> {
        self.size
    }
}
#[cfg(test)]
mod tests;
