//! Opt-in, live-Docker proof for the MCP-owned setup-app ensure boundary.
//!
//! Every Bosn setup operation in this test travels over the production
//! `bosn mcp` stdio JSON-RPC process. Docker is used only to independently
//! inspect the resulting container and to perform ownership-checked cleanup.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bosn_engine::{CommandResult, DockerEngine, RunOptions};
use bosn_service::Client;
use kernal_api::async_engine::{Runtime, RuntimeBuilder};
use serde_json::{Value, json};

const PINNED_ALPINE: &str =
    "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b";
const READY_DEADLINE: Duration = Duration::from_secs(10);
const JOB_DEADLINE: Duration = Duration::from_secs(90);
const DOCKER_DEADLINE: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: u64 = 1024 * 1024;
const MANAGED_LABEL: &str = "com.zackees.bosn.setup-managed";
const CONTENT_LABEL: &str = "com.zackees.bosn.setup-content-sha256";
const NAME_LABEL: &str = "com.zackees.bosn.setup-container";

/// Reaps a daemon after assertions. Direct IPC below is only the daemon
/// lifecycle control: all setup plan/ensure/observation actions use MCP.
struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn start(state: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["daemon", "serve", "--state-dir"])
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start production Bosn daemon");
        Self { child }
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + READY_DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().expect("observe daemon") {
                return status;
            }
            assert!(Instant::now() < deadline, "daemon did not exit in time");
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

/// Production stdio MCP client that treats every stdout line as JSON-RPC.
/// A banner, Docker diagnostic, or other non-protocol output therefore fails
/// this test at the boundary rather than being silently tolerated.
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
            serde_json::from_str(&line).expect("MCP stdout must contain JSON-RPC only");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        assert!(response.get("error").is_none(), "MCP error: {response}");
        response["result"].clone()
    }

    fn initialize(&mut self) {
        let initialized = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "bosn-live-docker-test", "version": "1"},
            }),
        );
        assert_eq!(initialized["protocolVersion"], "2025-06-18");
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

    fn tool_error(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(
            result["isError"], true,
            "unsafe MCP tool call was accepted: {result}"
        );
        result
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait MCP server");
        assert!(status.success(), "MCP server exit: {status}");
        let mut stderr = String::new();
        self.stderr
            .read_to_string(&mut stderr)
            .expect("read MCP stderr");
        assert!(stderr.is_empty(), "MCP server wrote stderr: {stderr}");
        let mut remaining = String::new();
        self.stdout
            .read_to_string(&mut remaining)
            .expect("drain MCP stdout");
        assert!(
            remaining.is_empty(),
            "MCP server wrote unsolicited stdout: {remaining}"
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

#[derive(Debug, Eq, PartialEq)]
struct ContainerInspection {
    id: String,
    running: bool,
    image: String,
    managed: String,
    content_sha256: String,
    container_name: String,
}

fn docker_capture(
    engine: &DockerEngine,
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
) -> CommandResult {
    engine
        .with_args(args)
        .capture(RunOptions::bounded(DOCKER_DEADLINE, 64 * 1024))
        .expect("run bounded kernal-api Docker diagnostic")
}

fn inspect_container(
    engine: &DockerEngine,
    container_name: &str,
) -> Result<Option<ContainerInspection>, String> {
    let inspect_format = format!(
        "{{{{.Id}}}}\t{{{{.State.Running}}}}\t{{{{.Config.Image}}}}\t{{{{index .Config.Labels \"{MANAGED_LABEL}\"}}}}\t{{{{index .Config.Labels \"{CONTENT_LABEL}\"}}}}\t{{{{index .Config.Labels \"{NAME_LABEL}\"}}}}"
    );
    let result = docker_capture(
        engine,
        [
            "container",
            "inspect",
            "--format",
            inspect_format.as_str(),
            container_name,
        ],
    );
    if result.exit_code == 1 {
        return Ok(None);
    }
    if !result.ok() {
        return Err(format!(
            "docker inspect exited {}: {}",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let text = String::from_utf8(result.stdout)
        .map_err(|_| "docker inspect returned non-UTF-8 output".to_owned())?;
    let fields: Vec<_> = text.trim_end_matches(['\r', '\n']).split('\t').collect();
    if fields.len() != 6 || fields.iter().any(|field| field.is_empty()) {
        return Err("docker inspect returned an incomplete Bosn container record".into());
    }
    let running = match fields[1] {
        "true" => true,
        "false" => false,
        _ => return Err("docker inspect returned an invalid running value".into()),
    };
    Ok(Some(ContainerInspection {
        id: fields[0].into(),
        running,
        image: fields[2].into(),
        managed: fields[3].into(),
        content_sha256: fields[4].into(),
        container_name: fields[5].into(),
    }))
}

/// Removes only the one content-addressed container this test has proved it
/// created. It never uses a selector, prune, image removal, or name returned
/// by the engine.
struct ExactContainerCleanup {
    engine: DockerEngine,
    container_name: String,
    content_sha256: String,
}

impl Drop for ExactContainerCleanup {
    fn drop(&mut self) {
        match inspect_container(&self.engine, &self.container_name) {
            Ok(None) => {}
            Ok(Some(observed))
                if observed.managed == "v1"
                    && observed.content_sha256 == self.content_sha256
                    && observed.container_name == self.container_name =>
            {
                let result = docker_capture(
                    &self.engine,
                    ["container", "rm", "--force", self.container_name.as_str()],
                );
                if !result.ok() {
                    eprintln!(
                        "live MCP setup ensure cleanup could not remove {}: {}",
                        self.container_name,
                        String::from_utf8_lossy(&result.stderr)
                    );
                }
            }
            Ok(Some(_)) => eprintln!(
                "live MCP setup ensure cleanup refused unexpected container {}",
                self.container_name
            ),
            Err(error) => eprintln!(
                "live MCP setup ensure cleanup could not inspect {}: {error}",
                self.container_name
            ),
        }
    }
}

fn runtime() -> Runtime {
    RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test runtime")
}

fn wait_for_daemon_via_mcp(mcp: &mut McpChild) {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        let result = mcp.request(
            "tools/call",
            json!({"name": "bosn_status", "arguments": {}}),
        );
        if result["isError"] == false {
            assert!(
                result["structuredContent"]["registry_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty()),
                "MCP reported an empty registry id: {result}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not become MCP-ready: {result}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_success_and_logs(mcp: &mut McpChild, job_id: u64) {
    let deadline = Instant::now() + JOB_DEADLINE;
    loop {
        let status = mcp.tool("bosn_job_status", json!({"job_id": job_id}));
        // Logs are observed through the same public MCP cursor endpoint while
        // the job runs, not by reaching into the daemon client afterwards.
        let logs = mcp.tool(
            "bosn_job_logs",
            json!({"job_id": job_id, "after": 0, "limit": 64}),
        );
        let records = logs["records"].as_array().expect("MCP log records");
        assert!(records.iter().all(|record| {
            record["line"]
                .as_str()
                .is_some_and(|line| line.len() <= 2_048)
        }));
        match status["state"].as_str() {
            Some("Succeeded") => return,
            Some("Failed" | "Cancelled" | "Superseded") => {
                panic!("MCP setup ensure job {job_id} ended {status}; logs: {logs}");
            }
            Some("Queued" | "Running" | "Cancelling") => {}
            state => panic!("unexpected MCP setup ensure job state {state:?}: {status}"),
        }
        assert!(
            Instant::now() < deadline,
            "MCP setup ensure job {job_id} timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn pinned_alpine_identity(engine: &DockerEngine) -> String {
    let result = docker_capture(
        engine,
        ["image", "inspect", "--format", "{{.Id}}", PINNED_ALPINE],
    );
    assert!(
        result.ok(),
        "Docker image {PINNED_ALPINE:?} is unavailable: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let identity = String::from_utf8(result.stdout).expect("image identity is UTF-8");
    let identity = identity.trim().to_owned();
    assert!(!identity.is_empty(), "pinned Alpine image has no identity");
    identity
}

fn test_unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn ensure_arguments(workspace: &Path, config: &Path) -> Value {
    json!({
        "workspace": workspace,
        "config": config,
        "policy": "refresh",
        "deadline_ms": JOB_DEADLINE.as_millis() as u64,
        "output_limit": OUTPUT_LIMIT,
    })
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test mcp_setup_ensure_docker --locked -- --ignored --exact live_docker_mcp_setup_ensure_creates_and_reuses_one_managed_app`
///
/// This requires a usable Docker daemon and the exact pre-pulled
/// `PINNED_ALPINE` image. It sends setup plan, ensure, status, and log calls
/// only over production `bosn mcp` stdio JSON-RPC. It validates that raw
/// Docker, mount, and command fields fail closed at that public boundary;
/// Docker inspection is verifier-only. Cleanup rechecks all three ownership
/// labels before removing only the exact test app and never removes images.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_mcp_setup_ensure_creates_and_reuses_one_managed_app() {
    let engine = DockerEngine::docker();
    let expected_image = pinned_alpine_identity(&engine);
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let config_root = root.path().join("config");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::create_dir_all(&config_root).expect("create config directory");
    let config = config_root.join("setup.toml");
    let unique = test_unique_suffix();
    std::fs::write(
        &config,
        format!(
            "version = 1\n[app]\nimage = '{PINNED_ALPINE}'\ncommand = 'exec sleep 120 # bosn-mcp-live-{unique}'\n"
        ),
    )
    .expect("write self-contained setup document");

    let mut first_daemon = DaemonChild::start(&state);
    let mut first_mcp = McpChild::start(&state);
    first_mcp.initialize();
    wait_for_daemon_via_mcp(&mut first_mcp);
    let tools = first_mcp.request("tools/list", json!({}));
    let ensure_tool = tools["tools"]
        .as_array()
        .expect("MCP tools array")
        .iter()
        .find(|tool| tool["name"] == "bosn_setup_ensure")
        .expect("MCP must expose bosn_setup_ensure");
    assert_eq!(
        ensure_tool["inputSchema"]["additionalProperties"], false,
        "the MCP schema must remain closed to engine injection"
    );

    // Plan through MCP only, both to prove the semantic endpoint and to derive
    // the exact deterministic container identity for fail-closed cleanup.
    let plan = first_mcp.tool(
        "bosn_setup_plan",
        json!({"workspace": workspace, "config": config, "policy": "refresh"}),
    );
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["app_source"]["kind"], "pinned_image");
    let content_sha256 = plan["content_sha256"]
        .as_str()
        .expect("MCP plan content hash")
        .to_owned();
    let container_name = format!("bosn-setup-{content_sha256}");
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect deterministic test container")
            .is_none(),
        "unique test container name already exists; refusing to touch it"
    );
    let cleanup = ExactContainerCleanup {
        engine: engine.clone(),
        container_name: container_name.clone(),
        content_sha256: content_sha256.clone(),
    };

    // None of these caller-selected engine controls can cross the MCP boundary.
    for (field, value) in [
        (
            "docker_args",
            json!(["container", "rm", "--force", "foreign"]),
        ),
        ("mount", json!({"source": "/", "target": "/host"})),
        ("command", json!("touch /host/pwned")),
    ] {
        let mut arguments = ensure_arguments(&workspace, &config)
            .as_object()
            .expect("ensure arguments object")
            .clone();
        arguments.insert(field.to_owned(), value);
        let failure = first_mcp.tool_error("bosn_setup_ensure", Value::Object(arguments));
        assert_eq!(
            failure["content"][0]["text"], "unsupported tool argument",
            "MCP did not reject injected {field}"
        );
    }

    let first = first_mcp.tool("bosn_setup_ensure", ensure_arguments(&workspace, &config));
    let first_job = first["job_id"].as_u64().expect("first MCP ensure job ID");
    assert_eq!(first["action"], "setup_ensure");
    wait_for_success_and_logs(&mut first_mcp, first_job);
    let first_inspection = inspect_container(&engine, &container_name)
        .expect("inspect first managed MCP app")
        .expect("first managed MCP app exists");
    assert!(first_inspection.running, "first MCP app is not running");
    assert_eq!(first_inspection.image, expected_image);
    assert_eq!(first_inspection.managed, "v1");
    assert_eq!(first_inspection.content_sha256, content_sha256);
    assert_eq!(first_inspection.container_name, container_name);

    // Close the first stdio session before daemon restart. No setup request has
    // used Client or the native setup CLI; Client is lifecycle-only here.
    first_mcp.finish();
    let lifecycle_runtime = runtime();
    lifecycle_runtime
        .run(
            Client::for_state(&state)
                .expect("first lifecycle client")
                .shutdown(),
        )
        .expect("stop first daemon");
    assert!(
        first_daemon.wait_for_exit().success(),
        "first daemon failed"
    );

    let mut second_daemon = DaemonChild::start(&state);
    let mut second_mcp = McpChild::start(&state);
    second_mcp.initialize();
    wait_for_daemon_via_mcp(&mut second_mcp);
    let second = second_mcp.tool("bosn_setup_ensure", ensure_arguments(&workspace, &config));
    let second_job = second["job_id"].as_u64().expect("second MCP ensure job ID");
    wait_for_success_and_logs(&mut second_mcp, second_job);
    let second_inspection = inspect_container(&engine, &container_name)
        .expect("inspect reused managed MCP app")
        .expect("reused managed MCP app exists");
    assert!(second_inspection.running, "reused MCP app is not running");
    assert_eq!(
        second_inspection.id, first_inspection.id,
        "fresh MCP session/daemon replaced a matching app instead of reusing it"
    );
    assert_eq!(second_inspection.image, expected_image);
    assert_eq!(second_inspection.managed, "v1");
    assert_eq!(second_inspection.content_sha256, content_sha256);
    assert_eq!(second_inspection.container_name, container_name);

    second_mcp.finish();
    lifecycle_runtime
        .run(
            Client::for_state(&state)
                .expect("second lifecycle client")
                .shutdown(),
        )
        .expect("stop second daemon");
    assert!(
        second_daemon.wait_for_exit().success(),
        "second daemon failed"
    );
    assert!(
        std::fs::read_dir(&workspace)
            .expect("read workspace")
            .next()
            .is_none(),
        "setup ensure wrote into the caller-selected workspace"
    );
    drop(cleanup);
    assert!(
        inspect_container(&engine, &container_name)
            .expect("inspect exact container after cleanup")
            .is_none(),
        "exact live-test container remained after cleanup"
    );
}
