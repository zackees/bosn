#![cfg(feature = "native-test-helper")]

use std::{
    env,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bosn_engine::{CommandError, DockerEngine, EngineEvent, RunOptions};
use kernal_api::async_engine::{CancellationSource, RuntimeBuilder};
use kernal_api::platform::process::{ProcessIdentityCapture, capture_identity};

fn runtime() -> kernal_api::async_engine::Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn helper(args: &[&str]) -> DockerEngine {
    DockerEngine::synthetic_for_test(
        env!("CARGO_BIN_EXE_bosn-engine-native-helper"),
        args.iter().copied(),
    )
}
fn events() -> (
    kernal_api::async_engine::Sender<EngineEvent>,
    kernal_api::async_engine::Receiver<EngineEvent>,
) {
    kernal_api::async_engine::channel(8)
}

// The integration-test executable is a portable synthetic child: arguments
// after `--exact-helper` choose its behavior. No Docker resource is touched.
#[test]
fn capture_keeps_streams_separate_and_exit_130_is_ordinary() {
    let engine = helper(&["--exact-helper", "separate-130"]);
    let result = engine
        .capture(RunOptions::bounded(Duration::from_secs(2), 1024))
        .expect("ordinary result");
    assert_eq!(result.exit_code, 130);
    assert_eq!(result.stdout, b"out\n");
    assert_eq!(result.stderr, b"err\n");
}

#[test]
fn spawn_and_deadline_are_distinct() {
    let missing = DockerEngine::synthetic_for_test(
        "bosn-engine-definitely-missing",
        std::iter::empty::<&str>(),
    );
    assert!(matches!(
        missing.capture(RunOptions::bounded(Duration::from_millis(50), 128)),
        Err(CommandError::Spawn(_))
    ));
    let engine = helper(&["--exact-helper", "sleep"]);
    assert!(matches!(
        engine.capture(RunOptions::bounded(Duration::from_millis(30), 128)),
        Err(CommandError::Deadline { .. })
    ));
}

#[test]
fn bounded_capture_rejects_oversized_unterminated_output() {
    let engine = helper(&["--exact-helper", "large"]);
    assert!(matches!(
        engine.capture(RunOptions::bounded(Duration::from_secs(2), 16)),
        Err(CommandError::OutputLimit { limit: 16, .. })
    ));
}

#[test]
fn stream_publishes_before_exit_and_cancellation_reaps_the_client() {
    let engine = helper(&["--exact-helper", "stream"]);
    let cancel = CancellationSource::new();
    let token = cancel.token();
    let (sender, mut receiver) = events();
    let result = runtime().run(async {
        let reader = kernal_api::async_engine::launch(async move {
            let event = receiver.recv().await;
            cancel.cancel();
            event
        });
        let result = engine
            .stream(
                RunOptions::streaming(Duration::from_secs(2), 1024),
                Some(&token),
                &sender,
            )
            .await;
        drop(sender);
        let event = kernal_api::async_engine::timeout(Duration::from_secs(1), reader)
            .await
            .expect("reader deadline")
            .expect("reader task");
        (result, event)
    });
    let (result, seen) = result;
    let pid = match result {
        Err(CommandError::Cancelled {
            reaped_pid: Some(pid),
            cleanup: None,
        }) => pid,
        other => panic!("expected a cleanly reaped cancellation, got {other:?}"),
    };
    assert!(matches!(
        capture_identity(pid),
        ProcessIdentityCapture::Exited
    ));
    assert!(matches!(seen, Some(EngineEvent::Stdout(bytes)) if bytes == b"first\n"));
}

#[test]
fn pre_cancel_does_not_attempt_to_spawn() {
    let engine = DockerEngine::synthetic_for_test(
        "bosn-engine-definitely-missing",
        std::iter::empty::<&str>(),
    );
    let cancel = CancellationSource::new();
    cancel.cancel();
    let (sender, _receiver) = events();
    let result = runtime().run(async {
        engine
            .stream(
                RunOptions::streaming(Duration::from_secs(1), 128),
                Some(&cancel.token()),
                &sender,
            )
            .await
    });
    assert!(matches!(
        result,
        Err(CommandError::Cancelled {
            reaped_pid: None,
            cleanup: None
        })
    ));
}

#[test]
fn async_capture_leaves_the_kernel_runtime_responsive() {
    let engine = helper(&["--exact-helper", "short-sleep"]);
    let started = Instant::now();
    let timer_at = Arc::new(AtomicU64::new(u64::MAX));
    let observed = Arc::clone(&timer_at);
    let (result, ()) = runtime().run(async {
        kernal_api::async_engine::join(
            engine.capture_async(RunOptions::bounded(Duration::from_secs(2), 128)),
            async move {
                kernal_api::async_engine::sleep(Duration::from_millis(20)).await;
                observed.store(started.elapsed().as_millis() as u64, Ordering::Release);
            },
        )
        .await
    });
    assert!(result.is_ok());
    assert!(
        timer_at.load(Ordering::Acquire) < 150,
        "timer ran only after blocking capture"
    );
}

#[test]
fn stream_limit_reaps_the_child() {
    let engine = helper(&["--exact-helper", "large-sleep"]);
    let (sender, _receiver) = events();
    let result = runtime().run(async {
        engine
            .stream(
                RunOptions::streaming(Duration::from_secs(2), 16),
                None,
                &sender,
            )
            .await
    });
    let pid = match result {
        Err(CommandError::OutputLimit {
            limit: 16,
            reaped_pid: Some(pid),
            cleanup: None,
        }) => pid,
        other => panic!("expected a cleanly reaped output limit, got {other:?}"),
    };
    assert!(matches!(
        capture_identity(pid),
        ProcessIdentityCapture::Exited
    ));
}

#[test]
fn slow_output_consumer_fails_without_blocking_and_reaps_the_client() {
    let engine = helper(&["--exact-helper", "huge-sleep"]);
    let (sender, _receiver) = kernal_api::async_engine::channel(1);
    let started = Instant::now();
    let timer = Arc::new(AtomicU64::new(u64::MAX));
    let observed = Arc::clone(&timer);
    let (result, ()) = runtime().run(async {
        kernal_api::async_engine::join(
            engine.stream(
                RunOptions::streaming(Duration::from_secs(2), 32 * 1024),
                None,
                &sender,
            ),
            async move {
                kernal_api::async_engine::sleep(Duration::from_millis(20)).await;
                observed.store(started.elapsed().as_millis() as u64, Ordering::Release);
            },
        )
        .await
    });
    let pid = match result {
        Err(CommandError::OutputConsumerSlow {
            reaped_pid: Some(pid),
            cleanup: None,
        }) => pid,
        other => panic!("expected a reaped slow-consumer failure, got {other:?}"),
    };
    assert!(
        timer.load(Ordering::Acquire) < 150,
        "stream blocked the runtime on its consumer"
    );
    assert!(matches!(
        capture_identity(pid),
        ProcessIdentityCapture::Exited
    ));
}

#[test]
fn closed_output_consumer_reaps_the_client() {
    let engine = helper(&["--exact-helper", "stream"]);
    let (sender, receiver) = events();
    drop(receiver);
    let result = runtime().run(async {
        engine
            .stream(
                RunOptions::streaming(Duration::from_secs(2), 1024),
                None,
                &sender,
            )
            .await
    });
    let pid = match result {
        Err(CommandError::OutputConsumerClosed {
            reaped_pid: Some(pid),
            cleanup: None,
        }) => pid,
        other => panic!("expected a reaped closed-consumer failure, got {other:?}"),
    };
    assert!(matches!(
        capture_identity(pid),
        ProcessIdentityCapture::Exited
    ));
}

#[test]
fn stream_preserves_an_ordinary_nonzero_130() {
    let engine = helper(&["--exact-helper", "separate-130"]);
    let (sender, _receiver) = events();
    let result = runtime().run(async {
        engine
            .stream(
                RunOptions::streaming(Duration::from_secs(2), 1024),
                None,
                &sender,
            )
            .await
    });
    assert_eq!(result.expect("ordinary result").exit_code, 130);
}

#[cfg(unix)]
#[test]
fn closed_pipes_do_not_bypass_the_deadline_or_spin() {
    let engine = helper(&["--exact-helper", "closed-pipes-sleep"]);
    let (sender, _receiver) = events();
    let started = Instant::now();
    let yielded = Arc::new(AtomicU64::new(u64::MAX));
    let observed = Arc::clone(&yielded);
    let (result, ()) = runtime().run(async {
        kernal_api::async_engine::join(
            engine.stream(
                RunOptions::streaming(Duration::from_millis(80), 128),
                None,
                &sender,
            ),
            async move {
                kernal_api::async_engine::sleep(Duration::from_millis(20)).await;
                observed.store(started.elapsed().as_millis() as u64, Ordering::Release);
            },
        )
        .await
    });
    assert!(matches!(
        result,
        Err(CommandError::Deadline { cleanup: None, .. })
    ));
    assert!(
        yielded.load(Ordering::Acquire) < 60,
        "closed pipes made stream spin"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn current_directory_with_spaces_and_environment_reach_the_docker_transport() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("monotonic wall clock")
        .as_nanos();
    let path = env::temp_dir().join(format!("bosn engine space {} {nonce}", std::process::id()));
    std::fs::create_dir_all(&path).expect("temporary context directory");
    let result = helper(&["--exact-helper", "context"])
        .current_dir(&path)
        .env("BOSN_ENGINE_TEST_ENV", "present")
        .capture(RunOptions::bounded(Duration::from_secs(2), 1024))
        .expect("context result");
    std::fs::remove_dir(&path).expect("remove empty temporary context directory");
    let output = String::from_utf8(result.stdout).expect("UTF-8 helper output");
    assert!(output.contains(path.to_string_lossy().as_ref()));
    assert!(output.ends_with("present\n"));
}

#[test]
#[ignore = "requires a running Docker daemon; run explicitly for the read-only smoke"]
fn rust_docker_diagnostics_smoke_is_read_only_when_docker_is_available() {
    let result =
        DockerEngine::docker().diagnostics(RunOptions::bounded(Duration::from_secs(5), 16 * 1024));
    match result {
        Ok(diagnostics) => {
            assert!(diagnostics.client.ok());
            assert!(diagnostics.server.ok());
            assert!(diagnostics.info.ok());
        }
        Err(error) => panic!("Docker diagnostics failed: {error}"),
    }
}
