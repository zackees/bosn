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

use crate::{Client, Error, JobLogPage, JobStatus, Status};
use kernal_api::async_engine::{Runtime, RuntimeBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufReader, Read, Write},
    path::PathBuf,
};

/// The date-version negotiated during the classic MCP initialize lifecycle.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// Keep a malformed peer from making the server buffer an unbounded line.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Leave room below the product daemon's one-mebibyte IPC frame limit.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_MCP_LOG_RECORDS: u32 = 64;

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
    let client = Client::for_state(state_dir.into())?;
    let mut backend = DaemonBackend {
        runtime: &runtime,
        client,
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_transport(stdin.lock(), stdout.lock(), &mut backend)
}

trait Backend {
    fn status(&mut self) -> Result<Status, Error>;
    fn job_status(&mut self, id: u64) -> Result<JobStatus, Error>;
    fn job_logs(&mut self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error>;
    fn cancel_job(&mut self, id: u64) -> Result<(), Error>;
}

struct DaemonBackend<'a> {
    runtime: &'a Runtime,
    client: Client,
}
impl Backend for DaemonBackend<'_> {
    fn status(&mut self) -> Result<Status, Error> {
        self.runtime.run(self.client.status())
    }
    fn job_status(&mut self, id: u64) -> Result<JobStatus, Error> {
        self.runtime.run(self.client.job_status(id))
    }
    fn job_logs(&mut self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error> {
        self.runtime.run(self.client.job_logs(id, after, limit))
    }
    fn cancel_job(&mut self, id: u64) -> Result<(), Error> {
        self.runtime.run(self.client.cancel_job(id))
    }
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

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "bosn_status",
                "description": "Read the native Bosn daemon registry status. Does not start a daemon or mutate state.",
                "inputSchema": {"type": "object", "additionalProperties": false},
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_job_status",
                "description": "Read the state of one native daemon job.",
                "inputSchema": job_id_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_job_logs",
                "description": "Read a bounded, cursor-paginated page of native daemon job logs.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["job_id"],
                    "properties": {
                        "job_id": {"type": "integer", "minimum": 1},
                        "after": {"type": "integer", "minimum": 0, "default": 0},
                        "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MCP_LOG_RECORDS, "default": MAX_MCP_LOG_RECORDS}
                    }
                },
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_job_cancel",
                "description": "Request cancellation of one non-terminal native daemon job. This changes daemon job state but does not delete resources.",
                "inputSchema": job_id_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            }
        ]
    })
}

fn job_id_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["job_id"],
        "properties": {"job_id": {"type": "integer", "minimum": 1}}
    })
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
    let result = match name {
        "bosn_status" => {
            if !arguments.is_empty() {
                return tool_error("bosn_status accepts no arguments");
            }
            backend
                .status()
                .map(status_json)
                .map_err(|_| ToolFailure::Daemon)
        }
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
            let limit = optional_u64(arguments, "limit", u64::from(MAX_MCP_LOG_RECORDS)).and_then(
                |value| {
                    (value > 0 && value <= u64::from(MAX_MCP_LOG_RECORDS))
                        .then_some(value as u32)
                        .ok_or(ToolFailure::Invalid("limit must be within 1..=64"))
                },
            );
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
        _ => return tool_error("unknown Bosn MCP tool"),
    };
    match result {
        Ok(value) => tool_success(value),
        Err(ToolFailure::Invalid(message)) => tool_error(message),
        Err(ToolFailure::Daemon) => tool_error("native Bosn daemon request failed"),
    }
}

enum ToolFailure {
    Invalid(&'static str),
    Daemon,
}
impl ToolFailure {
    fn message(&self) -> &'static str {
        match self {
            Self::Invalid(message) => message,
            Self::Daemon => "native Bosn daemon request failed",
        }
    }
}

fn only_arguments(
    arguments: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), ToolFailure> {
    arguments
        .keys()
        .all(|key| allowed.contains(&key.as_str()))
        .then_some(())
        .ok_or(ToolFailure::Invalid("unsupported tool argument"))
}

fn job_id(arguments: &serde_json::Map<String, Value>) -> Result<u64, ToolFailure> {
    let id = optional_u64(arguments, "job_id", 0)?;
    (id > 0)
        .then_some(id)
        .ok_or(ToolFailure::Invalid("job_id must be a positive integer"))
}

fn optional_u64(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
    default: u64,
) -> Result<u64, ToolFailure> {
    match arguments.get(key) {
        None => Ok(default),
        Some(value) => value.as_u64().ok_or(ToolFailure::Invalid(
            "numeric arguments must be unsigned integers",
        )),
    }
}

fn status_json(status: Status) -> Value {
    json!({
        "registry_id": status.registry_id,
        "schema_version": status.schema_version,
        "resources": status.resources,
        "leases": status.leases,
        "sessions": status.sessions,
        "reconciliation_required": status.reconciliation_required,
    })
}

fn job_json(job: JobStatus) -> Value {
    json!({"job_id": job.id, "state": job.state, "error": job.error})
}

fn log_page_json(page: JobLogPage) -> Value {
    let records: Vec<Value> = page
        .records
        .into_iter()
        .map(|record| json!({"cursor": record.cursor, "line": record.line}))
        .collect();
    json!({
        "retained_from": page.retained_from,
        "next": page.next,
        "gap": page.gap,
        "records": records,
    })
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
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeBackend {
        cancelled: Vec<u64>,
    }
    impl Backend for FakeBackend {
        fn status(&mut self) -> Result<Status, Error> {
            Ok(Status {
                registry_id: "123e4567-e89b-42d3-a456-426614174000".into(),
                schema_version: 5,
                resources: 3,
                leases: 2,
                sessions: 1,
                reconciliation_required: false,
            })
        }
        fn job_status(&mut self, id: u64) -> Result<JobStatus, Error> {
            Ok(JobStatus {
                id,
                state: "Running".into(),
                error: None,
            })
        }
        fn job_logs(&mut self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error> {
            Ok(JobLogPage {
                retained_from: 0,
                next: after + 1,
                gap: false,
                records: vec![crate::JobLogRecord {
                    cursor: after,
                    line: format!("job={id} limit={limit}"),
                }],
            })
        }
        fn cancel_job(&mut self, id: u64) -> Result<(), Error> {
            self.cancelled.push(id);
            Ok(())
        }
    }

    fn exchange(input: &str, backend: &mut FakeBackend) -> Vec<Value> {
        let mut output = Vec::new();
        serve_transport(input.as_bytes(), &mut output, backend).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn stdio_initialize_lists_tools_and_calls_status() {
        let mut backend = FakeBackend::default();
        let replies = exchange(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_status","arguments":{}}}"#,
                "\n",
            ),
            &mut backend,
        );
        assert_eq!(replies.len(), 3);
        assert_eq!(
            replies[0]["result"]["protocolVersion"],
            MCP_PROTOCOL_VERSION
        );
        let names: Vec<&str> = replies[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "bosn_status",
                "bosn_job_status",
                "bosn_job_logs",
                "bosn_job_cancel"
            ]
        );
        assert_eq!(replies[2]["result"]["isError"], false);
        assert_eq!(replies[2]["result"]["structuredContent"]["resources"], 3);
    }

    #[test]
    fn stdio_tool_call_validates_arguments_and_cancels_exact_job() {
        let mut backend = FakeBackend::default();
        let replies = exchange(
            concat!(
                r#"{"jsonrpc":"2.0","id":"a","method":"initialize"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":"b","method":"tools/call","params":{"name":"bosn_job_cancel","arguments":{"job_id":9}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":"c","method":"tools/call","params":{"name":"bosn_job_logs","arguments":{"job_id":9,"limit":65}}}"#,
                "\n",
            ),
            &mut backend,
        );
        assert_eq!(backend.cancelled, [9]);
        assert_eq!(replies[1]["result"]["isError"], false);
        assert_eq!(replies[2]["result"]["isError"], true);
    }

    #[test]
    fn oversized_input_is_rejected_without_unbounded_buffering() {
        let mut backend = FakeBackend::default();
        let input = format!("{}\n", "x".repeat(MAX_REQUEST_BYTES + 1));
        let replies = exchange(&input, &mut backend);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["error"]["code"], -32600);
    }

    #[test]
    fn notifications_never_mutate_daemon_jobs() {
        let mut backend = FakeBackend::default();
        let replies = exchange(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"bosn_job_cancel","arguments":{"job_id":9}}}"#,
                "\n",
            ),
            &mut backend,
        );
        assert_eq!(replies.len(), 1);
        assert!(backend.cancelled.is_empty());
    }
}
