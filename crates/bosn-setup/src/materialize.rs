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
                let asset_root = self.directory.join(&content_hash);
                let created = ensure_private_directory(&asset_root)?;
                let lock_path = asset_root.join(LOCK_NAME);
                let lock =
                    fs::open_lock_file(&lock_path).map_err(SetupMaterializeError::Filesystem)?;
                let _lock = fs::lock_exclusive(&lock).map_err(SetupMaterializeError::Filesystem)?;

                let receipt = receipt_bytes(&content_hash, &expected)?;
                let receipt_path = asset_root.join(RECEIPT_NAME);
                match fs::context_path_metadata_no_follow(&receipt_path) {
                    Ok(metadata) if metadata.kind == fs::ContextPathKind::RegularFile => {
                        verify_complete_assets(&asset_root, &expected, &receipt)?;
                    }
                    Ok(_) => return Err(SetupMaterializeError::ExistingAssetsConflict),
                    Err(error) if error.kind() == io::ErrorKind::NotFound && created => {
                        write_expected_assets(&asset_root, &expected)?;
                        atomic_write_new(&receipt_path, &receipt)?;
                        fs::sync_directory(&asset_root)
                            .map_err(SetupMaterializeError::Filesystem)?;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Err(SetupMaterializeError::IncompleteAssets);
                    }
                    Err(error) => return Err(SetupMaterializeError::Filesystem(error)),
                }

                let dockerfile_path = asset_root.join("Dockerfile");
                let files = expected
                    .iter()
                    .filter(|asset| asset.relative != "Dockerfile")
                    .map(|asset| MaterializedAsset {
                        path: asset_root.join(&asset.relative),
                        content_sha256: sha256_bytes(&asset.content).to_hex(),
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
    content: Vec<u8>,
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
        content: dockerfile.as_bytes().to_vec(),
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
            content: file.content.as_bytes().to_vec(),
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

fn receipt_bytes(
    content_hash: &str,
    expected: &[ExpectedAsset],
) -> Result<Vec<u8>, SetupMaterializeError> {
    let mut receipt = format!("{RECEIPT_MAGIC}\ncontent-sha256={content_hash}\n").into_bytes();
    for asset in expected {
        receipt.extend_from_slice(asset.relative.as_bytes());
        receipt.push(b'\t');
        receipt.extend_from_slice(sha256_bytes(&asset.content).to_hex().as_bytes());
        receipt.push(b'\n');
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
        let parent = path
            .parent()
            .ok_or(SetupMaterializeError::InvalidAssetPath)?;
        ensure_asset_parent(asset_root, parent)?;
        atomic_write_new(&path, &asset.content)?;
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
    for asset in expected {
        allowed_files.insert(asset.relative.clone());
        let mut parent = Path::new(&asset.relative).parent();
        while let Some(directory) = parent {
            if directory.as_os_str().is_empty() {
                break;
            }
            allowed_directories.insert(directory.to_string_lossy().replace('\\', "/"));
            parent = directory.parent();
        }
        let path = asset_path(asset_root, &asset.relative)?;
        let bytes = fs::read_private_regular_file_bounded(&path, asset.content.len())
            .map_err(|_| SetupMaterializeError::ExistingAssetsConflict)?;
        if bytes != asset.content {
            return Err(SetupMaterializeError::ExistingAssetsConflict);
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
        if entry.is_symbolic_link()
            || (!entry.is_file() && !entry.is_directory())
            || (entry.is_file() && !allowed_files.contains(&relative))
            || (entry.is_directory() && !allowed_directories.contains(&relative))
        {
            return Err(SetupMaterializeError::ExistingAssetsConflict);
        }
    }
    Ok(())
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
