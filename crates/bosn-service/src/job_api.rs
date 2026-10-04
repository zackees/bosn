//! Typed handles onto the job actor and its command vocabulary.

use super::*;

#[derive(Clone)]
pub(crate) struct JobActor {
    pub(crate) sender: async_engine::Sender<JobCommand>,
}
pub(crate) enum JobCommand {
    Submit {
        workspace: String,
        stack: String,
        digest: String,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    Status {
        id: u64,
        reply: async_engine::OneshotSender<Result<jobs::Job, Error>>,
    },
    Cancel {
        id: u64,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    Logs {
        id: u64,
        after: u64,
        limit: usize,
        reply: async_engine::OneshotSender<Result<jobs::LogPage, Error>>,
    },
    SubmitSetupPrepare {
        request: SetupPrepareRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitSetupTask {
        request: SetupTaskJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitSetupAppTask {
        request: SetupAppTaskJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitSetupEnsure {
        request: SetupEnsureJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitManifestEnsure {
        request: ManifestEnsureJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitManifestConverge {
        request: ManifestConvergeJobRequest,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    SubmitManifestAppTask {
        request: ManifestAppTaskJobRequest,
        /// Cancel the job once no status/log poll arrives for this long
        /// (#357). `None` keeps the job running to its deadline.
        follow_lease: Option<Duration>,
        reply: async_engine::OneshotSender<Result<u64, Error>>,
    },
    /// The job actor, rather than an executor task, owns the transition from
    /// a cancellable running job to a durably recorded successful ensure.
    /// It deliberately awaits the registry transaction before it processes a
    /// later Cancel command, then settles the job before replying.
    PersistSetupEnsure {
        id: u64,
        execution: SetupEnsureExecution,
        reply: async_engine::OneshotSender<Result<(), String>>,
    },
    PersistManifestEnsure {
        id: u64,
        execution: SetupEnsureExecution,
        contract: ManifestRecoveryContract,
        reply: async_engine::OneshotSender<Result<(), String>>,
    },
    /// Record one completed member of an all-stack convergence while keeping
    /// the parent job running for later members. This preserves each stack's
    /// normal atomic record-then-rollover transition and makes partial batch
    /// success durable if a later stack fails or is cancelled.
    PersistManifestConvergeStack {
        id: u64,
        execution: SetupEnsureExecution,
        contract: ManifestRecoveryContract,
        reply: async_engine::OneshotSender<Result<(), String>>,
    },
    /// Consecutive output lines of one job, batched by its forwarder so a
    /// chatty job costs one actor command per burst, not one per line.
    Log {
        id: u64,
        lines: Vec<String>,
    },
    Completed {
        id: u64,
        kind: SetupJobKind,
        result: Result<String, String>,
    },
    /// The accounting view behind `bosn jobs` (#358), as JSON.
    List {
        reply: async_engine::OneshotSender<String>,
    },
    Stop(async_engine::OneshotSender<()>),
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SetupJobKind {
    Prepare,
    Task,
    AppTask,
    Ensure,
    ManifestEnsure,
    ManifestConverge,
    ManifestAppTask,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetupEnsureEventOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetupEnsureEvent {
    pub(crate) kind: &'static str,
    pub(crate) detail: String,
}

impl SetupEnsureEvent {
    pub(crate) fn submitted(id: u64, request: &SetupEnsureJobRequest) -> Self {
        let policy = match request.policy {
            SetupPreparePolicy::Refresh => "refresh",
            SetupPreparePolicy::Offline => "offline",
        };
        // The event stream is diagnostic metadata, not a configuration
        // archive. In particular, locators can contain credentials, signed
        // query strings, or sensitive local path components.
        let source = config_locator_kind(&request.config);
        Self {
            kind: "setup.ensure.submitted",
            detail: format!("job_id={id} policy={policy} source={source}"),
        }
    }

    pub(crate) fn terminal(id: u64, outcome: SetupEnsureEventOutcome) -> Self {
        let (kind, outcome) = match outcome {
            SetupEnsureEventOutcome::Succeeded => ("setup.ensure.succeeded", "succeeded"),
            SetupEnsureEventOutcome::Failed => ("setup.ensure.failed", "failed"),
            SetupEnsureEventOutcome::Cancelled => ("setup.ensure.cancelled", "cancelled"),
            SetupEnsureEventOutcome::Superseded => ("setup.ensure.superseded", "superseded"),
        };
        Self {
            kind,
            // Never include executor receipts, engine output, workspace paths,
            // container IDs, or a config locator in terminal diagnostics.
            detail: format!("job_id={id} outcome={outcome}"),
        }
    }
}

pub(crate) fn config_locator_kind(config: &str) -> &'static str {
    if config.starts_with("https://") {
        "https"
    } else if config.starts_with("http://") {
        "http"
    } else if config.starts_with("file://") {
        "file"
    } else {
        "path"
    }
}

pub(crate) enum SetupJobRequest {
    Prepare(SetupPrepareRequest),
    Task(SetupTaskJobRequest),
    AppTask(SetupAppTaskJobRequest),
    Ensure(SetupEnsureJobRequest),
    ManifestEnsure(ManifestEnsureJobRequest),
    ManifestConverge(ManifestConvergeJobRequest),
    ManifestAppTask(ManifestAppTaskJobRequest),
}

#[derive(Clone)]
pub(crate) struct SetupExecutors {
    pub(crate) state_dir: Option<PathBuf>,
    pub(crate) prepare: Arc<dyn SetupPrepareExecutor>,
    pub(crate) task: Arc<dyn SetupTaskExecutor>,
    pub(crate) app_task: Arc<dyn SetupAppTaskExecutor>,
    pub(crate) ensure: Arc<dyn SetupEnsureExecutor>,
    pub(crate) manifest_ensure: Arc<dyn ManifestEnsureExecutor>,
    pub(crate) manifest_app_task: Arc<dyn ManifestAppTaskExecutor>,
    /// Runner accounting (#358). `None` in tests that exercise only the
    /// admission and registry paths.
    pub(crate) runners: Option<Arc<runners::Runners>>,
}
impl JobActor {
    pub(crate) async fn submit(
        &self,
        workspace: String,
        stack: String,
        digest: String,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::Submit {
                workspace,
                stack,
                digest,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn status(&self, id: u64) -> Result<jobs::Job, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::Status { id, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn cancel(&self, id: u64) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::Cancel { id, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn logs(
        &self,
        id: u64,
        after: u64,
        limit: usize,
    ) -> Result<jobs::LogPage, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::Logs {
                id,
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn submit_setup_prepare(
        &self,
        request: SetupPrepareRequest,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitSetupPrepare { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn submit_setup_task(
        &self,
        request: SetupTaskJobRequest,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitSetupTask { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn submit_setup_app_task(
        &self,
        request: SetupAppTaskJobRequest,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitSetupAppTask { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn submit_setup_ensure(
        &self,
        request: SetupEnsureJobRequest,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitSetupEnsure { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn submit_manifest_ensure(
        &self,
        request: ManifestEnsureJobRequest,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitManifestEnsure { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn submit_manifest_converge(
        &self,
        request: ManifestConvergeJobRequest,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitManifestConverge { request, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn submit_manifest_app_task(
        &self,
        request: ManifestAppTaskJobRequest,
        follow_lease: Option<Duration>,
    ) -> Result<u64, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::SubmitManifestAppTask {
                request,
                follow_lease,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn stop(&self) {
        let (reply, wait) = async_engine::oneshot_channel();
        if self.sender.send(JobCommand::Stop(reply)).await.is_ok() {
            let _ = wait.await;
        }
    }
    pub(crate) async fn list(&self) -> Result<String, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(JobCommand::List { reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)
    }
}

/// How many finished jobs `bosn jobs` shows.
const JOBS_VIEW_RECENT: usize = 20;

/// The `bosn jobs` document: capacity, lane load, every unfinished job and
/// the most recent finished ones, with each running task's accounting.
pub(crate) fn jobs_view(jobs: &Jobs, runners: Option<&runners::Runners>) -> serde_json::Value {
    let epoch_ms = |at: Option<SystemTime>| {
        at.and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
    };
    let views: BTreeMap<u64, runners::RunView> = runners
        .map(|r| {
            r.views()
                .into_iter()
                .map(|v| (v.record.job_id, v))
                .collect()
        })
        .unwrap_or_default();
    let now = Instant::now();
    let list: Vec<serde_json::Value> = jobs
        .snapshot(JOBS_VIEW_RECENT)
        .into_iter()
        .map(|job| {
            let mut entry = serde_json::json!({
                "id": job.id,
                "state": job_state_name(job.state),
                "class": job.class.label(),
                "slot": job.slot,
                "workspace": job.workspace,
                "key": job.stack,
                "submitted_ms": epoch_ms(Some(job.submitted_at)),
                "started_ms": epoch_ms(job.started_at),
                "finished_ms": epoch_ms(job.finished_at),
                // Idle: since the last log line or Docker request, as the
                // stall sweep sees it.
                "idle_seconds": job.last_progress
                    .filter(|_| !job.state.terminal())
                    .map(|at| {
                        let docker = views.get(&job.id).map(|v| {
                            Instant::now()
                                .checked_sub(Duration::from_millis(
                                    docker_proxy::now_ms().saturating_sub(v.last_activity_ms),
                                ))
                                .unwrap_or(now)
                        });
                        now.saturating_duration_since(docker.map_or(at, |d| d.max(at))).as_secs()
                    }),
                "error": job.error,
            });
            if let Some(view) = views.get(&job.id) {
                entry["run"] = serde_json::to_value(view).unwrap_or_default();
            }
            entry
        })
        .collect();
    let load = jobs.load();
    let lane = |class| {
        let (queued, running) = load.get(&class).copied().unwrap_or_default();
        serde_json::json!({"queued": queued, "running": running})
    };
    let policy = jobs.policy();
    let capacity = runners.map(|r| r.capacity().clone());
    serde_json::json!({
        "version": 1,
        "capacity": {
            "runner_slots": policy.runner_slots,
            "control_slots": policy.control_slots,
            "cpus_per_slot": capacity.as_ref().map(|c| c.cpus_per_slot),
            "memory_per_slot": capacity.as_ref().and_then(|c| c.memory_per_slot),
            "stall_seconds": capacity.as_ref().and_then(|c| c.stall_after).map(|d| d.as_secs()),
            "docker_proxy": capacity.as_ref().is_some_and(|c| c.docker_proxy),
            "host_cpus": capacity::host_cpus(),
        },
        "lanes": {"runner": lane(jobs::JobClass::Runner), "control": lane(jobs::JobClass::Control)},
        "jobs": list,
    })
}

fn job_state_name(state: jobs::JobState) -> &'static str {
    match state {
        jobs::JobState::Queued => "queued",
        jobs::JobState::Running => "running",
        jobs::JobState::Cancelling => "cancelling",
        jobs::JobState::Succeeded => "succeeded",
        jobs::JobState::Failed => "failed",
        jobs::JobState::Cancelled => "cancelled",
        jobs::JobState::Superseded => "superseded",
    }
}
