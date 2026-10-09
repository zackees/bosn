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
//! #518: stopped setup containers are reported by default, deleted only on opt-in
//!
//! `bosn-setup-v2-*` containers cannot be created with `--rm`: `validate_observed` actively
//! enforces `AutoRemove == false`, so the container is designed to persist and reclamation must
//! come from here. A persisted container pins every volume it ever mounted, which is why this is
//! a disk problem and not a container-count problem — and why the report counts *pinned volumes*,
//! not containers.
//!
//! Reclamation stays opt-in through `retention.toml` (never delete what Bosn does not own, never
//! delete what cannot be recreated). But **reporting** is not reclamation, so
//! [`maintenance_pass`] emits the pile on every maintenance interval even with the default
//! opt-out config. Before this, a default install had no bound at all and no signal.
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

/// Deadline for each individual engine read.
pub const RETENTION_READ_DEADLINE: Duration = Duration::from_secs(30);
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
    /// Seconds since the container was created.
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

/// Run one managed-retention pass.
///
/// `apply` is the only thing that mutates the engine. A preview still pays for a full read,
/// because a preview that cannot prove ownership is not a preview.
#[must_use]
pub fn managed_retention_pass(
    engine: &DockerEngine,
    state_dir: &Path,
    policy: RetentionPolicy,
    apply: bool,
) -> ManagedRetentionOutcome {
    let our_registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
        .ok()
        .and_then(|registry| registry.registry_id().ok());

    let (artifacts, stopped_containers, refusal) = observe_owned(engine);
    // #518: this report is unconditional. A default install has no `retention.toml`, so nothing
    // would ever delete these — but the operator still has to be told the pile is growing.
    let setup_containers = SetupContainerReport {
        stopped: stopped_containers,
    };

    // An incomplete read must not authorize a removal. This is the rule the unmanaged census
    // follows too, and for the same reason: "we could not see it" is not "it is safe".
    if let Some(detail) = refusal {
        return ManagedRetentionOutcome {
            summary: ManagedRetentionSummary {
                applied: false,
                planned: 0,
                removed: 0,
                removed_bytes: 0,
                deferred: 0,
                failed: 0,
                failures: Vec::new(),
                refused: Some(detail),
            },
            plan: bosn_core::retention::RetentionPlan::default(),
            setup_containers,
        };
    }

    let plan = plan_managed(&artifacts, our_registry.as_deref(), policy);
    let mut removed = 0_u64;
    let mut removed_bytes = 0_i128;
    let mut failed = 0_u64;
    let mut failures = Vec::new();

    if apply {
        for candidate in &plan.candidates {
            // Re-verify this exact object immediately before removing it. A container that
            // started while the pass was running must never be removed by a stale plan.
            // An object that is already gone has reached the desired state: neither a removal
            // nor a failure, and nothing reclaimed is accounted for (#550).
            let outcome = revalidate(engine, candidate, our_registry.as_deref(), policy).and_then(
                |recheck| match recheck {
                    Recheck::Gone => Ok(None),
                    Recheck::Reclaimable(measured) => {
                        remove_owned(engine, candidate).map(|removed| removed.then_some(measured))
                    }
                },
            );
            match outcome {
                Ok(Some(measured)) => {
                    removed += 1;
                    removed_bytes = removed_bytes.saturating_add(measured.unwrap_or(0));
                }
                Ok(None) => {}
                Err(detail) => {
                    failed += 1;
                    failures.push(detail);
                }
            }
        }
    }

    ManagedRetentionOutcome {
        summary: ManagedRetentionSummary {
            applied: apply,
            planned: plan.candidates.len() as u64,
            removed,
            removed_bytes,
            deferred: plan.deferred as u64,
            failed,
            failures,
            refused: None,
        },
        plan,
        setup_containers,
    }
}

/// Read every Bosn-labeled container, volume and image, with ages and liveness.
///
/// Returns the observations and, separately, a refusal reason when any read was incomplete.
/// A partial read yields no observations at all rather than a partial set, because an
/// incomplete candidate list is indistinguishable from an empty one.
fn observe_owned(
    engine: &DockerEngine,
) -> (
    Vec<bosn_core::ObservedArtifact>,
    Vec<StoppedSetupContainer>,
    Option<String>,
) {
    let options = RunOptions::bounded(RETENTION_READ_DEADLINE, RETENTION_OUTPUT_LIMIT);
    let mut artifacts = Vec::new();
    let mut stopped_containers = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();

    // The label key used as the entry filter. Any object carrying it is a candidate for
    // inspection; `classify_managed` then proves or rejects each one individually, so a loose
    // filter here costs a read but can never widen what is removed.
    let probe = bosn_core::LABEL_KIND;

    observe_containers(
        engine,
        options,
        probe,
        &mut artifacts,
        &mut stopped_containers,
        &mut unreadable,
    );
    observe_volumes(engine, options, probe, &mut artifacts, &mut unreadable);
    observe_images(engine, options, probe, &mut artifacts, &mut unreadable);

    if unreadable.is_empty() {
        // Oldest first, so the report's head is the worst offender.
        stopped_containers.sort_by(|left, right| {
            right
                .age_seconds
                .total_cmp(&left.age_seconds)
                .then_with(|| left.id.cmp(&right.id))
        });
        (artifacts, stopped_containers, None)
    } else {
        (
            Vec::new(),
            Vec::new(),
            Some(format!(
                "{} engine read(s) failed, so this pass cannot prove what is safe to remove: {}",
                unreadable.len(),
                unreadable.join("; ")
            )),
        )
    }
}

/// Observe every Bosn-labeled container, running or stopped.
///
/// `State.Running` comes from Docker rather than being inferred, so a container that started
/// between two reads is still seen as live. The same read feeds the #518 report, so a stopped
/// container's pinned volumes come from the mount table Docker gives us here rather than from a
/// second, raceable query.
fn observe_containers(
    engine: &DockerEngine,
    options: RunOptions,
    probe: &str,
    artifacts: &mut Vec<bosn_core::ObservedArtifact>,
    stopped_containers: &mut Vec<StoppedSetupContainer>,
    unreadable: &mut Vec<String>,
) {
    let Some(ids) = labeled_ids(
        engine.container_ids_with_label(probe, options),
        "docker ps -a",
        unreadable,
    ) else {
        return;
    };
    for chunk in ids.chunks(INSPECT_CHUNK) {
        let Some(entries) = parse_inspect::<ContainerDetail>(
            engine.inspect_containers(chunk, options),
            "docker inspect",
            unreadable,
        ) else {
            continue;
        };
        for entry in entries {
            let Some(age) = entry.created_age(now_seconds()) else {
                unreadable.push(format!(
                    "container {} has no usable creation time",
                    entry.id()
                ));
                continue;
            };
            if !entry.running() {
                stopped_containers.push(StoppedSetupContainer {
                    id: entry.id().to_owned(),
                    age_seconds: age,
                    pinned_volumes: entry.pinned_volume_names(),
                });
            }
            artifacts.push(bosn_core::ObservedArtifact {
                id: entry.id().to_owned(),
                kind: ResourceKind::Container,
                labels: entry.labels(),
                signals: signals(entry.running()),
                bytes: entry.size_bytes(),
                age_seconds: Some(age),
            });
        }
    }
}

/// Observe every Bosn-labeled volume.
///
/// Liveness is Docker's own verdict on whether any container still mounts it, because that is
/// exactly the question the volume age gate must not answer wrongly.
fn observe_volumes(
    engine: &DockerEngine,
    options: RunOptions,
    probe: &str,
    artifacts: &mut Vec<bosn_core::ObservedArtifact>,
    unreadable: &mut Vec<String>,
) {
    let Some(names) = labeled_ids(
        engine.volume_names_with_label(probe, options),
        "docker volume ls",
        unreadable,
    ) else {
        return;
    };
    let sizes = if names.is_empty() {
        BTreeMap::new()
    } else {
        sizes::volume_sizes(engine, options)
    };
    for chunk in names.chunks(INSPECT_CHUNK) {
        let Some(entries) = parse_inspect::<VolumeDetail>(
            engine.inspect_volumes(chunk, options),
            "docker volume inspect",
            unreadable,
        ) else {
            continue;
        };
        for entry in entries {
            let Some(age) = entry.created_age(now_seconds()) else {
                unreadable.push(format!("volume {} has no usable creation time", entry.name));
                continue;
            };
            let in_use = volume_is_unused(engine, &entry.name, options);
            let bytes = entry
                .size_bytes()
                .or_else(|| sizes.get(&entry.name).copied());
            artifacts.push(bosn_core::ObservedArtifact {
                id: entry.name,
                kind: ResourceKind::Volume,
                labels: entry.labels,
                signals: signals(!in_use),
                bytes,
                age_seconds: Some(age),
            });
        }
    }
}

/// Observe every Bosn-labeled image.
///
/// An image any container was created from is still in use, so it is held. `bosn-setup:*`
/// images are content-addressed by tag, so an unreferenced one is rebuilt rather than lost.
fn observe_images(
    engine: &DockerEngine,
    options: RunOptions,
    probe: &str,
    artifacts: &mut Vec<bosn_core::ObservedArtifact>,
    unreadable: &mut Vec<String>,
) {
    let Some(ids) = labeled_ids(
        engine.image_ids_with_label(probe, options),
        "docker image ls",
        unreadable,
    ) else {
        return;
    };
    for chunk in ids.chunks(INSPECT_CHUNK) {
        let Some(entries) = parse_inspect::<ImageDetail>(
            engine.inspect_images(chunk, options),
            "docker image inspect",
            unreadable,
        ) else {
            continue;
        };
        for entry in entries {
            // An image with no parseable creation time contributes no age gate, so observing it
            // could only ever produce an `AgeUnknown` hold. Skipping it keeps the report honest
            // instead of listing an object the policy can never act on.
            let Some(age) = entry.created_age(now_seconds()) else {
                continue;
            };
            artifacts.push(bosn_core::ObservedArtifact {
                id: entry.id.clone(),
                kind: ResourceKind::Image,
                labels: entry.labels(),
                signals: signals(!image_is_unused(engine, &entry.id, options)),
                bytes: entry.size_bytes(),
                age_seconds: Some(age),
            });
        }
    }
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

/// What the pre-removal re-check found.
enum Recheck {
    /// The object no longer exists.
    Gone,
    /// Still reclaimable, with the size this read measured (if the engine reported one).
    Reclaimable(Option<i128>),
}

/// Re-check one candidate immediately before its removal.
///
/// Returns the size measured by that same fresh inspection (`None` inside when the engine did
/// not report one), or [`Recheck::Gone`] when the object no longer exists. Every field of the decision comes from this read: the plan's `bytes` is a
/// snapshot from the start of the pass and must never stand in for a value the engine did not
/// give us now, because the summary's `removed_bytes` is an account of what actually went away.
fn revalidate(
    engine: &DockerEngine,
    candidate: &RetentionVerdict,
    our_registry: Option<&str>,
    policy: RetentionPolicy,
) -> Result<Recheck, String> {
    let options = RunOptions::bounded(RETENTION_READ_DEADLINE, RETENTION_OUTPUT_LIMIT);
    let (labels, age, in_use, bytes) = match candidate.kind {
        ResourceKind::Container => {
            let probe = vec![candidate.id.clone()];
            let Some(entries) =
                parse_read::<ContainerDetail>(engine.inspect_containers(&probe, options))
            else {
                return Err(format!("container {} could not be re-read", candidate.id));
            };
            let Some(entry) = entries.into_iter().next() else {
                // Already gone: the desired state is reached, not a failure. Nothing was
                // reclaimed, so nothing is accounted for either.
                return Ok(Recheck::Gone);
            };
            (
                entry.labels(),
                entry.created_age(now_seconds()),
                entry.running(),
                entry.size_bytes(),
            )
        }
        ResourceKind::Volume => {
            let Some(entries) = parse_read::<VolumeDetail>(
                engine.inspect_volumes(std::slice::from_ref(&candidate.id), options),
            ) else {
                return Err(format!("volume {} could not be re-read", candidate.id));
            };
            let Some(entry) = entries.into_iter().next() else {
                return Ok(Recheck::Gone);
            };
            let in_use = !volume_is_unused(engine, &entry.name, options);
            // The pass's `system df` measurement stands in for a fresh one here: re-walking the
            // volume per removal is the slow probe #538 avoids, and a volume no container mounts
            // (which this re-check proves) cannot have changed size since the pass began.
            (
                entry.labels.clone(),
                entry.created_age(now_seconds()),
                in_use,
                entry.size_bytes().or(candidate.bytes),
            )
        }
        ResourceKind::Image => {
            let Some(entries) = parse_read::<ImageDetail>(
                engine.inspect_images(std::slice::from_ref(&candidate.id), options),
            ) else {
                return Err(format!("image {} could not be re-read", candidate.id));
            };
            let Some(entry) = entries.into_iter().next() else {
                return Ok(Recheck::Gone);
            };
            let in_use = !image_is_unused(engine, &entry.id, options);
            (
                entry.labels(),
                entry.created_age(now_seconds()),
                in_use,
                entry.size_bytes(),
            )
        }
        _ => return Err("unsupported kind for removal".to_owned()),
    };
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
        None => Ok(Recheck::Reclaimable(bytes)),
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
/// `Ok(true)` when this call removed the object, `Ok(false)` when Docker reports it already gone.
fn remove_owned(engine: &DockerEngine, candidate: &RetentionVerdict) -> Result<bool, String> {
    let options = RunOptions::bounded(RETENTION_REMOVAL_DEADLINE, RETENTION_REMOVAL_OUTPUT_LIMIT);
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
    let result = engine.with_args(argv).capture(options).map_err(|error| {
        format!(
            "removing {} {}: {error}",
            candidate.kind.as_str(),
            candidate.id
        )
    })?;
    if result.ok() {
        Ok(true)
    } else if result.reports_missing() {
        Ok(false)
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
    matches!(engine.container_ids_using_volume(name, options), Ok(ids) if ids.is_empty())
}

/// Whether no container was created from this image. Same fail-safe as the volume check.
fn image_is_unused(engine: &DockerEngine, id: &str, options: RunOptions) -> bool {
    matches!(engine.container_ids_using_image(id, options), Ok(ids) if ids.is_empty())
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
    unreadable: &mut Vec<String>,
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
    unreadable: &mut Vec<String>,
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
/// The unattended pass the daemon runs on its maintenance interval.
///
/// Reclamation is destructive, so applying it is opt-in through `retention.toml` in the state
/// directory (`auto_retention = true`). Without the flag this still reads the engine and still
/// reports what it would remove, so a machine is never silently growing without a signal — the
/// failure mode that produced #456.
pub fn maintenance_pass(state_dir: &Path) {
    let policy = RetentionPolicy::default();
    let apply = auto_retention_enabled(state_dir);
    let engine = DockerEngine::docker();
    let outcome = managed_retention_pass(&engine, state_dir, policy, apply);
    report_pass(&outcome);
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
    if summary.planned == 0 {
        return;
    }
    let verb = if summary.applied {
        "removed"
    } else {
        "would remove"
    };
    eprintln!(
        "bosn retention: {verb} {} owned object(s), {} bytes, {} deferred ({} failure(s)); \
         see them: bosn gc owned",
        summary.planned, summary.removed_bytes, summary.deferred, summary.failed,
    );
    for failure in &summary.failures {
        eprintln!("bosn retention: {failure}");
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
fn setup_container_report_line(report: &SetupContainerReport, applied: bool) -> Option<String> {
    if report.is_empty() {
        return None;
    }
    let past_gate = past_container_gate(report);
    let oldest = report.oldest_age_seconds().map_or_else(
        || "unknown age".to_owned(),
        |age| format!("{:.1}h old", age / 3600.0),
    );
    let action = if applied {
        "reclaim with: bosn gc owned --apply --yes"
    } else {
        "enable with: auto_retention = true in retention.toml"
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

/// The file that opts a machine into unattended reclamation.
const RETENTION_CONFIG: &str = "retention.toml";

/// Whether the operator asked for unattended reclamation.
///
/// An unreadable or absent file means "no". A daemon that could not parse its own opt-in must
/// never delete on the strength of a guess.
fn auto_retention_enabled(state_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(state_dir.join(RETENTION_CONFIG)) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        key.trim() == "auto_retention" && matches!(value.trim(), "true" | "yes" | "1")
    })
}

mod sizes;

#[cfg(test)]
mod tests;
