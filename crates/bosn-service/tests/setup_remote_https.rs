//! Black-box acceptance for a remote, one-file setup document.
//!
//! This exercises the normal kernel-backed HTTPS transport through the native
//! Bosn CLI. No test transport, raw HTTP client, or Docker control is used.

mod support;

use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Output},
};

use kernal_api::hash::sha256_bytes;
use serde_json::Value;
use support::tls_setup_server::{TlsSetupServer, certificate_path};

const TEST_SECRET: &str = "bosn-remote-test-secret-must-not-persist";
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn remote_document(task: &str) -> String {
    format!(
        "version = 1\n[app]\nimage = 'registry.example/remote@sha256:{DIGEST}'\n[task.{task}]\ncommand = 'echo {task}'\n"
    )
}

fn run_bosn(certificate: &Path, args: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bosn"))
        // Trust is passed only to this child. Bosn constructs its ordinary
        // verified kernal-api client; no Bosn API can weaken TLS validation.
        .env("SSL_CERT_FILE", certificate)
        .env("NO_PROXY", "localhost,127.0.0.1")
        // Setup plan must remain engine-inert. A Docker contact would turn
        // this deterministic URL/cache test into a host-dependent test.
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .args(args)
        .output()
        .expect("run native Bosn CLI")
}

fn plan_args<'a>(
    state: &'a Path,
    workspace: &'a Path,
    locator: &'a str,
    policy: &'a str,
) -> Vec<&'a OsStr> {
    vec![
        "setup".as_ref(),
        "plan".as_ref(),
        "--state-dir".as_ref(),
        state.as_os_str(),
        "--workspace".as_ref(),
        workspace.as_os_str(),
        "--config".as_ref(),
        locator.as_ref(),
        policy.as_ref(),
        "--json".as_ref(),
    ]
}

fn text(output: &Output) -> String {
    format!(
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn state_text(path: &Path) -> String {
    let mut output = String::new();
    for entry in std::fs::read_dir(path).expect("read Bosn state") {
        let entry = entry.expect("read state entry");
        let entry_path = entry.path();
        if entry.file_type().expect("read state entry type").is_dir() {
            output.push_str(&state_text(&entry_path));
        } else {
            output.push_str(&String::from_utf8_lossy(
                &std::fs::read(&entry_path).expect("read state file"),
            ));
        }
    }
    output
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test setup_remote_https --locked`
///
/// The public fixture identity is local test data, trusted only by the native
/// CLI children. The server never reaches Docker; `setup plan` is the highest
/// production path that can prove resolver/cache behavior without an opt-in
/// Docker daemon. The companion ignored live-Docker test proves remote daemon
/// ensure separately.
#[test]
fn production_https_plan_records_redacted_provenance_and_reuses_cache_offline() {
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("create selected workspace");
    let original = remote_document("original");
    let changed = remote_document("changed");
    let mut server = TlsSetupServer::start(original.as_bytes());
    let locator = server.url(&format!("/setup.toml?token={TEST_SECRET}"));
    let certificate = certificate_path();

    let online = run_bosn(
        &certificate,
        &plan_args(&state, &workspace, &locator, "--refresh"),
    );
    assert!(
        online.status.success(),
        "online plan failed: {}",
        text(&online)
    );
    let online_json: Value = serde_json::from_slice(&online.stdout).expect("online plan JSON");
    assert_eq!(online_json["action"], "plan");
    assert_eq!(online_json["applied"], false);
    assert_eq!(online_json["source_kind"], "https");
    assert_eq!(
        online_json["content_sha256"],
        sha256_bytes(original.as_bytes()).to_hex(),
        "receipt must identify the exact fetched remote bytes"
    );
    assert_eq!(online_json["task_names"], serde_json::json!(["original"]));
    assert_eq!(server.request_count(), 1, "refresh made one HTTPS request");
    assert!(
        !text(&online).contains(TEST_SECRET),
        "request credential leaked through CLI receipt or diagnostics"
    );

    // A changed remote representation must not be discovered, fetched, or
    // applied by an explicit offline operation.
    server.replace_body(changed.as_bytes());
    let offline_while_server_is_live = run_bosn(
        &certificate,
        &plan_args(&state, &workspace, &locator, "--offline"),
    );
    assert!(
        offline_while_server_is_live.status.success(),
        "offline plan failed: {}",
        text(&offline_while_server_is_live)
    );
    let offline_while_server_is_live: Value =
        serde_json::from_slice(&offline_while_server_is_live.stdout).expect("offline plan JSON");
    assert_eq!(offline_while_server_is_live, online_json);
    assert_eq!(
        server.request_count(),
        1,
        "offline policy contacted a changed remote source"
    );

    // Stop the only source, then prove the verified durable cache still owns
    // the same receipt. This also prevents an accidental network fallback from
    // looking like a passing test.
    server.stop();
    let offline_after_stop = run_bosn(
        &certificate,
        &plan_args(&state, &workspace, &locator, "--offline"),
    );
    assert!(
        offline_after_stop.status.success(),
        "offline cached plan failed after server stop: {}",
        text(&offline_after_stop)
    );
    let offline_after_stop: Value =
        serde_json::from_slice(&offline_after_stop.stdout).expect("offline cached plan JSON");
    assert_eq!(offline_after_stop, online_json);

    let persisted = state_text(&state);
    let redacted_locator = locator.replace(TEST_SECRET, "[redacted]");
    assert!(
        persisted.matches(&redacted_locator).count() >= 2,
        "cache must retain both redacted requested and resolved HTTPS provenance"
    );
    assert!(
        !persisted.contains(TEST_SECRET),
        "remote credential leaked into durable state"
    );
    assert!(
        std::fs::read_dir(&workspace)
            .expect("read selected workspace")
            .next()
            .is_none(),
        "remote setup plan wrote into selected workspace"
    );

    // Userinfo is refused before any transport operation, and even the
    // validation failure must never reflect a caller credential.
    let credentialed = format!("https://user:{TEST_SECRET}@localhost:1/setup.toml");
    let rejected = run_bosn(
        &certificate,
        &plan_args(&state, &workspace, &credentialed, "--refresh"),
    );
    assert!(
        !rejected.status.success(),
        "credential-bearing locator unexpectedly accepted"
    );
    assert!(
        !text(&rejected).contains(TEST_SECRET),
        "credential leaked through rejected setup diagnostics"
    );
}
