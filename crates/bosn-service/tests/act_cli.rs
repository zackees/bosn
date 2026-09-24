#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, process::Command};

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
    fs::write(&path, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'act version 0.2.88'; exit 0; fi\nif [ \"$1\" = -l ] && [ \"$2\" = -W ]; then if [ \"$BOSN_FAKE_ACT_MODE\" = hang ]; then sleep 4; fi; if [ \"$BOSN_FAKE_ACT_MODE\" = closehang ]; then exec 1>&- 2>&-; sleep 4; fi; if [ \"$BOSN_FAKE_ACT_MODE\" = flood ]; then head -c 2097152 /dev/zero; exit 0; fi; if [ \"$BOSN_FAKE_ACT_MODE\" = dirty ]; then printf changed > src.rs; fi; printf 'Stage  Job ID       Job name       Workflow name  Workflow file  Events\\n0      lint         Lint           CI             ci.yml         push\\n1      build-linux  Linux build    CI             ci.yml         push\\n'; exit 0; fi\nexit 7\n").unwrap();
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
fn act_plan_uses_workspace_for_both_relative_binary_queries() {
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
        .env("BOSN_FAKE_ACT_MODE", "dirty")
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
        (sha.as_str(), true, "workflow differs"),
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
    for mode in ["hang", "closehang"] {
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
            .env("BOSN_FAKE_ACT_MODE", mode)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    }
}

#[cfg(unix)]
#[test]
fn act_plan_rejects_oversized_list_output() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("ci.yml"), "name: CI\non: [push]\n").unwrap();
    let sha = commit_workflow(root.path(), "ci.yml");
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
        .env("BOSN_FAKE_ACT_MODE", "flood")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("output limit"));
    assert!(output.stdout.is_empty());
}
