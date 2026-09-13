//! Explicit, inert setup planning for Rust callers.
//!
//! This is the composition point for setup acquisition and generated-asset
//! materialization.  It deliberately has no engine, registry, daemon, or
//! workspace-write dependency: callers receive an inspectable receipt that a
//! later, separately-authorized apply operation may consume.

use std::path::{Path, PathBuf};

use bosn_core::{SetupApp, SetupSource, SetupTask};
use std::collections::BTreeMap;

use crate::{
    KernalHttpTransport, MaterializedSetupSource, SetupAcquireError, SetupAcquirePolicy,
    SetupAssetStore, SetupCache, SetupMaterializeError, SetupRemoteTransport, SetupSourceKind,
    acquire_setup_document,
};

/// All caller-selected inputs for one setup plan.  There are no default paths
/// or implicit refresh policies at this boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupPlanRequest {
    /// Owner-private Bosn state root for the verified source cache and, for
    /// inline Dockerfiles, generated build assets.
    pub state_dir: PathBuf,
    /// Existing local directory that a later apply operation may mount.  This
    /// API only canonicalizes and observes it; it never writes there.
    pub workspace: PathBuf,
    /// Local configuration path or HTTPS configuration URL.
    pub locator: String,
    /// Either an explicit fresh read or an explicit verified-cache reuse.
    pub policy: SetupAcquirePolicy,
}

/// The application source presented by an inert setup plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupPlanAppSource {
    /// The setup file names an immutable image.  No generated build assets
    /// are necessary.
    PinnedImage { image: String },
    /// The setup file contains a Dockerfile, materialized under `asset_root`.
    InlineDockerfile { dockerfile_path: PathBuf },
}

/// Typed receipt of a setup plan.  The source bytes have been validated and
/// cached according to the request, while Docker and the Bosn daemon remain
/// untouched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupPlan {
    /// Acquisition type recorded in durable source provenance.
    pub source_kind: SetupSourceKind,
    /// SHA-256 of the exact, validated setup document bytes.
    pub content_sha256: String,
    /// Schema version accepted by the parser.
    pub schema_version: u64,
    /// Canonical absolute workspace directory.
    pub workspace_root: PathBuf,
    /// Content-addressed generated asset root for inline Dockerfiles only.
    pub asset_root: Option<PathBuf>,
    /// Sorted task names from the validated document.
    pub task_names: Vec<String>,
    /// The validated app declaration retained for a later typed operation.
    ///
    /// This is deliberately structured document data rather than raw source
    /// bytes or Docker arguments.  Front ends that only need an inert receipt
    /// should continue to expose [`Self::task_names`] rather than these values.
    pub app: SetupApp,
    /// Named task declarations retained from the same validated document as
    /// the receipt.  A setup-task executor validates these again before any
    /// engine call; they are never caller-provided command arguments.
    pub tasks: BTreeMap<String, SetupTask>,
    /// The bounded app-source shape, without exposing Dockerfile contents.
    pub app_source: SetupPlanAppSource,
}

/// Planning stops before side effects outside Bosn-owned cache/asset state.
#[derive(Debug)]
pub enum SetupPlanError {
    Acquisition(SetupAcquireError),
    Materialization(SetupMaterializeError),
}

impl std::fmt::Display for SetupPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquisition(error) => write!(formatter, "setup acquisition: {error}"),
            Self::Materialization(error) => write!(formatter, "setup materialization: {error}"),
        }
    }
}

impl std::error::Error for SetupPlanError {}

impl From<SetupAcquireError> for SetupPlanError {
    fn from(error: SetupAcquireError) -> Self {
        Self::Acquisition(error)
    }
}

impl From<SetupMaterializeError> for SetupPlanError {
    fn from(error: SetupMaterializeError) -> Self {
        Self::Materialization(error)
    }
}

/// Acquire and materialize one setup document using the production bounded
/// HTTPS transport.  The caller must use a kernal-api async runtime.
pub async fn plan_setup(request: SetupPlanRequest) -> Result<SetupPlan, SetupPlanError> {
    let transport = KernalHttpTransport::new()?;
    plan_setup_with_transport(request, &transport).await
}

/// Testable form of [`plan_setup`] with an explicit HTTPS transport.
///
/// The transport is never called for a local locator or for offline cache
/// reuse.  This function's only writes are the existing cache and, for an
/// inline Dockerfile, its generated owner-private asset tree.
pub async fn plan_setup_with_transport<T: SetupRemoteTransport>(
    request: SetupPlanRequest,
    transport: &T,
) -> Result<SetupPlan, SetupPlanError> {
    let cache = SetupCache::under_state_dir(&request.state_dir)?;
    let store = SetupAssetStore::under_state_dir(&request.state_dir)?;
    let resolved =
        acquire_setup_document(&cache, transport, &request.locator, request.policy).await?;
    let materialized = store.materialize(&resolved, &request.workspace)?;

    let app_source = match materialized.source() {
        MaterializedSetupSource::PinnedImage { image } => SetupPlanAppSource::PinnedImage {
            image: image.clone(),
        },
        MaterializedSetupSource::InlineDockerfile {
            dockerfile_path, ..
        } => SetupPlanAppSource::InlineDockerfile {
            dockerfile_path: dockerfile_path.clone(),
        },
    };
    debug_assert!(matches!(
        (&resolved.document.app.source, &app_source),
        (
            SetupSource::PinnedImage(_),
            SetupPlanAppSource::PinnedImage { .. }
        ) | (
            SetupSource::InlineDockerfile(_),
            SetupPlanAppSource::InlineDockerfile { .. }
        )
    ));
    Ok(SetupPlan {
        source_kind: materialized.provenance().source_kind,
        content_sha256: materialized.provenance().content_sha256.clone(),
        schema_version: materialized.provenance().schema_version,
        workspace_root: materialized.workspace_root().to_path_buf(),
        asset_root: materialized.asset_root().map(Path::to_path_buf),
        task_names: materialized.tasks().keys().cloned().collect(),
        app: materialized.app().clone(),
        tasks: materialized.tasks().clone(),
        app_source,
    })
}
