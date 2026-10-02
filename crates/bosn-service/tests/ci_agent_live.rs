//! Opt-in, live-Docker proof that an agent can drive local CI through MCP
//! alone: plan, run, follow logs by cursor, wait, read the failure report,
//! fix the workflow (uncommitted), rerun and see success, without ever
//! receiving an unbounded response.
//!
//! Run with:
//! `cargo test -p bosn-service --test ci_agent_live -- --ignored`
//! It needs Docker; the first run pulls the pinned engine and runner images.

use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use serde_json::{Value, json};

const MAX_TOOL_RESPONSE: usize = 64 * 1024;
const FAILING: &str = "on: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo compiling\n      - name: Check\n        run: echo broken && exit 7\n";
const FIXED: &str = "on: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo compiling\n      - name: Check\n        run: echo fixed\n";

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn start(state: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["mcp", "--state-dir"])
            .arg(state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut mcp = Self {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        mcp.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "agent-e2e", "version": "1"}}));
        mcp
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(700))
            .expect("MCP reply");
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id);
        reply["result"].clone()
    }

    /// Call one tool; every response must stay under the agent cap.
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.rpc("tools/call", json!({"name": name, "arguments": arguments}));
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            text.len() <= MAX_TOOL_RESPONSE,
            "{name} reply is {} bytes",
            text.len()
        );
        assert_ne!(result["isError"], true, "{name} failed: {text}");
        result["structuredContent"].clone()
    }

    /// Wait (in bounded calls) until the run finishes.
    fn finish(&mut self, run: &str) -> Value {
        loop {
            let status = self.tool("bosn_ci_wait", json!({"run": run, "deadline_ms": 120000}));
            if status["finished"] == true {
                return status;
            }
        }
    }

    /// Follow logs by cursor: every record exactly once, each page bounded.
    fn all_logs(&mut self, run: &str) -> Vec<String> {
        let (mut since, mut seqs, mut texts) = (0u64, Vec::new(), Vec::new());
        loop {
            let page = self.tool(
                "bosn_ci_logs",
                json!({"run": run, "since_seq": since, "limit": 50}),
            );
            for record in page["records"].as_array().unwrap() {
                seqs.push(record["seq"].as_u64().unwrap());
                texts.push(record["text"].as_str().unwrap().to_string());
            }
            since = page["next_seq"].as_u64().unwrap();
            if page["more"] == false {
                break;
            }
        }
        let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
        assert_eq!(seqs, expected, "each record exactly once, in order");
        texts
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
#[ignore = "needs Docker; see the module docs"]
fn an_agent_plans_runs_follows_fixes_and_reruns_through_mcp_alone() {
    // A Unix socket path must stay short; long sandbox temp dirs are not.
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 40 {
        std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".cache")
    } else {
        base
    };
    let root = tempfile::Builder::new()
        .prefix("bci")
        .tempdir_in(base)
        .unwrap();
    let state = root.path().join("s");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(repo.join(".github/workflows")).unwrap();
    std::fs::write(repo.join(".github/workflows/ci.yml"), FAILING).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["daemon", "serve", "--state-dir"])
        .arg(&state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let workspace = repo.to_string_lossy().to_string();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut mcp = Mcp::start(&state);
        let plan = mcp.tool("bosn_ci_plan", json!({"workspace": workspace}));
        assert_eq!(plan["event"], "push");
        let run = mcp.tool("bosn_ci_run", json!({"workspace": workspace}))["run"]
            .as_str()
            .unwrap()
            .to_string();
        let done = mcp.finish(&run);
        assert_eq!(done["conclusion"], "failure");
        assert_eq!(done["exit_code"], 1);
        assert!(mcp.all_logs(&run).iter().any(|l| l.contains("broken")));
        let report = mcp.tool("bosn_ci_report", json!({"run": run}));
        assert_eq!(report["first_failure"]["step"], "Check");
        assert_eq!(report["first_failure"]["exit_code"], 7);
        let tail = report["first_failure"]["tail"].as_array().unwrap();
        assert!(tail.iter().any(|l| l.as_str().unwrap().contains("broken")));
        assert!(
            !tail
                .iter()
                .any(|l| l.as_str().unwrap().contains("compiling")),
            "only the failing step's tail"
        );
        // The agent fixes the workflow without committing; the run sees it.
        std::fs::write(repo.join(".github/workflows/ci.yml"), FIXED).unwrap();
        let rerun = mcp.tool("bosn_ci_run", json!({"workspace": workspace}));
        assert_ne!(rerun["run"], run.as_str());
        assert!(rerun["record"]["dirty"].is_string(), "dirty tree recorded");
        let done = mcp.finish(rerun["run"].as_str().unwrap());
        assert_eq!(done["conclusion"], "success", "{done}");
        assert_eq!(done["exit_code"], 0);
    }));
    let _ = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["daemon", "stop", "--state-dir"])
        .arg(&state)
        .status();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
