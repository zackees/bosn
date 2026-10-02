//! Opt-in, live-Docker proof for #335: `actions/checkout` with a `ref:` is
//! served from the run's frozen snapshot (uncommitted work included), a
//! checkout of another public repository pinned to a commit needs no token,
//! and a checkout of another repository at a moving ref fails loudly instead
//! of testing the wrong code. A detached `HEAD` gives jobs a real repository
//! (#393), and a dirty tree is checked out as a synthetic commit, so a
//! workflow that cleans its tree still builds the uncommitted work (#394).
//! Run with `cargo test -p bosn-service --test ci_checkout_live -- --ignored`.

use std::{path::Path, process::Command};

use serde_json::Value;

const WORKFLOW: &str = r#"on: [push, pull_request]
jobs:
  head:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event_name == 'pull_request' && github.event.pull_request.head.sha || github.sha }}
      - env:
          SOURCE_REF: ${{ github.event_name == 'pull_request' && github.event.pull_request.head.sha || github.sha }}
        run: |
          set -eux
          test "$(git rev-parse HEAD)" = "$SOURCE_REF"
          test -z "$(git status --porcelain)"
          git restore --staged --worktree -- .
          grep -q uncommitted-edit marker.txt
          test -f untracked.txt
  own:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.sha }}
      - run: grep -q uncommitted-edit marker.txt
      - uses: actions/checkout@v4
        with:
          path: nested
      - run: grep -q uncommitted-edit nested/marker.txt
  pinned:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          repository: actions/checkout
          ref: 11bd71901bbe5b1630ceea73d27597364c9af683
          path: dep
      - run: test -f dep/action.yml
  other:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          repository: someone/elsewhere
"#;

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

fn short_temp() -> tempfile::TempDir {
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 40 {
        std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".cache")
    } else {
        base
    };
    tempfile::Builder::new()
        .prefix("bck")
        .tempdir_in(base)
        .unwrap()
}

/// A committed repository holding [`WORKFLOW`], with a private daemon.
struct Fixture {
    root: tempfile::TempDir,
    repo: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = short_temp();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".github/workflows")).unwrap();
        std::fs::write(repo.join(".github/workflows/ci.yml"), WORKFLOW).unwrap();
        std::fs::write(repo.join("marker.txt"), "committed\n").unwrap();
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
        Self { root, repo }
    }

    fn run(&self, job: &str, trigger: &str) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["ci", "run", "--wait", "--json", "--job", job])
            .args(["--trigger", trigger, "--state-dir"])
            .arg(self.root.path().join("s"))
            .current_dir(&self.repo)
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)))
    }

    fn head(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.repo)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn stop(&self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["daemon", "stop", "--state-dir"])
            .arg(self.root.path().join("s"))
            .status();
    }
}

#[test]
#[ignore = "needs Docker"]
fn a_detached_dirty_workspace_is_checked_out_as_a_commit_a_restore_keeps() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-q", "--detach"]);
    let base = fixture.head();
    std::fs::write(fixture.repo.join("marker.txt"), "uncommitted-edit\n").unwrap();
    std::fs::write(fixture.repo.join("untracked.txt"), "new\n").unwrap();
    let push = fixture.run("head", "push");
    let pr = fixture.run("head", "pr");
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "-qm", "edit"]);
    git(&fixture.repo, &["checkout", "-q", "--detach"]);
    let clean = fixture.run("head", "push");
    fixture.stop();
    for run in [&push, &pr] {
        assert_eq!(run["conclusion"], "success", "{run}");
        assert_eq!(run["sha"], base.as_str(), "the record keeps the real base");
        assert!(run["dirty"].is_string(), "{run}");
        let commit = run["commit"]
            .as_str()
            .expect("the synthetic commit is recorded");
        assert_ne!(commit, base);
    }
    assert_eq!(
        push["commit"], pr["commit"],
        "the same tree, the same commit"
    );
    assert_eq!(clean["conclusion"], "success", "{clean}");
    assert_eq!(clean["sha"], fixture.head().as_str());
    assert!(
        clean["dirty"].is_null() && clean["commit"].is_null(),
        "{clean}"
    );
}

#[test]
#[ignore = "needs Docker"]
fn checkout_with_a_ref_sees_uncommitted_work_pinned_repos_need_no_token_others_fail_loudly() {
    let fixture = Fixture::new();
    std::fs::write(fixture.repo.join("marker.txt"), "uncommitted-edit\n").unwrap();
    let own = fixture.run("own", "push");
    let pinned = fixture.run("pinned", "push");
    let other = fixture.run("other", "push");
    fixture.stop();
    assert_eq!(own["conclusion"], "success", "{own}");
    assert!(
        own["dirty"].is_string(),
        "the run used the uncommitted tree"
    );
    assert_eq!(pinned["conclusion"], "success", "{pinned}");
    assert_eq!(other["conclusion"], "failure", "{other}");
}
