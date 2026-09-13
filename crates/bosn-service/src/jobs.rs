//! Bounded daemon-owned admission policy. Engine execution is deliberately
//! outside this module: only validated semantic jobs may reach that layer.

use std::collections::{BTreeMap, VecDeque};

pub const DEFAULT_MAX_LOG_RECORDS: usize = 5_000;
/// Keep a complete page well below the daemon frame cap, including protobuf
/// overhead. Engine adapters must split output into records at this boundary.
pub const MAX_LOG_LINE_BYTES: usize = 2_048;
pub const MAX_LOG_PAGE_RECORDS: usize = 256;
pub const MAX_LOG_PAGE_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
}
impl JobState {
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Superseded
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub id: u64,
    pub workspace: String,
    pub stack: String,
    pub digest: String,
    pub state: JobState,
    pub error: Option<String>,
    pub log_start: u64,
    logs: VecDeque<(u64, String)>,
}
impl Job {
    fn key(&self) -> (String, String) {
        (self.workspace.clone(), self.stack.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Submission {
    Started(u64),
    Queued(u64),
    Joined(u64),
    Superseded { job: u64, replacement: u64 },
}
#[derive(Debug)]
pub enum JobError {
    Unknown,
    Finished,
    Closing,
    LogTooLarge,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogPage {
    pub retained_from: u64,
    pub next: u64,
    pub gap: bool,
    pub records: Vec<(u64, String)>,
}

#[derive(Default)]
struct Slot {
    active: Option<u64>,
    pending: Option<u64>,
}
pub struct Jobs {
    max_running: usize,
    max_logs: usize,
    next: u64,
    running: usize,
    closing: bool,
    jobs: BTreeMap<u64, Job>,
    slots: BTreeMap<(String, String), Slot>,
    queue: VecDeque<u64>,
    /// IDs which have just transitioned from queued to running.  The daemon
    /// actor drains this handoff and attaches the semantic executor; the
    /// scheduler itself never knows about Docker or process ownership.
    started: VecDeque<u64>,
}

impl Jobs {
    pub fn new(max_running: usize) -> Self {
        Self {
            max_running: max_running.max(1),
            max_logs: DEFAULT_MAX_LOG_RECORDS,
            next: 1,
            ..Self::default()
        }
    }
    pub fn submit(
        &mut self,
        workspace: &str,
        stack: &str,
        digest: &str,
    ) -> Result<Submission, JobError> {
        if self.closing {
            return Err(JobError::Closing);
        }
        let key = (workspace.into(), stack.into());
        let (active, pending) = {
            let s = self.slots.entry(key.clone()).or_default();
            (s.active, s.pending)
        };
        if let Some(id) = active {
            let j = &self.jobs[&id];
            if j.digest == digest && j.state != JobState::Cancelling {
                return Ok(Submission::Joined(id));
            }
        }
        if let Some(id) = pending {
            if self.jobs[&id].digest == digest {
                return Ok(Submission::Joined(id));
            }
            self.finish(id, JobState::Superseded, Some("superseded".into()));
        }
        let id = self.next;
        self.next += 1;
        let state = JobState::Queued;
        self.jobs.insert(
            id,
            Job {
                id,
                workspace: workspace.into(),
                stack: stack.into(),
                digest: digest.into(),
                state,
                error: None,
                log_start: 0,
                logs: VecDeque::new(),
            },
        );
        if active.is_some() {
            self.slots.get_mut(&key).unwrap().pending = Some(id);
            Ok(Submission::Queued(id))
        } else {
            self.slots.get_mut(&key).unwrap().active = Some(id);
            self.queue.push_back(id);
            self.pump();
            Ok(if self.jobs[&id].state == JobState::Running {
                Submission::Started(id)
            } else {
                Submission::Queued(id)
            })
        }
    }
    fn pump(&mut self) {
        if self.closing {
            return;
        }
        while self.running < self.max_running {
            let Some(id) = self.queue.pop_front() else {
                return;
            };
            if self.jobs[&id].state != JobState::Queued {
                continue;
            }
            self.jobs.get_mut(&id).unwrap().state = JobState::Running;
            self.running += 1;
            self.started.push_back(id);
        }
    }
    /// Drain newly admitted running jobs exactly once.
    pub fn take_started(&mut self) -> Vec<u64> {
        self.started.drain(..).collect()
    }
    pub fn cancel(&mut self, id: u64) -> Result<(), JobError> {
        let state = self.jobs.get(&id).ok_or(JobError::Unknown)?.state;
        if state.terminal() {
            return Err(JobError::Finished);
        }
        if state == JobState::Running {
            self.jobs.get_mut(&id).unwrap().state = JobState::Cancelling;
            return Ok(());
        }
        self.finish(id, JobState::Cancelled, Some("cancelled".into()));
        Ok(())
    }
    pub fn job(&self, id: u64) -> Result<Job, JobError> {
        self.jobs.get(&id).cloned().ok_or(JobError::Unknown)
    }
    pub fn settle(&mut self, id: u64, ok: bool) -> Result<(), JobError> {
        self.settle_with_error(id, ok, None)
    }
    /// Settle a running operation, retaining a bounded diagnostic only for a
    /// failed terminal state.  A cancellation always wins a concurrently
    /// arriving successful or failed executor result.
    pub fn settle_with_error(
        &mut self,
        id: u64,
        ok: bool,
        error: Option<String>,
    ) -> Result<(), JobError> {
        let state = self.jobs.get(&id).ok_or(JobError::Unknown)?.state;
        if state.terminal() {
            return Err(JobError::Finished);
        }
        let terminal = if state == JobState::Cancelling {
            JobState::Cancelled
        } else if ok {
            JobState::Succeeded
        } else {
            JobState::Failed
        };
        self.finish(
            id,
            terminal,
            (terminal == JobState::Failed).then_some(error).flatten(),
        );
        Ok(())
    }
    /// Stop admission and make every non-running queued job terminal.  The
    /// daemon separately cancels direct child owners for running jobs before
    /// awaiting their completion/reap notices.
    pub fn close(&mut self) {
        self.closing = true;
        let queued: Vec<u64> = self
            .jobs
            .iter()
            .filter_map(|(&id, job)| (job.state == JobState::Queued).then_some(id))
            .collect();
        for id in queued {
            self.finish(id, JobState::Cancelled, Some("daemon stopping".into()));
        }
    }
    fn finish(&mut self, id: u64, state: JobState, error: Option<String>) {
        let key = self.jobs[&id].key();
        let was_running = matches!(
            self.jobs[&id].state,
            JobState::Running | JobState::Cancelling
        );
        let j = self.jobs.get_mut(&id).unwrap();
        j.state = state;
        j.error = error;
        if was_running {
            self.running -= 1
        }
        let s = self.slots.get_mut(&key).unwrap();
        if s.active == Some(id) {
            s.active = s.pending.take();
            if let Some(next) = s.active {
                self.queue.push_back(next)
            }
        } else if s.pending == Some(id) {
            s.pending = None
        }
        self.pump();
    }
    pub fn log(&mut self, id: u64, line: String) -> Result<u64, JobError> {
        if line.len() > MAX_LOG_LINE_BYTES {
            return Err(JobError::LogTooLarge);
        }
        let j = self.jobs.get_mut(&id).ok_or(JobError::Unknown)?;
        let cursor = j.log_start + j.logs.len() as u64;
        j.logs.push_back((cursor, line));
        if j.logs.len() > self.max_logs {
            j.logs.pop_front();
            j.log_start += 1
        }
        Ok(cursor)
    }
    pub fn logs(&self, id: u64, after: u64) -> Result<(u64, Vec<(u64, String)>), JobError> {
        let j = self.jobs.get(&id).ok_or(JobError::Unknown)?;
        Ok((
            j.log_start,
            j.logs
                .iter()
                .filter(|(n, _)| *n >= after.max(j.log_start))
                .cloned()
                .collect(),
        ))
    }
    pub fn log_page(&self, id: u64, after: u64, limit: usize) -> Result<LogPage, JobError> {
        let j = self.jobs.get(&id).ok_or(JobError::Unknown)?;
        let begin = after.max(j.log_start);
        let mut page_bytes = 0;
        let mut records = Vec::new();
        for (cursor, line) in j
            .logs
            .iter()
            .filter(|(n, _)| *n >= begin)
            .take(limit.min(MAX_LOG_PAGE_RECORDS))
        {
            if page_bytes + line.len() > MAX_LOG_PAGE_BYTES {
                break;
            }
            page_bytes += line.len();
            records.push((*cursor, line.clone()));
        }
        let next = records.last().map_or(begin, |(n, _)| n.saturating_add(1));
        Ok(LogPage {
            retained_from: j.log_start,
            next,
            gap: after < j.log_start,
            records,
        })
    }
    pub fn shutdown(&mut self) {
        self.closing = true;
        let ids = self
            .jobs
            .iter()
            .filter_map(|(id, j)| (!j.state.terminal()).then_some(*id))
            .collect::<Vec<_>>();
        for id in ids {
            let _ = self.cancel(id);
        }
    }
}
impl Default for Jobs {
    fn default() -> Self {
        Self {
            max_running: 1,
            max_logs: DEFAULT_MAX_LOG_RECORDS,
            next: 1,
            running: 0,
            closing: false,
            jobs: BTreeMap::new(),
            slots: BTreeMap::new(),
            queue: VecDeque::new(),
            started: VecDeque::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_supersedes_and_never_rejoins_a_cancelling_active_job() {
        let mut jobs = Jobs::new(1);
        assert_eq!(jobs.submit("w", "s", "a").unwrap(), Submission::Started(1));
        assert_eq!(jobs.submit("w", "s", "a").unwrap(), Submission::Joined(1));
        assert_eq!(jobs.submit("w", "s", "b").unwrap(), Submission::Queued(2));
        assert_eq!(jobs.submit("w", "s", "c").unwrap(), Submission::Queued(3));
        assert_eq!(jobs.jobs[&2].state, JobState::Superseded);
        jobs.cancel(1).unwrap();
        assert_eq!(jobs.submit("w", "s", "a").unwrap(), Submission::Queued(4));
        jobs.settle(1, false).unwrap();
        assert_eq!(jobs.jobs[&4].state, JobState::Running);
    }

    #[test]
    fn cap_cursors_and_shutdown_are_bounded() {
        let mut jobs = Jobs::new(1);
        let first = match jobs.submit("a", "s", "x").unwrap() {
            Submission::Started(id) => id,
            _ => unreachable!(),
        };
        let second = match jobs.submit("b", "s", "x").unwrap() {
            Submission::Queued(id) => id,
            _ => unreachable!(),
        };
        assert_eq!(jobs.jobs[&second].state, JobState::Queued);
        jobs.max_logs = 2;
        jobs.log(first, "one".into()).unwrap();
        jobs.log(first, "two".into()).unwrap();
        jobs.log(first, "three".into()).unwrap();
        let (start, records) = jobs.logs(first, 0).unwrap();
        assert_eq!(start, 1);
        assert_eq!(records, vec![(1, "two".into()), (2, "three".into())]);
        jobs.shutdown();
        assert_eq!(jobs.jobs[&second].state, JobState::Cancelled);
        assert!(matches!(jobs.submit("c", "s", "x"), Err(JobError::Closing)));
    }
    #[test]
    fn log_page_reports_eviction_gap_and_bounded_next_cursor() {
        let mut jobs = Jobs::new(1);
        let id = match jobs.submit("w", "s", "d").unwrap() {
            Submission::Started(id) => id,
            _ => unreachable!(),
        };
        jobs.max_logs = 2;
        for line in ["a", "b", "c"] {
            jobs.log(id, line.into()).unwrap();
        }
        let page = jobs.log_page(id, 0, 1).unwrap();
        assert!(page.gap);
        assert_eq!(page.retained_from, 1);
        assert_eq!(page.records, vec![(1, "b".into())]);
        assert_eq!(page.next, 2);
    }

    #[test]
    fn log_records_are_bounded_before_they_can_exceed_the_ipc_frame() {
        let mut jobs = Jobs::new(1);
        let id = match jobs.submit("w", "s", "d").unwrap() {
            Submission::Started(id) => id,
            _ => unreachable!(),
        };
        assert!(matches!(
            jobs.log(id, "x".repeat(MAX_LOG_LINE_BYTES + 1)),
            Err(JobError::LogTooLarge)
        ));
    }
}
