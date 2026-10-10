//! Pure retention policy for resources Bosn owns.
//!
//! The unmanaged census deliberately protects everything this registry owns
//! (`ProtectedReason::OwnedByThisRegistry`), so Bosn's own containers, volumes and images were
//! never a candidate for anything: `bosn-setup-v2-*` is created without `--rm` and has no idle
//! reaper, `bosn-v-stack-*` / `bosn-v-machine-*` are explicit-release only, and `bosn-setup:*`
//! images have no expiry at all. On a long-lived machine those accumulate without bound, and a
//! pinned container keeps every volume it ever mounted alive behind it.
//!
//! This module is the missing half: given observations of Bosn-owned resources, decide which are
//! past their age gate and safe to reclaim. It answers one question — "may this exact object be
//! removed?" — and deliberately does not decide *when* to ask. Callers own the clock, the engine
//! and the registry; this module only validates data and returns conservative verdicts, the same
//! contract as the rest of `bosn-core`.
//!
//! Three rules make the policy safe to run unattended:
//!
//! 1. **Ownership is proven, never assumed.** An object is reclaimable only when its complete
//!    Bosn label set is present and names *this* registry. Incomplete labels fail closed.
//! 2. **In use means alive.** A container that is running, or a volume any container still
//!    mounts, is never reclaimable regardless of age.
//! 3. **Pinned means forever.** `Retention::Pinned` is an explicit human promise and no age gate
//!    overrides it.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::unmanaged::is_bosn_label;
use crate::{
    LABEL_RETENTION, ObservedArtifact, OwnershipClass, ResourceKind, Retention, classify_ownership,
};

/// Default age gate for a stopped container that Bosn owns.
///
/// Setup containers are recreated on demand and carry no state worth keeping once stopped, so
/// this is deliberately short: the leak that motivated this module was an unbounded pile of
/// exited containers each pinning a stack volume.
pub const DEFAULT_CONTAINER_TTL: Duration = Duration::from_secs(6 * 3600);

/// Default age gate for a Bosn-owned volume.
///
/// Durable stack and machine scopes are cached build/tool state. The gate is long because a
/// cold rebuild is expensive, and short enough that the cache cannot grow without bound.
pub const DEFAULT_VOLUME_TTL: Duration = Duration::from_secs(14 * 86_400);

/// Default age gate for a Bosn-owned image.
///
/// Images are content-addressed by tag, so a removed one is rebuilt rather than lost. This is
/// long because `bosn-setup:*` builds are slow, and every unreferenced image is a candidate for
/// a later generation.
pub const DEFAULT_IMAGE_TTL: Duration = Duration::from_secs(30 * 86_400);

/// Upper bound on one reclamation pass. A plan larger than this stops early and reports the
/// remainder rather than running unbounded.
pub const MAX_MANAGED_REMOVALS: usize = 1024;

/// Per-kind age gates and the size ceiling for one pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetentionPolicy {
    pub container_ttl: Duration,
    pub volume_ttl: Duration,
    pub image_ttl: Duration,
    /// Reclaim at most this many bytes per pass. Exceeding it is not an error: the pass takes
    /// the oldest first and leaves the rest for the next one.
    pub max_bytes: Option<i128>,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            container_ttl: DEFAULT_CONTAINER_TTL,
            volume_ttl: DEFAULT_VOLUME_TTL,
            image_ttl: DEFAULT_IMAGE_TTL,
            max_bytes: None,
        }
    }
}

impl RetentionPolicy {
    /// The age gate for one resource kind. A kind with no gate is never age-reclaimed.
    #[must_use]
    pub fn ttl_for(self, kind: ResourceKind) -> Option<Duration> {
        match kind {
            ResourceKind::Container => Some(self.container_ttl),
            ResourceKind::Volume => Some(self.volume_ttl),
            ResourceKind::Image => Some(self.image_ttl),
            ResourceKind::Builder | ResourceKind::Network => None,
        }
    }
}

/// Why a Bosn-owned object is being kept despite being past its age gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HoldReason {
    /// Running, or mounted by a container that exists.
    InUse,
    /// `Retention::Pinned`: an explicit promise no age gate overrides.
    Pinned,
    /// Still within its age gate.
    WithinTtl,
    /// The engine did not report an age, so eligibility cannot be proven. Fail closed.
    AgeUnknown,
    /// Complete labels naming a different registry.
    ForeignRegistry,
    /// Bosn labels present but incomplete: ownership cannot be proven.
    IncompleteLabels,
    /// Carries no Bosn label at all. Not ours to reclaim.
    NotBosnOwned,
    /// A network or builder record: outside this policy's scope.
    UnsupportedKind,
}

impl HoldReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InUse => "in-use",
            Self::Pinned => "pinned",
            Self::WithinTtl => "within-ttl",
            Self::AgeUnknown => "age-unknown",
            Self::ForeignRegistry => "foreign-registry",
            Self::IncompleteLabels => "incomplete-labels",
            Self::NotBosnOwned => "not-bosn-owned",
            Self::UnsupportedKind => "unsupported-kind",
        }
    }

    /// Every reason, in report order.
    pub const ALL: [Self; 8] = [
        Self::InUse,
        Self::Pinned,
        Self::WithinTtl,
        Self::AgeUnknown,
        Self::ForeignRegistry,
        Self::IncompleteLabels,
        Self::NotBosnOwned,
        Self::UnsupportedKind,
    ];

    /// The inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value)
    }
}

/// The verdict for one observed object.
#[derive(Clone, Debug, PartialEq)]
pub struct RetentionVerdict {
    pub id: String,
    pub kind: ResourceKind,
    /// `None` when the object may be removed.
    pub hold: Option<HoldReason>,
    /// Age past the applicable gate, when the object is held only by its age gate.
    pub age_seconds: Option<f64>,
    pub bytes: Option<i128>,
}

impl RetentionVerdict {
    /// Whether this exact object may be removed right now.
    #[must_use]
    pub fn is_reclaimable(&self) -> bool {
        self.hold.is_none()
    }
}

/// Classify one observed object against the policy.
///
/// The order of the checks is the safety contract. Ownership is settled first, because an object
/// that is not provably ours is never a candidate no matter how old or idle it is. Liveness is
/// settled next, before age, so a running container is never even measured against a gate. Only
/// then does the age gate apply.
#[must_use]
pub fn classify_managed(
    artifact: &ObservedArtifact,
    our_registry: Option<&str>,
    policy: RetentionPolicy,
) -> RetentionVerdict {
    let mut verdict = RetentionVerdict {
        id: artifact.id.clone(),
        kind: artifact.kind,
        hold: None,
        age_seconds: artifact.age_seconds,
        bytes: artifact.bytes,
    };

    // 1. Ownership. A bare `bosn-act-*` name is not evidence; only the label set is.
    if !artifact.labels.keys().any(|key| is_bosn_label(key)) {
        verdict.hold = Some(HoldReason::NotBosnOwned);
        return verdict;
    }
    match classify_ownership(&artifact.labels, our_registry) {
        OwnershipClass::IncompleteLabels => {
            verdict.hold = Some(HoldReason::IncompleteLabels);
            return verdict;
        }
        OwnershipClass::ForeignRegistry => {
            verdict.hold = Some(HoldReason::ForeignRegistry);
            return verdict;
        }
        // `Unlabeled` is unreachable: the first check proved a Bosn label exists.
        OwnershipClass::Ours | OwnershipClass::Unlabeled => {}
    }

    // 2. Scope. A network or builder record has no age-based contract here.
    let Some(ttl) = policy.ttl_for(artifact.kind) else {
        verdict.hold = Some(HoldReason::UnsupportedKind);
        return verdict;
    };

    // 3. Liveness, before age. A container that is running, or a volume something still mounts,
    //    is never reclaimable at any age.
    if artifact.signals.in_use {
        verdict.hold = Some(HoldReason::InUse);
        return verdict;
    }

    // 4. An explicit human promise outranks every age gate.
    if is_pinned(&artifact.labels) {
        verdict.hold = Some(HoldReason::Pinned);
        return verdict;
    }

    // 5. The age gate itself. An unmeasured age cannot authorize a removal.
    let Some(age) = artifact.age_seconds else {
        verdict.hold = Some(HoldReason::AgeUnknown);
        return verdict;
    };
    let gate = ttl.as_secs_f64();
    if age < gate {
        verdict.hold = Some(HoldReason::WithinTtl);
    }
    verdict
}

/// Whether the labels carry an explicit pin.
///
/// A missing retention label is *not* a pin: durable volumes that predate the label, or that
/// were written by an older build, still get the benefit of the doubt and are held rather than
/// reclaimed on the strength of an absent field.
fn is_pinned(labels: &BTreeMap<String, String>) -> bool {
    labels
        .get(LABEL_RETENTION)
        .and_then(|value| Retention::parse(value))
        == Some(Retention::Pinned)
}

/// One reclamation pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetentionPlan {
    /// Reclaimable objects, in removal order and capped by the policy's budget.
    pub candidates: Vec<RetentionVerdict>,
    /// Objects held despite being past their age gate, with the reason.
    pub held: Vec<RetentionVerdict>,
    /// Bytes the capped candidates would reclaim.
    pub bytes: i128,
    /// Reclaimable objects left over because a cap was reached.
    pub deferred: usize,
}

impl RetentionPlan {
    /// How many observed objects each reason held (#545). An object nothing reclaimed is never
    /// silent: "no candidates" and "held for a reason" read differently.
    #[must_use]
    pub fn held_counts(&self) -> BTreeMap<HoldReason, u64> {
        let mut counts = BTreeMap::new();
        for verdict in &self.held {
            if let Some(reason) = verdict.hold {
                *counts.entry(reason).or_insert(0) += 1;
            }
        }
        counts
    }
}

/// Build a plan for one pass over the observed Bosn-owned objects.
///
/// Removal order is the dependency order Docker requires: containers first, because a stopped
/// container pins every volume it mounted; then volumes, which only become removable once no
/// container references them; then images. Within a kind, oldest first, so a capped pass takes
/// the bytes that have been idle longest.
#[must_use]
pub fn plan_managed(
    artifacts: &[ObservedArtifact],
    our_registry: Option<&str>,
    policy: RetentionPolicy,
) -> RetentionPlan {
    let mut classified: Vec<RetentionVerdict> = artifacts
        .iter()
        .map(|artifact| classify_managed(artifact, our_registry, policy))
        .collect();

    let mut plan = RetentionPlan::default();
    for verdict in &mut classified {
        if verdict.is_reclaimable() {
            plan.candidates.push(verdict.clone());
        } else {
            plan.held.push(verdict.clone());
        }
    }

    // Oldest first inside each kind, so ties on an unmeasured age keep a stable order.
    plan.candidates.sort_by(|left, right| {
        kind_rank(left.kind)
            .cmp(&kind_rank(right.kind))
            .then_with(|| {
                right
                    .age_seconds
                    .unwrap_or(f64::MIN)
                    .total_cmp(&left.age_seconds.unwrap_or(f64::MIN))
            })
            .then_with(|| left.id.cmp(&right.id))
    });

    apply_budget(&mut plan, policy);
    plan
}

/// Truncate the plan to `MAX_MANAGED_REMOVALS` and the optional byte ceiling.
///
/// An unmeasured size is deferred rather than counted as zero: a byte ceiling exists to bound
/// what a pass destroys, and an object whose size the engine would not report cannot be proven
/// to fit inside it. It still counts against the object cap either way.
fn apply_budget(plan: &mut RetentionPlan, policy: RetentionPolicy) {
    let mut bytes = 0_i128;
    let mut kept: Vec<RetentionVerdict> = Vec::with_capacity(plan.candidates.len());
    let mut deferred = 0_usize;

    for verdict in plan.candidates.drain(..) {
        if kept.len() >= MAX_MANAGED_REMOVALS {
            deferred += 1;
            continue;
        }
        if let Some(ceiling) = policy.max_bytes {
            // An unmeasured size is not zero. Defer it rather than assume it is cheap.
            let Some(size) = verdict.bytes else {
                deferred += 1;
                continue;
            };
            if bytes.saturating_add(size) > ceiling {
                deferred += 1;
                continue;
            }
            bytes = bytes.saturating_add(size);
            kept.push(verdict);
            continue;
        }
        bytes = bytes.saturating_add(verdict.bytes.unwrap_or(0));
        kept.push(verdict);
    }

    plan.candidates = kept;
    plan.bytes = bytes;
    plan.deferred = deferred;
}

/// Containers before volumes before images.
fn kind_rank(kind: ResourceKind) -> u8 {
    match kind {
        ResourceKind::Container => 0,
        ResourceKind::Volume => 1,
        ResourceKind::Image => 2,
        ResourceKind::Builder => 3,
        ResourceKind::Network => 4,
    }
}

/// Seconds since the Unix epoch, or `None` if the clock predates it.
///
/// A negative observation is not an age: a clock skewed into the future must fail closed rather
/// than make everything look infinitely old.
#[must_use]
pub fn age_seconds(created: SystemTime) -> Option<f64> {
    let secs = created.duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
    (secs >= 0.0).then_some(secs)
}

#[cfg(test)]
mod tests;
