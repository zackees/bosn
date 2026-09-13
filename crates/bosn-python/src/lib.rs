//! Deliberately small Python boundary over the Bosn-owned Rust daemon protocol.
//!
//! Product policy stays in Rust.  This extension only converts Python values
//! into the typed Rust client and maps its result into immutable Python values.

use bosn_core::{parse_and_plan_compose_yaml, parse_setup_config_locator};
use bosn_service::{
    Client as ServiceClient, DoctorReport as ServiceDoctorReport, JobLogPage as ServiceJobLogPage,
    JobStatus as ServiceJobStatus, MAX_REGISTRY_DIAGNOSTIC_PAGE,
    RegistryResourcePage as ServiceRegistryResourcePage, SetupAdoptRequest,
    SetupDoneResult as ServiceSetupDoneResult, SetupEnsureEventPage as ServiceSetupEnsureEventPage,
    SetupEnsureJobRequest, SetupGcApplyResult as ServiceSetupGcApplyResult,
    SetupGcPreviewPage as ServiceSetupGcPreviewPage, SetupPreparePolicy, SetupPrepareRequest,
    SetupReconcileMissingRepairResult as ServiceSetupReconcileMissingRepairResult,
    SetupReconcilePreviewPage as ServiceSetupReconcilePreviewPage,
    SetupRetiredStopResult as ServiceSetupRetiredStopResult, SetupTaskJobRequest,
    Status as ServiceStatus,
};
use bosn_setup::{
    SetupAcquirePolicy, SetupPlan as RustSetupPlan, SetupPlanAppSource, SetupPlanRequest,
    SetupSourceKind, plan_setup,
};
use kernal_api::async_engine::RuntimeBuilder;
use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
    types::PyTuple,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PYTHON_PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SETUP_PREPARE_DEADLINE_MS: u64 = 5 * 60 * 1_000;
const MAX_SETUP_PREPARE_OUTPUT_BYTES: u32 = 8 * 1024 * 1024;
const MAX_JOB_LOG_RECORDS: u32 = 256;
const MAX_REGISTRY_RECORDS: u32 = MAX_REGISTRY_DIAGNOSTIC_PAGE;
/// Keep the pure native boundary finite even though the caller already owns
/// the Python string.  MCP uses a smaller transport-specific limit.
const MAX_COMPOSE_DOCUMENT_BYTES: usize = 1024 * 1024;

#[pyclass(module = "bosn._native", frozen)]
pub struct Client {
    state_dir: PathBuf,
}

#[pymethods]
impl Client {
    #[new]
    fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    #[getter]
    fn state_dir(&self) -> String {
        self.state_dir.to_string_lossy().into_owned()
    }

    /// Return the daemon's typed, read-only registry status.
    ///
    /// The synchronous Python method releases the GIL while a kernal-api
    /// runtime performs the bounded authenticated IPC round trip.
    fn status(&self, py: Python<'_>) -> PyResult<Status> {
        let state_dir = self.state_dir.clone();
        let status = py
            .detach(move || status(&state_dir).map_err(|error| error.to_string()))
            .map_err(PyRuntimeError::new_err)?;
        Ok(Status::from(status))
    }

    /// Run fixed, daemon-owned read-only health checks. This accepts no
    /// Docker, command, path, deadline, or output controls. An absent daemon
    /// is returned as the typed ``"unavailable"`` state rather than causing
    /// this extension to open or initialize a registry.
    fn doctor(&self, py: Python<'_>) -> PyResult<DoctorReport> {
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            doctor(&state_dir)
                .map(DoctorReport::from)
                .map_err(service_error)
        })
    }

    /// Read a bounded page of path-safe managed-resource diagnostics from the
    /// already-running daemon. This method never opens, creates, or migrates
    /// the registry itself; an unavailable daemon raises a runtime error.
    #[pyo3(signature = (*, after = 0, limit = 64))]
    fn registry_resources(
        &self,
        after: u64,
        limit: u32,
        py: Python<'_>,
    ) -> PyResult<RegistryResourcePage> {
        validate_registry_page(after, limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            registry_resources(&state_dir, after, limit)
                .map(RegistryResourcePage::from)
                .map_err(service_error)
        })
    }

    /// Read a bounded newest-first page of credential-safe setup ensure event
    /// history from the already-running daemon. It never initializes state.
    #[pyo3(signature = (*, after = 0, limit = 64))]
    fn setup_ensure_events(
        &self,
        after: u64,
        limit: u32,
        py: Python<'_>,
    ) -> PyResult<SetupEnsureEventPage> {
        validate_registry_page(after, limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            setup_ensure_events(&state_dir, after, limit)
                .map(SetupEnsureEventPage::from)
                .map_err(service_error)
        })
    }
    /// Return a non-destructive, bounded future-GC preview. The daemon uses
    /// only durable ownership facts; this method cannot call Docker or apply
    /// a collection operation.
    #[pyo3(signature = (workspace, *, after = 0, limit = 64))]
    fn setup_gc_preview(
        &self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
        py: Python<'_>,
    ) -> PyResult<SetupGcPreviewPage> {
        validate_registry_page(after, limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            setup_gc_preview(&state_dir, workspace, after, limit)
                .map(SetupGcPreviewPage::from)
                .map_err(service_error)
        })
    }
    /// Read-only fixed Docker/registry drift preview. No repair, lifecycle,
    /// raw Docker arguments, or registry write is reachable from Python.
    #[pyo3(signature = (workspace, *, after = 0, limit = 64))]
    fn setup_reconcile_preview(
        &self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
        py: Python<'_>,
    ) -> PyResult<SetupReconcilePreviewPage> {
        validate_registry_page(after, limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            setup_reconcile_preview(&state_dir, workspace, after, limit)
                .map(SetupReconcilePreviewPage::from)
                .map_err(service_error)
        })
    }
    /// Retire one preview-token-bound missing setup app in registry accounting
    /// only. ``confirm=True`` is required; no Docker name, image, argv,
    /// mount, or lifecycle control is accepted at this Python boundary.
    #[pyo3(signature = (workspace, candidate_token, *, confirm))]
    fn setup_reconcile_repair_missing(
        &self,
        workspace: PathBuf,
        candidate_token: String,
        confirm: bool,
        py: Python<'_>,
    ) -> PyResult<SetupReconcileMissingRepairResult> {
        if !confirm {
            return Err(PyValueError::new_err(
                "setup_reconcile_repair_missing requires confirm=True",
            ));
        }
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            setup_reconcile_repair_missing(&state_dir, workspace, candidate_token)
                .map(SetupReconcileMissingRepairResult::from)
                .map_err(service_error)
        })
    }
    /// Destructively remove exactly one preview candidate. `confirm` must be
    /// true and `candidate_token` must come from this API's preview result;
    /// no Docker identifier, argv, image, or selector can be supplied.
    #[pyo3(signature = (workspace, candidate_token, *, confirm))]
    fn setup_gc_apply(
        &self,
        workspace: PathBuf,
        candidate_token: String,
        confirm: bool,
        py: Python<'_>,
    ) -> PyResult<SetupGcApplyResult> {
        if !confirm {
            return Err(PyValueError::new_err(
                "setup_gc_apply requires confirm=True",
            ));
        }
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            setup_gc_apply(&state_dir, workspace, candidate_token)
                .map(SetupGcApplyResult::from)
                .map_err(service_error)
        })
    }

    /// Stop one preview-derived retired setup app. ``confirm=True`` is
    /// required; Docker names, images, argv, and timeouts are not accepted.
    #[pyo3(signature = (workspace, candidate_token, *, confirm))]
    fn setup_stop_retired(
        &self,
        workspace: PathBuf,
        candidate_token: String,
        confirm: bool,
        py: Python<'_>,
    ) -> PyResult<SetupRetiredStopResult> {
        if !confirm {
            return Err(PyValueError::new_err(
                "setup_stop_retired requires confirm=True",
            ));
        }
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            setup_stop_retired(&state_dir, workspace, candidate_token)
                .map(SetupRetiredStopResult::from)
                .map_err(service_error)
        })
    }

    /// Mark this workspace's active setup ownership done in the daemon
    /// registry. This is registry-only: it never contacts Docker or deletes
    /// resources. Explicit ``confirm=True`` is required.
    #[pyo3(signature = (workspace, *, confirm))]
    fn setup_done(
        &self,
        workspace: PathBuf,
        confirm: bool,
        py: Python<'_>,
    ) -> PyResult<SetupDoneResult> {
        if !confirm {
            return Err(PyValueError::new_err("setup_done requires confirm=True"));
        }
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            setup_done(&state_dir, workspace)
                .map(SetupDoneResult::from)
                .map_err(service_error)
        })
    }

    /// Create an inert, validated setup plan using an explicit acquisition policy.
    ///
    /// `policy` must be either ``"online_refresh"`` (read and validate the
    /// local file or HTTPS document, then update Bosn's private cache) or
    /// ``"offline_cache_only"`` (reuse only an existing verified cache
    /// record).  Planning may create private cache/generated-asset state under
    /// this client's ``state_dir``, but it never writes ``workspace``, invokes
    /// Docker, contacts the Bosn daemon, or applies the setup document.
    ///
    /// The GIL is released while the kernal-api runtime performs the bounded
    /// acquisition and filesystem work.
    #[pyo3(signature = (workspace, config_locator, *, policy))]
    fn plan_setup(
        &self,
        workspace: PathBuf,
        config_locator: String,
        policy: &str,
        py: Python<'_>,
    ) -> PyResult<SetupPlan> {
        let policy = parse_setup_policy(policy)?;
        let state_dir = self.state_dir.clone();
        let plan = py
            .detach(move || native_plan_setup(state_dir, workspace, config_locator, policy))
            .map_err(PyRuntimeError::new_err)?;
        Ok(SetupPlan::from(plan))
    }

    /// Submit a bounded semantic setup-image preparation job to the local
    /// Bosn daemon and return its durable ID without waiting for Docker work.
    ///
    /// This accepts only a workspace and validated setup-document locator;
    /// it has no command, Docker argv, container, mount, or task-execution
    /// inputs. `policy` uses the same explicit values as [`Self::plan_setup`]:
    /// ``"online_refresh"`` or ``"offline_cache_only"``. `deadline_ms` is
    /// bounded to five minutes and `output_limit` to eight MiB before any IPC
    /// is attempted. The GIL is released for the authenticated IPC roundtrip.
    #[pyo3(signature = (workspace, config_locator, *, policy, deadline_ms, output_limit))]
    fn submit_setup_prepare(
        &self,
        workspace: PathBuf,
        config_locator: String,
        policy: &str,
        deadline_ms: u64,
        output_limit: u32,
        py: Python<'_>,
    ) -> PyResult<u64> {
        let policy = parse_prepare_policy(policy)?;
        validate_setup_prepare_input(&workspace, &config_locator, deadline_ms, output_limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            submit_setup_prepare(
                &state_dir,
                SetupPrepareRequest {
                    workspace,
                    config: config_locator,
                    policy,
                    deadline: Duration::from_millis(deadline_ms),
                    output_limit: output_limit as usize,
                },
            )
            .map_err(service_error)
        })
    }

    /// Submit one declared setup task to the local Bosn daemon and return its
    /// durable job ID without waiting for planning, image preparation, or task
    /// execution.
    ///
    /// The selected `task_name` must be a valid declared-task identifier; its
    /// command, image, mounts, environment, and working directory come only
    /// from the validated setup document. This method exposes no Docker,
    /// container, command, mount, or process controls. `policy`,
    /// `deadline_ms`, and `output_limit` have the same bounded semantic
    /// contract as [`Self::submit_setup_prepare`]. The GIL is released for the
    /// authenticated IPC roundtrip.
    #[pyo3(signature = (workspace, config_locator, *, policy, task_name, deadline_ms, output_limit))]
    #[allow(clippy::too_many_arguments)] // Required by the stable Python API signature.
    fn submit_setup_task(
        &self,
        workspace: PathBuf,
        config_locator: String,
        policy: &str,
        task_name: String,
        deadline_ms: u64,
        output_limit: u32,
        py: Python<'_>,
    ) -> PyResult<u64> {
        let policy = parse_prepare_policy(policy)?;
        validate_setup_task_input(
            &workspace,
            &config_locator,
            &task_name,
            deadline_ms,
            output_limit,
        )?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            submit_setup_task(
                &state_dir,
                SetupTaskJobRequest {
                    workspace,
                    config: config_locator,
                    policy,
                    task_name,
                    deadline: Duration::from_millis(deadline_ms),
                    output_limit: output_limit as usize,
                },
            )
            .map_err(service_error)
        })
    }

    /// Submit one complete, daemon-owned setup application ensure and return
    /// its durable job ID without waiting for planning, image preparation, or
    /// application mutation.
    ///
    /// The application name, image, command, mounts, environment, labels,
    /// working directory, and engine choices are derived solely from the
    /// validated setup document. This method exposes no Docker, container,
    /// command, mount, process, or task controls. `policy`, `deadline_ms`,
    /// and `output_limit` use the same bounded semantic contract as
    /// [`Self::submit_setup_prepare`]. The GIL is released only for the
    /// authenticated IPC roundtrip.
    #[pyo3(signature = (workspace, config_locator, *, policy, deadline_ms, output_limit))]
    fn submit_setup_ensure(
        &self,
        workspace: PathBuf,
        config_locator: String,
        policy: &str,
        deadline_ms: u64,
        output_limit: u32,
        py: Python<'_>,
    ) -> PyResult<u64> {
        let policy = parse_prepare_policy(policy)?;
        validate_setup_prepare_input(&workspace, &config_locator, deadline_ms, output_limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            submit_setup_ensure(
                &state_dir,
                SetupEnsureJobRequest {
                    workspace,
                    config: config_locator,
                    policy,
                    deadline: Duration::from_millis(deadline_ms),
                    output_limit: output_limit as usize,
                },
            )
            .map_err(service_error)
        })
    }
    /// Confirmed recovery of lost local registry ownership for an existing
    /// Bosn-managed app. The daemon derives every Docker identity itself.
    #[pyo3(signature = (workspace, config_locator, *, policy, deadline_ms, output_limit, confirm))]
    #[allow(clippy::too_many_arguments)] // Required by the stable Python API signature.
    fn setup_adopt(
        &self,
        workspace: PathBuf,
        config_locator: String,
        policy: &str,
        deadline_ms: u64,
        output_limit: u32,
        confirm: bool,
        py: Python<'_>,
    ) -> PyResult<bool> {
        if !confirm {
            return Err(PyValueError::new_err("setup_adopt requires confirm=True"));
        }
        let policy = parse_prepare_policy(policy)?;
        validate_setup_prepare_input(&workspace, &config_locator, deadline_ms, output_limit)?;
        let state = self.state_dir.clone();
        py.detach(move || {
            setup_adopt(
                &state,
                SetupAdoptRequest {
                    workspace,
                    config: config_locator,
                    policy,
                    deadline: Duration::from_millis(deadline_ms),
                    output_limit: output_limit as usize,
                    confirm: true,
                },
            )
            .map(|v| v.adopted)
            .map_err(service_error)
        })
    }

    /// Return typed status for a daemon job. The GIL is released while the
    /// bounded authenticated IPC roundtrip is in flight.
    fn job_status(&self, job_id: u64, py: Python<'_>) -> PyResult<JobStatus> {
        validate_job_id(job_id)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            job_status(&state_dir, job_id)
                .map(JobStatus::from)
                .map_err(service_error)
        })
    }

    /// Return at most 256 cursor-addressed daemon log records. `next` can be
    /// passed as `after` on the next poll; `gap` reports evicted history.
    #[pyo3(signature = (job_id, *, after = 0, limit = 64))]
    fn job_logs(
        &self,
        job_id: u64,
        after: u64,
        limit: u32,
        py: Python<'_>,
    ) -> PyResult<JobLogPage> {
        validate_job_id(job_id)?;
        if limit == 0 || limit > MAX_JOB_LOG_RECORDS {
            return Err(PyValueError::new_err("limit must be between 1 and 256"));
        }
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            job_logs(&state_dir, job_id, after, limit)
                .map(JobLogPage::from)
                .map_err(service_error)
        })
    }

    /// Request cooperative cancellation of a daemon job. The eventual terminal
    /// state remains observable through [`Self::job_status`].
    fn cancel_job(&self, job_id: u64, py: Python<'_>) -> PyResult<()> {
        validate_job_id(job_id)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || cancel_job(&state_dir, job_id).map_err(service_error))
    }
}

#[pyclass(module = "bosn._native", frozen)]
pub struct Status {
    #[pyo3(get)]
    registry_id: String,
    #[pyo3(get)]
    schema_version: u32,
    #[pyo3(get)]
    resources: u64,
    #[pyo3(get)]
    leases: u64,
    #[pyo3(get)]
    sessions: u64,
    #[pyo3(get)]
    reconciliation_required: bool,
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
    daemon: String,
    #[pyo3(get)]
    registry: String,
    #[pyo3(get)]
    engine: String,
    #[pyo3(get)]
    client_version: Option<String>,
    #[pyo3(get)]
    server_version: Option<String>,
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
    id: u64,
    #[pyo3(get)]
    state: String,
    #[pyo3(get)]
    error: Option<String>,
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
    cursor: u64,
    #[pyo3(get)]
    line: String,
}

/// Immutable cursor page returned by [`Client::job_logs`].
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct JobLogPage {
    #[pyo3(get)]
    retained_from: u64,
    #[pyo3(get)]
    next: u64,
    #[pyo3(get)]
    gap: bool,
    records: Vec<JobLogRecord>,
}

/// A path-safe managed resource diagnostic. Workspace/scope bindings are not
/// exposed by this Python API.
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct RegistryResource {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    stack: String,
    #[pyo3(get)]
    generation: String,
    #[pyo3(get)]
    state: String,
    #[pyo3(get)]
    retention: String,
    #[pyo3(get)]
    created_at: f64,
    #[pyo3(get)]
    last_used: f64,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct RegistryResourcePage {
    #[pyo3(get)]
    next: Option<u64>,
    records: Vec<RegistryResource>,
}
#[pymethods]
impl RegistryResourcePage {
    #[getter]
    fn records(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
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
    id: String,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    generation: String,
    #[pyo3(get)]
    token: String,
    #[pyo3(get)]
    reason: String,
}
#[derive(Clone, Debug)]
#[pyclass(module = "bosn._native", frozen, skip_from_py_object)]
pub struct SetupGcPreviewCounts {
    #[pyo3(get)]
    protected_not_retired: u64,
    #[pyo3(get)]
    protected_ambiguous_use: u64,
    #[pyo3(get)]
    protected_lease: u64,
    #[pyo3(get)]
    protected_session: u64,
    #[pyo3(get)]
    excluded_unmanaged: u64,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupGcPreviewPage {
    #[pyo3(get)]
    next: Option<u64>,
    #[pyo3(get)]
    counts: SetupGcPreviewCounts,
    candidates: Vec<SetupGcCandidate>,
}
#[pymethods]
impl SetupGcPreviewPage {
    #[getter]
    fn candidates(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
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
pub struct SetupReconcileRecord {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    generation: String,
    #[pyo3(get)]
    drift: String,
    #[pyo3(get)]
    repair_token: Option<String>,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupReconcilePreviewPage {
    #[pyo3(get)]
    next: Option<u64>,
    records: Vec<SetupReconcileRecord>,
}
#[pymethods]
impl SetupReconcilePreviewPage {
    #[getter]
    fn records(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
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
    repaired: bool,
    #[pyo3(get)]
    already_repaired: bool,
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
    removed: bool,
    #[pyo3(get)]
    reconciled_missing: bool,
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
    stopped: bool,
    #[pyo3(get)]
    already_stopped: bool,
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
    uses_completed: u64,
    #[pyo3(get)]
    resources_completed: u64,
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
    cursor: u64,
    #[pyo3(get)]
    at: f64,
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    detail: String,
}
#[derive(Debug)]
#[pyclass(module = "bosn._native", frozen)]
pub struct SetupEnsureEventPage {
    #[pyo3(get)]
    next: Option<u64>,
    records: Vec<SetupEnsureEvent>,
}
#[pymethods]
impl SetupEnsureEventPage {
    #[getter]
    fn records(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
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
    fn records(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
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
    source_kind: String,
    #[pyo3(get)]
    content_sha256: String,
    #[pyo3(get)]
    schema_version: u64,
    #[pyo3(get)]
    workspace: String,
    #[pyo3(get)]
    asset_root: Option<String>,
    task_names: Vec<String>,
    #[pyo3(get)]
    app_source_kind: String,
    #[pyo3(get)]
    image: Option<String>,
    #[pyo3(get)]
    dockerfile_path: Option<String>,
    #[pyo3(get)]
    applied: bool,
}

#[pymethods]
impl SetupPlan {
    /// Task names as an immutable tuple in validated lexical order.
    #[getter]
    fn task_names(&self, py: Python<'_>) -> PyResult<Py<PyTuple>> {
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
    version: u32,
    #[pyo3(get)]
    digest: String,
    #[pyo3(get)]
    normalized_json: String,
    #[pyo3(get)]
    document_json: String,
    #[pyo3(get)]
    applied: bool,
}

/// Return the native extension version, which must match the Python package.
#[pyfunction]
fn native_version() -> &'static str {
    PYTHON_PACKAGE_VERSION
}

/// Return the Bosn-owned daemon protocol version supported by this extension.
#[pyfunction]
fn protocol_version() -> u32 {
    bosn_service::PROTOCOL_VERSION
}

/// Parse and validate a caller-supplied Compose YAML document without reading
/// a path, contacting a daemon/engine, or writing state.
#[pyfunction]
fn plan_compose_yaml(source: &str) -> PyResult<ComposePlan> {
    if source.len() > MAX_COMPOSE_DOCUMENT_BYTES {
        return Err(PyValueError::new_err(format!(
            "Compose YAML exceeds the {MAX_COMPOSE_DOCUMENT_BYTES}-byte limit"
        )));
    }
    let plan = parse_and_plan_compose_yaml(source)
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    let document_json = serde_json::to_string(&plan.document)
        .map_err(|_| PyRuntimeError::new_err("could not encode Compose plan"))?;
    Ok(ComposePlan {
        version: plan.version,
        digest: plan.digest,
        normalized_json: plan.normalized_json,
        document_json,
        applied: false,
    })
}

fn status(state_dir: &Path) -> Result<ServiceStatus, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async { ServiceClient::for_state(state_dir)?.status().await })
}

fn doctor(state_dir: &Path) -> Result<ServiceDoctorReport, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async { ServiceClient::for_state(state_dir)?.doctor().await })
}

fn registry_resources(
    state_dir: &Path,
    after: u64,
    limit: u32,
) -> Result<ServiceRegistryResourcePage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .registry_resources(after, limit)
            .await
    })
}

fn setup_ensure_events(
    state_dir: &Path,
    after: u64,
    limit: u32,
) -> Result<ServiceSetupEnsureEventPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_ensure_events(after, limit)
            .await
    })
}
fn setup_gc_preview(
    state_dir: &Path,
    workspace: PathBuf,
    after: u64,
    limit: u32,
) -> Result<ServiceSetupGcPreviewPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_gc_preview(workspace, after, limit)
            .await
    })
}
fn setup_reconcile_preview(
    state_dir: &Path,
    workspace: PathBuf,
    after: u64,
    limit: u32,
) -> Result<ServiceSetupReconcilePreviewPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_reconcile_preview(workspace, after, limit)
            .await
    })
}
fn setup_reconcile_repair_missing(
    state_dir: &Path,
    workspace: PathBuf,
    candidate_token: String,
) -> Result<ServiceSetupReconcileMissingRepairResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_reconcile_repair_missing(workspace, &candidate_token, true)
            .await
    })
}
fn setup_gc_apply(
    state_dir: &Path,
    workspace: PathBuf,
    candidate_token: String,
) -> Result<ServiceSetupGcApplyResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_gc_apply(workspace, &candidate_token, true)
            .await
    })
}
fn setup_stop_retired(
    state_dir: &Path,
    workspace: PathBuf,
    candidate_token: String,
) -> Result<ServiceSetupRetiredStopResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_stop_retired(workspace, &candidate_token, true)
            .await
    })
}

fn setup_done(
    state_dir: &Path,
    workspace: PathBuf,
) -> Result<ServiceSetupDoneResult, bosn_service::Error> {
    let client = ServiceClient::for_state(state_dir)?;
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .map_err(bosn_service::Error::Io)?;
    runtime.run(client.setup_done(workspace, true))
}
fn setup_adopt(
    state_dir: &Path,
    request: SetupAdoptRequest,
) -> Result<bosn_service::SetupAdoptResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_adopt(request)
            .await
    })
}

fn submit_setup_prepare(
    state_dir: &Path,
    request: SetupPrepareRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_setup_prepare(request)
            .await
    })
}

fn submit_setup_task(
    state_dir: &Path,
    request: SetupTaskJobRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_setup_task(request)
            .await
    })
}

fn submit_setup_ensure(
    state_dir: &Path,
    request: SetupEnsureJobRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_setup_ensure(request)
            .await
    })
}

fn job_status(state_dir: &Path, job_id: u64) -> Result<ServiceJobStatus, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .job_status(job_id)
            .await
    })
}

fn job_logs(
    state_dir: &Path,
    job_id: u64,
    after: u64,
    limit: u32,
) -> Result<ServiceJobLogPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .job_logs(job_id, after, limit)
            .await
    })
}

fn cancel_job(state_dir: &Path, job_id: u64) -> Result<(), bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .cancel_job(job_id)
            .await
    })
}

fn parse_setup_policy(policy: &str) -> PyResult<SetupAcquirePolicy> {
    match policy {
        "online_refresh" => Ok(SetupAcquirePolicy::OnlineRefresh),
        "offline_cache_only" => Ok(SetupAcquirePolicy::OfflineCacheOnly),
        _ => Err(PyValueError::new_err(
            "policy must be 'online_refresh' or 'offline_cache_only'",
        )),
    }
}

fn parse_prepare_policy(policy: &str) -> PyResult<SetupPreparePolicy> {
    match policy {
        "online_refresh" => Ok(SetupPreparePolicy::Refresh),
        "offline_cache_only" => Ok(SetupPreparePolicy::Offline),
        _ => Err(PyValueError::new_err(
            "policy must be 'online_refresh' or 'offline_cache_only'",
        )),
    }
}

fn validate_setup_prepare_input(
    workspace: &Path,
    config_locator: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> PyResult<()> {
    let workspace = workspace
        .to_str()
        .ok_or_else(|| PyValueError::new_err("workspace must be valid UTF-8"))?;
    if workspace.is_empty() || workspace.len() > 8 * 1024 || workspace.bytes().any(|byte| byte == 0)
    {
        return Err(PyValueError::new_err("workspace is empty or invalid"));
    }
    // This is pure core parsing only: it rejects non-HTTPS remote locators,
    // userinfo, fragments, whitespace, and oversized values before IPC.
    // Do not echo the caller's locator, which may contain credentials.
    parse_setup_config_locator(config_locator)
        .map_err(|_| PyValueError::new_err("setup config locator is invalid"))?;
    if deadline_ms == 0 || deadline_ms > MAX_SETUP_PREPARE_DEADLINE_MS {
        return Err(PyValueError::new_err(
            "deadline_ms must be between 1 and 300000",
        ));
    }
    if output_limit == 0 || output_limit > MAX_SETUP_PREPARE_OUTPUT_BYTES {
        return Err(PyValueError::new_err(
            "output_limit must be between 1 and 8388608",
        ));
    }
    Ok(())
}

/// Keep this syntactic check identical to the daemon wire validator and the
/// setup-document schema. Existence in the document remains a daemon-owned
/// execution concern, so the Python boundary never parses or runs task data.
fn validate_setup_task_input(
    workspace: &Path,
    config_locator: &str,
    task_name: &str,
    deadline_ms: u64,
    output_limit: u32,
) -> PyResult<()> {
    validate_setup_prepare_input(workspace, config_locator, deadline_ms, output_limit)?;
    if task_name.is_empty()
        || task_name.len() > 64
        || !task_name.as_bytes()[0].is_ascii_alphanumeric()
        || !task_name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        })
    {
        return Err(PyValueError::new_err("task_name is invalid"));
    }
    Ok(())
}

fn validate_job_id(job_id: u64) -> PyResult<()> {
    if job_id == 0 {
        return Err(PyValueError::new_err("job_id must be positive"));
    }
    Ok(())
}

fn validate_registry_page(_after: u64, limit: u32) -> PyResult<()> {
    if limit == 0 || limit > MAX_REGISTRY_RECORDS {
        return Err(PyValueError::new_err("limit must be between 1 and 64"));
    }
    Ok(())
}

/// Remove common URL credentials and credential-bearing query values before a
/// daemon diagnostic crosses the Python boundary. Service protocol failures do
/// not include request text, but job logs originate with external tools and
/// are therefore handled defensively here as well.
fn redact_diagnostic(value: &str) -> String {
    let mut redacted = value.to_owned();
    for scheme in ["https://", "http://"] {
        let mut search_from = 0;
        while let Some(relative) = redacted[search_from..].find(scheme) {
            let start = search_from + relative + scheme.len();
            let end = redacted[start..]
                .find(|character: char| {
                    character.is_whitespace() || character == '/' || character == '?'
                })
                .map(|offset| start + offset)
                .unwrap_or(redacted.len());
            if let Some(at) = redacted[start..end].find('@') {
                let at = start + at;
                redacted.replace_range(start..=at, "[redacted]@");
                search_from = start + "[redacted]@".len();
            } else {
                search_from = end;
            }
        }
    }
    for key in [
        "token",
        "access_token",
        "password",
        "secret",
        "api_key",
        "apikey",
        "authorization",
    ] {
        let needle = format!("{key}=");
        let mut search_from = 0;
        while let Some(relative) = redacted[search_from..].to_ascii_lowercase().find(&needle) {
            let start = search_from + relative + needle.len();
            let end = redacted[start..]
                .find(|character: char| {
                    character == '&'
                        || character.is_whitespace()
                        || character == '"'
                        || character == '\''
                })
                .map(|offset| start + offset)
                .unwrap_or(redacted.len());
            redacted.replace_range(start..end, "[redacted]");
            search_from = start + "[redacted]".len();
        }
    }
    redacted
}

fn service_error(error: bosn_service::Error) -> PyErr {
    let message = match error {
        bosn_service::Error::Io(_) | bosn_service::Error::Deadline => {
            "Bosn daemon is unavailable or did not respond in time"
        }
        bosn_service::Error::Unauthorized => "Bosn daemon authentication failed",
        bosn_service::Error::EndpointOccupied(_) => "Bosn daemon endpoint is unavailable",
        bosn_service::Error::Protocol(_) => "Bosn daemon rejected the request",
        bosn_service::Error::Registry(_)
        | bosn_service::Error::Random
        | bosn_service::Error::ActorClosed => "Bosn daemon request failed",
    };
    PyRuntimeError::new_err(message)
}

fn source_kind_name(source_kind: SetupSourceKind) -> &'static str {
    match source_kind {
        SetupSourceKind::LocalFile => "local_file",
        SetupSourceKind::Https => "https",
    }
}

fn native_plan_setup(
    state_dir: PathBuf,
    workspace: PathBuf,
    config_locator: String,
    policy: SetupAcquirePolicy,
) -> Result<RustSetupPlan, String> {
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime
        .run(plan_setup(SetupPlanRequest {
            state_dir,
            workspace,
            locator: config_locator,
            policy,
        }))
        .map_err(|error| error.to_string())
}

/// Run the native JSON-RPC MCP server on the caller's stdio streams.
///
/// The Python CLI uses this only as a packaged launcher. The protocol loop,
/// daemon client, and all lifecycle authority remain in Rust.
#[pyfunction]
fn run_mcp(state_dir: PathBuf, py: Python<'_>) -> PyResult<()> {
    py.detach(move || bosn_service::mcp::serve_stdio(state_dir).map_err(|error| error.to_string()))
        .map_err(PyRuntimeError::new_err)
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Client>()?;
    module.add_class::<Status>()?;
    module.add_class::<DoctorReport>()?;
    module.add_class::<JobStatus>()?;
    module.add_class::<JobLogRecord>()?;
    module.add_class::<JobLogPage>()?;
    module.add_class::<RegistryResource>()?;
    module.add_class::<RegistryResourcePage>()?;
    module.add_class::<SetupGcCandidate>()?;
    module.add_class::<SetupGcPreviewCounts>()?;
    module.add_class::<SetupGcPreviewPage>()?;
    module.add_class::<SetupReconcileRecord>()?;
    module.add_class::<SetupReconcilePreviewPage>()?;
    module.add_class::<SetupReconcileMissingRepairResult>()?;
    module.add_class::<SetupGcApplyResult>()?;
    module.add_class::<SetupRetiredStopResult>()?;
    module.add_class::<SetupDoneResult>()?;
    module.add_class::<SetupEnsureEvent>()?;
    module.add_class::<SetupEnsureEventPage>()?;
    module.add_class::<SetupPlan>()?;
    module.add_class::<ComposePlan>()?;
    module.add_function(wrap_pyfunction!(native_version, module)?)?;
    module.add_function(wrap_pyfunction!(protocol_version, module)?)?;
    module.add_function(wrap_pyfunction!(plan_compose_yaml, module)?)?;
    module.add_function(wrap_pyfunction!(run_mcp, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "embedded-python-tests")]
    use bosn_service::{
        Service, SetupEnsureExecution, SetupEnsureExecutor, SetupEnsureImageResource,
        SetupEnsureResource, SetupPrepareExecutor, SetupTaskExecutor,
    };
    #[cfg(feature = "embedded-python-tests")]
    use kernal_api::async_engine::{self, CancellationToken, RuntimeBuilder, Sender};
    #[cfg(feature = "embedded-python-tests")]
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Instant,
    };

    #[cfg(feature = "embedded-python-tests")]
    struct FakeSetupExecutor {
        started: AtomicUsize,
        cancelled: AtomicUsize,
    }

    #[cfg(feature = "embedded-python-tests")]
    impl FakeSetupExecutor {
        fn new() -> Self {
            Self {
                started: AtomicUsize::new(0),
                cancelled: AtomicUsize::new(0),
            }
        }
    }

    #[cfg(feature = "embedded-python-tests")]
    impl SetupPrepareExecutor for FakeSetupExecutor {
        fn execute<'a>(
            &'a self,
            _request: SetupPrepareRequest,
            cancellation: &'a CancellationToken,
            logs: &'a Sender<String>,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                logs.send("[fake] setup preparation started".into())
                    .await
                    .map_err(|_| "fake log consumer closed".to_owned())?;
                for _ in 0..100 {
                    if cancellation.is_cancelled() {
                        self.cancelled.fetch_add(1, Ordering::SeqCst);
                        return Err("fake cancellation observed".into());
                    }
                    async_engine::sleep(Duration::from_millis(10)).await;
                }
                Ok("fake prepared sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
            })
        }
    }

    #[cfg(feature = "embedded-python-tests")]
    struct FakeSetupTaskExecutor {
        started: AtomicUsize,
        cancelled: AtomicUsize,
    }

    #[cfg(feature = "embedded-python-tests")]
    impl FakeSetupTaskExecutor {
        fn new() -> Self {
            Self {
                started: AtomicUsize::new(0),
                cancelled: AtomicUsize::new(0),
            }
        }
    }

    #[cfg(feature = "embedded-python-tests")]
    impl SetupTaskExecutor for FakeSetupTaskExecutor {
        fn execute<'a>(
            &'a self,
            _request: SetupTaskJobRequest,
            cancellation: &'a CancellationToken,
            logs: &'a Sender<String>,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                logs.send("[fake] setup task started".into())
                    .await
                    .map_err(|_| "fake log consumer closed".to_owned())?;
                for _ in 0..100 {
                    if cancellation.is_cancelled() {
                        self.cancelled.fetch_add(1, Ordering::SeqCst);
                        return Err("fake task cancellation observed".into());
                    }
                    async_engine::sleep(Duration::from_millis(10)).await;
                }
                Ok("fake task completed".into())
            })
        }
    }

    #[cfg(feature = "embedded-python-tests")]
    struct FakeSetupEnsureExecutor {
        started: AtomicUsize,
        cancelled: AtomicUsize,
    }

    #[cfg(feature = "embedded-python-tests")]
    impl FakeSetupEnsureExecutor {
        fn new() -> Self {
            Self {
                started: AtomicUsize::new(0),
                cancelled: AtomicUsize::new(0),
            }
        }
    }

    #[cfg(feature = "embedded-python-tests")]
    impl SetupEnsureExecutor for FakeSetupEnsureExecutor {
        fn execute<'a>(
            &'a self,
            request: SetupEnsureJobRequest,
            cancellation: &'a CancellationToken,
            logs: &'a Sender<String>,
        ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                logs.send("[fake] setup ensure started".into())
                    .await
                    .map_err(|_| "fake log consumer closed".to_owned())?;
                for _ in 0..100 {
                    if cancellation.is_cancelled() {
                        self.cancelled.fetch_add(1, Ordering::SeqCst);
                        return Err("fake ensure cancellation observed".into());
                    }
                    async_engine::sleep(Duration::from_millis(10)).await;
                }
                Ok(SetupEnsureExecution {
                    receipt: "fake setup ensured".into(),
                    resource: SetupEnsureResource {
                        id: "setup-container:python-test".into(),
                        name: "bosn-setup-python-test".into(),
                        stack: "setup".into(),
                        generation: "sha256:python-test".into(),
                        workspace: request.workspace.to_string_lossy().into_owned(),
                    },
                    image: SetupEnsureImageResource {
                        id: "setup-image:sha256:python-test".into(),
                        name: "setup-image:sha256:python-test".into(),
                        stack: "setup".into(),
                        generation: "sha256:python-test".into(),
                        workspace: request.workspace.to_string_lossy().into_owned(),
                    },
                })
            })
        }
    }

    #[test]
    fn native_version_matches_python_distribution() {
        assert_eq!(native_version(), "0.1.3");
    }

    #[test]
    fn compose_plan_is_pure_and_returns_an_immutable_receipt() {
        let plan = plan_compose_yaml("services:\n  api:\n    image: alpine:3.21\n").unwrap();
        assert_eq!(plan.version, 1);
        assert!(plan.digest.starts_with("sha256:"));
        assert!(plan.document_json.contains("alpine:3.21"));
        assert!(plan.normalized_json.contains("alpine:3.21"));
        assert!(!plan.applied);
        assert!(plan_compose_yaml("services: {}\n").is_err());
    }

    #[cfg(feature = "embedded-python-tests")]
    #[test]
    fn python_module_exposes_compose_plan_without_a_client_or_daemon() {
        Python::initialize();
        Python::attach(|py| {
            let module = PyModule::new(py, "bosn_native_test").unwrap();
            _native(&module).unwrap();
            let plan = module
                .getattr("plan_compose_yaml")
                .unwrap()
                .call1(("services:\n  api:\n    image: alpine:3.21\n",))
                .unwrap();
            assert_eq!(
                plan.getattr("version").unwrap().extract::<u32>().unwrap(),
                1
            );
            assert!(!plan.getattr("applied").unwrap().extract::<bool>().unwrap());
            assert!(
                plan.getattr("document_json")
                    .unwrap()
                    .extract::<String>()
                    .unwrap()
                    .contains("alpine:3.21")
            );
        });
    }

    #[cfg(feature = "embedded-python-tests")]
    #[test]
    fn python_client_submits_and_observes_fake_setup_job_without_docker() {
        Python::initialize();
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let executor = Arc::new(FakeSetupExecutor::new());
        RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_prepare_executor(executor.clone())
                        .serve(),
                );
                let wire_client = wait_for_client(&state).await;
                let submitted = Instant::now();
                let python_state = state.clone();
                let python_workspace = workspace.clone();
                let (first, second) = std::thread::spawn(move || {
                    Python::initialize();
                    Python::attach(|py| {
                        let python_client = Client {
                            state_dir: python_state,
                        };
                        let first = python_client.submit_setup_prepare(
                            python_workspace.clone(),
                            "https://example.invalid/setup.toml".into(),
                            "online_refresh",
                            2_000,
                            4 * 1024,
                            py,
                        )?;
                        let second = python_client.submit_setup_prepare(
                            python_workspace,
                            "https://example.invalid/setup.toml".into(),
                            "online_refresh",
                            2_000,
                            4 * 1024,
                            py,
                        )?;
                        Ok::<_, PyErr>((first, second))
                    })
                })
                .join()
                .expect("Python submit thread panicked")
                .unwrap();
                assert!(submitted.elapsed() < Duration::from_millis(250));
                assert_eq!(first, second);

                wait_for(|| executor.started.load(Ordering::SeqCst) == 1).await;
                let page = wait_for_python_logs(&state, first).await;
                assert_eq!(page.records.len(), 1);
                assert_eq!(page.records[0].cursor, 0);
                assert_eq!(page.records[0].line, "[fake] setup preparation started");
                assert_eq!(page.next, 1);
                assert!(!page.gap);

                let python_state = state.clone();
                let running = std::thread::spawn(move || {
                    Python::attach(|py| {
                        Client {
                            state_dir: python_state,
                        }
                        .job_status(first, py)
                    })
                })
                .join()
                .expect("Python status thread panicked")
                .unwrap();
                assert_eq!(running.id, first);
                assert!(matches!(running.state.as_str(), "Running" | "Cancelling"));
                let python_state = state.clone();
                std::thread::spawn(move || {
                    Python::attach(|py| {
                        Client {
                            state_dir: python_state,
                        }
                        .cancel_job(first, py)
                    })
                })
                .join()
                .expect("Python cancellation thread panicked")
                .unwrap();
                wait_for_job_state(&wire_client, first, "Cancelled").await;
                assert_eq!(executor.cancelled.load(Ordering::SeqCst), 1);

                wire_client.shutdown().await.unwrap();
                async_engine::timeout(Duration::from_secs(5), server)
                    .await
                    .expect("service did not stop")
                    .expect("service task failed")
                    .expect("service returned error");
            });
    }

    #[cfg(feature = "embedded-python-tests")]
    #[test]
    fn python_client_submits_and_cancels_coalesced_fake_setup_task_without_docker() {
        Python::initialize();
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let executor = Arc::new(FakeSetupTaskExecutor::new());
        RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_task_executor(executor.clone())
                        .serve(),
                );
                let wire_client = wait_for_client(&state).await;
                let submitted = Instant::now();
                let python_state = state.clone();
                let python_workspace = workspace.clone();
                let (first, second) = std::thread::spawn(move || {
                    Python::initialize();
                    Python::attach(|py| {
                        let python_client = Client {
                            state_dir: python_state,
                        };
                        let first = python_client.submit_setup_task(
                            python_workspace.clone(),
                            "https://example.invalid/setup.toml".into(),
                            "online_refresh",
                            "wait".into(),
                            2_000,
                            4 * 1024,
                            py,
                        )?;
                        let second = python_client.submit_setup_task(
                            python_workspace,
                            "https://example.invalid/setup.toml".into(),
                            "online_refresh",
                            "wait".into(),
                            2_000,
                            4 * 1024,
                            py,
                        )?;
                        Ok::<_, PyErr>((first, second))
                    })
                })
                .join()
                .expect("Python submit thread panicked")
                .unwrap();
                assert!(submitted.elapsed() < Duration::from_millis(250));
                assert_eq!(first, second);

                wait_for(|| executor.started.load(Ordering::SeqCst) == 1).await;
                let python_state = state.clone();
                let running = std::thread::spawn(move || {
                    Python::attach(|py| {
                        Client {
                            state_dir: python_state,
                        }
                        .job_status(first, py)
                    })
                })
                .join()
                .expect("Python status thread panicked")
                .unwrap();
                assert_eq!(running.id, first);
                assert!(matches!(running.state.as_str(), "Running" | "Cancelling"));

                let python_state = state.clone();
                std::thread::spawn(move || {
                    Python::attach(|py| {
                        Client {
                            state_dir: python_state,
                        }
                        .cancel_job(first, py)
                    })
                })
                .join()
                .expect("Python cancellation thread panicked")
                .unwrap();
                wait_for_job_state(&wire_client, first, "Cancelled").await;
                assert_eq!(executor.cancelled.load(Ordering::SeqCst), 1);

                wire_client.shutdown().await.unwrap();
                async_engine::timeout(Duration::from_secs(5), server)
                    .await
                    .expect("service did not stop")
                    .expect("service task failed")
                    .expect("service returned error");
            });
    }

    #[cfg(feature = "embedded-python-tests")]
    #[test]
    fn python_client_submits_and_cancels_coalesced_fake_setup_ensure_without_docker() {
        Python::initialize();
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let executor = Arc::new(FakeSetupEnsureExecutor::new());
        RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_ensure_executor(executor.clone())
                        .serve(),
                );
                let wire_client = wait_for_client(&state).await;
                let submitted = Instant::now();
                let python_state = state.clone();
                let python_workspace = workspace.clone();
                let (first, second) = std::thread::spawn(move || {
                    Python::initialize();
                    Python::attach(|py| {
                        let python_client = Client {
                            state_dir: python_state,
                        };
                        let first = python_client.submit_setup_ensure(
                            python_workspace.clone(),
                            "https://example.invalid/setup.toml".into(),
                            "online_refresh",
                            2_000,
                            4 * 1024,
                            py,
                        )?;
                        let second = python_client.submit_setup_ensure(
                            python_workspace,
                            "https://example.invalid/setup.toml".into(),
                            "online_refresh",
                            2_000,
                            4 * 1024,
                            py,
                        )?;
                        Ok::<_, PyErr>((first, second))
                    })
                })
                .join()
                .expect("Python submit thread panicked")
                .unwrap();
                assert!(submitted.elapsed() < Duration::from_millis(250));
                assert_eq!(first, second);

                wait_for(|| executor.started.load(Ordering::SeqCst) == 1).await;
                let page = wait_for_python_logs(&state, first).await;
                assert_eq!(page.records[0].line, "[fake] setup ensure started");

                let python_state = state.clone();
                std::thread::spawn(move || {
                    Python::attach(|py| {
                        Client {
                            state_dir: python_state,
                        }
                        .cancel_job(first, py)
                    })
                })
                .join()
                .expect("Python cancellation thread panicked")
                .unwrap();
                wait_for_job_state(&wire_client, first, "Cancelled").await;
                assert_eq!(executor.cancelled.load(Ordering::SeqCst), 1);

                wire_client.shutdown().await.unwrap();
                async_engine::timeout(Duration::from_secs(5), server)
                    .await
                    .expect("service did not stop")
                    .expect("service task failed")
                    .expect("service returned error");
            });
    }

    #[cfg(feature = "embedded-python-tests")]
    #[test]
    fn python_prepare_input_and_diagnostics_do_not_expose_credentials() {
        Python::initialize();
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let client = Client {
            state_dir: temporary.path().join("no-daemon"),
        };
        Python::attach(|py| {
            let error = client
                .submit_setup_prepare(
                    temporary.path().join("workspace"),
                    "https://user:top-secret@example.invalid/setup.toml".into(),
                    "online_refresh",
                    1_000,
                    4 * 1024,
                    py,
                )
                .unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));

            let error = client
                .submit_setup_ensure(
                    temporary.path().join("workspace"),
                    "https://user:ensure-secret@example.invalid/setup.toml".into(),
                    "online_refresh",
                    1_000,
                    4 * 1024,
                    py,
                )
                .unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));
            assert!(!error.to_string().contains("ensure-secret"));

            let error = client
                .submit_setup_ensure(
                    temporary.path().join("workspace"),
                    "https://example.invalid/setup.toml".into(),
                    "online_refresh",
                    0,
                    4 * 1024,
                    py,
                )
                .unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));
            assert!(!error.to_string().contains("top-secret"));

            let error = client
                .submit_setup_prepare(
                    temporary.path().join("workspace"),
                    "https://example.invalid/setup.toml".into(),
                    "online_refresh",
                    0,
                    4 * 1024,
                    py,
                )
                .unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));

            let error = client.job_status(1, py).unwrap_err();
            assert!(error.is_instance_of::<PyRuntimeError>(py));
            assert!(!error.to_string().contains("no-daemon"));

            let error = client
                .setup_reconcile_repair_missing(
                    temporary.path().join("workspace"),
                    "srm1-00".into(),
                    false,
                    py,
                )
                .unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));
        });
        assert_eq!(
            redact_diagnostic("https://user:secret@example.test/a?token=also-secret&safe=value"),
            "https://[redacted]@example.test/a?token=[redacted]&safe=value"
        );
    }

    #[cfg(feature = "embedded-python-tests")]
    #[test]
    fn python_registry_diagnostics_match_the_authenticated_daemon_surface() {
        Python::initialize();
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");

        RuntimeBuilder::multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(Service::new(state.clone()).serve());
                let wire = wait_for_client(&state).await;
                let python_state = state.clone();
                let workspace = temporary.path().join("workspace");
                let (doctor, resources, events, preview) = std::thread::spawn(move || {
                    Python::attach(|py| {
                        let client = Client {
                            state_dir: python_state,
                        };
                        Ok::<_, PyErr>((
                            client.doctor(py)?,
                            client.registry_resources(0, 1, py)?,
                            client.setup_ensure_events(0, 1, py)?,
                            client.setup_gc_preview(workspace, 0, 1, py)?,
                        ))
                    })
                })
                .join()
                .unwrap()
                .unwrap();
                assert_eq!(doctor.daemon, "ready");
                assert_eq!(doctor.registry, "ready");
                assert!(resources.records.is_empty());
                assert!(events.records.is_empty());
                assert!(preview.next.is_none());
                let direct = wire.registry_resources(0, 1).await.unwrap();
                assert_eq!(direct.records.len(), resources.records.len());
                wire.shutdown().await.unwrap();
                async_engine::timeout(Duration::from_secs(5), server)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            });
    }

    #[cfg(feature = "embedded-python-tests")]
    async fn wait_for_client(state: &Path) -> ServiceClient {
        let client = ServiceClient::for_state(state).unwrap();
        for _ in 0..50 {
            if client.ping().await.is_ok() {
                return client;
            }
            async_engine::sleep(Duration::from_millis(10)).await;
        }
        panic!("fake daemon did not become ready");
    }

    #[cfg(feature = "embedded-python-tests")]
    async fn wait_for(predicate: impl Fn() -> bool) {
        for _ in 0..100 {
            if predicate() {
                return;
            }
            async_engine::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition did not become true");
    }

    #[cfg(feature = "embedded-python-tests")]
    async fn wait_for_job_state(client: &ServiceClient, id: u64, wanted: &str) {
        for _ in 0..100 {
            if client.job_status(id).await.unwrap().state == wanted {
                return;
            }
            async_engine::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} did not reach {wanted}");
    }

    #[cfg(feature = "embedded-python-tests")]
    async fn wait_for_python_logs(state: &Path, id: u64) -> JobLogPage {
        for _ in 0..100 {
            let state = state.to_path_buf();
            let page = std::thread::spawn(move || {
                Python::attach(|py| Client { state_dir: state }.job_logs(id, 0, 16, py))
            })
            .join()
            .expect("Python logs thread panicked")
            .unwrap();
            if !page.records.is_empty() {
                return page;
            }
            async_engine::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} did not emit logs");
    }
}
