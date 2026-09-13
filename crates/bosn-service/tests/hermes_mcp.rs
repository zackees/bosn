//! Opt-in acceptance proof for the pinned Hermes MCP client and `bosn mcp`.
//!
//! Hermes Agent has a native registration/discovery command, but deliberately
//! has no CLI subcommand that invokes an MCP tool without starting an
//! inference-backed agent session. This test therefore proves registration and
//! discovery through the pinned Hermes executable, then exercises semantic
//! calls over a separate production stdio session using the same executable
//! and environment Hermes registered. No unit-test backend is involved.
//!
//! Run only where Hermes Agent `0.21.0` is installed:
//! `BOSN_HERMES_ACCEPTANCE=1 cargo test -p bosn-service --test hermes_mcp --
//! --ignored --exact hermes_agent_stdio_contract_survives_daemon_restart`.

#![cfg(unix)]

use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

use bosn_service::Client;
use kernal_api::async_engine::RuntimeBuilder;
use serde_json::{Value, json};

const HERMES_VERSION: &str = "0.21.0";
const READY_DEADLINE: Duration = Duration::from_secs(10);
const JOB_DEADLINE: Duration = Duration::from_secs(10);
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn start(state: &Path, fake_bin: &Path) -> Self {
        let inherited_path = env::var_os("PATH").unwrap_or_default();
        let path = env::join_paths(
            std::iter::once(fake_bin.to_path_buf()).chain(env::split_paths(&inherited_path)),
        )
        .expect("construct daemon PATH");
        let child = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .env("PATH", path)
            .args(["daemon", "serve", "--state-dir"])
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start production Bosn daemon");
        Self { child }
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + READY_DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().expect("observe daemon") {
                assert!(status.success(), "daemon did not stop cleanly: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "daemon did not stop in time");
            std::thread::sleep(Duration::from_millis(20));
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

struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: ChildStderr,
    next_id: u64,
}

impl McpChild {
    fn start(state: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .env("BOSN_STATE_DIR", state)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start production Bosn MCP server");
        Self {
            stdin: Some(child.stdin.take().expect("MCP stdin")),
            stdout: BufReader::new(child.stdout.take().expect("MCP stdout")),
            stderr: child.stderr.take().expect("MCP stderr"),
            child,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let stdin = self.stdin.as_mut().expect("MCP stdin remains open");
        serde_json::to_writer(
            &mut *stdin,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .expect("encode MCP request");
        stdin.write_all(b"\n").expect("write MCP newline");
        stdin.flush().expect("flush MCP request");

        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read MCP response");
        assert!(!line.is_empty(), "MCP server closed stdout before response");
        let response: Value =
            serde_json::from_str(&line).expect("MCP stdout must be JSON-RPC only");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        assert!(response.get("error").is_none(), "MCP error: {response}");
        response["result"].clone()
    }

    fn notify_initialized(&mut self) {
        let stdin = self.stdin.as_mut().expect("MCP stdin remains open");
        serde_json::to_writer(
            &mut *stdin,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .expect("encode initialized notification");
        stdin
            .write_all(b"\n")
            .expect("write initialized notification");
        stdin.flush().expect("flush initialized notification");
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(result["isError"], false, "MCP tool failed: {result}");
        result["structuredContent"].clone()
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait MCP server");
        assert!(status.success(), "MCP server exit: {status}");
        let mut stderr = String::new();
        self.stderr
            .read_to_string(&mut stderr)
            .expect("read MCP stderr");
        assert!(
            stderr.is_empty(),
            "MCP server wrote diagnostics to stderr: {stderr}"
        );
        let mut remaining = String::new();
        self.stdout
            .read_to_string(&mut remaining)
            .expect("drain MCP stdout");
        assert!(
            remaining.is_empty(),
            "MCP server wrote non-requested stdout: {remaining}"
        );
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn runtime() -> kernal_api::async_engine::Runtime {
    RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test runtime")
}

fn wait_for_daemon(daemon: &mut DaemonChild, state: &Path) -> Client {
    let client = Client::for_state(state).expect("client");
    let runtime = runtime();
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        if runtime.run(client.ping()).is_ok() {
            return client;
        }
        assert!(daemon.child.try_wait().expect("observe daemon").is_none());
        assert!(Instant::now() < deadline, "daemon did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_job(mcp: &mut McpChild, id: u64, expected: &str) -> Value {
    let deadline = Instant::now() + JOB_DEADLINE;
    loop {
        let status = mcp.tool("bosn_job_status", json!({"job_id": id}));
        if status["state"] == expected {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} never became {expected}: {status}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_fake_docker(directory: &Path) {
    let docker = directory.join("docker");
    fs::write(
        &docker,
        "#!/bin/sh\nif [ \"$1\" = image ] && [ \"$2\" = pull ]; then\n  echo 'fake Docker pull started' >&2\n  exec /bin/sleep 60\nfi\necho \"unexpected fake Docker invocation: $*\" >&2\nexit 97\n",
    )
    .expect("write fake Docker");
    let mut permissions = fs::metadata(&docker)
        .expect("fake Docker metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(docker, permissions).expect("make fake Docker executable");
}

fn hermes(command: &[OsString], hermes_home: &Path, stdin: Option<&[u8]>) -> std::process::Output {
    let mut process = Command::new("hermes");
    process
        .args(command)
        .env("HERMES_HOME", hermes_home)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().expect("start Hermes Agent");
    if let Some(stdin) = stdin {
        child
            .stdin
            .as_mut()
            .expect("Hermes stdin")
            .write_all(stdin)
            .expect("answer Hermes registration prompt");
    }
    child.wait_with_output().expect("wait Hermes Agent")
}

fn hermes_stdout(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "Hermes failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("Hermes stdout UTF-8")
}

/// This is intentionally both ignored and environment-gated: it needs a
/// user-installed, pinned Hermes client and creates an isolated local daemon.
#[test]
#[ignore = "requires BOSN_HERMES_ACCEPTANCE=1 and Hermes Agent 0.21.0"]
fn hermes_agent_stdio_contract_survives_daemon_restart() {
    if env::var("BOSN_HERMES_ACCEPTANCE").as_deref() != Ok("1") {
        eprintln!("set BOSN_HERMES_ACCEPTANCE=1 to run the Hermes acceptance proof");
        return;
    }

    let version = Command::new("hermes")
        .arg("--version")
        .output()
        .expect("Hermes Agent must be installed");
    let version_stdout = hermes_stdout(&version);
    assert!(
        version_stdout.contains(&format!("Hermes Agent v{HERMES_VERSION}")),
        "expected pinned Hermes Agent {HERMES_VERSION}, got: {version_stdout}"
    );

    let root = tempfile::tempdir().expect("acceptance tempdir");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let fake_bin = root.path().join("fake-bin");
    let hermes_home = root.path().join("hermes-home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&fake_bin).expect("fake Docker directory");
    write_fake_docker(&fake_bin);
    let config = root.path().join("setup.toml");
    fs::write(
        &config,
        format!("version = 1\n[app]\nimage = 'registry.example/demo@sha256:{DIGEST}'\n"),
    )
    .expect("setup document");

    let mut daemon = DaemonChild::start(&state, &fake_bin);
    let client = wait_for_daemon(&mut daemon, &state);

    let binary = PathBuf::from(env!("CARGO_BIN_EXE_bosn"));
    let mut add_args = vec![
        OsString::from("mcp"),
        OsString::from("add"),
        OsString::from("bosn-contract"),
        OsString::from("--command"),
        binary.into_os_string(),
        OsString::from("--env"),
        OsString::from(format!("BOSN_STATE_DIR={}", state.display())),
    ];
    if let Some(value) = env::var_os("LD_LIBRARY_PATH") {
        if !value.is_empty() {
            add_args.push(OsString::from(format!(
                "LD_LIBRARY_PATH={}",
                value.to_string_lossy()
            )));
        }
    }
    add_args.extend([OsString::from("--args"), OsString::from("mcp")]);
    let registered = hermes(&add_args, &hermes_home, Some(b"y\n"));
    let registered_stdout = hermes_stdout(&registered);
    assert!(registered_stdout.contains("Connected! Found 8 tool(s)"));
    assert!(registered_stdout.contains("Saved 'bosn-contract'"));

    let discovery = hermes(
        &[
            OsString::from("mcp"),
            OsString::from("test"),
            OsString::from("bosn-contract"),
        ],
        &hermes_home,
        None,
    );
    let discovery_stdout = hermes_stdout(&discovery);
    for tool in [
        "bosn_status",
        "bosn_job_status",
        "bosn_job_logs",
        "bosn_job_cancel",
        "bosn_setup_plan",
        "bosn_setup_prepare",
        "bosn_setup_ensure",
        "bosn_setup_task",
    ] {
        assert!(
            discovery_stdout.contains(tool),
            "Hermes did not discover {tool}"
        );
    }

    // The production server is launched with exactly the daemon state Hermes
    // registered above. Every response is parsed as a JSON-RPC line so a
    // banner or diagnostic on stdout fails this black-box session immediately.
    let mut mcp = McpChild::start(&state);
    let initialized = mcp.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "hermes-acceptance", "version": HERMES_VERSION}}),
    );
    assert_eq!(initialized["protocolVersion"], "2025-06-18");
    mcp.notify_initialized();
    let listed = mcp.request("tools/list", json!({}));
    assert_eq!(listed["tools"].as_array().expect("tools array").len(), 8);

    let status = mcp.tool("bosn_status", json!({}));
    assert!(
        status["registry_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );

    let plan = mcp.tool(
        "bosn_setup_plan",
        json!({"workspace": workspace, "config": config, "policy": "refresh"}),
    );
    assert_eq!(plan["action"], "plan");
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["app_source"]["kind"], "pinned_image");

    // The daemon owns the fake Docker process. Hermes/MCP only submits the
    // semantic prepare request, observes its bounded logs, and cancels it.
    let prepare = mcp.tool(
        "bosn_setup_prepare",
        json!({
            "workspace": workspace,
            "config": config,
            "policy": "refresh",
            "deadline_ms": 60_000,
            "output_limit": 8_192,
        }),
    );
    let job_id = prepare["job_id"].as_u64().expect("prepare job ID");
    let running = wait_for_job(&mut mcp, job_id, "Running");
    assert_eq!(running["job_id"], job_id);

    let deadline = Instant::now() + JOB_DEADLINE;
    loop {
        let logs = mcp.tool(
            "bosn_job_logs",
            json!({"job_id": job_id, "after": 0, "limit": 64}),
        );
        assert!(logs["records"].as_array().is_some());
        if logs["records"]
            .as_array()
            .expect("records array")
            .iter()
            .any(|record| {
                record["line"]
                    .as_str()
                    .is_some_and(|line| line.contains("fake Docker pull started"))
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fake Docker log was not observable: {logs}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let cancelled = mcp.tool("bosn_job_cancel", json!({"job_id": job_id}));
    assert_eq!(
        cancelled,
        json!({"job_id": job_id, "cancel_requested": true})
    );
    assert_eq!(
        wait_for_job(&mut mcp, job_id, "Cancelled")["state"],
        "Cancelled"
    );

    runtime().run(client.shutdown()).expect("stop first daemon");
    daemon.wait_for_exit();
    let offline = mcp.request(
        "tools/call",
        json!({"name": "bosn_status", "arguments": {}}),
    );
    assert_eq!(
        offline["isError"], true,
        "stopped daemon unexpectedly remained reachable"
    );

    let mut restarted = DaemonChild::start(&state, &fake_bin);
    wait_for_daemon(&mut restarted, &state);
    let recovered = mcp.tool("bosn_status", json!({}));
    assert!(
        recovered["registry_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );

    runtime()
        .run(
            Client::for_state(&state)
                .expect("restart client")
                .shutdown(),
        )
        .expect("stop restarted daemon");
    restarted.wait_for_exit();
    mcp.finish();
}
