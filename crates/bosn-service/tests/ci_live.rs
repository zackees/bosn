//! Opt-in, live-Docker proofs of `bosn ci`'s host contract (#342):
//!
//! - the host engine's containers, networks, volumes and images are the same
//!   after a run ends in success, failure, timeout, a killed client and a
//!   killed daemon;
//! - act never sees the host socket: the engine has no bind mount, and the
//!   socket inside it belongs to the nested daemon;
//! - the recorded matrix/`needs:`/failure fixture yields the right tree,
//!   exit 1, and a report holding only the failing step's tail;
//! - a second run restores `actions/cache` from the local cache server;
//! - a run claims the prepared spare engine (#410), a new one is prepared,
//!   and stopping the daemon leaves nothing it owned on the host.
//!
//! Run with:
//! `cargo test -p bosn-service --test ci_live -- --ignored --test-threads 1`
//! It needs Docker and network access (runner image, `actions/cache`).
//! The leak check compares what bosn owns on the host engine (its ownership
//! label and engine names), so other tools using Docker concurrently do not
//! disturb it.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use bosn_service::ci::reply::RunView;
use serde::de::DeserializeOwned;
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
    /// A daemon without a spare engine: one engine per run, named after it.
    fn new() -> Self {
        Self::with_spares(false)
    }

    fn with_spares(spares: bool) -> Self {
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
        let state = root.path().join("s");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("config.toml"),
            format!("[engine]\nspares = {}\n", u8::from(spares)),
        )
        .unwrap();
        let mut live = Self {
            state,
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

    /// `bosn daemon stop`, then wait for the process to exit.
    fn stop_daemon(&mut self) {
        assert!(self.cli(&["daemon", "stop"]).status.success());
        let mut child = self.daemon.take().unwrap();
        child.wait().unwrap();
    }

    /// Until the daemon reports a ready spare engine other than `not`; its name.
    fn spare_ready(&self, not: Option<&str>, limit: Duration) -> String {
        let mut name = String::new();
        wait_until("a spare engine is ready", limit, || {
            let spare = &self.json(&["ci", "runners", "--json"]).1["runners"]["spare"];
            name = spare["engine"].as_str().unwrap_or_default().to_string();
            spare["state"] == "ready" && Some(name.as_str()) != not
        });
        name
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
        self.wait_as(run)
    }

    fn wait_as<T: DeserializeOwned>(&self, run: &str) -> (Option<i32>, T) {
        let out = self.cli(&["ci", "wait", run, "--deadline-ms", "900000", "--json"]);
        let reply = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
            panic!(
                "wait {run}: {error}: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code(), reply)
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

/// The bosn ownership label every engine, cache volume and network carries.
const OWNED: &str = "label=com.zackees.bosn.registry";

/// Everything on the host engine a run could leave behind: what bosn owns
/// (its label, its `bosn-act-` engine names) and the engine image's
/// repository. Other tools' resources are ignored, so the check holds on a
/// machine where something else uses Docker at the same time; the isolation
/// assertions prove act never reaches the host engine at all.
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
        let mut containers: BTreeSet<String> = set(&["ps", "-aq", "--no-trunc", "--filter", OWNED]);
        containers.extend(set(&[
            "ps",
            "-aq",
            "--no-trunc",
            "--filter",
            "name=bosn-act-",
        ]));
        Self {
            containers,
            networks: set(&["network", "ls", "-q", "--no-trunc", "--filter", OWNED]),
            volumes: set(&["volume", "ls", "-q", "--filter", OWNED]),
            images: set(&["images", "-aq", "--no-trunc", "docker"]),
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
    let key = format!("bosn-live-{}", std::process::id());
    live.workflow(&format!(
        "on: [push]\njobs:\n{}",
        cache_job("c", &key, "mkdir -p cached && echo kept > cached/x", None)
    ));
    let first = live.submit(&[]);
    let (code, first_view) = live.wait_as::<RunView>(&first);
    assert_eq!(code, Some(0), "{first_view:?}\n{}", live.logs(&first));
    assert!(
        !live.logs(&first).contains("Cache restored"),
        "a fresh key cannot hit"
    );
    // Verify actual bytes in a different engine, rather than a cache-hit log alone.
    live.workflow(&format!(
        "on: [push]\njobs:\n{}",
        cache_job(
            "c",
            &key,
            "test \"$(cat cached/x)\" = kept && echo RESTORED_BYTES",
            None
        )
    ));
    let second = live.submit(&[]);
    let (code, second_view) = live.wait_as::<RunView>(&second);
    assert_eq!(code, Some(0), "{second_view:?}\n{}", live.logs(&second));
    assert!(first_view.record.engine_id.is_some(), "{first_view:?}");
    assert_ne!(first_view.record.engine_id, second_view.record.engine_id);
    let logs = live.logs(&second);
    assert!(logs.contains("Cache restored"), "{logs}");
    assert!(logs.contains("RESTORED_BYTES"), "{logs}");
}

#[test]
#[ignore = "needs Docker and network access; see the module docs"]
fn a_later_job_restores_actions_cache_from_an_earlier_job() {
    let live = Live::new();
    let key = format!("bosn-jobs-{}", std::process::id());
    live.workflow(&format!(
        "on: [push]\njobs:\n{}{}",
        cache_job(
            "save",
            &key,
            "mkdir -p cached && echo kept > cached/x",
            None
        ),
        cache_job(
            "restore",
            &key,
            "test \"$(cat cached/x)\" = kept && echo JOB_RESTORED_BYTES",
            Some("save")
        )
    ));
    let run = live.submit(&[]);
    let (code, view) = live.wait_as::<RunView>(&run);
    let logs = live.logs(&run);
    assert_eq!(code, Some(0), "{view:?}\n{logs}");
    assert_eq!(view.jobs.total, 2, "{view:?}");
    assert_eq!(view.jobs.completed, 2, "{view:?}");
    assert!(logs.contains("Cache restored"), "{logs}");
    assert!(logs.contains("JOB_RESTORED_BYTES"), "{logs}");
}

#[test]
#[ignore = "needs Docker and network access; see the module docs"]
fn repository_namespaces_do_not_restore_each_others_archives() {
    let writer = Live::new();
    let reader = Live::new();
    let key = format!("bosn-namespaces-{}", std::process::id());
    writer.workflow(&format!(
        "on: [push]\njobs:\n{}",
        cache_job("c", &key, "mkdir -p cached && echo kept > cached/x", None)
    ));
    let run = writer.submit(&[]);
    let (code, source) = writer.wait_as::<RunView>(&run);
    assert_eq!(code, Some(0), "{source:?}\n{}", writer.logs(&run));
    reader.workflow(&format!(
        "on: [push]\njobs:\n{}",
        cache_job(
            "c",
            &key,
            "test ! -e cached/x && echo REPOSITORY_CACHE_MISS",
            None
        )
    ));
    let run = reader.submit(&[]);
    let (code, target) = reader.wait_as::<RunView>(&run);
    let logs = reader.logs(&run);
    assert_eq!(code, Some(0), "{target:?}\n{logs}");
    assert_ne!(
        source.record.cache_namespace(),
        target.record.cache_namespace()
    );
    assert!(!logs.contains("Cache restored"), "{logs}");
    assert!(logs.contains("REPOSITORY_CACHE_MISS"), "{logs}");
}

fn cache_job(name: &str, key: &str, command: &str, needs: Option<&str>) -> String {
    let needs = needs
        .map(|job| format!("    needs: {job}\n"))
        .unwrap_or_default();
    format!(
        "  {name}:\n{needs}    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/cache@v4\n        with:\n          path: cached\n          key: {key}\n      - run: {command}\n"
    )
}

/// A container's or volume's labels and immutable ID.
fn inspect(target: &str, format: &str) -> String {
    docker(&["inspect", "--format", format, target])
}

/// What one registry owns on the host engine: its containers, networks and
/// volumes (the machine-wide cache volume may carry another registry's
/// label, and outlives every daemon by design).
fn owned_by(registry: &str) -> (String, String, String) {
    let filter = format!("label=com.zackees.bosn.registry={registry}");
    (
        docker(&["ps", "-aq", "--no-trunc", "--filter", &filter]),
        docker(&["network", "ls", "-q", "--filter", &filter]),
        docker(&["volume", "ls", "-q", "--filter", &filter]),
    )
}

#[test]
#[ignore = "needs Docker; see the module docs"]
fn a_run_claims_the_spare_a_new_one_is_prepared_and_stop_removes_it() {
    let images = docker(&["images", "-aq", "--no-trunc", "docker"]);
    let mut live = Live::with_spares(true);
    // A daemon keeps a spare once it runs CI; the first run may pull images.
    let warm = live.submit(&[]);
    let (code, view) = live.wait(&warm);
    assert_eq!(code, Some(0), "{view}\n{}", live.logs(&warm));
    let first = live.spare_ready(None, Duration::from_secs(600));
    let registry = inspect(
        &first,
        "{{index .Config.Labels \"com.zackees.bosn.registry\"}}",
    );
    let first_id = inspect(&first, "{{.Id}}");
    assert_eq!(
        inspect(
            &first,
            "{{index .Config.Labels \"com.zackees.bosn.act.spare\"}}"
        ),
        "true"
    );
    let run = live.submit(&[]);
    let (code, view) = live.wait(&run);
    assert_eq!(
        (code, &view["conclusion"]),
        (Some(0), &Value::from("success")),
        "{view}"
    );
    assert_eq!(
        view["engine_id"],
        Value::from(first_id.as_str()),
        "the run ran on the spare"
    );
    let logs = live.logs(&run);
    assert!(
        logs.contains(&format!("claiming prepared spare engine {first}")),
        "{logs}"
    );
    assert!(logs.contains("spare engine claimed in"), "{logs}");
    assert!(!logs.contains("creating isolated engine"), "{logs}");
    assert!(!logs.contains("preparing engine"), "{logs}");
    if let Some(prepared) = logs
        .lines()
        .find(|line| line.contains("engine prepared in"))
    {
        eprintln!("spare run: {prepared}");
    }
    // A replacement is prepared; the claimed spare is gone with its run.
    let second = live.spare_ready(Some(&first), Duration::from_secs(300));
    assert_ne!(second, first);
    let (containers, _, _) = owned_by(&registry);
    assert_eq!(
        containers,
        inspect(&second, "{{.Id}}"),
        "only the new spare"
    );
    // Stopping the daemon removes it: nothing this daemon owned is left.
    live.stop_daemon();
    assert_eq!(
        owned_by(&registry),
        (String::new(), String::new(), String::new())
    );
    assert_eq!(docker(&["images", "-aq", "--no-trunc", "docker"]), images);
}
