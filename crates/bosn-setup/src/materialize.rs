//! Safe, content-addressed setup build-asset materialization.
//!
//! Setup documents are inert until this module is called with an explicit
//! workspace.  The workspace is observed through kernal-api, but is never
//! modified: generated Docker build assets live below an owner-private Bosn
//! state directory keyed by the validated document receipt's content hash.
//!
//! The available kernel filesystem facade deliberately does not expose an
//! `openat` capability sandbox.  Consequently this module only writes below a
//! Bosn-created, owner-private root, takes an advisory lock there, rejects
//! links and unexpected tree entries, and publishes the completion receipt
//! last.  A pre-existing root without that receipt is a failed/foreign partial
//! materialization and is refused rather than repaired in place.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write as _},
    path::{Component, Path, PathBuf},
};

use bosn_core::{
    MAX_COMPANION_FILE_BYTES, MAX_INLINE_DOCKERFILE_BYTES, SetupApp, SetupSource, SetupTask,
};
use kernal_api::{
    hash::sha256_bytes,
    platform::{fs, ipc},
};

use crate::{ResolvedSetupDocument, SetupProvenance};

/// Maximum receipt size, independently bounded from a setup document.
pub const MAX_ASSET_RECEIPT_BYTES: usize = 32 * 1024;
const ASSET_DIRECTORY: &str = "setup-assets";
const RECEIPT_NAME: &str = ".bosn-materialization-v1";
const LOCK_NAME: &str = ".bosn-materialization.lock";
const RECEIPT_MAGIC: &str = "BOSN-SETUP-ASSET-1";
const TEMPORARY_ATTEMPTS: u8 = 32;

/// A private content-addressed store for generated setup build assets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupAssetStore {
    directory: PathBuf,
}

/// One already-observed entry from a manifest Docker build context. The daemon
/// obtains these values through the bounded generation collector before asking
/// the setup layer to create private assets; this type is deliberately not a
/// host path or a Docker argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestBuildEntry {
    File { path: String, content: Vec<u8> },
    Directory { path: String },
    Symlink { path: String, target: String },
}

impl SetupAssetStore {
    /// Create or validate `<state_dir>/setup-assets` as Bosn-owned state.
    ///
    /// `state_dir` is explicit; no product directory or machine policy is
    /// inferred here.  Existing symlinks are rejected before the kernel is
    /// asked to create/harden a directory.
    pub fn under_state_dir(state_dir: impl AsRef<Path>) -> Result<Self, SetupMaterializeError> {
        let state_dir = state_dir.as_ref();
        ensure_private_directory(state_dir)?;
        let directory = state_dir.join(ASSET_DIRECTORY);
        ensure_private_directory(&directory)?;
        Ok(Self { directory })
    }

    /// The owner-private directory containing only generated setup assets.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Validate one explicit workspace and return an inert, deterministic plan.
    ///
    /// Pinned images intentionally produce no asset directory or writes.  An
    /// inline Dockerfile produces `Dockerfile` plus document companion files
    /// beneath `<store>/<setup-content-sha256>/`.
    pub fn materialize(
        &self,
        resolved: &ResolvedSetupDocument,
        workspace: impl AsRef<Path>,
    ) -> Result<MaterializedSetupPlan, SetupMaterializeError> {
        let workspace_root = validate_workspace(workspace.as_ref())?;
        let app = resolved.document.app.clone();
        let tasks = resolved.document.tasks.clone();
        let provenance = resolved.provenance.clone();

        match &resolved.document.app.source {
            SetupSource::PinnedImage(image) => Ok(MaterializedSetupPlan {
                workspace_root,
                asset_root: None,
                source: MaterializedSetupSource::PinnedImage {
                    image: image.clone(),
                },
                app,
                tasks,
                provenance,
            }),
            SetupSource::InlineDockerfile(dockerfile) => {
                let content_hash = validated_content_hash(&provenance.content_sha256)?;
                let expected = expected_assets(dockerfile, &resolved.document.files)?;
                let asset_root =
                    self.materialize_expected_assets(&content_hash, &expected, "Dockerfile")?;

                let dockerfile_path = asset_root.join("Dockerfile");
                let files = expected
                    .iter()
                    .filter(|asset| asset.relative != "Dockerfile")
                    .map(|asset| MaterializedAsset {
                        path: asset_root.join(&asset.relative),
                        content_sha256: match &asset.kind {
                            ExpectedAssetKind::File(content) => sha256_bytes(content).to_hex(),
                            _ => unreachable!("inline setup assets are regular files"),
                        },
                    })
                    .collect();
                Ok(MaterializedSetupPlan {
                    workspace_root,
                    asset_root: Some(asset_root),
                    source: MaterializedSetupSource::InlineDockerfile {
                        dockerfile_path,
                        files,
                    },
                    app,
                    tasks,
                    provenance,
                })
            }
        }
    }

    /// Materialize a bounded, already-selected manifest Docker context below
    /// Bosn-owned state.  This is intentionally narrower than Docker's raw
    /// build interface: callers supply content bytes, never a host context
    /// path, build args, tag, or Docker argv. `dockerfile` is a safe relative
    /// context label, never a host path.
    pub fn materialize_manifest_context(
        &self,
        content_sha256: &str,
        dockerfile: &str,
        entries: &[ManifestBuildEntry],
    ) -> Result<PathBuf, SetupMaterializeError> {
        let content_hash = validated_content_hash(content_sha256)?;
        if !valid_relative_asset_path(dockerfile) {
            return Err(SetupMaterializeError::InvalidAssetPath);
        }
        let mut expected = Vec::with_capacity(entries.len());
        let mut paths = BTreeSet::new();
        let mut total = 0_usize;
        for entry in entries {
            let path = manifest_entry_path(entry);
            if !valid_relative_asset_path(path)
                || matches!(path, RECEIPT_NAME | LOCK_NAME)
                || !paths.insert(path.to_owned())
            {
                return Err(SetupMaterializeError::InvalidAssetPath);
            }
            match entry {
                ManifestBuildEntry::File { content, .. } => {
                    if content.len() > MAX_COMPANION_FILE_BYTES {
                        return Err(SetupMaterializeError::InvalidAssetPath);
                    }
                    total = total
                        .checked_add(content.len())
                        .ok_or(SetupMaterializeError::InvalidAssetPath)?;
                    if total > 64 * 1024 * 1024 {
                        return Err(SetupMaterializeError::InvalidAssetPath);
                    }
                    expected.push(ExpectedAsset::file(path, content.clone()));
                }
                ManifestBuildEntry::Directory { .. } => {
                    expected.push(ExpectedAsset::directory(path));
                }
                ManifestBuildEntry::Symlink { target, .. } => {
                    if !safe_link_target(path, target) {
                        return Err(SetupMaterializeError::InvalidAssetPath);
                    }
                    // Pinned kernal-api can observe links but has no public
                    // capability to create one below a private root. Do not
                    // bypass Bosn's OS facade boundary with std::fs here.
                    return Err(SetupMaterializeError::SymlinkCreationUnavailable);
                }
            }
        }
        if !matches!(
            expected.iter().find(|asset| asset.relative == dockerfile),
            Some(ExpectedAsset {
                kind: ExpectedAssetKind::File(_),
                ..
            })
        ) {
            return Err(SetupMaterializeError::InvalidAssetPath);
        }
        expected.sort_by(|left, right| left.relative.cmp(&right.relative));
        self.materialize_expected_assets(&content_hash, &expected, dockerfile)
    }

    fn materialize_expected_assets(
        &self,
        content_hash: &str,
        expected: &[ExpectedAsset],
        dockerfile: &str,
    ) -> Result<PathBuf, SetupMaterializeError> {
        let asset_root = self.directory.join(content_hash);
        let created = ensure_private_directory(&asset_root)?;
        let lock_path = asset_root.join(LOCK_NAME);
        let lock = fs::open_lock_file(&lock_path).map_err(SetupMaterializeError::Filesystem)?;
        let _lock = fs::lock_exclusive(&lock).map_err(SetupMaterializeError::Filesystem)?;

        let receipt = receipt_bytes(content_hash, dockerfile, expected)?;
        let receipt_path = asset_root.join(RECEIPT_NAME);
        match fs::context_path_metadata_no_follow(&receipt_path) {
            Ok(metadata) if metadata.kind == fs::ContextPathKind::RegularFile => {
                verify_complete_assets(&asset_root, expected, &receipt)?;
            }
            Ok(_) => return Err(SetupMaterializeError::ExistingAssetsConflict),
            Err(error) if error.kind() == io::ErrorKind::NotFound && created => {
                write_expected_assets(&asset_root, expected)?;
                atomic_write_new(&receipt_path, &receipt)?;
                fs::sync_directory(&asset_root).map_err(SetupMaterializeError::Filesystem)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(SetupMaterializeError::IncompleteAssets);
            }
            Err(error) => return Err(SetupMaterializeError::Filesystem(error)),
        }
        Ok(asset_root)
    }
}

/// The build source selected by a materialized plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializedSetupSource {
    PinnedImage {
        image: String,
    },
    InlineDockerfile {
        dockerfile_path: PathBuf,
        files: Vec<MaterializedAsset>,
    },
}

/// A companion build asset that is safe to pass to a later build operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedAsset {
    pub path: PathBuf,
    pub content_sha256: String,
}

/// Inert result of setup asset materialization.  This type invokes no engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedSetupPlan {
    workspace_root: PathBuf,
    asset_root: Option<PathBuf>,
    source: MaterializedSetupSource,
    app: SetupApp,
    tasks: BTreeMap<String, SetupTask>,
    provenance: SetupProvenance,
}

impl MaterializedSetupPlan {
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn asset_root(&self) -> Option<&Path> {
        self.asset_root.as_deref()
    }

    pub fn source(&self) -> &MaterializedSetupSource {
        &self.source
    }

    pub fn app(&self) -> &SetupApp {
        &self.app
    }

    pub fn tasks(&self) -> &BTreeMap<String, SetupTask> {
        &self.tasks
    }

    /// The validated acquisition receipt is retained privately in the plan;
    /// callers can inspect it but cannot mutate the plan's provenance.
    pub fn provenance(&self) -> &SetupProvenance {
        &self.provenance
    }
}

/// Why setup materialization stopped without invoking any engine.
#[derive(Debug)]
pub enum SetupMaterializeError {
    InvalidWorkspace,
    InvalidContentHash,
    InvalidAssetPath,
    ExistingAssetsConflict,
    IncompleteAssets,
    /// The pinned kernal-api revision has no public, safe private-root link
    /// creation facade. Manifest link transport must remain fail-closed until
    /// that facade exists.
    SymlinkCreationUnavailable,
    Filesystem(io::Error),
}

impl std::fmt::Display for SetupMaterializeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWorkspace => {
                formatter.write_str("setup workspace is not a usable local directory")
            }
            Self::InvalidContentHash => {
                formatter.write_str("setup provenance content hash is invalid")
            }
            Self::InvalidAssetPath => formatter.write_str("setup generated asset path is invalid"),
            Self::ExistingAssetsConflict => {
                formatter.write_str("existing setup assets conflict with the validated receipt")
            }
            Self::IncompleteAssets => {
                formatter.write_str("existing setup assets are incomplete and were not repaired")
            }
            Self::SymlinkCreationUnavailable => formatter
                .write_str("kernal-api does not provide safe private-root symlink creation"),
            Self::Filesystem(_) => formatter.write_str("setup asset filesystem operation failed"),
        }
    }
}

impl std::error::Error for SetupMaterializeError {}

fn validate_workspace(path: &Path) -> Result<PathBuf, SetupMaterializeError> {
    if !path.is_absolute() {
        return Err(SetupMaterializeError::InvalidWorkspace);
    }
    let metadata = fs::context_path_metadata_no_follow(path)
        .map_err(|_| SetupMaterializeError::InvalidWorkspace)?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err(SetupMaterializeError::InvalidWorkspace);
    }
    let canonical =
        fs::canonical_context_path(path).map_err(|_| SetupMaterializeError::InvalidWorkspace)?;
    let canonical_metadata = fs::context_path_metadata_no_follow(&canonical)
        .map_err(|_| SetupMaterializeError::InvalidWorkspace)?;
    if canonical_metadata.kind != fs::ContextPathKind::Directory {
        return Err(SetupMaterializeError::InvalidWorkspace);
    }
    Ok(canonical)
}

/// Ensure a product-owned private directory only after rejecting an existing
/// non-directory/link final component.  The boolean says whether this call
/// observed it absent before asking the kernel to create it.
fn ensure_private_directory(path: &Path) -> Result<bool, SetupMaterializeError> {
    let was_absent = match fs::context_path_metadata_no_follow(path) {
        Ok(metadata) if metadata.kind == fs::ContextPathKind::Directory => false,
        Ok(_) => return Err(SetupMaterializeError::ExistingAssetsConflict),
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => return Err(SetupMaterializeError::Filesystem(error)),
    };
    ipc::ensure_owner_private_directory(path).map_err(SetupMaterializeError::Filesystem)?;
    let metadata =
        fs::context_path_metadata_no_follow(path).map_err(SetupMaterializeError::Filesystem)?;
    if metadata.kind != fs::ContextPathKind::Directory {
        return Err(SetupMaterializeError::ExistingAssetsConflict);
    }
    Ok(was_absent)
}

fn validated_content_hash(value: &str) -> Result<String, SetupMaterializeError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SetupMaterializeError::InvalidContentHash);
    }
    Ok(value.into())
}

#[derive(Clone, Debug)]
struct ExpectedAsset {
    relative: String,
    kind: ExpectedAssetKind,
}

#[derive(Clone, Debug)]
enum ExpectedAssetKind {
    File(Vec<u8>),
    Directory,
    // This remains part of the authenticated receipt grammar even while the
    // pinned kernel lacks a safe private-root link-creation facade.
    #[allow(dead_code)]
    Symlink(String),
}

impl ExpectedAsset {
    fn file(path: &str, content: Vec<u8>) -> Self {
        Self {
            relative: path.into(),
            kind: ExpectedAssetKind::File(content),
        }
    }

    fn directory(path: &str) -> Self {
        Self {
            relative: path.into(),
            kind: ExpectedAssetKind::Directory,
        }
    }
}

fn expected_assets(
    dockerfile: &str,
    files: &[bosn_core::CompanionFile],
) -> Result<Vec<ExpectedAsset>, SetupMaterializeError> {
    if dockerfile.len() > MAX_INLINE_DOCKERFILE_BYTES {
        return Err(SetupMaterializeError::ExistingAssetsConflict);
    }
    let mut output = vec![ExpectedAsset {
        relative: "Dockerfile".into(),
        kind: ExpectedAssetKind::File(dockerfile.as_bytes().to_vec()),
    }];
    let mut names = BTreeSet::from(["Dockerfile".to_owned()]);
    for file in files {
        if file.content.len() > MAX_COMPANION_FILE_BYTES
            || !valid_relative_asset_path(&file.path)
            || matches!(file.path.as_str(), RECEIPT_NAME | LOCK_NAME)
            || !names.insert(file.path.clone())
        {
            return Err(SetupMaterializeError::InvalidAssetPath);
        }
        output.push(ExpectedAsset {
            relative: file.path.clone(),
            kind: ExpectedAssetKind::File(file.content.as_bytes().to_vec()),
        });
    }
    Ok(output)
}

fn valid_relative_asset_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    !path.is_empty()
        && path.len() <= 4096
        && !path.contains('\\')
        && !path.contains('\0')
        && !path.starts_with('/')
        // A Windows drive-relative spelling is not safe as a generated-path
        // component even on Unix, where `Path` would otherwise call it normal.
        && !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        && path.split('/').all(|component| {
            !component.is_empty() && component != "." && component != ".."
        })
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn manifest_entry_path(entry: &ManifestBuildEntry) -> &str {
    match entry {
        ManifestBuildEntry::File { path, .. }
        | ManifestBuildEntry::Directory { path }
        | ManifestBuildEntry::Symlink { path, .. } => path,
    }
}

/// Accept only a non-absolute UTF-8 target whose lexical resolution from the
/// link's parent remains within the materialized context. The target need not
/// exist: Docker permits a dangling in-context link, but it must never spell an
/// escape from the private context root.
fn safe_link_target(link_path: &str, target: &str) -> bool {
    if target.is_empty()
        || target.contains(['\\', '\0', ':'])
        || target.starts_with('/')
        || !valid_relative_asset_path(link_path)
    {
        return false;
    }
    let mut resolved: Vec<&str> = link_path.split('/').collect();
    resolved.pop();
    for component in target.split('/') {
        match component {
            "" => return false,
            "." => (),
            ".." => {
                if resolved.pop().is_none() {
                    return false;
                }
            }
            normal if !normal.contains(['\\', '\0', ':']) => resolved.push(normal),
            _ => return false,
        }
    }
    true
}

fn receipt_bytes(
    content_hash: &str,
    dockerfile: &str,
    expected: &[ExpectedAsset],
) -> Result<Vec<u8>, SetupMaterializeError> {
    let mut receipt =
        format!("{RECEIPT_MAGIC}\ncontent-sha256={content_hash}\ndockerfile-path={dockerfile}\n")
            .into_bytes();
    for asset in expected {
        match &asset.kind {
            ExpectedAssetKind::File(content) => {
                receipt.extend_from_slice(b"F\t");
                receipt.extend_from_slice(asset.relative.as_bytes());
                receipt.push(b'\t');
                receipt.extend_from_slice(sha256_bytes(content).to_hex().as_bytes());
                receipt.push(b'\n');
            }
            ExpectedAssetKind::Directory => {
                receipt.extend_from_slice(b"D\t");
                receipt.extend_from_slice(asset.relative.as_bytes());
                receipt.push(b'\n');
            }
            ExpectedAssetKind::Symlink(target) => {
                receipt.extend_from_slice(b"L\t");
                receipt.extend_from_slice(asset.relative.as_bytes());
                receipt.push(b'\t');
                receipt.extend_from_slice(target.as_bytes());
                receipt.push(b'\n');
            }
        }
    }
    if receipt.len() > MAX_ASSET_RECEIPT_BYTES {
        return Err(SetupMaterializeError::ExistingAssetsConflict);
    }
    Ok(receipt)
}

fn write_expected_assets(
    asset_root: &Path,
    expected: &[ExpectedAsset],
) -> Result<(), SetupMaterializeError> {
    for asset in expected {
        let path = asset_path(asset_root, &asset.relative)?;
        match &asset.kind {
            ExpectedAssetKind::File(content) => {
                let parent = path
                    .parent()
                    .ok_or(SetupMaterializeError::InvalidAssetPath)?;
                ensure_asset_parent(asset_root, parent)?;
                atomic_write_new(&path, content)?;
            }
            ExpectedAssetKind::Directory => {
                ensure_asset_parent(asset_root, &path)?;
            }
            ExpectedAssetKind::Symlink(_) => {
                return Err(SetupMaterializeError::SymlinkCreationUnavailable);
            }
        }
    }
    Ok(())
}

fn asset_path(asset_root: &Path, relative: &str) -> Result<PathBuf, SetupMaterializeError> {
    if !valid_relative_asset_path(relative) {
        return Err(SetupMaterializeError::InvalidAssetPath);
    }
    let path = asset_root.join(relative);
    if !path.starts_with(asset_root) {
        return Err(SetupMaterializeError::InvalidAssetPath);
    }
    Ok(path)
}

fn ensure_asset_parent(asset_root: &Path, parent: &Path) -> Result<(), SetupMaterializeError> {
    let relative = parent
        .strip_prefix(asset_root)
        .map_err(|_| SetupMaterializeError::InvalidAssetPath)?;
    let mut current = asset_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(SetupMaterializeError::InvalidAssetPath);
        };
        current.push(component);
        ensure_private_directory(&current)?;
    }
    Ok(())
}

fn atomic_write_new(path: &Path, bytes: &[u8]) -> Result<(), SetupMaterializeError> {
    let parent = path
        .parent()
        .ok_or(SetupMaterializeError::InvalidAssetPath)?;
    match fs::context_path_metadata_no_follow(path) {
        Ok(metadata) if metadata.kind == fs::ContextPathKind::RegularFile => {
            let existing = fs::read_private_regular_file_bounded(path, bytes.len())
                .map_err(SetupMaterializeError::Filesystem)?;
            if existing == bytes {
                return Ok(());
            }
            return Err(SetupMaterializeError::ExistingAssetsConflict);
        }
        Ok(_) => return Err(SetupMaterializeError::ExistingAssetsConflict),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(SetupMaterializeError::Filesystem(error)),
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SetupMaterializeError::InvalidAssetPath)?;
    for attempt in 0..TEMPORARY_ATTEMPTS {
        let temporary = parent.join(format!(".{name}.bosn-{attempt}"));
        let mut file = match fs::create_private_file(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SetupMaterializeError::Filesystem(error)),
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            return Err(SetupMaterializeError::Filesystem(error));
        }
        drop(file);
        fs::replace_file(&temporary, path).map_err(SetupMaterializeError::Filesystem)?;
        fs::sync_directory(parent).map_err(SetupMaterializeError::Filesystem)?;
        return Ok(());
    }
    Err(SetupMaterializeError::Filesystem(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "setup asset temporary-name space exhausted",
    )))
}

fn verify_complete_assets(
    asset_root: &Path,
    expected: &[ExpectedAsset],
    receipt: &[u8],
) -> Result<(), SetupMaterializeError> {
    let receipt_path = asset_root.join(RECEIPT_NAME);
    let existing_receipt =
        fs::read_private_regular_file_bounded(&receipt_path, MAX_ASSET_RECEIPT_BYTES)
            .map_err(|_| SetupMaterializeError::ExistingAssetsConflict)?;
    if existing_receipt != receipt {
        return Err(SetupMaterializeError::ExistingAssetsConflict);
    }

    let mut allowed_files = BTreeSet::from([RECEIPT_NAME.to_owned(), LOCK_NAME.to_owned()]);
    let mut allowed_directories = BTreeSet::new();
    let mut allowed_links = BTreeMap::new();
    for asset in expected {
        match &asset.kind {
            ExpectedAssetKind::File(content) => {
                allowed_files.insert(asset.relative.clone());
                let path = asset_path(asset_root, &asset.relative)?;
                let bytes = fs::read_private_regular_file_bounded(&path, content.len())
                    .map_err(|_| SetupMaterializeError::ExistingAssetsConflict)?;
                if bytes != *content {
                    return Err(SetupMaterializeError::ExistingAssetsConflict);
                }
            }
            ExpectedAssetKind::Directory => {
                allowed_directories.insert(asset.relative.clone());
            }
            ExpectedAssetKind::Symlink(target) => {
                allowed_links.insert(asset.relative.clone(), target.clone());
            }
        }
        let mut parent = Path::new(&asset.relative).parent();
        while let Some(directory) = parent {
            if directory.as_os_str().is_empty() {
                break;
            }
            allowed_directories.insert(directory.to_string_lossy().replace('\\', "/"));
            parent = directory.parent();
        }
    }

    for entry in fs::DirectoryWalk::new(asset_root.to_path_buf())
        .sorted(true)
        .walk()
    {
        let entry = entry.map_err(SetupMaterializeError::Filesystem)?;
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(asset_root)
            .map_err(|_| SetupMaterializeError::ExistingAssetsConflict)?
            .to_string_lossy()
            .replace('\\', "/");
        if (!entry.is_file() && !entry.is_directory() && !entry.is_symbolic_link())
            || (entry.is_file() && !allowed_files.contains(&relative))
            || (entry.is_directory() && !allowed_directories.contains(&relative))
            || (entry.is_symbolic_link()
                && fs::read_context_link(entry.path())
                    .ok()
                    .and_then(|target| target.to_str().map(str::to_owned))
                    .as_ref()
                    != allowed_links.get(&relative))
        {
            return Err(SetupMaterializeError::ExistingAssetsConflict);
        }
    }
    Ok(())
}

/// Revalidate the owner-private build tree named by an inert setup plan before
/// it is handed to a Docker build.  This deliberately treats the durable
/// receipt as untrusted input: a changed file, link, extra build-context entry,
/// or mismatched content-addressed root fails closed.
pub(crate) fn verify_materialized_assets(
    content_hash: &str,
    asset_root: &Path,
) -> Result<(), SetupMaterializeError> {
    let content_hash = validated_content_hash(content_hash)?;
    match fs::context_path_metadata_no_follow(asset_root) {
        Ok(metadata) if metadata.kind == fs::ContextPathKind::Directory => {}
        _ => return Err(SetupMaterializeError::ExistingAssetsConflict),
    }
    match asset_root.parent() {
        Some(parent) => match fs::context_path_metadata_no_follow(parent) {
            Ok(metadata) if metadata.kind == fs::ContextPathKind::Directory => {}
            _ => return Err(SetupMaterializeError::ExistingAssetsConflict),
        },
        None => return Err(SetupMaterializeError::ExistingAssetsConflict),
    }
    if asset_root.file_name().and_then(|value| value.to_str()) != Some(content_hash.as_str())
        || asset_root
            .parent()
            .and_then(|value| value.file_name())
            .and_then(|value| value.to_str())
            != Some(ASSET_DIRECTORY)
    {
        return Err(SetupMaterializeError::ExistingAssetsConflict);
    }

    let receipt_path = asset_root.join(RECEIPT_NAME);
    let receipt = fs::read_private_regular_file_bounded(&receipt_path, MAX_ASSET_RECEIPT_BYTES)
        .map_err(|_| SetupMaterializeError::ExistingAssetsConflict)?;
    let receipt_text =
        std::str::from_utf8(&receipt).map_err(|_| SetupMaterializeError::ExistingAssetsConflict)?;
    let mut lines = receipt_text.split_terminator('\n');
    if lines.next() != Some(RECEIPT_MAGIC)
        || lines.next() != Some(&format!("content-sha256={content_hash}"))
        || !receipt_text.ends_with('\n')
    {
        return Err(SetupMaterializeError::ExistingAssetsConflict);
    }

    let dockerfile = lines
        .next()
        .and_then(|line| line.strip_prefix("dockerfile-path="))
        .filter(|path| valid_relative_asset_path(path))
        .ok_or(SetupMaterializeError::ExistingAssetsConflict)?;
    let mut expected = Vec::new();
    let mut names = BTreeSet::new();
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        let (kind, relative, value) = match fields.as_slice() {
            ["F", relative, digest] if valid_content_hash(digest) => ('F', *relative, *digest),
            ["D", relative] => ('D', *relative, ""),
            ["L", relative, target] if safe_link_target(relative, target) => {
                ('L', *relative, *target)
            }
            _ => return Err(SetupMaterializeError::ExistingAssetsConflict),
        };
        if !valid_relative_asset_path(relative) || !names.insert(relative.to_owned()) {
            return Err(SetupMaterializeError::ExistingAssetsConflict);
        }
        expected.push((kind, relative.to_owned(), value.to_owned()));
    }
    if !matches!(
        expected.iter().find(|(_, path, _)| path == dockerfile),
        Some(('F', _, _))
    ) {
        return Err(SetupMaterializeError::ExistingAssetsConflict);
    }

    let mut canonical =
        format!("{RECEIPT_MAGIC}\ncontent-sha256={content_hash}\ndockerfile-path={dockerfile}\n")
            .into_bytes();
    let mut allowed_files = BTreeSet::from([RECEIPT_NAME.to_owned(), LOCK_NAME.to_owned()]);
    let mut allowed_directories = BTreeSet::new();
    let mut allowed_links = BTreeMap::new();
    for (kind, relative, value) in &expected {
        canonical.extend_from_slice(kind.to_string().as_bytes());
        canonical.push(b'\t');
        canonical.extend_from_slice(relative.as_bytes());
        if *kind != 'D' {
            canonical.push(b'\t');
            canonical.extend_from_slice(value.as_bytes());
        }
        canonical.push(b'\n');
        match kind {
            'F' => {
                allowed_files.insert(relative.clone());
                let bytes = fs::read_private_regular_file_bounded(
                    &asset_path(asset_root, relative)?,
                    MAX_COMPANION_FILE_BYTES,
                )
                .map_err(|_| SetupMaterializeError::ExistingAssetsConflict)?;
                if sha256_bytes(&bytes).to_hex() != *value {
                    return Err(SetupMaterializeError::ExistingAssetsConflict);
                }
            }
            'D' => {
                allowed_directories.insert(relative.clone());
            }
            'L' => {
                allowed_links.insert(relative.clone(), value.clone());
            }
            _ => unreachable!(),
        }
        let mut parent = Path::new(relative).parent();
        while let Some(directory) = parent {
            if directory.as_os_str().is_empty() {
                break;
            }
            allowed_directories.insert(directory.to_string_lossy().replace('\\', "/"));
            parent = directory.parent();
        }
    }
    if receipt != canonical {
        return Err(SetupMaterializeError::ExistingAssetsConflict);
    }
    for entry in fs::DirectoryWalk::new(asset_root.to_path_buf())
        .sorted(true)
        .walk()
    {
        let entry = entry.map_err(SetupMaterializeError::Filesystem)?;
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(asset_root)
            .map_err(|_| SetupMaterializeError::ExistingAssetsConflict)?
            .to_string_lossy()
            .replace('\\', "/");
        if (!entry.is_file() && !entry.is_directory() && !entry.is_symbolic_link())
            || (entry.is_file() && !allowed_files.contains(&relative))
            || (entry.is_directory() && !allowed_directories.contains(&relative))
            || (entry.is_symbolic_link()
                && fs::read_context_link(entry.path())
                    .ok()
                    .and_then(|target| target.to_str().map(str::to_owned))
                    .as_ref()
                    != allowed_links.get(&relative))
        {
            return Err(SetupMaterializeError::ExistingAssetsConflict);
        }
    }
    Ok(())
}

fn valid_content_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SetupProvenance, SetupSourceKind};
    use bosn_core::parse_setup_document_toml;

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn resolved(source: &str) -> ResolvedSetupDocument {
        ResolvedSetupDocument {
            document: parse_setup_document_toml(source).unwrap(),
            provenance: SetupProvenance {
                requested_locator: "setup.toml".into(),
                resolved_locator: None,
                content_sha256: sha256_bytes(source.as_bytes()).to_hex(),
                schema_version: 1,
                fetched_at_unix_seconds: 0,
                source_kind: SetupSourceKind::LocalFile,
            },
        }
    }

    fn inline() -> ResolvedSetupDocument {
        resolved(
            r#"version = 1
[app]
dockerfile = "FROM scratch\nCOPY hello.txt /hello.txt\n"
[[file]]
path = "hello.txt"
content = "hello\n"
[[file]]
path = "nested/config.txt"
content = "config\n"
"#,
        )
    }

    fn store_and_workspace() -> (tempfile::TempDir, SetupAssetStore, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        ipc::ensure_owner_private_directory(&workspace).unwrap();
        let store = SetupAssetStore::under_state_dir(&state).unwrap();
        (temp, store, workspace)
    }

    #[test]
    fn inline_assets_are_private_content_addressed_and_idempotent() {
        let (_temp, store, workspace) = store_and_workspace();
        let document = inline();
        let first = store.materialize(&document, &workspace).unwrap();
        let root = first.asset_root().unwrap().to_path_buf();
        assert_eq!(
            root.file_name().unwrap().to_string_lossy(),
            document.provenance.content_sha256,
        );
        assert_eq!(
            fs::read_private_regular_file_bounded(&root.join("Dockerfile"), 1024).unwrap(),
            b"FROM scratch\nCOPY hello.txt /hello.txt\n"
        );
        assert_eq!(
            fs::read_private_regular_file_bounded(&root.join("nested/config.txt"), 1024).unwrap(),
            b"config\n"
        );
        assert!(matches!(
            first.source(),
            MaterializedSetupSource::InlineDockerfile { .. }
        ));
        let second = store.materialize(&document, &workspace).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn pinned_image_never_creates_a_generated_asset_root() {
        let (_temp, store, workspace) = store_and_workspace();
        let document = resolved(&format!(
            "version = 1\n[app]\nimage = 'example.invalid/app@sha256:{DIGEST}'\n[[file]]\npath = 'ignored.txt'\ncontent = 'ignored'\n"
        ));
        let plan = store.materialize(&document, &workspace).unwrap();
        assert!(plan.asset_root().is_none());
        let mut cursor = fs::DirectoryCursor::open(store.directory()).unwrap();
        assert!(cursor.next_entry().unwrap().is_none());
    }

    #[test]
    fn invalid_companion_paths_fail_before_asset_root_creation() {
        let (_temp, store, workspace) = store_and_workspace();
        let mut document = inline();
        document.document.files[0].path = "../escape".into();
        assert!(matches!(
            store.materialize(&document, &workspace),
            Err(SetupMaterializeError::InvalidAssetPath)
        ));
        let mut cursor = fs::DirectoryCursor::open(store.directory()).unwrap();
        assert!(cursor.next_entry().unwrap().is_none());
    }

    #[test]
    fn reserved_or_cross_platform_ambiguous_asset_paths_are_refused() {
        let (_temp, store, workspace) = store_and_workspace();
        for path in [RECEIPT_NAME, LOCK_NAME, "nested/.", "C:relative"] {
            let mut document = inline();
            document.document.files[0].path = path.into();
            assert!(matches!(
                store.materialize(&document, &workspace),
                Err(SetupMaterializeError::InvalidAssetPath)
            ));
        }
        let mut cursor = fs::DirectoryCursor::open(store.directory()).unwrap();
        assert!(cursor.next_entry().unwrap().is_none());
    }

    #[test]
    fn manifest_context_preserves_alternate_dockerfile_and_empty_directory() {
        let (_temp, store, _workspace) = store_and_workspace();
        let root = store
            .materialize_manifest_context(
                DIGEST,
                "docker/Dockerfile",
                &[
                    ManifestBuildEntry::Directory {
                        path: "docker".into(),
                    },
                    ManifestBuildEntry::Directory {
                        path: "empty".into(),
                    },
                    ManifestBuildEntry::File {
                        path: "docker/Dockerfile".into(),
                        content: b"FROM scratch\nCOPY payload /payload\n".to_vec(),
                    },
                    ManifestBuildEntry::File {
                        path: "payload".into(),
                        content: b"ok\n".to_vec(),
                    },
                ],
            )
            .unwrap();
        assert_eq!(
            fs::context_path_metadata_no_follow(&root.join("empty"))
                .unwrap()
                .kind,
            fs::ContextPathKind::Directory
        );
        assert!(verify_materialized_assets(DIGEST, &root).is_ok());
    }

    #[test]
    fn manifest_context_rejects_escaping_links_and_fails_closed_without_kernel_creation() {
        let (_temp, store, _workspace) = store_and_workspace();
        for (target, expected) in [
            ("../outside", SetupMaterializeError::InvalidAssetPath),
            ("payload", SetupMaterializeError::SymlinkCreationUnavailable),
        ] {
            let result = store.materialize_manifest_context(
                DIGEST,
                "Dockerfile",
                &[
                    ManifestBuildEntry::File {
                        path: "Dockerfile".into(),
                        content: b"FROM scratch\n".to_vec(),
                    },
                    ManifestBuildEntry::Symlink {
                        path: "link".into(),
                        target: target.into(),
                    },
                ],
            );
            assert_eq!(result.unwrap_err().to_string(), expected.to_string());
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepare_verification_rejects_a_mutated_typed_link() {
        let (_temp, store, _workspace) = store_and_workspace();
        let root = store.directory().join(DIGEST);
        ipc::ensure_owner_private_directory(&root).unwrap();
        let expected = [
            ExpectedAsset::file("Dockerfile", b"FROM scratch\n".to_vec()),
            ExpectedAsset {
                relative: "alias".into(),
                kind: ExpectedAssetKind::Symlink("Dockerfile".into()),
            },
        ];
        atomic_write_new(
            &root.join(RECEIPT_NAME),
            &receipt_bytes(DIGEST, "Dockerfile", &expected).unwrap(),
        )
        .unwrap();
        atomic_write_new(&root.join("Dockerfile"), b"FROM scratch\n").unwrap();
        std::os::unix::fs::symlink("Dockerfile", root.join("alias")).unwrap();
        assert!(verify_materialized_assets(DIGEST, &root).is_ok());
        std::fs::remove_file(root.join("alias")).unwrap();
        std::os::unix::fs::symlink("../outside", root.join("alias")).unwrap();
        assert!(matches!(
            verify_materialized_assets(DIGEST, &root),
            Err(SetupMaterializeError::ExistingAssetsConflict)
        ));
    }

    #[test]
    fn partial_or_tampered_existing_assets_are_refused() {
        let (_temp, store, workspace) = store_and_workspace();
        let document = inline();
        let root = store.directory().join(&document.provenance.content_sha256);
        ipc::ensure_owner_private_directory(&root).unwrap();
        atomic_write_new(&root.join("Dockerfile"), b"partial").unwrap();
        assert!(matches!(
            store.materialize(&document, &workspace),
            Err(SetupMaterializeError::IncompleteAssets)
        ));

        let alternate = resolved(
            r#"version = 1
[app]
dockerfile = "FROM scratch\n"
"#,
        );
        let plan = store.materialize(&alternate, &workspace).unwrap();
        let root = plan.asset_root().unwrap();
        let replacement = root.join(".tamper");
        atomic_write_new(&replacement, b"bad").unwrap();
        fs::replace_file(&replacement, &root.join("Dockerfile")).unwrap();
        assert!(matches!(
            store.materialize(&alternate, &workspace),
            Err(SetupMaterializeError::ExistingAssetsConflict)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlinked_asset_subtree_is_refused_without_following_it() {
        let (temp, store, workspace) = store_and_workspace();
        let document = inline();
        let root = store
            .materialize(&document, &workspace)
            .unwrap()
            .asset_root()
            .unwrap()
            .to_path_buf();
        std::fs::remove_file(root.join("nested/config.txt")).unwrap();
        std::fs::remove_dir(root.join("nested")).unwrap();
        let outside = temp.path().join("outside");
        ipc::ensure_owner_private_directory(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("nested")).unwrap();
        assert!(matches!(
            store.materialize(&document, &workspace),
            Err(SetupMaterializeError::ExistingAssetsConflict)
        ));
        assert!(fs::context_path_metadata_no_follow(&outside.join("config.txt")).is_err());
    }

    #[test]
    fn workspace_is_canonicalized_and_never_written() {
        let (_temp, store, workspace) = store_and_workspace();
        let plan = store.materialize(&inline(), &workspace).unwrap();
        assert_ne!(plan.asset_root().unwrap(), plan.workspace_root());
        assert!(
            !plan
                .asset_root()
                .unwrap()
                .starts_with(plan.workspace_root())
        );
        let mut cursor = fs::DirectoryCursor::open(&workspace).unwrap();
        assert!(cursor.next_entry().unwrap().is_none());
    }
}
