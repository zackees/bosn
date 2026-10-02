//! Bounded daemon-owned admission policy. Engine execution is deliberately
//! outside this module: only validated semantic jobs may reach that layer.
//!
//! # Policy (#12, restored and extended by #358)
//!
//! * Per `(workspace, stack)` key at most one job runs and one waits; an
//!   identical digest joins, a different one supersedes the waiting entry
//!   (#12). This is unchanged.
//! * Distinct keys run in parallel up to their class's cap. Task jobs use
//!   **runner slots** (default `max(1, ncpu * 2)`, see `capacity`); short
//!   control operations (ensure, prepare, converge) use their own **control
//!   lane**, so a stack ensure never waits behind long tasks.
//! * Within a class, admission is **round-robin across workspaces**, not
//!   FIFO: one checkout that queues ten jobs cannot starve another checkout's
//!   single job. A queued job waits only for a slot of its own class, so a
//!   long act run never blocks a quick job from another session.
//! * A running runner job holds a numbered slot (the lowest free index) for
//!   accounting, until it settles.

use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant, SystemTime},
};

pub const DEFAULT_MAX_LOG_RECORDS: usize = 5_000;
/// Keep a complete page well below the daemon frame cap, including protobuf
/// overhead. Engine adapters must split output into records at this boundary.
pub const MAX_LOG_LINE_BYTES: usize = 2_048;
pub const MAX_LOG_PAGE_RECORDS: usize = 256;
pub const MAX_LOG_PAGE_BYTES: usize = 512 * 1024;
/// Terminal jobs kept for status/log polls; see [`Jobs::retire_finished`].
pub const RETAINED_FINISHED_JOBS: usize = 256;
pub const RETAINED_FINISHED_MIN_AGE: Duration = Duration::from_secs(600);

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

/// Which lane admits a job; see the module policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum JobClass {
    /// Long-running tasks (`bosn run --task`): runner slots.
    Runner,
    /// Daemon operations (ensure, prepare, converge): the control lane.
    Control,
}
impl JobClass {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Runner => "runner",
            Self::Control => "control",
        }
    }
}

/// Lane sizes. [`Jobs::new`] uses one value for both.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerPolicy {
    pub runner_slots: usize,
    pub control_slots: usize,
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
    pub class: JobClass,
    /// Runner slot index while running (runner jobs only).
    pub slot: Option<usize>,
    pub submitted_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub finished_at: Option<SystemTime>,
    /// Last log line, or admission when none yet; feeds stall detection.
    pub last_progress: Option<Instant>,
    logs: VecDeque<(u64, String)>,
}
impl Job {
    fn key(&self) -> (String, String) {
        (self.workspace.clone(), self.stack.clone())
    }
    /// Everything but the retained log, which can hold thousands of records:
    /// status polls and the accounting view never need it.
    fn summary(&self) -> Self {
        Self {
            id: self.id,
            workspace: self.workspace.clone(),
            stack: self.stack.clone(),
            digest: self.digest.clone(),
            state: self.state,
            error: self.error.clone(),
            log_start: self.log_start,
            class: self.class,
            slot: self.slot,
            submitted_at: self.submitted_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            last_progress: self.last_progress,
            logs: VecDeque::new(),
        }
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
/// A follower's promise to keep polling a job (#357). `bosn run` streams a
/// job until it ends; when it dies (SIGTERM, SIGHUP, SIGKILL, a crash) the
/// polls stop, and the job is cancelled instead of running to its deadline.
#[derive(Clone, Copy, Debug)]
struct Lease {
    period: Duration,
    seen: Instant,
}
pub struct Jobs {
    policy: SchedulerPolicy,
    max_logs: usize,
    next: u64,
    /// Running jobs per class.
    running: BTreeMap<JobClass, usize>,
    closing: bool,
    jobs: BTreeMap<u64, Job>,
    slots: BTreeMap<(String, String), Slot>,
    /// Admissible queued jobs per class, per workspace, FIFO within one
    /// workspace; see [`Self::pump`].
    queues: BTreeMap<JobClass, BTreeMap<String, VecDeque<u64>>>,
    /// The workspace each class served last, for round-robin admission.
    served: BTreeMap<JobClass, String>,
    /// Runner slot ownership by index.
    runner_slots: Vec<Option<u64>>,
    /// Terminal jobs in finishing order, for bounded retention.
    finished: VecDeque<u64>,
    /// IDs which have just transitioned from queued to running.  The daemon
    /// actor drains this handoff and attaches the semantic executor; the
    /// scheduler itself never knows about Docker or process ownership.
    started: VecDeque<u64>,
    /// Follow leases of unfinished jobs; see [`Lease`].
    leases: BTreeMap<u64, Lease>,
}

impl Jobs {
    /// One cap for both lanes; mainly for tests.
    pub fn new(max_running: usize) -> Self {
        Self::with_policy(SchedulerPolicy {
            runner_slots: max_running,
            control_slots: max_running,
        })
    }
    pub fn with_policy(policy: SchedulerPolicy) -> Self {
        let policy = SchedulerPolicy {
            runner_slots: policy.runner_slots.max(1),
            control_slots: policy.control_slots.max(1),
        };
        Self {
            policy,
            runner_slots: vec![None; policy.runner_slots],
            ..Self::default()
        }
    }
    pub fn policy(&self) -> SchedulerPolicy {
        self.policy
    }
    /// Submit a control-lane job; see [`Self::submit_class`].
    pub fn submit(
        &mut self,
        workspace: &str,
        stack: &str,
        digest: &str,
    ) -> Result<Submission, JobError> {
        self.submit_class(workspace, stack, digest, JobClass::Control)
    }
    pub fn submit_class(
        &mut self,
        workspace: &str,
        stack: &str,
        digest: &str,
        class: JobClass,
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
                class,
                slot: None,
                submitted_at: SystemTime::now(),
                started_at: None,
                finished_at: None,
                last_progress: None,
                logs: VecDeque::new(),
            },
        );
        if active.is_some() {
            self.slots.get_mut(&key).unwrap().pending = Some(id);
            Ok(Submission::Queued(id))
        } else {
            self.slots.get_mut(&key).unwrap().active = Some(id);
            self.enqueue(id);
            self.pump();
            Ok(if self.jobs[&id].state == JobState::Running {
                Submission::Started(id)
            } else {
                Submission::Queued(id)
            })
        }
    }
    fn enqueue(&mut self, id: u64) {
        let job = &self.jobs[&id];
        self.queues
            .entry(job.class)
            .or_default()
            .entry(job.workspace.clone())
            .or_default()
            .push_back(id);
    }
    fn cap(&self, class: JobClass) -> usize {
        match class {
            JobClass::Runner => self.policy.runner_slots,
            JobClass::Control => self.policy.control_slots,
        }
    }
    /// Admit queued jobs while their class has capacity. Workspaces take
    /// turns: the next admission goes to the first workspace (in key order)
    /// after the one served last, wrapping around.
    fn pump(&mut self) {
        if self.closing {
            return;
        }
        let mut lapsed = Vec::new();
        let now = Instant::now();
        for class in [JobClass::Control, JobClass::Runner] {
            let mut deferred = Vec::new();
            while self.running.get(&class).copied().unwrap_or(0) < self.cap(class) {
                let Some(id) = self.next_queued(class) else {
                    break;
                };
                // A follower that is already gone never gets its job
                // started: its lease lapsed while the job waited.
                if self.lease_lapsed(id, now) {
                    lapsed.push(id);
                    continue;
                }
                // A follower that has not polled for half its lease is
                // probably gone: hold the job (and leave the slot free)
                // until it polls again or its lease lapses. A live `bosn
                // run` polls every 200 ms against a 30 s lease.
                if self.lease_stale(id, now) {
                    deferred.push(id);
                    continue;
                }
                let slot = (class == JobClass::Runner).then(|| {
                    let index = self
                        .runner_slots
                        .iter()
                        .position(Option::is_none)
                        .expect("a runner below its cap has a free slot");
                    self.runner_slots[index] = Some(id);
                    index
                });
                let job = self.jobs.get_mut(&id).unwrap();
                job.state = JobState::Running;
                job.slot = slot;
                job.started_at = Some(SystemTime::now());
                job.last_progress = Some(Instant::now());
                *self.running.entry(class).or_default() += 1;
                self.started.push_back(id);
            }
            for id in deferred.into_iter().rev() {
                let workspace = self.jobs[&id].workspace.clone();
                self.queues
                    .entry(class)
                    .or_default()
                    .entry(workspace)
                    .or_default()
                    .push_front(id);
            }
        }
        for id in lapsed {
            let _ = self.log(
                id,
                "[bosn] cancelling: the client following this job stopped polling before it started"
                    .into(),
            );
            // Finishing a never-started job admits the next one in turn.
            self.finish(id, JobState::Cancelled, Some("cancelled".into()));
        }
    }
    fn lease_stale(&self, id: u64, now: Instant) -> bool {
        self.leases
            .get(&id)
            .is_some_and(|lease| now.saturating_duration_since(lease.seen) > lease.period / 2)
    }
    fn lease_lapsed(&self, id: u64, now: Instant) -> bool {
        self.leases
            .get(&id)
            .is_some_and(|lease| now.saturating_duration_since(lease.seen) > lease.period)
    }
    /// Pop the next admissible job of `class`, round-robin by workspace.
    /// Entries that stopped being queued (cancelled or superseded) are
    /// dropped on the way.
    fn next_queued(&mut self, class: JobClass) -> Option<u64> {
        let queues = self.queues.get_mut(&class)?;
        loop {
            let after = self.served.get(&class);
            let workspace = after
                .and_then(|last| {
                    queues
                        .range::<String, _>((
                            std::ops::Bound::Excluded(last),
                            std::ops::Bound::Unbounded,
                        ))
                        .next()
                })
                .or_else(|| queues.iter().next())
                .map(|(workspace, _)| workspace.clone())?;
            let queue = queues.get_mut(&workspace).unwrap();
            let id = queue.pop_front();
            if queue.is_empty() {
                queues.remove(&workspace);
            }
            let Some(id) = id else { continue };
            if self
                .jobs
                .get(&id)
                .is_some_and(|job| job.state == JobState::Queued)
            {
                self.served.insert(class, workspace);
                return Some(id);
            }
        }
    }
    /// Attach a follow lease to an unfinished job. Leasing a job again keeps
    /// the shorter period.
    pub fn lease(&mut self, id: u64, period: Duration, now: Instant) {
        if self.jobs.get(&id).is_none_or(|job| job.state.terminal()) {
            return;
        }
        let lease = self.leases.entry(id).or_insert(Lease { period, seen: now });
        lease.period = lease.period.min(period);
        lease.seen = now;
    }
    /// A status or log poll for a job renews its lease, if it has one.
    pub fn touch(&mut self, id: u64, now: Instant) {
        if let Some(lease) = self.leases.get_mut(&id) {
            lease.seen = now;
            // A job held back for a quiet follower may start now.
            if self
                .jobs
                .get(&id)
                .is_some_and(|job| job.state == JobState::Queued)
            {
                self.pump();
            }
        }
    }
    /// Queued or running jobs whose follower has not polled within its lease.
    /// A job already cancelling is left to finish.
    pub fn expired_leases(&self, now: Instant) -> Vec<u64> {
        self.leases
            .iter()
            .filter(|(id, lease)| {
                now.saturating_duration_since(lease.seen) > lease.period
                    && self.jobs.get(id).is_some_and(|job| {
                        matches!(job.state, JobState::Queued | JobState::Running)
                    })
            })
            .map(|(id, _)| *id)
            .collect()
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
    /// The job without its log; read the log with [`Self::log_page`].
    pub fn job(&self, id: u64) -> Result<Job, JobError> {
        self.jobs
            .get(&id)
            .map(Job::summary)
            .ok_or(JobError::Unknown)
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
        j.finished_at = Some(SystemTime::now());
        let class = j.class;
        if let Some(slot) = j.slot.take() {
            self.runner_slots[slot] = None;
        }
        self.leases.remove(&id);
        if was_running {
            *self.running.get_mut(&class).unwrap() -= 1;
        }
        let s = self.slots.get_mut(&key).unwrap();
        let mut promoted = None;
        if s.active == Some(id) {
            s.active = s.pending.take();
            promoted = s.active;
        } else if s.pending == Some(id) {
            s.pending = None
        }
        if s.active.is_none() && s.pending.is_none() {
            self.slots.remove(&key);
        }
        if let Some(next) = promoted {
            self.enqueue(next)
        }
        self.finished.push_back(id);
        self.retire_finished();
        self.pump();
    }
    /// Keep the newest [`RETAINED_FINISHED_JOBS`] terminal jobs (and their
    /// logs). Older ones are forgotten once they have been finished for
    /// [`RETAINED_FINISHED_MIN_AGE`], so a follower that polls a little late
    /// still sees the outcome, while a long-lived daemon's memory stays
    /// bounded.
    fn retire_finished(&mut self) {
        while self.finished.len() > RETAINED_FINISHED_JOBS {
            let oldest = self.finished[0];
            let old_enough = self.jobs[&oldest]
                .finished_at
                .and_then(|at| at.elapsed().ok())
                .is_none_or(|age| age >= RETAINED_FINISHED_MIN_AGE);
            if !old_enough {
                break;
            }
            self.finished.pop_front();
            self.jobs.remove(&oldest);
        }
    }
    /// Unfinished jobs plus up to `recent` most recently finished ones, for
    /// the `bosn jobs` accounting view.
    pub fn snapshot(&self, recent: usize) -> Vec<Job> {
        let mut out: Vec<Job> = self
            .jobs
            .values()
            .filter(|job| !job.state.terminal())
            .map(Job::summary)
            .collect();
        out.extend(
            self.finished
                .iter()
                .rev()
                .take(recent)
                .filter_map(|id| self.jobs.get(id).map(Job::summary)),
        );
        out
    }
    /// Count of queued and running jobs per class.
    pub fn load(&self) -> BTreeMap<JobClass, (usize, usize)> {
        let mut load = BTreeMap::new();
        for job in self.jobs.values() {
            let entry: &mut (usize, usize) = load.entry(job.class).or_default();
            match job.state {
                JobState::Queued => entry.0 += 1,
                JobState::Running | JobState::Cancelling => entry.1 += 1,
                _ => {}
            }
        }
        load
    }
    /// Running jobs (not already cancelling) whose last progress is older
    /// than `after`. `activity` may report a later progress instant from
    /// outside the log stream, such as Docker traffic.
    pub fn stalled(
        &self,
        now: Instant,
        after: Duration,
        activity: impl Fn(u64) -> Option<Instant>,
    ) -> Vec<u64> {
        self.jobs
            .values()
            .filter(|job| job.state == JobState::Running && job.class == JobClass::Runner)
            .filter(|job| {
                let last = job.last_progress.into_iter().chain(activity(job.id)).max();
                last.is_some_and(|last| now.saturating_duration_since(last) > after)
            })
            .map(|job| job.id)
            .collect()
    }
    pub fn log(&mut self, id: u64, line: String) -> Result<u64, JobError> {
        if line.len() > MAX_LOG_LINE_BYTES {
            return Err(JobError::LogTooLarge);
        }
        let j = self.jobs.get_mut(&id).ok_or(JobError::Unknown)?;
        if !j.state.terminal() {
            j.last_progress = Some(Instant::now());
        }
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
            policy: SchedulerPolicy {
                runner_slots: 1,
                control_slots: 1,
            },
            max_logs: DEFAULT_MAX_LOG_RECORDS,
            next: 1,
            running: BTreeMap::new(),
            closing: false,
            jobs: BTreeMap::new(),
            slots: BTreeMap::new(),
            queues: BTreeMap::new(),
            served: BTreeMap::new(),
            runner_slots: vec![None],
            finished: VecDeque::new(),
            started: VecDeque::new(),
            leases: BTreeMap::new(),
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
    fn a_follow_lease_expires_for_queued_and_running_jobs_until_polled() {
        let mut jobs = Jobs::new(1);
        let start = Instant::now();
        let lease = Duration::from_secs(30);
        let running = match jobs.submit("a", "s", "x").unwrap() {
            Submission::Started(id) => id,
            other => panic!("{other:?}"),
        };
        let queued = match jobs.submit("b", "s", "x").unwrap() {
            Submission::Queued(id) => id,
            other => panic!("{other:?}"),
        };
        let unleased = match jobs.submit("c", "s", "x").unwrap() {
            Submission::Queued(id) => id,
            other => panic!("{other:?}"),
        };
        jobs.lease(running, lease, start);
        jobs.lease(queued, lease, start);
        assert!(jobs.expired_leases(start + lease).is_empty(), "not yet");

        // Polling one job renews only that job's lease.
        jobs.touch(running, start + Duration::from_secs(20));
        let later = start + Duration::from_secs(31);
        assert_eq!(jobs.expired_leases(later), vec![queued]);
        let much_later = start + Duration::from_secs(51);
        assert_eq!(jobs.expired_leases(much_later), vec![running, queued]);
        assert!(!jobs.expired_leases(much_later).contains(&unleased));

        // A queued job that expires is cancelled before it ever starts.
        jobs.cancel(queued).unwrap();
        assert_eq!(jobs.jobs[&queued].state, JobState::Cancelled);
        // A running one is left to its executor once it is cancelling.
        jobs.cancel(running).unwrap();
        assert!(jobs.expired_leases(much_later).is_empty());
        jobs.settle(running, false).unwrap();
        assert!(jobs.leases.is_empty(), "finished jobs forget their lease");

        // The slot passes to the unleased job, which never expires.
        assert_eq!(jobs.jobs[&unleased].state, JobState::Running);
        assert!(
            jobs.expired_leases(start + Duration::from_secs(3600))
                .is_empty()
        );

        // A second follower can only shorten a lease; a finished job cannot
        // be leased again.
        let next = match jobs.submit("d", "s", "x").unwrap() {
            Submission::Queued(id) => id,
            other => panic!("{other:?}"),
        };
        jobs.lease(next, lease, start);
        jobs.lease(next, Duration::from_secs(5), start);
        assert_eq!(
            jobs.expired_leases(start + Duration::from_secs(6)),
            vec![next]
        );
        jobs.cancel(next).unwrap();
        jobs.lease(next, lease, start);
        assert!(jobs.leases.is_empty());
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

    fn started(submission: Submission) -> u64 {
        match submission {
            Submission::Started(id) => id,
            other => panic!("expected a start, got {other:?}"),
        }
    }
    fn queued(submission: Submission) -> u64 {
        match submission {
            Submission::Queued(id) => id,
            other => panic!("expected a queue, got {other:?}"),
        }
    }

    #[test]
    fn distinct_workspaces_run_in_parallel_up_to_the_runner_cap() {
        // #358 RED: with Jobs::new(1) workspace B stayed Queued while A ran.
        let mut jobs = Jobs::with_policy(SchedulerPolicy {
            runner_slots: 3,
            control_slots: 1,
        });
        let a = started(jobs.submit_class("a", "t", "x", JobClass::Runner).unwrap());
        let b = started(jobs.submit_class("b", "t", "x", JobClass::Runner).unwrap());
        let c = started(jobs.submit_class("c", "t", "x", JobClass::Runner).unwrap());
        let d = queued(jobs.submit_class("d", "t", "x", JobClass::Runner).unwrap());
        assert_eq!(
            [a, b, c].map(|id| jobs.jobs[&id].slot),
            [Some(0), Some(1), Some(2)]
        );
        assert_eq!(jobs.take_started(), vec![a, b, c]);
        // The freed slot index is reused by the next admission.
        jobs.settle(b, true).unwrap();
        assert_eq!(jobs.jobs[&d].state, JobState::Running);
        assert_eq!(jobs.jobs[&d].slot, Some(1));
        assert_eq!(jobs.jobs[&b].slot, None, "a finished job holds no slot");
    }

    #[test]
    fn a_full_runner_lane_never_blocks_a_control_job() {
        let mut jobs = Jobs::with_policy(SchedulerPolicy {
            runner_slots: 1,
            control_slots: 1,
        });
        started(
            jobs.submit_class("a", "act", "x", JobClass::Runner)
                .unwrap(),
        );
        queued(
            jobs.submit_class("b", "act", "x", JobClass::Runner)
                .unwrap(),
        );
        // Another session's ensure starts at once despite the runner queue.
        started(jobs.submit("c", "ensure", "x").unwrap());
        assert_eq!(jobs.load()[&JobClass::Runner], (1, 1));
        assert_eq!(jobs.load()[&JobClass::Control], (0, 1));
    }

    #[test]
    fn admission_is_round_robin_across_workspaces() {
        let mut jobs = Jobs::new(1);
        let first = started(jobs.submit_class("a", "s0", "x", JobClass::Runner).unwrap());
        // Workspace a floods the queue before b and c submit one job each.
        let a: Vec<u64> = (1..=3)
            .map(|n| {
                queued(
                    jobs.submit_class("a", &format!("s{n}"), "x", JobClass::Runner)
                        .unwrap(),
                )
            })
            .collect();
        let b = queued(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
        let c = queued(jobs.submit_class("c", "s", "x", JobClass::Runner).unwrap());
        let mut order = Vec::new();
        let mut running = first;
        for _ in 0..5 {
            jobs.settle(running, true).unwrap();
            running = jobs
                .jobs
                .values()
                .find(|job| job.state == JobState::Running)
                .unwrap()
                .id;
            order.push(running);
        }
        assert_eq!(order, vec![b, c, a[0], a[1], a[2]]);
    }

    #[test]
    fn a_cancelled_or_superseded_queue_entry_is_skipped() {
        let mut jobs = Jobs::new(1);
        let a = started(jobs.submit_class("a", "s", "x", JobClass::Runner).unwrap());
        let b = queued(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
        let c = queued(jobs.submit_class("c", "s", "x", JobClass::Runner).unwrap());
        jobs.cancel(b).unwrap();
        jobs.settle(a, true).unwrap();
        assert_eq!(jobs.jobs[&c].state, JobState::Running);
        assert_eq!(jobs.jobs[&c].slot, Some(0));
    }

    #[test]
    fn stalled_reports_running_runner_jobs_without_recent_progress() {
        let mut jobs = Jobs::new(2);
        let quiet = started(jobs.submit_class("a", "s", "x", JobClass::Runner).unwrap());
        let chatty = started(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
        let control = started(jobs.submit("c", "s", "x").unwrap());
        let now = Instant::now();
        let later = now + Duration::from_secs(120);
        jobs.jobs.get_mut(&chatty).unwrap().last_progress = Some(later);
        let after = Duration::from_secs(60);
        assert_eq!(jobs.stalled(later, after, |_| None), vec![quiet]);
        // Docker activity counts as progress too.
        assert!(
            jobs.stalled(later, after, |id| (id == quiet).then_some(later))
                .is_empty()
        );
        // A cancelling job is already being torn down; a control job is not
        // subject to stall teardown.
        jobs.cancel(quiet).unwrap();
        assert!(jobs.stalled(later, after, |_| None).is_empty());
        assert_eq!(jobs.jobs[&control].class, JobClass::Control);
        // A log line is progress.
        jobs.log(chatty, "tick".into()).unwrap();
        assert!(jobs.jobs[&chatty].last_progress.unwrap() >= now);
    }

    #[test]
    fn finished_jobs_are_retained_boundedly() {
        let mut jobs = Jobs::new(1);
        let total = RETAINED_FINISHED_JOBS + 10;
        let mut ids = Vec::new();
        for n in 0..total {
            let id = started(
                jobs.submit_class(&format!("w{n}"), "s", "x", JobClass::Runner)
                    .unwrap(),
            );
            jobs.settle(id, true).unwrap();
            ids.push(id);
        }
        // Too young to forget: everything is still pollable.
        assert_eq!(jobs.jobs.len(), total);
        for id in &ids[..10] {
            jobs.jobs.get_mut(id).unwrap().finished_at =
                Some(SystemTime::now() - RETAINED_FINISHED_MIN_AGE * 2);
        }
        let id = started(
            jobs.submit_class("late", "s", "x", JobClass::Runner)
                .unwrap(),
        );
        jobs.settle(id, true).unwrap();
        assert_eq!(jobs.jobs.len(), RETAINED_FINISHED_JOBS + 1);
        assert!(matches!(jobs.job(ids[0]), Err(JobError::Unknown)));
        assert!(jobs.job(ids[10]).is_ok());
        assert_eq!(jobs.snapshot(3).len(), 3);
    }

    #[test]
    fn a_queued_job_whose_follower_is_gone_is_cancelled_not_started() {
        let mut jobs = Jobs::new(1);
        let start = Instant::now();
        let running = started(jobs.submit_class("a", "s", "x", JobClass::Runner).unwrap());
        let gone = queued(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
        let next = queued(jobs.submit_class("c", "s", "x", JobClass::Runner).unwrap());
        jobs.lease(gone, Duration::from_millis(1), start);
        std::thread::sleep(Duration::from_millis(5));
        jobs.settle(running, true).unwrap();
        assert_eq!(jobs.jobs[&gone].state, JobState::Cancelled);
        assert!(jobs.jobs[&gone].started_at.is_none(), "never started");
        assert_eq!(
            jobs.jobs[&next].state,
            JobState::Running,
            "the slot passes on"
        );
        assert_eq!(jobs.take_started(), vec![running, next]);
    }
    #[test]
    fn a_quiet_followers_queued_job_waits_and_starts_when_it_polls_again() {
        let mut jobs = Jobs::new(1);
        let start = Instant::now();
        let running = started(jobs.submit_class("a", "s", "x", JobClass::Runner).unwrap());
        let quiet = queued(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
        let other = queued(jobs.submit_class("c", "s", "x", JobClass::Runner).unwrap());
        jobs.lease(quiet, Duration::from_millis(40), start);
        std::thread::sleep(Duration::from_millis(25));
        // Past half its lease: held back, and the slot goes to the next job.
        jobs.settle(running, true).unwrap();
        assert_eq!(jobs.jobs[&quiet].state, JobState::Queued);
        assert_eq!(jobs.jobs[&other].state, JobState::Running);
        jobs.settle(other, true).unwrap();
        assert_eq!(jobs.jobs[&quiet].state, JobState::Queued, "still quiet");
        // Its follower polls: it starts at once.
        jobs.touch(quiet, Instant::now());
        assert_eq!(jobs.jobs[&quiet].state, JobState::Running);
    }
}
