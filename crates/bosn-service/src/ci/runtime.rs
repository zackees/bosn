//! The daemon's CI runtime: admits submissions through the scheduler,
//! executes admitted runs on isolated engines, and answers typed requests.
//! Files go through [`Store`]; verdicts come from [`super::report`].

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

use bosn_registry::act::{ActEngineIntent, ActEngineRecord};
use kernal_api::async_engine::{self, CancellationSource};
use serde_json::Value;

use super::{
    config::WidgetConfig,
    engine::{
        ACT_VERSION, ActEngineBackend, ActInvocation, CACHE_VOLUME, CacheVolume, EngineLine,
        RUNNER_IMAGE, SecretEnv, act_artifact,
    },
    events::Feed,
    lifecycle::{self, CleanupEnd, EngineObserver, EnginePlan, EngineReport, ExecutionEnd},
    model::{ActParser, LogRecord, RunTree, parse_act_list, select_jobs},
    provider,
    reply::*,
    report,
    scheduler::{Admission, Scheduler},
    store::{INDEX_STRIDE, LogFilter, LogQuery, LogWriter, Settings, Store},
    ui::UiHandle,
    widget::{LaunchTrigger, WidgetPresence, WidgetState},
    wire::*,
    workflow,
};
use crate::{RegistryActor, secrets::SecretMasker};
mod observer;
mod persist;
mod runners;
mod widget;
use observer::*;
use persist::{RecordWriter, Save};

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
    /// Bumped on every change; orders this run's record writes.
    version: u64,
}

impl RunSlot {
    /// The record's next version, to write once the lock is released.
    fn snapshot(&mut self) -> Save {
        self.version += 1;
        Save {
            record: self.record.clone(),
            version: self.version,
        }
    }
}

struct CiState {
    scheduler: Scheduler,
    runs: BTreeMap<String, RunSlot>,
    /// Run IDs, oldest first.
    order: Vec<String>,
    /// The cache volume is being removed: dispatch holds new runs.
    clearing_cache: bool,
}

impl CiState {
    fn slot(&self, run: &str) -> Result<&RunSlot, CiError> {
        self.runs
            .get(run)
            .ok_or_else(|| CiError::new("not_found", format!("no run {run}")))
    }
    /// Track a run whose record is on disk at `version`.
    fn insert(&mut self, record: RunRecord, version: u64) {
        self.order.push(record.id.clone());
        self.runs.insert(
            record.id.clone(),
            RunSlot {
                record,
                cancel: None,
                index: Vec::new(),
                version,
            },
        );
    }
}

/// The daemon-side CI runtime. Cheap to clone.
#[derive(Clone)]
pub struct CiRuntime {
    /// The daemon state directory (secrets are read from it per run).
    state_dir: std::path::PathBuf,
    store: Store,
    writer: RecordWriter,
    state: Arc<Mutex<CiState>>,
    kick: async_engine::Sender<()>,
    registry: RegistryActor,
    backend: Arc<dyn ActEngineBackend>,
    feed: Feed,
    /// Set once when the opt-in UI listener is serving.
    ui: Arc<OnceLock<Arc<UiHandle>>>,
    widget: Arc<Mutex<WidgetState>>,
    widget_config: Arc<Mutex<WidgetConfig>>,
    launch_failure_logged: Arc<std::sync::atomic::AtomicBool>,
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
            clearing_cache: false,
        };
        for mut record in store.load_runs() {
            if record.state != RunState::Done {
                record.finish(
                    Conclusion::Error,
                    Some("interrupted: the daemon stopped during this run".into()),
                );
                store.save_run(&record);
            }
            state.insert(record, 0);
        }
        let (kick, mut kicked) = async_engine::channel(64);
        let runtime = Self {
            state_dir: state_dir.to_path_buf(),
            writer: RecordWriter::new(store.clone()),
            store,
            state: Arc::new(Mutex::new(state)),
            kick,
            registry,
            backend,
            feed: Feed::new(),
            ui: Arc::new(OnceLock::new()),
            widget: Arc::new(Mutex::new(WidgetState::default())),
            widget_config: Arc::new(Mutex::new(WidgetConfig::default())),
            launch_failure_logged: Arc::default(),
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

    /// Dispatch one typed request; the reply is that operation's typed reply
    /// (see [`super::reply`]) serialized for the wire.
    pub async fn handle(&self, request: CiRequest) -> Result<Value, CiError> {
        match request {
            CiRequest::Submit { request } => wire(self.submit(request).await),
            CiRequest::List {
                workspace,
                state,
                limit,
            } => wire(Ok(self.list(
                workspace.as_deref(),
                state,
                limit.unwrap_or(50).min(500),
            ))),
            CiRequest::Show { run, tree } => wire(
                self.record(&run)
                    .map(|r| RunView::of(r, tree.unwrap_or(true))),
            ),
            CiRequest::Logs {
                run,
                job,
                section,
                since_seq,
                limit,
                max_bytes,
            } => wire(
                self.logs(
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
            ),
            CiRequest::Cancel { run } => wire(self.cancel(&run)),
            CiRequest::Retry { run, job } => wire(self.retry(&run, job).await),
            CiRequest::Report { run, tail } => wire(
                self.report(&run, tail.unwrap_or(DEFAULT_REPORT_TAIL).clamp(1, 500))
                    .await,
            ),
            CiRequest::Runners { action } => wire(self.runners(action).await),
            CiRequest::UiGrant { path } => wire(self.ui_grant(path).await),
            CiRequest::WidgetHello {
                pid,
                session,
                explicit,
            } => wire(Ok(self.widget_hello(pid, &session, explicit))),
            CiRequest::WidgetPoll { pid } => wire(Ok(self.widget_poll(pid))),
            CiRequest::WidgetDismiss { session } => wire(Ok(self.widget_dismiss(&session))),
            CiRequest::WidgetCommand { command } => wire(self.widget_command(command)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CiState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn kick(&self) {
        let _ = self.kick.try_send(());
    }

    /// Announce a change on the live feed (under the state lock, so events
    /// keep the order of changes) and return its write.
    fn announce(&self, slot: &mut RunSlot) -> Save {
        self.feed.publish(&slot.record);
        slot.snapshot()
    }

    /// Attach the UI listener so `UiGrant` can issue links for it.
    pub fn attach_ui(&self, handle: Arc<UiHandle>) {
        let _ = self.ui.set(handle);
    }

    async fn ui_grant(&self, path: Option<String>) -> Result<UiGrantReply, CiError> {
        let ui = self.ui.get().ok_or_else(|| {
            CiError::refused(
                "the dashboard is disabled; set `[ui] enabled = true` in <state>/config.toml and restart the daemon",
            )
        })?;
        let next = path.unwrap_or_else(|| "/".into());
        if !next.starts_with('/') || next.starts_with("//") || next.len() > 256 {
            return Err(CiError::refused("path must be a local dashboard path"));
        }
        let token = ui
            .auth
            .grant()
            .await
            .map_err(|e| CiError::new("internal", e))?;
        Ok(UiGrantReply {
            url: format!("{}/auth?token={token}&next={next}", ui.origin),
        })
    }

    /// The live run feed (lossy; see [`super::events`]).
    pub fn feed(&self) -> &Feed {
        &self.feed
    }

    pub(crate) fn record(&self, run: &str) -> Result<RunRecord, CiError> {
        self.lock().slot(run).map(|s| s.record.clone())
    }

    /// Apply `change` to a run's record and write the result in the
    /// background.
    fn update(&self, run: &str, change: impl FnOnce(&mut RunSlot)) -> Option<RunRecord> {
        let save = {
            let mut state = self.lock();
            let slot = state.runs.get_mut(run)?;
            change(slot);
            self.announce(slot)
        };
        let record = save.record.clone();
        self.writer.save_detached(save);
        Some(record)
    }

    async fn submit(&self, request: SubmitRequest) -> Result<SubmitReply, CiError> {
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
            self.admit(record, staging.clone(), payload).await
        }
        .await;
        if admitted.is_err() && valid_uuid(&request.staging) {
            let _ = std::fs::remove_dir_all(&staging);
        }
        admitted
    }

    /// Queue (or coalesce) a record whose source is staged at `staging`.
    /// The source is placed and the record written before the scheduler
    /// can see the run, with no lock held.
    async fn admit(
        &self,
        record: RunRecord,
        staging: PathBuf,
        payload: Vec<u8>,
    ) -> Result<SubmitReply, CiError> {
        let (store, writer, id) = (self.store.clone(), self.writer.clone(), record.id.clone());
        let first = Save {
            record: record.clone(),
            version: 1,
        };
        blocking(move || {
            let placed = store.place(&id, &staging, &payload);
            match placed {
                Ok(()) => writer.write(&first),
                Err(_) => writer.remove_run(&id),
            }
            placed
        })
        .await?
        .map_err(|e| CiError::new("internal", format!("cannot place snapshot: {e}")))?;
        let admitted = {
            let mut state = self.lock();
            match state.scheduler.submit(record.key(), record.id.clone()) {
                Admission::Coalesced(existing) => {
                    Admitted::Joined(state.runs.get_mut(&existing).map(|slot| {
                        slot.record.submitters += 1;
                        self.announce(slot)
                    }))
                }
                Admission::Queued(id) => {
                    self.feed.publish(&record);
                    let view = RunView::of(record.clone(), false);
                    state.insert(record.clone(), 1);
                    Admitted::Queued(SubmitReply {
                        queue_position: state.scheduler.queue_position(&id),
                        run: id,
                        coalesced: false,
                        record: view,
                    })
                }
            }
        };
        match admitted {
            Admitted::Queued(reply) => {
                self.kick();
                self.maybe_launch_widget(LaunchTrigger::Activity);
                Ok(reply)
            }
            Admitted::Joined(joined) => {
                // The run joined a live one: drop the copy placed for it.
                let (writer, placed) = (self.writer.clone(), record.id.clone());
                blocking(move || writer.remove_run(&placed)).await?;
                let joined =
                    joined.ok_or_else(|| CiError::new("internal", "coalesced run is missing"))?;
                let reply = SubmitReply {
                    run: joined.record.id.clone(),
                    coalesced: true,
                    queue_position: None,
                    record: RunView::of(joined.record.clone(), false),
                };
                self.writer.save_detached(joined);
                Ok(reply)
            }
        }
    }

    fn list(&self, workspace: Option<&str>, filter: Option<RunState>, limit: usize) -> ListReply {
        let state = self.lock();
        let runs = state
            .order
            .iter()
            .rev()
            .filter_map(|id| state.runs.get(id))
            .filter(|slot| workspace.is_none_or(|w| slot.record.workspace == w))
            .filter(|slot| filter.is_none_or(|f| slot.record.state == f))
            .take(limit)
            .map(|slot| RunView::of(slot.record.clone(), false))
            .collect();
        ListReply {
            runs,
            runners: runner_status(&state.scheduler, self.widget_presence()),
        }
    }

    fn logs(
        &self,
        run: &str,
        filter: LogFilter<'_>,
        since: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<LogsReply, CiError> {
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
        Ok(LogsReply {
            run: run.into(),
            more: page.truncated || page.next_seq < visible,
            records: page.records,
            next_seq: page.next_seq,
            total_records: visible,
            done,
        })
    }

    fn cancel(&self, run: &str) -> Result<CancelReply, CiError> {
        let mut state = self.lock();
        if state.scheduler.cancel_queued(run) {
            drop(state);
            self.update(run, |slot| {
                slot.record
                    .finish(Conclusion::Cancelled, Some("cancelled while queued".into()));
            });
            return Ok(CancelReply {
                run: run.into(),
                cancelled: true,
                state: RunState::Done,
            });
        }
        let slot = state.slot(run)?;
        let cancelled = match (&slot.cancel, slot.record.state) {
            (Some(source), RunState::Running) => {
                source.cancel();
                true
            }
            _ => false,
        };
        Ok(CancelReply {
            run: run.into(),
            cancelled,
            state: slot.record.state,
        })
    }

    async fn retry(&self, run: &str, job: Option<String>) -> Result<SubmitReply, CiError> {
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
        self.admit(original.retry(id, job), staging, payload).await
    }

    async fn report(&self, run: &str, tail: usize) -> Result<RunReport, CiError> {
        let record = self.record(run)?;
        let store = self.store.clone();
        let origin = self.ui.get().map(|ui| ui.origin.clone());
        blocking(move || {
            report::report(&record, origin.as_deref(), |job, section| {
                let filter = LogFilter {
                    job: Some(job),
                    section: Some(section),
                };
                store.tail(&record.id, filter, tail)
            })
        })
        .await
    }

    /// Start every run the scheduler admits. Each is marked running in the
    /// same critical section, so a cancel never sees an admitted run that is
    /// neither queued nor running.
    fn dispatch(&self) {
        let started: Vec<(Save, CancellationSource)> = {
            let mut state = self.lock();
            let admitted = if state.clearing_cache {
                Vec::new()
            } else {
                state.scheduler.admit()
            };
            admitted
                .into_iter()
                .filter_map(|id| {
                    let slot = state.runs.get_mut(&id)?;
                    let cancel = CancellationSource::new();
                    slot.cancel = Some(cancel.clone());
                    slot.record.state = RunState::Running;
                    slot.record.started_at = Some(lifecycle::now_seconds());
                    Some((self.announce(slot), cancel))
                })
                .collect()
        };
        for (save, cancel) in started {
            let record = save.record.clone();
            self.writer.save_detached(save);
            let id = record.id.clone();
            let runtime = self.clone();
            let task = async_engine::launch(async move { runtime.execute(record, cancel).await });
            // A panicking run must still free its slot and end its record.
            let watchdog = self.clone();
            async_engine::launch(async move {
                if task.await.is_err() {
                    watchdog
                        .complete(&id, |record| {
                            record.finish(
                                Conclusion::Error,
                                Some("internal error: the run task failed".into()),
                            )
                        })
                        .await;
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
            job: record.job.clone(),
            masker: SecretMasker::new(Vec::<String>::new()),
            seq: 0,
            last_publish: Instant::now(),
        };
        let outcome = self.drive(&record, &cancel, &mut observer).await;
        observer.publish();
        let mut tree = std::mem::take(&mut observer.parser.tree);
        let declared = workflow::declared_steps(&self.store.source(&record.id), &record.workflow);
        let (conclusion, mut reason) = report::conclude(&outcome, &mut tree, &declared);
        let lost = observer.log.as_ref().map_or(observer.seq, LogWriter::lost);
        if lost > 0 {
            let note = format!("{lost} log records could not be written");
            reason = Some(reason.map_or(note.clone(), |r| format!("{r}; {note}")));
        }
        self.complete(&record.id, |record| {
            record.tree = tree.clone();
            record.finish(conclusion, reason.clone());
            if let Ok(report) = &outcome {
                record_engine_report(record, report);
            }
        })
        .await;
        let _ = self.prune(None, None).await;
    }

    /// End a running run: write the finished record before readers can see
    /// `done` (so a restart never reads a stale in-progress record), then
    /// apply `finish` and free its scheduler slot. Nothing is written under
    /// the state lock; a change that lands during the write (a coalesced
    /// submitter) is kept and written again.
    async fn complete(&self, run: &str, finish: impl Fn(&mut RunRecord)) {
        let finished = {
            let mut state = self.lock();
            state
                .runs
                .get_mut(run)
                .filter(|slot| slot.record.state != RunState::Done)
                .map(|slot| {
                    let mut save = slot.snapshot();
                    finish(&mut save.record);
                    save
                })
        };
        let written = match finished {
            Some(save) => {
                let written = save.record.clone();
                let _ = self.writer.save(save).await;
                Some(written)
            }
            None => None,
        };
        // `done` and the freed slot become visible together.
        let rewrite = {
            let mut state = self.lock();
            let mut rewrite = None;
            if let Some(written) = written
                && let Some(slot) = state.runs.get_mut(run)
                && slot.record.state != RunState::Done
            {
                slot.cancel = None;
                finish(&mut slot.record);
                self.feed.publish(&slot.record);
                if slot.record != written {
                    rewrite = Some(slot.snapshot());
                }
            }
            state.scheduler.finish(run);
            rewrite
        };
        if let Some(save) = rewrite {
            self.writer.save_detached(save);
        }
        self.kick();
    }

    async fn drive(
        &self,
        record: &RunRecord,
        cancel: &CancellationSource,
        observer: &mut RunObserver,
    ) -> Result<EngineReport, String> {
        self.localize_checkouts(record, observer);
        let deadline = async_engine::Deadline::after(Duration::from_secs(record.timeout_secs));
        let Ok(plan) = async_engine::timeout_at(deadline, self.plan(record, deadline)).await else {
            // Nothing exists on the host yet: an engine is only created
            // once the plan is complete.
            return Ok(EngineReport {
                execution: ExecutionEnd::TimedOut,
                cleanup: CleanupEnd::Removed,
                engine_id: None,
            });
        };
        let plan = plan?;
        observer.masker = SecretMasker::new(plan.invocation.secrets.0.iter().map(|(_, v)| v));
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

    /// The opted-in secrets, read from the daemon's secret store. A refused
    /// secret fails the run; a missing one runs without it (act then calls
    /// GitHub anonymously, limited to 60 requests per hour).
    fn secrets(&self, record: &RunRecord) -> Result<SecretEnv, String> {
        let mut env = Vec::new();
        for name in &record.secrets {
            let key = crate::secrets::secret_env_name(name)
                .ok_or_else(|| format!("unknown secret {name}"))?;
            if let Some(value) = crate::secrets::read_secret(&self.state_dir, name)? {
                env.push((key.to_string(), value));
            }
        }
        Ok(SecretEnv(env))
    }

    /// Serve own-repository checkouts from the snapshot (#335) and say so.
    fn localize_checkouts(&self, record: &RunRecord, observer: &mut RunObserver) {
        let source = self.store.source(&record.id);
        match super::checkout::localize_tree(&source, &record.repository) {
            Ok(0) => {}
            Ok(changed) => observer.note(&format!(
                "{changed} actions/checkout step(s) of this repository are served from the frozen snapshot"
            )),
            Err(error) => observer.note(&format!("workflow left as written: {error}")),
        }
    }

    /// The immutable engine intent and act invocation for a record.
    async fn plan(
        &self,
        record: &RunRecord,
        deadline: async_engine::Deadline,
    ) -> Result<EnginePlan, String> {
        let artifact = act_artifact(std::env::consts::ARCH)
            .ok_or("no pinned act build for this host architecture")?;
        let (_, runner_digest) = RUNNER_IMAGE
            .rsplit_once('@')
            .ok_or("runner image is not pinned")?;
        let registry_id = self
            .registry
            .status()
            .await
            .map_err(|e| format!("registry: {e}"))?
            .registry_id;
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
                creation_profile: None,
            },
            source: self.store.source(&record.id),
            event: self.store.event(&record.id),
            invocation: ActInvocation {
                event: record.event.clone(),
                workflow: record.workflow.clone(),
                job: record.job.clone(),
                cache_namespace: record.cache_namespace(),
                secrets: self.secrets(record)?,
            },
            cache: CacheVolume::machine(&registry_id, lifecycle::now_seconds())?,
            deadline,
        })
    }
}

fn runner_status(scheduler: &Scheduler, widget: WidgetPresence) -> RunnerStatus {
    RunnerStatus {
        limit: scheduler.limit(),
        running: scheduler.running(),
        queued: scheduler.queued(),
        drained: scheduler.drained(),
        engine: "act".into(),
        act_version: ACT_VERSION.into(),
        widget,
    }
}

/// Serialize a typed reply for the daemon frame.
fn wire<T: serde::Serialize>(reply: Result<T, CiError>) -> Result<Value, CiError> {
    reply.and_then(|reply| {
        serde_json::to_value(reply).map_err(|e| CiError::new("internal", e.to_string()))
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

/// The scheduler's answer to a submission, decided under the state lock.
enum Admitted {
    Queued(SubmitReply),
    /// Coalesced into a live run: that run's new version, to write.
    Joined(Option<Save>),
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
    pub(crate) fn listing(&self) -> ListReply {
        self.list(None, None, 100)
    }
}
