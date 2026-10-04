//! Bounded startup readiness, including verified act bootstrap before dockerd.

use super::{CONTROL_DEADLINE, DockerActBackend, PULL_DEADLINE, async_engine, owned};
use std::time::Duration;

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StartupState {
    running: bool,
    exit_code: i64,
    error: String,
}

fn require_running(document: &[u8]) -> Result<(), String> {
    let state: StartupState = serde_json::from_slice(document)
        .map_err(|error| format!("engine startup state: {error}"))?;
    if !state.running {
        return Err(format!(
            "engine exited before readiness: code {}, error {:?}",
            state.exit_code, state.error
        ));
    }
    Ok(())
}

impl DockerActBackend {
    pub(super) async fn wait_ready(&self, engine: &str) -> Result<(), String> {
        // Previously installation had its own pull budget after readiness.
        // Bootstrap now does it before dockerd, retaining that total allowance.
        let budget = PULL_DEADLINE + Duration::from_secs(60);
        let deadline = async_engine::Deadline::after(budget);
        loop {
            if deadline.is_elapsed() {
                return Err(format!(
                    "nested engine did not become ready within {}s",
                    budget.as_secs()
                ));
            }
            let probe = Self::exec(engine, "docker info >/dev/null 2>&1");
            if self
                .run(probe, deadline.remaining().min(CONTROL_DEADLINE))
                .await?
                .ok()
            {
                return Ok(());
            }
            let state = self
                .run(
                    owned(&[
                        "container",
                        "inspect",
                        "--format",
                        "{{json .State}}",
                        engine,
                    ]),
                    deadline.remaining().min(CONTROL_DEADLINE),
                )
                .await?;
            if !state.ok() {
                return Err(format!(
                    "engine startup inspection failed: {}",
                    String::from_utf8_lossy(&state.stderr).trim()
                ));
            }
            require_running(&state.stdout)?;
            async_engine::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exited_or_unprovable_engine_does_not_wait_for_download_budget() {
        assert!(
            require_running(br#"{"Running":true,"ExitCode":0,"Error":"","Status":"running"}"#)
                .is_ok()
        );
        for document in [
            &br#"{"Running":false,"ExitCode":1,"Error":"bootstrap failed"}"#[..],
            &br#"{"Running":false,"ExitCode":0,"Error":""}"#[..],
            &br#"{"Running":true}"#[..],
            &br#"{"Running":"true","ExitCode":0,"Error":""}"#[..],
            &br#"null"#[..],
        ] {
            assert!(require_running(document).is_err());
        }
    }
}
