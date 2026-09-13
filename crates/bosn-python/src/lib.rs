//! Deliberately small Python boundary over the Bosn-owned Rust daemon protocol.
//!
//! Product policy stays in Rust.  This extension only converts Python values
//! into the typed Rust client and maps its result into immutable Python values.

use bosn_service::{Client as ServiceClient, Status as ServiceStatus};
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

const PYTHON_PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

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

fn status(state_dir: &Path) -> Result<ServiceStatus, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async { ServiceClient::for_state(state_dir)?.status().await })
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
    module.add_class::<SetupPlan>()?;
    module.add_function(wrap_pyfunction!(native_version, module)?)?;
    module.add_function(wrap_pyfunction!(protocol_version, module)?)?;
    module.add_function(wrap_pyfunction!(run_mcp, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_version_matches_python_distribution() {
        assert_eq!(native_version(), "0.1.3");
    }
}
