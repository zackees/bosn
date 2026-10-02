//! Opt-in, live-Docker proof for #335: `actions/checkout` with a `ref:` is
//! served from the run's frozen snapshot (uncommitted work included), and a
//! checkout of another repository fails loudly instead of testing the wrong
//! code. Run with `cargo test -p bosn-service --test ci_checkout_live -- --ignored`.

use std::{path::Path, process::Command};

use serde_json::Value;

const WORKFLOW: &str = r#"on: [push]
jobs:
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

#[test]
#[ignore = "needs Docker"]
fn checkout_with_a_ref_sees_uncommitted_work_and_other_repos_fail_loudly() {
    let root = short_temp();
    let repo = root.path().join("repo");
    let state = root.path().join("s");
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
    std::fs::write(repo.join("marker.txt"), "uncommitted-edit\n").unwrap();
    let run = |job: &str| -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["ci", "run", "--wait", "--json", "--job", job, "--state-dir"])
            .arg(&state)
            .current_dir(&repo)
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)))
    };
    let own = run("own");
    let other = run("other");
    let _ = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["daemon", "stop", "--state-dir"])
        .arg(&state)
        .status();
    assert_eq!(own["conclusion"], "success", "{own}");
    assert!(
        own["dirty"].is_string(),
        "the run used the uncommitted tree"
    );
    assert_eq!(other["conclusion"], "failure", "{other}");
}
