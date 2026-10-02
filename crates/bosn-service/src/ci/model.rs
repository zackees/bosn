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
    pub status: ItemStatus,
    pub conclusion: Option<ItemConclusion>,
    pub sections: Vec<Section>,
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

impl RunTree {
    /// Seed the tree with every declared job, queued, grouped by stage.
    pub fn declared(jobs: &[DeclaredJob]) -> Self {
        let mut tree = Self::default();
        for job in jobs {
            let group = tree.group_mut(&job.stage.to_string());
            group.jobs.push(Job {
                key: job.job_id.clone(),
                job_id: job.job_id.clone(),
                name: job.name.clone(),
                matrix: None,
                status: ItemStatus::Queued,
                conclusion: None,
                sections: Vec::new(),
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

    /// Find the job for an act record, materializing a matrix leg the first
    /// time it appears. A declared placeholder (key == job ID) is replaced by
    /// its first leg; later legs are added beside it in the same group.
    fn job_for(&mut self, key: &str, job_id: &str, matrix: Option<&Value>) -> &mut Job {
        if let Some((g, j)) = self.find(key) {
            return &mut self.groups[g].jobs[j];
        }
        let placeholder = self.groups.iter().enumerate().find_map(|(g, group)| {
            group
                .jobs
                .iter()
                .position(|job| job.job_id == job_id && job.key == job_id && job.matrix.is_none())
                .map(|j| (g, j))
        });
        let sibling_group = self
            .groups
            .iter()
            .position(|group| group.jobs.iter().any(|job| job.job_id == job_id));
        let job = Job {
            key: key.into(),
            job_id: job_id.into(),
            name: job_name_from_key(key),
            matrix: matrix.filter(|m| !m.is_null()).cloned(),
            status: ItemStatus::Queued,
            conclusion: None,
            sections: Vec::new(),
        };
        if let Some((g, j)) = placeholder {
            let name = self.groups[g].jobs[j].name.clone();
            self.groups[g].jobs[j] = Job {
                name: if job.matrix.is_some() { job.name } else { name },
                ..job
            };
            return &mut self.groups[g].jobs[j];
        }
        let g = match sibling_group {
            Some(g) => g,
            None => {
                let name = "0".to_string();
                self.group_mut(&name);
                self.groups.iter().position(|x| x.name == name).unwrap_or(0)
            }
        };
        self.groups[g].jobs.push(job);
        let last = self.groups[g].jobs.len() - 1;
        &mut self.groups[g].jobs[last]
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
            let Some(steps) = declared.get(&job.job_id).filter(|_| ran) else {
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
                        exit_code: None,
                    },
                );
            }
        }
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

    /// Fold one stdout line. Returns the log record to persist.
    pub fn feed(&mut self, seq: u64, line: &str) -> LogRecord {
        let line = line.trim_end_matches(['\r', '\n']);
        let record = |job: Option<String>, section: Option<String>, text: String| LogRecord {
            seq,
            stream: "stdout".into(),
            job,
            section,
            text,
        };
        let Ok(act) = serde_json::from_str::<ActLine>(line) else {
            if !line.trim().is_empty() {
                self.tree.malformed_lines += 1;
            }
            return record(None, None, line.into());
        };
        let text = act.text();
        let (Some(key), Some(job_id)) = (act.job_key(), act.job_id()) else {
            return record(None, None, text);
        };
        let notice = Notice::of(&act.msg);
        let job = self.tree.job_for(&key, &job_id, act.matrix.as_ref());
        if job.status == ItemStatus::Queued {
            job.status = ItemStatus::InProgress;
        }
        if notice == Notice::Unsupported {
            job.status = ItemStatus::Completed;
            job.conclusion = Some(ItemConclusion::Unsupported);
        }
        if let Some(result) = act.job_result {
            job.status = ItemStatus::Completed;
            job.conclusion = Some(result.into());
        }
        let section = act.step().and_then(|step| {
            let index = match job
                .sections
                .iter()
                .position(|s| s.id == step.id && s.stage == step.stage)
            {
                Some(i) => i,
                None => {
                    job.sections.push(Section {
                        name: act.step.clone().unwrap_or_else(|| step.id.clone()),
                        id: step.id,
                        stage: step.stage,
                        status: ItemStatus::Queued,
                        conclusion: None,
                        first_seq: None,
                        last_seq: None,
                        duration_ms: None,
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
            }
            if let Some(result) = act.step_result {
                section.status = ItemStatus::Completed;
                section.conclusion = Some(result.into());
                section.duration_ms = act.execution_time.map(|ns| ns / 1_000_000);
            }
            if let Notice::ExitCode(code) = notice {
                section.exit_code = Some(code);
            }
            owned.then(|| format!("{}:{}", section.stage, section.id))
        });
        record(Some(key), section, text)
    }
}

/// One `act --json` line, parsed eagerly. Unknown fields are ignored so a
/// newer act cannot break an older daemon.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ActLine {
    #[serde(default)]
    msg: String,
    job: Option<String>,
    #[serde(rename = "jobID")]
    job_id: Option<String>,
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

/// What an act message announces, classified once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Notice {
    StepStarted,
    Unsupported,
    ExitCode(i32),
    Plain,
}

impl Notice {
    fn of(msg: &str) -> Self {
        if msg.starts_with("⭐ Run") {
            return Self::StepStarted;
        }
        if msg.contains("Skipping unsupported platform") {
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
    /// act pads job names for column alignment.
    fn job_key(&self) -> Option<String> {
        self.job.as_deref().map(|j| j.trim().to_string())
    }
    fn job_id(&self) -> Option<String> {
        self.job_id.as_deref().map(|j| j.trim().to_string())
    }
    fn text(&self) -> String {
        if self.raw_output {
            self.msg.trim_end_matches('\n').to_string()
        } else {
            self.msg.clone()
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
mod tests {
    use super::*;

    const FIXTURE: &str =
        include_str!("../../tests/fixtures/act/act-0.2.88-matrix-needs-failure.jsonl");
    const LIST: &str = "Stage  Job ID  Job name  Workflow name  Workflow file  Events\n\
                        0      a       a         spike          ci.yml         push  \n\
                        1      b       b         spike          ci.yml         push  \n\
                        1      off     Off job   spike          ci.yml         push  \n";

    fn parse(list: &str, lines: &str) -> (ActParser, Vec<LogRecord>) {
        let mut parser = ActParser::new(RunTree::declared(&parse_act_list(list)));
        let records = lines
            .lines()
            .enumerate()
            .map(|(i, line)| parser.feed(i as u64 + 1, line))
            .collect();
        (parser, records)
    }

    #[test]
    fn act_list_columns_tolerate_spaces_in_names() {
        let jobs = parse_act_list(LIST);
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[2].name, "Off job");
        assert_eq!(jobs[1].stage, 1);
        assert!(parse_act_list("no header here").is_empty());
    }

    #[test]
    fn fixture_folds_matrix_legs_needs_stages_and_failure() {
        let (mut parser, records) = parse(LIST, FIXTURE);
        parser.tree.settle_finished();
        let tree = &parser.tree;
        assert_eq!(tree.malformed_lines, 0);
        assert_eq!(
            tree.groups
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            ["0", "1"]
        );
        let legs: Vec<_> = tree.groups[0].jobs.iter().map(|j| j.key.as_str()).collect();
        assert_eq!(
            legs,
            ["spike/a-1", "spike/a-2"],
            "matrix legs share jobID a"
        );
        assert!(
            tree.groups[0]
                .jobs
                .iter()
                .all(|j| j.conclusion == Some(ItemConclusion::Success))
        );
        let (job, section) = tree.first_failure().expect("failure");
        assert_eq!(job.key, "spike/b");
        let section = section.expect("failing step");
        assert_eq!((section.id.as_str(), section.stage.as_str()), ("1", "Main"));
        assert_eq!(section.exit_code, Some(3));
        assert_eq!(tree.skipped_jobs(), ["off"], "declared but never run");
        // Every record that names a section lies inside that section's range.
        for record in records.iter().filter(|r| r.section.is_some()) {
            let job = tree
                .jobs()
                .find(|j| Some(&j.key) == record.job.as_ref())
                .unwrap();
            let s = job
                .sections
                .iter()
                .find(|s| Some(format!("{}:{}", s.stage, s.id)) == record.section)
                .unwrap();
            assert!(s.first_seq.unwrap() <= record.seq && record.seq <= s.last_seq.unwrap());
        }
    }

    #[test]
    fn sections_never_overlap_within_a_job_property() {
        // Property over every prefix of the fixture and a shuffled-by-job
        // interleaving: ranges inside one job are disjoint.
        let lines: Vec<&str> = FIXTURE.lines().collect();
        for cut in 1..=lines.len() {
            let (parser, _) = parse(LIST, &lines[..cut].join("\n"));
            for job in parser.tree.jobs() {
                let mut ranges: Vec<_> = job
                    .sections
                    .iter()
                    .filter_map(|s| Some((s.first_seq?, s.last_seq?)))
                    .collect();
                ranges.sort();
                for pair in ranges.windows(2) {
                    assert!(pair[0].1 < pair[1].0, "overlap in {}: {pair:?}", job.key);
                }
            }
        }
    }

    #[test]
    fn unsupported_skip_unknown_fields_and_malformed_lines() {
        let list = "Stage  Job ID  Job name  Workflow name  Workflow file  Events\n\
                    0      lin     lin       w              ci.yml         push\n\
                    0      mac     mac       w              ci.yml         push\n";
        let lines = [
            r#"{"job":"w/mac","jobID":"mac","level":"info","msg":"🚧  Skipping unsupported platform -- Try running with `-P macos-latest=...`","future":{"x":1}}"#,
            r#"{"job":"w/lin","jobID":"lin","msg":"⭐ Run Main a","stage":"Main","step":"a","stepID":["0"]}"#,
            r#"{"job":"w/lin","jobID":"lin","msg":"noise","stage":"Main","step":"b","stepID":["1"]}"#,
            r#"{"job":"w/lin","jobID":"lin","msg":"  ✅  Success - Main a","stage":"Main","stepID":["0"],"stepResult":"success","executionTime":2000000}"#,
            r#"{"job":"w/lin","jobID":"lin","msg":"⭐ Run Main composite","stage":"Main","stepID":["2","0"]}"#,
            r#"{"job":"w/lin","jobID":"lin","msg":"🏁  Job succeeded","jobResult":"success"}"#,
            r#"{"truncated":"#,
            "",
        ];
        let (mut parser, records) = parse(list, &lines.join("\n"));
        parser.tree.settle_finished();
        assert_eq!(
            parser.tree.malformed_lines, 1,
            "empty lines are not malformed"
        );
        assert_eq!(parser.tree.unsupported_jobs(), ["w/mac"]);
        let lin = parser.tree.jobs().find(|j| j.key == "w/lin").unwrap();
        let by_id = |id: &str| lin.sections.iter().find(|s| s.id == id).unwrap();
        assert_eq!(by_id("0").duration_ms, Some(2));
        assert_eq!(by_id("1").conclusion, Some(ItemConclusion::Skipped));
        assert_eq!(by_id("2/0").conclusion, Some(ItemConclusion::Failure));
        assert_eq!(records[2].section, None, "pre-start noise is not a section");
        assert_eq!(records[6].text, r#"{"truncated":"#);
    }

    #[test]
    fn cancellation_marks_every_unfinished_item() {
        let lines: Vec<&str> = FIXTURE.lines().take(20).collect();
        let (mut parser, _) = parse(LIST, &lines.join("\n"));
        parser.tree.cancel_unfinished();
        for job in parser.tree.jobs() {
            assert_eq!(job.status, ItemStatus::Completed);
            assert!(job.conclusion.is_some());
            assert!(
                job.sections
                    .iter()
                    .all(|s| s.status == ItemStatus::Completed)
            );
        }
        assert!(
            parser
                .tree
                .jobs()
                .any(|j| j.conclusion == Some(ItemConclusion::Cancelled))
        );
    }
}
