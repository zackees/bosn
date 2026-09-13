use std::{path::Path, process::Command};

use serde_json::Value;

fn run(file: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bosn"))
        // This command must remain independent from Docker even if the
        // environment normally provides an engine endpoint.
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .args(["compose", "plan", "--file"])
        .arg(file)
        .arg("--json")
        .output()
        .unwrap()
}

#[test]
fn compose_plan_reads_only_the_explicit_file_and_returns_an_inert_receipt() {
    let root = tempfile::tempdir().unwrap();
    let compose = root.path().join("compose.yaml");
    std::fs::write(&compose, "services:\n  api:\n    image: alpine:3.21\n").unwrap();

    let output = run(&compose);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["action"], "compose_plan");
    assert_eq!(receipt["applied"], false);
    assert_eq!(receipt["version"], 1);
    assert_eq!(
        receipt["document"]["services"]["api"]["image"],
        "alpine:3.21"
    );
    assert!(receipt["digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        1,
        "planning did not create state or side files"
    );
}

#[test]
fn compose_plan_rejects_a_missing_or_oversized_file_without_output() {
    let root = tempfile::tempdir().unwrap();
    let missing = run(&root.path().join("missing.yaml"));
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());

    let oversized = root.path().join("oversized.yaml");
    std::fs::write(&oversized, vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let oversized = run(&oversized);
    assert_eq!(oversized.status.code(), Some(1));
    assert!(oversized.stdout.is_empty());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("byte limit"));
}
