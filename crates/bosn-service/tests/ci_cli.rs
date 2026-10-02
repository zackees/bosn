//! `bosn ci` command-line contract without Docker: golden `plan` output in
//! text and JSON (checked against the published schema), the exit-code
//! contract for refusals (3), which never start a daemon, and the run verbs
//! against a real, drained daemon (nothing executes, so no Docker).

use std::{path::Path, process::Command};

use serde_json::Value;

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

fn repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".github/workflows")).unwrap();
    std::fs::write(
        repo.join(".github/workflows/ci.yml"),
        "on: [push]\njobs: {}\n",
    )
    .unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/demo.git",
        ],
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn bosn(args: &[&str], cwd: &Path, state: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(args)
        .arg("--state-dir")
        .arg(state)
        .current_dir(cwd)
        .env("BOSN_CI_ACTOR", "human")
        // No desktop: the CLI never launches the widget from a test.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}

fn schema() -> Value {
    serde_json::from_str(include_str!("../../../docs/ci.schema.json")).unwrap()
}

/// `value`'s top-level keys are the published definition's: none extra, none
/// required missing.
fn conforms(value: &Value, definition: &str) {
    let schema = schema();
    let def = &schema["$defs"][definition];
    let properties = def["properties"].as_object().expect(definition);
    for key in value.as_object().unwrap().keys() {
        assert!(properties.contains_key(key), "{key} is not in {definition}");
    }
    for required in def["required"].as_array().unwrap() {
        let required = required.as_str().unwrap();
        assert!(
            value.get(required).is_some(),
            "{definition}: {required} missing"
        );
    }
}

#[test]
fn plan_json_conforms_to_the_published_schema_and_text_is_golden() {
    let root = tempfile::tempdir().unwrap();
    let repo = repo(root.path());
    let state = root.path().join("state");
    let out = bosn(
        &["ci", "plan", "--trigger", "pr", "--mode", "test", "--json"],
        &repo,
        &state,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan: Value = serde_json::from_slice(&out.stdout).unwrap();
    conforms(&plan, "Plan");
    assert_eq!(plan["event"], "pull_request");
    assert_eq!(plan["repository"], "example/demo");
    assert_eq!(
        plan["payload"]["pull_request"]["labels"][0]["name"],
        "ci-test"
    );
    assert!(!state.exists(), "plan never starts a daemon");

    let text = bosn(
        &["ci", "plan", "--trigger", "pr", "--mode", "test"],
        &repo,
        &state,
    );
    let sha = plan["sha"].as_str().unwrap();
    let golden = format!(
        "provider: github\nworkflow: .github/workflows/ci.yml\ntrigger: pr (pull_request)\nmode: test\nsha: {sha}\nactor: human\n"
    );
    assert_eq!(String::from_utf8_lossy(&text.stdout), golden);
}

#[test]
fn refusals_exit_3_without_starting_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    let repo = repo(root.path());
    let state = root.path().join("state");
    std::fs::write(repo.join("dirty.txt"), "uncommitted").unwrap();
    let cases: [(&[&str], &str); 5] = [
        (
            &["ci", "run", "--trigger", "release", "--mode", "full"],
            "clean tree",
        ),
        (&["ci", "list", "--state", "bogus"], "unknown state"),
        (&["ci", "logs"], "exactly one RUN"),
        (&["ci", "nope"], "usage"),
        (&["ci", "plan", "--mode", "huge"], "unknown mode"),
    ];
    for (args, expected) in cases {
        let out = bosn(args, &repo, &state);
        assert_eq!(out.status.code(), Some(3), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    std::fs::write(repo.join(".gitlab-ci.yml"), "stages: [test]\n").unwrap();
    let both = bosn(&["ci", "plan"], &repo, &state);
    assert_eq!(both.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&both.stderr).contains("pass --provider"));
    assert!(!state.exists(), "refusals never start a daemon");
}

/// Stops the daemon a test started, even when an assertion fails.
struct Daemon<'a> {
    cwd: &'a Path,
    state: &'a Path,
}
impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let _ = bosn(&["daemon", "stop"], self.cwd, self.state);
    }
}

fn json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn run_verbs_keep_their_exit_codes_and_output_against_a_drained_daemon() {
    // A Unix socket path must stay short; long sandbox temp dirs are not.
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 40 {
        std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".cache")
    } else {
        base
    };
    let root = tempfile::Builder::new()
        .prefix("bcli")
        .tempdir_in(base)
        .unwrap();
    let repo = repo(root.path());
    let state = root.path().join("s");
    let _daemon = Daemon {
        cwd: &repo,
        state: &state,
    };

    let drained = bosn(&["ci", "runners", "drain", "--json"], &repo, &state);
    assert_eq!(drained.status.code(), Some(0));
    let drained = json(&drained);
    conforms(&drained, "RunnersReply");
    assert_eq!(drained["runners"]["drained"], true);

    let submitted = bosn(&["ci", "run", "--json"], &repo, &state);
    assert_eq!(submitted.status.code(), Some(0), "queued is not a failure");
    let submitted = json(&submitted);
    conforms(&submitted, "SubmitReply");
    let run = submitted["run"].as_str().unwrap().to_string();

    let waited = bosn(
        &["ci", "wait", &run, "--deadline-ms", "300", "--json"],
        &repo,
        &state,
    );
    assert_eq!(
        waited.status.code(),
        Some(2),
        "not finished by the deadline"
    );
    let waited = json(&waited);
    conforms(&waited, "RunView");
    assert_eq!(waited["state"], "queued");

    let listed = bosn(&["ci", "list", "--json"], &repo, &state);
    assert_eq!(listed.status.code(), Some(0));
    let listed = json(&listed);
    conforms(&listed, "ListReply");
    assert_eq!(listed["runs"][0]["id"], run.as_str());
    let text = bosn(&["ci", "runners"], &repo, &state);
    assert_eq!(
        String::from_utf8_lossy(&text.stdout),
        format!(
            "runners: 0 running, 1 queued, limit {} (drained)\n",
            drained["runners"]["limit"]
        )
    );

    let cancelled = bosn(&["ci", "cancel", &run], &repo, &state);
    assert_eq!(cancelled.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&cancelled.stdout),
        format!("run {run} cancelled (done)\n")
    );
    let again = bosn(&["ci", "cancel", &run, "--json"], &repo, &state);
    assert_eq!(again.status.code(), Some(1), "nothing left to cancel");
    let again = json(&again);
    conforms(&again, "CancelReply");
    assert_eq!(again["cancelled"], false);

    let done = bosn(
        &["ci", "wait", &run, "--deadline-ms", "5000"],
        &repo,
        &state,
    );
    assert_eq!(done.status.code(), Some(2), "a cancelled run exits 2");
    let shown = bosn(&["ci", "show", &run], &repo, &state);
    assert!(
        String::from_utf8_lossy(&shown.stdout)
            .starts_with(&format!("run {run} done cancelled  sha ")),
        "{}",
        String::from_utf8_lossy(&shown.stdout)
    );
}

#[test]
fn a_command_right_after_daemon_stop_reaches_a_fresh_daemon() {
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 40 {
        std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".cache")
    } else {
        base
    };
    let root = tempfile::Builder::new()
        .prefix("bstop")
        .tempdir_in(base)
        .unwrap();
    let repo = repo(root.path());
    let state = root.path().join("s");
    let _daemon = Daemon {
        cwd: &repo,
        state: &state,
    };
    assert!(bosn(&["ci", "runners"], &repo, &state).status.success());
    for round in 0..5 {
        let stopped = bosn(&["daemon", "stop"], &repo, &state);
        assert!(stopped.status.success(), "round {round}: stop failed");
        let next = bosn(&["ci", "runners"], &repo, &state);
        assert!(
            next.status.success(),
            "round {round}: {}",
            String::from_utf8_lossy(&next.stderr)
        );
    }
}
