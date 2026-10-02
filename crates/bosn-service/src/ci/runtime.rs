//! The daemon's CI runtime: admits submissions through the scheduler,
//! executes admitted runs on isolated engines, and answers typed requests.
//! Files go through [`Store`]; verdicts come from [`super::report`].

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use bosn_registry::act::{ActEngineIntent, ActEngineRecord};
use kernal_api::async_engine::{self, CancellationSource};
use serde_json::{Value, json};

use super::{
    engine::{
        ACT_VERSION, ActEngineBackend, ActInvocation, EngineLine, RUNNER_IMAGE, act_artifact,
    },
    lifecycle::{self, CleanupEnd, EngineObserver, EnginePlan, EngineReport, ExecutionEnd},
    model::{ActParser, LogRecord, RunTree, parse_act_list},
    provider, report,
    scheduler::{Admission, Scheduler},
    store::{INDEX_STRIDE, LogFilter, LogQuery, LogWriter, Settings, Store},
    wire::*,
};
use crate::RegistryActor;

/// Finished runs kept, and how many of them keep their source for retry.
const KEEP_RUNS: usize = 200;
const KEEP_SOURCES: usize = 10;
/// How often a running run's progress becomes visible to readers.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(250);

struct RunSlot {
    record: RunRecord,
    cancel: Option<CancellationSource>,
    /// Sparse (seq, byte offset) index into the run's log.
    index: Vec<(u64, u64)>,
}

struct CiState {
    scheduler: Scheduler,
    runs: BTreeMap<String, RunSlot>,
    /// Run IDs, oldest first.
    order: Vec<String>,
}

impl CiState {
    fn slot(&self, run: &str) -> Result<&RunSlot, CiError> {
        self.runs
            .get(run)
            .ok_or_else(|| CiError::new("not_found", format!("no run {run}")))
    }
    fn insert(&mut self, record: RunRecord) {
        self.order.push(record.id.clone());
        self.runs.insert(
            record.id.clone(),
            RunSlot {
                record,
                cancel: None,
                index: Vec::new(),
            },
        );
    }
}

/// The daemon-side CI runtime. Cheap to clone.
#[derive(Clone)]
pub struct CiRuntime {
    store: Store,
    state: Arc<Mutex<CiState>>,
    kick: async_engine::Sender<()>,
    registry: RegistryActor,
    backend: Arc<dyn ActEngineBackend>,
}

impl CiRuntime {
    /// Load persisted runs (marking any a previous daemon left unfinished as
    /// interrupted; their engines are reconciled by [`Self::recover_engines`])
    /// and start the dispatcher.
    pub fn start(
        state_dir: &Path,
        registry: RegistryActor,
        backend: Arc<dyn ActEngineBackend>,
        default_limit: usize,
    ) -> Self {
        let store = Store::open(state_dir).unwrap_or_else(|_| Store::new(state_dir));
        let settings = store.load_settings().unwrap_or(Settings {
            limit: default_limit,
            drained: false,
        });
        let mut scheduler = Scheduler::new(settings.limit);
        scheduler.set_drained(settings.drained);
        let mut state = CiState {
            scheduler,
            runs: BTreeMap::new(),
            order: Vec::new(),
        };
        for mut record in store.load_runs() {
            if record.state != RunState::Done {
                record.finish(
                    Conclusion::Error,
                    Some("interrupted: the daemon stopped during this run".into()),
                );
                store.save_run(&record);
            }
            state.insert(record);
        }
        let (kick, mut kicked) = async_engine::channel(64);
        let runtime = Self {
            store,
            state: Arc::new(Mutex::new(state)),
            kick,
            registry,
            backend,
        };
        let dispatcher = runtime.clone();
        async_engine::launch(async move {
            while kicked.recv().await.is_some() {
                dispatcher.dispatch();
            }
        })
        .detach();
        runtime
    }

    /// Engine records a previous daemon left unfinished. Call before the
    /// daemon accepts requests; reconcile the result with
    /// [`Self::reconcile_engines`] (it may take a while, so in the background).
    pub async fn pending_engines(&self) -> Result<Vec<ActEngineRecord>, String> {
        lifecycle::pending_records(&self.registry).await
    }

    pub async fn reconcile_engines(
        &self,
        records: &[ActEngineRecord],
    ) -> lifecycle::RecoveryReport {
        lifecycle::reconcile(&self.registry, self.backend.as_ref(), records).await
    }

    pub async fn handle(&self, request: CiRequest) -> Result<Value, CiError> {
        match request {
            CiRequest::Submit { request } => self.submit(request).await,
            CiRequest::List {
                workspace,
                state,
                limit,
            } => Ok(self.list(workspace.as_deref(), state, limit.unwrap_or(50).min(500))),
            CiRequest::Show { run, tree } => self.show(&run, tree.unwrap_or(true)),
            CiRequest::Logs {
                run,
                job,
                section,
                since_seq,
                limit,
                max_bytes,
            } => self.logs(
                &run,
                LogFilter {
                    job: job.as_deref(),
                    section: section.as_deref(),
                },
                since_seq.unwrap_or(0),
                limit.unwrap_or(DEFAULT_LOG_PAGE_RECORDS).clamp(1, 5_000),
                max_bytes
                    .unwrap_or(DEFAULT_LOG_PAGE_BYTES)
                    .clamp(1024, MAX_LOG_PAGE_BYTES),
            ),
            CiRequest::Cancel { run } => self.cancel(&run),
            CiRequest::Retry { run, job } => self.retry(&run, job).await,
            CiRequest::Report { run, tail } => {
                self.report(&run, tail.unwrap_or(DEFAULT_REPORT_TAIL).clamp(1, 500))
                    .await
            }
            CiRequest::Runners { action } => self.runners(action).await,
        }
    }

    fn lock(&self) -> MutexGuard<'_, CiState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn kick(&self) {
        let _ = self.kick.try_send(());
    }

    pub(crate) fn record(&self, run: &str) -> Result<RunRecord, CiError> {
        self.lock().slot(run).map(|s| s.record.clone())
    }

    /// Apply `change` to a run's record and persist the result.
    fn update(&self, run: &str, change: impl FnOnce(&mut RunSlot)) -> Option<RunRecord> {
        let mut state = self.lock();
        let slot = state.runs.get_mut(run)?;
        change(slot);
        let record = slot.record.clone();
        self.store.save_run(&record);
        Some(record)
    }

    async fn submit(&self, request: SubmitRequest) -> Result<Value, CiError> {
        let staging = self.store.staging(&request.staging);
        let admitted = async {
            request.validate()?;
            if !staging.join("source").is_dir() {
                return Err(CiError::refused("staged snapshot is missing"));
            }
            let (event, payload) = provider::github_event(
                request.trigger,
                request.mode,
                &request.sha,
                request.branch.as_deref(),
                &provider::repository(request.origin.as_deref()),
                request.pr_number.unwrap_or(1),
            );
            let payload = serde_json::to_vec_pretty(&payload).unwrap_or_default();
            let record = RunRecord::queued(new_uuid().await?, &request, event, &payload);
            self.admit(record, &staging, &payload)
        }
        .await;
        if admitted.is_err() && valid_uuid(&request.staging) {
            let _ = std::fs::remove_dir_all(&staging);
        }
        admitted
    }

    /// Queue (or coalesce) a record whose source is staged at `staging`.
    fn admit(&self, record: RunRecord, staging: &Path, payload: &[u8]) -> Result<Value, CiError> {
        let mut state = self.lock();
        let id = match state.scheduler.submit(record.key(), record.id.clone()) {
            Admission::Coalesced(existing) => {
                drop(state);
                let _ = std::fs::remove_dir_all(staging);
                let joined = self
                    .update(&existing, |slot| slot.record.submitters += 1)
                    .ok_or_else(|| CiError::new("internal", "coalesced run is missing"))?;
                return Ok(json!({"run": existing, "coalesced": true, "record": joined.summary()}));
            }
            Admission::Queued(id) => id,
        };
        if let Err(error) = self.store.place(&id, staging, payload) {
            state.scheduler.finish(&id);
            return Err(CiError::new(
                "internal",
                format!("cannot place snapshot: {error}"),
            ));
        }
        self.store.save_run(&record);
        let summary = record.summary();
        state.insert(record);
        let position = state.scheduler.queue_position(&id);
        drop(state);
        self.kick();
        Ok(json!({"run": id, "coalesced": false, "queue_position": position, "record": summary}))
    }

    fn list(&self, workspace: Option<&str>, filter: Option<RunState>, limit: usize) -> Value {
        let state = self.lock();
        let runs: Vec<Value> = state
            .order
            .iter()
            .rev()
            .filter_map(|id| state.runs.get(id))
            .filter(|slot| workspace.is_none_or(|w| slot.record.workspace == w))
            .filter(|slot| filter.is_none_or(|f| slot.record.state == f))
            .take(limit)
            .map(|slot| slot.record.summary())
            .collect();
        json!({"runs": runs, "runners": runner_status(&state.scheduler)})
    }

    fn show(&self, run: &str, tree: bool) -> Result<Value, CiError> {
        let record = self.record(run)?;
        if !tree {
            return Ok(record.summary());
        }
        let mut value = serde_json::to_value(&record).unwrap_or(Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "exit_code".into(),
                json!(record.conclusion.map(Conclusion::exit_code)),
            );
        }
        Ok(value)
    }

    fn logs(
        &self,
        run: &str,
        filter: LogFilter<'_>,
        since: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Value, CiError> {
        let (visible, offset, done) = {
            let state = self.lock();
            let slot = state.slot(run)?;
            let offset = slot
                .index
                .iter()
                .rev()
                .find(|(seq, _)| *seq <= since + 1)
                .map_or(0, |(_, offset)| *offset);
            (
                slot.record.log_records,
                offset,
                slot.record.state == RunState::Done,
            )
        };
        let page = self.store.read_log(
            run,
            offset,
            &LogQuery {
                since,
                visible,
                filter,
                limit,
                max_bytes,
            },
        );
        Ok(json!({
            "run": run,
            "records": page.records,
            "next_seq": page.next_seq,
            "total_records": visible,
            "more": page.truncated || page.next_seq < visible,
            "done": done,
        }))
    }

    fn cancel(&self, run: &str) -> Result<Value, CiError> {
        let mut state = self.lock();
        if state.scheduler.cancel_queued(run) {
            drop(state);
            self.update(run, |slot| {
                slot.record
                    .finish(Conclusion::Cancelled, Some("cancelled while queued".into()));
            });
            return Ok(json!({"run": run, "cancelled": true, "state": RunState::Done}));
        }
        let slot = state.slot(run)?;
        let cancelled = match (&slot.cancel, slot.record.state) {
            (Some(source), RunState::Running) => {
                source.cancel();
                true
            }
            _ => false,
        };
        Ok(json!({"run": run, "cancelled": cancelled, "state": slot.record.state}))
    }

    async fn retry(&self, run: &str, job: Option<String>) -> Result<Value, CiError> {
        let original = self.record(run)?;
        if original.state != RunState::Done {
            return Err(CiError::refused("the run is still in progress"));
        }
        if job.as_deref().is_some_and(|j| !valid_job(j)) {
            return Err(CiError::refused("invalid job ID"));
        }
        if !self.store.source(run).is_dir() {
            return Err(CiError::refused(
                "this run's source snapshot was pruned; submit a new run",
            ));
        }
        let id = new_uuid().await?;
        // A copy, so the retry stays independent of the original's pruning.
        let (store, from, staging_id) = (self.store.clone(), run.to_string(), id.clone());
        let staged = blocking(move || {
            let staging = store.stage_copy(&from, &staging_id)?;
            let payload = std::fs::read(store.event(&from))?;
            Ok::<_, std::io::Error>((staging, payload))
        })
        .await?;
        let (staging, payload) =
            staged.map_err(|e| CiError::new("internal", format!("cannot copy snapshot: {e}")))?;
        self.admit(original.retry(id, job), &staging, &payload)
    }

    async fn report(&self, run: &str, tail: usize) -> Result<Value, CiError> {
        let record = self.record(run)?;
        let store = self.store.clone();
        blocking(move || {
            report::report(&record, |job, section| {
                let filter = LogFilter {
                    job: Some(job),
                    section: Some(section),
                };
                store.tail(&record.id, filter, tail)
            })
        })
        .await
    }

    async fn runners(&self, action: RunnerAction) -> Result<Value, CiError> {
        let mut pruned = None;
        if let RunnerAction::PruneCache {
            older_than_secs,
            max_bytes,
        } = action
        {
            pruned = Some(self.prune(older_than_secs, max_bytes).await?);
        }
        let mut state = self.lock();
        match action {
            RunnerAction::List | RunnerAction::PruneCache { .. } => {}
            RunnerAction::Drain => state.scheduler.set_drained(true),
            RunnerAction::Resume => state.scheduler.set_drained(false),
            RunnerAction::SetLimit { limit } => {
                if !(1..=256).contains(&limit) {
                    return Err(CiError::refused("limit must be between 1 and 256"));
                }
                state.scheduler.set_limit(limit);
            }
        }
        self.store.save_settings(&Settings {
            limit: state.scheduler.limit(),
            drained: state.scheduler.drained(),
        });
        let status = runner_status(&state.scheduler);
        drop(state);
        self.kick();
        Ok(json!({"runners": status, "pruned_runs": pruned}))
    }

    /// Remove finished runs by age, total size, or beyond the retention
    /// counts, and drop sources of all but the newest few. Live runs are
    /// kept. Sizes are measured and files deleted off the state lock.
    async fn prune(
        &self,
        older_than: Option<u64>,
        max_bytes: Option<u64>,
    ) -> Result<Vec<String>, CiError> {
        let sizes = match max_bytes {
            None => BTreeMap::new(),
            Some(_) => {
                let finished = self.lock().finished_newest_first();
                let store = self.store.clone();
                blocking(move || {
                    finished
                        .into_iter()
                        .map(|id| {
                            let size = store.run_bytes(&id);
                            (id, size)
                        })
                        .collect()
                })
                .await?
            }
        };
        let plan = self.lock().plan_prune(older_than, max_bytes, &sizes);
        let store = self.store.clone();
        let pruned = plan.runs.clone();
        blocking(move || plan.apply(&store)).await?;
        Ok(pruned)
    }

    /// Start every run the scheduler admits. Each is marked running in the
    /// same critical section, so a cancel never sees an admitted run that is
    /// neither queued nor running.
    fn dispatch(&self) {
        let started: Vec<(RunRecord, CancellationSource)> = {
            let mut state = self.lock();
            let admitted = state.scheduler.admit();
            admitted
                .into_iter()
                .filter_map(|id| {
                    let slot = state.runs.get_mut(&id)?;
                    let cancel = CancellationSource::new();
                    slot.cancel = Some(cancel.clone());
                    slot.record.state = RunState::Running;
                    slot.record.started_at = Some(lifecycle::now_seconds());
                    self.store.save_run(&slot.record);
                    Some((slot.record.clone(), cancel))
                })
                .collect()
        };
        for (record, cancel) in started {
            let id = record.id.clone();
            let runtime = self.clone();
            let task = async_engine::launch(async move { runtime.execute(record, cancel).await });
            // A panicking run must still free its slot and end its record.
            let watchdog = self.clone();
            async_engine::launch(async move {
                if task.await.is_err() {
                    watchdog.complete(&id, |record| {
                        record.finish(
                            Conclusion::Error,
                            Some("internal error: the run task failed".into()),
                        )
                    });
                }
            })
            .detach();
        }
    }

    async fn execute(&self, record: RunRecord, cancel: CancellationSource) {
        let mut observer = RunObserver {
            runtime: self.clone(),
            id: record.id.clone(),
            log: self.store.log_writer(&record.id),
            parser: ActParser::default(),
            seq: 0,
            last_publish: Instant::now(),
        };
        let outcome = self.drive(&record, &cancel, &mut observer).await;
        observer.publish();
        let mut tree = std::mem::take(&mut observer.parser.tree);
        let (conclusion, reason) = report::conclude(&outcome, &mut tree);
        self.complete(&record.id, |record| {
            record.tree = tree;
            record.finish(conclusion, reason);
            if let Ok(report) = &outcome {
                record_engine_report(record, report);
            }
        });
        let _ = self.prune(None, None).await;
    }

    /// End a running run: apply `finish`, persist it before readers can see
    /// `done` (so a restart never reads a stale in-progress record), and
    /// free its scheduler slot.
    fn complete(&self, run: &str, finish: impl FnOnce(&mut RunRecord)) {
        let mut state = self.lock();
        if let Some(slot) = state.runs.get_mut(run)
            && slot.record.state != RunState::Done
        {
            slot.cancel = None;
            finish(&mut slot.record);
            self.store.save_run(&slot.record);
        }
        state.scheduler.finish(run);
        drop(state);
        self.kick();
    }

    async fn drive(
        &self,
        record: &RunRecord,
        cancel: &CancellationSource,
        observer: &mut RunObserver,
    ) -> Result<EngineReport, String> {
        let plan = self.plan(record).await?;
        observer.note(&format!(
            "run {} sha {}{} workflow {} trigger {} mode {} actor {}",
            record.id,
            record.sha,
            record
                .dirty
                .as_ref()
                .map(|d| format!(" +dirty:{}", &d[..12]))
                .unwrap_or_default(),
            record.workflow,
            record.trigger.as_str(),
            record.mode.as_str(),
            record.actor
        ));
        Ok(lifecycle::run_on_engine(
            &self.registry,
            self.backend.as_ref(),
            &plan,
            &cancel.token(),
            observer,
        )
        .await)
    }

    /// The immutable engine intent and act invocation for a record.
    async fn plan(&self, record: &RunRecord) -> Result<EnginePlan, String> {
        let artifact = act_artifact(std::env::consts::ARCH)
            .ok_or("no pinned act build for this host architecture")?;
        let (_, runner_digest) = RUNNER_IMAGE
            .rsplit_once('@')
            .ok_or("runner image is not pinned")?;
        Ok(EnginePlan {
            act: artifact,
            intent: ActEngineIntent {
                run_id: record.id.clone(),
                workspace: record.workspace.clone(),
                candidate_sha: record.sha.clone(),
                payload_sha256: record.payload_sha256.clone(),
                snapshot_sha256: record.tree_digest.clone(),
                act_version: ACT_VERSION.into(),
                act_image_digest: format!("sha256:{}", artifact.sha256),
                engine_image_digest: self.backend.resolve_engine_image().await?,
                runner_image_digest: runner_digest.into(),
                created_at: lifecycle::now_seconds(),
            },
            source: self.store.source(&record.id),
            event: self.store.event(&record.id),
            invocation: ActInvocation {
                event: record.event.clone(),
                workflow: record.workflow.clone(),
                job: record.job.clone(),
            },
            deadline: Duration::from_secs(record.timeout_secs),
        })
    }
}

fn runner_status(scheduler: &Scheduler) -> Value {
    json!({
        "limit": scheduler.limit(),
        "running": scheduler.running(),
        "queued": scheduler.queued(),
        "drained": scheduler.drained(),
        "engine": "act",
        "act_version": ACT_VERSION,
    })
}

fn record_engine_report(record: &mut RunRecord, report: &EngineReport) {
    record.engine_id = report.engine_id.clone();
    record.act_exit_code = match report.execution {
        ExecutionEnd::Exited(code) => Some(code),
        _ => None,
    };
    record.cleanup = Some(match &report.cleanup {
        CleanupEnd::Removed => "removed".into(),
        CleanupEnd::Failed(e) => format!("failed: {e}"),
    });
}

/// Run blocking file work on the kernel's blocking lane.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, CiError> {
    async_engine::launch_blocking(work)
        .await
        .map_err(|_| CiError::new("internal", "blocking task failed"))
}

/// What one prune pass deletes: decided under the lock, applied outside it.
struct PrunePlan {
    runs: Vec<String>,
    sources: Vec<String>,
}
impl PrunePlan {
    fn apply(&self, store: &Store) {
        for id in &self.runs {
            store.remove_run(id);
        }
        for id in &self.sources {
            store.remove_source(id);
        }
    }
}

impl CiState {
    fn finished_newest_first(&self) -> Vec<String> {
        self.order
            .iter()
            .rev()
            .filter(|id| self.runs[*id].record.state == RunState::Done)
            .cloned()
            .collect()
    }

    /// Drop pruned runs from the state now; their files go in `apply`.
    fn plan_prune(
        &mut self,
        older_than: Option<u64>,
        max_bytes: Option<u64>,
        sizes: &BTreeMap<String, u64>,
    ) -> PrunePlan {
        let now = lifecycle::now_seconds();
        let mut plan = PrunePlan {
            runs: Vec::new(),
            sources: Vec::new(),
        };
        let mut kept_bytes = 0;
        for (newest_first, id) in self.finished_newest_first().into_iter().enumerate() {
            let age = now - self.runs[&id].record.created_at;
            let size = sizes.get(&id).copied().unwrap_or(0);
            if newest_first >= KEEP_RUNS
                || older_than.is_some_and(|s| age > s as f64)
                || max_bytes.is_some_and(|m| kept_bytes + size > m)
            {
                self.runs.remove(&id);
                plan.runs.push(id);
                continue;
            }
            if newest_first >= KEEP_SOURCES {
                plan.sources.push(id);
            }
            kept_bytes += size;
        }
        self.order.retain(|id| self.runs.contains_key(id));
        plan
    }
}

/// Streams one run's output into its log, parser and published record.
struct RunObserver {
    runtime: CiRuntime,
    id: String,
    log: Option<LogWriter>,
    parser: ActParser,
    seq: u64,
    last_publish: Instant,
}

impl RunObserver {
    fn append(&mut self, record: LogRecord) {
        if let Some(log) = &mut self.log {
            let offset = log.append(&record);
            if record.seq % INDEX_STRIDE == 1 {
                let mut state = self.runtime.lock();
                if let Some(slot) = state.runs.get_mut(&self.id) {
                    slot.index.push((record.seq, offset));
                }
            }
        }
        if self.last_publish.elapsed() > PUBLISH_INTERVAL {
            self.publish();
        }
    }

    /// Flush the log, then make its records and the current tree visible.
    fn publish(&mut self) {
        if let Some(log) = &mut self.log {
            log.flush();
        }
        self.last_publish = Instant::now();
        let (seq, tree) = (self.seq, self.parser.tree.clone());
        self.runtime.update(&self.id, |slot| {
            slot.record.log_records = seq;
            slot.record.tree = tree;
        });
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

impl EngineObserver for RunObserver {
    fn note(&mut self, text: &str) {
        let seq = self.next_seq();
        self.append(LogRecord {
            seq,
            stream: "bosn".into(),
            job: None,
            section: None,
            text: text.into(),
        });
    }
    fn declared(&mut self, listing: &str) {
        self.parser = ActParser::new(RunTree::declared(&parse_act_list(listing)));
        self.publish();
    }
    fn line(&mut self, line: EngineLine) {
        let seq = self.next_seq();
        let record = match line {
            EngineLine::Stdout(text) => self.parser.feed(seq, &text),
            EngineLine::Stderr(text) => LogRecord {
                seq,
                stream: "stderr".into(),
                job: None,
                section: None,
                text,
            },
        };
        self.append(record);
    }
}

/// The run directory root, for tests.
#[cfg(test)]
impl CiRuntime {
    pub(crate) fn staging_dir(&self, id: &str) -> std::path::PathBuf {
        self.store.staging(id)
    }
    pub(crate) fn save(&self, record: &RunRecord) {
        self.store.save_run(record);
    }
    pub(crate) fn listing(&self) -> Value {
        self.list(None, None, 100)
    }
}
