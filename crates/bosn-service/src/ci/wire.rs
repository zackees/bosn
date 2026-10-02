//! CI wire types: requests, run records and conclusions, shared by the
//! daemon, the CLI, MCP and the Python client.

use std::{path::Path, time::Duration};

use serde::{Deserialize, Serialize};

use super::{
    model::RunTree,
    provider::{self, Mode, Provider, Trigger},
    scheduler::RunKey,
};

pub const SCHEMA_VERSION: u32 = 1;
/// Default per-call caps; agents never receive unbounded output.
pub const DEFAULT_LOG_PAGE_RECORDS: usize = 500;
pub const MAX_LOG_PAGE_BYTES: usize = 256 * 1024;
pub const DEFAULT_LOG_PAGE_BYTES: usize = 48 * 1024;
pub const DEFAULT_REPORT_TAIL: usize = 40;
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_RUN_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);

vocabulary!(
    /// Where a run is in its life.
    RunState, "state" { Queued => "queued", Running => "running", Done => "done" }
);

vocabulary!(
    /// Run conclusion. Exit codes: success 0, failure/error 1,
    /// cancelled/timed_out 2, refused/incomplete 3.
    Conclusion, "conclusion" {
        Success => "success",
        Failure => "failure",
        Cancelled => "cancelled",
        TimedOut => "timed_out",
        /// Some jobs could not run here, or none succeeded; never a pass.
        Incomplete => "incomplete",
        Refused => "refused",
        /// The engine or its cleanup failed; never a pass.
        Error => "error",
    }
);

impl Conclusion {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failure | Self::Error => 1,
            Self::Cancelled | Self::TimedOut => 2,
            Self::Refused | Self::Incomplete => 3,
        }
    }
}

/// A submission. Built by [`crate::Client::ci_submit`] after it snapshots the
/// workspace into the daemon's staging area.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub staging: String,
    pub workspace: String,
    pub provider: Provider,
    pub engine: String,
    pub workflow: String,
    pub job: Option<String>,
    pub trigger: Trigger,
    pub mode: Mode,
    pub actor: String,
    pub sha: String,
    pub branch: Option<String>,
    pub tree_digest: String,
    pub dirty: bool,
    pub origin: Option<String>,
    pub pr_number: Option<u64>,
    pub timeout_secs: Option<u64>,
    /// Opt-in daemon-owned secrets by name (only `github_token`).
    #[serde(default)]
    pub secrets: Vec<String>,
}

impl SubmitRequest {
    /// Semantic checks the daemon applies before touching any file. The
    /// staging directory itself is checked by the runtime.
    pub fn validate(&self) -> Result<(), CiError> {
        let refuse = |m: &str| Err(CiError::refused(m));
        if !valid_uuid(&self.staging) {
            return refuse("invalid staging ID");
        }
        if !valid_sha(&self.sha) || !valid_hex(&self.tree_digest, 64) {
            return refuse("invalid SHA or tree digest");
        }
        provider::require_supported(self.provider).map_err(CiError::refused)?;
        if self.engine != "act" {
            return refuse("only the act engine is supported");
        }
        if !Path::new(&self.workspace).is_absolute() || self.workspace.len() > 4096 {
            return refuse("workspace must be an absolute path");
        }
        if !valid_workflow(&self.workflow) {
            return refuse("workflow must be a file under .github/workflows/");
        }
        if self.job.as_deref().is_some_and(|j| !valid_job(j)) {
            return refuse("invalid job ID");
        }
        if self.actor.is_empty() || self.actor.len() > 200 {
            return refuse("invalid actor");
        }
        if self.secrets.len() > 4
            || self
                .secrets
                .iter()
                .any(|s| crate::secrets::secret_env_name(s).is_none())
        {
            return refuse("unknown secret (only github_token is supported)");
        }
        provider::validate(self.trigger, self.mode, self.dirty).map_err(CiError::refused)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout_secs
            .map_or(DEFAULT_RUN_TIMEOUT, Duration::from_secs)
            .min(MAX_RUN_TIMEOUT)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerAction {
    List,
    Drain,
    Resume,
    SetLimit {
        limit: usize,
    },
    PruneCache {
        older_than_secs: Option<u64>,
        max_bytes: Option<u64>,
    },
}

/// Every CI operation. Each maps to one typed handler; there is no generic
/// command surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum CiRequest {
    Submit {
        request: SubmitRequest,
    },
    List {
        workspace: Option<String>,
        state: Option<RunState>,
        limit: Option<usize>,
    },
    Show {
        run: String,
        tree: Option<bool>,
    },
    Logs {
        run: String,
        job: Option<String>,
        section: Option<String>,
        since_seq: Option<u64>,
        limit: Option<usize>,
        max_bytes: Option<usize>,
    },
    Cancel {
        run: String,
    },
    Retry {
        run: String,
        job: Option<String>,
    },
    Report {
        run: String,
        tail: Option<usize>,
    },
    Runners {
        action: RunnerAction,
    },
    /// A widget process starting (`explicit` = typed by the user).
    WidgetHello {
        pid: u32,
        session: String,
        explicit: bool,
    },
    /// The widget's heartbeat; the reply carries its pending commands.
    WidgetPoll {
        pid: u32,
    },
    /// A deliberate quit from the widget's menu.
    WidgetDismiss {
        session: String,
    },
    /// Queue a command for the widget (from the dashboard or `bosn ui`).
    WidgetCommand {
        command: super::widget::WidgetCommand,
    },
    /// A single-use dashboard link (only from the owner-only socket).
    UiGrant {
        path: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunRecord {
    pub schema_version: u32,
    pub id: String,
    pub provider: Provider,
    pub engine: String,
    pub workspace: String,
    pub repository: String,
    pub sha: String,
    pub branch: Option<String>,
    /// Tree digest when the run used uncommitted work (`sha + dirty`).
    pub dirty: Option<String>,
    pub tree_digest: String,
    pub workflow: String,
    pub job: Option<String>,
    pub trigger: Trigger,
    pub mode: Mode,
    pub actor: String,
    pub event: String,
    pub payload_sha256: String,
    pub state: RunState,
    pub conclusion: Option<Conclusion>,
    pub reason: Option<String>,
    pub created_at: f64,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub act_exit_code: Option<i32>,
    pub cleanup: Option<String>,
    pub engine_id: Option<String>,
    pub act_version: String,
    pub runner_image: String,
    pub submitters: u32,
    pub retry_of: Option<String>,
    pub timeout_secs: u64,
    /// Opt-in secret names; values never leave the daemon's secret store.
    #[serde(default)]
    pub secrets: Vec<String>,
    pub log_records: u64,
    #[serde(default)]
    pub tree: RunTree,
}

impl RunRecord {
    /// A queued record for a validated submission.
    pub fn queued(id: String, request: &SubmitRequest, event: &str, payload: &[u8]) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            id,
            provider: request.provider,
            engine: request.engine.clone(),
            workspace: request.workspace.clone(),
            repository: provider::repository(request.origin.as_deref()),
            sha: request.sha.clone(),
            branch: request.branch.clone(),
            dirty: request.dirty.then(|| request.tree_digest.clone()),
            tree_digest: request.tree_digest.clone(),
            workflow: request.workflow.clone(),
            job: request.job.clone(),
            trigger: request.trigger,
            mode: request.mode,
            actor: request.actor.clone(),
            event: event.into(),
            payload_sha256: sha256_hex(payload),
            state: RunState::Queued,
            conclusion: None,
            reason: None,
            created_at: super::lifecycle::now_seconds(),
            started_at: None,
            finished_at: None,
            act_exit_code: None,
            cleanup: None,
            engine_id: None,
            act_version: super::engine::ACT_VERSION.into(),
            runner_image: super::engine::RUNNER_IMAGE.into(),
            submitters: 1,
            retry_of: None,
            timeout_secs: request.timeout().as_secs(),
            secrets: request.secrets.clone(),
            log_records: 0,
            tree: RunTree::default(),
        }
    }

    /// A queued re-run of a finished run, optionally narrowed to one job.
    pub fn retry(&self, id: String, job: Option<String>) -> Self {
        Self {
            id,
            job: job.or_else(|| self.job.clone()),
            state: RunState::Queued,
            conclusion: None,
            reason: None,
            created_at: super::lifecycle::now_seconds(),
            started_at: None,
            finished_at: None,
            act_exit_code: None,
            cleanup: None,
            engine_id: None,
            submitters: 1,
            retry_of: Some(self.id.clone()),
            log_records: 0,
            tree: RunTree::default(),
            ..self.clone()
        }
    }

    /// Mark a record a previous daemon left unfinished.
    /// The act cache-server namespace: one store per repository identity
    /// (the origin repository, else the checkout path).
    pub fn cache_namespace(&self) -> String {
        let identity = if self.repository == super::provider::LOCAL_REPOSITORY {
            &self.workspace
        } else {
            &self.repository
        };
        sha256_hex(identity.as_bytes())[..16].to_string()
    }

    /// Mark the run done. Unfinished jobs and steps are cancelled.
    pub fn finish(&mut self, conclusion: Conclusion, reason: Option<String>) {
        self.state = RunState::Done;
        self.conclusion = Some(conclusion);
        self.reason = reason;
        self.finished_at = Some(super::lifecycle::now_seconds());
        self.tree.cancel_unfinished();
    }

    pub(crate) fn key(&self) -> RunKey {
        RunKey {
            sha: self.sha.clone(),
            dirty: self.dirty.clone(),
            workflow: self.workflow.clone(),
            job: self.job.clone(),
            trigger: self.trigger.as_str().into(),
            mode: self.mode.as_str().into(),
            provider: self.provider.as_str().into(),
            engine: self.engine.clone(),
            payload_sha256: self.payload_sha256.clone(),
            timeout_secs: self.timeout_secs,
            secrets: self.secrets.clone(),
        }
    }
}

#[derive(Debug)]
pub struct CiError {
    pub code: &'static str,
    pub message: String,
}
impl CiError {
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new("refused", message)
    }
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl From<CiError> for crate::Error {
    fn from(error: CiError) -> Self {
        Self::Ci {
            code: error.code.into(),
            message: error.message,
        }
    }
}

impl std::fmt::Display for CiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub(crate) fn valid_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(part, len)| valid_hex(part, len))
}

pub(crate) fn valid_sha(value: &str) -> bool {
    valid_hex(value, 40)
}

pub(crate) fn valid_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A relative workflow file under `.github/workflows/`, no traversal.
pub(crate) fn valid_workflow(value: &str) -> bool {
    valid_name(value, 512)
        && value.starts_with(".github/workflows/")
        && !value.split('/').any(|part| part.is_empty() || part == "..")
}

/// A job ID act receives as an argument value: never option-shaped.
pub(crate) fn valid_job(value: &str) -> bool {
    valid_name(value, 128) && !value.starts_with('-')
}

pub(crate) fn valid_name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':'))
}

/// A fresh random v4 UUID (lowercase).
pub async fn new_uuid() -> Result<String, CiError> {
    let bytes = kernal_api::random::SecureRandom::new(1, Duration::from_secs(3))
        .map_err(|_| CiError::new("internal", "randomness unavailable"))?
        .bytes(16)
        .await
        .map_err(|_| CiError::new("internal", "randomness unavailable"))?;
    Ok(crate::uuid(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = kernal_api::hash::Sha256Hasher::new();
    hasher.update(bytes);
    hasher.finalize().to_hex()
}
