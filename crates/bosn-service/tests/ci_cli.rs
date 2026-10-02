//! `bosn ci` command-line contract without Docker: golden `plan` output in
//! text and JSON (checked against the published schema), and the exit-code
//! contract for refusals (3), which never start a daemon.

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
        .output()
        .unwrap()
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
    let schema: Value = serde_json::from_str(include_str!("../../../docs/ci.schema.json")).unwrap();
    let plan_schema = &schema["$defs"]["Plan"];
    let properties = plan_schema["properties"].as_object().unwrap();
    for key in plan.as_object().unwrap().keys() {
        assert!(
            properties.contains_key(key),
            "{key} is not in the published Plan schema"
        );
    }
    for required in plan_schema["required"].as_array().unwrap() {
        assert!(
            plan.get(required.as_str().unwrap()).is_some(),
            "{required} missing"
        );
    }
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
