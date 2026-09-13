use std::{path::Path, process::Command};

use serde_json::Value;

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn run(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bosn"))
        // The plan command must not consult or require Docker.  A deliberately
        // unusable endpoint makes an accidental engine invocation fail loudly.
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .args(args)
        .output()
        .unwrap()
}

fn pinned_document() -> String {
    format!(
        "version = 1\n[app]\nimage = 'registry.example/demo@sha256:{DIGEST}'\n[task.check]\ncommand = 'echo check'\n"
    )
}

fn inline_document() -> &'static str {
    "version = 1\n[app]\ndockerfile = 'FROM scratch'\n[task.check]\ncommand = 'echo check'\n[[file]]\npath = 'check.sh'\ncontent = \"#!/bin/sh\\necho check\\n\"\n"
}

fn plan_args<'a>(
    state: &'a Path,
    workspace: &'a Path,
    config: &'a Path,
    policy: &'a str,
) -> Vec<&'a std::ffi::OsStr> {
    vec![
        "setup".as_ref(),
        "plan".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--workspace".as_ref(),
        workspace.as_os_str(),
        "--config".as_ref(),
        config.as_os_str(),
        policy.as_ref(),
        "--json".as_ref(),
    ]
}

#[test]
fn pinned_plan_json_is_not_applied_and_offline_reuses_the_verified_local_receipt() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let config = config_root.path().join("setup.toml");
    std::fs::write(&config, pinned_document()).unwrap();

    let online = run(&plan_args(
        state.path(),
        workspace.path(),
        &config,
        "--refresh",
    ));
    assert!(
        online.status.success(),
        "{}",
        String::from_utf8_lossy(&online.stderr)
    );
    let online: Value = serde_json::from_slice(&online.stdout).unwrap();
    assert_eq!(online["action"], "plan");
    assert_eq!(online["applied"], false);
    assert_eq!(online["source_kind"], "local_file");
    assert_eq!(online["schema_version"], 1);
    assert_eq!(online["asset_root"], Value::Null);
    assert_eq!(online["task_names"], serde_json::json!(["check"]));
    assert_eq!(online["app_source"]["kind"], "pinned_image");
    assert_eq!(
        online["workspace"],
        std::fs::canonicalize(workspace.path())
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );

    std::fs::remove_file(&config).unwrap();
    let offline = run(&plan_args(
        state.path(),
        workspace.path(),
        &config,
        "--offline",
    ));
    assert!(
        offline.status.success(),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    let offline: Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(offline, online);
}

#[test]
fn inline_plan_json_reports_private_assets_without_writing_the_workspace() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let config = config_root.path().join("setup.toml");
    std::fs::write(&config, inline_document()).unwrap();

    let output = run(&plan_args(
        state.path(),
        workspace.path(),
        &config,
        "--refresh",
    ));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["applied"], false);
    assert_eq!(value["app_source"]["kind"], "inline_dockerfile");
    let asset_root = Path::new(value["asset_root"].as_str().unwrap());
    assert!(asset_root.starts_with(state.path()));
    assert!(!asset_root.starts_with(workspace.path()));
    assert_eq!(
        std::fs::read_to_string(asset_root.join("Dockerfile")).unwrap(),
        "FROM scratch"
    );
    assert!(
        std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn plan_rejects_ambiguous_policy_before_creating_state() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("must-not-exist");
    let workspace = tempfile::tempdir().unwrap();
    let config = root.path().join("setup.toml");
    std::fs::write(&config, pinned_document()).unwrap();
    let output = run(&[
        "setup".as_ref(),
        "plan".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--workspace".as_ref(),
        workspace.path().as_os_str(),
        "--config".as_ref(),
        config.as_os_str(),
        "--refresh".as_ref(),
        "--offline".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!state.exists());
}

#[test]
fn prepare_rejects_malformed_or_ambiguous_inputs_before_state_or_daemon_contact() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = root.path().join("setup.toml");
    std::fs::write(&config, pinned_document()).unwrap();

    for (name, extra) in [
        ("ambiguous-policy", vec!["--refresh", "--offline"]),
        ("missing-deadline", vec!["--refresh"]),
        ("zero-deadline", vec!["--refresh", "--deadline-ms", "0"]),
        (
            "oversize-output",
            vec![
                "--refresh",
                "--deadline-ms",
                "1",
                "--output-limit",
                "8388609",
            ],
        ),
        (
            "duplicate-json",
            vec![
                "--refresh",
                "--deadline-ms",
                "1",
                "--output-limit",
                "1",
                "--json",
                "--json",
            ],
        ),
    ] {
        let state = root.path().join(name);
        let mut args: Vec<&std::ffi::OsStr> = vec![
            "setup".as_ref(),
            "prepare".as_ref(),
            "--state-dir".as_ref(),
            state.as_os_str(),
            "--workspace".as_ref(),
            workspace.path().as_os_str(),
            "--config".as_ref(),
            config.as_os_str(),
        ];
        args.extend(extra.into_iter().map(std::ffi::OsStr::new));
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(output.stdout.is_empty(), "{name}");
        assert!(!state.exists(), "{name}");
    }
}
