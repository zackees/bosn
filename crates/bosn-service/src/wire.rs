//! Protobuf wire types for daemon requests and replies, and reply decoding.

use super::*;

#[derive(Message)]
pub(crate) struct ReplyWire {
    #[prost(uint32, tag = "1")]
    pub(crate) code: u32,
    #[prost(string, tag = "2")]
    pub(crate) registry_id: String,
    #[prost(uint32, tag = "3")]
    pub(crate) schema_version: u32,
    #[prost(uint64, tag = "4")]
    pub(crate) resources: u64,
    #[prost(uint64, tag = "5")]
    pub(crate) leases: u64,
    #[prost(uint64, tag = "6")]
    pub(crate) sessions: u64,
    #[prost(bool, tag = "7")]
    pub(crate) reconciliation_required: bool,
    #[prost(uint64, tag = "8")]
    pub(crate) job_id: u64,
    #[prost(string, tag = "9")]
    pub(crate) job_state: String,
    #[prost(string, tag = "10")]
    pub(crate) job_error: String,
    #[prost(message, repeated, tag = "11")]
    pub(crate) logs: Vec<LogRecordWire>,
    #[prost(uint64, tag = "12")]
    pub(crate) retained_from: u64,
    #[prost(uint64, tag = "13")]
    pub(crate) next_log_cursor: u64,
    #[prost(bool, tag = "14")]
    pub(crate) log_gap: bool,
    #[prost(uint64, tag = "15")]
    pub(crate) diagnostic_next: u64,
    #[prost(bool, tag = "16")]
    pub(crate) diagnostic_has_next: bool,
    #[prost(message, repeated, tag = "17")]
    pub(crate) resources_diagnostic: Vec<ResourceDiagnosticWire>,
    #[prost(message, repeated, tag = "18")]
    pub(crate) setup_ensure_events: Vec<SetupEnsureEventWire>,
    #[prost(string, tag = "19")]
    pub(crate) doctor_daemon: String,
    #[prost(string, tag = "20")]
    pub(crate) doctor_registry: String,
    #[prost(string, tag = "21")]
    pub(crate) doctor_engine: String,
    #[prost(string, tag = "22")]
    pub(crate) doctor_client_version: String,
    #[prost(string, tag = "23")]
    pub(crate) doctor_server_version: String,
    #[prost(message, repeated, tag = "24")]
    pub(crate) setup_gc_candidates: Vec<SetupGcCandidateWire>,
    #[prost(uint64, tag = "25")]
    pub(crate) gc_protected_not_retired: u64,
    #[prost(uint64, tag = "26")]
    pub(crate) gc_protected_ambiguous_use: u64,
    #[prost(uint64, tag = "27")]
    pub(crate) gc_protected_lease: u64,
    #[prost(uint64, tag = "28")]
    pub(crate) gc_protected_session: u64,
    #[prost(uint64, tag = "29")]
    pub(crate) gc_excluded_unmanaged: u64,
    #[prost(bool, tag = "30")]
    pub(crate) gc_removed: bool,
    #[prost(bool, tag = "31")]
    pub(crate) gc_reconciled_missing: bool,
    #[prost(uint64, tag = "32")]
    pub(crate) setup_done_uses: u64,
    #[prost(uint64, tag = "33")]
    pub(crate) setup_done_resources: u64,
    #[prost(bool, tag = "34")]
    pub(crate) setup_adopted: bool,
    #[prost(bool, tag = "35")]
    pub(crate) setup_retired_stopped: bool,
    #[prost(bool, tag = "36")]
    pub(crate) setup_retired_already_stopped: bool,
    #[prost(message, repeated, tag = "37")]
    pub(crate) setup_reconcile_records: Vec<SetupReconcileRecordWire>,
    #[prost(bool, tag = "38")]
    pub(crate) setup_reconcile_repaired: bool,
    #[prost(bool, tag = "39")]
    pub(crate) setup_reconcile_already_repaired: bool,
    #[prost(message, repeated, tag = "40")]
    pub(crate) manifest_volume_gc_candidates: Vec<ManifestVolumeGcCandidateWire>,
    #[prost(uint64, tag = "41")]
    pub(crate) volume_gc_protected_not_retired: u64,
    #[prost(uint64, tag = "42")]
    pub(crate) volume_gc_protected_policy: u64,
    #[prost(uint64, tag = "43")]
    pub(crate) volume_gc_protected_ambiguous_use: u64,
    #[prost(uint64, tag = "44")]
    pub(crate) volume_gc_protected_lease: u64,
    #[prost(uint64, tag = "45")]
    pub(crate) volume_gc_protected_session: u64,
    #[prost(uint64, tag = "46")]
    pub(crate) volume_gc_protected_intent: u64,
    #[prost(uint64, tag = "47")]
    pub(crate) volume_gc_excluded_unmanaged: u64,
    #[prost(bool, tag = "48")]
    pub(crate) volume_gc_removed: bool,
    #[prost(bool, tag = "49")]
    pub(crate) volume_gc_reconciled_missing: bool,
    #[prost(uint64, tag = "50")]
    pub(crate) unmanaged_removed: u64,
    #[prost(int64, tag = "51")]
    pub(crate) unmanaged_removed_bytes: i64,
    #[prost(uint64, tag = "52")]
    pub(crate) unmanaged_failed: u64,
    #[prost(string, repeated, tag = "53")]
    pub(crate) unmanaged_failures: Vec<String>,
    #[prost(string, tag = "54")]
    pub(crate) unmanaged_refused: String,
    #[prost(uint64, tag = "55")]
    pub(crate) unmanaged_planned: u64,
    /// The daemon's release version, on a ping reply only. Empty from a
    /// daemon that predates the version handshake (bosn 0.1.5 and older).
    #[prost(string, tag = "56")]
    pub(crate) daemon_version: String,
    /// Codes 360/361: a JSON CI reply, or a JSON `{code, message}` CI error.
    #[prost(string, tag = "57")]
    pub(crate) ci_reply: String,
    /// Code 220, `bosn jobs` (#358): the accounting view as a JSON document.
    #[prost(string, tag = "58")]
    pub(crate) jobs_json: String,
    /// Code 230, `bosn gc owned` (#456): what the managed-retention pass did or would do.
    #[prost(bool, tag = "59")]
    pub(crate) owned_applied: bool,
    #[prost(uint64, tag = "60")]
    pub(crate) owned_planned: u64,
    #[prost(uint64, tag = "61")]
    pub(crate) owned_removed: u64,
    #[prost(int64, tag = "62")]
    pub(crate) owned_removed_bytes: i64,
    #[prost(uint64, tag = "63")]
    pub(crate) owned_deferred: u64,
    #[prost(uint64, tag = "64")]
    pub(crate) owned_failed: u64,
    #[prost(string, repeated, tag = "65")]
    pub(crate) owned_failures: Vec<String>,
    #[prost(string, tag = "66")]
    pub(crate) owned_refused: String,
}
#[derive(Message)]
pub(crate) struct LogRecordWire {
    #[prost(uint64, tag = "1")]
    pub(crate) cursor: u64,
    #[prost(string, tag = "2")]
    pub(crate) line: String,
}
#[derive(Message)]
pub(crate) struct ResourceDiagnosticWire {
    #[prost(string, tag = "1")]
    pub(crate) id: String,
    #[prost(string, tag = "2")]
    pub(crate) kind: String,
    #[prost(string, tag = "3")]
    pub(crate) name: String,
    #[prost(string, tag = "4")]
    pub(crate) stack: String,
    #[prost(string, tag = "5")]
    pub(crate) generation: String,
    #[prost(string, tag = "6")]
    pub(crate) state: String,
    #[prost(string, tag = "7")]
    pub(crate) retention: String,
    #[prost(double, tag = "8")]
    pub(crate) created_at: f64,
    #[prost(double, tag = "9")]
    pub(crate) last_used: f64,
}
impl From<RegistryResourceDiagnostic> for ResourceDiagnosticWire {
    fn from(value: RegistryResourceDiagnostic) -> Self {
        Self {
            id: value.id,
            kind: value.kind,
            name: value.name,
            stack: value.stack,
            generation: value.generation,
            state: value.state,
            retention: value.retention,
            created_at: value.created_at,
            last_used: value.last_used,
        }
    }
}
impl From<ResourceDiagnosticWire> for RegistryResourceDiagnostic {
    fn from(value: ResourceDiagnosticWire) -> Self {
        Self {
            id: value.id,
            kind: value.kind,
            name: value.name,
            stack: value.stack,
            generation: value.generation,
            state: value.state,
            retention: value.retention,
            created_at: value.created_at,
            last_used: value.last_used,
        }
    }
}
#[derive(Message)]
pub(crate) struct SetupGcCandidateWire {
    #[prost(string, tag = "1")]
    pub(crate) id: String,
    #[prost(string, tag = "2")]
    pub(crate) name: String,
    #[prost(string, tag = "3")]
    pub(crate) generation: String,
    #[prost(string, tag = "4")]
    pub(crate) reason: String,
    #[prost(string, tag = "5")]
    pub(crate) token: String,
}
impl From<SetupGcCandidateDiagnostic> for SetupGcCandidateWire {
    fn from(value: SetupGcCandidateDiagnostic) -> Self {
        Self {
            id: value.id,
            name: value.name,
            generation: value.generation,
            reason: value.reason,
            token: value.token,
        }
    }
}
impl From<SetupGcCandidateWire> for SetupGcCandidateDiagnostic {
    fn from(value: SetupGcCandidateWire) -> Self {
        Self {
            id: value.id,
            name: value.name,
            generation: value.generation,
            reason: value.reason,
            token: value.token,
        }
    }
}
#[derive(Message)]
pub(crate) struct ManifestVolumeGcCandidateWire {
    #[prost(string, tag = "1")]
    pub(crate) id: String,
    #[prost(string, tag = "2")]
    pub(crate) name: String,
    #[prost(string, tag = "3")]
    pub(crate) generation: String,
    #[prost(string, tag = "4")]
    pub(crate) reason: String,
    #[prost(string, tag = "5")]
    pub(crate) token: String,
}
impl From<ManifestVolumeGcCandidateDiagnostic> for ManifestVolumeGcCandidateWire {
    fn from(v: ManifestVolumeGcCandidateDiagnostic) -> Self {
        Self {
            id: v.id,
            name: v.name,
            generation: v.generation,
            reason: v.reason,
            token: v.token,
        }
    }
}
impl From<ManifestVolumeGcCandidateWire> for ManifestVolumeGcCandidateDiagnostic {
    fn from(v: ManifestVolumeGcCandidateWire) -> Self {
        Self {
            id: v.id,
            name: v.name,
            generation: v.generation,
            reason: v.reason,
            token: v.token,
        }
    }
}
#[derive(Message)]
pub(crate) struct SetupEnsureEventWire {
    #[prost(uint64, tag = "1")]
    pub(crate) cursor: u64,
    #[prost(double, tag = "2")]
    pub(crate) at: f64,
    #[prost(string, tag = "3")]
    pub(crate) kind: String,
    #[prost(string, tag = "4")]
    pub(crate) detail: String,
}
impl From<SetupEnsureEventDiagnostic> for SetupEnsureEventWire {
    fn from(value: SetupEnsureEventDiagnostic) -> Self {
        Self {
            cursor: value.cursor,
            at: value.at,
            kind: value.kind,
            detail: value.detail,
        }
    }
}
impl From<SetupEnsureEventWire> for SetupEnsureEventDiagnostic {
    fn from(value: SetupEnsureEventWire) -> Self {
        Self {
            cursor: value.cursor,
            at: value.at,
            kind: value.kind,
            detail: value.detail,
        }
    }
}
#[derive(Message)]
pub(crate) struct SetupReconcileRecordWire {
    #[prost(string, tag = "1")]
    pub(crate) id: String,
    #[prost(string, tag = "2")]
    pub(crate) name: String,
    #[prost(string, tag = "3")]
    pub(crate) generation: String,
    #[prost(string, tag = "4")]
    pub(crate) drift: String,
    #[prost(string, tag = "5")]
    pub(crate) repair_token: String,
}
impl From<SetupReconcileRecord> for SetupReconcileRecordWire {
    fn from(value: SetupReconcileRecord) -> Self {
        Self {
            id: value.id,
            name: value.name,
            generation: value.generation,
            drift: value.drift,
            repair_token: value.repair_token.unwrap_or_default(),
        }
    }
}
impl From<SetupReconcileRecordWire> for SetupReconcileRecord {
    fn from(value: SetupReconcileRecordWire) -> Self {
        Self {
            id: value.id,
            name: value.name,
            generation: value.generation,
            drift: value.drift,
            repair_token: (!value.repair_token.is_empty()).then_some(value.repair_token),
        }
    }
}
pub(crate) enum Reply {
    Pong(String),
    Status(Status),
    Shutdown,
    Job(u64),
    JobStatus(JobStatus),
    Cancelled,
    JobLogs(JobLogPage),
    RegistryResources(RegistryResourcePage),
    SetupEnsureEvents(SetupEnsureEventPage),
    SetupGcPreview(SetupGcPreviewPage),
    SetupGcApply(SetupGcApplyResult),
    SetupRetiredStop(SetupRetiredStopResult),
    SetupDone(SetupDoneResult),
    SetupAdopt(SetupAdoptResult),
    Doctor(DoctorReport),
    SetupReconcilePreview(SetupReconcilePreviewPage),
    SetupReconcileMissingRepair(SetupReconcileMissingRepairResult),
    ManifestVolumeGcPreview(ManifestVolumeGcPreviewPage),
    ManifestVolumeGcApply(ManifestVolumeGcApplyResult),
    UnmanagedApply(UnmanagedApplySummary),
    ManagedRetention(ManagedRetentionSummary),
    Ci(String),
    CiError(String),
    Jobs(String),
}
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
pub(crate) fn decode_reply(v: ReplyWire) -> Result<Reply, Error> {
    match v.code {
        10 => Ok(Reply::Pong(v.daemon_version)),
        360 => Ok(Reply::Ci(v.ci_reply)),
        361 => Ok(Reply::CiError(v.ci_reply)),
        20 => Ok(Reply::Status(Status {
            registry_id: v.registry_id,
            schema_version: v.schema_version,
            resources: v.resources,
            leases: v.leases,
            sessions: v.sessions,
            reconciliation_required: v.reconciliation_required,
        })),
        30 => Ok(Reply::Shutdown),
        40 => Ok(Reply::Job(v.job_id)),
        50 => Ok(Reply::JobStatus(JobStatus {
            id: v.job_id,
            state: v.job_state,
            error: (!v.job_error.is_empty()).then_some(v.job_error),
        })),
        60 => Ok(Reply::Cancelled),
        70 => Ok(Reply::JobLogs(JobLogPage {
            retained_from: v.retained_from,
            next: v.next_log_cursor,
            gap: v.log_gap,
            records: v
                .logs
                .into_iter()
                .map(|record| JobLogRecord {
                    cursor: record.cursor,
                    line: record.line,
                })
                .collect(),
        })),
        80 => Ok(Reply::RegistryResources(RegistryResourcePage {
            next: v.diagnostic_has_next.then_some(v.diagnostic_next),
            records: v.resources_diagnostic.into_iter().map(Into::into).collect(),
        })),
        90 => Ok(Reply::SetupEnsureEvents(SetupEnsureEventPage {
            next: v.diagnostic_has_next.then_some(v.diagnostic_next),
            records: v.setup_ensure_events.into_iter().map(Into::into).collect(),
        })),
        100 => Ok(Reply::Doctor(DoctorReport {
            daemon: v.doctor_daemon,
            registry: v.doctor_registry,
            engine: v.doctor_engine,
            client_version: (!v.doctor_client_version.is_empty())
                .then_some(v.doctor_client_version),
            server_version: (!v.doctor_server_version.is_empty())
                .then_some(v.doctor_server_version),
        })),
        110 => Ok(Reply::SetupGcPreview(SetupGcPreviewPage {
            next: v.diagnostic_has_next.then_some(v.diagnostic_next),
            candidates: v.setup_gc_candidates.into_iter().map(Into::into).collect(),
            counts: SetupGcPreviewCounts {
                protected_not_retired: v.gc_protected_not_retired,
                protected_ambiguous_use: v.gc_protected_ambiguous_use,
                protected_lease: v.gc_protected_lease,
                protected_session: v.gc_protected_session,
                excluded_unmanaged: v.gc_excluded_unmanaged,
            },
        })),
        120 => Ok(Reply::SetupGcApply(SetupGcApplyResult {
            removed: v.gc_removed,
            reconciled_missing: v.gc_reconciled_missing,
        })),
        130 => Ok(Reply::SetupDone(SetupDoneResult {
            uses_completed: v.setup_done_uses,
            resources_completed: v.setup_done_resources,
        })),
        140 => Ok(Reply::SetupAdopt(SetupAdoptResult {
            adopted: v.setup_adopted,
        })),
        150 => Ok(Reply::SetupRetiredStop(SetupRetiredStopResult {
            stopped: v.setup_retired_stopped,
            already_stopped: v.setup_retired_already_stopped,
        })),
        160 => Ok(Reply::SetupReconcilePreview(SetupReconcilePreviewPage {
            next: v.diagnostic_has_next.then_some(v.diagnostic_next),
            records: v
                .setup_reconcile_records
                .into_iter()
                .map(Into::into)
                .collect(),
        })),
        170 => Ok(Reply::SetupReconcileMissingRepair(
            SetupReconcileMissingRepairResult {
                repaired: v.setup_reconcile_repaired,
                already_repaired: v.setup_reconcile_already_repaired,
            },
        )),
        180 => Ok(Reply::ManifestVolumeGcPreview(
            ManifestVolumeGcPreviewPage {
                next: v.diagnostic_has_next.then_some(v.diagnostic_next),
                candidates: v
                    .manifest_volume_gc_candidates
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                counts: ManifestVolumeGcPreviewCounts {
                    protected_not_retired: v.volume_gc_protected_not_retired,
                    protected_policy: v.volume_gc_protected_policy,
                    protected_ambiguous_use: v.volume_gc_protected_ambiguous_use,
                    protected_lease: v.volume_gc_protected_lease,
                    protected_session: v.volume_gc_protected_session,
                    protected_intent: v.volume_gc_protected_intent,
                    excluded_unmanaged: v.volume_gc_excluded_unmanaged,
                },
            },
        )),
        190 => Ok(Reply::ManifestVolumeGcApply(ManifestVolumeGcApplyResult {
            removed: v.volume_gc_removed,
            reconciled_missing: v.volume_gc_reconciled_missing,
        })),
        200 => Ok(Reply::UnmanagedApply(UnmanagedApplySummary {
            planned: v.unmanaged_planned,
            removed: v.unmanaged_removed,
            removed_bytes: i128::from(v.unmanaged_removed_bytes),
            failed: v.unmanaged_failed,
            failures: v.unmanaged_failures,
            refused: (!v.unmanaged_refused.is_empty()).then_some(v.unmanaged_refused),
        })),
        220 => Ok(Reply::Jobs(v.jobs_json)),
        230 => Ok(Reply::ManagedRetention(ManagedRetentionSummary {
            applied: v.owned_applied,
            planned: v.owned_planned,
            removed: v.owned_removed,
            removed_bytes: i128::from(v.owned_removed_bytes),
            deferred: v.owned_deferred,
            failed: v.owned_failed,
            failures: v.owned_failures,
            refused: (!v.owned_refused.is_empty()).then_some(v.owned_refused),
        })),
        1 => Err(Error::Protocol("unsupported protocol")),
        2 => Err(Error::Protocol("unknown operation")),
        _ => Err(Error::Protocol("daemon error")),
    }
}

#[derive(Message)]
pub(crate) struct Request {
    #[prost(uint32, tag = "1")]
    pub(crate) protocol_version: u32,
    #[prost(uint32, tag = "2")]
    pub(crate) operation: u32,
    #[prost(string, tag = "3")]
    pub(crate) workspace: String,
    #[prost(string, tag = "4")]
    pub(crate) stack: String,
    #[prost(string, tag = "5")]
    pub(crate) digest: String,
    #[prost(uint64, tag = "6")]
    pub(crate) job_id: u64,
    #[prost(uint64, tag = "7")]
    pub(crate) log_after: u64,
    #[prost(uint32, tag = "8")]
    pub(crate) log_limit: u32,
    #[prost(string, tag = "9")]
    pub(crate) setup_config: String,
    #[prost(uint32, tag = "10")]
    pub(crate) setup_policy: u32,
    #[prost(uint64, tag = "11")]
    pub(crate) setup_deadline_ms: u64,
    #[prost(uint32, tag = "12")]
    pub(crate) setup_output_limit: u32,
    #[prost(string, tag = "13")]
    pub(crate) setup_task_name: String,
    #[prost(uint64, tag = "14")]
    pub(crate) diagnostic_after: u64,
    #[prost(uint32, tag = "15")]
    pub(crate) diagnostic_limit: u32,
    #[prost(string, tag = "16")]
    pub(crate) gc_candidate_token: String,
    #[prost(bool, tag = "17")]
    pub(crate) gc_confirm: bool,
    #[prost(bool, tag = "18")]
    pub(crate) setup_done_confirm: bool,
    #[prost(bool, tag = "19")]
    pub(crate) setup_adopt_confirm: bool,
    #[prost(string, repeated, tag = "20")]
    pub(crate) unmanaged_include: Vec<String>,
    #[prost(uint64, tag = "21")]
    pub(crate) unmanaged_ttl_seconds: u64,
    /// Operation 36: one JSON-encoded [`ci::CiRequest`].
    #[prost(string, tag = "22")]
    pub(crate) ci_request: String,
    /// Manifest app task only: cancel the job once its follower has not
    /// polled for this many milliseconds (#357). Zero means no lease.
    #[prost(uint64, tag = "23")]
    pub(crate) follow_lease_ms: u64,
    /// Operation 38, `bosn gc owned` (#456): the per-kind age gates in seconds.
    #[prost(uint64, tag = "24")]
    pub(crate) owned_container_ttl_secs: u64,
    #[prost(uint64, tag = "25")]
    pub(crate) owned_volume_ttl_secs: u64,
    #[prost(uint64, tag = "26")]
    pub(crate) owned_image_ttl_secs: u64,
    /// Zero means no byte ceiling for this pass.
    #[prost(int64, tag = "27")]
    pub(crate) owned_max_bytes: i64,
    /// Set only together with `gc_confirm`: this is a destructive pass, not a preview.
    #[prost(bool, tag = "28")]
    pub(crate) owned_confirm: bool,
}
impl Request {
    pub(crate) fn operation(operation: u32) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            operation,
            workspace: String::new(),
            stack: String::new(),
            digest: String::new(),
            job_id: 0,
            log_after: 0,
            log_limit: 0,
            setup_config: String::new(),
            setup_policy: 0,
            setup_deadline_ms: 0,
            setup_output_limit: 0,
            setup_task_name: String::new(),
            diagnostic_after: 0,
            diagnostic_limit: 0,
            gc_candidate_token: String::new(),
            gc_confirm: false,
            setup_done_confirm: false,
            setup_adopt_confirm: false,
            unmanaged_include: Vec::new(),
            unmanaged_ttl_seconds: 0,
            ci_request: String::new(),
            follow_lease_ms: 0,
            owned_container_ttl_secs: 0,
            owned_volume_ttl_secs: 0,
            owned_image_ttl_secs: 0,
            owned_max_bytes: 0,
            owned_confirm: false,
        }
    }
}
