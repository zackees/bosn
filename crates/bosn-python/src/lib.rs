//! Deliberately small Python boundary over the Bosn-owned Rust daemon protocol.
//!
//! Product policy stays in Rust.  This extension only converts Python values
//! into the typed Rust client and maps its result into immutable Python values.

use bosn_core::{parse_and_plan_compose_yaml, parse_setup_config_locator};
use bosn_service::{
    Client as ServiceClient, DoctorReport as ServiceDoctorReport, JobLogPage as ServiceJobLogPage,
    JobStatus as ServiceJobStatus, MAX_REGISTRY_DIAGNOSTIC_PAGE, ManifestAppTaskJobRequest,
    ManifestConvergeJobRequest, ManifestEnsureJobRequest,
    ManifestVolumeGcApplyResult as ServiceManifestVolumeGcApplyResult,
    ManifestVolumeGcPreviewPage as ServiceManifestVolumeGcPreviewPage,
    RegistryResourcePage as ServiceRegistryResourcePage, SetupAdoptRequest, SetupAppTaskJobRequest,
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
mod calls;
mod checks;
mod types;
use calls::*;
use checks::*;
pub use types::*;

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

    /// Call one local-CI tool (the same contract as the `bosn_ci_*` MCP
    /// tools: `plan`, `run`, `status`, `list`, `logs`, `wait`, `cancel`,
    /// `report`, `runners`) with keyword arguments; returns the typed reply
    /// as a dict. Replies stay under 64 KiB; long work returns a run ID.
    #[pyo3(signature = (tool, **arguments))]
    fn ci(
        &self,
        tool: &str,
        arguments: Option<&Bound<'_, pyo3::types::PyDict>>,
        py: Python<'_>,
    ) -> PyResult<Py<PyAny>> {
        let json = py.import("json")?;
        let encoded: String = match arguments {
            Some(arguments) => json.call_method1("dumps", (arguments,))?.extract()?,
            None => "{}".into(),
        };
        let name = format!("bosn_ci_{tool}");
        let state_dir = self.state_dir.clone();
        let reply = py
            .detach(move || ci_call(&state_dir, &name, &encoded))
            .map_err(PyRuntimeError::new_err)?;
        Ok(json.call_method1("loads", (reply,))?.unbind())
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

    /// Read a bounded newest-first page of credential-safe setup ensure and
    /// native-manifest lifecycle event history from the already-running
    /// daemon. It never initializes state.
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
    #[pyo3(signature = (workspace, *, after = 0, limit = 64))]
    fn manifest_volume_gc_preview(
        &self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
        py: Python<'_>,
    ) -> PyResult<ManifestVolumeGcPreviewPage> {
        validate_registry_page(after, limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            manifest_volume_gc_preview(&state_dir, workspace, after, limit)
                .map(ManifestVolumeGcPreviewPage::from)
                .map_err(service_error)
        })
    }
    /// Preview durable manifest data that is excluded from normal GC. Apply
    /// accepts only one resulting opaque token with explicit confirmation.
    #[pyo3(signature = (workspace, *, after = 0, limit = 64))]
    fn manifest_volume_release_preview(
        &self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
        py: Python<'_>,
    ) -> PyResult<ManifestVolumeGcPreviewPage> {
        validate_registry_page(after, limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            manifest_volume_release_preview(&state_dir, workspace, after, limit)
                .map(ManifestVolumeGcPreviewPage::from)
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
    #[pyo3(signature = (workspace, candidate_token, *, confirm))]
    fn manifest_volume_gc_apply(
        &self,
        workspace: PathBuf,
        candidate_token: String,
        confirm: bool,
        py: Python<'_>,
    ) -> PyResult<ManifestVolumeGcApplyResult> {
        if !confirm {
            return Err(PyValueError::new_err(
                "manifest_volume_gc_apply requires confirm=True",
            ));
        }
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            manifest_volume_gc_apply(&state_dir, workspace, candidate_token)
                .map(ManifestVolumeGcApplyResult::from)
                .map_err(service_error)
        })
    }
    #[pyo3(signature = (workspace, candidate_token, *, confirm))]
    fn manifest_volume_release_apply(
        &self,
        workspace: PathBuf,
        candidate_token: String,
        confirm: bool,
        py: Python<'_>,
    ) -> PyResult<ManifestVolumeGcApplyResult> {
        if !confirm {
            return Err(PyValueError::new_err(
                "manifest_volume_release_apply requires confirm=True",
            ));
        }
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            manifest_volume_release_apply(&state_dir, workspace, candidate_token)
                .map(ManifestVolumeGcApplyResult::from)
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
    /// local TOML file, supported Compose YAML file (`.yaml`/`.yml`), or HTTPS
    /// document, then update Bosn's private cache) or
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

    /// Submit one named task into the already ensured Bosn setup application.
    /// The daemon derives and re-verifies the only container target and the
    /// command from the document; this method exposes no Docker or shell
    /// controls. Cancellation requests only stop the local exec client, so a
    /// cancelled job never claims the in-container command was stopped.
    #[pyo3(signature = (workspace, config_locator, *, policy, task_name, deadline_ms, output_limit))]
    #[allow(clippy::too_many_arguments)]
    fn submit_setup_app_task(
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
            submit_setup_app_task(
                &state_dir,
                SetupAppTaskJobRequest {
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
    /// Submit deterministic convergence of every stack declared in a legacy
    /// manifest. The legacy TOML schema has no dependency relation, so the
    /// daemon uses its canonical lexical stack order and accepts no root or
    /// Docker selector from Python.
    #[pyo3(signature = (workspace, manifest, *, deadline_ms, output_limit))]
    fn submit_manifest_converge(
        &self,
        workspace: PathBuf,
        manifest: String,
        deadline_ms: u64,
        output_limit: u32,
        py: Python<'_>,
    ) -> PyResult<u64> {
        validate_manifest_converge_input(&workspace, &manifest, deadline_ms, output_limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            submit_manifest_converge(
                &state_dir,
                ManifestConvergeJobRequest {
                    workspace,
                    manifest,
                    deadline: Duration::from_millis(deadline_ms),
                    output_limit: output_limit as usize,
                },
            )
            .map_err(service_error)
        })
    }
    /// Submit one bounded native ensure for an explicitly selected legacy
    /// manifest stack. The manifest path is relative to workspace; the daemon
    /// derives every image/container detail and rejects unsupported fields.
    #[pyo3(signature = (workspace, manifest, stack, *, deadline_ms, output_limit))]
    #[allow(clippy::too_many_arguments)]
    fn submit_manifest_ensure(
        &self,
        workspace: PathBuf,
        manifest: String,
        stack: String,
        deadline_ms: u64,
        output_limit: u32,
        py: Python<'_>,
    ) -> PyResult<u64> {
        validate_manifest_ensure_input(&workspace, &manifest, &stack, deadline_ms, output_limit)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            submit_manifest_ensure(
                &state_dir,
                ManifestEnsureJobRequest {
                    workspace,
                    manifest,
                    stack,
                    deadline: Duration::from_millis(deadline_ms),
                    output_limit: output_limit as usize,
                },
            )
            .map_err(service_error)
        })
    }
    /// Submit one named task inside an already ensured supported manifest
    /// stack. The daemon re-reads the declaration and accepts no command or
    /// container selector from Python.
    #[pyo3(signature = (workspace, manifest, stack, task_name, *, deadline_ms, output_limit))]
    #[allow(clippy::too_many_arguments)]
    fn submit_manifest_app_task(
        &self,
        workspace: PathBuf,
        manifest: String,
        stack: String,
        task_name: String,
        deadline_ms: u64,
        output_limit: u32,
        py: Python<'_>,
    ) -> PyResult<u64> {
        validate_manifest_ensure_input(&workspace, &manifest, &stack, deadline_ms, output_limit)?;
        validate_manifest_task_name(&task_name)?;
        let state_dir = self.state_dir.clone();
        py.detach(move || {
            submit_manifest_app_task(
                &state_dir,
                ManifestAppTaskJobRequest {
                    workspace,
                    manifest,
                    stack,
                    task_name,
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
    module.add_class::<ManifestVolumeGcCandidate>()?;
    module.add_class::<ManifestVolumeGcPreviewCounts>()?;
    module.add_class::<ManifestVolumeGcPreviewPage>()?;
    module.add_class::<ManifestVolumeGcApplyResult>()?;
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
mod tests;
