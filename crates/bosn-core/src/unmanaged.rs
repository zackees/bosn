//! Pure classification of Docker artifacts Bosn does not own.
//!
//! This is the census half of the workstream described by
//! [`docs/rust-unmanaged.md`](../../../docs/rust-unmanaged.md): it answers "what is on this
//! host that Bosn does not own, how big is it, and what may be reclaimed". It performs no
//! engine, filesystem, clock, or environment reads — callers pass `now` and the already
//! captured engine observation, matching the rest of this crate.
//!
//! Nothing here deletes anything, and nothing here decides to delete. The output is a
//! report, a per-artifact eligibility verdict, and the plan those verdicts imply. Running
//! that plan is a daemon-owned operation; it is not in this crate and not in this build.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{
    LABEL_REGISTRY, NAMESPACE, REQUIRED_LABELS, ResourceKind,
};

/// Default Tier-1 age gate. Matches `foreign_ttl` in #148.
pub const DEFAULT_TTL_SECONDS: f64 = 7.0 * 86_400.0;
/// Warn at all above this footprint, so a healthy machine stays silent.
pub const DEFAULT_WARN_BYTES: i128 = 5 * 1024 * 1024 * 1024;
/// The object-count half of the same threshold.
pub const DEFAULT_WARN_OBJECTS: u64 = 25;
/// An acknowledged footprint re-warns once it grows by this ratio.
pub const ACK_GROWTH_RATIO: f64 = 1.25;
/// ...or once this much time has passed, whichever comes first.
pub const ACK_MAX_AGE_SECONDS: f64 = 30.0 * 86_400.0;

/// When a footprint is worth mentioning.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WarningThreshold {
    pub bytes: i128,
    pub objects: u64,
}

impl Default for WarningThreshold {
    fn default() -> Self {
        Self {
            bytes: DEFAULT_WARN_BYTES,
            objects: DEFAULT_WARN_OBJECTS,
        }
    }
}

/// A footprint the user should be told about.
#[derive(Clone, Debug, PartialEq)]
pub struct Warning {
    pub reclaimable_objects: u64,
    pub reclaimable_bytes: i128,
    /// Tier-2 objects awaiting judgment.
    pub review_objects: u64,
    pub review_bytes: i128,
    /// Bytes the census can see but cannot remove by immutable ID.
    pub report_only_bytes: i128,
    /// The census could not be read completely. A partial census is never a clean machine.
    pub partial: bool,
}

/// Whether the census is worth a warning, and what to say.
///
/// A partial census always warns: silence would claim a cleanliness that was never
/// established. Otherwise the threshold decides, so a healthy machine says nothing.
#[must_use]
pub fn warning(census: &Census, threshold: WarningThreshold) -> Option<Warning> {
    let review_objects: u64 = census
        .classes
        .iter()
        .filter(|summary| summary.tier == Tier::Review)
        .map(|summary| summary.objects)
        .sum();
    let review_bytes: i128 = census
        .classes
        .iter()
        .filter(|summary| summary.tier == Tier::Review)
        .map(|summary| summary.bytes)
        .sum();
    let report_only_bytes: i128 = census
        .classes
        .iter()
        .filter(|summary| !is_removable_by_id(summary.class))
        .map(|summary| summary.eligible_bytes)
        .sum();
    let over = census.reclaimable_bytes >= threshold.bytes
        || census.reclaimable_objects >= threshold.objects;
    if !over && !census.partial {
        return None;
    }
    Some(Warning {
        reclaimable_objects: census.reclaimable_objects,
        reclaimable_bytes: census.reclaimable_bytes,
        review_objects,
        review_bytes,
        report_only_bytes,
        partial: census.partial,
    })
}

/// Whether a class can be removed by naming one immutable identity.
///
/// Build cache is the exception: `buildx` exposes no per-record removal, only an
/// age-filtered prune, and a filtered prune is not the same as deleting a proven, listed
/// object. It is reported so the warning is truthful, and never swept.
#[must_use]
pub fn is_removable_by_id(class: UnmanagedClass) -> bool {
    !matches!(class, UnmanagedClass::BuildCache)
}

/// A remembered acknowledgement of the current footprint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Acknowledgement {
    pub at: f64,
    pub objects: u64,
    pub bytes: i128,
}

/// Whether an acknowledgement still suppresses the warning.
#[must_use]
pub fn acknowledgement_suppresses(
    acknowledgement: Option<Acknowledgement>,
    census: &Census,
    now: f64,
) -> bool {
    let Some(acknowledgement) = acknowledgement else {
        return false;
    };
    if !now.is_finite() || !acknowledgement.at.is_finite() || now < acknowledgement.at {
        // An unreadable clock must not silently re-arm a warning the user acknowledged.
        return true;
    }
    if now - acknowledgement.at >= ACK_MAX_AGE_SECONDS {
        return false;
    }
    let byte_ceiling = (acknowledgement.bytes as f64 * ACK_GROWTH_RATIO) as i128;
    let object_ceiling = (acknowledgement.objects as f64 * ACK_GROWTH_RATIO) as u64;
    census.reclaimable_bytes <= byte_ceiling && census.reclaimable_objects <= object_ceiling
}

/// One artifact the plan would remove.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanCandidate {
    pub id: String,
    pub class: UnmanagedClass,
    pub bytes: i128,
}

/// What `gc --unmanaged` would do.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Tier-1 artifacts eligible for removal, in removal order.
    pub candidates: Vec<PlanCandidate>,
    /// Tier-2 artifacts awaiting an explicit `--include`.
    pub review: Vec<PlanCandidate>,
    /// Artifacts that are eligible but cannot be removed by identity.
    pub report_only: Vec<ClassSummary>,
    pub bytes: i128,
}

/// Build the plan for one observation.
///
/// Only Tier 1 is selected, and only artifacts the engine measured. `include` opts specific
/// Tier-2 artifacts in by identity; there is deliberately no way to select all of them.
#[must_use]
pub fn plan(
    artifacts: &[ObservedArtifact],
    our_registry: Option<&str>,
    config: CensusConfig,
    include: &[String],
) -> Plan {
    let mut candidates = Vec::new();
    let mut review = Vec::new();
    let mut report_only: BTreeMap<UnmanagedClass, ClassSummary> = BTreeMap::new();
    let mut bytes = 0i128;
    for artifact in artifacts {
        // A plan is never built from an artifact the engine did not measure.
        let Some(artifact_bytes) = artifact.bytes else {
            continue;
        };
        let verdict = classify(
            artifact.kind,
            &artifact.labels,
            our_registry,
            artifact.signals,
            artifact.age_seconds,
            config,
        );
        let Some(class) = verdict.class else {
            continue;
        };
        if verdict.protected.is_some() {
            continue;
        }
        if !verdict.age_eligible {
            continue;
        }
        if !is_removable_by_id(class) {
            let entry = report_only.entry(class).or_insert(ClassSummary {
                class,
                tier: class.tier(),
                objects: 0,
                bytes: 0,
                eligible_objects: 0,
                eligible_bytes: 0,
                oldest_age_seconds: None,
            });
            entry.objects += 1;
            entry.bytes += artifact_bytes;
            entry.eligible_objects += 1;
            entry.eligible_bytes += artifact_bytes;
            continue;
        }
        let candidate = PlanCandidate {
            id: artifact.id.clone(),
            class,
            bytes: artifact_bytes,
        };
        match class.tier() {
            Tier::Reclaimable => {
                bytes += artifact_bytes;
                candidates.push(candidate);
            }
            Tier::Review => {
                if include.iter().any(|id| id == &artifact.id) {
                    bytes += artifact_bytes;
                    candidates.push(candidate);
                } else {
                    review.push(candidate);
                }
            }
        }
    }
    candidates.sort_by_key(|candidate| removal_rank(candidate.class));
    Plan {
        candidates,
        review,
        report_only: report_only.into_values().collect(),
        bytes,
    }
}

/// Removal order: containers first, because removing them is what releases the image
/// references blocking image deletion. Mirrors `collectable_ordered`'s intent for owned
/// resources.
#[must_use]
pub fn removal_rank(class: UnmanagedClass) -> u8 {
    match class {
        UnmanagedClass::StoppedContainer => 0,
        UnmanagedClass::UnreferencedImage | UnmanagedClass::DanglingImage => 1,
        UnmanagedClass::AnonymousVolume | UnmanagedClass::NamedVolume => 2,
        UnmanagedClass::BuildCache => 3,
    }
}

/// How much of this artifact Bosn owns, per the label contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnershipClass {
    /// Complete label set naming this registry. Excluded from the census.
    Ours,
    /// Complete label set naming a different registry UUID.
    ForeignRegistry,
    /// Some Bosn labels, but not the complete required set.
    IncompleteLabels,
    /// No Bosn label at all.
    Unlabeled,
}

/// What an unowned artifact is, and therefore how it may be treated.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum UnmanagedClass {
    // Tier 1 — reclaimable.
    DanglingImage,
    StoppedContainer,
    AnonymousVolume,
    BuildCache,
    // Tier 2 — never swept; `--include` only.
    UnreferencedImage,
    NamedVolume,
}

impl UnmanagedClass {
    #[must_use]
    pub fn tier(self) -> Tier {
        match self {
            Self::DanglingImage
            | Self::StoppedContainer
            | Self::AnonymousVolume
            | Self::BuildCache => Tier::Reclaimable,
            Self::UnreferencedImage | Self::NamedVolume => Tier::Review,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DanglingImage => "dangling-image",
            Self::StoppedContainer => "stopped-container",
            Self::AnonymousVolume => "anonymous-volume",
            Self::BuildCache => "build-cache",
            Self::UnreferencedImage => "unreferenced-image",
            Self::NamedVolume => "named-volume",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    /// Safe to remove with a documented command.
    Reclaimable,
    /// Needs a human decision; enters a plan only when named explicitly.
    Review,
}

/// Why an artifact is excluded from reclamation.
///
/// Every one of these is a *keep*. Missing information is never a reason to reclaim.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ProtectedReason {
    /// Owned by this registry. Owned lifecycle has its own GC.
    OwnedByThisRegistry,
    /// Complete labels naming another registry.
    ForeignRegistry,
    /// Incomplete label set: ownership cannot be proven either way.
    IncompleteLabels,
    /// Attached to, or is, a running or in-use resource.
    InUse,
    /// Size could not be measured. Fail closed.
    Unmeasured,
    /// No classification rule applies to this shape.
    Unclassified,
}

impl ProtectedReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnedByThisRegistry => "owned-by-this-registry",
            Self::ForeignRegistry => "foreign-registry",
            Self::IncompleteLabels => "incomplete-labels",
            Self::InUse => "in-use",
            Self::Unmeasured => "unmeasured",
            Self::Unclassified => "unclassified",
        }
    }
}

/// Engine-observed facts about one artifact, beyond its labels and age.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Signals {
    /// Referenced by, or itself, a live/attached consumer.
    pub in_use: bool,
    /// Image: no repository and no tag.
    pub dangling: bool,
    /// Volume: Docker marked it anonymous.
    pub anonymous: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CensusConfig {
    pub ttl_seconds: f64,
}

impl Default for CensusConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: DEFAULT_TTL_SECONDS,
        }
    }
}

/// The verdict for one artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Classification {
    pub ownership: OwnershipClass,
    pub class: Option<UnmanagedClass>,
    pub protected: Option<ProtectedReason>,
    /// Whether the artifact is past its age gate.
    ///
    /// This is deliberately independent of tier: a Tier-2 artifact past its gate is still
    /// never swept on its own, but it may be named explicitly with `--include`. Sweepability
    /// is a property of the tier, not of the gate.
    pub age_eligible: bool,
}

/// Classify one observed artifact.
///
/// The order is deliberate: ownership and retention protect before any reclaim rule runs,
/// and unknown age fails closed.
#[must_use]
pub fn classify(
    kind: ResourceKind,
    labels: &BTreeMap<String, String>,
    our_registry: Option<&str>,
    signals: Signals,
    age_seconds: Option<f64>,
    config: CensusConfig,
) -> Classification {
    let ownership = classify_ownership(labels, our_registry);
    let base = Classification {
        ownership,
        class: None,
        protected: None,
        age_eligible: false,
    };
    let protect = |reason| Classification {
        protected: Some(reason),
        ..base
    };
    match ownership {
        OwnershipClass::Ours => return protect(ProtectedReason::OwnedByThisRegistry),
        OwnershipClass::ForeignRegistry => return protect(ProtectedReason::ForeignRegistry),
        OwnershipClass::IncompleteLabels => return protect(ProtectedReason::IncompleteLabels),
        OwnershipClass::Unlabeled => {}
    }
    // Pinning needs no branch here. A pin is a label, so a pinned artifact necessarily
    // carries Bosn labels and has already resolved to `Ours`, `ForeignRegistry`, or
    // `IncompleteLabels` above — and every one of those is a protected class whose own
    // lifecycle honours retention. Restating the check here would be unreachable code.
    if signals.in_use {
        return protect(ProtectedReason::InUse);
    }
    let Some(age) = age_seconds else {
        return protect(ProtectedReason::Unmeasured);
    };
    let (class, gate) = match kind {
        ResourceKind::Image => {
            // Only Docker's own dangling verdict is safe to reclaim unattended. A tagged,
            // unreferenced image is reviewable, never sweepable: whether it still exists in
            // a registry cannot be proven from the engine's accounting data, so removing it
            // could destroy the only copy.
            let class = if signals.dangling {
                UnmanagedClass::DanglingImage
            } else {
                UnmanagedClass::UnreferencedImage
            };
            (class, config.ttl_seconds)
        }
        ResourceKind::Container => (UnmanagedClass::StoppedContainer, config.ttl_seconds),
        ResourceKind::Volume => {
            if signals.anonymous {
                (UnmanagedClass::AnonymousVolume, config.ttl_seconds)
            } else {
                (UnmanagedClass::NamedVolume, config.ttl_seconds)
            }
        }
        ResourceKind::Builder => (UnmanagedClass::BuildCache, config.ttl_seconds),
        // The engine's accounting document reports no networks, so a network artifact has
        // neither a byte total nor an age. It is protected as unclassified, never guessed at.
        ResourceKind::Network => return protect(ProtectedReason::Unclassified),
    };
    Classification {
        class: Some(class),
        age_eligible: age >= gate,
        ..base
    }
}

/// Classify ownership from a label map.
///
/// Any key in the Bosn namespace counts as "a Bosn label is present". An artifact with some
/// but not all of the required keys is *incomplete*, which protects it: ownership cannot be
/// proven, and a name never proves ownership.
#[must_use]
pub fn classify_ownership(
    labels: &BTreeMap<String, String>,
    our_registry: Option<&str>,
) -> OwnershipClass {
    if !labels.keys().any(|key| is_bosn_label(key)) {
        return OwnershipClass::Unlabeled;
    }
    if !REQUIRED_LABELS.iter().all(|key| labels.contains_key(*key)) {
        return OwnershipClass::IncompleteLabels;
    }
    match (labels.get(LABEL_REGISTRY), our_registry) {
        (Some(registry), Some(ours)) if registry == ours => OwnershipClass::Ours,
        _ => OwnershipClass::ForeignRegistry,
    }
}

#[must_use]
pub fn is_bosn_label(key: &str) -> bool {
    key == NAMESPACE || key.strip_prefix(NAMESPACE).is_some_and(|rest| rest.starts_with('.'))
}

/// One artifact's engine observations, ready to classify.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedArtifact {
    pub id: String,
    pub kind: ResourceKind,
    pub labels: BTreeMap<String, String>,
    pub signals: Signals,
    /// Size in bytes, or `None` when the engine did not report a parseable size.
    pub bytes: Option<i128>,
    pub age_seconds: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClassSummary {
    pub class: UnmanagedClass,
    pub tier: Tier,
    pub objects: u64,
    pub bytes: i128,
    /// Objects past their age gate.
    pub eligible_objects: u64,
    pub eligible_bytes: i128,
    pub oldest_age_seconds: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProtectedSummary {
    pub reason: ProtectedReason,
    pub objects: u64,
    pub bytes: i128,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Census {
    pub classes: Vec<ClassSummary>,
    pub protected: Vec<ProtectedSummary>,
    /// Objects in Tier 1 that are past their gate.
    pub reclaimable_objects: u64,
    pub reclaimable_bytes: i128,
    /// Set when any input was unmeasurable or unparseable. A partial census is never a
    /// clean machine, and no plan may be built from it.
    pub partial: bool,
    /// Docker reports human-unit sizes with ~4 significant digits. Every byte figure in this
    /// census is derived from those and is therefore approximate.
    pub bytes_approximate: bool,
}

/// Aggregate a full observation into per-class and per-reason summaries.
#[must_use]
pub fn census(artifacts: &[ObservedArtifact], our_registry: Option<&str>, config: CensusConfig) -> Census {
    let mut classes: BTreeMap<UnmanagedClass, ClassSummary> = BTreeMap::new();
    let mut protected: BTreeMap<ProtectedReason, ProtectedSummary> = BTreeMap::new();
    let mut partial = false;
    for artifact in artifacts {
        let verdict = classify(
            artifact.kind,
            &artifact.labels,
            our_registry,
            artifact.signals,
            artifact.age_seconds,
            config,
        );
        let bytes = artifact.bytes.unwrap_or(0);
        if artifact.bytes.is_none() {
            partial = true;
        }
        if let Some(reason) = verdict.protected {
            let entry = protected.entry(reason).or_insert(ProtectedSummary {
                reason,
                objects: 0,
                bytes: 0,
            });
            entry.objects += 1;
            entry.bytes += bytes;
            continue;
        }
        let Some(class) = verdict.class else {
            partial = true;
            continue;
        };
        let entry = classes.entry(class).or_insert(ClassSummary {
            class,
            tier: class.tier(),
            objects: 0,
            bytes: 0,
            eligible_objects: 0,
            eligible_bytes: 0,
            oldest_age_seconds: None,
        });
        entry.objects += 1;
        entry.bytes += bytes;
        if let Some(age) = artifact.age_seconds {
            entry.oldest_age_seconds = Some(match entry.oldest_age_seconds {
                Some(current) if current >= age => current,
                _ => age,
            });
        }
        // Only a Tier-1 class can be swept, so only a Tier-1 class contributes to the
        // reclaimable totals the warning and the threshold are built on.
        if verdict.age_eligible && class.tier() == Tier::Reclaimable {
            entry.eligible_objects += 1;
            entry.eligible_bytes += bytes;
        }
    }
    let reclaimable_classes: Vec<ClassSummary> = classes.values().copied().collect();
    // "Reclaimable" means a command can actually take it. Build cache is Tier 1 but has no
    // per-object removal, so counting it here would promise bytes no command can free; it is
    // reported separately instead.
    let countable = |summary: &&ClassSummary| {
        summary.tier == Tier::Reclaimable && is_removable_by_id(summary.class)
    };
    let reclaimable_objects = reclaimable_classes
        .iter()
        .filter(countable)
        .map(|summary| summary.eligible_objects)
        .sum();
    let reclaimable_bytes = reclaimable_classes
        .iter()
        .filter(countable)
        .map(|summary| summary.eligible_bytes)
        .sum();
    Census {
        classes: reclaimable_classes,
        protected: protected.values().copied().collect(),
        reclaimable_objects,
        reclaimable_bytes,
        partial,
        bytes_approximate: true,
    }
}

/// Parse Docker's comma-joined `Labels` field.
///
/// Docker joins `key=value` pairs with a comma and does not escape a comma inside a value, so
/// a value containing a comma is unresolvable here. That limitation is why every consequence
/// of a missing or malformed label is "keep".
#[must_use]
pub fn parse_label_list(raw: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match entry.split_once('=') {
            Some((key, value)) => {
                labels.insert(key.trim().to_owned(), value.to_owned());
            }
            None => {
                labels.insert(entry.to_owned(), String::new());
            }
        }
    }
    labels
}

/// Parse a Docker human-unit size into bytes.
///
/// Docker uses decimal units (`kB`, `MB`, `GB`, …) in its accounting output and appends `*`
/// to approximate buildx figures. Anything unrecognised, including `N/A`, is `None` so the
/// caller can fail closed rather than treat it as zero.
#[must_use]
pub fn parse_docker_size(raw: &str) -> Option<i128> {
    let raw = raw.trim().trim_end_matches('*').trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("n/a") {
        return None;
    }
    let split = raw
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    if number.is_empty() {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let multiplier: f64 = match unit.trim() {
        "" | "B" => 1.0,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "PB" => 1e15,
        "KiB" => 1024.0,
        "MiB" => 1024.0f64.powi(2),
        "GiB" => 1024.0f64.powi(3),
        "TiB" => 1024.0f64.powi(4),
        "PiB" => 1024.0f64.powi(5),
        _ => return None,
    };
    let bytes = value * multiplier;
    if !bytes.is_finite() || bytes > i128::MAX as f64 {
        return None;
    }
    Some(bytes.round() as i128)
}

/// Parse the timestamp formats Docker's accounting output uses, as Unix seconds.
///
/// Observed shapes are `YYYY-MM-DD HH:MM:SS ±HHMM TZ` (images, containers) and
/// `YYYY-MM-DD HH:MM:SS[.fraction] ±HHMM TZ` (build cache). The numeric offset is used
/// directly, so no timezone database is involved. The trailing zone abbreviation is ignored.
#[must_use]
pub fn parse_docker_timestamp(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    let (date, rest) = raw.split_once(' ')?;
    let (time, rest) = rest.trim().split_once(' ')?;
    let (year, month, day) = {
        let mut parts = date.split('-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: i64 = parts.next()?.parse().ok()?;
        let day: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        (year, month, day)
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (time, None),
    };
    let (hour, minute, second) = {
        let mut parts = clock.split(':');
        let hour: i64 = parts.next()?.parse().ok()?;
        let minute: i64 = parts.next()?.parse().ok()?;
        let second: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        (hour, minute, second)
    };
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    let sub_second = match fraction {
        Some(fraction) => {
            if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let digits: String = fraction.chars().take(9).collect();
            let scale = 10f64.powi(i32::try_from(digits.len()).ok()?);
            digits.parse::<f64>().ok()? / scale
        }
        None => 0.0,
    };
    // The numeric offset is the first token after the time; a missing offset is unknown, and
    // unknown resolves to no age, which protects.
    let offset_token = rest.trim().split(' ').next()?;
    let offset_seconds = parse_offset(offset_token)?;
    let days = days_from_civil(year, month, day);
    let seconds = days as f64 * 86_400.0
        + (hour * 3600 + minute * 60 + second) as f64
        + sub_second
        - offset_seconds as f64;
    seconds.is_finite().then_some(seconds)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parse the RFC 3339 form `docker inspect` uses, as Unix seconds.
///
/// `docker volume inspect` and `docker network inspect` report `2026-09-13T18:41:43-07:00`,
/// with a `T` separator and a colon in the offset. Both are normalised to the shape
/// [`parse_docker_timestamp`] already handles, so one numeric-offset path covers every
/// timestamp this crate reads.
#[must_use]
pub fn parse_rfc3339_timestamp(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    // Only a `T` in the date/time separator position marks RFC 3339. A zone abbreviation such
    // as `UTC` also contains a `T`, and mistaking that for the separator would split the
    // string mid-word.
    if trimmed.as_bytes().get(10) != Some(&b'T') {
        return parse_docker_timestamp(trimmed);
    }
    let (Some(date), Some(rest)) = (trimmed.get(..10), trimmed.get(11..)) else {
        return None;
    };
    if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        return parse_docker_timestamp(&format!("{date} {clock} +0000"));
    }
    // The offset is the last sign in the remainder; there is no space before it, unlike the
    // accounting format, so one is inserted here.
    let split = rest.rfind(['+', '-'])?;
    let (clock, offset) = rest.split_at(split);
    parse_docker_timestamp(&format!("{date} {clock} {offset}"))
}

/// Parse `+HHMM` / `-HHMM` (also accepting `+HH:MM`) into seconds east of UTC.
fn parse_offset(raw: &str) -> Option<i64> {
    let (sign, digits) = match raw.as_bytes().first()? {
        b'+' => (1i64, &raw[1..]),
        b'-' => (-1i64, &raw[1..]),
        _ => return None,
    };
    let digits: String = digits.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

/// One section of `docker system df -v --format json`.
#[derive(Debug, Deserialize)]
pub struct SystemDfReport {
    #[serde(rename = "Images", default)]
    pub images: Vec<DfImage>,
    #[serde(rename = "Containers", default)]
    pub containers: Vec<DfContainer>,
    #[serde(rename = "Volumes", default)]
    pub volumes: Vec<DfVolume>,
    #[serde(rename = "BuildCache", default)]
    pub build_cache: Vec<DfBuildCache>,
}

#[derive(Debug, Deserialize)]
pub struct DfImage {
    #[serde(rename = "ID", default)]
    pub id: String,
    #[serde(rename = "Repository", default)]
    pub repository: String,
    #[serde(rename = "Tag", default)]
    pub tag: String,
    #[serde(rename = "Digest", default)]
    pub digest: String,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    #[serde(rename = "Size", default)]
    pub size: String,
    #[serde(rename = "Containers", default)]
    pub containers: String,
}

#[derive(Debug, Deserialize)]
pub struct DfContainer {
    #[serde(rename = "ID", default)]
    pub id: String,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    #[serde(rename = "Labels", default)]
    pub labels: String,
    #[serde(rename = "Size", default)]
    pub size: String,
    #[serde(rename = "State", default)]
    pub state: String,
}

#[derive(Debug, Deserialize)]
pub struct DfVolume {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Labels", default)]
    pub labels: String,
    #[serde(rename = "Links", default)]
    pub links: String,
    #[serde(rename = "Size", default)]
    pub size: String,
}

#[derive(Debug, Deserialize)]
pub struct DfBuildCache {
    #[serde(rename = "ID", default)]
    pub id: String,
    #[serde(rename = "Size", default)]
    pub size: String,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    #[serde(rename = "InUse", default)]
    pub in_use: String,
}

/// One volume's detail from `docker volume inspect`.
///
/// The accounting document reports no volume creation time, so this is the only source of
/// volume age. Labels arrive as a map here rather than as the joined string the accounting
/// document uses.
#[derive(Debug, Deserialize)]
pub struct InspectedVolume {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    /// `docker inspect` reports `null` rather than `{}` for an unlabelled object, so this
    /// cannot be a bare map.
    #[serde(rename = "Labels", default, deserialize_with = "null_as_empty_map")]
    pub labels: BTreeMap<String, String>,
}

fn null_as_empty_map<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<BTreeMap<String, String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Everything one engine read pass produced.
#[derive(Debug)]
pub struct EngineObservation<'a> {
    pub report: &'a SystemDfReport,
    /// Image IDs Docker itself reports as dangling.
    pub dangling_image_ids: &'a [String],
    /// Image IDs carrying any Bosn label.
    pub bosn_labeled_image_ids: &'a [String],
    /// Volume detail. A volume absent here has no known age and is protected accordingly.
    pub inspected_volumes: &'a [InspectedVolume],
    pub now: f64,
}

/// Parse one engine read pass into observations.
///
/// Image ownership cannot be read from the accounting document, because `docker image ls`
/// does not expose labels, so the caller supplies the ID sets it gathered with the engine's
/// label and dangling filters.
#[must_use]
pub fn observe(input: EngineObservation<'_>) -> Vec<ObservedArtifact> {
    let report = input.report;
    let now = input.now;
    let labeled: std::collections::BTreeSet<&str> = input
        .bosn_labeled_image_ids
        .iter()
        .map(String::as_str)
        .collect();
    let dangling_ids: std::collections::BTreeSet<&str> = input
        .dangling_image_ids
        .iter()
        .map(String::as_str)
        .collect();
    let inspected: BTreeMap<&str, &InspectedVolume> = input
        .inspected_volumes
        .iter()
        .map(|volume| (volume.name.as_str(), volume))
        .collect();
    let mut observed = Vec::new();
    for image in &report.images {
        let age = parse_docker_timestamp(&image.created_at).map(|at| now - at);
        let in_use = parse_count(&image.containers).unwrap_or(1) > 0;
        // Docker's own filter is authoritative: an untagged image that is still the parent
        // of a tagged one is not dangling, and must not be offered as reclaimable.
        let dangling = dangling_ids.contains(image.id.as_str());
        let labels = if labeled.contains(image.id.as_str()) {
            // The engine proved this image carries a Bosn label but cannot cheaply prove
            // *which*; an incomplete map resolves to the protective `IncompleteLabels`.
            BTreeMap::from([(LABEL_REGISTRY.to_owned(), String::new())])
        } else {
            BTreeMap::new()
        };
        observed.push(ObservedArtifact {
            id: image.id.clone(),
            kind: ResourceKind::Image,
            labels,
            signals: Signals {
                in_use,
                dangling,
                anonymous: false,
            },
            bytes: parse_docker_size(&image.size),
            age_seconds: age,
        });
    }
    for container in &report.containers {
        let age = parse_docker_timestamp(&container.created_at).map(|at| now - at);
        let labels = parse_label_list(&container.labels);
        observed.push(ObservedArtifact {
            id: container.id.clone(),
            kind: ResourceKind::Container,
            labels,
            signals: Signals {
                in_use: container.state != "exited" && container.state != "created",
                dangling: false,
                anonymous: false,
            },
            bytes: parse_docker_size(&container.size),
            age_seconds: age,
        });
    }
    for volume in &report.volumes {
        let detail = inspected.get(volume.name.as_str());
        // Prefer the inspected label map when it is available; the accounting document's
        // joined string cannot represent a label value containing a comma.
        let labels = match detail {
            Some(detail) if !detail.labels.is_empty() => detail.labels.clone(),
            _ => parse_label_list(&volume.labels),
        };
        let anonymous = labels.contains_key("com.docker.volume.anonymous")
            || is_anonymous_volume_name(&volume.name);
        observed.push(ObservedArtifact {
            id: volume.name.clone(),
            kind: ResourceKind::Volume,
            labels,
            signals: Signals {
                in_use: parse_count(&volume.links).unwrap_or(1) > 0,
                dangling: false,
                anonymous,
            },
            bytes: parse_docker_size(&volume.size),
            // Without inspect detail there is no creation time, so the volume is protected
            // as unmeasured rather than assumed old.
            age_seconds: detail
                .and_then(|detail| parse_rfc3339_timestamp(&detail.created_at))
                .map(|at| now - at),
        });
    }
    for entry in &report.build_cache {
        let age = parse_docker_timestamp(&entry.created_at).map(|at| now - at);
        observed.push(ObservedArtifact {
            id: entry.id.clone(),
            kind: ResourceKind::Builder,
            labels: BTreeMap::new(),
            signals: Signals {
                in_use: entry.in_use.eq_ignore_ascii_case("true"),
                dangling: false,
                anonymous: false,
            },
            bytes: parse_docker_size(&entry.size),
            age_seconds: age,
        });
    }
    observed
}

fn is_anonymous_volume_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_count(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn complete(registry: &str) -> BTreeMap<String, String> {
        labels(&[
            (LABEL_REGISTRY, registry),
            (crate::LABEL_KIND, "image"),
            (crate::LABEL_STACK, "app"),
            (crate::LABEL_GENERATION, "sha256:abc"),
            (crate::LABEL_SCOPE, "stack"),
            (crate::LABEL_WORKSPACE, "/w"),
            (crate::LABEL_CREATED, "1"),
        ])
    }

    #[test]
    fn ownership_requires_the_complete_label_set() {
        assert_eq!(
            classify_ownership(&complete("r1"), Some("r1")),
            OwnershipClass::Ours
        );
        assert_eq!(
            classify_ownership(&complete("r1"), Some("r2")),
            OwnershipClass::ForeignRegistry
        );
        assert_eq!(
            classify_ownership(&labels(&[(crate::LABEL_KIND, "image")]), Some("r1")),
            OwnershipClass::IncompleteLabels
        );
        assert_eq!(classify_ownership(&BTreeMap::new(), Some("r1")), OwnershipClass::Unlabeled);
    }

    #[test]
    fn one_missing_required_label_is_incomplete_not_unlabeled() {
        let mut partial = complete("r1");
        partial.remove(crate::LABEL_CREATED);
        assert_eq!(
            classify_ownership(&partial, Some("r1")),
            OwnershipClass::IncompleteLabels
        );
    }

    #[test]
    fn names_never_prove_ownership() {
        // A bosn-looking name with no labels is still unlabeled.
        assert_eq!(classify_ownership(&BTreeMap::new(), Some("r1")), OwnershipClass::Unlabeled);
    }

    #[test]
    fn classifier_selects_the_expected_class_per_kind() {
        let config = CensusConfig::default();
        let old = Some(DEFAULT_TTL_SECONDS + 1.0);

        let dangling = classify(
            ResourceKind::Image,
            &BTreeMap::new(),
            None,
            Signals {
                dangling: true,
                ..Signals::default()
            },
            old,
            config,
        );
        assert_eq!(dangling.class, Some(UnmanagedClass::DanglingImage));
        assert!(dangling.age_eligible);

        // A tagged, unreferenced image is reviewable but never sweepable: its remote
        // existence cannot be proven from the engine's accounting data.
        let tagged = classify(
            ResourceKind::Image,
            &BTreeMap::new(),
            None,
            Signals::default(),
            old,
            config,
        );
        assert_eq!(tagged.class, Some(UnmanagedClass::UnreferencedImage));
        assert_eq!(tagged.class.unwrap().tier(), Tier::Review);
        // Past its gate, but a review-tier class is still never swept on its own.
        assert!(tagged.age_eligible);

        let container = classify(
            ResourceKind::Container,
            &BTreeMap::new(),
            None,
            Signals::default(),
            old,
            config,
        );
        assert_eq!(container.class, Some(UnmanagedClass::StoppedContainer));

        let anon_volume = classify(
            ResourceKind::Volume,
            &BTreeMap::new(),
            None,
            Signals {
                anonymous: true,
                ..Signals::default()
            },
            old,
            config,
        );
        assert_eq!(anon_volume.class, Some(UnmanagedClass::AnonymousVolume));

        let named_volume = classify(
            ResourceKind::Volume,
            &BTreeMap::new(),
            None,
            Signals::default(),
            old,
            config,
        );
        assert_eq!(named_volume.class, Some(UnmanagedClass::NamedVolume));

        let cache = classify(
            ResourceKind::Builder,
            &BTreeMap::new(),
            None,
            Signals::default(),
            old,
            config,
        );
        assert_eq!(cache.class, Some(UnmanagedClass::BuildCache));
    }

    #[test]
    fn protection_wins_over_reclaimability() {
        let config = CensusConfig::default();
        let old = Some(DEFAULT_TTL_SECONDS * 10.0);
        let ours = classify(
            ResourceKind::Image,
            &complete("r1"),
            Some("r1"),
            Signals {
                dangling: true,
                ..Signals::default()
            },
            old,
            config,
        );
        assert_eq!(ours.protected, Some(ProtectedReason::OwnedByThisRegistry));

        let foreign = classify(
            ResourceKind::Image,
            &complete("other"),
            Some("r1"),
            Signals::default(),
            old,
            config,
        );
        assert_eq!(foreign.protected, Some(ProtectedReason::ForeignRegistry));

        let incomplete = classify(
            ResourceKind::Image,
            &labels(&[(crate::LABEL_SCOPE, "stack")]),
            Some("r1"),
            Signals::default(),
            old,
            config,
        );
        assert_eq!(incomplete.protected, Some(ProtectedReason::IncompleteLabels));

        // A pin is a label, so it cannot appear on an unlabeled artifact. A half-written
        // label set containing only a pin is therefore incomplete, and protected as such.
        // Pinning is honoured transitively: every pinned artifact lands in a protected
        // ownership class before any reclaim rule runs.
        let pinned_but_incomplete = classify(
            ResourceKind::Volume,
            &labels(&[(crate::LABEL_RETENTION, "pinned")]),
            None,
            Signals {
                anonymous: true,
                ..Signals::default()
            },
            old,
            config,
        );
        assert_eq!(
            pinned_but_incomplete.protected,
            Some(ProtectedReason::IncompleteLabels)
        );

        let in_use = classify(
            ResourceKind::Container,
            &BTreeMap::new(),
            None,
            Signals {
                in_use: true,
                ..Signals::default()
            },
            old,
            config,
        );
        assert_eq!(in_use.protected, Some(ProtectedReason::InUse));
    }

    fn artifact(id: &str, kind: ResourceKind, bytes: i128, age: f64) -> ObservedArtifact {
        ObservedArtifact {
            id: id.to_owned(),
            kind,
            labels: BTreeMap::new(),
            signals: Signals::default(),
            bytes: Some(bytes),
            age_seconds: Some(age),
        }
    }

    #[test]
    fn a_healthy_machine_is_silent() {
        let artifacts = [artifact("c", ResourceKind::Container, 1024, DEFAULT_TTL_SECONDS * 2.0)];
        let census = census(&artifacts, None, CensusConfig::default());
        assert_eq!(warning(&census, WarningThreshold::default()), None);
    }

    #[test]
    fn the_threshold_decides_by_bytes_or_objects() {
        let artifacts = [artifact(
            "c",
            ResourceKind::Container,
            DEFAULT_WARN_BYTES + 1,
            DEFAULT_TTL_SECONDS * 2.0,
        )];
        let census = census(&artifacts, None, CensusConfig::default());
        let warning = warning(&census, WarningThreshold::default()).expect("over the byte gate");
        assert!(!warning.partial);
        assert_eq!(warning.reclaimable_objects, 1);
    }

    #[test]
    fn a_partial_census_always_warns() {
        let census = Census {
            partial: true,
            ..Census::default()
        };
        let warning = warning(&census, WarningThreshold::default()).expect("partial warns");
        assert!(warning.partial);
        assert_eq!(warning.reclaimable_bytes, 0);
    }

    #[test]
    fn the_reclaimable_total_never_promises_bytes_no_command_can_free() {
        let artifacts = [
            artifact("c", ResourceKind::Container, 100, DEFAULT_TTL_SECONDS * 2.0),
            artifact("cache", ResourceKind::Builder, 900, DEFAULT_TTL_SECONDS * 2.0),
        ];
        let census = census(&artifacts, None, CensusConfig::default());
        assert_eq!(census.reclaimable_objects, 1);
        assert_eq!(census.reclaimable_bytes, 100);
        // The build cache is still reported, and the warning says so separately.
        let summary = census
            .classes
            .iter()
            .find(|summary| summary.class == UnmanagedClass::BuildCache)
            .expect("build cache reported");
        assert_eq!(summary.eligible_bytes, 900);
        assert!(
            warning(&census, WarningThreshold::default()).is_none(),
            "below threshold, so no warning at all"
        );
        let loud = warning(
            &census,
            WarningThreshold {
                bytes: 1,
                objects: 1,
            },
        )
        .expect("over threshold");
        assert_eq!(loud.report_only_bytes, 900);
        assert_eq!(loud.reclaimable_bytes, 100);
    }

    #[test]
    fn build_cache_is_reported_but_never_removed_by_id() {
        let artifacts = [artifact(
            "cache",
            ResourceKind::Builder,
            4096,
            DEFAULT_TTL_SECONDS * 2.0,
        )];
        let selected = plan(&artifacts, None, CensusConfig::default(), &[]);
        assert!(selected.candidates.is_empty(), "build cache is never a candidate");
        assert_eq!(selected.report_only.len(), 1);
        assert_eq!(selected.report_only[0].class, UnmanagedClass::BuildCache);
        assert_eq!(selected.report_only[0].eligible_bytes, 4096);
    }

    #[test]
    fn a_plan_selects_tier_one_only_unless_named() {
        let mut dangling_image =
            artifact("image", ResourceKind::Image, 200, DEFAULT_TTL_SECONDS * 2.0);
        dangling_image.signals.dangling = true;
        let mut unmeasured =
            artifact("unmeasured", ResourceKind::Container, 0, DEFAULT_TTL_SECONDS * 2.0);
        unmeasured.bytes = None;
        let artifacts = [
            artifact("container", ResourceKind::Container, 100, DEFAULT_TTL_SECONDS * 2.0),
            dangling_image,
            // Tagged and unreferenced: reviewable, never swept.
            artifact("local", ResourceKind::Image, 300, DEFAULT_TTL_SECONDS * 2.0),
            artifact("young", ResourceKind::Container, 400, 60.0),
            unmeasured,
        ];
        let selected = plan(&artifacts, None, CensusConfig::default(), &[]);
        let ids: Vec<&str> = selected.candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["container", "image"], "containers before images");
        assert_eq!(selected.bytes, 300);
        assert!(selected.review.iter().any(|c| c.id == "local"));

        // Naming a Tier-2 artifact opts exactly that one in.
        let included = plan(&artifacts, None, CensusConfig::default(), &["local".to_owned()]);
        let ids: Vec<&str> = included.candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["container", "image", "local"]);
        assert_eq!(included.bytes, 600);
        assert!(included.review.iter().all(|c| c.id != "local"));
    }

    #[test]
    fn an_unmeasured_artifact_never_enters_a_plan() {
        let mut unmeasured = artifact("x", ResourceKind::Container, 0, DEFAULT_TTL_SECONDS * 2.0);
        unmeasured.bytes = None;
        let selected = plan(&[unmeasured], None, CensusConfig::default(), &[]);
        assert!(selected.candidates.is_empty());
        assert_eq!(selected.bytes, 0);
    }

    #[test]
    fn an_acknowledgement_suppresses_until_the_footprint_grows() {
        let artifacts = [artifact(
            "c",
            ResourceKind::Container,
            DEFAULT_WARN_BYTES + 1,
            DEFAULT_TTL_SECONDS * 2.0,
        )];
        let census = census(&artifacts, None, CensusConfig::default());
        let now = 1_000_000.0;
        let ack = Acknowledgement {
            at: now,
            objects: census.reclaimable_objects,
            bytes: census.reclaimable_bytes,
        };
        assert!(acknowledgement_suppresses(Some(ack), &census, now));
        // Material growth re-arms it.
        let grown = Census {
            reclaimable_bytes: (ack.bytes as f64 * 1.5) as i128,
            ..census.clone()
        };
        assert!(!acknowledgement_suppresses(Some(ack), &grown, now));
        // So does age.
        assert!(!acknowledgement_suppresses(
            Some(ack),
            &census,
            now + ACK_MAX_AGE_SECONDS
        ));
        // An unreadable clock keeps the user's acknowledgement.
        assert!(acknowledgement_suppresses(Some(ack), &census, f64::NAN));
        assert!(!acknowledgement_suppresses(None, &census, now));
    }

    #[test]
    fn networks_are_protected_because_the_census_cannot_measure_them() {
        // `docker system df -v` has no network section, so a network has no size and no age.
        // It is never reclaimable, whatever its apparent age.
        let verdict = classify(
            ResourceKind::Network,
            &BTreeMap::new(),
            None,
            Signals::default(),
            Some(DEFAULT_TTL_SECONDS * 100.0),
            CensusConfig::default(),
        );
        assert_eq!(verdict.class, None);
        assert_eq!(verdict.protected, Some(ProtectedReason::Unclassified));
    }

    #[test]
    fn unknown_age_fails_closed() {
        let verdict = classify(
            ResourceKind::Volume,
            &BTreeMap::new(),
            None,
            Signals {
                anonymous: true,
                ..Signals::default()
            },
            None,
            CensusConfig::default(),
        );
        assert_eq!(verdict.class, None);
        assert_eq!(verdict.protected, Some(ProtectedReason::Unmeasured));
    }

    #[test]
    fn below_the_age_gate_is_reported_but_not_eligible() {
        let verdict = classify(
            ResourceKind::Container,
            &BTreeMap::new(),
            None,
            Signals::default(),
            Some(60.0),
            CensusConfig::default(),
        );
        assert_eq!(verdict.class, Some(UnmanagedClass::StoppedContainer));
        assert!(!verdict.age_eligible);
    }

    #[test]
    fn census_totals_only_count_eligible_tier_one() {
        let artifacts = vec![
            ObservedArtifact {
                id: "old".into(),
                kind: ResourceKind::Container,
                labels: BTreeMap::new(),
                signals: Signals::default(),
                bytes: Some(100),
                age_seconds: Some(DEFAULT_TTL_SECONDS * 2.0),
            },
            ObservedArtifact {
                id: "new".into(),
                kind: ResourceKind::Container,
                labels: BTreeMap::new(),
                signals: Signals::default(),
                bytes: Some(500),
                age_seconds: Some(1.0),
            },
            ObservedArtifact {
                id: "local".into(),
                kind: ResourceKind::Image,
                labels: BTreeMap::new(),
                signals: Signals::default(),
                bytes: Some(900),
                age_seconds: Some(DEFAULT_TTL_SECONDS * 2.0),
            },
        ];
        let census = census(&artifacts, None, CensusConfig::default());
        assert_eq!(census.reclaimable_objects, 1);
        assert_eq!(census.reclaimable_bytes, 100);
        assert!(census.bytes_approximate);
        assert!(!census.partial);
        let stopped = census
            .classes
            .iter()
            .find(|summary| summary.class == UnmanagedClass::StoppedContainer)
            .expect("stopped container summary");
        assert_eq!(stopped.objects, 2);
        assert_eq!(stopped.eligible_objects, 1);
        // Tier 2 is reported but never counted as reclaimable.
        let local_only = census
            .classes
            .iter()
            .find(|summary| summary.class == UnmanagedClass::UnreferencedImage)
            .expect("local-only summary");
        assert_eq!(local_only.tier, Tier::Review);
        assert_eq!(local_only.eligible_objects, 0);
    }

    #[test]
    fn an_unmeasurable_size_makes_the_census_partial() {
        let artifacts = vec![ObservedArtifact {
            id: "x".into(),
            kind: ResourceKind::Container,
            labels: BTreeMap::new(),
            signals: Signals::default(),
            bytes: None,
            age_seconds: Some(DEFAULT_TTL_SECONDS * 2.0),
        }];
        assert!(census(&artifacts, None, CensusConfig::default()).partial);
    }

    #[test]
    fn docker_sizes_parse_and_unknown_units_refuse() {
        assert_eq!(parse_docker_size("32B"), Some(32));
        assert_eq!(parse_docker_size("15.92kB"), Some(15_920));
        assert_eq!(parse_docker_size("195.6MB"), Some(195_600_000));
        assert_eq!(parse_docker_size("1.67GB"), Some(1_670_000_000));
        assert_eq!(parse_docker_size("117MB*"), Some(117_000_000));
        assert_eq!(parse_docker_size("2GiB"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_docker_size("N/A"), None);
        assert_eq!(parse_docker_size(""), None);
        assert_eq!(parse_docker_size("12 furlongs"), None);
        assert_eq!(parse_docker_size("-5MB"), None);
    }

    #[test]
    fn docker_timestamps_parse_with_numeric_offsets() {
        // 1970-01-01T00:00:00Z
        assert_eq!(parse_docker_timestamp("1970-01-01 00:00:00 +0000 UTC"), Some(0.0));
        // A negative offset shifts the instant later in UTC.
        assert_eq!(
            parse_docker_timestamp("1970-01-01 00:00:00 -0700 PDT"),
            Some(7.0 * 3600.0)
        );
        // Fractional seconds are truncated to nanosecond precision.
        let fraction = parse_docker_timestamp("1970-01-01 00:00:01.5 +0000 UTC").unwrap();
        assert!((fraction - 1.5).abs() < 1e-9);
        assert_eq!(parse_docker_timestamp("2026-09-16 12:13:31 -0700 PDT"), Some(1_789_586_011.0));
        assert_eq!(parse_docker_timestamp("not a timestamp"), None);
        assert_eq!(parse_docker_timestamp("2026-13-16 12:13:31 -0700 PDT"), None);
        assert_eq!(parse_docker_timestamp("2026-09-16 12:13:31"), None);
    }

    #[test]
    fn label_lists_parse_and_a_bare_key_counts_as_present() {
        let parsed = parse_label_list("a=1,b=2");
        assert_eq!(parsed.get("a"), Some(&"1".to_owned()));
        assert_eq!(parsed.get("b"), Some(&"2".to_owned()));
        assert!(parse_label_list("com.docker.volume.anonymous=").contains_key("com.docker.volume.anonymous"));
        assert!(parse_label_list("bare").contains_key("bare"));
        assert!(parse_label_list("").is_empty());
    }

    #[test]
    fn observe_maps_the_df_sections_onto_artifacts() {
        let json = include_str!("../tests/fixtures/system_df_minimal.json");
        let report: SystemDfReport = serde_json::from_str(json).expect("fixture parses");
        let observed = observe(EngineObservation {
            report: &report,
            dangling_image_ids: &["sha256:danglingimage".to_owned()],
            bosn_labeled_image_ids: &["sha256:bosnimage".to_owned()],
            inspected_volumes: &[],
            now: 1_789_595_611.0,
        });
        assert_eq!(observed.len(), 4);
        let image = &observed[0];
        assert_eq!(image.kind, ResourceKind::Image);
        assert!(image.signals.dangling);
        assert!(!image.signals.in_use);
        let labeled = observed
            .iter()
            .find(|artifact| artifact.id == "sha256:bosnimage")
            .expect("labeled image present");
        assert_eq!(
            classify_ownership(&labeled.labels, Some("r1")),
            OwnershipClass::IncompleteLabels
        );
        let volume = observed
            .iter()
            .find(|artifact| artifact.kind == ResourceKind::Volume)
            .expect("volume present");
        assert!(volume.signals.anonymous, "anonymous volume detected from its label");
        assert_eq!(
            volume.age_seconds, None,
            "a volume without inspect detail has no age, and is therefore protected"
        );
    }

    #[test]
    fn only_docker_reported_dangling_images_are_dangling() {
        // An untagged image that is still the parent of a tagged one is not dangling. If the
        // caller's dangling set omits it, the heuristic must not override that.
        let json = include_str!("../tests/fixtures/system_df_minimal.json");
        let report: SystemDfReport = serde_json::from_str(json).expect("fixture parses");
        let observed = observe(EngineObservation {
            report: &report,
            dangling_image_ids: &[],
            bosn_labeled_image_ids: &[],
            inspected_volumes: &[],
            now: 1_789_595_611.0,
        });
        let image = &observed[0];
        assert!(!image.signals.dangling);
        // It is still untagged and local-only, so it lands in the review tier rather than
        // disappearing.
        let verdict = classify(
            image.kind,
            &image.labels,
            None,
            image.signals,
            image.age_seconds,
            CensusConfig::default(),
        );
        assert_eq!(verdict.class, Some(UnmanagedClass::UnreferencedImage));
    }

    #[test]
    fn inspected_volumes_supply_the_age_the_accounting_document_lacks() {
        let json = include_str!("../tests/fixtures/system_df_minimal.json");
        let report: SystemDfReport = serde_json::from_str(json).expect("fixture parses");
        let now = 1_789_595_611.0;
        let observed = observe(EngineObservation {
            report: &report,
            dangling_image_ids: &[],
            bosn_labeled_image_ids: &[],
            inspected_volumes: &[InspectedVolume {
                name: "f1c1cdf1f2ba212d6d08336115a83821aa80e1971ce9aa2513dd65bfce07f7ca".to_owned(),
                created_at: "2026-08-01T00:00:00-07:00".to_owned(),
                labels: BTreeMap::from([(
                    "com.docker.volume.anonymous".to_owned(),
                    String::new(),
                )]),
            }],
            now,
        });
        let volume = observed
            .iter()
            .find(|artifact| artifact.kind == ResourceKind::Volume)
            .expect("volume present");
        let age = volume.age_seconds.expect("inspected age");
        // 2026-08-01T00:00:00-07:00 is 2026-08-01T07:00:00Z.
        assert!((age - (now - 1_785_567_600.0)).abs() < 1.0, "age was {age}");
        let verdict = classify(
            volume.kind,
            &volume.labels,
            None,
            volume.signals,
            volume.age_seconds,
            CensusConfig::default(),
        );
        assert_eq!(verdict.class, Some(UnmanagedClass::AnonymousVolume));
        assert!(verdict.age_eligible, "a month-old anonymous volume is eligible");
    }

    #[test]
    fn rfc3339_and_accounting_timestamps_agree() {
        assert_eq!(
            parse_rfc3339_timestamp("2026-09-13T18:41:43-07:00"),
            parse_docker_timestamp("2026-09-13 18:41:43 -0700 PDT")
        );
        let fraction = parse_rfc3339_timestamp("2026-09-16T08:56:39.263133667-07:00").unwrap();
        assert!((fraction - 1_789_574_199.263_133_667).abs() < 1e-6);
        assert_eq!(
            parse_rfc3339_timestamp("2026-09-13 18:41:43 +0000 UTC"),
            parse_docker_timestamp("2026-09-13 18:41:43 +0000 UTC")
        );
        assert_eq!(parse_rfc3339_timestamp("nonsense"), None);
        // A UTC designator is accepted, and a date without a zone is not.
        assert_eq!(
            parse_rfc3339_timestamp("1970-01-01T00:00:00Z"),
            Some(0.0)
        );
        assert_eq!(parse_rfc3339_timestamp("1970-01-01T00:00:00"), None);
    }
}
