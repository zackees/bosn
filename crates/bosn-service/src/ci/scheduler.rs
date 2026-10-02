//! Machine-wide CI admission: one FIFO queue, one live concurrency limit, and
//! coalescing of identical submissions onto the run already queued/running.
//! Pure state; the CI actor owns the only instance and performs effects.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Everything that makes two submissions the same execution.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RunKey {
    pub sha: String,
    pub dirty: Option<String>,
    pub workflow: String,
    pub job: Option<String>,
    pub trigger: String,
    pub mode: String,
    pub provider: String,
    pub engine: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Admission {
    /// A new run was queued under the caller's ID.
    Queued(String),
    /// An identical run is already queued or running; share its ID.
    Coalesced(String),
}

#[derive(Debug)]
pub struct Scheduler {
    limit: usize,
    drained: bool,
    queue: VecDeque<String>,
    running: BTreeSet<String>,
    live: BTreeMap<RunKey, String>,
    keys: BTreeMap<String, RunKey>,
}

/// Default limit: physical cores / 4, at least one.
pub fn default_limit(cores: usize) -> usize {
    (cores / 4).max(1)
}

impl Scheduler {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            drained: false,
            queue: VecDeque::new(),
            running: BTreeSet::new(),
            live: BTreeMap::new(),
            keys: BTreeMap::new(),
        }
    }

    pub fn submit(&mut self, key: RunKey, id: String) -> Admission {
        if let Some(existing) = self.live.get(&key) {
            return Admission::Coalesced(existing.clone());
        }
        self.live.insert(key.clone(), id.clone());
        self.keys.insert(id.clone(), key);
        self.queue.push_back(id.clone());
        Admission::Queued(id)
    }

    /// Runs to start now, in FIFO order. Callers must start every returned run.
    pub fn admit(&mut self) -> Vec<String> {
        let mut started = Vec::new();
        while !self.drained && self.running.len() < self.limit {
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            self.running.insert(id.clone());
            started.push(id);
        }
        started
    }

    /// A run reached a terminal state: free its slot and its coalescing key.
    pub fn finish(&mut self, id: &str) {
        self.running.remove(id);
        self.queue.retain(|q| q != id);
        if let Some(key) = self.keys.remove(id) {
            self.live.remove(&key);
        }
    }

    /// Remove a queued run. Returns false when it is running or unknown.
    pub fn cancel_queued(&mut self, id: &str) -> bool {
        if !self.queue.iter().any(|q| q == id) {
            return false;
        }
        self.finish(id);
        true
    }

    /// Lowering the limit never stops a running job; raising it lets
    /// waiting runs start on the next [`Self::admit`].
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
    }

    pub fn set_drained(&mut self, drained: bool) {
        self.drained = drained;
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
    pub fn drained(&self) -> bool {
        self.drained
    }
    pub fn running(&self) -> usize {
        self.running.len()
    }
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains(id)
    }
    pub fn queue_position(&self, id: &str) -> Option<usize> {
        self.queue.iter().position(|q| q == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: usize) -> RunKey {
        RunKey {
            sha: format!("{n:040x}"),
            dirty: None,
            workflow: ".github/workflows/ci.yml".into(),
            job: None,
            trigger: "push".into(),
            mode: "minimal".into(),
            provider: "github".into(),
            engine: "act".into(),
        }
    }

    #[test]
    fn fifty_submissions_with_ten_keys_make_ten_executions() {
        let mut s = Scheduler::new(3);
        let mut ids = BTreeMap::new();
        for i in 0..50 {
            let k = key(i % 10);
            let id = match s.submit(k.clone(), format!("run-{i}")) {
                Admission::Queued(id) => id,
                Admission::Coalesced(id) => id,
            };
            // Every submitter of one key gets that key's single run ID.
            assert_eq!(ids.entry(i % 10).or_insert(id.clone()), &id);
        }
        assert_eq!(ids.len(), 10);
        let mut executed = BTreeSet::new();
        loop {
            let started = s.admit();
            assert!(s.running() <= 3, "limit respected");
            if started.is_empty() && s.running() == 0 {
                break;
            }
            for id in started {
                executed.insert(id.clone());
                s.finish(&id);
            }
        }
        assert_eq!(executed.len(), 10);
        assert_eq!(executed, ids.values().cloned().collect());
    }

    #[test]
    fn limit_changes_apply_live_and_never_stop_running_work() {
        let mut s = Scheduler::new(1);
        for i in 0..4 {
            s.submit(key(i), format!("r{i}"));
        }
        assert_eq!(s.admit(), ["r0"]);
        s.set_limit(3);
        assert_eq!(s.admit(), ["r1", "r2"], "raising starts waiting runs");
        s.set_limit(1);
        assert!(s.admit().is_empty());
        assert_eq!(s.running(), 3, "lowering kills nothing");
        s.finish("r0");
        s.finish("r1");
        assert!(s.admit().is_empty(), "still above the lowered limit");
        s.finish("r2");
        assert_eq!(s.admit(), ["r3"]);
    }

    #[test]
    fn drain_holds_the_queue_and_finished_keys_can_run_again() {
        let mut s = Scheduler::new(2);
        s.set_drained(true);
        s.submit(key(1), "a".into());
        assert!(s.admit().is_empty());
        s.set_drained(false);
        assert_eq!(s.admit(), ["a"]);
        assert_eq!(
            s.submit(key(1), "b".into()),
            Admission::Coalesced("a".into())
        );
        s.finish("a");
        assert_eq!(s.submit(key(1), "b".into()), Admission::Queued("b".into()));
        assert!(s.cancel_queued("b"));
        assert!(!s.cancel_queued("b"));
        assert_eq!(default_limit(16), 4);
        assert_eq!(default_limit(2), 1);
    }
}
