//! Deliberately small Python boundary over the Bosn-owned Rust daemon protocol.
//!
//! Product policy stays in Rust.  This extension only converts Python values
//! into the typed Rust client and maps its result into immutable Python values.

use bosn_service::{Client as ServiceClient, Status as ServiceStatus};
use kernal_api::async_engine::RuntimeBuilder;
use pyo3::{exceptions::PyRuntimeError, prelude::*};
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
