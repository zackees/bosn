use std::{
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use bosn_service::Client;
use kernal_api::async_engine::RuntimeBuilder;
use serde_json::{Value, json};

const READY_DEADLINE: Duration = Duration::from_secs(5);

struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn start(state: &Path) -> Self {
        Self {
            child: Command::new(env!("CARGO_BIN_EXE_bosn"))
                // This daemon control increment has no engine work. An
                // unusable endpoint makes accidental Docker contact fail
                // loudly instead of becoming an ambient test dependency.
                .env("DOCKER_HOST", "tcp://127.0.0.1:1")
                .args(["daemon", "serve", "--state-dir"])
                .arg(state)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        }
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + READY_DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "daemon did not exit in time");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn run(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bosn"))
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .args(args)
        .output()
        .unwrap()
}

fn wait_for_client(daemon: &mut DaemonChild, state: &Path) -> Client {
    let client = Client::for_state(state).unwrap();
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        if runtime.run(client.ping()).is_ok() {
            return client;
        }
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon exited before becoming ready"
        );
        assert!(Instant::now() < deadline, "daemon did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn daemon_cli_serves_status_stops_and_preserves_singleton() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let mut daemon = DaemonChild::start(&state);
    let client = wait_for_client(&mut daemon, &state);
    assert!(
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(client.ping())
            .is_ok()
    );

    let status = run(&[
        "daemon".as_ref(),
        "status".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["action"],
        "daemon_status"
    );
    let value: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(value["daemon"], "online");
    assert_eq!(value["schema_version"], 5);
    assert_eq!(value["daemon_version"], value["client_version"]);
    assert!(
        value["daemon_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert!(value["version_mismatch"].is_null());
    assert!(
        value["registry_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );

    let mut second = DaemonChild::start(&state);
    assert!(
        !second.wait_for_exit().success(),
        "a second daemon unexpectedly acquired the singleton writer"
    );

    let stopped = run(&[
        "daemon".as_ref(),
        "stop".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&stopped.stdout).unwrap(),
        json!({"action": "daemon_stop", "stopped": true})
    );
    assert!(daemon.wait_for_exit().success());
}

#[test]
fn registry_cli_is_daemon_only_bounded_and_matches_native_client() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let missing = run(&[
        "registry".as_ref(),
        "resources".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(!missing.status.success());
    assert!(
        !state.exists(),
        "read-only CLI unexpectedly initialized state"
    );
    let mut daemon = DaemonChild::start(&state);
    let client = wait_for_client(&mut daemon, &state);
    let output = run(&[
        "registry".as_ref(),
        "resources".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--limit".as_ref(),
        "1".as_ref(),
        "--json".as_ref(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["action"], "registry_resources");
    let expected = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(client.registry_resources(0, 1))
        .unwrap();
    assert_eq!(
        value["records"].as_array().unwrap().len(),
        expected.records.len()
    );
    let malformed = run(&[
        "registry".as_ref(),
        "setup-ensure-events".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--limit".as_ref(),
        "65".as_ref(),
        "--json".as_ref(),
    ]);
    assert!(!malformed.status.success());
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(client.shutdown())
        .unwrap();
    assert!(daemon.wait_for_exit().success());
}

#[test]
fn malformed_daemon_arguments_do_not_create_state() {
    let root = tempfile::tempdir().unwrap();
    for (name, command, extra) in [
        ("serve-json", "serve", vec!["--json"]),
        ("serve-duplicate", "serve", vec!["--state-dir", "other"]),
        ("status-unknown", "status", vec!["--unknown"]),
        ("status-duplicate-json", "status", vec!["--json", "--json"]),
        ("stop-missing-state", "stop", vec![]),
    ] {
        let state = root.path().join(name);
        let mut args: Vec<&std::ffi::OsStr> = vec![
            "daemon".as_ref(),
            command.as_ref(),
            "--state-dir".as_ref(),
            state.as_os_str(),
        ];
        args.extend(extra.into_iter().map(std::ffi::OsStr::new));
        if name == "stop-missing-state" {
            args = vec!["daemon".as_ref(), "stop".as_ref()];
        }
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(output.stdout.is_empty(), "{name}");
        assert!(!state.exists(), "{name}");
    }

    let output = run(&[
        "daemon".as_ref(),
        "serve".as_ref(),
        "--state-dir".as_ref(),
        "".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
fn daemon_json_failures_are_stable_and_redacted() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("no-daemon-state");
    for (command, expected) in [
        (
            "status",
            json!({"action": "daemon_status", "error": "request failed"}),
        ),
        (
            "stop",
            json!({"action": "daemon_stop", "error": "request failed"}),
        ),
    ] {
        let output = run(&[
            "daemon".as_ref(),
            command.as_ref(),
            "--state-dir".as_ref(),
            state.as_os_str(),
            "--json".as_ref(),
        ]);
        assert_eq!(output.status.code(), Some(1), "{command}");
        assert!(output.stderr.is_empty(), "{command}");
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            expected,
            "{command}"
        );
        assert!(!state.exists(), "{command}");
    }
}

/// A daemon from another release (here: one that, like bosn 0.1.5 and older,
/// reports no version) is refused with the remedy before any request it could
/// misread -- 0.1.4 answered a newer client's manifest ensure with a bare
/// `ConnectionReset`. It is never stopped on the user's behalf.
#[test]
fn bosn_run_refuses_a_daemon_from_another_release_with_the_remedy() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("bosn.toml"),
        "[stack.one]\nimage = 'example.invalid/a@sha256:0000000000000000000000000000000000000000000000000000000000000000'\n[task.lint]\nstack = 'one'\ncmd = 'true'\n",
    )
    .unwrap();
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let server_state = state.clone();
    let server = std::thread::spawn(move || {
        RuntimeBuilder::multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .run(bosn_service::Service::new(server_state).serve())
    });
    let client = Client::for_state(&state).unwrap();
    let deadline = Instant::now() + READY_DEADLINE;
    while runtime.run(client.ping()).is_err() {
        assert!(Instant::now() < deadline, "old daemon did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(runtime.run(client.daemon_version()).unwrap(), "");

    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .current_dir(&workspace)
        .args(["run", "--task", "lint", "--state-dir"])
        .arg(&state)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("is an older bosn (0.1.5 or earlier"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "but this client is bosn {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "`bosn daemon stop --state-dir {}`",
            state.display()
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("ensuring stack"), "{stderr}");

    let status = run(&[
        "daemon".as_ref(),
        "status".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(status.status.success());
    let value: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(value["daemon_version"].is_null());
    assert!(
        value["version_mismatch"]
            .as_str()
            .is_some_and(|text| text.contains("bosn daemon stop"))
    );

    // The refused daemon was left running for whoever owns it.
    assert!(runtime.run(client.ping()).is_ok());
    runtime.run(client.shutdown()).unwrap();
    server.join().unwrap().unwrap();
}

/// A daemon killed without cleanup leaves its socket file behind. The next
/// `serve` holds the sole registry writer, finds nothing listening, and
/// reclaims the file instead of failing with `EndpointOccupied` forever.
#[cfg(unix)]
#[test]
fn serve_reclaims_a_socket_file_left_by_a_killed_daemon() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let mut first = DaemonChild::start(&state);
    let client = wait_for_client(&mut first, &state);
    let sockets = || {
        std::fs::read_dir(&state)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                use std::os::unix::fs::FileTypeExt as _;
                entry.file_type().is_ok_and(|kind| kind.is_socket())
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    };
    let before = sockets();
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    // Linux and macOS use a filesystem socket a SIGKILLed daemon cannot unlink.
    assert!(!before.is_empty(), "expected a filesystem socket endpoint");
    assert_eq!(sockets(), before, "the killed daemon's socket file remains");

    let mut second = DaemonChild::start(&state);
    let client_again = wait_for_client(&mut second, &state);
    assert!(
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(client_again.ping())
            .is_ok()
    );
    drop(client);
    let stop = run(&[
        "daemon".as_ref(),
        "stop".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
    ]);
    assert!(stop.status.success());
    assert!(second.wait_for_exit().success());
}
