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
                    executable: false,
                },
                ManifestBuildEntry::File {
                    path: "payload".into(),
                    content: b"ok\n".to_vec(),
                    executable: false,
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
fn manifest_context_keeps_a_files_execute_bit_and_stays_private() {
    let (_temp, store, _workspace) = store_and_workspace();
    let entries = [
        ManifestBuildEntry::File {
            path: "Dockerfile".into(),
            content: b"FROM scratch\nCOPY tool /tool\n".to_vec(),
            executable: false,
        },
        ManifestBuildEntry::File {
            path: "tool".into(),
            content: b"#!/bin/sh\n".to_vec(),
            executable: true,
        },
    ];
    let root = store
        .materialize_manifest_context(DIGEST, "Dockerfile", &entries)
        .unwrap();
    let executable = |name: &str| {
        fs::context_path_metadata_no_follow(&root.join(name))
            .unwrap()
            .executable
    };
    // Windows has no per-file execute bit.
    assert_eq!(
        executable("tool"),
        !kernal_api::platform::host::target_is_windows()
    );
    assert!(!executable("Dockerfile"));
    // The private-asset checks still accept it, on first use and on reuse.
    assert!(verify_materialized_assets(DIGEST, &root).is_ok());
    assert_eq!(
        store
            .materialize_manifest_context(DIGEST, "Dockerfile", &entries)
            .unwrap(),
        root
    );
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
                    executable: false,
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
            executable: false,
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
