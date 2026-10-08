//! Diagnostic, GC-preview and repair result types, and their opaque tokens.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub registry_id: String,
    pub schema_version: u32,
    pub resources: u64,
    pub leases: u64,
    pub sessions: u64,
    pub reconciliation_required: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStatus {
    pub id: u64,
    pub state: String,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobLogRecord {
    pub cursor: u64,
    pub line: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobLogPage {
    pub retained_from: u64,
    pub next: u64,
    pub gap: bool,
    pub records: Vec<JobLogRecord>,
}

/// One credential- and path-safe registry resource diagnostic.  This is not a
/// raw registry row: workspace and scope bindings remain local registry
/// implementation details, while these stable facts identify managed state.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryResourceDiagnostic {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub stack: String,
    pub generation: String,
    pub state: String,
    pub retention: String,
    pub created_at: f64,
    pub last_used: f64,
}

/// Bounded offset-cursor page of safe managed-resource diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryResourcePage {
    pub next: Option<u64>,
    pub records: Vec<RegistryResourceDiagnostic>,
}

/// One safe, logical future-GC candidate. This is preview metadata only: it
/// is never an engine identifier and cannot be used to request deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupGcCandidateDiagnostic {
    pub id: String,
    pub name: String,
    pub generation: String,
    /// Opaque, preview-derived binding. Apply accepts only this complete token
    /// plus the exact workspace; it never accepts a Docker name or selector.
    pub token: String,
    pub reason: String,
}
/// Result of one deliberate setup-GC apply.  `reconciled_missing` means Docker
/// reported the exact previously-owned container absent and the daemon removed
/// only its still-eligible registry record; it never means a broad prune ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupGcApplyResult {
    pub removed: bool,
    pub reconciled_missing: bool,
}
/// Outcome of one `gc owned` pass, preview or applied.
///
/// Every count here is measured by the daemon from its own re-derived plan. The caller's
/// preview is never trusted: the daemon rebuilds the plan immediately before removing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedRetentionSummary {
    /// Protection decisions across stages, including registry-level refusals.
    /// An object can contribute more than once when re-observed in later stages.
    pub held_total: u64,
    /// Bounded descriptions of objects protected from this pass.
    pub held: Vec<String>,
    /// False for a preview. A preview never mutates the engine.
    pub applied: bool,
    /// Reclaimable objects the re-derived plan selected, before any removal.
    pub planned: u64,
    /// Objects actually removed. Zero for a preview.
    pub removed: u64,
    /// Measured after the pass completed, not predicted before it.
    pub removed_bytes: i128,
    /// Reclaimable objects left for a later pass because a cap was reached.
    pub deferred: u64,
    /// Removals the engine refused or failed. Each names the exact object.
    pub failed: u64,
    pub failures: Vec<String>,
    /// Set when the pass refused to remove anything.
    pub refused: Option<String>,
}

/// Outcome of one `gc --unmanaged --apply`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmanagedApplySummary {
    /// Candidates the daemon re-derived immediately before removing anything.
    pub planned: u64,
    pub removed: u64,
    /// Measured after the pass completed, not predicted before it.
    pub removed_bytes: i128,
    pub failed: u64,
    pub failures: Vec<String>,
    /// Set when the pass refused to remove anything.
    pub refused: Option<String>,
}

/// Result of stopping one exact retired setup generation.  An already-stopped
/// exact candidate is intentionally idempotent: no Docker mutation or event
/// write occurs, and its retired registry record remains for GC apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupRetiredStopResult {
    pub stopped: bool,
    pub already_stopped: bool,
}
/// Result of an explicit, registry-only setup workspace completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupDoneResult {
    pub uses_completed: u64,
    pub resources_completed: u64,
}
impl From<SetupDone> for SetupDoneResult {
    fn from(value: SetupDone) -> Self {
        Self {
            uses_completed: value.uses_completed,
            resources_completed: value.resources_completed,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupGcPreviewCounts {
    pub protected_not_retired: u64,
    pub protected_ambiguous_use: u64,
    pub protected_lease: u64,
    pub protected_session: u64,
    pub excluded_unmanaged: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupGcPreviewPage {
    pub next: Option<u64>,
    pub candidates: Vec<SetupGcCandidateDiagnostic>,
    pub counts: SetupGcPreviewCounts,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestVolumeGcCandidateDiagnostic {
    pub id: String,
    pub name: String,
    pub generation: String,
    pub token: String,
    pub reason: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestVolumeGcPreviewCounts {
    pub protected_not_retired: u64,
    pub protected_policy: u64,
    pub protected_ambiguous_use: u64,
    pub protected_lease: u64,
    pub protected_session: u64,
    pub protected_intent: u64,
    pub excluded_unmanaged: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestVolumeGcPreviewPage {
    pub next: Option<u64>,
    pub candidates: Vec<ManifestVolumeGcCandidateDiagnostic>,
    pub counts: ManifestVolumeGcPreviewCounts,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestVolumeGcApplyResult {
    pub removed: bool,
    pub reconciled_missing: bool,
}

/// A bounded, read-only comparison between one durable managed-container
/// record and Docker. It intentionally contains no workspace, URL, engine
/// output, or repair token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupReconcileRecord {
    pub id: String,
    pub name: String,
    pub generation: String,
    /// `matching_running`, `matching_stopped`, `missing`, `name_mismatch`,
    /// `label_mismatch`, `image_mismatch`, `inspect_error`, or `unknown`.
    /// Unknown is conservative and must never be treated as a
    /// repair/GC candidate.
    pub drift: String,
    /// Present only for a previewed, currently repairable `missing` record.
    /// This opaque binding is never a Docker identifier and is revalidated by
    /// the daemon immediately before its registry-only transition.
    pub repair_token: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupReconcilePreviewPage {
    pub next: Option<u64>,
    pub records: Vec<SetupReconcileRecord>,
}
/// Result of confirmation-gated repair of one previewed missing setup app.
/// The repair changes durable registry lifecycle state only; it never mutates
/// Docker. `already_repaired` makes a repeated exact token safely idempotent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupReconcileMissingRepairResult {
    pub repaired: bool,
    pub already_repaired: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct SetupReconcileCandidate {
    pub(crate) resource: Resource,
    pub(crate) image_identities: Vec<String>,
    pub(crate) missing_repairable: bool,
}
pub(crate) type SetupReconcileCandidates = (Option<u64>, Vec<SetupReconcileCandidate>);

pub(crate) fn classify_setup_reconcile(
    candidate: &SetupReconcileCandidate,
    inspected: Result<Option<SetupReconcileObserved>, String>,
) -> &'static str {
    match (
        candidate.resource.generation.strip_prefix("sha256:"),
        inspected,
    ) {
        (_, Ok(None)) => "missing",
        (Some(_), Ok(Some(observed)))
            if observed.name != format!("/{}", candidate.resource.name) =>
        {
            "name_mismatch"
        }
        (Some(content), Ok(Some(observed)))
            if observed.managed != "v1"
                || observed.content != content
                || observed.container != candidate.resource.name =>
        {
            "label_mismatch"
        }
        (Some(_), Ok(Some(observed)))
            if !candidate
                .image_identities
                .iter()
                .any(|image| image == &observed.image_identity) =>
        {
            "image_mismatch"
        }
        (Some(_), Ok(Some(observed))) => {
            if observed.running {
                "matching_running"
            } else {
                "matching_stopped"
            }
        }
        (_, Err(_)) => "inspect_error",
        _ => "unknown",
    }
}

pub(crate) fn setup_gc_preview_diagnostic(value: SetupGcPreview) -> SetupGcPreviewPage {
    SetupGcPreviewPage {
        next: value.candidates.next_offset.map(|value| value as u64),
        candidates: value
            .candidates
            .items
            .into_iter()
            .map(|candidate| SetupGcCandidateDiagnostic {
                token: setup_gc_token(&candidate),
                id: candidate.id,
                name: candidate.name,
                generation: candidate.generation,
                reason: "retired_managed_setup_container".into(),
            })
            .collect(),
        counts: SetupGcPreviewCounts {
            protected_not_retired: value.counts.protected_not_retired,
            protected_ambiguous_use: value.counts.protected_ambiguous_use,
            protected_lease: value.counts.protected_lease,
            protected_session: value.counts.protected_session,
            excluded_unmanaged: value.counts.excluded_unmanaged,
        },
    }
}

pub(crate) fn manifest_volume_gc_preview_diagnostic(
    value: ManifestVolumeGcPreview,
) -> ManifestVolumeGcPreviewPage {
    ManifestVolumeGcPreviewPage {
        next: value.candidates.next_offset.map(|v| v as u64),
        candidates: value
            .candidates
            .items
            .into_iter()
            .map(|v| ManifestVolumeGcCandidateDiagnostic {
                token: manifest_volume_gc_token(&v),
                id: v.id,
                name: v.name,
                generation: v.generation,
                reason: "retired_manifest_warm_spec_volume".into(),
            })
            .collect(),
        counts: ManifestVolumeGcPreviewCounts {
            protected_not_retired: value.counts.protected_not_retired,
            protected_policy: value.counts.protected_policy,
            protected_ambiguous_use: value.counts.protected_ambiguous_use,
            protected_lease: value.counts.protected_lease,
            protected_session: value.counts.protected_session,
            protected_intent: value.counts.protected_intent,
            excluded_unmanaged: value.counts.excluded_unmanaged,
        },
    }
}

pub(crate) fn setup_gc_token(candidate: &bosn_registry::SetupGcCandidate) -> String {
    // Hex makes a delimiter-free opaque transport value without adding a
    // parser-sensitive dependency. It is an identity binding, not a secret:
    // the actor always revalidates the decoded tuple immediately before any
    // engine operation and again while finalizing registry state.
    let mut bytes = Vec::new();
    for value in [&candidate.id, &candidate.name, &candidate.generation] {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    let mut token = String::from("sgc1-");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}
pub(crate) fn manifest_volume_gc_token(
    candidate: &bosn_registry::ManifestVolumeGcCandidate,
) -> String {
    let mut bytes = Vec::new();
    for value in [&candidate.id, &candidate.name, &candidate.generation] {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    let mut token = String::from("mvg1-");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}
pub(crate) fn manifest_volume_release_token(
    candidate: &bosn_registry::ManifestVolumeGcCandidate,
) -> String {
    let mut bytes = Vec::new();
    for value in [&candidate.id, &candidate.name, &candidate.generation] {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    let mut token = String::from("mvr1-");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}

pub(crate) fn setup_reconcile_missing_token(candidate: &SetupReconcileCandidate) -> String {
    let mut bytes = Vec::new();
    for value in [
        &candidate.resource.id,
        &candidate.resource.name,
        &candidate.resource.generation,
    ] {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    let mut token = String::from("srm1-");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}

pub(crate) fn parse_setup_gc_token(token: &str) -> Result<(String, String, String), Error> {
    let encoded = token
        .strip_prefix("sgc1-")
        .ok_or(Error::Protocol("invalid setup gc candidate"))?;
    if encoded.is_empty() || encoded.len() > 24 * 1024 || encoded.len() % 2 != 0 {
        return Err(Error::Protocol("invalid setup gc candidate"));
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for chunk in encoded.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(chunk)
            .map_err(|_| Error::Protocol("invalid setup gc candidate"))?;
        bytes.push(
            u8::from_str_radix(text, 16)
                .map_err(|_| Error::Protocol("invalid setup gc candidate"))?,
        );
    }
    let mut fields = bytes.split(|value| *value == 0);
    let field = |value: Option<&[u8]>| -> Result<String, Error> {
        let value = value.ok_or(Error::Protocol("invalid setup gc candidate"))?;
        if value.is_empty() || value.len() > 8 * 1024 || value.contains(&0) {
            return Err(Error::Protocol("invalid setup gc candidate"));
        }
        std::str::from_utf8(value)
            .map(str::to_owned)
            .map_err(|_| Error::Protocol("invalid setup gc candidate"))
    };
    let result = (
        field(fields.next())?,
        field(fields.next())?,
        field(fields.next())?,
    );
    if fields.next() != Some(&[]) || fields.next().is_some() {
        return Err(Error::Protocol("invalid setup gc candidate"));
    }
    Ok(result)
}
pub(crate) fn parse_manifest_volume_gc_token(
    token: &str,
) -> Result<(String, String, String), Error> {
    let encoded = token
        .strip_prefix("mvg1-")
        .ok_or(Error::Protocol("invalid manifest volume gc candidate"))?;
    parse_setup_gc_token(&format!("sgc1-{encoded}"))
        .map_err(|_| Error::Protocol("invalid manifest volume gc candidate"))
}
pub(crate) fn parse_manifest_volume_release_token(
    token: &str,
) -> Result<(String, String, String), Error> {
    let encoded = token
        .strip_prefix("mvr1-")
        .ok_or(Error::Protocol("invalid manifest volume release candidate"))?;
    parse_setup_gc_token(&format!("sgc1-{encoded}"))
        .map_err(|_| Error::Protocol("invalid manifest volume release candidate"))
}

pub(crate) fn parse_setup_reconcile_missing_token(
    token: &str,
) -> Result<(String, String, String), Error> {
    let normalized = token
        .strip_prefix("srm1-")
        .ok_or(Error::Protocol("invalid setup reconcile repair candidate"))?;
    parse_setup_gc_token(&format!("sgc1-{normalized}"))
        .map_err(|_| Error::Protocol("invalid setup reconcile repair candidate"))
}

/// One redacted setup-ensure registry event. Event details are authored by the
/// daemon's allowlisted event formatter, not copied from config, engine, or
/// workspace input.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupEnsureEventDiagnostic {
    pub cursor: u64,
    pub at: f64,
    pub kind: String,
    pub detail: String,
}

/// Bounded offset-cursor page of recent setup-ensure history, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupEnsureEventPage {
    pub next: Option<u64>,
    pub records: Vec<SetupEnsureEventDiagnostic>,
}

/// Bounded stable diagnostic result. A daemon-unavailable result is produced
/// by the client without opening a registry; all other fields are authored by
/// the existing daemon after its read-only integrity check and fixed engine
/// probe. No paths, raw process output, endpoint names, or credentials occur.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorReport {
    pub daemon: String,
    pub registry: String,
    pub engine: String,
    pub client_version: Option<String>,
    pub server_version: Option<String>,
}
impl DoctorReport {
    pub(crate) fn daemon_unavailable() -> Self {
        Self {
            daemon: "unavailable".into(),
            registry: "unavailable".into(),
            engine: "unavailable".into(),
            client_version: None,
            server_version: None,
        }
    }
    pub(crate) fn from_engine(registry: &'static str, engine: DockerDoctorReport) -> Self {
        Self {
            daemon: "ready".into(),
            registry: registry.into(),
            engine: match engine.state {
                DockerDoctorState::Ready => "ready",
                DockerDoctorState::Unavailable => "unavailable",
                DockerDoctorState::Deadline => "deadline",
                DockerDoctorState::OutputLimit => "output_limit",
                DockerDoctorState::InvalidResponse => "invalid_response",
            }
            .into(),
            client_version: engine.client_version,
            server_version: engine.server_version,
        }
    }
}

pub(crate) fn resource_diagnostic(value: Resource) -> RegistryResourceDiagnostic {
    RegistryResourceDiagnostic {
        id: value.id,
        kind: value.kind.as_str().into(),
        name: value.name,
        stack: value.stack,
        generation: value.generation,
        state: value.state.as_str().into(),
        retention: value.retention.as_str().into(),
        created_at: value.created_at,
        last_used: value.last_used,
    }
}

pub(crate) fn event_diagnostic(value: Event) -> SetupEnsureEventDiagnostic {
    SetupEnsureEventDiagnostic {
        cursor: u64::try_from(value.id).unwrap_or(0),
        at: value.at,
        kind: value.kind,
        detail: value.detail,
    }
}

impl From<RegistryStatus> for Status {
    fn from(v: RegistryStatus) -> Self {
        Self {
            registry_id: v.registry_id,
            schema_version: v.schema_version,
            resources: v.resources,
            leases: v.leases,
            sessions: v.sessions,
            reconciliation_required: v.reconciliation_required,
        }
    }
}
