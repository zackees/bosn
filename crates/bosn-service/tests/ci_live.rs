//! Opt-in, live-Docker proofs of `bosn ci`'s host contract (#342):
//!
//! - the host engine's containers, networks, volumes and images are the same
//!   after a run ends in success, failure, timeout, a killed client and a
//!   killed daemon;
//! - act never sees the host socket: the engine has no bind mount, and the
//!   socket inside it belongs to the nested daemon;
//! - the recorded matrix/`needs:`/failure fixture yields the right tree,
//!   exit 1, and a report holding only the failing step's tail;
//! - a second run restores `actions/cache` from the local cache server.
//!
//! Run with:
//! `cargo test -p bosn-service --test ci_live -- --ignored --test-threads 1`
//! It needs Docker and network access (runner image, `actions/cache`).
//! The leak check compares the whole host engine, so run it on a host where
//! nothing else is creating Docker resources at the same time.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;

const SUCCESS: &str =
    "on: [push]\njobs:\n  ok:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo fine\n";
const FAILURE: &str =
    "on: [push]\njobs:\n  bad:\n    runs-on: ubuntu-latest\n    steps:\n      - run: exit 4\n";
const SLEEP: &str = "on: [push]\njobs:\n  slow:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo started && sleep SECONDS\n";

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
    assert!(status.success());
}

/// A checkout, a state directory with a short path (Unix socket limit) and
/// the daemon serving it.
struct Live {
    _root: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
    daemon: Option<Child>,
}

impl Live {
    fn new() -> Self {
        let base = std::env::temp_dir();
        let base = if base.as_os_str().len() > 40 {
            PathBuf::from(std::env::var("HOME").unwrap()).join(".cache")
        } else {
            base
        };
        let root = tempfile::Builder::new()
            .prefix("blive")
            .tempdir_in(base)
            .unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".github/workflows")).unwrap();
        std::fs::write(repo.join(".github/workflows/ci.yml"), SUCCESS).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        let mut live = Self {
            state: root.path().join("s"),
            repo,
            _root: root,
            daemon: None,
        };
        live.start_daemon();
        live
    }

    fn start_daemon(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["daemon", "serve", "--state-dir"])
            .arg(&self.state)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        wait_until("the daemon answers", Duration::from_secs(30), || {
            self.cli(&["ci", "runners", "--json"]).status.success()
        });
    }

    /// SIGKILL the daemon mid-run, as a crash would.
    fn kill_daemon(&mut self) {
        let mut child = self.daemon.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn workflow(&self, text: &str) {
        std::fs::write(self.repo.join(".github/workflows/ci.yml"), text).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bosn"));
        command
            .args(args)
            .arg("--state-dir")
            .arg(&self.state)
            .current_dir(&self.repo)
            .env("BOSN_CI_ACTOR", "human")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> (Option<i32>, Value) {
        let out = self.cli(args);
        let value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?}: {e}: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code(), value)
    }

    /// Queue a run of the current workflow; returns its ID.
    fn submit(&self, extra: &[&str]) -> String {
        let mut args = vec!["ci", "run", "--json"];
        args.extend_from_slice(extra);
        let (code, reply) = self.json(&args);
        assert_eq!(code, Some(0), "{reply}");
        reply["run"].as_str().unwrap().to_string()
    }

    fn show(&self, run: &str) -> Value {
        self.json(&["ci", "show", run, "--json"]).1
    }

    /// Wait for a run to end; returns the CLI's exit code and the record.
    fn wait(&self, run: &str) -> (Option<i32>, Value) {
        self.json(&["ci", "wait", run, "--deadline-ms", "900000", "--json"])
    }

    /// Until the run's job is executing (its first step printed).
    fn wait_running(&self, run: &str) {
        wait_until("the job starts", Duration::from_secs(600), || {
            let view = self.show(run);
            view["state"] == "running"
                && String::from_utf8_lossy(&self.cli(&["ci", "logs", run]).stdout)
                    .contains("started")
        });
    }

    fn logs(&self, run: &str) -> String {
        String::from_utf8_lossy(&self.cli(&["ci", "logs", run]).stdout).into_owned()
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.cli(&["daemon", "stop"]);
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Everything on the host engine a run could leave behind.
#[derive(Debug, PartialEq, Eq)]
struct HostSnapshot {
    containers: BTreeSet<String>,
    networks: BTreeSet<String>,
    volumes: BTreeSet<String>,
    images: BTreeSet<String>,
}

impl HostSnapshot {
    fn take() -> Self {
        let set = |args: &[&str]| docker(args).lines().map(str::to_string).collect();
        Self {
            containers: set(&["ps", "-aq", "--no-trunc"]),
            networks: set(&["network", "ls", "-q", "--no-trunc"]),
            volumes: set(&["volume", "ls", "-q"]),
            images: set(&["images", "-aq", "--no-trunc"]),
        }
    }

    /// The host is back to `self` within `limit` (cleanup finishes before a
    /// run reads `done`, but recovery after a daemon crash is asynchronous).
    fn restored_within(&self, scenario: &str, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            let now = Self::take();
            if &now == self {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{scenario}: the host engine changed\nbefore: {self:#?}\nafter: {now:#?}"
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// The engine container serving `run` never sees the host socket.
fn assert_isolated(run: &str) {
    let engine = format!("bosn-act-{run}");
    let mounts: Value = serde_json::from_str(&docker(&[
        "inspect",
        "--format",
        "{{json .Mounts}}",
        &engine,
    ]))
    .unwrap();
    for mount in mounts.as_array().unwrap() {
        assert_eq!(mount["Type"], "volume", "no bind mounts: {mount}");
        let text = mount.to_string();
        assert!(
            !text.contains("docker.sock"),
            "host socket mounted: {mount}"
        );
    }
    let docker_host = docker(&["exec", &engine, "sh", "-c", "echo ${DOCKER_HOST:-unset}"]);
    assert!(
        docker_host == "unset" || docker_host == "unix:///var/run/docker.sock",
        "act's docker client points at {docker_host}"
    );
    let host = docker(&["info", "--format", "{{.ID}}"]);
    let nested = docker(&["exec", &engine, "docker", "info", "--format", "{{.ID}}"]);
    assert_ne!(
        host, nested,
        "the socket inside the engine is the nested daemon"
    );
}

#[test]
#[ignore = "needs Docker; see the module docs"]
fn the_host_engine_is_unchanged_after_every_way_a_run_can_end() {
    let mut live = Live::new();
    // Warm up: the cache volume, engine image and runner image exist after
    // the first run by design; the baseline is the host after it.
    let (code, _) = live.wait(&live.submit(&[]));
    assert_eq!(code, Some(0));
    let baseline = HostSnapshot::take();
    let settle = Duration::from_secs(30);

    let (code, view) = live.wait(&live.submit(&[]));
    assert_eq!(
        (code, &view["conclusion"]),
        (Some(0), &Value::from("success"))
    );
    baseline.restored_within("success", settle);

    live.workflow(FAILURE);
    let (code, view) = live.wait(&live.submit(&[]));
    assert_eq!(
        (code, &view["conclusion"]),
        (Some(1), &Value::from("failure"))
    );
    baseline.restored_within("failure", settle);

    // The timeout covers planning and preparing too, so leave room for both.
    live.workflow(&SLEEP.replace("SECONDS", "600"));
    let run = live.submit(&["--timeout-secs", "90"]);
    live.wait_running(&run);
    assert_isolated(&run);
    let (code, view) = live.wait(&run);
    assert_eq!(
        (code, &view["conclusion"]),
        (Some(2), &Value::from("timed_out"))
    );
    baseline.restored_within("timeout", settle);

    // A client killed while it waits: the run carries on and is cleaned up.
    live.workflow(&SLEEP.replace("SECONDS", "15"));
    let mut client = live
        .command(&["ci", "run", "--wait", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the run is listed", Duration::from_secs(60), || {
        live.json(&["ci", "list", "--json", "--limit", "1"]).1["runs"][0]["state"] != "done"
    });
    let run = live.json(&["ci", "list", "--json", "--limit", "1"]).1["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    live.wait_running(&run);
    client.kill().unwrap();
    client.wait().unwrap();
    let (code, view) = live.wait(&run);
    assert_eq!(
        (code, &view["conclusion"]),
        (Some(0), &Value::from("success"))
    );
    baseline.restored_within("client SIGKILL", settle);

    // A daemon killed mid-run: the next daemon marks the run interrupted
    // and removes its engine.
    live.workflow(&SLEEP.replace("SECONDS", "600"));
    let run = live.submit(&[]);
    live.wait_running(&run);
    live.kill_daemon();
    live.start_daemon();
    let (code, view) = live.wait(&run);
    assert_eq!(code, Some(1), "{view}");
    assert!(
        view["reason"].as_str().unwrap().contains("interrupted"),
        "{view}"
    );
    baseline.restored_within("daemon SIGKILL", Duration::from_secs(180));
}

#[test]
#[ignore = "needs Docker; see the module docs"]
fn the_recorded_fixture_yields_its_tree_exit_1_and_only_the_failing_tail() {
    let live = Live::new();
    live.workflow(include_str!(
        "fixtures/act/act-0.2.88-matrix-needs-failure.yml"
    ));
    let run = live.submit(&[]);
    let (code, view) = live.wait(&run);
    assert_eq!(code, Some(1), "{view}");
    assert_eq!(view["conclusion"], "failure");
    let jobs: Vec<&Value> = view["tree"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["jobs"].as_array().unwrap())
        .collect();
    let legs: Vec<&Value> = jobs
        .iter()
        .copied()
        .filter(|j| j["job_id"] == "a")
        .collect();
    assert_eq!(legs.len(), 2, "two matrix legs: {jobs:#?}");
    for leg in &legs {
        assert_eq!(leg["conclusion"], "success");
        assert!(
            leg["sections"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["conclusion"] == "skipped"),
            "the `if: false` step is listed as skipped: {leg}"
        );
    }
    let b = jobs.iter().find(|j| j["job_id"] == "b").expect("job b");
    assert_eq!(b["conclusion"], "failure");
    let stages: Vec<&str> = view["tree"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(stages, ["0", "1"], "`needs: a` puts b in a later stage");

    let (code, report) = live.json(&["ci", "report", &run, "--json"]);
    assert_eq!(code, Some(1));
    let failure = &report["first_failure"];
    assert_eq!(failure["job_name"], "b", "{report}");
    assert_eq!(failure["exit_code"], 3, "{report}");
    let tail = failure["tail"].to_string();
    assert!(
        !tail.contains("hello"),
        "another job's output leaked: {tail}"
    );
    assert!(
        !tail.contains("before"),
        "an earlier step's output leaked: {tail}"
    );
    assert_eq!(report["coverage_complete"], true);
}

#[test]
#[ignore = "needs Docker and network access; see the module docs"]
fn a_second_run_restores_actions_cache_from_the_local_server() {
    let live = Live::new();
    // A key no earlier test run used, so the first run must miss.
    let key = format!("bosn-live-{}", std::process::id());
    live.workflow(&format!(
        "on: [push]\njobs:\n  c:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/cache@v4\n        with:\n          path: cached\n          key: {key}\n      - run: mkdir -p cached && echo kept > cached/x\n"
    ));
    let first = live.submit(&[]);
    assert_eq!(live.wait(&first).0, Some(0));
    assert!(
        !live.logs(&first).contains("Cache restored"),
        "a fresh key cannot hit"
    );
    let second = live.submit(&[]);
    assert_eq!(live.wait(&second).0, Some(0));
    assert!(
        live.logs(&second).contains("Cache restored"),
        "{}",
        live.logs(&second)
    );
}
