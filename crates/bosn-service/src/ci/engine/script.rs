//! A fixed bosn script run in the engine with bytes on stdin (#650).

use bosn_engine::{DockerEngine, RunOptions};

use super::{CONTROL_DEADLINE, CONTROL_OUTPUT};

/// Run `args` (a `docker exec -i ... sh -ec <script>`) with `stdin`, which
/// travels through an unlinked private file, never argv or the environment.
pub(super) async fn run(
    docker: &DockerEngine,
    args: Vec<String>,
    stdin: &[u8],
) -> Result<String, String> {
    let file = private_input(stdin).map_err(|e| format!("engine script input: {e}"))?;
    let result = docker
        .with_args(args)
        .capture_with_stdin_file_async(
            file,
            stdin.len() as u64,
            RunOptions::bounded(CONTROL_DEADLINE, CONTROL_OUTPUT),
            None,
        )
        .await
        .map_err(|e| format!("engine script: {e}"))?;
    if !result.ok() {
        return Err(format!(
            "engine script failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

/// `bytes` in a regular file that no other process can open: created
/// exclusively with mode 0600 and unlinked before it is read.
fn private_input(bytes: &[u8]) -> std::io::Result<std::fs::File> {
    use std::io::{Seek, Write};
    let name = format!(
        "bosn-engine-input-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let path = std::env::temp_dir().join(name);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    let _ = std::fs::remove_file(&path);
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(file)
}
