//! Bounded Model Context Protocol server for the native Bosn control plane.
//!
//! This module deliberately implements the small, stable JSON-RPC-over-stdio
//! subset required for MCP tools.  The official Rust SDK (`rmcp`) owns a Tokio
//! runtime and Tokio stdio transport.  Bosn must instead keep async runtime
//! ownership in `kernal-api`, so importing `rmcp` would create a second runtime
//! at the product boundary.  A future kernal-api adapter can replace this
//! module; until then its intentionally narrow implementation keeps the
//! externally required JSON-RPC boundary separate from Bosn's authenticated
//! local protobuf protocol.
//!
//! Standard output is exclusively newline-delimited JSON-RPC messages.  This
//! is important for Hermes and other stdio clients: all human diagnostics stay
//! with the command launcher on stderr.  Requests, output, log pages, and
//! numeric arguments are bounded before they reach the native daemon.

use crate::{
    Client, DoctorReport, Error, JobLogPage, JobStatus, MANIFEST_MAX_DEADLINE, MANIFEST_MAX_OUTPUT,
    MAX_REGISTRY_DIAGNOSTIC_PAGE, ManifestAppTaskJobRequest, ManifestConvergeJobRequest,
    ManifestEnsureJobRequest, ManifestVolumeGcApplyResult, ManifestVolumeGcPreviewPage,
    RegistryResourcePage, SetupAdoptRequest, SetupAdoptResult, SetupAppTaskJobRequest,
    SetupDoneResult, SetupEnsureEventPage, SetupEnsureJobRequest, SetupGcApplyResult,
    SetupGcPreviewPage, SetupPreparePolicy, SetupPrepareRequest, SetupReconcileMissingRepairResult,
    SetupReconcilePreviewPage, SetupRetiredStopResult, SetupTaskJobRequest, Status,
};
use bosn_core::parse_and_plan_compose_yaml;
use bosn_setup::{
    SetupAcquirePolicy, SetupPlan, SetupPlanAppSource, SetupPlanRequest, SetupSourceKind,
    plan_setup,
};
use kernal_api::async_engine::{Runtime, RuntimeBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufReader, Read, Write},
    path::PathBuf,
};
mod arguments;
mod backend;
mod render;
mod schemas;
use arguments::*;
use backend::*;
use render::*;
use schemas::*;

/// The date-version negotiated during the classic MCP initialize lifecycle.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// Keep a malformed peer from making the server buffer an unbounded line.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Leave room below the product daemon's one-mebibyte IPC frame limit.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_MCP_LOG_RECORDS: u32 = 64;
const MAX_MCP_REGISTRY_RECORDS: u32 = MAX_REGISTRY_DIAGNOSTIC_PAGE;
/// Bound filesystem and URL strings independently from the JSON-RPC line
/// bound.  `bosn-core` also validates the locator before it is observed.
const MAX_MCP_SETUP_STRING_BYTES: usize = 8 * 1024;
/// A caller-supplied YAML document only.  This is intentionally well below
/// the JSON-RPC frame limit and never names a server-side path.
const MAX_MCP_COMPOSE_DOCUMENT_BYTES: usize = 32 * 1024;

/// Native default matching the current Python command's state-root contract.
pub fn default_state_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("BOSN_STATE_DIR") {
        return PathBuf::from(value);
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(value) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(value).join("bosn");
        }
        return kernal_api::platform::host::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("AppData")
            .join("Local")
            .join("bosn");
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(value).join("bosn");
        }
        kernal_api::platform::host::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("state")
            .join("bosn")
    }
}

/// Serve a single stdio MCP session against one existing Bosn state directory.
///
/// This does not start a daemon.  The tool calls use the authenticated native
/// client and therefore fail closed when no compatible native daemon is live.
pub fn serve_stdio(state_dir: impl Into<PathBuf>) -> Result<(), Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(Error::Io)?;
    let state_dir = state_dir.into();
    let client = Client::for_state(&state_dir)?;
    let mut backend = DaemonBackend {
        runtime: &runtime,
        client,
        state_dir,
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_transport(stdin.lock(), stdout.lock(), &mut backend)
}

#[derive(Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

enum LineError {
    Io(io::Error),
    TooLarge,
}

/// Generic transport form is intentionally testable with in-memory streams.
fn serve_transport<R: Read, W: Write, B: Backend>(
    input: R,
    mut output: W,
    backend: &mut B,
) -> Result<(), Error> {
    let mut input = BufReader::new(input);
    let mut initialized = false;
    loop {
        let request = match read_line_bounded(&mut input) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(LineError::Io(error)) => return Err(Error::Io(error)),
            Err(LineError::TooLarge) => {
                write_response(
                    &mut output,
                    rpc_error(Value::Null, -32600, "request exceeds the 64 KiB limit"),
                )?;
                return Ok(());
            }
        };
        if request.is_empty() {
            continue;
        }
        if let Some(response) = handle_request(&request, &mut initialized, backend) {
            write_response(&mut output, response)?;
        }
    }
}

fn read_line_bounded(input: &mut impl Read) -> Result<Option<Vec<u8>>, LineError> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => return Ok(Some(line)),
            Ok(_) if byte[0] == b'\n' => {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            Ok(_) if line.len() == MAX_REQUEST_BYTES => return Err(LineError::TooLarge),
            Ok(_) => line.push(byte[0]),
            Err(error) => return Err(LineError::Io(error)),
        }
    }
}

fn handle_request<B: Backend>(
    bytes: &[u8],
    initialized: &mut bool,
    backend: &mut B,
) -> Option<Value> {
    let request: RpcRequest = match serde_json::from_slice(bytes) {
        Ok(request) => request,
        Err(_) => return Some(rpc_error(Value::Null, -32700, "invalid JSON-RPC payload")),
    };
    let id = request.id.clone().unwrap_or(Value::Null);
    let notification = request.id.is_none();
    if request.jsonrpc != "2.0" || !valid_id(&id) {
        return (!notification).then(|| rpc_error(id, -32600, "invalid JSON-RPC request"));
    }
    // MCP clients must correlate tool calls.  Silently ignoring all other
    // notifications prevents a malformed cancel notification from mutating a
    // daemon job with no response channel to report the outcome.
    if notification && request.method != "notifications/initialized" {
        return None;
    }
    let response = match request.method.as_str() {
        "initialize" => {
            if notification {
                return None;
            }
            *initialized = true;
            rpc_result(
                id,
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "bosn", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Bosn exposes read-only native daemon diagnostics and bounded job control. It does not execute shell commands through MCP."
                }),
            )
        }
        "notifications/initialized" => return None,
        "tools/list" if *initialized => rpc_result(id, tools_list()),
        "tools/call" if *initialized => rpc_result(id, call_tool(request.params, backend)),
        "tools/list" | "tools/call" => rpc_error(id, -32002, "MCP session is not initialized"),
        _ if notification => return None,
        _ => rpc_error(id, -32601, "unsupported MCP method"),
    };
    Some(response)
}

fn valid_id(id: &Value) -> bool {
    id.is_null() || id.is_string() || id.is_number()
}

fn call_tool<B: Backend>(params: Value, backend: &mut B) -> Value {
    let Some(params) = params.as_object() else {
        return tool_error("tools/call params must be an object");
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return tool_error("tools/call requires a string tool name");
    };
    if params.keys().any(|key| key != "name" && key != "arguments") {
        return tool_error("unsupported tools/call parameter");
    }
    let empty_arguments = serde_json::Map::new();
    let arguments = match params.get("arguments") {
        None => &empty_arguments,
        Some(Value::Object(arguments)) => arguments,
        Some(_) => return tool_error("tool arguments must be an object"),
    };
    if let Some(result) = crate::ci::mcp::call(name, arguments, backend.ci().as_mut()) {
        return match result {
            Ok(value) => tool_success(value),
            Err(message) => tool_error(&message),
        };
    }
    let result =
        match name {
            "bosn_compose_plan" => compose_plan_request(arguments).and_then(|document| {
                parse_and_plan_compose_yaml(&document)
                    .map(compose_plan_json)
                    .map_err(|_| {
                        ToolFailure::Invalid("Bosn Compose document is invalid or unsupported")
                    })
            }),
            "bosn_status" => {
                if !arguments.is_empty() {
                    return tool_error("bosn_status accepts no arguments");
                }
                backend
                    .status()
                    .map(status_json)
                    .map_err(|_| ToolFailure::Daemon)
            }
            "bosn_jobs" => {
                if !arguments.is_empty() {
                    return tool_error("bosn_jobs accepts no arguments");
                }
                backend.jobs().map_err(|_| ToolFailure::Daemon)
            }
            "bosn_doctor" => {
                if !arguments.is_empty() {
                    return tool_error("bosn_doctor accepts no arguments");
                }
                backend
                    .doctor()
                    .map(doctor_json)
                    .map_err(|_| ToolFailure::Daemon)
            }
            "bosn_registry_resources" => {
                registry_page_arguments(arguments).and_then(|(after, limit)| {
                    backend
                        .registry_resources(after, limit)
                        .map(resource_page_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_setup_ensure_events" => {
                registry_page_arguments(arguments).and_then(|(after, limit)| {
                    backend
                        .setup_ensure_events(after, limit)
                        .map(setup_ensure_event_page_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_setup_gc_preview" => {
                setup_gc_preview_arguments(arguments).and_then(|(workspace, after, limit)| {
                    backend
                        .setup_gc_preview(workspace, after, limit)
                        .map(setup_gc_preview_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_manifest_volume_gc_preview" => {
                setup_gc_preview_arguments(arguments).and_then(|(workspace, after, limit)| {
                    backend
                        .manifest_volume_gc_preview(workspace, after, limit)
                        .map(manifest_volume_gc_preview_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_manifest_volume_gc_apply" => {
                setup_gc_apply_arguments(arguments).and_then(|(workspace, token)| {
                    backend
                        .manifest_volume_gc_apply(workspace, token)
                        .map(manifest_volume_gc_apply_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_manifest_volume_release_preview" => setup_gc_preview_arguments(arguments)
                .and_then(|(workspace, after, limit)| {
                    backend
                        .manifest_volume_release_preview(workspace, after, limit)
                        .map(manifest_volume_gc_preview_json)
                        .map_err(|_| ToolFailure::Daemon)
                }),
            "bosn_manifest_volume_release_apply" => {
                setup_gc_apply_arguments(arguments).and_then(|(workspace, token)| {
                    backend
                        .manifest_volume_release_apply(workspace, token)
                        .map(manifest_volume_gc_apply_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_setup_reconcile_preview" => {
                setup_gc_preview_arguments(arguments).and_then(|(workspace, after, limit)| {
                    backend
                        .setup_reconcile_preview(workspace, after, limit)
                        .map(setup_reconcile_preview_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_setup_reconcile_repair_missing" => {
                setup_gc_apply_arguments(arguments).and_then(|(workspace, token)| {
                    backend
                        .setup_reconcile_repair_missing(workspace, token)
                        .map(setup_reconcile_repair_missing_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_setup_gc_apply" => {
                setup_gc_apply_arguments(arguments).and_then(|(workspace, token)| {
                    backend
                        .setup_gc_apply(workspace, token)
                        .map(setup_gc_apply_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_setup_stop_retired" => {
                setup_gc_apply_arguments(arguments).and_then(|(workspace, token)| {
                    backend
                        .setup_stop_retired(workspace, token)
                        .map(setup_stop_retired_json)
                        .map_err(|_| ToolFailure::Daemon)
                })
            }
            "bosn_setup_done" => setup_done_arguments(arguments).and_then(|workspace| {
                backend
                    .setup_done(workspace)
                    .map(setup_done_json)
                    .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_setup_adopt" => setup_adopt_arguments(arguments).and_then(|request| {
                backend
                    .setup_adopt(request)
                    .map(|value| json!({"action":"setup_adopt","adopted":value.adopted}))
                    .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_job_status" => job_id(arguments).and_then(|id| {
                only_arguments(arguments, &["job_id"])?;
                backend
                    .job_status(id)
                    .map(job_json)
                    .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_job_logs" => {
                if let Err(error) = only_arguments(arguments, &["job_id", "after", "limit"]) {
                    return tool_error(error.message());
                }
                let id = job_id(arguments);
                let after = optional_u64(arguments, "after", 0);
                let limit = optional_u64(arguments, "limit", u64::from(MAX_MCP_LOG_RECORDS))
                    .and_then(|value| {
                        (value > 0 && value <= u64::from(MAX_MCP_LOG_RECORDS))
                            .then_some(value as u32)
                            .ok_or(ToolFailure::Invalid("limit must be within 1..=64"))
                    });
                match (id, after, limit) {
                    (Ok(id), Ok(after), Ok(limit)) => backend
                        .job_logs(id, after, limit)
                        .map(log_page_json)
                        .map_err(|_| ToolFailure::Daemon),
                    (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => Err(error),
                }
            }
            "bosn_job_cancel" => job_id(arguments).and_then(|id| {
                only_arguments(arguments, &["job_id"])?;
                backend
                    .cancel_job(id)
                    .map(|()| json!({"job_id": id, "cancel_requested": true}))
                    .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_setup_plan" => setup_plan_request(arguments).and_then(|request| {
                // The client/server invocation, not an untrusted MCP tool call,
                // owns state selection.  This prevents a model from directing the
                // server to create or inspect arbitrary state roots.
                backend
                    .setup_plan(request.workspace, request.locator, request.policy)
                    .map(setup_plan_json)
                    .map_err(|_| ToolFailure::Setup)
            }),
            "bosn_setup_prepare" => setup_prepare_request(arguments).and_then(|request| {
                backend
                    .submit_setup_prepare(request)
                    .map(|job_id| {
                        json!({
                            "action": "setup_prepare",
                            "submitted": true,
                            "job_id": job_id,
                        })
                    })
                    .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_setup_ensure" => setup_ensure_request(arguments).and_then(|request| {
                backend
                    .submit_setup_ensure(request)
                    .map(|job_id| {
                        json!({
                            "action": "setup_ensure",
                            "submitted": true,
                            "job_id": job_id,
                        })
                    })
                    .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_manifest_ensure" => manifest_ensure_request(arguments).and_then(|request| {
                backend
                .submit_manifest_ensure(request)
                .map(|job_id| json!({"action":"manifest_ensure","submitted":true,"job_id":job_id}))
                .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_manifest_converge" => manifest_converge_request(arguments).and_then(|request| {
                backend
                .submit_manifest_converge(request)
                .map(
                    |job_id| json!({"action":"manifest_converge","submitted":true,"job_id":job_id}),
                )
                .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_manifest_app_task" => manifest_app_task_request(arguments).and_then(|request| {
                backend
                .submit_manifest_app_task(request)
                .map(
                    |job_id| json!({"action":"manifest_app_task","submitted":true,"job_id":job_id}),
                )
                .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_setup_task" => setup_task_request(arguments).and_then(|request| {
                backend
                    .submit_setup_task(request)
                    .map(|job_id| {
                        json!({
                            "action": "setup_task",
                            "submitted": true,
                            "job_id": job_id,
                        })
                    })
                    .map_err(|_| ToolFailure::Daemon)
            }),
            "bosn_setup_app_task" => setup_app_task_request(arguments).and_then(|request| {
                backend
                    .submit_setup_app_task(request)
                    .map(|job_id| {
                        json!({
                            "action": "setup_app_task", "submitted": true, "job_id": job_id,
                        })
                    })
                    .map_err(|_| ToolFailure::Daemon)
            }),
            _ => return tool_error("unknown Bosn MCP tool"),
        };
    match result {
        Ok(value) => tool_success(value),
        Err(ToolFailure::Invalid(message)) => tool_error(message),
        Err(ToolFailure::Daemon) => tool_error("native Bosn daemon request failed"),
        Err(ToolFailure::Setup) => tool_error("Bosn setup plan failed"),
    }
}

fn tool_success(value: Value) -> Value {
    match serde_json::to_string(&value) {
        Ok(text) => json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": value,
            "isError": false,
        }),
        Err(_) => tool_error("could not serialize Bosn tool result"),
    }
}

fn tool_error(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn write_response(output: &mut impl Write, response: Value) -> Result<(), Error> {
    let encoded = serde_json::to_vec(&response).map_err(|_| Error::Protocol("MCP encode"))?;
    if encoded.len() > MAX_RESPONSE_BYTES {
        let fallback = rpc_error(Value::Null, -32603, "MCP response exceeds output limit");
        let encoded = serde_json::to_vec(&fallback).map_err(|_| Error::Protocol("MCP encode"))?;
        output.write_all(&encoded)?;
    } else {
        output.write_all(&encoded)?;
    }
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests;
