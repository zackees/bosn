//! `bosn gc --unmanaged --json` and `bosn scan --json` on an incomplete census (#316).
//!
//! A stand-in `docker` on `PATH` stalls on every read, so the census can only finish by
//! hitting the configured `--census-deadline-ms`. Nothing here touches a real engine.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Put a `docker` that never answers (a shell-builtin loop, so no `sleep` binary is needed) on a fresh `PATH` directory.
fn stalling_docker(dir: &Path) {
    let path = dir.join("docker");
    std::fs::write(&path, "#!/bin/sh\nwhile :; do :; done\n").expect("write fake docker");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn run(dir: &Path, args: &[&str]) -> (Output, Duration) {
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(args)
        .arg("--state-dir")
        .arg(dir.join("state"))
        .env("PATH", dir)
        .output()
        .expect("run bosn");
    (output, started.elapsed())
}

#[test]
fn gc_unmanaged_json_reports_an_incomplete_census_and_exits_non_zero() {
    let dir = tempfile::tempdir().expect("temp dir");
    stalling_docker(dir.path());
    let (output, elapsed) = run(
        dir.path(),
        &["gc", "--unmanaged", "--json", "--census-deadline-ms", "300"],
    );
    assert!(
        !output.status.success(),
        "an incomplete census exits non-zero"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the configured deadline, not the 30 s default, bounds the read: {elapsed:?}"
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    assert_eq!(document["action"], "gc_unmanaged_preview");
    assert_eq!(document["partial"], true);
    assert_eq!(document["apply_available"], false);
    assert_eq!(document["census_deadline_ms"], 300);
    assert_eq!(document["candidates"], serde_json::json!([]));
    let unreadable = document["unreadable"].as_array().expect("unreadable list");
    assert!(
        unreadable.iter().any(|detail| detail
            .as_str()
            .is_some_and(|text| text.contains("system df") && text.contains("deadline"))),
        "the reason names the read that failed: {unreadable:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("300 ms"),
        "the human message names the deadline: {stderr}"
    );
}

#[test]
fn scan_json_uses_the_same_partial_fields() {
    let dir = tempfile::tempdir().expect("temp dir");
    stalling_docker(dir.path());
    let (output, elapsed) = run(
        dir.path(),
        &["scan", "--json", "--census-deadline-ms", "300"],
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "deadline honoured: {elapsed:?}"
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    assert_eq!(document["partial"], true);
    assert!(
        document["unreadable"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
    );
}

#[test]
fn a_zero_or_malformed_deadline_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    stalling_docker(dir.path());
    for value in ["0", "soon"] {
        let (output, _) = run(
            dir.path(),
            &["gc", "--unmanaged", "--json", "--census-deadline-ms", value],
        );
        assert!(!output.status.success(), "{value} is refused");
        assert!(output.stdout.is_empty(), "a usage error writes no document");
    }
}
