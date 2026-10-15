//! Provider-neutral run model (Run -> Group -> Job -> Section -> log records)
//! and the GitHub/act implementation of it: a parser for `act --json` lines.
//!
//! The parser is total: a malformed or unknown line is counted and kept as a
//! run-level log record, never fatal. Unknown JSON fields are ignored so a
//! newer act cannot break an older daemon.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Lifecycle of one job or section.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Queued,
    InProgress,
    Completed,
}

/// Outcome of one job or section once completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
    /// The job needs a runner bosn cannot supervise (`runs-on: macos-*`).
    Unsupported,
    /// The job can only run on GitHub (GATE-012): not run locally, never a
    /// failure and never a coverage gap.
    RemoteOnly,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Section {
    /// Provider step identity; nested composite steps are joined with `/`.
    pub id: String,
    pub name: String,
    /// `Setup`, `Pre`, `Main`, `Post` or `Complete` for GitHub.
    pub stage: String,
    pub status: ItemStatus,
    pub conclusion: Option<ItemConclusion>,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Exit code reported for a failing step, when the provider names one.
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Job {
    /// Unique per run: GitHub job x matrix leg (act's `job` field).
    pub key: String,
    /// Workflow job ID shared by matrix legs.
    pub job_id: String,
    pub name: String,
    pub matrix: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Vec<JobIdentity>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_evidence: Option<JobOutputEvidence>,
    pub status: ItemStatus,
    pub conclusion: Option<ItemConclusion>,
    /// Why the job did not run locally (a `remote_only` job's reason).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub sections: Vec<Section>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Group {
    /// GitHub: the `needs:` stage index; GitLab: the stage name.
    pub name: String,
    pub jobs: Vec<Job>,
}

/// One job declared by the workflow, from the provider's job listing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeclaredJob {
    pub stage: u32,
    pub job_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunTree {
    pub groups: Vec<Group>,
    /// Lines that were not valid provider records (counted, never fatal).
    pub malformed_lines: u64,
}

/// One persisted log record. `seq` is the run-wide, gap-free cursor space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogRecord {
    pub seq: u64,
    /// `stdout`, `stderr` or `bosn` (daemon lifecycle notes).
    pub stream: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    pub text: String,
}

/// Cut `text` to at most `max` bytes on a character boundary.
pub fn truncate_text(text: &mut String, max: usize) {
    text.truncate(text.floor_char_boundary(max));
}

/// Parse `act -l` output into declared jobs. Columns are fixed-width and
/// located from the header, so job names containing spaces still parse.
pub fn parse_act_list(output: &str) -> Vec<DeclaredJob> {
    let mut lines = output.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.find(|l| l.starts_with("Stage") && l.contains("Job ID")) else {
        return Vec::new();
    };
    let (Some(id_at), Some(name_at), Some(workflow_at)) = (
        header.find("Job ID"),
        header.find("Job name"),
        header.find("Workflow name"),
    ) else {
        return Vec::new();
    };
    lines
        .filter_map(|line| {
            let column = |from: usize, to: usize| {
                line.get(from..to.min(line.len()))
                    .map(str::trim)
                    .unwrap_or("")
            };
            let stage = column(0, id_at).parse().ok()?;
            let job_id = column(id_at, name_at);
            let name = column(name_at, workflow_at);
            (!job_id.is_empty()).then(|| DeclaredJob {
                stage,
                job_id: job_id.into(),
                name: if name.is_empty() { job_id } else { name }.into(),
            })
        })
        .collect()
}

/// The declared jobs a run executes: all of them, or only `job` when the
/// run was narrowed with `--job` (act runs just that job).
pub fn select_jobs(jobs: Vec<DeclaredJob>, job: Option<&str>) -> Vec<DeclaredJob> {
    match job {
        None => jobs,
        Some(job) => jobs.into_iter().filter(|j| j.job_id == job).collect(),
    }
}

impl RunTree {
    /// Seed the tree with every declared job, queued, grouped by stage and
    /// ordered by job ID within it: `act -l` lists a stage's jobs in Go map
    /// order, which changes from run to run (#404).
    pub fn declared(jobs: &[DeclaredJob]) -> Self {
        let mut tree = Self::default();
        let mut jobs: Vec<&DeclaredJob> = jobs.iter().collect();
        jobs.sort_by(|a, b| (a.stage, &a.job_id).cmp(&(b.stage, &b.job_id)));
        for job in jobs {
            let group = tree.group_mut(&job.stage.to_string());
            group.jobs.push(Job {
                key: job.job_id.clone(),
                job_id: job.job_id.clone(),
                name: job.name.clone(),
                matrix: None,
                identity: None,
                output_evidence: None,
                status: ItemStatus::Queued,
                conclusion: None,
                reason: None,
                sections: Vec::new(),
                started_at: None,
                completed_at: None,
                duration_ms: None,
            });
        }
        tree
    }

    fn group_mut(&mut self, name: &str) -> &mut Group {
        if let Some(i) = self.groups.iter().position(|g| g.name == name) {
            return &mut self.groups[i];
        }
        // Keep numeric stages ordered regardless of discovery order.
        let at = self
            .groups
            .iter()
            .position(|g| stage_order(&g.name) > stage_order(name))
            .unwrap_or(self.groups.len());
        self.groups.insert(
            at,
            Group {
                name: name.into(),
                jobs: Vec::new(),
            },
        );
        &mut self.groups[at]
    }

    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.groups.iter().flat_map(|g| g.jobs.iter())
    }

    fn jobs_mut(&mut self) -> impl Iterator<Item = &mut Job> {
        self.groups.iter_mut().flat_map(|g| g.jobs.iter_mut())
    }

    fn find(&self, key: &str) -> Option<(usize, usize)> {
        self.groups.iter().enumerate().find_map(|(g, group)| {
            group
                .jobs
                .iter()
                .position(|job| job.key == key)
                .map(|j| (g, j))
        })
    }

    /// Mark every unfinished job and section `cancelled`.
    pub fn cancel_unfinished(&mut self) {
        for job in self.jobs_mut() {
            for section in &mut job.sections {
                if section.status != ItemStatus::Completed {
                    section.status = ItemStatus::Completed;
                    section.conclusion = Some(ItemConclusion::Cancelled);
                }
            }
            if job.status != ItemStatus::Completed {
                job.status = ItemStatus::Completed;
                job.conclusion = Some(ItemConclusion::Cancelled);
            }
        }
    }

    /// After the provider exits normally: a job that never produced a record
    /// was skipped (job-level `if:` false, or a failed `needs:` dependency);
    /// a section that never started inside a finished job was skipped.
    pub fn settle_finished(&mut self) {
        for job in self.jobs_mut() {
            if job.status == ItemStatus::Queued {
                job.status = ItemStatus::Completed;
                job.conclusion = Some(ItemConclusion::Skipped);
            }
            if job.status == ItemStatus::InProgress {
                // The provider exited without a job result: never a pass.
                job.status = ItemStatus::Completed;
                job.conclusion = Some(ItemConclusion::Failure);
            }
            for section in &mut job.sections {
                if section.status != ItemStatus::Completed {
                    section.status = ItemStatus::Completed;
                    section.conclusion = Some(if section.first_seq.is_none() {
                        ItemConclusion::Skipped
                    } else {
                        ItemConclusion::Failure
                    });
                }
            }
        }
    }

    /// List declared steps that never started in a job that ran, as
    /// `skipped` (act prints nothing for a step whose `if:` is false). Each
    /// is placed before the next declared step that did run, else before the
    /// job's post/complete sections.
    pub fn add_skipped_steps(&mut self, declared: &super::workflow::DeclaredSteps) {
        for job in self.jobs_mut() {
            let ran = matches!(
                job.conclusion,
                Some(ItemConclusion::Success | ItemConclusion::Failure | ItemConclusion::Cancelled)
            );
            let Some(steps) = declared.get(&job.declaration_key()).filter(|_| ran) else {
                continue;
            };
            for (index, step) in steps.iter().enumerate() {
                if job
                    .sections
                    .iter()
                    .any(|s| s.stage == "Main" && s.id == step.id)
                {
                    continue;
                }
                let later = |s: &Section| {
                    s.stage == "Main" && steps[index + 1..].iter().any(|d| d.id == s.id)
                };
                let at = job
                    .sections
                    .iter()
                    .position(|s| later(s) || s.stage == "Post" || s.stage == "Complete")
                    .unwrap_or(job.sections.len());
                job.sections.insert(
                    at,
                    Section {
                        id: step.id.clone(),
                        name: step.name.clone(),
                        stage: "Main".into(),
                        status: ItemStatus::Completed,
                        conclusion: Some(ItemConclusion::Skipped),
                        first_seq: None,
                        last_seq: None,
                        duration_ms: None,
                        started_at: None,
                        completed_at: None,
                        exit_code: None,
                    },
                );
            }
        }
    }

    /// A job that GitHub alone can run (GATE-012) ran locally only as a stub
    /// that prints its reason: it is `remote_only`, with that reason. A job
    /// whose own `if:` skipped it stays skipped, as on GitHub.
    pub fn mark_remote_only(&mut self, remote_only: &std::collections::BTreeMap<String, String>) {
        for job in self.jobs_mut() {
            if job.conclusion == Some(ItemConclusion::Success)
                && let Some(reason) = remote_only.get(&job.declaration_key())
            {
                job.conclusion = Some(ItemConclusion::RemoteOnly);
                job.reason = Some(reason.clone());
            }
        }
    }

    /// The distinct reasons unsupported jobs give, in job order.
    pub fn unsupported_reasons(&self) -> Vec<String> {
        let mut reasons: Vec<String> = Vec::new();
        for job in self.jobs() {
            if job.conclusion == Some(ItemConclusion::Unsupported)
                && let Some(reason) = &job.reason
                && !reasons.contains(reason)
            {
                reasons.push(reason.clone());
            }
        }
        reasons
    }

    pub fn unsupported_jobs(&self) -> Vec<String> {
        self.jobs()
            .filter(|j| j.conclusion == Some(ItemConclusion::Unsupported))
            .map(|j| j.key.clone())
            .collect()
    }

    pub fn skipped_jobs(&self) -> Vec<String> {
        self.jobs()
            .filter(|j| j.conclusion == Some(ItemConclusion::Skipped))
            .map(|j| j.key.clone())
            .collect()
    }

    /// The first failing job and its first failing section, in tree order.
    pub fn first_failure(&self) -> Option<(&Job, Option<&Section>)> {
        self.jobs()
            .find(|j| j.conclusion == Some(ItemConclusion::Failure))
            .map(|job| {
                let section = job
                    .sections
                    .iter()
                    .find(|s| s.conclusion == Some(ItemConclusion::Failure));
                (job, section)
            })
    }
}

fn stage_order(name: &str) -> (u8, u64, String) {
    match name.parse::<u64>() {
        Ok(n) => (0, n, String::new()),
        Err(_) => (1, 0, name.into()),
    }
}

/// `workflow/job-name-2` -> `job-name-2`.
fn job_name_from_key(key: &str) -> String {
    key.split_once('/').map_or(key, |(_, name)| name).into()
}

/// Incremental `act --json` parser that folds records into a [`RunTree`]
/// and assigns every line one log record in the run's seq space.
#[derive(Debug, Default)]
pub struct ActParser {
    pub tree: RunTree,
}

impl ActParser {
    pub fn new(tree: RunTree) -> Self {
        Self { tree }
    }

    /// Fold one stdout line. Returns the log record to persist, or `None`
    /// for a line that held only bosn's end-of-output mark
    /// ([`super::flush`]): it is neither folded nor given a seq.
    pub fn feed(&mut self, seq: u64, line: &str) -> Option<LogRecord> {
        let line = line.trim_end_matches(['\r', '\n']);
        let record =
            |job: Option<String>, section: Option<String>, text: String, stream: &str| LogRecord {
                seq,
                stream: stream.into(),
                job,
                section,
                text,
            };
        let Ok(mut act) = serde_json::from_str::<ActLine>(line) else {
            if !line.trim().is_empty() {
                self.tree.malformed_lines += 1;
            }
            return Some(record(None, None, line.into(), "stdout"));
        };
        act.untrap();
        let text = act.text()?;
        let stream = act.output_stream();
        let (Some(key), Some(job_id)) = (act.job_key(), act.job_id()) else {
            return Some(record(None, None, text, stream));
        };
        let notice = Notice::of(&act.msg, act.raw_output);
        let identity = act.qualified_identity();
        let key = identity.as_ref().ok().and_then(|i| i.as_ref()).map_or_else(
            || self.tree.leg_key(&key, act.matrix.as_ref()),
            |i| identity::execution_key(i),
        );
        let valid_identity = identity.as_ref().ok().and_then(|i| i.as_deref());
        let qualified = valid_identity.is_some();
        let job = self
            .tree
            .job_for(&key, &job_id, act.matrix.as_ref(), valid_identity);
        if valid_identity.is_some() {
            job.name = job_name_from_key(act.job.as_deref().unwrap_or(&job_id));
        }
        if let Err(error) = identity {
            job.status = ItemStatus::Completed;
            job.conclusion = Some(ItemConclusion::Unsupported);
            job.reason = Some(error);
        }
        if job.status == ItemStatus::Queued {
            job.status = ItemStatus::InProgress;
        }
        job.observe_start(act.time.as_deref());
        if notice == Notice::Unsupported {
            job.mark_unsupported(unsupported_reason(&act.msg));
        }
        // An unsupported leg stays unsupported: bosn's gate step for it
        // ([`super::matrix_runner`]) ends in a successful job.
        job.observe_result(act.job_result, act.time.as_deref());
        job.observe_outputs(&act, seq, qualified);
        let section = act.step().and_then(|step| {
            let index = match job
                .sections
                .iter()
                .position(|s| s.id == step.id && s.stage == step.stage)
            {
                Some(i) => i,
                None => {
                    job.sections.push(Section {
                        name: section_name(act.step.as_deref(), &step.id),
                        id: step.id,
                        stage: step.stage,
                        status: ItemStatus::Queued,
                        conclusion: None,
                        first_seq: None,
                        last_seq: None,
                        duration_ms: None,
                        started_at: None,
                        completed_at: None,
                        exit_code: None,
                    });
                    job.sections.len() - 1
                }
            };
            let section = &mut job.sections[index];
            // Ambient noise (git probes before the step starts) stays with
            // the job, not with a step that may never run.
            let owned = notice == Notice::StepStarted
                || section.first_seq.is_some()
                || act.step_result.is_some()
                || act.raw_output;
            if owned {
                section.first_seq.get_or_insert(seq);
                section.last_seq = Some(seq);
                if section.status == ItemStatus::Queued {
                    section.status = ItemStatus::InProgress;
                }
                section.observe_start(act.time.as_deref());
            }
            if let Some(result) = act.step_result {
                section.status = ItemStatus::Completed;
                section.conclusion = Some(result.into());
                section.duration_ms = act.execution_time.map(|ns| ns / 1_000_000);
                section.observe_end(act.time.as_deref());
            }
            if let Notice::ExitCode(code) = notice {
                section.exit_code = Some(code);
            }
            owned.then(|| format!("{}:{}", section.stage, section.id))
        });
        Some(record(Some(key), section, text, stream))
    }
}

/// A section's display name: act's step name, plus the sub-path for a step
/// nested in a composite action (act repeats the parent's name for those).
fn section_name(step: Option<&str>, id: &str) -> String {
    let name = step.unwrap_or(id);
    match id.split_once('/') {
        Some((_, nested)) => format!("{name} › {nested}"),
        None => name.to_string(),
    }
}

/// One `act --json` line, parsed eagerly. Unknown fields are ignored so a
/// newer act cannot break an older daemon.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ActLine {
    #[serde(flatten)]
    output: outputs::OutputEvent,
    #[serde(default)]
    msg: String,
    time: Option<String>,
    job: Option<String>,
    #[serde(rename = "jobID")]
    job_id: Option<String>,
    #[serde(rename = "jobIdentity")]
    job_identity: Option<identity::QualifiedIdentity>,
    matrix: Option<Value>,
    stage: Option<String>,
    step: Option<String>,
    /// Workflow steps; composite steps nest (`["2", "0"]`).
    #[serde(rename = "stepID")]
    step_ids: Option<Vec<String>>,
    /// act's synthetic `--setup-job` / `--complete-job` steps.
    #[serde(rename = "stepid")]
    synthetic_step_ids: Option<Vec<String>>,
    #[serde(rename = "stepResult")]
    step_result: Option<ActResult>,
    #[serde(rename = "jobResult")]
    job_result: Option<ActResult>,
    /// Nanoseconds.
    #[serde(rename = "executionTime")]
    execution_time: Option<u64>,
    #[serde(default)]
    raw_output: bool,
    raw_stream: Option<RawStream>,
}

#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum RawStream {
    Stdout,
    Stderr,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ActResult {
    Success,
    Skipped,
    Cancelled,
    /// `failure` and anything a newer act invents: never a pass.
    #[serde(other)]
    Failure,
}

impl From<ActResult> for ItemConclusion {
    fn from(result: ActResult) -> Self {
        match result {
            ActResult::Success => Self::Success,
            ActResult::Skipped => Self::Skipped,
            ActResult::Cancelled => Self::Cancelled,
            ActResult::Failure => Self::Failure,
        }
    }
}

impl Job {
    /// The job needs a runner bosn cannot supervise; keep the first reason.
    fn mark_unsupported(&mut self, reason: Option<String>) {
        self.status = ItemStatus::Completed;
        self.conclusion = Some(ItemConclusion::Unsupported);
        if self.reason.is_none() {
            self.reason = reason;
        }
    }
}

/// Why a job's runner is unsupported, naming its label and how to allow it:
/// from act's "Skipping unsupported platform -- Try running with `-P L=...`"
/// or from bosn's matrix gate line.
fn unsupported_reason(msg: &str) -> Option<String> {
    if let Some(runner) = msg.strip_prefix(super::matrix_runner::MARK) {
        return Some(super::matrix_runner::leg_reason(runner.trim()));
    }
    let (_, rest) = msg.split_once("-P ")?;
    let (label, _) = rest.split_once('=')?;
    let label = label.trim_matches(|c: char| c == '[' || c == ']' || c.is_whitespace());
    (!label.is_empty()).then(|| bosn_core::runner_images::unsupported_message(label))
}

/// What an act message announces, classified once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Notice {
    StepStarted,
    Unsupported,
    ExitCode(i32),
    Plain,
}

impl Notice {
    /// Classify one act message. `raw_output` lines are a step's captured
    /// stdout/stderr, never act's lifecycle: nested output that quotes
    /// act's own phrases must not conclude the job (#500). The reserved
    /// bosn matrix-gate marker ([`super::matrix_runner`]) is the one
    /// exception — the gate writes it to its own stdout, so it arrives raw.
    fn of(msg: &str, raw_output: bool) -> Self {
        if raw_output {
            return if msg.starts_with(super::matrix_runner::MARK) {
                Self::Unsupported
            } else {
                Self::Plain
            };
        }
        if msg.starts_with("⭐ Run") {
            return Self::StepStarted;
        }
        if msg.contains("Skipping unsupported platform")
            || msg.starts_with(super::matrix_runner::MARK)
        {
            return Self::Unsupported;
        }
        msg.strip_prefix("exitcode '")
            .and_then(|rest| rest.split_once('\''))
            .and_then(|(code, _)| code.parse().ok())
            .map_or(Self::Plain, Self::ExitCode)
    }
}

/// A section's identity within its job.
struct StepRef {
    id: String,
    stage: String,
}

impl ActLine {
    fn output_stream(&self) -> &'static str {
        if self.raw_output && matches!(self.raw_stream, Some(RawStream::Stderr)) {
            "stderr"
        } else {
            "stdout"
        }
    }
    /// Name and announce a step as the workflow wrote it: act names an
    /// unnamed `run:` step after its script, which in the run's copy starts
    /// with bosn's end-of-output trap. The step's own output is left alone.
    fn untrap(&mut self) {
        if let Some(step) = &self.step {
            self.step = Some(super::flush::untrap(step).into_owned());
        }
        if !self.raw_output {
            self.msg = super::flush::untrap(&self.msg).into_owned();
        }
    }
    /// act pads job names for column alignment.
    fn job_key(&self) -> Option<String> {
        self.job.as_deref().map(|j| j.trim().to_string())
    }
    fn job_id(&self) -> Option<String> {
        self.job_id.as_deref().map(|j| j.trim().to_string())
    }
    /// `None` for a raw line that held only bosn's end-of-output mark.
    fn text(&self) -> Option<String> {
        if self.raw_output {
            super::flush::unmark(self.msg.trim_end_matches('\n'))
                .map(super::matrix_runner::describe)
        } else {
            Some(self.msg.clone())
        }
    }
    fn step(&self) -> Option<StepRef> {
        if let Some(ids) = self.step_ids.as_ref().filter(|ids| !ids.is_empty()) {
            return Some(StepRef {
                id: ids.join("/"),
                stage: self.stage.clone().unwrap_or_else(|| "Main".into()),
            });
        }
        let id = self.synthetic_step_ids.as_ref()?.join("/");
        let stage = match id.as_str() {
            "--setup-job" => "Setup",
            "--complete-job" => "Complete",
            _ => "Main",
        };
        Some(StepRef {
            id,
            stage: stage.into(),
        })
    }
}

#[cfg(test)]
mod leg_tests;

#[cfg(test)]
mod output_tests;

mod identity;
pub use identity::JobIdentity;
mod outputs;
pub use outputs::{JobOutputEvidence, OutputEvidenceError};
mod legs;
#[cfg(test)]
mod parser_tests;
#[cfg(test)]
mod property_tests;
mod timing;
#[cfg(test)]
mod timing_tests;
#[cfg(test)]
mod trap_tests;
