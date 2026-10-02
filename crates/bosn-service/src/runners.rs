//! Centralized runner accounting (#358).
//!
//! The daemon's job table says which jobs are queued and running. This
//! registry adds what each running task job *holds*: its runner slot and
//! CPU allocation, its setup container, its Docker proxy socket, the cache
//! volumes it leased, and when it last showed activity. It is the one place
//! that answers "what is running on this machine, and what does it own".
//!
//! # Durability
//!
//! Every change to the set of runs is written to
//! `<state-dir>/runners/active.json` (temp file, then rename). A daemon that
//! dies takes the output streams and client leases of its jobs with it, so
//! its successor cannot re-adopt them; instead, before accepting work, it
//! **reaps** each recorded run: it stops the task's processes inside the
//! recorded setup container and removes every Docker object labelled with
//! that run. It then sweeps objects that carry this daemon's label but name
//! no live run (a ledger write that never happened). Nothing labelled by
//! another daemon or another run is ever touched.
//!
//! # Cache volumes that follow a job
//!
//! A job's named volumes can be mapped onto Bosn cache volumes (see
//! [`CacheRule`]). Two concurrency modes:
//!
//! * **exclusive** (default): per-key locking over a small pool of replicas.
//!   The lock is an OS file lock (`flock`) on `/tmp/bosn-<uid>/cache-locks/
//!   <volume>.lock`, held for the run: every bosn daemon of this user honours
//!   it, and the kernel drops it if the daemon dies.
//!   A job leases the lowest-numbered free replica for its whole run, so
//!   every workflow job inside that run (A, then B that `needs: A`) sees the
//!   same volume, and the next run (sequentially) gets the same warm
//!   replica. Concurrent runs get other replicas, so two runs never write
//!   one volume at once. When every replica is busy, a run gets a private
//!   cold volume that is removed with it, like a cache miss on GitHub.
//! * **shared**: one volume mounted read-write into every concurrent job,
//!   for tools that do their own locking (cargo's registry, uv's cache).

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::{
    capacity::RunnerCapacity,
    docker_api::{
        DockerApi, LABEL_CACHE, LABEL_CACHE_KEY, LABEL_DAEMON, LABEL_JOB, LABEL_RUN, LABEL_SLOT,
        Teardown,
    },
    docker_proxy::{Activity, VolumePolicy, now_ms},
};

pub const LEDGER_DIR: &str = "runners";
pub const LEDGER_FILE: &str = "active.json";
/// Longest proxy socket file name: `j<20 digits>-<12 hex>.sock`.
const SOCKET_NAME_MAX: usize = 40;

/// How a cache volume is shared between concurrent runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    Exclusive,
    Shared,
}

/// Which runs share one cache key.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheScope {
    /// Every run on this machine.
    Machine,
    /// Every checkout of one repository (its `origin` URL, else its path).
    Repo,
    /// One checkout.
    Workspace,
}

/// One cache mapping for the containers a task job creates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CacheRule {
    pub name: String,
    /// Map this named volume where a container references it.
    pub volume: Option<String>,
    /// Mount the cache here in every container that has nothing there.
    pub destination: Option<String>,
    pub scope: CacheScope,
    pub mode: CacheMode,
    pub replicas: usize,
}

impl CacheRule {
    /// act mounts `act-toolcache` at `/opt/hostedtoolcache` in every job
    /// container, and setup-python/node/uv install into it. Concurrent runs
    /// would install into one volume at once, so by default it becomes an
    /// exclusive, machine-wide cache pool.
    pub fn act_toolcache() -> Self {
        Self {
            name: "act-toolcache".into(),
            volume: Some("act-toolcache".into()),
            destination: None,
            scope: CacheScope::Machine,
            mode: CacheMode::Exclusive,
            replicas: 4,
        }
    }
}

/// One running task job, as persisted in the ledger and reported by
/// `bosn jobs`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub job_id: u64,
    /// The `com.zackees.bosn.run` label value on everything the job created.
    pub run: String,
    pub workspace: String,
    pub stack: String,
    pub task: String,
    pub slot: Option<usize>,
    pub nano_cpus: i64,
    pub memory: Option<u64>,
    pub started_ms: u64,
    pub container: Option<String>,
    pub proxy_socket: Option<String>,
    /// Leased cache volumes.
    pub caches: Vec<String>,
}

/// A live view of one run for `bosn jobs`.
#[derive(Clone, Debug, Serialize)]
pub struct RunView {
    #[serde(flatten)]
    pub record: RunRecord,
    pub last_activity_ms: u64,
    pub docker_requests: u64,
    pub docker_bytes: u64,
    pub docker_creates: u64,
}

struct Run {
    record: RunRecord,
    activity: Arc<Activity>,
}

#[derive(Default)]
struct Inner {
    runs: BTreeMap<u64, Run>,
    /// Cache volume name -> jobs holding it.
    holders: BTreeMap<String, BTreeSet<u64>>,
    /// Exclusive leases: volume -> (job, its held OS lock).
    locks: BTreeMap<String, (u64, std::fs::File)>,
}

pub struct Runners {
    capacity: RunnerCapacity,
    daemon: String,
    instance: String,
    ledger: PathBuf,
    proxy_dir: Option<PathBuf>,
    /// Where exclusive cache leases take their OS file locks.
    lock_dir: PathBuf,
    api: Option<DockerApi>,
    inner: Mutex<Inner>,
}

impl Runners {
    pub fn new(state_dir: &Path, capacity: RunnerCapacity) -> Self {
        let daemon = state_identity(state_dir);
        let instance = format!(
            "{:08x}",
            (now_ms() as u32) ^ std::process::id().rotate_left(16)
        );
        let proxy_dir = capacity.docker_proxy.then(|| proxy_dir(state_dir));
        let uid = kernal_api::platform::ipc::current_user_id().unwrap_or_else(|_| "0".into());
        Self {
            capacity,
            daemon,
            instance,
            ledger: state_dir.join(LEDGER_DIR).join(LEDGER_FILE),
            proxy_dir,
            lock_dir: PathBuf::from(format!("/tmp/bosn-{uid}")).join("cache-locks"),
            api: DockerApi::from_environment(),
            inner: Mutex::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_api(mut self, api: Option<DockerApi>) -> Self {
        self.api = api;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_lock_dir(mut self, dir: PathBuf) -> Self {
        self.lock_dir = dir;
        self
    }

    pub fn capacity(&self) -> &RunnerCapacity {
        &self.capacity
    }
    pub fn daemon(&self) -> &str {
        &self.daemon
    }
    pub fn proxy_dir(&self) -> Option<&Path> {
        self.proxy_dir.as_deref()
    }
    pub fn api(&self) -> Option<&DockerApi> {
        self.api.as_ref()
    }

    /// The run label for a job of this daemon process.
    pub fn run_label(&self, job_id: u64) -> String {
        format!("{}-{}-{job_id}", self.daemon, self.instance)
    }

    /// Labels for every Docker object a job creates.
    pub fn labels(&self, record: &RunRecord) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::from([
            (LABEL_RUN.to_owned(), record.run.clone()),
            (LABEL_DAEMON.to_owned(), self.daemon.clone()),
            (LABEL_JOB.to_owned(), record.job_id.to_string()),
        ]);
        if let Some(slot) = record.slot {
            labels.insert(LABEL_SLOT.to_owned(), slot.to_string());
        }
        labels
    }

    /// Record a newly started task job.
    pub fn begin(
        &self,
        job_id: u64,
        workspace: &str,
        stack: &str,
        task: &str,
        slot: Option<usize>,
    ) -> (RunRecord, Arc<Activity>) {
        let record = RunRecord {
            job_id,
            run: self.run_label(job_id),
            workspace: workspace.to_owned(),
            stack: stack.to_owned(),
            task: task.to_owned(),
            slot,
            nano_cpus: self.capacity.nano_cpus(),
            memory: self.capacity.memory_per_slot,
            started_ms: now_ms(),
            ..RunRecord::default()
        };
        let activity = Arc::new(Activity::new());
        {
            let mut inner = self.inner.lock().unwrap();
            inner.runs.insert(
                job_id,
                Run {
                    record: record.clone(),
                    activity: Arc::clone(&activity),
                },
            );
        }
        self.persist();
        (record, activity)
    }

    pub fn update(&self, job_id: u64, change: impl FnOnce(&mut RunRecord)) {
        {
            let mut inner = self.inner.lock().unwrap();
            let Some(run) = inner.runs.get_mut(&job_id) else {
                return;
            };
            change(&mut run.record);
        }
        self.persist();
    }

    /// Forget a finished run and release its cache leases.
    pub fn finish(&self, job_id: u64) -> Option<RunRecord> {
        let record = {
            let mut inner = self.inner.lock().unwrap();
            let run = inner.runs.remove(&job_id)?;
            for holders in inner.holders.values_mut() {
                holders.remove(&job_id);
            }
            inner.holders.retain(|_, holders| !holders.is_empty());
            inner.locks.retain(|_, (holder, file)| {
                if *holder != job_id {
                    return true;
                }
                release_lock(file);
                false
            });
            run.record
        };
        self.persist();
        Some(record)
    }

    pub fn activity(&self, job_id: u64) -> Option<Instant> {
        self.inner
            .lock()
            .unwrap()
            .runs
            .get(&job_id)
            .map(|run| run.activity.last_instant())
    }

    pub fn views(&self) -> Vec<RunView> {
        self.inner
            .lock()
            .unwrap()
            .runs
            .values()
            .map(|run| RunView {
                record: run.record.clone(),
                last_activity_ms: run.activity.last_ms(),
                docker_requests: run.activity.requests(),
                docker_bytes: run.activity.bytes(),
                docker_creates: run.activity.creates(),
            })
            .collect()
    }

    fn persist(&self) {
        let records: Vec<RunRecord> = self
            .inner
            .lock()
            .unwrap()
            .runs
            .values()
            .map(|run| run.record.clone())
            .collect();
        if let Err(error) = write_ledger(&self.ledger, &records) {
            eprintln!("bosn runners: ledger write failed: {error}");
        }
    }

    /// A fresh per-job proxy socket path inside [`Self::proxy_dir`].
    pub fn proxy_socket(&self, job_id: u64) -> Option<PathBuf> {
        let dir = self.proxy_dir.as_ref()?;
        let nonce = format!(
            "{:012x}",
            now_ms() ^ (job_id << 40) ^ u64::from(std::process::id())
        );
        Some(dir.join(format!("j{job_id}-{}.sock", &nonce[nonce.len() - 12..])))
    }

    /// Lease the cache volume for `rule` on behalf of `job_id`.
    pub fn lease_cache(&self, job_id: u64, rule: &CacheRule, key: &str) -> io::Result<Lease> {
        let base = format!("bosn-cache-{key}-{}", rule.name);
        let candidates: Vec<String> = match rule.mode {
            CacheMode::Shared => vec![base.clone()],
            CacheMode::Exclusive => (0..rule.replicas.max(1))
                .map(|i| format!("{base}-{i}"))
                .collect(),
        };
        let chosen = {
            let mut inner = self.inner.lock().unwrap();
            let mut pick = None;
            for name in &candidates {
                if rule.mode == CacheMode::Shared {
                    pick = Some(name.clone());
                    break;
                }
                if let Some((holder, _)) = inner.locks.get(name) {
                    if *holder == job_id {
                        pick = Some(name.clone());
                        break;
                    }
                    continue;
                }
                // Free in this daemon; the OS lock decides across daemons.
                if let Some(file) = self.try_lock(name)? {
                    inner.locks.insert(name.clone(), (job_id, file));
                    pick = Some(name.clone());
                    break;
                }
            }
            if let Some(name) = &pick {
                inner
                    .holders
                    .entry(name.clone())
                    .or_default()
                    .insert(job_id);
            }
            pick
        };
        let run = self.run_label(job_id);
        let (name, lease) = match chosen {
            Some(name) => (name.clone(), Lease::Kept(name)),
            None => {
                let name = format!("{base}-x{job_id}-{}", self.instance);
                (name.clone(), Lease::Private(name))
            }
        };
        if let Some(api) = &self.api
            && !api.volume_exists(&name)?
        {
            let mut labels = vec![
                (LABEL_CACHE.to_owned(), rule.name.clone()),
                (LABEL_CACHE_KEY.to_owned(), key.to_owned()),
                (LABEL_DAEMON.to_owned(), self.daemon.clone()),
            ];
            if matches!(lease, Lease::Private(_)) {
                // A private overflow copy is the run's own: teardown removes it.
                labels.push((LABEL_RUN.to_owned(), run));
            }
            api.create_volume(&name, &labels)?;
        }
        self.update(job_id, |record| {
            if !record.caches.contains(&name) {
                record.caches.push(name.clone());
            }
        });
        Ok(lease)
    }

    /// Take the OS lock for one cache volume without waiting. `Ok(None)`
    /// means another holder (this daemon or another) has it; any other
    /// failure is an error, never a silent private copy. A lock directory
    /// that cannot be created fails open to this daemon's in-memory
    /// bookkeeping.
    fn try_lock(&self, volume: &str) -> io::Result<Option<std::fs::File>> {
        if std::fs::create_dir_all(&self.lock_dir).is_err() {
            return std::fs::File::open("/dev/null").map(Some);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.lock_dir, std::fs::Permissions::from_mode(0o700));
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_dir.join(format!("{volume}.lock")))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    /// Remove everything labelled with `run`.
    pub fn teardown(&self, run: &str) -> Teardown {
        match &self.api {
            Some(api) => api.teardown_run(run),
            None => Teardown::default(),
        }
    }

    /// Runs recorded by a previous daemon process for this state dir.
    pub fn orphaned_runs(&self) -> Vec<RunRecord> {
        read_ledger(&self.ledger).unwrap_or_default()
    }

    /// Run labels carrying this daemon's label but naming no live run.
    pub fn stray_runs(&self) -> io::Result<BTreeSet<String>> {
        let Some(api) = &self.api else {
            return Ok(BTreeSet::new());
        };
        let live: BTreeSet<String> = self
            .inner
            .lock()
            .unwrap()
            .runs
            .values()
            .map(|run| run.record.run.clone())
            .collect();
        let mut stray = BTreeSet::new();
        for kind in ["containers", "networks", "volumes"] {
            for (_, labels) in api.labelled_with_labels(kind, LABEL_RUN)? {
                let ours = labels.get(LABEL_DAEMON).and_then(|v| v.as_str()) == Some(&self.daemon);
                if let Some(run) = labels.get(LABEL_RUN).and_then(|v| v.as_str())
                    && ours
                    && !live.contains(run)
                {
                    stray.insert(run.to_owned());
                }
            }
        }
        Ok(stray)
    }

    /// Clear the ledger after reaping.
    pub fn clear_ledger(&self) {
        self.persist();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Lease {
    /// A pooled (or shared) cache volume that outlives the run.
    Kept(String),
    /// Every replica was busy: a cold volume removed with the run.
    Private(String),
}
impl Lease {
    pub fn name(&self) -> &str {
        match self {
            Self::Kept(name) | Self::Private(name) => name,
        }
    }
}

/// A job's [`VolumePolicy`]: its cache rules plus run labels for any other
/// volume it creates.
pub struct JobVolumes {
    pub runners: Arc<Runners>,
    pub job_id: u64,
    pub run: String,
    pub rules: Vec<CacheRule>,
    pub keys: CacheKeys,
    pub notes: Option<Arc<dyn Fn(String) + Send + Sync>>,
    memo: Mutex<BTreeMap<String, String>>,
}

/// Hashed cache-key components for one job.
#[derive(Clone, Debug)]
pub struct CacheKeys {
    pub repo: String,
    pub workspace: String,
}
impl CacheKeys {
    pub fn key(&self, scope: CacheScope) -> &str {
        match scope {
            CacheScope::Machine => "m",
            CacheScope::Repo => &self.repo,
            CacheScope::Workspace => &self.workspace,
        }
    }
}

impl JobVolumes {
    /// The suffix that makes this run's container and volume names unique.
    pub fn name_suffix(&self) -> String {
        run_suffix(&self.run)
    }
    pub fn new(
        runners: Arc<Runners>,
        job_id: u64,
        rules: Vec<CacheRule>,
        keys: CacheKeys,
        notes: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> Self {
        let run = runners.run_label(job_id);
        Self {
            runners,
            job_id,
            run,
            rules,
            keys,
            notes,
            memo: Mutex::default(),
        }
    }

    fn lease(&self, rule: &CacheRule) -> io::Result<String> {
        let memo_key = format!("rule:{}", rule.name);
        if let Some(name) = self.memo.lock().unwrap().get(&memo_key) {
            return Ok(name.clone());
        }
        let lease = self
            .runners
            .lease_cache(self.job_id, rule, self.keys.key(rule.scope))?;
        if let (Lease::Private(name), Some(notes)) = (&lease, &self.notes) {
            notes(format!(
                "[bosn] every replica of cache {} is in use by other runs; this run gets a cold private volume {name}",
                rule.name
            ));
        }
        let name = lease.name().to_owned();
        self.memo.lock().unwrap().insert(memo_key, name.clone());
        Ok(name)
    }
}

impl VolumePolicy for JobVolumes {
    fn map_volume(&self, name: &str) -> io::Result<String> {
        if let Some(rule) = self
            .rules
            .iter()
            .find(|r| r.volume.as_deref() == Some(name))
        {
            return self.lease(rule);
        }
        if let Some(mapped) = self.memo.lock().unwrap().get(name) {
            return Ok(mapped.clone());
        }
        let suffix = format!("-{}", self.name_suffix());
        if name.starts_with("bosn-cache-") || name.ends_with(&suffix) {
            // Already a mapped name (for example a nested create): keep it.
            return Ok(name.to_owned());
        }
        let Some(api) = self.runners.api() else {
            return Ok(name.to_owned());
        };
        // A volume that already exists is someone's deliberate state: use it
        // as is. A volume the job brings into existence is the run's own,
        // under a run-unique name: act names per-job volumes after the
        // workflow and job only, so two runs of one workflow would otherwise
        // share (and delete) each other's `GITHUB_ENV` volume.
        let mapped = if api.volume_exists(name)? {
            name.to_owned()
        } else {
            let mapped = format!("{name}{suffix}");
            if !api.volume_exists(&mapped)? {
                api.create_volume(
                    &mapped,
                    &[
                        (LABEL_RUN.to_owned(), self.run.clone()),
                        (LABEL_DAEMON.to_owned(), self.runners.daemon().to_owned()),
                        (LABEL_JOB.to_owned(), self.job_id.to_string()),
                    ],
                )?;
            }
            mapped
        };
        self.memo
            .lock()
            .unwrap()
            .insert(name.to_owned(), mapped.clone());
        Ok(mapped)
    }

    fn injected_mounts(&self) -> io::Result<Vec<(String, String)>> {
        self.rules
            .iter()
            .filter(|rule| rule.volume.is_none())
            .filter_map(|rule| rule.destination.clone().map(|d| (d, rule)))
            .map(|(destination, rule)| Ok((destination, self.lease(rule)?)))
            .collect()
    }
}

/// `b` plus 8 hex of the run label: short, and unique per run.
pub fn run_suffix(run: &str) -> String {
    format!("b{}", short_hash(run.as_bytes(), 8))
}

/// A short, stable identity for a state directory (its canonical path).
pub fn state_identity(state_dir: &Path) -> String {
    let canonical = std::fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_owned());
    short_hash(canonical.to_string_lossy().as_bytes(), 16)
}

pub fn short_hash(bytes: &[u8], hex: usize) -> String {
    let digest = kernal_api::hash::blake3_bytes(bytes).to_hex();
    digest.chars().take(hex).collect()
}

/// The directory holding this daemon's proxy sockets: beside the registry
/// when a socket path there fits `sun_path`, else a short owner-private
/// directory under `/tmp`, like the daemon's own endpoint (#364).
pub fn proxy_dir(state_dir: &Path) -> PathBuf {
    let beside = state_dir.join("dp");
    if beside.as_os_str().len() + 1 + SOCKET_NAME_MAX < 100 {
        return beside;
    }
    let uid = kernal_api::platform::ipc::current_user_id().unwrap_or_else(|_| "0".into());
    PathBuf::from(format!("/tmp/bosn-{uid}")).join(format!("dp-{}", state_identity(state_dir)))
}

/// Create the proxy directory owner-only. It must exist before Docker binds
/// it into a setup container.
pub fn ensure_proxy_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = dir.parent()
            && parent.starts_with("/tmp")
        {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Release an exclusive lease's OS lock explicitly. A `flock` belongs to the
/// open file description, which a child forked by any other thread shares
/// until it execs; closing this daemon's fd alone would leave the replica
/// locked for that window (#406). Unlocking releases it for every copy.
fn release_lock(file: &std::fs::File) {
    if let Err(error) = file.unlock() {
        eprintln!("bosn runners: cache lock release failed: {error}");
    }
}

fn write_ledger(path: &Path, records: &[RunRecord]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(records).map_err(io::Error::other)?,
    )?;
    std::fs::rename(temporary, path)
}

fn read_ledger(path: &Path) -> io::Result<Vec<RunRecord>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runners(dir: &Path) -> Runners {
        let capacity = RunnerCapacity::defaults(Some(4));
        Runners::new(dir, capacity)
            .with_api(None)
            .with_lock_dir(dir.join("locks"))
    }

    #[test]
    fn two_daemons_never_lease_the_same_exclusive_replica() {
        let dir = tempfile::tempdir().unwrap();
        let locks = dir.path().join("shared-locks");
        let make = |state: &str| {
            Runners::new(&dir.path().join(state), RunnerCapacity::defaults(Some(4)))
                .with_api(None)
                .with_lock_dir(locks.clone())
        };
        let (first, second) = (make("a"), make("b"));
        let rule = CacheRule {
            replicas: 2,
            ..CacheRule::act_toolcache()
        };
        first.begin(1, "/w", "s", "t", None);
        second.begin(1, "/w", "s", "t", None);
        second.begin(2, "/w", "s", "t", None);
        let a = first.lease_cache(1, &rule, "m").unwrap();
        let b = second.lease_cache(1, &rule, "m").unwrap();
        assert_eq!(a, Lease::Kept("bosn-cache-m-act-toolcache-0".into()));
        assert_eq!(b, Lease::Kept("bosn-cache-m-act-toolcache-1".into()));
        assert!(matches!(
            second.lease_cache(2, &rule, "m").unwrap(),
            Lease::Private(_)
        ));
        // A finished run (or a dead daemon: the kernel drops its locks)
        // frees the replica for the other daemon.
        first.finish(1);
        second.begin(3, "/w", "s", "t", None);
        assert_eq!(second.lease_cache(3, &rule, "m").unwrap(), a);
    }

    /// A child forked by any other thread (the daemon spawns act, docker
    /// and git all the time) inherits a duplicate of every open lock fd
    /// until it execs. `finish` must release the lock itself, not rely on
    /// closing its own fd being the last reference (#406).
    #[test]
    fn finish_releases_the_lock_even_while_a_forked_child_holds_its_fd() {
        let dir = tempfile::tempdir().unwrap();
        let runners = runners(dir.path());
        let rule = CacheRule {
            replicas: 1,
            ..CacheRule::act_toolcache()
        };
        let replica = Lease::Kept("bosn-cache-m-act-toolcache-0".into());
        runners.begin(1, "/w", "s", "t", None);
        assert_eq!(runners.lease_cache(1, &rule, "m").unwrap(), replica);
        let inherited = runners.inner.lock().unwrap().locks["bosn-cache-m-act-toolcache-0"]
            .1
            .try_clone()
            .unwrap();
        runners.finish(1);
        let other_daemon = Runners::new(&dir.path().join("b"), RunnerCapacity::defaults(Some(4)))
            .with_api(None)
            .with_lock_dir(dir.path().join("locks"));
        other_daemon.begin(2, "/w", "s", "t", None);
        assert_eq!(other_daemon.lease_cache(2, &rule, "m").unwrap(), replica);
        runners.begin(3, "/w", "s", "t", None);
        assert!(matches!(
            runners.lease_cache(3, &rule, "m").unwrap(),
            Lease::Private(_)
        ));
        drop(inherited);
    }

    #[test]
    fn begin_update_finish_is_mirrored_in_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let runners = runners(dir.path());
        let (record, activity) = runners.begin(7, "/w", "act", "act-ci", Some(2));
        assert!(record.run.ends_with("-7"));
        assert_eq!(record.nano_cpus, 4_000_000_000);
        runners.update(7, |r| r.container = Some("bosn-setup-x".into()));
        let ledger = read_ledger(&dir.path().join("runners/active.json")).unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].container.as_deref(), Some("bosn-setup-x"));
        assert_eq!(
            runners.orphaned_runs(),
            ledger,
            "a successor reads the same"
        );
        activity.touch();
        assert!(runners.activity(7).is_some());
        assert_eq!(runners.views()[0].record.slot, Some(2));
        let labels = runners.labels(&record);
        assert_eq!(labels[LABEL_RUN], record.run);
        assert_eq!(labels[LABEL_SLOT], "2");
        assert_eq!(labels[LABEL_DAEMON], runners.daemon());
        runners.finish(7).unwrap();
        assert!(
            read_ledger(&dir.path().join("runners/active.json"))
                .unwrap()
                .is_empty()
        );
        assert!(runners.finish(7).is_none());
    }

    #[test]
    fn exclusive_caches_hand_each_concurrent_run_its_own_replica() {
        let dir = tempfile::tempdir().unwrap();
        let runners = runners(dir.path());
        let rule = CacheRule {
            replicas: 2,
            ..CacheRule::act_toolcache()
        };
        for job in [1, 2, 3] {
            runners.begin(job, "/w", "s", "t", None);
        }
        let a = runners.lease_cache(1, &rule, "m").unwrap();
        let again = runners.lease_cache(1, &rule, "m").unwrap();
        let b = runners.lease_cache(2, &rule, "m").unwrap();
        let c = runners.lease_cache(3, &rule, "m").unwrap();
        assert_eq!(a, Lease::Kept("bosn-cache-m-act-toolcache-0".into()));
        assert_eq!(again, a, "a run keeps its replica for every container");
        assert_eq!(b, Lease::Kept("bosn-cache-m-act-toolcache-1".into()));
        assert!(matches!(c, Lease::Private(ref name) if name.contains("-x3-")));
        // Releasing a replica makes it the next run's warm cache.
        runners.finish(1);
        runners.begin(4, "/w", "s", "t", None);
        assert_eq!(runners.lease_cache(4, &rule, "m").unwrap(), a);
        // Shared caches are never exclusive.
        let shared = CacheRule {
            mode: CacheMode::Shared,
            ..rule
        };
        assert_eq!(
            runners.lease_cache(2, &shared, "k").unwrap(),
            runners.lease_cache(3, &shared, "k").unwrap()
        );
    }

    #[test]
    fn proxy_sockets_fit_sun_path_even_for_long_state_dirs() {
        let short = Path::new("/home/u/.local/state/bosn");
        assert_eq!(proxy_dir(short), short.join("dp"));
        let long = PathBuf::from(format!("/home/u/{}", "x".repeat(120)));
        let dir = proxy_dir(&long);
        assert!(dir.starts_with("/tmp"));
        let runners = Runners::new(&long, RunnerCapacity::defaults(Some(1))).with_api(None);
        if let Some(socket) = runners.proxy_socket(u64::MAX) {
            assert!(socket.as_os_str().len() < 108, "{}", socket.display());
        }
    }
}
