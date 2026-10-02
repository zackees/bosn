//! Small, authenticated Rust daemon foundation. Product protobuf remains private.

use bosn_core::{
    ManifestRoots, ResourceKind, ResourceLabels, ResourceState, Retention, Scope, SetupApp,
    SetupSource, SetupTask, parse_manifest_toml,
};
use bosn_engine::{
    CommandError, CommandResult, DockerDoctorReport, DockerDoctorState, DockerEngine, EngineEvent,
    GuestScpCommand, GuestScpEngine, GuestSshCommand, GuestSshEngine, RunOptions,
};
use bosn_generation::{
    ContextEntry, ExternalImageIdentity,
    collector::{CollectorLimits, collect_context},
    dockerfile::external_images,
    stack_generation_async, stack_generation_from_context,
};
use bosn_registry::{
    Event, ExecutionSession, Lease, ManifestVolumeGcPreview, ReadOnlyRegistry, ReconciliationProof,
    Registry, RegistryStatus, Resource, ResourceUse, SetupDone, SetupGcPreview,
    VolumeCreationIntent,
};
#[cfg(test)]
use bosn_setup::PreparedImageKind;
use bosn_setup::{
    ManifestBuildEntry, PreparedImage, SetupAcquirePolicy, SetupAppTaskRequest, SetupAssetStore,
    SetupEnsureEngine, SetupEnsureRequest as CoreSetupEnsureRequest, SetupEnsureResult,
    SetupHostDockerSocket, SetupHostDockerSocketSource, SetupImageEngine, SetupMacosGuest,
    SetupNamedVolume, SetupPlan, SetupPlanAppSource, SetupPlanRequest, SetupTaskRequest,
    SetupTmpfs, SetupTmpfsSize, SetupTmpfsSizeUnit, adopt_setup_app, ensure_setup_app,
    execute_setup_app_task, execute_setup_task, plan_setup, prepare_setup_image,
};
use jobs::{Jobs, Submission};
use kernal_api::{
    async_engine::{self, CancellationSource},
    daemon_frame_v1::{
        DaemonFrame, DaemonFrameCodec, DaemonFrameDecode, DaemonFrameKind, DaemonPayloadEncoding,
    },
    hash::Sha256Hasher,
    platform::{
        fs, host,
        ipc::{self, AsyncListener, AsyncStream, Endpoint, EndpointAddressCandidates},
    },
};
use prost::Message;
use secrets::{MaskStream, SecretMasker};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub mod act_engine;
pub mod act_registry;
pub mod act_runtime;
pub mod autostart;
pub mod ci;
mod client;
mod diagnostics;
mod diagnostics_validate;
mod dispatch;
mod executors;
mod gc_apply;
pub mod github_proxy;
mod job_api;
mod job_loop;
mod job_support;
pub mod jobs;
mod manifest_executors;
mod manifest_plan;
mod manifest_recovery;
mod manifest_runtime;
pub mod mcp;
mod registry_actor;
mod registry_api;
mod registry_records;
pub mod secrets;
mod service;
mod setup_adopt;
mod setup_executors;
mod task_executors;
mod transport;
pub mod unmanaged;
mod wire;
mod wire_validate;
pub use client::*;
pub use diagnostics::*;
use diagnostics_validate::*;
use dispatch::*;
pub use executors::*;
use gc_apply::*;
use job_api::*;
use job_loop::*;
use job_support::*;
pub use manifest_executors::*;
use manifest_plan::*;
use manifest_recovery::*;
use manifest_runtime::*;
use registry_actor::*;
pub use registry_api::*;
use registry_records::*;
pub use service::*;
pub use setup_adopt::*;
pub use setup_executors::*;
pub use task_executors::*;
pub use transport::*;
use wire::*;
use wire_validate::*;

pub const PROTOCOL_VERSION: u32 = 1;
const PAYLOAD_PROTOCOL: u32 = 0x4253_4e01;
const MAX_FRAME: usize = 1024 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(3);
const SETUP_PREPARE_MAX_DEADLINE: Duration = Duration::from_secs(5 * 60);
const SETUP_PREPARE_MAX_OUTPUT: usize = 8 * 1024 * 1024;
/// PID 1 of every Linux manifest container: idle until stopped, and exit
/// promptly on `docker stop`. It is a fixed daemon constant, never manifest
/// or caller text.
const MANIFEST_LINUX_IDLE_COMMAND: &str =
    "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done";
/// Declared manifest operations run whole build/CI workloads (a cold
/// Dockerfile build, or `act` driving a CI job), so their caller-selected
/// budget may exceed the five-minute setup-document bound. The budget is
/// still finite and caller-declared; exec output is still held in memory, so
/// the output ceiling stays bounded.
pub const MANIFEST_MAX_DEADLINE: Duration = Duration::from_secs(4 * 60 * 60);
pub const MANIFEST_MAX_OUTPUT: usize = 64 * 1024 * 1024;
/// Bounds of a manifest app task's follow lease (#357): long enough to
/// survive a busy host, short enough that a killed follower's job stops soon.
pub const FOLLOW_LEASE_MIN: Duration = Duration::from_secs(1);
pub const FOLLOW_LEASE_MAX: Duration = Duration::from_secs(10 * 60);
const SETUP_PREPARE_COMMAND_QUEUE: usize = 64;
const SETUP_PREPARE_EVENT_QUEUE: usize = 16;
/// Manifest builds and tasks (for example `act` running a CI job) emit
/// bursts faster than the job log actor drains them one record at a time.
/// The engine drops the exec rather than block when this queue is full, so
/// buffer up to 1024 chunks of at most 8 KiB (8 MiB) before that happens.
const MANIFEST_ENGINE_EVENT_QUEUE: usize = 1024;
/// One fixed engine version probe. This is intentionally independent of setup
/// job limits: diagnostic callers cannot select a deadline, output budget, or
/// any Docker command.
const DOCTOR_REGISTRY_DEADLINE: Duration = Duration::from_millis(500);
const DOCTOR_ENGINE_DEADLINE: Duration = Duration::from_millis(1500);
const DOCTOR_ENGINE_OUTPUT: usize = 512;
/// Restart recovery is deliberately bounded and only covers records created
/// by this native manifest runtime.  It never scans Docker for candidates.
const MANIFEST_RECOVERY_MAX_CONTRACTS: usize = 64;
const MANIFEST_RECOVERY_ENGINE_DEADLINE: Duration = Duration::from_secs(3);
const MANIFEST_RECOVERY_TOTAL_DEADLINE: Duration = Duration::from_secs(20);
/// A diagnostic page is deliberately small enough to fit comfortably in the
/// authenticated IPC frame and every public front end.
pub const MAX_REGISTRY_DIAGNOSTIC_PAGE: u32 = 64;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Registry(bosn_registry::Error),
    Deadline,
    Unauthorized,
    Random,
    EndpointOccupied(String),
    ActorClosed,
    Protocol(&'static str),
    /// A typed CI refusal or failure (`code` is stable, e.g. `refused`).
    Ci {
        code: String,
        message: String,
    },
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<bosn_registry::Error> for Error {
    fn from(e: bosn_registry::Error) -> Self {
        Self::Registry(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

mod python_v4;
#[cfg(test)]
mod tests;
pub use python_v4::*;
