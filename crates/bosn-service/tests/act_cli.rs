#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, process::Command};

#[test]
fn bosn_payload_maps_literal_ci_labels_and_release_dispatch() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    for (event, mode, expected_event, expected_label) in [
        ("pull_request", "minimal", "pull_request", None),
        ("pull_request", "test", "pull_request", Some("ci-test")),
        ("pull_request", "full", "pull_request", Some("ci-full")),
        ("release", "full", "workflow_dispatch", None),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bosn"));
        command.args([
            "act", "payload", "--event", event, "--mode", mode, "--sha", sha,
        ]);
        if event == "pull_request" {
            command.args(["--pr-number", "17"]);
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value["github_event"], expected_event);
        assert_eq!(value["payload"]["repository"]["full_name"], "zackees/bosn");
        assert_eq!(value["executable"], false);
        if event == "release" {
            assert_eq!(value["payload"]["inputs"]["tier"], "full");
            assert_eq!(value["payload"]["inputs"]["commit_sha"], sha);
        } else {
            assert_eq!(value["payload"]["pull_request"]["head"]["sha"], sha);
            let labels = value["payload"]["pull_request"]["labels"]
                .as_array()
                .unwrap();
            assert_eq!(labels.len(), usize::from(expected_label.is_some()));
            if let Some(label) = expected_label {
                assert_eq!(labels[0]["name"], label);
            }
        }
    }
}

#[test]
fn bosn_payload_rejects_mismatched_events_and_missing_pr_identity() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    for args in [
        vec!["--event", "push", "--mode", "full", "--sha", sha],
        vec!["--event", "release", "--mode", "test", "--sha", sha],
        vec!["--event", "pull_request", "--mode", "test", "--sha", sha],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["act", "payload"])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
    }
}

fn commit_workflow(root: &std::path::Path, workflow: &str) -> String {
    assert!(root.join(workflow).is_file());
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(init.status.success());
    let add = Command::new("git")
        .args(["add", "--all"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(add.status.success());
    let commit = Command::new("git")
        .args([
            "-c",
            "user.name=Bosn Test",
            "-c",
            "user.email=bosn@example.invalid",
            "commit",
            "-q",
            "-m",
            "workflow",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(head.status.success());
    String::from_utf8(head.stdout).unwrap().trim().to_owned()
}

#[cfg(unix)]
fn fake_act(root: &std::path::Path) -> std::path::PathBuf {
    let path = root.join(".git/act-fake");
    fs::write(&path, r#"#!/bin/sh
printf '%s\n' "$PWD" > "$(dirname "$0")/act-fake-control"
if [ "$1" = --version ]; then echo 'act version 0.2.88'; exit 0; fi
mode=''
if [ -f "$(dirname "$0")/act-fake-mode" ]; then mode=$(cat "$(dirname "$0")/act-fake-mode"); fi
if [ "$1" = -l ] && [ "$2" = -C ] && [ "$4" = -W ]; then
  if [ "$mode" = hang ]; then sleep 4; fi
  if [ "$mode" = closehang ]; then exec 1>&- 2>&-; sleep 4; fi
  if [ "$mode" = flood ]; then head -c 2097152 /dev/zero; exit 0; fi
  if [ "$mode" = dirty ]; then printf changed > "$3/src.rs"; fi
  if [ "$mode" = fail ]; then exit 7; fi
  printf 'Stage  Job ID       Job name       Workflow name  Workflow file  Events\n0      lint         Lint           CI             ci.yml         push\n1      build-linux  Linux build    CI             ci.yml         push\n'; exit 0
fi
exit 7
"#).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn act_plan_is_read_only_and_emits_selected_inputs() {
    let root = tempfile::tempdir().unwrap();
    let workflow = root.path().join(".github/workflows/ci.yml");
    fs::create_dir_all(workflow.parent().unwrap()).unwrap();
    fs::write(&workflow, "name: CI\non: [push]\njobs: {}\n").unwrap();
    let sha = commit_workflow(root.path(), ".github/workflows/ci.yml");
    let act = fake_act(root.path());
    let result = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(root.path())
        .args([
            "--workflow",
            ".github/workflows/ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
        ])
        .arg(&act)
        .args(["--json"])
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["action"], "act_plan");
    assert_eq!(value["event"], "push");
    assert_eq!(value["act_version"], "0.2.88");
    assert_eq!(value["act_version_source"], "caller");
    assert_eq!(value["fleet_pin_verified"], false);
    assert_eq!(value["event_payload_resolved"], false);
    assert_eq!(value["executable"], false);
    assert_eq!(value["docker_resources_tracked"], false);
}

#[test]
fn act_run_refuses_untracked_docker_execution() {
    let result = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "run"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("isolated Docker ownership"));
}

#[test]
fn act_plan_rejects_release_without_full_mode_and_path_traversal() {
    let root = tempfile::tempdir().unwrap();
    for (event, mode, workflow) in [
        ("release", "minimal", "ci.yml"),
        ("push", "minimal", "../ci.yml"),
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["act", "plan", "--workspace"])
            .arg(root.path())
            .args([
                "--workflow",
                workflow,
                "--event",
                event,
                "--mode",
                mode,
                "--sha",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--act-version",
                "0.2.88",
                "--json",
            ])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn act_plan_lists_selected_jobs_from_pinned_act() {
    let root = tempfile::tempdir().unwrap();
    let workflow = root.path().join("ci.yml");
    fs::write(&workflow, "name: CI\non: [push]\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    let act = fake_act(root.path());
    let result = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(root.path())
        .args([
            "--workflow",
            "ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
        ])
        .arg(&act)
        .args(["--job", "build-linux", "--json"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["jobs"].as_array().unwrap().len(), 2);
    assert_eq!(value["selected_jobs"][0]["id"], "build-linux");
    assert_eq!(value["selected_jobs"][0]["stage"], 1);
}

#[cfg(unix)]
#[test]
fn act_plan_resolves_workspace_relative_binary_before_sterile_queries() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    fake_act(root.path());
    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(root.path())
        .args([
            "--workflow",
            "ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
            "./.git/act-fake",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn act_plan_rejects_hidden_index_changes() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    fs::write(root.path().join("src.rs"), "original\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    let act = fake_act(root.path());
    let flag = Command::new("git")
        .current_dir(root.path())
        .args(["update-index", "--assume-unchanged", "src.rs"])
        .output()
        .unwrap();
    assert!(flag.status.success());
    fs::write(root.path().join("src.rs"), "changed\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(root.path())
        .args([
            "--workflow",
            "ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
        ])
        .arg(&act)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("index hides"));
}

#[cfg(unix)]
#[test]
fn act_plan_rejects_source_changed_during_act_query() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    fs::write(root.path().join("src.rs"), "original\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    let act = fake_act(root.path());
    fs::write(root.path().join(".git/act-fake-mode"), "dirty").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(root.path())
        .args([
            "--workflow",
            "ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
        ])
        .arg(&act)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("changed during planning"));
}

#[cfg(unix)]
#[test]
fn act_plan_rejects_wrong_binary_version_and_missing_job() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    let act = fake_act(root.path());
    for (version, job, expected) in [
        ("0.2.87", "lint", "does not match"),
        ("0.2.88", "absent", "does not exist"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["act", "plan", "--workspace"])
            .arg(root.path())
            .args([
                "--workflow",
                "ci.yml",
                "--event",
                "push",
                "--mode",
                "minimal",
                "--sha",
                &sha,
                "--act-version",
                version,
                "--act-bin",
            ])
            .arg(&act)
            .args(["--job", job, "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
        assert!(output.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn act_plan_refuses_mismatched_sha_and_dirty_workflow() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    let act = fake_act(root.path());
    for (requested_sha, dirty, expected) in [
        ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false, "HEAD"),
        (sha.as_str(), true, "source differs"),
    ] {
        if dirty {
            fs::write(root.path().join("ci.yml"), "name: Altered\non: [push]\n").unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["act", "plan", "--workspace"])
            .arg(root.path())
            .args([
                "--workflow",
                "ci.yml",
                "--event",
                "push",
                "--mode",
                "minimal",
                "--sha",
                requested_sha,
                "--act-version",
                "0.2.88",
                "--act-bin",
            ])
            .arg(&act)
            .args(["--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn act_plan_bounds_committed_workflow_and_git_observations() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), vec![b'#'; (1 << 20) + 1]).unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    let run = |path: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bosn"));
        command
            .args(["act", "plan", "--workspace"])
            .arg(root.path())
            .args([
                "--workflow",
                "ci.yml",
                "--event",
                "push",
                "--mode",
                "minimal",
                "--sha",
                &sha,
                "--act-version",
                "0.2.88",
                "--json",
            ]);
        if let Some(path) = path {
            command.env("PATH", path);
        }
        command.output().unwrap()
    };
    let oversized = run(None);
    assert_eq!(oversized.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("exceeds 1 MiB"));
    assert!(oversized.stdout.is_empty());

    let commands = tempfile::tempdir().unwrap();
    let git = commands.path().join("git");
    fs::write(&git, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
    let delayed = run(Some(commands.path()));
    assert_eq!(delayed.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&delayed.stderr).contains("timed out"));
    assert!(delayed.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn act_plan_refuses_dirty_non_workflow_source() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    fs::write(root.path().join("src.rs"), "pub fn clean() {}\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    fs::write(root.path().join("src.rs"), "pub fn dirty() {}\n").unwrap();
    let act = fake_act(root.path());
    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(root.path())
        .args([
            "--workflow",
            "ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
        ])
        .arg(&act)
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("workspace source differs"));
}

#[cfg(unix)]
#[test]
fn act_plan_requires_checkout_root_as_workspace() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    let sha = commit_workflow(root.path(), "nested/ci.yml");
    let act = fake_act(root.path());
    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(&nested)
        .args([
            "--workflow",
            "ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
        ])
        .arg(&act)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Git checkout root"));
}

#[cfg(unix)]
#[test]
fn act_plan_times_out_hung_list() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    let act = fake_act(root.path());
    for mode in ["hang", "closehang", "fail"] {
        fs::write(root.path().join(".git/act-fake-mode"), mode).unwrap();
        let started = std::time::Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .args(["act", "plan", "--workspace"])
            .arg(root.path())
            .args([
                "--workflow",
                "ci.yml",
                "--event",
                "push",
                "--mode",
                "minimal",
                "--sha",
                &sha,
                "--act-version",
                "0.2.88",
                "--act-bin",
            ])
            .arg(&act)
            .args(["--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(if mode == "fail" {
                "query failed"
            } else {
                "timed out"
            })
        );
        assert!(output.stdout.is_empty());
        let control = fs::read_to_string(root.path().join(".git/act-fake-control")).unwrap();
        assert!(!std::path::Path::new(control.trim()).exists());
    }
}

#[cfg(unix)]
#[test]
fn act_plan_rejects_oversized_list_output() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
    let act = fake_act(root.path());
    fs::write(root.path().join(".git/act-fake-mode"), "flood").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(root.path())
        .args([
            "--workflow",
            "ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
        ])
        .arg(&act)
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("output limit"));
    assert!(output.stdout.is_empty());
}

#[cfg(unix)]
fn check_sterile_act_queries(with_config: bool) {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    if with_config {
        fs::write(
            root.path().join(".actrc"),
            "--secret HOST_TOKEN\n--job attacker\n",
        )
        .unwrap();
    }
    let sha = commit_workflow(root.path(), "ci.yml");
    let host = tempfile::tempdir().unwrap();
    if with_config {
        fs::write(host.path().join(".actrc"), "--secret HOST_TOKEN\n").unwrap();
    }
    fs::create_dir(host.path().join("act")).unwrap();
    if with_config {
        fs::write(host.path().join("act/actrc"), "--secret HOST_TOKEN\n").unwrap();
    }
    let act = root.path().join(".git/sterile-act");
    fs::write(&act, r#"#!/bin/sh
printf '%s\n%s\n%s\n' "$PWD" "$HOME" "$XDG_CONFIG_HOME" >> "$(dirname "$0")/query-audit"
if [ -f .actrc ] || [ -f "$HOME/.actrc" ] || [ -f "$XDG_CONFIG_HOME/act/actrc" ]; then echo 'inherited actrc' >&2; exit 21; fi
if [ -n "$HOST_TOKEN$GITHUB_TOKEN$GH_TOKEN$AWS_SECRET_ACCESS_KEY$DOCKER_HOST$LD_PRELOAD$BOSN_FAKE_ACT_MODE" ]; then echo 'inherited credential or configuration' >&2; exit 22; fi
[ -d "$HOME" ] && [ -d "$XDG_CONFIG_HOME" ] || exit 23
[ "$PWD" != "$(dirname "$(dirname "$0")")" ] || exit 24
if [ "$1" = --version ]; then echo 'act version 0.2.88'; exit 0; fi
[ "$1" = -l ] && [ "$2" = -C ] && [ "$3" = "$(dirname "$(dirname "$0")")" ] && [ "$4" = -W ] && [ "$5" = "$3/ci.yml" ] || { echo 'wrong explicit source/workflow arguments' >&2; exit 25; }
printf 'Stage  Job ID  Job name\n0  lint  Lint\n'
"#).unwrap();
    fs::set_permissions(&act, fs::Permissions::from_mode(0o700)).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["act", "plan", "--workspace"])
        .arg(root.path())
        .args([
            "--workflow",
            "ci.yml",
            "--event",
            "push",
            "--mode",
            "minimal",
            "--sha",
            &sha,
            "--act-version",
            "0.2.88",
            "--act-bin",
        ])
        .arg(&act)
        .arg("--json")
        .env("HOME", host.path())
        .env("XDG_CONFIG_HOME", host.path())
        .env("HOST_TOKEN", "host secret")
        .env("GITHUB_TOKEN", "github secret")
        .env("GH_TOKEN", "gh secret")
        .env("AWS_SECRET_ACCESS_KEY", "cloud secret")
        .env("DOCKER_HOST", "tcp://secret.invalid:2375")
        .env("BOSN_FAKE_ACT_MODE", "ambient")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["jobs"][0]["id"], "lint");
    assert_eq!(value["executable"], false);
    let audit = fs::read_to_string(root.path().join(".git/query-audit")).unwrap();
    let paths: Vec<_> = audit.lines().collect();
    assert_eq!(paths.len(), 6);
    assert_ne!(paths[0], paths[3]);
    for path in paths {
        assert!(
            !std::path::Path::new(path).exists(),
            "query directory remains: {path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn act_plan_queries_ignore_source_and_host_actrc() {
    check_sterile_act_queries(true);
}
#[cfg(unix)]
#[test]
fn act_plan_queries_clear_arbitrary_host_credentials() {
    check_sterile_act_queries(false);
}
