use super::*;
use std::io::Write;
fn input() -> (std::fs::File, u64, PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "bosn-stdin-3345-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    for _ in 0..32 {
        file.write_all(&[b'x'; 65536]).unwrap();
    }
    drop(file);
    (std::fs::File::open(&path).unwrap(), 2 << 20, path)
}
async fn blocked_session(engine: &DockerEngine) -> kernal_api::ProcessSession {
    engine
        .spec()
        .stdin(StreamMode::Piped)
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 8,
            max_chunk_bytes: 64 * 1024,
            post_exit_drain: ProcessPostExitDrain::AbandonAfter(Duration::from_millis(250)),
            kill_on_drop: true,
        })
        .await
        .unwrap()
}

#[test]
fn stdin_file_streams_large_input_and_preserves_transport_environment() {
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let (file, size, path) = input();
            let engine = DockerEngine::from_parts(
                "sh",
                [
                    "-c",
                    r#"printf '%s\n' "$BOSN_ENDPOINT"; sha256sum; printf diagnostic >&2"#,
                ],
            )
            .env("BOSN_ENDPOINT", "owned-endpoint");
            let result = engine
                .capture_with_stdin_file_async(
                    file,
                    size,
                    RunOptions::bounded(Duration::from_secs(5), 1024),
                    None,
                )
                .await
                .unwrap();
            assert!(result.ok());
            assert_eq!(
                result.stdout,
                format!(
                    "owned-endpoint\n{}  -\n",
                    "6932fd31e5daf4739b9fa78ff777b2831b0995cc1d0b0093cac80601902013bc"
                )
                .as_bytes()
            );
            assert_eq!(result.stderr, b"diagnostic");
            eprintln!("retained stdin fixture {}", path.display());
        });
}
#[test]
fn stdin_file_refuses_oversize_and_reaps_blocked_child_on_deadline_or_cancel() {
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let (file, size, path) = input();
            let engine = DockerEngine::from_parts("sh", ["-c", "exec sleep 30"]);
            assert!(matches!(
                engine
                    .capture_with_stdin_file_async(
                        file,
                        size - 1,
                        RunOptions::bounded(Duration::from_secs(1), 1024),
                        None
                    )
                    .await,
                Err(CommandError::Io(_))
            ));
            let session = blocked_session(&engine).await;
            let result = stdin_file::capture_session(
                std::fs::File::open(&path).unwrap(),
                size,
                RunOptions::bounded(Duration::from_millis(100), 1024),
                None,
                Instant::now(),
                session,
            )
            .await;
            let Err(CommandError::Deadline {
                reaped_pid: Some(pid),
                ..
            }) = result
            else {
                panic!("{result:?}")
            };
            assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
            let session = blocked_session(&engine).await;
            let source = async_engine::CancellationSource::new();
            let trigger = source.clone();
            let task = async_engine::launch(async move {
                async_engine::sleep(Duration::from_millis(50)).await;
                trigger.cancel();
            });
            let result = stdin_file::capture_session(
                std::fs::File::open(&path).unwrap(),
                size,
                RunOptions::bounded(Duration::from_secs(5), 1024),
                Some(&source.token()),
                Instant::now(),
                session,
            )
            .await;
            task.await.unwrap();
            let Err(CommandError::Cancelled {
                reaped_pid: Some(pid),
                ..
            }) = result
            else {
                panic!("{result:?}")
            };
            assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
            eprintln!("retained stdin fixture {}", path.display());
        });
}
#[test]
fn stdin_file_closed_child_input_and_nonregular_file_are_refused() {
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let (file, size, path) = input();
            let engine = DockerEngine::from_parts("sh", ["-c", "exec 0<&-; sleep 0.05; exit 0"]);
            assert!(
                engine
                    .capture_with_stdin_file_async(
                        file,
                        size,
                        RunOptions::bounded(Duration::from_secs(5), 1024),
                        None
                    )
                    .await
                    .is_err()
            );
            assert!(matches!(
                engine
                    .capture_with_stdin_file_async(
                        std::fs::File::open("/tmp").unwrap(),
                        size,
                        RunOptions::bounded(Duration::from_secs(1), 1024),
                        None
                    )
                    .await,
                Err(CommandError::Io(_))
            ));
            eprintln!("retained stdin fixture {}", path.display());
        });
}
#[test]
fn stdin_file_drains_both_streams_and_refuses_output_overflow() {
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let (file, size, path) = input();
            let engine = DockerEngine::from_parts("sh", ["-c", "printf '123456789' >&2; exec cat"]);
            assert!(matches!(
                engine
                    .capture_with_stdin_file_async(
                        file,
                        size,
                        RunOptions::bounded(Duration::from_secs(5), 1024),
                        None
                    )
                    .await,
                Err(CommandError::OutputLimit { .. })
            ));
            eprintln!("retained stdin fixture {}", path.display());
        });
}
