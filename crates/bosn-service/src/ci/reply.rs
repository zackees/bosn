//! Typed replies, one per [`super::CiRequest`] variant. The daemon serializes
//! these and clients deserialize them eagerly at the boundary, so no caller
//! probes untyped JSON.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    model::{ItemConclusion, ItemStatus, LogRecord},
    provider::{Mode, Provider, Trigger},
    wire::{Conclusion, RunRecord, RunState},
};

/// Any typed reply as one line of JSON (`--json` output), without callers
/// needing serde themselves.
pub trait JsonReply {
    fn to_json(&self) -> String;
}
impl<T: Serialize> JsonReply for T {
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The daemon's answer to a widget request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WidgetReply {
    pub presence: super::widget::WidgetPresence,
    /// False when an auto-launched widget should exit (dismissed session).
    pub allowed: bool,
    pub commands: Vec<super::widget::WidgetCommand>,
}

/// A single-use link that signs a browser in to the dashboard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UiGrantReply {
    pub url: String,
}

/// A refused or failed request. `code` is stable (`refused`, `not_found`,
/// `invalid_request`, `internal`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ErrorReply {
    pub code: String,
    pub message: String,
}

#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct JobCounts {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
}

/// A run as clients see it: the record plus fields derived from it. Listings
/// omit the job tree (`record.tree` is then empty).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunView {
    #[serde(flatten)]
    pub record: RunRecord,
    pub jobs: JobCounts,
    pub exit_code: Option<i32>,
}

impl RunView {
    pub fn of(mut record: RunRecord, with_tree: bool) -> Self {
        let jobs = JobCounts {
            total: record.tree.jobs().count(),
            completed: record
                .tree
                .jobs()
                .filter(|j| j.status == ItemStatus::Completed)
                .count(),
            failed: record
                .tree
                .jobs()
                .filter(|j| j.conclusion == Some(ItemConclusion::Failure))
                .count(),
        };
        if !with_tree {
            record.tree = Default::default();
        }
        Self {
            exit_code: record.conclusion.map(Conclusion::exit_code),
            jobs,
            record,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitReply {
    pub run: String,
    /// True when an identical run was already queued or running.
    pub coalesced: bool,
    pub queue_position: Option<usize>,
    pub record: RunView,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunnerStatus {
    pub limit: usize,
    pub running: usize,
    pub queued: usize,
    pub drained: bool,
    pub engine: String,
    pub act_version: String,
    /// Whether the desktop widget is running.
    pub widget: super::widget::WidgetPresence,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListReply {
    pub runs: Vec<RunView>,
    pub runners: RunnerStatus,
}

/// Parameters of one bounded log read (client side).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogsQuery {
    pub run: String,
    pub job: Option<String>,
    pub section: Option<String>,
    pub since_seq: u64,
    pub limit: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogsReply {
    pub run: String,
    pub records: Vec<LogRecord>,
    /// Cursor for the next call (`since_seq`).
    pub next_seq: u64,
    pub total_records: u64,
    /// More records are available now.
    pub more: bool,
    /// The run finished: no further records will appear.
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CancelReply {
    pub run: String,
    pub cancelled: bool,
    pub state: RunState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunnersReply {
    pub runners: RunnerStatus,
    pub pruned_runs: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FailureReport {
    pub job: String,
    pub job_name: String,
    /// `<stage>:<step id>`, usable as `ci logs --step`.
    pub section: Option<String>,
    pub step: Option<String>,
    pub exit_code: Option<i32>,
    /// The last lines of that one step only.
    pub tail: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobOutcomes {
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub skipped: Vec<String>,
    pub unsupported: Vec<String>,
}

/// The `ci report --json` agent contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunReport {
    pub schema_version: u32,
    pub run: String,
    pub state: RunState,
    pub conclusion: Option<Conclusion>,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub sha: String,
    pub dirty: Option<String>,
    pub workflow: String,
    pub trigger: Trigger,
    pub mode: Mode,
    pub actor: String,
    pub cleanup: Option<String>,
    pub first_failure: Option<FailureReport>,
    pub jobs: JobOutcomes,
    /// False when any job needed a runner bosn cannot supervise.
    pub coverage_complete: bool,
    pub ui_url: Option<String>,
    pub logs_command: String,
}

/// What `bosn ci run` would execute (`bosn ci plan`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Plan {
    pub schema_version: u32,
    pub workspace: PathBuf,
    pub provider: Provider,
    pub engine: String,
    pub workflow: String,
    pub job: Option<String>,
    pub trigger: Trigger,
    pub mode: Mode,
    pub event: String,
    /// The provider event payload, as act receives it.
    pub payload: Value,
    pub repository: String,
    pub sha: String,
    pub branch: Option<String>,
    pub dirty: bool,
    pub actor: String,
}
