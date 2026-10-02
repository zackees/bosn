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
//! that plan is a daemon-owned operation (`bosn gc --unmanaged --apply`), not in this crate.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{LABEL_REGISTRY, NAMESPACE, REQUIRED_LABELS, ResourceKind};
mod docker;
pub use docker::*;

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

/// Why free-space pressure may or may not justify evicting what Bosn owns.
///
/// This is G7 from #147. Free-space pressure is measured against the whole filesystem, while
/// the byte ceiling counts only Bosn-owned bytes. On a disk dominated by artifacts Bosn is
/// not permitted to reclaim, pressure latches on permanently and drives Bosn to evict its own
/// warm caches — for zero net benefit, because the bytes causing the pressure are not the
/// bytes being freed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PressureAttribution {
    /// Not under free-space pressure, so eviction policy is unchanged.
    NotUnderPressure,
    /// The shortfall is small enough that reclaiming owned bytes can actually close it.
    OwnedBytesCanClose,
    /// The shortfall exceeds everything Bosn owns: no owned eviction can help.
    ForeignBytesDominate,
    /// The census could not be read completely, so nothing may be evicted.
    CensusIncomplete,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PressureDecision {
    /// Whether owned warm caches may be evicted under this pressure.
    pub may_evict_owned: bool,
    /// How many bytes short of the free-space floor the filesystem is.
    pub shortfall_bytes: i128,
    pub attribution: PressureAttribution,
}

/// Decide whether pressure justifies evicting owned resources.
///
/// The rule is deliberately narrow. Eviction is only suppressed when it provably cannot help:
/// the shortfall is larger than everything Bosn owns. Everything else keeps the existing
/// behaviour, so this cannot silently disable retention.
#[must_use]
pub fn pressure_decision(
    under_pressure: bool,
    free_space_exceeded: bool,
    free_bytes: i128,
    min_free_bytes: i128,
    owned_bytes: i128,
    census_trustworthy: bool,
) -> PressureDecision {
    if !census_trustworthy {
        // An incomplete census is a reason to keep, never to free.
        return PressureDecision {
            may_evict_owned: false,
            shortfall_bytes: 0,
            attribution: PressureAttribution::CensusIncomplete,
        };
    }
    if !under_pressure {
        return PressureDecision {
            may_evict_owned: true,
            shortfall_bytes: 0,
            attribution: PressureAttribution::NotUnderPressure,
        };
    }
    // Pressure from a count or byte ceiling is about Bosn's own resources, not the disk, so
    // there is no shortfall to attribute.
    let shortfall = if free_space_exceeded {
        min_free_bytes.saturating_sub(free_bytes).max(0)
    } else {
        0
    };
    if shortfall <= owned_bytes {
        PressureDecision {
            may_evict_owned: true,
            shortfall_bytes: shortfall,
            attribution: PressureAttribution::OwnedBytesCanClose,
        }
    } else {
        PressureDecision {
            may_evict_owned: false,
            shortfall_bytes: shortfall,
            attribution: PressureAttribution::ForeignBytesDominate,
        }
    }
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
    key == NAMESPACE
        || key
            .strip_prefix(NAMESPACE)
            .is_some_and(|rest| rest.starts_with('.'))
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
pub fn census(
    artifacts: &[ObservedArtifact],
    our_registry: Option<&str>,
    config: CensusConfig,
) -> Census {
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

#[cfg(test)]
mod tests;
