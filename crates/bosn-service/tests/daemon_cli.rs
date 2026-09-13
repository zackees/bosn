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
