//! Streaming a bounded regular file into one engine command's stdin.

use super::*;

impl DockerEngine {
    /// Stream a regular file into this exact transport's piped stdin without
    /// retaining the input in memory. This is transport only, not ownership
    /// authorization. Killing the client does not prove a remote exec stopped.
    #[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
    pub async fn capture_with_stdin_file_async(
        &self,
        file: std::fs::File,
        input_limit: u64,
        options: RunOptions,
        cancellation: Option<&CancellationToken>,
    ) -> Result<CommandResult, CommandError> {
        use std::io::Read;
        let metadata = file.metadata().map_err(CommandError::Io)?;
        if !metadata.is_file() || metadata.len() > input_limit {
            return Err(CommandError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdin requires a bounded regular file",
            )));
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(CommandError::Cancelled {
                reaped_pid: None,
                cleanup: None,
            });
        }
        let started = Instant::now();
        let session = async_engine::timeout(
            options.deadline,
            self.spec()
                .stdin(StreamMode::Piped)
                .spawn_session(ProcessSessionOptions {
                    max_queued_chunks: 8,
                    max_chunk_bytes: 64 * 1024,
                    post_exit_drain: ProcessPostExitDrain::AbandonAfter(Duration::from_millis(250)),
                    kill_on_drop: true,
                }),
        )
        .await
        .map_err(|_| CommandError::Deadline {
            reaped_pid: None,
            cleanup: None,
        })?
        .map_err(CommandError::Spawn)?;
        let pid = session.id();
        let check = || -> Result<Duration, CommandError> {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(CommandError::Cancelled {
                    reaped_pid: Some(pid),
                    cleanup: None,
                });
            }
            options
                .deadline
                .checked_sub(started.elapsed())
                .ok_or(CommandError::Deadline {
                    reaped_pid: Some(pid),
                    cleanup: None,
                })
        };
        let writer = async {
            let result = async {
                let mut file = file;
                let mut total = 0u64;
                loop {
                    let remaining = check()?;
                    let read = async_engine::launch_blocking(move || {
                        let mut bytes = vec![0; 64 * 1024];
                        let result = file.read(&mut bytes);
                        (file, bytes, result)
                    });
                    let (returned, mut bytes, count) = async_engine::timeout(remaining, read)
                        .await
                        .map_err(|_| CommandError::Deadline {
                            reaped_pid: Some(pid),
                            cleanup: None,
                        })?
                        .map_err(|e| CommandError::Io(io::Error::other(e.to_string())))?;
                    file = returned;
                    let count = count.map_err(CommandError::Io)?;
                    if count == 0 {
                        session.close_stdin().await.map_err(CommandError::Io)?;
                        return Ok(());
                    }
                    total = total
                        .checked_add(count as u64)
                        .ok_or_else(|| CommandError::Io(io::Error::other("stdin size overflow")))?;
                    if total > input_limit {
                        return Err(CommandError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "stdin file grew beyond byte ceiling",
                        )));
                    }
                    bytes.truncate(count);
                    async_engine::timeout(check()?, session.write_stdin(&bytes))
                        .await
                        .map_err(|_| CommandError::Deadline {
                            reaped_pid: Some(pid),
                            cleanup: None,
                        })?
                        .map_err(CommandError::Io)?;
                }
            }
            .await;
            if result.is_err() {
                let _ = session.kill().await;
            }
            result
        };
        let reader = async {
            let result = async {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                loop {
                    let remaining = check()?;
                    match async_engine::timeout(remaining.min(SESSION_POLL), session.next_output())
                        .await
                    {
                        Ok(Some(ProcessOutputEvent::Chunk(chunk))) => {
                            let (destination, bytes) = match chunk {
                                ProcessOutputChunk::Stdout(bytes) => (&mut stdout, bytes),
                                ProcessOutputChunk::Stderr(bytes) => (&mut stderr, bytes),
                            };
                            if destination.len().saturating_add(bytes.len()) > options.output_limit
                            {
                                return Err(CommandError::OutputLimit {
                                    limit: options.output_limit,
                                    reaped_pid: Some(pid),
                                    cleanup: None,
                                });
                            }
                            destination.extend_from_slice(&bytes);
                            if stdout.len().saturating_add(stderr.len()) > options.output_limit {
                                return Err(CommandError::OutputLimit {
                                    limit: options.output_limit,
                                    reaped_pid: Some(pid),
                                    cleanup: None,
                                });
                            }
                        }
                        Ok(Some(ProcessOutputEvent::Completion(completion))) => {
                            if !matches!(
                                completion,
                                ProcessOutputCompletion::StdoutEof
                                    | ProcessOutputCompletion::StderrEof
                            ) {
                                return Err(CommandError::OutputCompletion {
                                    detail: format!("{completion:?}"),
                                    reaped_pid: Some(pid),
                                    cleanup: None,
                                });
                            }
                        }
                        Ok(None) => {
                            if let Some(exit) = session.poll().await.map_err(CommandError::Io)? {
                                return Ok(CommandResult {
                                    exit_code: exit
                                        .exit_code()
                                        .unwrap_or(-exit.signal().unwrap_or(0)),
                                    stdout,
                                    stderr,
                                });
                            }
                            async_engine::sleep(SESSION_POLL).await;
                        }
                        Err(_) => {}
                    }
                }
            }
            .await;
            if result.is_err() {
                let _ = session.kill().await;
            }
            result
        };
        let (written, captured) = async_engine::join(writer, reader).await;
        match (captured, written) {
            (Ok(captured), Ok(())) => Ok(captured),
            (Err(error), _) | (_, Err(error)) => {
                let _ = session.shutdown_output().await;
                reap(session, error).await
            }
        }
    }
}
