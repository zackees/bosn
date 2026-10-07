//! MCP tools for local CI (`bosn_ci_*`). Arguments are parsed eagerly into
//! typed structs (unknown fields refused); replies are the typed CI replies,
//! each kept under [`MAX_TOOL_RESPONSE_BYTES`] so an agent never receives
//! unbounded output. Long work returns a durable run ID to poll.

use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

use super::{
    LogsQuery, SubmitOptions,
    provider::{Mode, Provider, Trigger},
    reply::{JsonReply, RunView},
    wire::{RunState, RunnerAction},
};
use kernal_api::async_engine::Runtime;

use crate::{Client, Error};

/// Every CI tool response stays below this size.
pub const MAX_TOOL_RESPONSE_BYTES: usize = 64 * 1024;
/// Log pages leave room for the envelope inside the response cap.
const MAX_LOG_PAGE_BYTES: usize = 48 * 1024;
const MAX_REPORT_TAIL: usize = 20;
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);
const WAIT_POLL: Duration = Duration::from_millis(500);

/// The daemon operations the CI tools need (implemented by the MCP backend).
pub trait CiBackend {
    fn ci_submit(&mut self, options: SubmitOptions) -> Result<super::SubmitReply, Error>;
    fn ci_list(
        &mut self,
        workspace: Option<String>,
        state: Option<RunState>,
        limit: Option<usize>,
    ) -> Result<super::ListReply, Error>;
    fn ci_show(&mut self, run: String, tree: bool) -> Result<RunView, Error>;
    fn ci_logs(&mut self, query: LogsQuery) -> Result<super::LogsReply, Error>;
    fn ci_cancel(&mut self, run: String) -> Result<super::CancelReply, Error>;
    fn ci_retry(&mut self, run: String, job: Option<String>) -> Result<super::SubmitReply, Error>;
    fn ci_report(&mut self, run: String, tail: Option<usize>) -> Result<super::RunReport, Error>;
    fn ci_runners(&mut self, action: RunnerAction) -> Result<super::RunnersReply, Error>;
}

/// The real daemon behind the CI tools: a client on a caller's runtime.
pub struct ClientCi<'a> {
    runtime: &'a Runtime,
    client: &'a Client,
}
impl<'a> ClientCi<'a> {
    pub fn new(runtime: &'a Runtime, client: &'a Client) -> Self {
        Self { runtime, client }
    }
}
impl CiBackend for ClientCi<'_> {
    fn ci_submit(&mut self, options: SubmitOptions) -> Result<super::SubmitReply, Error> {
        self.runtime.run(self.client.ci_submit(options))
    }
    fn ci_list(
        &mut self,
        workspace: Option<String>,
        state: Option<RunState>,
        limit: Option<usize>,
    ) -> Result<super::ListReply, Error> {
        self.runtime
            .run(self.client.ci_list(workspace, state, limit))
    }
    fn ci_show(&mut self, run: String, tree: bool) -> Result<RunView, Error> {
        self.runtime.run(self.client.ci_show(run, tree))
    }
    fn ci_logs(&mut self, query: LogsQuery) -> Result<super::LogsReply, Error> {
        self.runtime.run(self.client.ci_logs(query))
    }
    fn ci_cancel(&mut self, run: String) -> Result<super::CancelReply, Error> {
        self.runtime.run(self.client.ci_cancel(run))
    }
    fn ci_retry(&mut self, run: String, job: Option<String>) -> Result<super::SubmitReply, Error> {
        self.runtime.run(self.client.ci_retry(run, job))
    }
    fn ci_report(&mut self, run: String, tail: Option<usize>) -> Result<super::RunReport, Error> {
        self.runtime.run(self.client.ci_report(run, tail))
    }
    fn ci_runners(&mut self, action: RunnerAction) -> Result<super::RunnersReply, Error> {
        self.runtime.run(self.client.ci_runners(action))
    }
}

/// A CI backend with no daemon: every call fails as unavailable.
pub struct Offline;
impl CiBackend for Offline {
    fn ci_submit(&mut self, _: SubmitOptions) -> Result<super::SubmitReply, Error> {
        Err(Error::ActorClosed)
    }
    fn ci_list(
        &mut self,
        _: Option<String>,
        _: Option<RunState>,
        _: Option<usize>,
    ) -> Result<super::ListReply, Error> {
        Err(Error::ActorClosed)
    }
    fn ci_show(&mut self, _: String, _: bool) -> Result<RunView, Error> {
        Err(Error::ActorClosed)
    }
    fn ci_logs(&mut self, _: LogsQuery) -> Result<super::LogsReply, Error> {
        Err(Error::ActorClosed)
    }
    fn ci_cancel(&mut self, _: String) -> Result<super::CancelReply, Error> {
        Err(Error::ActorClosed)
    }
    fn ci_retry(&mut self, _: String, _: Option<String>) -> Result<super::SubmitReply, Error> {
        Err(Error::ActorClosed)
    }
    fn ci_report(&mut self, _: String, _: Option<usize>) -> Result<super::RunReport, Error> {
        Err(Error::ActorClosed)
    }
    fn ci_runners(&mut self, _: RunnerAction) -> Result<super::RunnersReply, Error> {
        Err(Error::ActorClosed)
    }
}

/// What `bosn_ci_plan` and `bosn_ci_run` accept.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    workspace: PathBuf,
    workflow: Option<String>,
    job: Option<String>,
    provider: Option<Provider>,
    trigger: Option<Trigger>,
    mode: Option<Mode>,
    pr_number: Option<u64>,
    #[serde(default)]
    pr_title: Option<String>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    github_token: bool,
    /// `--input`, `--matrix` and `--env` (#430), validated with the plan.
    #[serde(default)]
    inputs: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    matrix: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}
impl RunArgs {
    fn options(self) -> SubmitOptions {
        SubmitOptions {
            params: super::params::RunParams {
                pr_title: self.pr_title,
                inputs: self.inputs,
                matrix: self.matrix,
                env: self.env,
            },
            workspace: self.workspace,
            provider: self.provider,
            engine: None,
            workflow: self.workflow,
            job: self.job,
            trigger: self.trigger,
            mode: self.mode,
            actor: None,
            sha: None,
            pr_number: self.pr_number,
            timeout_secs: self.timeout_secs,
            secrets: if self.github_token {
                vec!["github_token".into()]
            } else {
                Vec::new()
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    run: String,
    #[serde(default)]
    tree: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArg {
    run: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    workspace: Option<String>,
    state: Option<RunState>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsArgs {
    run: String,
    job: Option<String>,
    section: Option<String>,
    #[serde(default)]
    since_seq: u64,
    limit: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    run: String,
    deadline_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryArgs {
    run: String,
    job: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    run: String,
    tail: Option<usize>,
}

fn parse<T: DeserializeOwned>(arguments: &Map<String, Value>) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.clone()))
        .map_err(|e| format!("invalid arguments: {e}"))
}

fn daemon(error: Error) -> String {
    match error {
        Error::Ci { code, message } => format!("{code}: {message}"),
        _ => "native Bosn daemon request failed (is `bosn daemon serve` running?)".into(),
    }
}

/// Serialize a typed reply, enforcing the response cap.
fn bounded(reply: &impl JsonReply) -> Result<Value, String> {
    let text = reply.to_json();
    if text.len() > MAX_TOOL_RESPONSE_BYTES {
        return Err(format!(
            "reply is {} bytes, over the {MAX_TOOL_RESPONSE_BYTES}-byte tool limit; \
             ask for less (no tree, a smaller page or tail)",
            text.len()
        ));
    }
    serde_json::from_str(&text).map_err(|_| "could not encode reply".into())
}

/// Dispatch one `bosn_ci_*` tool. `None` when `name` is not a CI tool.
pub fn call(
    name: &str,
    arguments: &Map<String, Value>,
    backend: &mut dyn CiBackend,
) -> Option<Result<Value, String>> {
    let result = match name {
        "bosn_ci_plan" => parse::<RunArgs>(arguments)
            .and_then(|a| super::plan(&a.options()).map_err(daemon))
            .and_then(|plan| bounded(&plan)),
        "bosn_ci_run" => parse::<RunArgs>(arguments)
            .and_then(|a| backend.ci_submit(a.options()).map_err(daemon))
            .and_then(|reply| bounded(&reply)),
        "bosn_ci_status" => parse::<StatusArgs>(arguments)
            .and_then(|a| backend.ci_show(a.run, a.tree).map_err(daemon))
            .and_then(|view| bounded(&view)),
        "bosn_ci_list" => parse::<ListArgs>(arguments)
            .and_then(|a| {
                let limit = a.limit.map(|l| l.min(50)).or(Some(20));
                backend.ci_list(a.workspace, a.state, limit).map_err(daemon)
            })
            .and_then(|reply| bounded(&reply)),
        "bosn_ci_logs" => parse::<LogsArgs>(arguments)
            .and_then(|a| {
                backend
                    .ci_logs(LogsQuery {
                        run: a.run,
                        job: a.job,
                        section: a.section,
                        since_seq: a.since_seq,
                        limit: a.limit,
                        max_bytes: Some(
                            a.max_bytes
                                .unwrap_or(MAX_LOG_PAGE_BYTES)
                                .min(MAX_LOG_PAGE_BYTES),
                        ),
                    })
                    .map_err(daemon)
            })
            .and_then(|page| bounded(&page)),
        "bosn_ci_wait" => parse::<WaitArgs>(arguments).and_then(|a| {
            let deadline = Duration::from_millis(a.deadline_ms).min(MAX_WAIT);
            wait(backend, a.run, deadline)
        }),
        "bosn_ci_cancel" => parse::<RunArg>(arguments)
            .and_then(|a| backend.ci_cancel(a.run).map_err(daemon))
            .and_then(|reply| bounded(&reply)),
        "bosn_ci_retry" => parse::<RetryArgs>(arguments)
            .and_then(|a| backend.ci_retry(a.run, a.job).map_err(daemon))
            .and_then(|reply| bounded(&reply)),
        "bosn_ci_report" => parse::<ReportArgs>(arguments)
            .and_then(|a| {
                let tail = Some(a.tail.unwrap_or(MAX_REPORT_TAIL).min(MAX_REPORT_TAIL));
                backend.ci_report(a.run, tail).map_err(daemon)
            })
            .and_then(|report| bounded(&report)),
        "bosn_ci_runners" => parse::<RunnerAction>(arguments)
            .and_then(|action| backend.ci_runners(action).map_err(daemon))
            .and_then(|reply| bounded(&reply)),
        _ => return None,
    };
    Some(result)
}

/// Poll until the run is done or the deadline passes; returns the summary
/// with `finished` telling which.
fn wait(backend: &mut dyn CiBackend, run: String, deadline: Duration) -> Result<Value, String> {
    let started = std::time::Instant::now();
    loop {
        let view = backend.ci_show(run.clone(), false).map_err(daemon)?;
        let finished = view.record.state == RunState::Done;
        if finished || started.elapsed() >= deadline {
            let mut value = bounded(&view)?;
            if let Some(map) = value.as_object_mut() {
                map.insert("finished".into(), json!(finished));
            }
            return Ok(value);
        }
        std::thread::sleep(WAIT_POLL);
    }
}

fn run_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace"],
        "properties": {
            "workspace": {"type": "string", "description": "Absolute path of the Git checkout root."},
            "workflow": {"type": "string", "description": "Workflow file (default: .github/workflows/ci.yml, else the only workflow)."},
            "job": {"type": "string"},
            "provider": {"enum": ["github", "gitlab"]},
            "trigger": {"enum": ["pr", "push", "release", "workflow_dispatch", "workflow_call"], "description": "workflow_dispatch and workflow_call take inputs."},
            "mode": {"enum": ["minimal", "test", "full"]},
            "pr_number": {"type": "integer", "minimum": 1},
            "pr_title": {"type": "string", "maxLength": 4096, "description": "Synthetic PR title (trigger pr only); recorded in run identity."},
            "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 43200},
            "github_token": {"type": "boolean", "description": "Pass the daemon-owned github_token to the workflow."},
            "inputs": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Workflow inputs (trigger workflow_dispatch or workflow_call)."},
            "matrix": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Run only the matrix legs whose key has this value (act --matrix K:V)."},
            "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Extra job environment (act --env K=V); never a secret."}
        }
    })
}

fn run_ref_schema(extra: Value) -> Value {
    let mut properties =
        json!({"run": {"type": "string", "description": "Run ID from bosn_ci_run."}});
    if let (Some(map), Some(extra)) = (properties.as_object_mut(), extra.as_object()) {
        map.extend(extra.clone());
    }
    json!({"type": "object", "additionalProperties": false, "required": ["run"], "properties": properties})
}

fn annotations(read_only: bool, destructive: bool) -> Value {
    json!({"readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": read_only, "openWorldHint": false})
}

/// The CI tool descriptors for `tools/list`.
pub fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "bosn_ci_plan",
            "description": "Show what bosn_ci_run would execute for a checkout (provider, workflow, event payload, SHA, dirty), without copying anything or contacting the daemon.",
            "inputSchema": run_schema(),
            "annotations": annotations(true, false),
        }),
        json!({
            "name": "bosn_ci_run",
            "description": "Snapshot the checkout (uncommitted work included) and queue a local CI run on a daemon-owned isolated engine. Returns a durable run ID at once; identical submissions share one run. Follow with bosn_ci_wait / bosn_ci_logs / bosn_ci_report.",
            "inputSchema": run_schema(),
            "annotations": annotations(false, false),
        }),
        json!({
            "name": "bosn_ci_status",
            "description": "Read one run's state, conclusion and exit code (0 success, 1 failure/error, 2 cancelled/timed out, 3 refused/incomplete). `tree: true` adds the stage/job/step tree.",
            "inputSchema": run_ref_schema(json!({"tree": {"type": "boolean"}})),
            "annotations": annotations(true, false),
        }),
        json!({
            "name": "bosn_ci_list",
            "description": "List recent runs (newest first, at most 50) with runner status.",
            "inputSchema": {"type": "object", "additionalProperties": false, "properties": {
                "workspace": {"type": "string"},
                "state": {"enum": ["queued", "running", "done"]},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50}
            }},
            "annotations": annotations(true, false),
        }),
        json!({
            "name": "bosn_ci_logs",
            "description": "Read one bounded page of a run's log records after `since_seq` (each record exactly once across pages). Filter by `job` and `section` (`<stage>:<step id>` from bosn_ci_report). Continue with the returned `next_seq` while `more` is true.",
            "inputSchema": run_ref_schema(json!({
                "job": {"type": "string"},
                "section": {"type": "string"},
                "since_seq": {"type": "integer", "minimum": 0},
                "limit": {"type": "integer", "minimum": 1, "maximum": 5000},
                "max_bytes": {"type": "integer", "minimum": 1024, "maximum": MAX_LOG_PAGE_BYTES}
            })),
            "annotations": annotations(true, false),
        }),
        json!({
            "name": "bosn_ci_wait",
            "description": "Wait up to `deadline_ms` (at most 10 minutes) for a run to finish; returns its summary with `finished`. Call again to keep waiting.",
            "inputSchema": {"type": "object", "additionalProperties": false, "required": ["run", "deadline_ms"], "properties": {
                "run": {"type": "string"},
                "deadline_ms": {"type": "integer", "minimum": 0, "maximum": 600000}
            }},
            "annotations": annotations(true, false),
        }),
        json!({
            "name": "bosn_ci_cancel",
            "description": "Cancel a queued or running run. Its engine is removed and the run ends `cancelled`.",
            "inputSchema": run_ref_schema(json!({})),
            "annotations": annotations(false, true),
        }),
        json!({
            "name": "bosn_ci_retry",
            "description": "Run a finished run again from its frozen source snapshot (the same commit and uncommitted work), optionally only one `job`. Returns the new run, which records `retry_of`.",
            "inputSchema": run_ref_schema(json!({"job": {"type": "string", "description": "Job ID to rerun alone (from bosn_ci_report)."}})),
            "annotations": annotations(false, false),
        }),
        json!({
            "name": "bosn_ci_report",
            "description": "The agent failure summary: conclusion, exit code, the first failing job and step with only that step's last lines, skipped and unsupported jobs listed separately, and remote-only jobs (GATE-012: only GitHub can run them; never a failure) with their reasons. Never reports success for partial coverage.",
            "inputSchema": run_ref_schema(json!({"tail": {"type": "integer", "minimum": 1, "maximum": MAX_REPORT_TAIL}})),
            "annotations": annotations(true, false),
        }),
        json!({
            "name": "bosn_ci_runners",
            "description": "Runner management: list; drain/resume (stop or restart taking new runs); set_limit (live concurrency limit); prune_cache (finished runs by age/size); cache_usage (size of the machine-wide cache volume); clear_cache (remove that volume; refused while a run executes).",
            "inputSchema": {"type": "object", "additionalProperties": false, "required": ["action"], "properties": {
                "action": {"enum": ["list", "drain", "resume", "set_limit", "prune_cache", "cache_usage", "clear_cache"]},
                "limit": {"type": "integer", "minimum": 1, "maximum": 256},
                "older_than_secs": {"type": "integer", "minimum": 0},
                "max_bytes": {"type": "integer", "minimum": 0}
            }},
            "annotations": annotations(false, true),
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::{
        CancelReply, ListReply, LogsReply, RunReport, RunnerStatus, RunnersReply, SubmitReply,
        model::LogRecord,
    };

    /// A daemon double serving a fixed log of 1000 large records.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
    }
    const RECORDS: u64 = 1000;
    impl CiBackend for Fake {
        fn ci_submit(&mut self, _options: SubmitOptions) -> Result<SubmitReply, Error> {
            Err(Error::Ci {
                code: "refused".into(),
                message: "not in this test".into(),
            })
        }
        fn ci_list(
            &mut self,
            _workspace: Option<String>,
            _state: Option<RunState>,
            limit: Option<usize>,
        ) -> Result<ListReply, Error> {
            self.calls.push(format!("list {limit:?}"));
            Ok(ListReply {
                runs: Vec::new(),
                runners: RunnerStatus {
                    limit: 1,
                    running: 0,
                    queued: 0,
                    drained: false,
                    engine: "act".into(),
                    act_version: "0.2.88".into(),
                    execution_pins: None,
                    widget: crate::ci::widget::WidgetPresence::Absent,
                    spare: None,
                },
            })
        }
        fn ci_show(&mut self, _run: String, _tree: bool) -> Result<RunView, Error> {
            Err(Error::Ci {
                code: "not_found".into(),
                message: "no run".into(),
            })
        }
        fn ci_logs(&mut self, query: LogsQuery) -> Result<LogsReply, Error> {
            let budget = query.max_bytes.unwrap_or(usize::MAX);
            let mut records = Vec::new();
            let mut used = 0;
            let mut seq = query.since_seq;
            while seq < RECORDS {
                let record = LogRecord {
                    seq: seq + 1,
                    stream: "stdout".into(),
                    job: None,
                    section: None,
                    text: "x".repeat(700),
                };
                if used + 760 > budget || records.len() >= query.limit.unwrap_or(500) {
                    break;
                }
                used += 760;
                records.push(record);
                seq += 1;
            }
            Ok(LogsReply {
                run: query.run,
                records,
                next_seq: seq,
                total_records: RECORDS,
                more: seq < RECORDS,
                done: true,
            })
        }
        fn ci_cancel(&mut self, run: String) -> Result<CancelReply, Error> {
            Ok(CancelReply {
                run,
                cancelled: true,
                state: RunState::Done,
            })
        }
        fn ci_retry(&mut self, run: String, job: Option<String>) -> Result<SubmitReply, Error> {
            self.calls.push(format!("retry {run} {job:?}"));
            Ok(SubmitReply {
                run: "r2".into(),
                coalesced: false,
                queue_position: Some(0),
                record: RunView::of(crate::ci::tests::sample_record("r2"), false),
            })
        }
        fn ci_report(&mut self, _run: String, tail: Option<usize>) -> Result<RunReport, Error> {
            self.calls.push(format!("report {tail:?}"));
            Err(Error::Ci {
                code: "not_found".into(),
                message: "no run".into(),
            })
        }
        fn ci_runners(&mut self, action: RunnerAction) -> Result<RunnersReply, Error> {
            self.calls.push(format!("{action:?}"));
            Err(Error::Ci {
                code: "refused".into(),
                message: "x".into(),
            })
        }
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn every_tool_schema_is_a_closed_object_and_names_are_unique() {
        let tools = tools();
        let names: std::collections::BTreeSet<_> =
            tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), tools.len());
        for name in [
            "bosn_ci_plan",
            "bosn_ci_run",
            "bosn_ci_status",
            "bosn_ci_logs",
            "bosn_ci_wait",
            "bosn_ci_cancel",
            "bosn_ci_retry",
            "bosn_ci_report",
            "bosn_ci_runners",
        ] {
            assert!(names.contains(name), "{name}");
        }
        for tool in &tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
    }

    #[test]
    fn log_pages_stay_under_the_cap_and_return_every_record_once() {
        let mut backend = Fake::default();
        let mut since = 0u64;
        let mut seen = Vec::new();
        loop {
            let page = call(
                "bosn_ci_logs",
                &args(json!({"run": "r", "since_seq": since, "max_bytes": 1_000_000})),
                &mut backend,
            )
            .unwrap()
            .unwrap();
            assert!(page.to_string().len() <= MAX_TOOL_RESPONSE_BYTES);
            let page: LogsReply = serde_json::from_value(page).unwrap();
            seen.extend(page.records.iter().map(|r| r.seq));
            since = page.next_seq;
            if !page.more {
                break;
            }
        }
        assert_eq!(seen, (1..=RECORDS).collect::<Vec<_>>());
    }

    #[test]
    fn arguments_parse_eagerly_and_unknown_fields_are_refused() {
        let mut backend = Fake::default();
        let error = call(
            "bosn_ci_cancel",
            &args(json!({"run": "r", "force": true})),
            &mut backend,
        )
        .unwrap()
        .unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
        let error = call(
            "bosn_ci_run",
            &args(json!({"workspace": "/x", "mode": "huge"})),
            &mut backend,
        )
        .unwrap()
        .unwrap_err();
        assert!(error.contains("invalid arguments"), "{error}");
        assert!(
            call("bosn_status", &Map::new(), &mut backend).is_none(),
            "not a CI tool"
        );
        let _ = call(
            "bosn_ci_report",
            &args(json!({"run": "r", "tail": 500})),
            &mut backend,
        );
        let _ = call("bosn_ci_list", &args(json!({"limit": 9999})), &mut backend);
        let _ = call(
            "bosn_ci_runners",
            &args(json!({"action": "set_limit", "limit": 3})),
            &mut backend,
        );
        assert_eq!(
            backend.calls,
            ["report Some(20)", "list Some(50)", "SetLimit { limit: 3 }"],
            "caps applied and the runner action parsed into its variant"
        );
        let cancelled = call("bosn_ci_cancel", &args(json!({"run": "r"})), &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(cancelled["cancelled"], true);
    }

    #[test]
    fn pr_title_is_exposed_and_parsed_at_the_mcp_boundary() {
        assert_eq!(run_schema()["properties"]["pr_title"]["type"], "string");
        let options = serde_json::from_value::<RunArgs>(json!({
            "workspace": "/x", "trigger": "pr", "pr_title": "[ci-linux] proof"
        }))
        .unwrap()
        .options();
        assert_eq!(options.params.pr_title.as_deref(), Some("[ci-linux] proof"));
        options.params.validate(Trigger::Pr).unwrap();
        assert!(
            serde_json::from_value::<RunArgs>(json!({"workspace": "/x", "pr_title": 1})).is_err()
        );
    }

    #[test]
    fn run_arguments_carry_an_event_inputs_matrix_and_env() {
        let options = serde_json::from_value::<RunArgs>(json!({
            "workspace": "/x",
            "trigger": "workflow_call",
            "inputs": {"release_tag": "2.8.25", "mode": "candidate"},
            "matrix": {"target": "x86_64-unknown-linux-musl"},
            "env": {"PYTEST_ADDOPTS": "-s"}
        }))
        .unwrap()
        .options();
        assert_eq!(options.trigger, Some(Trigger::WorkflowCall));
        assert_eq!(options.params.inputs["mode"], "candidate");
        assert_eq!(options.params.matrix["target"], "x86_64-unknown-linux-musl");
        assert_eq!(options.params.env["PYTEST_ADDOPTS"], "-s");
        assert!(
            serde_json::from_value::<RunArgs>(json!({"workspace": "/x", "inputs": [1]})).is_err()
        );
    }

    #[test]
    fn retry_reruns_a_finished_run_optionally_one_job() {
        let mut backend = Fake::default();
        let reply = call(
            "bosn_ci_retry",
            &args(json!({"run": "r", "job": "build"})),
            &mut backend,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reply["run"], "r2");
        let _ = call("bosn_ci_retry", &args(json!({"run": "r"})), &mut backend);
        assert_eq!(backend.calls, ["retry r Some(\"build\")", "retry r None"]);
    }
}
