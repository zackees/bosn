//! Frozen result types returned to Python.

use super::*;

#[pyclass(module = "bosn._native", frozen)]
pub struct Status {
    #[pyo3(get)]
    pub(crate) registry_id: String,
    #[pyo3(get)]
    pub(crate) schema_version: u32,
    #[pyo3(get)]
    pub(crate) resources: u64,
    #[pyo3(get)]
    pub(crate) leases: u64,
    #[pyo3(get)]
    pub(crate) sessions: u64,
    #[pyo3(get)]
    pub(crate) reconciliation_required: bool,
}

impl From<ServiceStatus> for Status {
    fn from(value: ServiceStatus) -> Self {
        Self {
            registry_id: value.registry_id,
            schema_version: value.schema_version,
            resources: value.resources,
            leases: value.leases,
            sessions: value.sessions,
            reconciliation_required: value.reconciliation_required,
        }
    }
}

/// Stable bounded health result returned by [`Client::doctor`]. Versions are
/// present only when the daemon's fixed Docker version probe succeeded.
#[pyclass(module = "bosn._native", frozen)]
pub struct DoctorReport {
    #[pyo3(get)]
    pub(crate) daemon: String,
    #[pyo3(get)]
    pub(crate) registry: String,
    #[pyo3(get)]
    pub(crate) engine: String,
    #[pyo3(get)]
    pub(crate) client_version: Option<String>,
    #[pyo3(get)]
    pub(crate) server_version: Option<String>,
}
impl From<ServiceDoctorReport> for DoctorReport {
    fn from(value: ServiceDoctorReport) -> Self {
        Self {
            daemon: value.daemon,
            registry: value.registry,
            engine: value.engine,
            client_version: value.client_version,
            server_version: value.server_version,
        }
    }
}

/// Immutable typed status for a daemon job.
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct JobStatus {
    #[pyo3(get)]
    pub(crate) id: u64,
    #[pyo3(get)]
    pub(crate) state: String,
    #[pyo3(get)]
    pub(crate) error: Option<String>,
}

impl From<ServiceJobStatus> for JobStatus {
    fn from(value: ServiceJobStatus) -> Self {
        Self {
            id: value.id,
            state: value.state,
            error: value.error.map(|message| redact_diagnostic(&message)),
        }
    }
}

/// One bounded daemon job-log record.
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct JobLogRecord {
    #[pyo3(get)]
    pub(crate) cursor: u64,
    #[pyo3(get)]
    pub(crate) line: String,
}

/// Immutable cursor page returned by [`Client::job_logs`].
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct JobLogPage {
    #[pyo3(get)]
    pub(crate) retained_from: u64,
    #[pyo3(get)]
    pub(crate) next: u64,
    #[pyo3(get)]
    pub(crate) gap: bool,
    pub(crate) records: Vec<JobLogRecord>,
}

/// A path-safe managed resource diagnostic. Workspace/scope bindings are not
/// exposed by this Python API.
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct RegistryResource {
    #[pyo3(get)]
    pub(crate) id: String,
    #[pyo3(get)]
    pub(crate) kind: String,
    #[pyo3(get)]
    pub(crate) name: String,
    #[pyo3(get)]
    pub(crate) stack: String,
    #[pyo3(get)]
    pub(crate) generation: String,
    #[pyo3(get)]
    pub(crate) state: String,
    #[pyo3(get)]
    pub(crate) retention: String,
    #[pyo3(get)]
    pub(crate) created_at: f64,
    #[pyo3(get)]
    pub(crate) last_used: f64,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct RegistryResourcePage {
    #[pyo3(get)]
    pub(crate) next: Option<u64>,
    pub(crate) records: Vec<RegistryResource>,
}
#[pymethods]
impl RegistryResourcePage {
    #[getter]
    pub(crate) fn records(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
        let values = self
            .records
            .iter()
            .map(|record| {
                Py::new(
                    py,
                    RegistryResource {
                        id: record.id.clone(),
                        kind: record.kind.clone(),
                        name: record.name.clone(),
                        stack: record.stack.clone(),
                        generation: record.generation.clone(),
                        state: record.state.clone(),
                        retention: record.retention.clone(),
                        created_at: record.created_at,
                        last_used: record.last_used,
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        Ok(PyTuple::new(py, values)?.unbind())
    }
}
impl From<ServiceRegistryResourcePage> for RegistryResourcePage {
    fn from(value: ServiceRegistryResourcePage) -> Self {
        Self {
            next: value.next,
            records: value
                .records
                .into_iter()
                .map(|record| RegistryResource {
                    id: record.id,
                    kind: record.kind,
                    name: record.name,
                    stack: record.stack,
                    generation: record.generation,
                    state: record.state,
                    retention: record.retention,
                    created_at: record.created_at,
                    last_used: record.last_used,
                })
                .collect(),
        }
    }
}

/// Safe logical identity returned by a GC preview; not an engine deletion
/// handle. A future apply must obtain and revalidate ownership independently.
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupGcCandidate {
    #[pyo3(get)]
    pub(crate) id: String,
    #[pyo3(get)]
    pub(crate) name: String,
    #[pyo3(get)]
    pub(crate) generation: String,
    #[pyo3(get)]
    pub(crate) token: String,
    #[pyo3(get)]
    pub(crate) reason: String,
}
#[derive(Clone, Debug)]
#[pyclass(module = "bosn._native", frozen, skip_from_py_object)]
pub struct SetupGcPreviewCounts {
    #[pyo3(get)]
    pub(crate) protected_not_retired: u64,
    #[pyo3(get)]
    pub(crate) protected_ambiguous_use: u64,
    #[pyo3(get)]
    pub(crate) protected_lease: u64,
    #[pyo3(get)]
    pub(crate) protected_session: u64,
    #[pyo3(get)]
    pub(crate) excluded_unmanaged: u64,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupGcPreviewPage {
    #[pyo3(get)]
    pub(crate) next: Option<u64>,
    #[pyo3(get)]
    pub(crate) counts: SetupGcPreviewCounts,
    pub(crate) candidates: Vec<SetupGcCandidate>,
}
#[pymethods]
impl SetupGcPreviewPage {
    #[getter]
    pub(crate) fn candidates(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
        Ok(PyTuple::new(
            py,
            self.candidates
                .iter()
                .map(|value| {
                    Py::new(
                        py,
                        SetupGcCandidate {
                            id: value.id.clone(),
                            name: value.name.clone(),
                            generation: value.generation.clone(),
                            token: value.token.clone(),
                            reason: value.reason.clone(),
                        },
                    )
                })
                .collect::<PyResult<Vec<_>>>()?,
        )?
        .unbind())
    }
}
impl From<ServiceSetupGcPreviewPage> for SetupGcPreviewPage {
    fn from(value: ServiceSetupGcPreviewPage) -> Self {
        Self {
            next: value.next,
            candidates: value
                .candidates
                .into_iter()
                .map(|value| SetupGcCandidate {
                    id: value.id,
                    name: value.name,
                    generation: value.generation,
                    token: value.token,
                    reason: value.reason,
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
}

#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct ManifestVolumeGcCandidate {
    #[pyo3(get)]
    pub(crate) id: String,
    #[pyo3(get)]
    pub(crate) name: String,
    #[pyo3(get)]
    pub(crate) generation: String,
    #[pyo3(get)]
    pub(crate) token: String,
    #[pyo3(get)]
    pub(crate) reason: String,
}
#[derive(Clone, Debug)]
#[pyclass(module = "bosn._native", frozen, skip_from_py_object)]
pub struct ManifestVolumeGcPreviewCounts {
    #[pyo3(get)]
    pub(crate) protected_not_retired: u64,
    #[pyo3(get)]
    pub(crate) protected_policy: u64,
    #[pyo3(get)]
    pub(crate) protected_ambiguous_use: u64,
    #[pyo3(get)]
    pub(crate) protected_lease: u64,
    #[pyo3(get)]
    pub(crate) protected_session: u64,
    #[pyo3(get)]
    pub(crate) protected_intent: u64,
    #[pyo3(get)]
    pub(crate) excluded_unmanaged: u64,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct ManifestVolumeGcPreviewPage {
    #[pyo3(get)]
    pub(crate) next: Option<u64>,
    #[pyo3(get)]
    pub(crate) counts: ManifestVolumeGcPreviewCounts,
    pub(crate) candidates: Vec<ManifestVolumeGcCandidate>,
}
#[pymethods]
impl ManifestVolumeGcPreviewPage {
    #[getter]
    pub(crate) fn candidates(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
        Ok(PyTuple::new(
            py,
            self.candidates
                .iter()
                .map(|v| {
                    Py::new(
                        py,
                        ManifestVolumeGcCandidate {
                            id: v.id.clone(),
                            name: v.name.clone(),
                            generation: v.generation.clone(),
                            token: v.token.clone(),
                            reason: v.reason.clone(),
                        },
                    )
                })
                .collect::<PyResult<Vec<_>>>()?,
        )?
        .unbind())
    }
}
impl From<ServiceManifestVolumeGcPreviewPage> for ManifestVolumeGcPreviewPage {
    fn from(v: ServiceManifestVolumeGcPreviewPage) -> Self {
        Self {
            next: v.next,
            candidates: v
                .candidates
                .into_iter()
                .map(|c| ManifestVolumeGcCandidate {
                    id: c.id,
                    name: c.name,
                    generation: c.generation,
                    token: c.token,
                    reason: c.reason,
                })
                .collect(),
            counts: ManifestVolumeGcPreviewCounts {
                protected_not_retired: v.counts.protected_not_retired,
                protected_policy: v.counts.protected_policy,
                protected_ambiguous_use: v.counts.protected_ambiguous_use,
                protected_lease: v.counts.protected_lease,
                protected_session: v.counts.protected_session,
                protected_intent: v.counts.protected_intent,
                excluded_unmanaged: v.counts.excluded_unmanaged,
            },
        }
    }
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct ManifestVolumeGcApplyResult {
    #[pyo3(get)]
    pub(crate) removed: bool,
    #[pyo3(get)]
    pub(crate) reconciled_missing: bool,
}
impl From<ServiceManifestVolumeGcApplyResult> for ManifestVolumeGcApplyResult {
    fn from(v: ServiceManifestVolumeGcApplyResult) -> Self {
        Self {
            removed: v.removed,
            reconciled_missing: v.reconciled_missing,
        }
    }
}

#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupReconcileRecord {
    #[pyo3(get)]
    pub(crate) id: String,
    #[pyo3(get)]
    pub(crate) name: String,
    #[pyo3(get)]
    pub(crate) generation: String,
    #[pyo3(get)]
    pub(crate) drift: String,
    #[pyo3(get)]
    pub(crate) repair_token: Option<String>,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupReconcilePreviewPage {
    #[pyo3(get)]
    pub(crate) next: Option<u64>,
    pub(crate) records: Vec<SetupReconcileRecord>,
}
#[pymethods]
impl SetupReconcilePreviewPage {
    #[getter]
    pub(crate) fn records(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
        Ok(PyTuple::new(
            py,
            self.records
                .iter()
                .map(|v| {
                    Py::new(
                        py,
                        SetupReconcileRecord {
                            id: v.id.clone(),
                            name: v.name.clone(),
                            generation: v.generation.clone(),
                            drift: v.drift.clone(),
                            repair_token: v.repair_token.clone(),
                        },
                    )
                })
                .collect::<PyResult<Vec<_>>>()?,
        )?
        .unbind())
    }
}
impl From<ServiceSetupReconcilePreviewPage> for SetupReconcilePreviewPage {
    fn from(value: ServiceSetupReconcilePreviewPage) -> Self {
        Self {
            next: value.next,
            records: value
                .records
                .into_iter()
                .map(|v| SetupReconcileRecord {
                    id: v.id,
                    name: v.name,
                    generation: v.generation,
                    drift: v.drift,
                    repair_token: v.repair_token,
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupReconcileMissingRepairResult {
    #[pyo3(get)]
    pub(crate) repaired: bool,
    #[pyo3(get)]
    pub(crate) already_repaired: bool,
}
impl From<ServiceSetupReconcileMissingRepairResult> for SetupReconcileMissingRepairResult {
    fn from(value: ServiceSetupReconcileMissingRepairResult) -> Self {
        Self {
            repaired: value.repaired,
            already_repaired: value.already_repaired,
        }
    }
}

#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupGcApplyResult {
    #[pyo3(get)]
    pub(crate) removed: bool,
    #[pyo3(get)]
    pub(crate) reconciled_missing: bool,
}
impl From<ServiceSetupGcApplyResult> for SetupGcApplyResult {
    fn from(value: ServiceSetupGcApplyResult) -> Self {
        Self {
            removed: value.removed,
            reconciled_missing: value.reconciled_missing,
        }
    }
}

#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupRetiredStopResult {
    #[pyo3(get)]
    pub(crate) stopped: bool,
    #[pyo3(get)]
    pub(crate) already_stopped: bool,
}
impl From<ServiceSetupRetiredStopResult> for SetupRetiredStopResult {
    fn from(value: ServiceSetupRetiredStopResult) -> Self {
        Self {
            stopped: value.stopped,
            already_stopped: value.already_stopped,
        }
    }
}

#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupDoneResult {
    #[pyo3(get)]
    pub(crate) uses_completed: u64,
    #[pyo3(get)]
    pub(crate) resources_completed: u64,
}
impl From<ServiceSetupDoneResult> for SetupDoneResult {
    fn from(value: ServiceSetupDoneResult) -> Self {
        Self {
            uses_completed: value.uses_completed,
            resources_completed: value.resources_completed,
        }
    }
}

#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupEnsureEvent {
    #[pyo3(get)]
    pub(crate) cursor: u64,
    #[pyo3(get)]
    pub(crate) at: f64,
    #[pyo3(get)]
    pub(crate) kind: String,
    #[pyo3(get)]
    pub(crate) detail: String,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupEnsureEventPage {
    #[pyo3(get)]
    pub(crate) next: Option<u64>,
    pub(crate) records: Vec<SetupEnsureEvent>,
}
#[pymethods]
impl SetupEnsureEventPage {
    #[getter]
    pub(crate) fn records(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
        let values = self
            .records
            .iter()
            .map(|record| {
                Py::new(
                    py,
                    SetupEnsureEvent {
                        cursor: record.cursor,
                        at: record.at,
                        kind: record.kind.clone(),
                        detail: record.detail.clone(),
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        Ok(PyTuple::new(py, values)?.unbind())
    }
}
impl From<ServiceSetupEnsureEventPage> for SetupEnsureEventPage {
    fn from(value: ServiceSetupEnsureEventPage) -> Self {
        Self {
            next: value.next,
            records: value
                .records
                .into_iter()
                .map(|record| SetupEnsureEvent {
                    cursor: record.cursor,
                    at: record.at,
                    kind: record.kind,
                    detail: redact_diagnostic(&record.detail),
                })
                .collect(),
        }
    }
}

#[pymethods]
impl JobLogPage {
    /// Records as an immutable tuple, in cursor order.
    #[getter]
    pub(crate) fn records(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
        let values = self
            .records
            .iter()
            .map(|record| {
                Py::new(
                    py,
                    JobLogRecord {
                        cursor: record.cursor,
                        line: record.line.clone(),
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        Ok(PyTuple::new(py, values)?.unbind())
    }
}

impl From<ServiceJobLogPage> for JobLogPage {
    fn from(value: ServiceJobLogPage) -> Self {
        Self {
            retained_from: value.retained_from,
            next: value.next,
            gap: value.gap,
            records: value
                .records
                .into_iter()
                .map(|record| JobLogRecord {
                    cursor: record.cursor,
                    line: redact_diagnostic(&record.line),
                })
                .collect(),
        }
    }
}

/// Immutable receipt returned by [`Client::plan_setup`].
///
/// The receipt deliberately contains no setup-document source bytes.  Its
/// fields describe a fully validated, but unapplied, plan; application and
/// Docker execution stay behind a later explicit API.
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupPlan {
    #[pyo3(get)]
    pub(crate) source_kind: String,
    #[pyo3(get)]
    pub(crate) content_sha256: String,
    #[pyo3(get)]
    pub(crate) schema_version: u64,
    #[pyo3(get)]
    pub(crate) workspace: String,
    #[pyo3(get)]
    pub(crate) asset_root: Option<String>,
    pub(crate) task_names: Vec<String>,
    #[pyo3(get)]
    pub(crate) app_source_kind: String,
    #[pyo3(get)]
    pub(crate) image: Option<String>,
    #[pyo3(get)]
    pub(crate) dockerfile_path: Option<String>,
    #[pyo3(get)]
    pub(crate) applied: bool,
}

#[pymethods]
impl SetupPlan {
    /// Task names as an immutable tuple in validated lexical order.
    #[getter]
    pub(crate) fn task_names(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
        Ok(PyTuple::new(py, &self.task_names)?.unbind())
    }
}

impl From<RustSetupPlan> for SetupPlan {
    fn from(value: RustSetupPlan) -> Self {
        let (app_source_kind, image, dockerfile_path) = match value.app_source {
            SetupPlanAppSource::PinnedImage { image } => ("pinned_image".into(), Some(image), None),
            SetupPlanAppSource::InlineDockerfile { dockerfile_path } => (
                "inline_dockerfile".into(),
                None,
                Some(dockerfile_path.to_string_lossy().into_owned()),
            ),
        };
        Self {
            source_kind: source_kind_name(value.source_kind).into(),
            content_sha256: value.content_sha256,
            schema_version: value.schema_version,
            workspace: value.workspace_root.to_string_lossy().into_owned(),
            asset_root: value
                .asset_root
                .map(|path| path.to_string_lossy().into_owned()),
            task_names: value.task_names,
            app_source_kind,
            image,
            dockerfile_path,
            applied: false,
        }
    }
}

/// Immutable, read-only result of [`plan_compose_yaml`].
///
/// `document_json` and `normalized_json` contain only caller-supplied Compose
/// semantics.  They do not identify an engine, workspace, build context, or
/// a resource that Bosn may execute.
#[pyclass(module = "bosn._native", frozen)]
pub struct ComposePlan {
    #[pyo3(get)]
    pub(crate) version: u32,
    #[pyo3(get)]
    pub(crate) digest: String,
    #[pyo3(get)]
    pub(crate) normalized_json: String,
    #[pyo3(get)]
    pub(crate) document_json: String,
    #[pyo3(get)]
    pub(crate) applied: bool,
}
