//! Pure Bosn domain policy. Engine, registry, clock, and process observations are inputs.
//!
//! This crate deliberately has no system dependencies: callers collect observations and make
//! mutations; this crate only validates data and returns conservative decisions.

use std::collections::BTreeMap;

pub mod compose;
pub mod config;
pub mod manifest;
pub mod setup;
pub mod unmanaged;

pub use compose::{
    BuildSpec, COMPOSE_PLAN_VERSION, COMPOSE_SETUP_PLAN_VERSION, ComposeDocument, ComposeError,
    ComposeErrorCode, ComposePlan, ComposeSetupPlan, DependencySpec, HealthcheckSpec, MountSpec,
    RelativePath, ResourceSpec, ServiceSpec, parse_and_plan_compose_yaml,
    parse_and_translate_compose_yaml, parse_compose_yaml, plan_compose, translate_compose_to_setup,
};
pub use config::{
    AppPolicy, MachinePolicy, PolicyDefaults, PolicyError, PolicyOrigin, parse_machine_policy_toml,
    resolve_app_policy, resolve_machine_policy,
};
pub use manifest::{Manifest, ManifestError, ManifestRoots, parse_manifest_toml};
pub use setup::{
    CompanionFile, MAX_COMPANION_FILE_BYTES, MAX_COMPANION_FILES, MAX_ENVIRONMENT_ENTRIES,
    MAX_INLINE_DOCKERFILE_BYTES, MAX_MOUNTS, MAX_SETUP_DOCUMENT_BYTES, MAX_TASKS,
    SETUP_DOCUMENT_VERSION, SetupApp, SetupConfigLocator, SetupDocument, SetupDocumentError,
    SetupSource, SetupTask, WorkspaceMount, parse_setup_config_locator, parse_setup_document_toml,
};
pub use unmanaged::{
    ACK_GROWTH_RATIO, ACK_MAX_AGE_SECONDS, Acknowledgement, Census, CensusConfig, ClassSummary,
    Classification, DEFAULT_TTL_SECONDS, DEFAULT_WARN_BYTES, DEFAULT_WARN_OBJECTS,
    EngineObservation, InspectedVolume, ObservedArtifact, OwnershipClass, Plan, PlanCandidate,
    ProtectedReason, ProtectedSummary, Signals, SystemDfReport, Tier, UnmanagedClass, Warning,
    WarningThreshold, acknowledgement_suppresses, census, classify, classify_ownership,
    PressureAttribution, PressureDecision, is_removable_by_id, observe, parse_docker_size,
    parse_docker_timestamp, parse_label_list, parse_rfc3339_timestamp, plan, pressure_decision,
    removal_rank, warning,
};

pub const NAMESPACE: &str = "com.zackees.bosn";
pub const LABEL_REGISTRY: &str = "com.zackees.bosn.registry";
pub const LABEL_KIND: &str = "com.zackees.bosn.kind";
pub const LABEL_STACK: &str = "com.zackees.bosn.stack";
pub const LABEL_GENERATION: &str = "com.zackees.bosn.generation";
pub const LABEL_SCOPE: &str = "com.zackees.bosn.scope";
pub const LABEL_WORKSPACE: &str = "com.zackees.bosn.workspace";
pub const LABEL_CREATED: &str = "com.zackees.bosn.created";
pub const LABEL_RETENTION: &str = "com.zackees.bosn.retention";
pub const REQUIRED_LABELS: [&str; 7] = [
    LABEL_REGISTRY,
    LABEL_KIND,
    LABEL_STACK,
    LABEL_GENERATION,
    LABEL_SCOPE,
    LABEL_WORKSPACE,
    LABEL_CREATED,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ResourceKind {
    Container,
    Volume,
    Image,
    Builder,
    Network,
}
impl ResourceKind {
    pub fn parse(v: &str) -> Option<Self> {
        Some(match v {
            "container" => Self::Container,
            "volume" => Self::Volume,
            "image" => Self::Image,
            "builder" => Self::Builder,
            "network" => Self::Network,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Volume => "volume",
            Self::Image => "image",
            Self::Builder => "builder",
            Self::Network => "network",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Scope {
    Spec,
    Stack,
    Machine,
}
impl Scope {
    pub fn parse(v: &str) -> Option<Self> {
        Some(match v {
            "spec" => Self::Spec,
            "stack" => Self::Stack,
            "machine" => Self::Machine,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::Stack => "stack",
            Self::Machine => "machine",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retention {
    Warm,
    Pinned,
}
impl Retention {
    pub fn parse(v: &str) -> Option<Self> {
        Some(match v {
            "warm" => Self::Warm,
            "pinned" => Self::Pinned,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Pinned => "pinned",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceLabels {
    pub registry: String,
    pub kind: ResourceKind,
    pub stack: String,
    pub generation: String,
    pub scope: Scope,
    pub workspace: String,
    pub created: String,
    pub retention: Retention,
    pub retention_explicit: bool,
}
impl ResourceLabels {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: &str,
        kind: ResourceKind,
        stack: &str,
        generation: &str,
        scope: Scope,
        workspace: &str,
        created: &str,
        retention: Option<Retention>,
    ) -> Result<Self, LabelError> {
        if registry.is_empty() {
            return Err(LabelError::EmptyRegistry);
        }
        if [stack, generation, workspace, created]
            .iter()
            .any(|x| x.is_empty())
        {
            return Err(LabelError::Incomplete);
        }
        Ok(Self {
            registry: registry.into(),
            kind,
            stack: stack.into(),
            generation: generation.into(),
            scope,
            workspace: workspace.into(),
            created: created.into(),
            retention: retention.unwrap_or(Retention::Warm),
            retention_explicit: retention.is_some(),
        })
    }
    pub fn to_map(&self) -> BTreeMap<&'static str, String> {
        let mut m = BTreeMap::new();
        m.insert(LABEL_REGISTRY, self.registry.clone());
        m.insert(LABEL_KIND, self.kind.as_str().into());
        m.insert(LABEL_STACK, self.stack.clone());
        m.insert(LABEL_GENERATION, self.generation.clone());
        m.insert(LABEL_SCOPE, self.scope.as_str().into());
        m.insert(LABEL_WORKSPACE, self.workspace.clone());
        m.insert(LABEL_CREATED, self.created.clone());
        // A public value can be changed from legacy warm to pinned. Pinning is durable
        // policy, so its label must never be omitted even if the prior warm was legacy.
        if self.retention_explicit || self.retention == Retention::Pinned {
            m.insert(
                LABEL_RETENTION,
                match self.retention {
                    Retention::Warm => "warm",
                    Retention::Pinned => "pinned",
                }
                .into(),
            );
        }
        m
    }
    pub fn parse<I, K, V>(raw: I) -> Result<Self, LabelError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let m: BTreeMap<String, String> = raw
            .into_iter()
            .map(|(k, v)| (k.as_ref().into(), v.as_ref().into()))
            .collect();
        let get = |key| {
            m.get(key)
                .filter(|x| !x.is_empty())
                .map(String::as_str)
                .ok_or(LabelError::Incomplete)
        };
        let kind = ResourceKind::parse(get(LABEL_KIND)?).ok_or(LabelError::UnknownKind)?;
        let scope = Scope::parse(get(LABEL_SCOPE)?).ok_or(LabelError::UnknownScope)?;
        let ret = match m.get(LABEL_RETENTION) {
            None => None,
            Some(x) if x.is_empty() => return Err(LabelError::UnknownRetention),
            Some(x) => Some(Retention::parse(x).ok_or(LabelError::UnknownRetention)?),
        };
        Self::new(
            get(LABEL_REGISTRY)?,
            kind,
            get(LABEL_STACK)?,
            get(LABEL_GENERATION)?,
            scope,
            get(LABEL_WORKSPACE)?,
            get(LABEL_CREATED)?,
            ret,
        )
    }
    pub fn is_owned_by(&self, registry: &str) -> bool {
        !registry.is_empty()
            && self.registry == registry
            && !self.registry.is_empty()
            && !self.stack.is_empty()
            && !self.generation.is_empty()
            && !self.workspace.is_empty()
            && !self.created.is_empty()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelError {
    Incomplete,
    EmptyRegistry,
    UnknownKind,
    UnknownScope,
    UnknownRetention,
}
pub fn ownership_from_labels<I, K, V>(raw: I, registry: &str) -> bool
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    ResourceLabels::parse(raw).is_ok_and(|l| l.is_owned_by(registry))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceState {
    Active,
    Adopted,
    Done,
    /// Python-v4's durable terminal state. It must survive import even though
    /// no Rust lifecycle consumer has reconciliation authority yet.
    Retired,
}
impl ResourceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Adopted => "adopted",
            Self::Done => "done",
            Self::Retired => "retired",
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceSnapshot {
    pub id: String,
    pub name: String,
    pub kind: ResourceKind,
    pub stack: String,
    pub generation: String,
    pub scope: Scope,
    pub workspace: String,
    pub created_at: f64,
    pub last_used: f64,
    pub state: ResourceState,
    pub retention: Retention,
}
#[derive(Clone, Debug, PartialEq)]
pub struct LeaseSnapshot {
    pub id: String,
    pub resource_id: String,
    pub pid: u32,
    pub proc_start: Option<f64>,
    pub heartbeat_at: f64,
    pub ttl_seconds: f64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedLease {
    pub lease: LeaseSnapshot,
    /// A caller that did not obtain a liveness observation must pass `Unknown`.
    pub liveness: Liveness,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Liveness {
    ConfirmedDead,
    Alive { observed_start: Option<f64> },
    Unknown,
}
pub fn lease_expired(lease: &LeaseSnapshot, now: f64, observation: Liveness) -> bool {
    let elapsed = now - lease.heartbeat_at;
    if !finite(now)
        || !finite(lease.heartbeat_at)
        || !finite(lease.ttl_seconds)
        || lease.ttl_seconds < 0.0
        || !finite(elapsed)
        || elapsed <= lease.ttl_seconds
    {
        return false;
    };
    matches!(observation, Liveness::ConfirmedDead)
}
fn finite(v: f64) -> bool {
    v.is_finite()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionSignals {
    pub superseded: bool,
    pub workspace_done: bool,
}
/// Aggregate every registered consumer. Empty, current, or active use is protective.
pub fn retention_signals<I>(uses: I) -> RetentionSignals
where
    I: IntoIterator<Item = ConsumerUse>,
{
    let mut saw = false;
    let mut any_sup = false;
    for u in uses {
        saw = true;
        if !u.done && !u.superseded {
            return RetentionSignals::default();
        }
        any_sup |= u.superseded;
    }
    if !saw {
        return RetentionSignals::default();
    }
    if any_sup {
        RetentionSignals {
            superseded: true,
            workspace_done: false,
        }
    } else {
        RetentionSignals {
            superseded: false,
            workspace_done: true,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConsumerUse {
    pub done: bool,
    pub superseded: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pressure {
    pub under_pressure: bool,
    pub count_exceeded: bool,
    pub bytes_exceeded: bool,
    pub free_space_exceeded: bool,
    pub bytes_unknown: bool,
}
impl Pressure {
    pub fn assess(
        count: usize,
        bytes: i128,
        free: i128,
        count_ceiling: usize,
        bytes_ceiling: i128,
        min_free: i128,
        bytes_measured: bool,
    ) -> Self {
        let count_exceeded = count > count_ceiling;
        let bytes_exceeded = bytes_measured && bytes > bytes_ceiling;
        let free_space_exceeded = free < min_free;
        Self {
            under_pressure: count_exceeded || bytes_exceeded || free_space_exceeded,
            count_exceeded,
            bytes_exceeded,
            free_space_exceeded,
            bytes_unknown: !bytes_measured,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetentionConfig {
    pub container_idle_stop: f64,
    pub container_remove: f64,
    pub warm_volume_ttl: f64,
    pub superseded_cap: f64,
    pub quiet_period: f64,
}
impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            container_idle_stop: 3600.0,
            container_remove: 86400.0,
            warm_volume_ttl: 259200.0,
            superseded_cap: 86400.0,
            quiet_period: 86400.0,
        }
    }
}
impl RetentionConfig {
    fn valid(self) -> bool {
        [
            self.container_idle_stop,
            self.container_remove,
            self.warm_volume_ttl,
            self.superseded_cap,
            self.quiet_period,
        ]
        .iter()
        .all(|x| finite(*x) && *x >= 0.0)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct EvaluationInput {
    pub now: f64,
    pub resource: ResourceSnapshot,
    pub leases: Vec<ObservedLease>,
    pub signals: RetentionSignals,
    pub pressure: Pressure,
    pub config: RetentionConfig,
    pub running_containers: Option<Vec<String>>,
}
/// Whether a container may be stopped for idleness. This is deliberately separate from
/// collection: caller time and configuration are required and malformed timing is protected.
pub fn container_should_stop(
    resource: &ResourceSnapshot,
    now: f64,
    config: RetentionConfig,
) -> bool {
    if resource.kind != ResourceKind::Container
        || !finite(now)
        || !finite(resource.last_used)
        || !finite(config.container_idle_stop)
        || config.container_idle_stop < 0.0
    {
        return false;
    }
    let age = now - resource.last_used;
    finite(age) && age >= 0.0 && age >= config.container_idle_stop
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerdictReason {
    KeptLeased,
    KeptPinned,
    KeptRunning,
    KeptQuietPeriod,
    KeptCurrentImage,
    KeptWarm,
    KeptMachineScope,
    CollectSuperseded,
    CollectSupersededImage,
    CollectIdle,
    CollectDone,
    CollectPressure,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub resource: ResourceSnapshot,
    pub collect: bool,
    pub reason: VerdictReason,
}
impl Verdict {
    pub fn collect(resource: ResourceSnapshot, reason: VerdictReason) -> Self {
        Self {
            resource,
            collect: true,
            reason,
        }
    }
}
fn keep(i: &EvaluationInput, r: VerdictReason) -> Verdict {
    Verdict {
        resource: i.resource.clone(),
        collect: false,
        reason: r,
    }
}
fn take(i: &EvaluationInput, r: VerdictReason) -> Verdict {
    Verdict::collect(i.resource.clone(), r)
}
pub fn evaluate(i: &EvaluationInput) -> Verdict {
    if !finite(i.now) || !finite(i.resource.last_used) || !i.config.valid() {
        return keep(i, VerdictReason::KeptWarm);
    }
    if i.leases
        .iter()
        .any(|observed| !lease_expired(&observed.lease, i.now, observed.liveness))
    {
        return keep(i, VerdictReason::KeptLeased);
    }
    if i.resource.retention == Retention::Pinned {
        return keep(i, VerdictReason::KeptPinned);
    }
    let age = i.now - i.resource.last_used;
    if !finite(age) || age < 0.0 {
        return keep(i, VerdictReason::KeptWarm);
    }
    let running = i.resource.kind == ResourceKind::Container
        && match &i.running_containers {
            None => true,
            Some(names) => names.contains(&i.resource.name),
        };
    if running && !i.signals.workspace_done && !i.signals.superseded {
        return keep(i, VerdictReason::KeptRunning);
    }
    if i.resource.state == ResourceState::Adopted && age < i.config.quiet_period {
        return keep(i, VerdictReason::KeptQuietPeriod);
    }
    if i.signals.superseded {
        return if i.resource.kind == ResourceKind::Image {
            take(i, VerdictReason::CollectSupersededImage)
        } else if age >= i.config.superseded_cap {
            take(i, VerdictReason::CollectSuperseded)
        } else {
            keep(i, VerdictReason::KeptWarm)
        };
    }
    if i.signals.workspace_done && i.resource.scope != Scope::Machine {
        return take(i, VerdictReason::CollectDone);
    }
    if i.resource.kind == ResourceKind::Image {
        return keep(i, VerdictReason::KeptCurrentImage);
    }
    if i.pressure.under_pressure && i.resource.scope != Scope::Machine {
        return take(i, VerdictReason::CollectPressure);
    }
    if i.resource.scope == Scope::Machine && !i.pressure.under_pressure {
        return keep(i, VerdictReason::KeptMachineScope);
    }
    let ttl = match i.resource.kind {
        ResourceKind::Container | ResourceKind::Network => i.config.container_remove,
        _ => i.config.warm_volume_ttl,
    };
    if age >= ttl {
        take(i, VerdictReason::CollectIdle)
    } else {
        keep(i, VerdictReason::KeptWarm)
    }
}
pub fn collectable_ordered(mut v: Vec<Verdict>) -> Vec<Verdict> {
    v.retain(|x| x.collect);
    v.sort_by(|a, b| {
        let rank = |r| match r {
            VerdictReason::CollectSupersededImage => 0,
            VerdictReason::CollectSuperseded => 1,
            VerdictReason::CollectDone => 2,
            VerdictReason::CollectIdle => 3,
            VerdictReason::CollectPressure => 4,
            _ => 99,
        };
        (a.resource.scope == Scope::Machine)
            .cmp(&(b.resource.scope == Scope::Machine))
            .then_with(|| rank(a.reason).cmp(&rank(b.reason)))
            // `total_cmp` makes malformed snapshot data deterministic without allowing it
            // to panic or produce a non-total sort comparator.
            .then_with(|| a.resource.last_used.total_cmp(&b.resource.last_used))
    });
    v
}
