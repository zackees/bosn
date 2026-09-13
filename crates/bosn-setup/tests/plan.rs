use std::{
    future::{Ready, ready},
    path::Path,
};

use bosn_setup::{
    RemoteSetupResponse, RemoteTransportError, SetupAcquirePolicy, SetupPlanAppSource,
    SetupPlanRequest, SetupRemoteTransport, SetupSourceKind, plan_setup_with_transport,
};
use kernal_api::{async_engine::RuntimeBuilder, hash::sha256_bytes};

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct LocalOnlyTransport;

impl SetupRemoteTransport for LocalOnlyTransport {
    type FetchFuture<'a> = Ready<Result<RemoteSetupResponse, RemoteTransportError>>;

    fn fetch<'a>(&'a self, _locator: &'a str, _max_bytes: usize) -> Self::FetchFuture<'a> {
        // A local setup plan must not reach a network implementation.
        ready(Err(RemoteTransportError::Unavailable))
    }
}

fn run<T>(future: impl Future<Output = T>) -> T {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(future)
}

fn request(
    state: &Path,
    workspace: &Path,
    config: &Path,
    policy: SetupAcquirePolicy,
) -> SetupPlanRequest {
    SetupPlanRequest {
        state_dir: state.to_path_buf(),
        workspace: workspace.to_path_buf(),
        locator: config.to_string_lossy().into_owned(),
        policy,
    }
}

fn pinned_document() -> String {
    format!(
        "version = 1\n[app]\nimage = 'registry.example/demo@sha256:{DIGEST}'\n[task.check]\ncommand = 'echo check'\n[task.lint]\ncommand = 'echo lint'\n"
    )
}

fn inline_document() -> String {
    "version = 1\n[app]\ndockerfile = 'FROM scratch'\n[task.check]\ncommand = 'echo check'\n[[file]]\npath = 'scripts/check.sh'\ncontent = \"#!/bin/sh\\necho check\\n\"\n".into()
}

#[test]
fn pinned_local_plan_is_structured_and_offline_is_verified_cache_reuse() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = state.path().join("pinned.toml");
    let document = pinned_document();
    std::fs::write(&config, &document).unwrap();

    let first = run(plan_setup_with_transport(
        request(
            state.path(),
            workspace.path(),
            &config,
            SetupAcquirePolicy::OnlineRefresh,
        ),
        &LocalOnlyTransport,
    ))
    .unwrap();
    assert_eq!(first.source_kind, SetupSourceKind::LocalFile);
    assert_eq!(
        first.content_sha256,
        sha256_bytes(document.as_bytes()).to_hex()
    );
    assert_eq!(first.schema_version, 1);
    assert_eq!(
        first.workspace_root,
        std::fs::canonicalize(workspace.path()).unwrap()
    );
    assert_eq!(first.asset_root, None);
    assert_eq!(first.task_names, ["check", "lint"]);
    assert_eq!(
        first.tasks.get("check").map(|task| task.command.as_str()),
        Some("echo check")
    );
    assert_eq!(first.app.environment.len(), 0);
    assert!(first.app.mounts.is_empty());
    assert_eq!(
        first.app_source,
        SetupPlanAppSource::PinnedImage {
            image: format!("registry.example/demo@sha256:{DIGEST}"),
        }
    );

    std::fs::remove_file(&config).unwrap();
    let offline = run(plan_setup_with_transport(
        request(
            state.path(),
            workspace.path(),
            &config,
            SetupAcquirePolicy::OfflineCacheOnly,
        ),
        &LocalOnlyTransport,
    ))
    .unwrap();
    assert_eq!(offline, first);
}

#[test]
fn inline_local_plan_materializes_only_private_assets_and_reuses_them_offline() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = state.path().join("inline.toml");
    std::fs::write(&config, inline_document()).unwrap();

    let first = run(plan_setup_with_transport(
        request(
            state.path(),
            workspace.path(),
            &config,
            SetupAcquirePolicy::OnlineRefresh,
        ),
        &LocalOnlyTransport,
    ))
    .unwrap();
    let asset_root = first.asset_root.clone().expect("inline assets");
    assert!(asset_root.starts_with(state.path()));
    assert!(!asset_root.starts_with(workspace.path()));
    assert_eq!(
        std::fs::read_to_string(asset_root.join("Dockerfile")).unwrap(),
        "FROM scratch"
    );
    assert_eq!(
        std::fs::read_to_string(asset_root.join("scripts/check.sh")).unwrap(),
        "#!/bin/sh\necho check\n"
    );
    assert!(matches!(
        first.app_source,
        SetupPlanAppSource::InlineDockerfile { .. }
    ));
    assert!(
        std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none()
    );

    std::fs::remove_file(&config).unwrap();
    let offline = run(plan_setup_with_transport(
        request(
            state.path(),
            workspace.path(),
            &config,
            SetupAcquirePolicy::OfflineCacheOnly,
        ),
        &LocalOnlyTransport,
    ))
    .unwrap();
    assert_eq!(offline, first);
}
