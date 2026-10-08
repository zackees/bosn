//! Owned interactive transport for bounded control protocols.
use super::*;

impl DockerEngine {
    /// Spawn this exact configured transport with piped stdin. The session owns
    /// the client and kills it on drop; remote effects still require recovery.
    /// Callers must bound their protocol's duration and cumulative output.
    pub async fn spawn_interactive(
        &self,
        deadline: Duration,
    ) -> Result<kernal_api::ProcessSession, CommandError> {
        async_engine::timeout(
            deadline,
            self.spec()
                .stdin(StreamMode::Piped)
                .spawn_session(ProcessSessionOptions {
                    max_queued_chunks: 8,
                    max_chunk_bytes: 16 * 1024,
                    post_exit_drain: ProcessPostExitDrain::AbandonAfter(Duration::from_millis(250)),
                    kill_on_drop: true,
                }),
        )
        .await
        .map_err(|_| CommandError::Deadline {
            reaped_pid: None,
            cleanup: None,
        })?
        .map_err(CommandError::Spawn)
    }
}
