//! The one pre-flight every daemon-backed command runs before it sends work (#324).
//!
//! A daemon from another release can misread a request and answer with a reset
//! connection or a refusal, which the command would report as a bare "request
//! failed". Ask for the version first -- the one request every release answers --
//! and refuse with both versions and the remedy instead.

use super::*;

/// The mismatch message when the daemon for `state_dir` speaks another wire
/// protocol (or, predating the handshake, is another release); `None` when it
/// is compatible or does not answer (the command then fails, or starts a
/// daemon, in its own way). A compatible daemon from another release gets a
/// one-line note on stderr (#509).
pub(crate) fn daemon_mismatch(
    runtime: &kernal_api::async_engine::Runtime,
    client: &Client,
    state_dir: &Path,
) -> Option<String> {
    let identity = runtime.run(client.daemon_identity()).ok()?;
    let client_version = env!("CARGO_PKG_VERSION");
    if let Some(note) = bosn_service::daemon_release_skew(client_version, &identity) {
        eprintln!("bosn: {note}");
    }
    bosn_service::daemon_version_mismatch(state_dir, client_version, &identity)
}

/// Exit with the mismatch, as JSON on stdout or text on stderr, before
/// `command` sends anything to a daemon from another release.
/// `command` is the CLI spelling (`setup ensure`); its JSON `action` is the
/// snake-case form every command's JSON already uses (`setup_ensure`).
pub(crate) fn require_matching_daemon(
    state_dir: impl AsRef<Path>,
    command: &str,
    json_output: bool,
) {
    let state_dir = state_dir.as_ref();
    let Ok(client) = Client::for_state(state_dir) else {
        return;
    };
    let Ok(runtime) = RuntimeBuilder::current_thread().enable_all().build() else {
        return;
    };
    let Some(mismatch) = daemon_mismatch(&runtime, &client, state_dir) else {
        return;
    };
    if json_output {
        println!(
            "{}",
            json!({"action": command.replace([' ', '-'], "_"), "error": "version_mismatch", "message": mismatch})
        );
    } else {
        eprintln!("bosn {command}: {mismatch}");
    }
    std::process::exit(1);
}
