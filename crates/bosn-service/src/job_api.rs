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
    Log {
        id: u64,
        line: String,
    },
    Completed {
        id: u64,
        kind: SetupJobKind,
        result: Result<String, String>,
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
    pub(crate) prepare: Arc<dyn SetupPrepareExecutor>,
    pub(crate) task: Arc<dyn SetupTaskExecutor>,
    pub(crate) app_task: Arc<dyn SetupAppTaskExecutor>,
    pub(crate) ensure: Arc<dyn SetupEnsureExecutor>,
    pub(crate) manifest_ensure: Arc<dyn ManifestEnsureExecutor>,
    pub(crate) manifest_app_task: Arc<dyn ManifestAppTaskExecutor>,
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
}
