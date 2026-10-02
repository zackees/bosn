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

trait Backend {
    /// The CI tools' daemon view (see [`crate::ci::mcp`]).
    fn ci(&mut self) -> Box<dyn crate::ci::mcp::CiBackend + '_>;
    fn status(&mut self) -> Result<Status, Error>;
    fn doctor(&mut self) -> Result<DoctorReport, Error>;
    fn registry_resources(&mut self, after: u64, limit: u32)
    -> Result<RegistryResourcePage, Error>;
    fn setup_ensure_events(
        &mut self,
        after: u64,
        limit: u32,
    ) -> Result<SetupEnsureEventPage, Error>;
    fn setup_gc_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<SetupGcPreviewPage, Error>;
    fn manifest_volume_gc_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error>;
    fn manifest_volume_release_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error>;
    fn setup_reconcile_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<SetupReconcilePreviewPage, Error>;
    fn setup_reconcile_repair_missing(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupReconcileMissingRepairResult, Error>;
    fn setup_gc_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupGcApplyResult, Error>;
    fn manifest_volume_gc_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error>;
    fn manifest_volume_release_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error>;
    fn setup_stop_retired(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupRetiredStopResult, Error>;
    fn setup_done(&mut self, workspace: PathBuf) -> Result<SetupDoneResult, Error>;
    fn setup_adopt(&mut self, request: SetupAdoptRequest) -> Result<SetupAdoptResult, Error>;
    fn job_status(&mut self, id: u64) -> Result<JobStatus, Error>;
    fn job_logs(&mut self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error>;
    fn cancel_job(&mut self, id: u64) -> Result<(), Error>;
    /// Submit a bounded, semantic setup image-preparation job. The daemon,
    /// rather than the MCP process, owns all Docker interaction.
    fn submit_setup_prepare(&mut self, request: SetupPrepareRequest) -> Result<u64, Error>;
    /// Submit a bounded, semantic application ensure job. The daemon derives
    /// every lifecycle detail from the validated setup document and refuses a
    /// foreign or mismatched candidate rather than replacing it.
    fn submit_setup_ensure(&mut self, request: SetupEnsureJobRequest) -> Result<u64, Error>;
    fn submit_manifest_ensure(&mut self, request: ManifestEnsureJobRequest) -> Result<u64, Error>;
    fn submit_manifest_converge(
        &mut self,
        request: ManifestConvergeJobRequest,
    ) -> Result<u64, Error>;
    fn submit_manifest_app_task(
        &mut self,
        request: ManifestAppTaskJobRequest,
    ) -> Result<u64, Error>;
    /// Submit one complete setup plan, image-preparation, and declared-task
    /// job.  The named task is the only executable selection exposed to MCP;
    /// the daemon derives all task details from the validated setup document.
    fn submit_setup_task(&mut self, request: SetupTaskJobRequest) -> Result<u64, Error>;
    fn submit_setup_app_task(&mut self, request: SetupAppTaskJobRequest) -> Result<u64, Error>;
    /// Generate an inert setup receipt under the state root selected when the
    /// MCP process was started.  Tool arguments intentionally cannot replace
    /// that root.
    fn setup_plan(
        &mut self,
        workspace: PathBuf,
        locator: String,
        policy: SetupAcquirePolicy,
    ) -> Result<SetupPlan, Error>;
}

struct DaemonBackend<'a> {
    runtime: &'a Runtime,
    client: Client,
    state_dir: PathBuf,
}
impl Backend for DaemonBackend<'_> {
    fn ci(&mut self) -> Box<dyn crate::ci::mcp::CiBackend + '_> {
        Box::new(crate::ci::mcp::ClientCi::new(self.runtime, &self.client))
    }
    fn status(&mut self) -> Result<Status, Error> {
        self.runtime.run(self.client.status())
    }
    fn doctor(&mut self) -> Result<DoctorReport, Error> {
        self.runtime.run(self.client.doctor())
    }
    fn registry_resources(
        &mut self,
        after: u64,
        limit: u32,
    ) -> Result<RegistryResourcePage, Error> {
        self.runtime
            .run(self.client.registry_resources(after, limit))
    }
    fn setup_ensure_events(
        &mut self,
        after: u64,
        limit: u32,
    ) -> Result<SetupEnsureEventPage, Error> {
        self.runtime
            .run(self.client.setup_ensure_events(after, limit))
    }
    fn setup_gc_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<SetupGcPreviewPage, Error> {
        self.runtime
            .run(self.client.setup_gc_preview(workspace, after, limit))
    }
    fn manifest_volume_gc_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        self.runtime.run(
            self.client
                .manifest_volume_gc_preview(workspace, after, limit),
        )
    }
    fn manifest_volume_release_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        self.runtime.run(
            self.client
                .manifest_volume_release_preview(workspace, after, limit),
        )
    }
    fn setup_reconcile_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<SetupReconcilePreviewPage, Error> {
        self.runtime
            .run(self.client.setup_reconcile_preview(workspace, after, limit))
    }
    fn setup_reconcile_repair_missing(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupReconcileMissingRepairResult, Error> {
        self.runtime.run(
            self.client
                .setup_reconcile_repair_missing(workspace, &token, true),
        )
    }
    fn setup_gc_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupGcApplyResult, Error> {
        self.runtime
            .run(self.client.setup_gc_apply(workspace, &token, true))
    }
    fn manifest_volume_gc_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error> {
        self.runtime.run(
            self.client
                .manifest_volume_gc_apply(workspace, &token, true),
        )
    }
    fn manifest_volume_release_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error> {
        self.runtime.run(
            self.client
                .manifest_volume_release_apply(workspace, &token, true),
        )
    }
    fn setup_stop_retired(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupRetiredStopResult, Error> {
        self.runtime
            .run(self.client.setup_stop_retired(workspace, &token, true))
    }
    fn setup_done(&mut self, workspace: PathBuf) -> Result<SetupDoneResult, Error> {
        self.runtime.run(self.client.setup_done(workspace, true))
    }
    fn setup_adopt(&mut self, request: SetupAdoptRequest) -> Result<SetupAdoptResult, Error> {
        self.runtime.run(self.client.setup_adopt(request))
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
    fn submit_setup_prepare(&mut self, request: SetupPrepareRequest) -> Result<u64, Error> {
        self.runtime.run(self.client.submit_setup_prepare(request))
    }
    fn submit_setup_ensure(&mut self, request: SetupEnsureJobRequest) -> Result<u64, Error> {
        self.runtime.run(self.client.submit_setup_ensure(request))
    }
    fn submit_manifest_ensure(&mut self, request: ManifestEnsureJobRequest) -> Result<u64, Error> {
        self.runtime
            .run(self.client.submit_manifest_ensure(request))
    }
    fn submit_manifest_converge(
        &mut self,
        request: ManifestConvergeJobRequest,
    ) -> Result<u64, Error> {
        self.runtime
            .run(self.client.submit_manifest_converge(request))
    }
    fn submit_manifest_app_task(
        &mut self,
        request: ManifestAppTaskJobRequest,
    ) -> Result<u64, Error> {
        self.runtime
            .run(self.client.submit_manifest_app_task(request))
    }
    fn submit_setup_task(&mut self, request: SetupTaskJobRequest) -> Result<u64, Error> {
        self.runtime.run(self.client.submit_setup_task(request))
    }
    fn submit_setup_app_task(&mut self, request: SetupAppTaskJobRequest) -> Result<u64, Error> {
        self.runtime.run(self.client.submit_setup_app_task(request))
    }
    fn setup_plan(
        &mut self,
        workspace: PathBuf,
        locator: String,
        policy: SetupAcquirePolicy,
    ) -> Result<SetupPlan, Error> {
        // Keep detailed filesystem/remote errors out of the MCP boundary.
        self.runtime
            .run(plan_setup(SetupPlanRequest {
                state_dir: self.state_dir.clone(),
                workspace,
                locator,
                policy,
            }))
            .map_err(|_| Error::Protocol("setup plan failed"))
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
    let mut list = json!({
        "tools": [
            {
                "name": "bosn_status",
                "description": "Read the native Bosn daemon registry status. Does not start a daemon or mutate state.",
                "inputSchema": {"type": "object", "additionalProperties": false},
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_doctor",
                "description": "Run fixed daemon-owned, read-only registry integrity and Docker version checks. It accepts no arguments, never starts a daemon, initializes or migrates a registry, mutates Docker, or returns raw engine output.",
                "inputSchema": {"type": "object", "additionalProperties": false},
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_registry_resources",
                "description": "Read a bounded cursor page of path-safe managed-resource diagnostics from the already-running native Bosn daemon. Does not start a daemon, initialize a registry, or mutate state.",
                "inputSchema": registry_page_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_ensure_events",
                "description": "Read a bounded newest-first cursor page of credential-safe setup ensure and native-manifest lifecycle history from the already-running native Bosn daemon. Does not start a daemon, initialize a registry, or mutate state.",
                "inputSchema": registry_page_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_gc_preview",
                "description": "Preview only future collection candidates for retired Bosn-managed setup containers in one workspace. This never starts a daemon, writes SQLite, calls Docker, stops, deletes, or applies GC. A future apply must recheck all ownership facts.",
                "inputSchema": setup_gc_preview_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {"name":"bosn_manifest_volume_gc_preview","description":"Preview only retired disposable native-manifest volumes in one workspace. Only warm spec-scoped volume generations can appear; machine, stack, and pinned data are protected. Never writes SQLite or calls Docker.","inputSchema":setup_gc_preview_schema(),"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {"name":"bosn_manifest_volume_gc_apply","description":"DESTRUCTIVE: remove exactly one preview-token-bound retired warm spec native-manifest volume after the daemon rechecks registry ownership, exact Docker labels, and empty Docker attachment state. Explicit confirmation required.","inputSchema":setup_gc_apply_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false}},
            {"name":"bosn_manifest_volume_release_preview","description":"Preview durable native-manifest volumes that normal GC protects: stack/machine scope or pinned retention. It never writes state or calls Docker; apply requires one returned opaque token.","inputSchema":setup_gc_preview_schema(),"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {"name":"bosn_manifest_volume_release_apply","description":"DESTRUCTIVE: explicitly release exactly one preview-token-bound durable manifest volume. The daemon rechecks registry uses, leases, sessions, intents, exact Docker labels, and attachments immediately before fixed-name removal. Explicit confirmation required.","inputSchema":setup_gc_apply_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false}},
            {"name":"bosn_setup_reconcile_preview","description":"Read-only compare of durable Bosn setup-container ownership with fixed Docker inspection for one workspace. It never repairs, writes SQLite, creates/starts/stops/removes Docker resources, or accepts engine controls.","inputSchema":setup_gc_preview_schema(),"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {"name":"bosn_setup_reconcile_repair_missing","description":"STATE CHANGE: retire exactly one preview-token-bound active managed setup app only after the daemon rechecks ownership/use protection and fixed Docker inspection still proves it missing. It never starts, creates, stops, removes, or otherwise mutates Docker.","inputSchema":setup_gc_apply_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {
                "name": "bosn_setup_gc_apply",
                "description": "DESTRUCTIVE: remove exactly one retired Bosn-managed setup container using a preview candidate token and explicit confirmation. The daemon rechecks registry ownership and Docker labels before removal.",
                "inputSchema": setup_gc_apply_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_stop_retired",
                "description": "DESTRUCTIVE: stop exactly one running retired Bosn-managed setup container using a preview candidate token and explicit confirmation. It retains the registry record for later GC apply and never removes images, volumes, or containers.",
                "inputSchema": setup_gc_apply_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_done",
                "description": "STATE CHANGE: mark this workspace's active setup registry ownership done. It never calls Docker, stops/removes resources, or accepts engine controls; confirmation is required.",
                "inputSchema": setup_done_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {"name":"bosn_setup_adopt","description":"STATE CHANGE: restore registry ownership only after the daemon proves an existing app has exact Bosn labels, deterministic name, and prepared image identity. Confirmation required; no Docker controls.","inputSchema":setup_adopt_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
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
            },
            {
                "name": "bosn_setup_plan",
                "description": "Validate, cache, and materialize one Bosn setup document into the MCP server's preselected private state directory. A `.yaml`/`.yml` locator is accepted only for the supported lossless single-service Compose-to-setup subset; all other locators remain TOML-only. Returns an inert receipt only: applied is always false; it does not start a daemon or invoke Docker.",
                "inputSchema": setup_plan_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_compose_plan",
                "description": "Parse, validate, and digest caller-supplied Compose YAML using Bosn's documented subset. This is pure review data: it reads no file, contacts no daemon or Docker engine, writes no state, and never executes Compose.",
                "inputSchema": compose_plan_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_prepare",
                "description": "Submit one bounded daemon-owned setup image-preparation job. Returns promptly with a durable job ID; poll the existing job tools for outcome and logs. It does not start a daemon, run setup tasks, or accept Docker, mount, or output-path controls.",
                "inputSchema": setup_prepare_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_ensure",
                "description": "Submit one bounded daemon-owned setup application ensure job. Returns promptly with a durable job ID; poll the existing job tools for outcome and logs. The daemon may create an absent app or start a matching stopped app, but refuses foreign or mismatched candidates and never deletes or replaces an app.",
                "inputSchema": setup_ensure_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_manifest_ensure",
                "description": "Submit a bounded daemon-owned ensure of one explicitly named legacy Bosn manifest stack. The supported runtime slice accepts a workspace-contained local TOML manifest, an immutable external image or a workspace Dockerfile (root or nested) whose external images are digest-pinned, declared environment, typed managed volumes, safe workspace binds/workdir, an explicit host Docker socket bind (its resources are outside Bosn supervision), and bounded tmpfs target/ro/rw/size/exec/mode declarations. Dockerfile contexts are copied into daemon-owned content-addressed state; symlinks, other host paths, replacement controls, and arbitrary Docker controls are refused.",
                "inputSchema": manifest_ensure_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_manifest_converge",
                "description": "Submit one bounded daemon-owned convergence of every stack in a workspace-contained legacy Bosn TOML manifest. The current manifest schema has no dependency edges or root selector, so the daemon snapshots the declared stack names and ensures them in deterministic lexical order, one at a time. Each member keeps the normal typed image/build, volumes, workspace mounts/workdir, tmpfs, bounded guest, registry, and rollover safeguards; a later failure leaves earlier successful member records durable. Callers cannot select stack order, Docker arguments, mounts, images, or commands.",
                "inputSchema": manifest_converge_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_manifest_app_task",
                "description": "Submit one named task declared by an already ensured supported manifest stack. The daemon re-reads the manifest and proves exact running ownership before fixed exec; commands, containers, Docker arguments, mounts, and environment controls are refused. Cancellation may leave remote completion unknown.",
                "inputSchema": manifest_app_task_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_task",
                "description": "Submit one bounded daemon-owned setup plan, image-preparation, and declared-task job. Returns promptly with a durable job ID; poll the existing job tools for outcome and logs. It accepts only a named task declared in the setup document, never task commands or engine controls.",
                "inputSchema": setup_task_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_app_task",
                "description": "Submit one declared task for execution inside an already ensured Bosn setup application. The daemon re-plans and verifies the exact managed container before a fixed exec; it accepts no command, Docker, or container controls. Returns a durable job ID promptly. Cancelling the local exec client does not establish that the in-container command stopped.",
                "inputSchema": setup_task_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            }
        ]
    });
    if let Some(tools) = list["tools"].as_array_mut() {
        tools.extend(crate::ci::mcp::tools());
    }
    list
}

fn compose_plan_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["document"],
        "properties": {
            "document": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_COMPOSE_DOCUMENT_BYTES, "description": "Complete caller-supplied Compose YAML. Filesystem paths and URLs are not accepted by this tool."}
        }
    })
}

fn setup_plan_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace", "config", "policy"],
        "properties": {
            "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES},
            "config": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},
            "policy": {"type": "string", "enum": ["refresh", "offline"], "description": "refresh reads the selected source; offline reuses only its verified cached receipt."}
        }
    })
}

fn setup_prepare_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace", "config", "policy", "deadline_ms", "output_limit"],
        "properties": {
            "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES},
            "config": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},
            "policy": {"type": "string", "enum": ["refresh", "offline"]},
            "deadline_ms": {"type": "integer", "minimum": 1, "maximum": 300000},
            "output_limit": {"type": "integer", "minimum": 1, "maximum": 8388608}
        }
    })
}

fn setup_ensure_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace", "config", "policy", "deadline_ms", "output_limit"],
        "properties": {
            "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES},
            "config": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},
            "policy": {"type": "string", "enum": ["refresh", "offline"]},
            "deadline_ms": {"type": "integer", "minimum": 1, "maximum": 300000},
            "output_limit": {"type": "integer", "minimum": 1, "maximum": 8388608}
        }
    })
}

fn manifest_ensure_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","manifest","stack","deadline_ms","output_limit"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "manifest":{"type":"string","minLength":1,"maxLength":4096,"description":"Safe relative TOML path beneath workspace; URLs and absolute paths are refused."},
        "stack":{"type":"string","minLength":1,"maxLength":128},
        "deadline_ms":{"type":"integer","minimum":1,"maximum":14400000},
        "output_limit":{"type":"integer","minimum":1,"maximum":67108864}
    }})
}
fn manifest_converge_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","manifest","deadline_ms","output_limit"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "manifest":{"type":"string","minLength":1,"maxLength":4096,"description":"Safe relative TOML path beneath workspace; URLs, dependency selectors, and absolute paths are refused."},
        "deadline_ms":{"type":"integer","minimum":1,"maximum":14400000},
        "output_limit":{"type":"integer","minimum":1,"maximum":67108864}
    }})
}
fn manifest_app_task_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","manifest","stack","task_name","deadline_ms","output_limit"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "manifest":{"type":"string","minLength":1,"maxLength":4096,"description":"Safe relative TOML path beneath workspace; URLs and absolute paths are refused."},
        "stack":{"type":"string","minLength":1,"maxLength":128},
        "task_name":{"type":"string","minLength":1,"maxLength":64,"description":"A declared task belonging to the selected stack."},
        "deadline_ms":{"type":"integer","minimum":1,"maximum":14400000},
        "output_limit":{"type":"integer","minimum":1,"maximum":67108864}
    }})
}

fn setup_task_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace", "config", "policy", "task_name", "deadline_ms", "output_limit"],
        "properties": {
            "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES},
            "config": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},
            "policy": {"type": "string", "enum": ["refresh", "offline"]},
            "task_name": {"type": "string", "minLength": 1, "maxLength": 64, "description": "A declared setup task name: starts alphanumeric and then uses alphanumerics, underscores, or hyphens."},
            "deadline_ms": {"type": "integer", "minimum": 1, "maximum": 300000},
            "output_limit": {"type": "integer", "minimum": 1, "maximum": 8388608}
        }
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

fn registry_page_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "after": {"type": "integer", "minimum": 0, "default": 0, "description": "Opaque offset cursor returned as next; start at 0."},
            "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MCP_REGISTRY_RECORDS, "default": MAX_MCP_REGISTRY_RECORDS}
        }
    })
}
fn setup_gc_preview_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "required": ["workspace"], "properties": {
        "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "Workspace selector; it is never returned in preview output."},
        "after": {"type": "integer", "minimum": 0, "default": 0},
        "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MCP_REGISTRY_RECORDS, "default": MAX_MCP_REGISTRY_RECORDS}
    }})
}
fn setup_gc_apply_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","candidate_token","confirm"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "candidate_token":{"type":"string","minLength":5,"maxLength":24580},
        "confirm":{"const":true,"description":"Explicit destructive confirmation."}
    }})
}
fn setup_done_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","confirm"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "confirm":{"const":true,"description":"Explicit state-change confirmation."}
    }})
}
fn setup_adopt_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","config","policy","deadline_ms","output_limit","confirm"],"properties":{"workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},"config":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES,"description":"An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},"policy":{"type":"string","enum":["refresh","offline"]},"deadline_ms":{"type":"integer","minimum":1,"maximum":300000},"output_limit":{"type":"integer","minimum":1,"maximum":8388608},"confirm":{"const":true}}})
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

fn compose_plan_request(arguments: &serde_json::Map<String, Value>) -> Result<String, ToolFailure> {
    only_arguments(arguments, &["document"])?;
    let document = arguments
        .get("document")
        .and_then(Value::as_str)
        .ok_or(ToolFailure::Invalid("document must be a string"))?;
    if document.is_empty() || document.len() > MAX_MCP_COMPOSE_DOCUMENT_BYTES {
        return Err(ToolFailure::Invalid(
            "document must be within 1..=32768 bytes",
        ));
    }
    Ok(document.to_owned())
}

enum ToolFailure {
    Invalid(&'static str),
    Daemon,
    Setup,
}
impl ToolFailure {
    fn message(&self) -> &'static str {
        match self {
            Self::Invalid(message) => message,
            Self::Daemon => "native Bosn daemon request failed",
            Self::Setup => "Bosn setup plan failed",
        }
    }
}

struct SetupPlanInput {
    workspace: PathBuf,
    locator: String,
    policy: SetupAcquirePolicy,
}

struct SetupPrepareInput {
    workspace: PathBuf,
    config: String,
    policy: SetupPreparePolicy,
    deadline_ms: u64,
    output_limit: usize,
}

struct SetupTaskInput {
    workspace: PathBuf,
    config: String,
    policy: SetupPreparePolicy,
    task_name: String,
    deadline_ms: u64,
    output_limit: usize,
}

fn setup_plan_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupPlanInput, ToolFailure> {
    only_arguments(arguments, &["workspace", "config", "policy"])?;
    let workspace = required_setup_string(arguments, "workspace")?;
    let locator = required_setup_string(arguments, "config")?;
    let policy = match required_setup_string(arguments, "policy")?.as_str() {
        "refresh" => SetupAcquirePolicy::OnlineRefresh,
        "offline" => SetupAcquirePolicy::OfflineCacheOnly,
        _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
    };
    Ok(SetupPlanInput {
        workspace: PathBuf::from(workspace),
        locator,
        policy,
    })
}

fn setup_prepare_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupPrepareRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "config",
            "policy",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let input = SetupPrepareInput {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        config: required_setup_string(arguments, "config")?,
        policy: match required_setup_string(arguments, "policy")?.as_str() {
            "refresh" => SetupPreparePolicy::Refresh,
            "offline" => SetupPreparePolicy::Offline,
            _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
        },
        deadline_ms: required_bounded_u64(arguments, "deadline_ms", 300_000)?,
        output_limit: required_bounded_u64(arguments, "output_limit", 8 * 1024 * 1024)? as usize,
    };
    Ok(SetupPrepareRequest {
        workspace: input.workspace,
        config: input.config,
        policy: input.policy,
        deadline: std::time::Duration::from_millis(input.deadline_ms),
        output_limit: input.output_limit,
    })
}

fn setup_ensure_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupEnsureJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "config",
            "policy",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let input = SetupPrepareInput {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        config: required_setup_string(arguments, "config")?,
        policy: match required_setup_string(arguments, "policy")?.as_str() {
            "refresh" => SetupPreparePolicy::Refresh,
            "offline" => SetupPreparePolicy::Offline,
            _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
        },
        deadline_ms: required_bounded_u64(arguments, "deadline_ms", 300_000)?,
        output_limit: required_bounded_u64(arguments, "output_limit", 8 * 1024 * 1024)? as usize,
    };
    Ok(SetupEnsureJobRequest {
        workspace: input.workspace,
        config: input.config,
        policy: input.policy,
        deadline: std::time::Duration::from_millis(input.deadline_ms),
        output_limit: input.output_limit,
    })
}

fn manifest_ensure_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<ManifestEnsureJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "manifest",
            "stack",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let manifest = required_setup_string(arguments, "manifest")?;
    if manifest.starts_with('/')
        || manifest.contains('\\')
        || manifest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ToolFailure::Invalid(
            "manifest must be a safe workspace-relative path",
        ));
    }
    let stack = required_setup_string(arguments, "stack")?;
    if stack.len() > 128
        || !stack
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ToolFailure::Invalid("manifest stack is invalid"));
    }
    Ok(ManifestEnsureJobRequest {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        manifest,
        stack,
        deadline: std::time::Duration::from_millis(required_bounded_u64(
            arguments,
            "deadline_ms",
            MANIFEST_MAX_DEADLINE.as_millis() as u64,
        )?),
        output_limit: required_bounded_u64(arguments, "output_limit", MANIFEST_MAX_OUTPUT as u64)?
            as usize,
    })
}
fn manifest_converge_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<ManifestConvergeJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &["workspace", "manifest", "deadline_ms", "output_limit"],
    )?;
    let manifest = required_setup_string(arguments, "manifest")?;
    if manifest.starts_with('/')
        || manifest.contains('\\')
        || manifest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ToolFailure::Invalid(
            "manifest must be a safe workspace-relative path",
        ));
    }
    Ok(ManifestConvergeJobRequest {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        manifest,
        deadline: std::time::Duration::from_millis(required_bounded_u64(
            arguments,
            "deadline_ms",
            MANIFEST_MAX_DEADLINE.as_millis() as u64,
        )?),
        output_limit: required_bounded_u64(arguments, "output_limit", MANIFEST_MAX_OUTPUT as u64)?
            as usize,
    })
}
fn manifest_app_task_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<ManifestAppTaskJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "manifest",
            "stack",
            "task_name",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let base = manifest_ensure_request(&{
        let mut copied = arguments.clone();
        copied.remove("task_name");
        copied
    })?;
    let task_name = required_setup_task_name(arguments)?;
    Ok(ManifestAppTaskJobRequest {
        workspace: base.workspace,
        manifest: base.manifest,
        stack: base.stack,
        task_name,
        deadline: base.deadline,
        output_limit: base.output_limit,
    })
}

fn setup_task_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupTaskJobRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "config",
            "policy",
            "task_name",
            "deadline_ms",
            "output_limit",
        ],
    )?;
    let input = SetupTaskInput {
        workspace: PathBuf::from(required_setup_string(arguments, "workspace")?),
        config: required_setup_string(arguments, "config")?,
        policy: match required_setup_string(arguments, "policy")?.as_str() {
            "refresh" => SetupPreparePolicy::Refresh,
            "offline" => SetupPreparePolicy::Offline,
            _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
        },
        task_name: required_setup_task_name(arguments)?,
        deadline_ms: required_bounded_u64(arguments, "deadline_ms", 300_000)?,
        output_limit: required_bounded_u64(arguments, "output_limit", 8 * 1024 * 1024)? as usize,
    };
    Ok(SetupTaskJobRequest {
        workspace: input.workspace,
        config: input.config,
        policy: input.policy,
        task_name: input.task_name,
        deadline: std::time::Duration::from_millis(input.deadline_ms),
        output_limit: input.output_limit,
    })
}

fn setup_app_task_request(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupAppTaskJobRequest, ToolFailure> {
    let task = setup_task_request(arguments)?;
    Ok(SetupAppTaskJobRequest {
        workspace: task.workspace,
        config: task.config,
        policy: task.policy,
        task_name: task.task_name,
        deadline: task.deadline,
        output_limit: task.output_limit,
    })
}

fn required_setup_string(
    arguments: &serde_json::Map<String, Value>,
    key: &'static str,
) -> Result<String, ToolFailure> {
    let value = arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or(ToolFailure::Invalid("setup arguments must be strings"))?;
    (!value.is_empty()
        && value.len() <= MAX_MCP_SETUP_STRING_BYTES
        && !value.bytes().any(|byte| byte == 0))
    .then_some(value.to_owned())
    .ok_or(ToolFailure::Invalid(
        "setup argument is empty or exceeds 8 KiB",
    ))
}

/// Keep task selection semantic at the MCP boundary.  This exact grammar is
/// also enforced by the authenticated daemon wire boundary: an alphanumeric
/// first byte followed by alphanumerics, underscores, or non-leading hyphens.
fn required_setup_task_name(
    arguments: &serde_json::Map<String, Value>,
) -> Result<String, ToolFailure> {
    let task_name = required_setup_string(arguments, "task_name")?;
    (task_name.len() <= 64
        && task_name.as_bytes()[0].is_ascii_alphanumeric()
        && task_name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        }))
    .then_some(task_name)
    .ok_or(ToolFailure::Invalid(
        "task_name is not a declared-task name",
    ))
}

fn required_bounded_u64(
    arguments: &serde_json::Map<String, Value>,
    key: &'static str,
    maximum: u64,
) -> Result<u64, ToolFailure> {
    let value = arguments
        .get(key)
        .and_then(Value::as_u64)
        .ok_or(ToolFailure::Invalid(
            "setup bounds must be positive integers",
        ))?;
    (value > 0 && value <= maximum)
        .then_some(value)
        .ok_or(ToolFailure::Invalid(
            "setup bound is outside its allowed range",
        ))
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

fn registry_page_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<(u64, u32), ToolFailure> {
    only_arguments(arguments, &["after", "limit"])?;
    let after = optional_u64(arguments, "after", 0)?;
    let limit = optional_u64(arguments, "limit", u64::from(MAX_MCP_REGISTRY_RECORDS))?;
    (limit > 0 && limit <= u64::from(MAX_MCP_REGISTRY_RECORDS))
        .then_some((after, limit as u32))
        .ok_or(ToolFailure::Invalid("limit must be within 1..=64"))
}
fn setup_gc_preview_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<(PathBuf, u64, u32), ToolFailure> {
    only_arguments(arguments, &["workspace", "after", "limit"])?;
    let workspace = arguments
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_MCP_SETUP_STRING_BYTES
                && !value.bytes().any(|byte| byte == 0)
        })
        .map(PathBuf::from)
        .ok_or(ToolFailure::Invalid(
            "workspace must be a non-empty bounded string",
        ))?;
    let after = optional_u64(arguments, "after", 0)?;
    let limit = optional_u64(arguments, "limit", u64::from(MAX_MCP_REGISTRY_RECORDS))?;
    if limit == 0 || limit > u64::from(MAX_MCP_REGISTRY_RECORDS) {
        return Err(ToolFailure::Invalid("limit must be within 1..=64"));
    }
    let limit = limit as u32;
    Ok((workspace, after, limit))
}
fn setup_gc_apply_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<(PathBuf, String), ToolFailure> {
    only_arguments(arguments, &["workspace", "candidate_token", "confirm"])?;
    let workspace = arguments
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_MCP_SETUP_STRING_BYTES
                && !value.bytes().any(|byte| byte == 0)
        })
        .map(PathBuf::from)
        .ok_or(ToolFailure::Invalid(
            "workspace must be a non-empty bounded string",
        ))?;
    let token = arguments
        .get("candidate_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 24 * 1024)
        .map(str::to_owned)
        .ok_or(ToolFailure::Invalid(
            "candidate_token must be bounded string",
        ))?;
    if arguments.get("confirm") != Some(&Value::Bool(true)) {
        return Err(ToolFailure::Invalid("confirm must be true"));
    }
    Ok((workspace, token))
}
fn setup_done_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<PathBuf, ToolFailure> {
    only_arguments(arguments, &["workspace", "confirm"])?;
    if arguments.get("confirm") != Some(&Value::Bool(true)) {
        return Err(ToolFailure::Invalid("confirm must be true"));
    }
    arguments
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_MCP_SETUP_STRING_BYTES
                && !value.bytes().any(|byte| byte == 0)
        })
        .map(PathBuf::from)
        .ok_or(ToolFailure::Invalid(
            "workspace must be a non-empty bounded string",
        ))
}
fn setup_adopt_arguments(
    arguments: &serde_json::Map<String, Value>,
) -> Result<SetupAdoptRequest, ToolFailure> {
    only_arguments(
        arguments,
        &[
            "workspace",
            "config",
            "policy",
            "deadline_ms",
            "output_limit",
            "confirm",
        ],
    )?;
    if arguments.get("confirm") != Some(&Value::Bool(true)) {
        return Err(ToolFailure::Invalid("confirm must be true"));
    }
    let workspace = arguments
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|v| {
            !v.is_empty() && v.len() <= MAX_MCP_SETUP_STRING_BYTES && !v.bytes().any(|b| b == 0)
        })
        .map(PathBuf::from)
        .ok_or(ToolFailure::Invalid(
            "workspace must be a non-empty bounded string",
        ))?;
    let config = arguments
        .get("config")
        .and_then(Value::as_str)
        .filter(|v| {
            !v.is_empty() && v.len() <= MAX_MCP_SETUP_STRING_BYTES && !v.bytes().any(|b| b == 0)
        })
        .map(str::to_owned)
        .ok_or(ToolFailure::Invalid(
            "config must be a non-empty bounded string",
        ))?;
    let policy = match arguments.get("policy").and_then(Value::as_str) {
        Some("refresh") => SetupPreparePolicy::Refresh,
        Some("offline") => SetupPreparePolicy::Offline,
        _ => return Err(ToolFailure::Invalid("policy must be refresh or offline")),
    };
    let deadline = required_bounded_u64(arguments, "deadline_ms", 300000)?;
    let output = required_bounded_u64(arguments, "output_limit", 8388608)?;
    Ok(SetupAdoptRequest {
        workspace,
        config,
        policy,
        deadline: std::time::Duration::from_millis(deadline),
        output_limit: output as usize,
        confirm: true,
    })
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

fn doctor_json(report: DoctorReport) -> Value {
    json!({
        "daemon": report.daemon,
        "registry": report.registry,
        "engine": report.engine,
        "client_version": report.client_version,
        "server_version": report.server_version,
    })
}

fn resource_page_json(page: RegistryResourcePage) -> Value {
    let records: Vec<Value> = page
        .records
        .into_iter()
        .map(|record| {
            json!({
                "id": record.id, "kind": record.kind, "name": record.name,
                "stack": record.stack, "generation": record.generation,
                "state": record.state, "retention": record.retention,
                "created_at": record.created_at, "last_used": record.last_used,
            })
        })
        .collect();
    json!({"next": page.next, "records": records})
}

fn setup_ensure_event_page_json(page: SetupEnsureEventPage) -> Value {
    let records: Vec<Value> = page.records.into_iter().map(|record| json!({
        "cursor": record.cursor, "at": record.at, "kind": record.kind, "detail": record.detail,
    })).collect();
    json!({"next": page.next, "records": records})
}
fn setup_gc_preview_json(page: SetupGcPreviewPage) -> Value {
    let candidates: Vec<_> = page.candidates.into_iter().map(|candidate| json!({"id": candidate.id, "name": candidate.name, "generation": candidate.generation, "token":candidate.token, "reason": candidate.reason})).collect();
    json!({"next": page.next, "candidates": candidates, "counts": {"protected_not_retired": page.counts.protected_not_retired, "protected_ambiguous_use": page.counts.protected_ambiguous_use, "protected_lease": page.counts.protected_lease, "protected_session": page.counts.protected_session, "excluded_unmanaged": page.counts.excluded_unmanaged}})
}
fn setup_reconcile_preview_json(page: SetupReconcilePreviewPage) -> Value {
    json!({"preview_only":true,"next":page.next,"records":page.records.into_iter().map(|record| json!({"id":record.id,"name":record.name,"generation":record.generation,"drift":record.drift,"repair_token":record.repair_token})).collect::<Vec<_>>()})
}
fn setup_reconcile_repair_missing_json(result: SetupReconcileMissingRepairResult) -> Value {
    json!({"repaired":result.repaired,"already_repaired":result.already_repaired})
}
fn setup_gc_apply_json(result: SetupGcApplyResult) -> Value {
    json!({"removed":result.removed,"reconciled_missing":result.reconciled_missing})
}
fn manifest_volume_gc_preview_json(page: ManifestVolumeGcPreviewPage) -> Value {
    json!({"preview_only":true,"next":page.next,"candidates":page.candidates.into_iter().map(|v| json!({"id":v.id,"name":v.name,"generation":v.generation,"token":v.token,"reason":v.reason})).collect::<Vec<_>>(),"counts":{"protected_not_retired":page.counts.protected_not_retired,"protected_policy":page.counts.protected_policy,"protected_ambiguous_use":page.counts.protected_ambiguous_use,"protected_lease":page.counts.protected_lease,"protected_session":page.counts.protected_session,"protected_intent":page.counts.protected_intent,"excluded_unmanaged":page.counts.excluded_unmanaged}})
}
fn manifest_volume_gc_apply_json(result: ManifestVolumeGcApplyResult) -> Value {
    json!({"removed":result.removed,"reconciled_missing":result.reconciled_missing})
}
fn setup_stop_retired_json(result: SetupRetiredStopResult) -> Value {
    json!({"stopped": result.stopped, "already_stopped": result.already_stopped})
}
fn setup_done_json(result: SetupDoneResult) -> Value {
    json!({"uses_completed":result.uses_completed,"resources_completed":result.resources_completed})
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

fn setup_plan_json(plan: SetupPlan) -> Value {
    let app_source = match plan.app_source {
        SetupPlanAppSource::PinnedImage { image } => {
            json!({"kind": "pinned_image", "image": image})
        }
        SetupPlanAppSource::InlineDockerfile { dockerfile_path } => {
            json!({"kind": "inline_dockerfile", "dockerfile_path": dockerfile_path})
        }
    };
    json!({
        "action": "plan",
        "applied": false,
        "source_kind": match plan.source_kind {
            SetupSourceKind::LocalFile => "local_file",
            SetupSourceKind::Https => "https",
        },
        "content_sha256": plan.content_sha256,
        "schema_version": plan.schema_version,
        "workspace": plan.workspace_root,
        "asset_root": plan.asset_root,
        "task_names": plan.task_names,
        "app_source": app_source,
    })
}

fn compose_plan_json(plan: bosn_core::ComposePlan) -> Value {
    json!({
        "action": "compose_plan",
        "applied": false,
        "version": plan.version,
        "digest": plan.digest,
        "document": plan.document,
        "normalized_json": plan.normalized_json,
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
mod tests;
