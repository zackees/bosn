#![cfg(feature = "native-test-helper")]

use std::{fs, path::Path, time::Duration};

use bosn_engine::{DockerEngine, EngineEvent, RunOptions};
use bosn_generation::{
    ExternalImageIdentity,
    resolver::{
        ResolutionError, ResolutionPolicy, expand_automatic_platforms_streaming, resolve_images,
        resolve_images_streaming, resolve_required_images_streaming,
    },
};
use kernal_api::async_engine::{CancellationSource, RuntimeBuilder, channel};

fn required(reference: &str, platform: Option<&str>) -> ExternalImageIdentity {
    ExternalImageIdentity {
        reference: reference.into(),
        platform: platform.map(Into::into),
        identity: None,
    }
}
fn engine(scenario: &str) -> (DockerEngine, tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("temporary resolver fixture directory");
    let log = directory.path().join("commands.log");
    let engine = DockerEngine::synthetic_for_test(
        env!("CARGO_BIN_EXE_bosn-generation-resolver-helper"),
        ["--resolver-helper"],
    )
    .env("BOSN_RESOLVER_SCENARIO", scenario)
    .env("BOSN_RESOLVER_LOG", &log);
    (engine, directory, log)
}
fn options() -> RunOptions {
    RunOptions::bounded(Duration::from_secs(2), 4096)
}
fn calls(log: &Path) -> String {
    fs::read_to_string(log).unwrap_or_default()
}
fn runtime() -> kernal_api::async_engine::Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("resolver test runtime")
}

#[test]
fn present_receipt_is_immutable_and_does_not_pull() {
    let (engine, _directory, log) = engine("present");
    let got = resolve_images(
        &engine,
        &[required("alpine", None)],
        ResolutionPolicy::PullIfMissing,
        options(),
    )
    .unwrap();
    assert_eq!(got[0].identity, Some(format!("sha256:{}", "a".repeat(64))));
    assert!(!calls(&log).contains("pull"));
}

#[test]
fn malformed_initial_and_post_pull_ids_are_rejected() {
    let (first_engine, _first_directory, _) = engine("malformed");
    assert!(matches!(
        resolve_images(
            &first_engine,
            &[required("alpine", None)],
            ResolutionPolicy::PullIfMissing,
            options()
        ),
        Err(ResolutionError::Malformed { .. })
    ));
    let (second_engine, _second_directory, log) = engine("missing-then-malformed");
    assert!(matches!(
        resolve_images(
            &second_engine,
            &[required("alpine", None)],
            ResolutionPolicy::PullIfMissing,
            options()
        ),
        Err(ResolutionError::Malformed { .. })
    ));
    assert!(calls(&log).contains("pull"));
}

#[test]
fn missing_pull_then_valid_retains_platform_and_duplicates_inspect_once() {
    let (engine, _directory, log) = engine("missing-then-present");
    let required = required("alpine", Some("linux/arm64"));
    let got = resolve_images(
        &engine,
        &[required.clone(), required],
        ResolutionPolicy::PullIfMissing,
        options(),
    )
    .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].platform, Some("linux/arm64".into()));
    let log = calls(&log);
    assert_eq!(log.matches("\u{1f}image\u{1f}inspect\u{1f}").count(), 2);
    assert_eq!(log.matches("\u{1f}pull\u{1f}").count(), 1);
    assert!(
        log.lines()
            .all(|line| line.contains("--platform\u{1f}linux/arm64"))
    );
}

#[test]
fn read_only_missing_does_not_pull() {
    let (engine, _directory, log) = engine("missing");
    let got = resolve_images(
        &engine,
        &[required("alpine", None)],
        ResolutionPolicy::ReadOnly,
        options(),
    )
    .unwrap();
    assert_eq!(got, [required("alpine", None)]);
    assert!(!calls(&log).contains("pull"));
}

#[test]
fn daemon_unsupported_and_near_missing_inspects_never_pull() {
    for scenario in ["permission", "unsupported", "near-missing"] {
        let (engine, _directory, log) = engine(scenario);
        assert!(matches!(
            resolve_images(
                &engine,
                &[required("alpine", None)],
                ResolutionPolicy::PullIfMissing,
                options()
            ),
            Err(ResolutionError::InspectFailed { .. })
        ));
        assert!(!calls(&log).contains("pull"));
    }
}

#[test]
fn post_pull_permission_is_not_missing() {
    let (engine, _directory, log) = engine("missing-then-permission");
    assert!(matches!(
        resolve_images(
            &engine,
            &[required("alpine", None)],
            ResolutionPolicy::PullIfMissing,
            options()
        ),
        Err(ResolutionError::InspectFailed { .. })
    ));
    assert_eq!(calls(&log).matches("\u{1f}pull\u{1f}").count(), 1);
}

#[test]
fn invalid_reference_or_platform_launches_nothing() {
    for input in [
        required("", None),
        required("-alpine", None),
        required("al\0pine", None),
        required("alpine", Some("")),
        required("alpine", Some("-linux/arm64")),
        required("alpine", Some("linux\0arm64")),
    ] {
        let (engine, _directory, log) = engine("present");
        assert!(matches!(
            resolve_images(
                &engine,
                &[input],
                ResolutionPolicy::PullIfMissing,
                options()
            ),
            Err(ResolutionError::InvalidInput { .. })
        ));
        assert!(!log.exists());
    }
}

#[test]
fn invalid_later_entry_prevents_every_command_in_the_batch() {
    let (engine, _directory, log) = engine("present");
    assert!(matches!(
        resolve_images(
            &engine,
            &[required("alpine", None), required("-not-an-image", None)],
            ResolutionPolicy::PullIfMissing,
            options()
        ),
        Err(ResolutionError::InvalidInput { .. })
    ));
    assert!(
        !log.exists(),
        "a later invalid requirement must prevent earlier inspection"
    );
}

#[test]
fn async_pre_cancel_rejects_before_any_child_launch() {
    let (engine, _directory, log) = engine("present");
    let source = CancellationSource::new();
    source.cancel();
    let (sender, _receiver) = channel::<EngineEvent>(8);
    assert!(matches!(
        runtime().run(resolve_images_streaming(
            &engine,
            &[required("alpine", None)],
            ResolutionPolicy::PullIfMissing,
            options(),
            &source.token(),
            &sender,
        )),
        Err(ResolutionError::Transport(
            bosn_engine::CommandError::Cancelled { .. }
        ))
    ));
    assert!(!log.exists(), "pre-cancel must not spawn a child");
}

#[test]
fn async_present_receipt_streams_through_kernel_transport() {
    let (engine, _directory, _log) = engine("present");
    let source = CancellationSource::new();
    let (sender, mut receiver) = channel::<EngineEvent>(8);
    let resolved = runtime().run(async {
        let resolved = resolve_images_streaming(
            &engine,
            &[required("alpine", None)],
            ResolutionPolicy::PullIfMissing,
            options(),
            &source.token(),
            &sender,
        )
        .await?;
        let event = receiver.recv().await.expect("inspect output event");
        Ok::<_, ResolutionError>((resolved, event))
    });
    let (resolved, event) = resolved.expect("async receipt");
    assert_eq!(
        resolved[0].identity,
        Some(format!("sha256:{}", "a".repeat(64)))
    );
    assert!(matches!(event, EngineEvent::Stdout(_)));
}

#[test]
fn async_cancellation_reaps_a_slow_inspect_before_returning() {
    let (engine, _directory, log) = engine("slow-present");
    let source = CancellationSource::new();
    let token = source.token();
    let (sender, _receiver) = channel::<EngineEvent>(8);
    let result = runtime().run(async {
        kernal_api::async_engine::join(
            resolve_images_streaming(
                &engine,
                &[required("alpine", None)],
                ResolutionPolicy::PullIfMissing,
                options(),
                &token,
                &sender,
            ),
            async {
                kernal_api::async_engine::sleep(Duration::from_millis(25)).await;
                source.cancel();
            },
        )
        .await
        .0
    });
    assert!(matches!(
        result,
        Err(ResolutionError::Transport(
            bosn_engine::CommandError::Cancelled {
                reaped_pid: Some(_),
                ..
            }
        ))
    ));
    assert!(calls(&log).contains("image\u{1f}inspect"));
}

#[test]
fn automatic_platforms_expand_once_from_the_daemon() {
    let (engine, _directory, log) = engine("automatic-platform");
    let source = CancellationSource::new();
    let (sender, _receiver) = channel::<EngineEvent>(8);
    let expanded = runtime()
        .run(expand_automatic_platforms_streaming(
            &engine,
            &[required(
                "registry/$BUILDOS/tool:$TARGETARCH",
                Some("${BUILDPLATFORM}"),
            )],
            options(),
            &source.token(),
            &sender,
        ))
        .expect("automatic expansion");
    assert_eq!(expanded[0].reference, "registry/linux/tool:amd64");
    assert_eq!(expanded[0].platform.as_deref(), Some("linux/amd64"));
    assert_eq!(calls(&log).matches("version").count(), 1);
}

#[test]
fn resolved_receipts_use_expanded_platform_keys() {
    let (engine, _directory, _log) = engine("automatic-platform-present");
    let source = CancellationSource::new();
    let (sender, _receiver) = channel::<EngineEvent>(16);
    let got = runtime()
        .run(resolve_required_images_streaming(
            &engine,
            &[required("alpine", Some("$TARGETPLATFORM"))],
            ResolutionPolicy::ReadOnly,
            options(),
            &source.token(),
            &sender,
        ))
        .expect("expanded receipt");
    assert_eq!(got[0].platform.as_deref(), Some("linux/amd64"));
    assert!(got[0].identity.is_some());
}
