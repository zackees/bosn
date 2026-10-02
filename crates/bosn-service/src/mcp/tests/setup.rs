//! Setup plan/prepare/ensure/task tools: semantic inputs only, redacted errors.

use super::*;

#[test]
fn setup_plan_tool_is_read_only_and_uses_only_server_selected_state() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_plan","arguments":{"workspace":"/workspace","config":"/configs/setup.toml","policy":"refresh"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(replies[1]["result"]["structuredContent"]["applied"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"]["source_kind"],
        "local_file"
    );
    assert_eq!(backend.daemon_reads, 0);
    assert!(backend.cancelled.is_empty());
    assert_eq!(backend.setup_calls.len(), 1);
    assert_eq!(backend.setup_calls[0].0, PathBuf::from("/workspace"));
    assert_eq!(backend.setup_calls[0].1, "/configs/setup.toml");
    assert_eq!(backend.setup_calls[0].2, SetupAcquirePolicy::OnlineRefresh);
}

#[test]
fn setup_plan_rejects_malformed_or_ambiguous_arguments_before_execution() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_plan","arguments":{"workspace":"/workspace","config":"/configs/setup.toml"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_plan","arguments":{"workspace":"/workspace","config":"/configs/setup.toml","policy":"refresh","state_dir":"/attacker-selected"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_setup_plan","arguments":{"workspace":"/workspace","config":"/configs/setup.toml","policy":"refresh,offline"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 4);
    assert!(replies[1]["result"]["isError"].as_bool().unwrap());
    assert!(replies[2]["result"]["isError"].as_bool().unwrap());
    assert!(replies[3]["result"]["isError"].as_bool().unwrap());
    assert!(backend.setup_calls.is_empty());
    assert_eq!(backend.daemon_reads, 0);
}

#[test]
fn setup_prepare_submits_all_immutable_semantic_inputs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"https://configs.example/setup.toml","policy":"offline","deadline_ms":1234,"output_limit":7654321}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"],
        json!({
            "action": "setup_prepare", "submitted": true, "job_id": 42,
        })
    );
    assert_eq!(backend.setup_prepare_calls.len(), 1);
    let request = &backend.setup_prepare_calls[0];
    assert_eq!(request.workspace, PathBuf::from("/workspace"));
    assert_eq!(request.config, "https://configs.example/setup.toml");
    assert_eq!(request.policy, SetupPreparePolicy::Offline);
    assert_eq!(request.deadline, std::time::Duration::from_millis(1234));
    assert_eq!(request.output_limit, 7_654_321);
}

#[test]
fn setup_prepare_rejects_invalid_or_extra_arguments_without_submission() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":0,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":300001,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":8388609}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1,"docker_argv":["rm","-rf","/"]}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 6);
    assert!(
        replies[1..]
            .iter()
            .all(|reply| reply["result"]["isError"] == true)
    );
    assert!(backend.setup_prepare_calls.is_empty());
}

#[test]
fn setup_prepare_daemon_errors_are_redacted() {
    let mut backend = FakeBackend {
        setup_prepare_error: true,
        ..Default::default()
    };
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
        ),
        &mut backend,
    );
    let content = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(content, "native Bosn daemon request failed");
    assert!(!content.contains("secret"));
}

#[test]
fn setup_ensure_submits_all_immutable_semantic_inputs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"https://configs.example/setup.toml","policy":"offline","deadline_ms":1234,"output_limit":7654321}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"],
        json!({
            "action": "setup_ensure", "submitted": true, "job_id": 44,
        })
    );
    assert_eq!(backend.setup_ensure_calls.len(), 1);
    let request = &backend.setup_ensure_calls[0];
    assert_eq!(request.workspace, PathBuf::from("/workspace"));
    assert_eq!(request.config, "https://configs.example/setup.toml");
    assert_eq!(request.policy, SetupPreparePolicy::Offline);
    assert_eq!(request.deadline, std::time::Duration::from_millis(1234));
    assert_eq!(request.output_limit, 7_654_321);
}

#[test]
fn setup_ensure_rejects_malformed_extra_or_out_of_range_arguments_without_submission() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"other","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":0,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":300001,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":8388609}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1,"container":"attacker-selected"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 8);
    assert!(
        replies[1..]
            .iter()
            .all(|reply| reply["result"]["isError"] == true)
    );
    assert!(backend.setup_ensure_calls.is_empty());
}

#[test]
fn setup_ensure_daemon_errors_are_redacted() {
    let mut backend = FakeBackend {
        setup_ensure_error: true,
        ..Default::default()
    };
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
        ),
        &mut backend,
    );
    let content = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(content, "native Bosn daemon request failed");
    assert!(!content.contains("secret"));
}

#[test]
fn setup_task_submits_every_immutable_input_and_declared_task_name() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"https://configs.example/setup.toml","policy":"offline","task_name":"check-build_2","deadline_ms":1234,"output_limit":7654321}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"],
        json!({
            "action": "setup_task", "submitted": true, "job_id": 43,
        })
    );
    assert_eq!(backend.setup_task_calls.len(), 1);
    let request = &backend.setup_task_calls[0];
    assert_eq!(request.workspace, PathBuf::from("/workspace"));
    assert_eq!(request.config, "https://configs.example/setup.toml");
    assert_eq!(request.policy, SetupPreparePolicy::Offline);
    assert_eq!(request.task_name, "check-build_2");
    assert_eq!(request.deadline, std::time::Duration::from_millis(1234));
    assert_eq!(request.output_limit, 7_654_321);
}

#[test]
fn setup_app_task_submits_only_declared_semantic_inputs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_app_task","arguments":{"workspace":"/workspace","config":"https://configs.example/setup.toml","policy":"offline","task_name":"check-build_2","deadline_ms":1234,"output_limit":7654321}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"],
        json!({"action": "setup_app_task", "submitted": true, "job_id": 45})
    );
    assert_eq!(backend.setup_app_task_calls.len(), 1);
    let request = &backend.setup_app_task_calls[0];
    assert_eq!(request.workspace, PathBuf::from("/workspace"));
    assert_eq!(request.config, "https://configs.example/setup.toml");
    assert_eq!(request.policy, SetupPreparePolicy::Offline);
    assert_eq!(request.task_name, "check-build_2");
    assert_eq!(request.deadline, std::time::Duration::from_millis(1234));
    assert_eq!(request.output_limit, 7_654_321);
}

#[test]
fn setup_task_rejects_malformed_extra_or_out_of_range_arguments_without_submission() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"-check","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check/run","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":0,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":300001,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":1,"output_limit":8388609}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":1,"output_limit":1,"command":"rm -rf /"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 8);
    assert!(
        replies[1..]
            .iter()
            .all(|reply| reply["result"]["isError"] == true)
    );
    assert!(backend.setup_task_calls.is_empty());
}

#[test]
fn setup_task_daemon_errors_are_redacted() {
    let mut backend = FakeBackend {
        setup_task_error: true,
        ..Default::default()
    };
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
        ),
        &mut backend,
    );
    let content = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(content, "native Bosn daemon request failed");
    assert!(!content.contains("secret"));
}

fn pinned_document() -> String {
    format!(
        "version = 1\n[app]\nimage = 'registry.example/demo@sha256:{}'\n[task.check]\ncommand = 'echo check'\n",
        "a".repeat(64)
    )
}

fn inline_document() -> &'static str {
    "version = 1\n[app]\ndockerfile = 'FROM scratch'\n[task.check]\ncommand = 'echo check'\n[[file]]\npath = 'check.sh'\ncontent = \"#!/bin/sh\\necho check\\n\"\n"
}

fn live_setup_plan(
    state_dir: &std::path::Path,
    workspace: &std::path::Path,
    config: &std::path::Path,
    policy: &str,
) -> Value {
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = Client::for_state(state_dir).unwrap();
    let mut backend = DaemonBackend {
        runtime: &runtime,
        client,
        state_dir: state_dir.to_path_buf(),
    };
    let input = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "bosn_setup_plan", "arguments": {
            "workspace": workspace,
            "config": config,
            "policy": policy,
        }},
    }))
    .unwrap();
    let replies = exchange(
        &format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}}\n{input}\n"),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    replies[1]["result"]["structuredContent"].clone()
}

#[test]
fn setup_plan_mcp_local_pinned_and_offline_reuse_never_start_daemon() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let config = config_root.path().join("setup.toml");
    std::fs::write(&config, pinned_document()).unwrap();

    let online = live_setup_plan(state.path(), workspace.path(), &config, "refresh");
    assert_eq!(online["action"], "plan");
    assert_eq!(online["applied"], false);
    assert_eq!(online["source_kind"], "local_file");
    assert_eq!(online["asset_root"], Value::Null);
    assert_eq!(online["task_names"], json!(["check"]));
    assert_eq!(online["app_source"]["kind"], "pinned_image");
    assert!(!state.path().join("registry.sqlite3").exists());

    std::fs::remove_file(&config).unwrap();
    let offline = live_setup_plan(state.path(), workspace.path(), &config, "offline");
    assert_eq!(offline, online);
    assert!(!state.path().join("registry.sqlite3").exists());
}

#[test]
fn setup_plan_mcp_inline_materializes_only_under_server_state() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let config = config_root.path().join("setup.toml");
    std::fs::write(&config, inline_document()).unwrap();

    let plan = live_setup_plan(state.path(), workspace.path(), &config, "refresh");
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["app_source"]["kind"], "inline_dockerfile");
    let asset_root = std::path::Path::new(plan["asset_root"].as_str().unwrap());
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
    assert!(!state.path().join("registry.sqlite3").exists());
}
